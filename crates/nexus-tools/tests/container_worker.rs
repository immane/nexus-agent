use nexus_tools::container_worker_protocol::{
    MAX_RESPONSE_BYTES, ProtocolError, read_request, validate_workspace_paths, write_response,
};
use serde_json::json;
use std::io::Cursor;

#[test]
fn rejects_duplicate_keys_even_when_nested() {
    for input in [
        r#"{"version":1,"operation":"host_read","arguments":{"path":"a","path":"b"},"output_limit":10}"#,
        r#"{"version":1,"operation":"host_read","arguments":{"path":"a","nested":{"x":1,"x":2}},"output_limit":10}"#,
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
        r#"{"version":1,"operation":"host_read","arguments":{"path":"a","other":1},"output_limit":10}"#,
        r#"{"version":1,"operation":"host_write","arguments":{"path":"a"},"output_limit":10}"#,
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
