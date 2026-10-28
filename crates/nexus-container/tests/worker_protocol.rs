use nexus_container::worker_protocol::{
    MAX_RESPONSE_BYTES, ProtocolError, read_request, validate_workspace_paths, write_response,
};
use serde_json::json;
use std::io::Cursor;

#[test]
fn rejects_duplicate_keys_even_when_nested() {
    for input in [
        r#"{"version":2,"operation":"host_read","arguments":{"path":"a","path":"b"},"output_limit":10,"tools":{"list_entry_limit":1,"search_file_bytes":1}}"#,
        r#"{"version":2,"operation":"host_read","arguments":{"path":"a","nested":{"x":1,"x":2}},"output_limit":10,"tools":{"list_entry_limit":1,"search_file_bytes":1}}"#,
    ] {
        assert!(matches!(
            read_request(&mut Cursor::new(input)),
            Err(ProtocolError::InvalidRequest)
        ));
    }
}

#[test]
fn requires_exact_operation_arguments_and_workspace_lexical_scope() {
    for input in [
        r#"{"version":2,"operation":"host_read","arguments":{"path":"a","other":1},"output_limit":10,"tools":{"list_entry_limit":1,"search_file_bytes":1}}"#,
        r#"{"version":2,"operation":"host_write","arguments":{"path":"a"},"output_limit":10,"tools":{"list_entry_limit":1,"search_file_bytes":1}}"#,
    ] {
        assert!(read_request(&mut Cursor::new(input)).is_err());
    }
    assert!(
        validate_workspace_paths("host_read", &json!({"path":"/workspace/../etc/passwd"})).is_err()
    );
    assert!(validate_workspace_paths("host_read", &json!({"path":"/workspace/file"})).is_ok());
    assert!(validate_workspace_paths("host_read", &json!({"path":"sub/file"})).is_ok());
}

#[test]
fn response_expansion_is_bounded_before_writing() {
    let mut output = Vec::new();
    let result = write_response(
        &mut output,
        json!({"content":"x".repeat(MAX_RESPONSE_BYTES)}),
    );
    assert!(matches!(result, Err(ProtocolError::ResponseTooLarge)));
    assert!(output.is_empty());
}

#[test]
fn read_pagination_round_trips_and_remains_closed_and_jailed() {
    let arguments = json!({"path":"sub/file", "offset":8192, "limit":16384});
    let wire = json!({"version":2,"operation":"host_read","arguments":arguments,"output_limit":20000,"tools":{"list_entry_limit":10000,"search_file_bytes":1048576}});
    let request = read_request(&mut Cursor::new(wire.to_string())).unwrap();
    assert_eq!(request.arguments, arguments);
    assert!(validate_workspace_paths(&request.operation, &request.arguments).is_ok());
    for arguments in [
        json!({"path":"a","offset":-1}),
        json!({"path":"a","offset":1.5}),
        json!({"path":"a","offset":null}),
        json!({"path":"a","offset":9223372036854775808_u64}),
        json!({"path":"a","limit":0}),
        json!({"path":"a","limit":16385}),
        json!({"path":"a","limit":"10"}),
        json!({"path":"a","offset":0,"extra":true}),
    ] {
        let wire = json!({"version":2,"operation":"host_read","arguments":arguments,"output_limit":20000,"tools":{"list_entry_limit":10000,"search_file_bytes":1048576}});
        assert!(
            read_request(&mut Cursor::new(wire.to_string())).is_err(),
            "{arguments}"
        );
    }
    assert!(
        validate_workspace_paths(
            "host_read",
            &json!({"path":"../secret","offset":8192,"limit":4})
        )
        .is_err()
    );
}
