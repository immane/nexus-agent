//! Public-boundary hardening for `nexus_core::ids`.
//!
//! Every identifier newtype is exercised through the public API only: valid
//! construction, parse/display round-trip, rejection precedence, and
//! cross-type scoping. All inputs are fixed constants, so the tests are
//! deterministic.

#![forbid(unsafe_code)]

use std::any::TypeId;
use std::collections::HashMap;

use nexus_core::ids::MAX_ID_LEN;
use nexus_core::{
    ApprovalId, CallId, IdError, M0_REVISION, RequestId, RunId, SessionId, ToolId, TurnId,
};

/// The complete lock charset: 26 + 26 + 10 + 2 = 64 bytes = `MAX_ID_LEN`.
const FULL_CHARSET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn is_lock_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

fn assert_rejected<T: std::fmt::Debug>(result: Result<T, IdError>, expected: IdError, input: &str) {
    match result {
        Ok(value) => panic!("{input:?} must be rejected, constructed {value:?}"),
        Err(error) => assert_eq!(error, expected, "wrong error for {input:?}"),
    }
}

/// Shared contract for the six single-string identifier newtypes.
fn exercise_single_string_id<T, F>(type_name: &str, parse: F)
where
    F: Fn(&str) -> Result<T, IdError>,
    T: AsRef<str> + Clone + std::fmt::Debug + std::fmt::Display + PartialEq,
{
    // Valid construction, accessors, and display round-trip.
    for raw in ["a", "A", "0", "_", "-", "a-b_C9", FULL_CHARSET] {
        let id = parse(raw)
            .unwrap_or_else(|error| panic!("{type_name} rejected valid {raw:?}: {error}"));
        assert_eq!(AsRef::<str>::as_ref(&id), raw, "{type_name} AsRef<str>");
        assert_eq!(id.to_string(), raw, "{type_name} Display");
        let reparsed = parse(AsRef::<str>::as_ref(&id))
            .unwrap_or_else(|error| panic!("{type_name} could not re-parse {raw:?}: {error}"));
        assert_eq!(reparsed, id, "{type_name} round-trip {raw:?}");
        assert_eq!(reparsed.clone(), id, "{type_name} Clone {raw:?}");
    }

    // Every printable ASCII byte is accepted exactly when it is a lock byte.
    for byte in 0x20u8..=0x7E {
        let raw = (byte as char).to_string();
        if is_lock_byte(byte) {
            let id =
                parse(&raw).unwrap_or_else(|error| panic!("{type_name} rejected {raw:?}: {error}"));
            assert_eq!(id.to_string(), raw, "{type_name} Display of {raw:?}");
        } else {
            assert_rejected(parse(&raw), IdError::IllegalChar, &raw);
        }
    }

    // Non-ASCII never passes the charset check.
    for raw in ["é", "ü", "中", "🙂", "\u{00A0}", "\u{200B}"] {
        assert_rejected(parse(raw), IdError::IllegalChar, raw);
    }

    // Empty and length boundaries; TooLong wins over IllegalChar.
    assert_rejected(parse(""), IdError::Empty, "");
    let overlong = "x".repeat(MAX_ID_LEN + 1);
    assert_rejected(parse(&overlong), IdError::TooLong, &overlong);
    let overlong_illegal = format!("{}!", "x".repeat(MAX_ID_LEN));
    assert_rejected(
        parse(&overlong_illegal),
        IdError::TooLong,
        &overlong_illegal,
    );
    let boundary = "x".repeat(MAX_ID_LEN);
    assert_eq!(
        parse(&boundary)
            .expect("maximum length is accepted")
            .to_string(),
        boundary,
        "{type_name} max-length boundary"
    );
}

/// Shared identity contract: equal values stay equal, distinct text does not.
fn exercise_hash_map_identity<T, F>(type_name: &str, parse: F)
where
    F: Fn(&str) -> Result<T, IdError>,
    T: Eq + std::hash::Hash,
{
    let mut map: HashMap<T, u8> = HashMap::new();
    assert_eq!(map.insert(parse("key").expect("valid"), 1), None);
    assert_eq!(
        map.get(&parse("key").expect("valid")),
        Some(&1),
        "{type_name} equality"
    );
    assert_eq!(
        map.get(&parse("other").expect("valid")),
        None,
        "{type_name} distinct values"
    );
}

#[test]
fn max_id_len_is_the_lock_boundary() {
    assert_eq!(MAX_ID_LEN, 64);
    assert_eq!(FULL_CHARSET.len(), MAX_ID_LEN);
}

#[test]
fn every_single_string_id_honors_the_shared_contract() {
    exercise_single_string_id("SessionId", |raw| SessionId::new(raw));
    exercise_single_string_id("RunId", |raw| RunId::new(raw));
    exercise_single_string_id("TurnId", |raw| TurnId::new(raw));
    exercise_single_string_id("CallId", |raw| CallId::new(raw));
    exercise_single_string_id("RequestId", |raw| RequestId::new(raw));
    exercise_single_string_id("ApprovalId", |raw| ApprovalId::new(raw));
}

#[test]
fn constructors_accept_owned_and_borrowed_input() {
    let raw = String::from("owned-and-borrowed");
    assert_eq!(
        SessionId::new(raw.clone()).expect("owned").as_str(),
        raw.as_str()
    );
    assert_eq!(
        SessionId::new(raw.as_str()).expect("borrowed").as_str(),
        raw.as_str()
    );
    assert_eq!(
        RunId::new(raw.clone()).expect("owned").as_str(),
        raw.as_str()
    );
    assert_eq!(
        RunId::new(raw.as_str()).expect("borrowed").as_str(),
        raw.as_str()
    );
    assert_eq!(
        TurnId::new(raw.clone()).expect("owned").as_str(),
        raw.as_str()
    );
    assert_eq!(
        TurnId::new(raw.as_str()).expect("borrowed").as_str(),
        raw.as_str()
    );
    assert_eq!(
        CallId::new(raw.clone()).expect("owned").as_str(),
        raw.as_str()
    );
    assert_eq!(
        CallId::new(raw.as_str()).expect("borrowed").as_str(),
        raw.as_str()
    );
    assert_eq!(
        RequestId::new(raw.clone()).expect("owned").as_str(),
        raw.as_str()
    );
    assert_eq!(
        RequestId::new(raw.as_str()).expect("borrowed").as_str(),
        raw.as_str()
    );
    assert_eq!(
        ApprovalId::new(raw.clone()).expect("owned").as_str(),
        raw.as_str()
    );
    assert_eq!(
        ApprovalId::new(raw.as_str()).expect("borrowed").as_str(),
        raw.as_str()
    );
    assert_eq!(
        ToolId::new(raw.clone(), M0_REVISION).expect("owned").name(),
        raw.as_str()
    );
    assert_eq!(
        ToolId::new(raw.as_str(), M0_REVISION)
            .expect("borrowed")
            .name(),
        raw.as_str()
    );
}

#[test]
fn as_str_returns_the_exact_validated_raw_value() {
    assert_eq!(SessionId::new("s-1").expect("valid").as_str(), "s-1");
    assert_eq!(RunId::new("r-1").expect("valid").as_str(), "r-1");
    assert_eq!(TurnId::new("t-1").expect("valid").as_str(), "t-1");
    assert_eq!(CallId::new("c-1").expect("valid").as_str(), "c-1");
    assert_eq!(RequestId::new("q-1").expect("valid").as_str(), "q-1");
    assert_eq!(ApprovalId::new("a-1").expect("valid").as_str(), "a-1");
    assert_eq!(
        ToolId::new("host_read", M0_REVISION).expect("valid").name(),
        "host_read"
    );
}

#[test]
fn every_id_type_is_a_distinct_type() {
    let types: [(&str, TypeId); 7] = [
        ("SessionId", TypeId::of::<SessionId>()),
        ("RunId", TypeId::of::<RunId>()),
        ("TurnId", TypeId::of::<TurnId>()),
        ("CallId", TypeId::of::<CallId>()),
        ("RequestId", TypeId::of::<RequestId>()),
        ("ApprovalId", TypeId::of::<ApprovalId>()),
        ("ToolId", TypeId::of::<ToolId>()),
    ];
    for (index, (name, type_id)) in types.iter().enumerate() {
        for (other_name, other_type_id) in &types[index + 1..] {
            assert_ne!(type_id, other_type_id, "{name} must not alias {other_name}");
        }
    }
}

#[test]
fn the_same_raw_text_never_aliases_across_types() {
    let raw = "shared";
    let session = SessionId::new(raw).expect("valid");
    let run = RunId::new(raw).expect("valid");
    let turn = TurnId::new(raw).expect("valid");
    let call = CallId::new(raw).expect("valid");
    let request = RequestId::new(raw).expect("valid");
    let approval = ApprovalId::new(raw).expect("valid");
    let tool = ToolId::new(raw, M0_REVISION).expect("valid");

    // Each type parses the same opaque text independently.
    assert_eq!(session.as_str(), raw);
    assert_eq!(run.as_str(), raw);
    assert_eq!(turn.as_str(), raw);
    assert_eq!(call.as_str(), raw);
    assert_eq!(request.as_str(), raw);
    assert_eq!(approval.as_str(), raw);
    assert_eq!(tool.name(), raw);

    // No wrapper renders as another: Debug carries the concrete type.
    let debug_forms = [
        format!("{session:?}"),
        format!("{run:?}"),
        format!("{turn:?}"),
        format!("{call:?}"),
        format!("{request:?}"),
        format!("{approval:?}"),
        format!("{tool:?}"),
    ];
    for (index, form) in debug_forms.iter().enumerate() {
        for other in &debug_forms[index + 1..] {
            assert_ne!(form, other, "identifier wrappers must stay distinct");
        }
    }
}

#[test]
fn tool_id_carries_name_and_exact_revision() {
    let tool = ToolId::new("host_read", 7).expect("valid");
    assert_eq!(tool.name(), "host_read");
    assert_eq!(tool.revision(), 7);
    assert_eq!(tool.to_string(), "host_read@7");

    let boundary = ToolId::new("t".repeat(MAX_ID_LEN), M0_REVISION).expect("max-length name");
    assert_eq!(boundary.name().len(), MAX_ID_LEN);
    assert_eq!(boundary.revision(), M0_REVISION);

    let max_revision = ToolId::new("host_read", u32::MAX).expect("valid");
    assert_eq!(max_revision.revision(), u32::MAX);
    assert_eq!(max_revision.to_string(), format!("host_read@{}", u32::MAX));
}

#[test]
fn tool_id_display_round_trips_through_name_and_revision() {
    for (name, revision) in [("host_read", 0u32), ("a", 1), ("Z_9-x", u32::MAX)] {
        let tool = ToolId::new(name, revision).expect("valid");
        let rendered = tool.to_string();
        let (parsed_name, parsed_revision) = rendered
            .split_once('@')
            .expect("display separates name and revision");
        assert_eq!(parsed_name, name);
        let parsed_revision: u32 = parsed_revision.parse().expect("revision is numeric");
        assert_eq!(parsed_revision, revision);
        let reparsed = ToolId::new(parsed_name, parsed_revision).expect("round-trip is valid");
        assert_eq!(reparsed, tool);
        assert!(reparsed.is_compatible_with(&tool));
    }
}

#[test]
fn tool_id_rejects_invalid_names_with_the_shared_rules() {
    assert_rejected(ToolId::new("", M0_REVISION), IdError::Empty, "");
    assert_rejected(ToolId::new("a/b", M0_REVISION), IdError::IllegalChar, "a/b");
    let overlong = "x".repeat(MAX_ID_LEN + 1);
    assert_rejected(
        ToolId::new(&overlong, M0_REVISION),
        IdError::TooLong,
        &overlong,
    );
    // The revision does not participate in name validation.
    assert_eq!(
        ToolId::new("host_read", u32::MAX)
            .expect("valid")
            .revision(),
        u32::MAX
    );
}

#[test]
fn tool_id_compatibility_is_exact_equality() {
    let current = ToolId::new("host_read", M0_REVISION).expect("valid");
    let same = ToolId::new("host_read", M0_REVISION).expect("valid");
    let newer = ToolId::new("host_read", M0_REVISION + 1).expect("valid");
    let renamed = ToolId::new("host_write", M0_REVISION).expect("valid");

    assert_eq!(current, same);
    assert!(current.is_compatible_with(&same));
    assert!(same.is_compatible_with(&current));

    assert_ne!(current, newer);
    assert!(!current.is_compatible_with(&newer));
    assert!(!newer.is_compatible_with(&current));

    assert_ne!(current, renamed);
    assert!(!current.is_compatible_with(&renamed));
    assert!(!renamed.is_compatible_with(&current));
}

#[test]
fn id_error_is_copy_with_stable_messages() {
    assert_eq!(IdError::Empty.to_string(), "identifier is empty");
    assert_eq!(
        IdError::TooLong.to_string(),
        "identifier exceeds 64 characters"
    );
    assert_eq!(
        IdError::IllegalChar.to_string(),
        "identifier contains illegal characters"
    );

    for error in [IdError::Empty, IdError::TooLong, IdError::IllegalChar] {
        let copied: IdError = error;
        assert_eq!(copied, error);
        assert!(std::error::Error::source(&error).is_none());
        let erased: &dyn std::error::Error = &error;
        assert_eq!(erased.to_string(), error.to_string());
    }
}

#[test]
fn ids_are_stable_hash_map_keys() {
    exercise_hash_map_identity("SessionId", |raw| SessionId::new(raw));
    exercise_hash_map_identity("RunId", |raw| RunId::new(raw));
    exercise_hash_map_identity("TurnId", |raw| TurnId::new(raw));
    exercise_hash_map_identity("CallId", |raw| CallId::new(raw));
    exercise_hash_map_identity("RequestId", |raw| RequestId::new(raw));
    exercise_hash_map_identity("ApprovalId", |raw| ApprovalId::new(raw));
    exercise_hash_map_identity("ToolId", |raw| ToolId::new(raw, M0_REVISION));
}
