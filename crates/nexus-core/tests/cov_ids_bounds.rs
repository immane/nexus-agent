#![forbid(unsafe_code)]

//! Public-boundary hardening tests for opaque identifier validation.
//!
//! Covers the rejection contract of every identifier newtype in
//! [`nexus_core::ids`] (empty, overlong, illegal byte), the exact 64-byte
//! acceptance boundary, the M0 revision constant, and [`ToolId`]'s
//! exact-equality compatibility rule. Everything here is deterministic and
//! goes through the public API only.

use std::collections::HashSet;

use nexus_core::ids::MAX_ID_LEN;
use nexus_core::{
    ApprovalId, CallId, IdError, M0_REVISION, RequestId, RunId, SessionId, ToolId, TurnId,
};

/// Asserts that all seven identifier types reject `raw` with exactly
/// `expected`.
///
/// The macro exists so every boundary case is applied to every identifier
/// type; a per-type helper would let one type silently drop out.
macro_rules! assert_all_ids_reject {
    ($raw:expr, $expected:expr) => {{
        let raw: &str = $raw;
        let expected: IdError = $expected;
        assert_eq!(SessionId::new(raw), Err(expected), "SessionId {raw:?}");
        assert_eq!(RunId::new(raw), Err(expected), "RunId {raw:?}");
        assert_eq!(TurnId::new(raw), Err(expected), "TurnId {raw:?}");
        assert_eq!(CallId::new(raw), Err(expected), "CallId {raw:?}");
        assert_eq!(RequestId::new(raw), Err(expected), "RequestId {raw:?}");
        assert_eq!(ApprovalId::new(raw), Err(expected), "ApprovalId {raw:?}");
        assert_eq!(
            ToolId::new(raw, M0_REVISION),
            Err(expected),
            "ToolId {raw:?}"
        );
    }};
}

/// Asserts that all seven identifier types accept `raw`, preserve it verbatim
/// through `as_str`, `Display`, and `AsRef<str>`, and that `ToolId` attaches
/// `M0_REVISION`.
macro_rules! assert_all_ids_accept {
    ($raw:expr) => {{
        let raw: &str = $raw;

        let session = SessionId::new(raw).expect("SessionId accepts valid input");
        assert_eq!(session.as_str(), raw);
        assert_eq!(session.to_string(), raw);
        assert_eq!(session.as_ref() as &str, raw);

        let run = RunId::new(raw).expect("RunId accepts valid input");
        assert_eq!(run.as_str(), raw);
        assert_eq!(run.to_string(), raw);
        assert_eq!(run.as_ref() as &str, raw);

        let turn = TurnId::new(raw).expect("TurnId accepts valid input");
        assert_eq!(turn.as_str(), raw);
        assert_eq!(turn.to_string(), raw);
        assert_eq!(turn.as_ref() as &str, raw);

        let call = CallId::new(raw).expect("CallId accepts valid input");
        assert_eq!(call.as_str(), raw);
        assert_eq!(call.to_string(), raw);
        assert_eq!(call.as_ref() as &str, raw);

        let request = RequestId::new(raw).expect("RequestId accepts valid input");
        assert_eq!(request.as_str(), raw);
        assert_eq!(request.to_string(), raw);
        assert_eq!(request.as_ref() as &str, raw);

        let approval = ApprovalId::new(raw).expect("ApprovalId accepts valid input");
        assert_eq!(approval.as_str(), raw);
        assert_eq!(approval.to_string(), raw);
        assert_eq!(approval.as_ref() as &str, raw);

        let tool = ToolId::new(raw, M0_REVISION).expect("ToolId accepts valid input");
        assert_eq!(tool.name(), raw);
        assert_eq!(tool.revision(), M0_REVISION);
        assert_eq!(tool.to_string(), format!("{raw}@0"));
    }};
}

#[test]
fn max_id_len_contract_is_64_bytes() {
    assert_eq!(MAX_ID_LEN, 64);
}

#[test]
fn all_id_types_accept_exactly_max_len() {
    let repeated = "a".repeat(MAX_ID_LEN);
    assert_all_ids_accept!(&repeated);

    // The whole documented charset exactly fills the 64-byte cap:
    // 26 upper + 26 lower + 10 digits + `-` + `_`.
    let full_charset = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    assert_eq!(full_charset.len(), MAX_ID_LEN);
    assert_all_ids_accept!(full_charset);
}

#[test]
fn all_id_types_accept_single_char_from_each_valid_class() {
    for raw in ["A", "Z", "a", "z", "0", "9", "-", "_"] {
        assert_all_ids_accept!(raw);
    }
}

#[test]
fn all_id_types_reject_empty_with_empty_error() {
    assert_all_ids_reject!("", IdError::Empty);
}

#[test]
fn all_id_types_reject_one_past_max_with_too_long() {
    let over = "a".repeat(MAX_ID_LEN + 1);
    assert_eq!(over.len(), MAX_ID_LEN + 1);
    assert_all_ids_reject!(&over, IdError::TooLong);
}

#[test]
fn all_id_types_reject_far_over_max_with_too_long() {
    let huge = "x".repeat(MAX_ID_LEN * 16);
    assert_all_ids_reject!(&huge, IdError::TooLong);
}

#[test]
fn all_id_types_reject_illegal_bytes_with_illegal_char() {
    // Bytes immediately outside each accepted range, common punctuation and
    // separators, whitespace/control bytes, and non-ASCII text.
    let illegal = [
        "@",
        "[",
        "`",
        "{",
        "/",
        ":", // adjacent to A-Z, a-z, 0-9
        ".",
        ",",
        ";",
        "+",
        "=",
        "\\",
        "|",
        "~",
        "!",
        "#",
        "$",
        "%",
        "&",
        "*",
        "(",
        ")",
        "<",
        ">",
        "?",
        "^",
        "'",
        "\"",
        "]",
        "}", // punctuation
        " ",
        "\t",
        "\n",
        "\r",
        "\0", // whitespace and control
        "a/b",
        "a b",
        "a:b",
        "a.b",
        "a@b", // embedded in otherwise valid text
        "é",
        "aé",
        "ünïcode",
        "😀",
        "a😀b", // non-ASCII
    ];
    for raw in illegal {
        assert_all_ids_reject!(raw, IdError::IllegalChar);
    }
}

#[test]
fn rejection_order_is_empty_then_too_long_then_illegal_char() {
    // Overlong input reports TooLong even though it also contains an illegal
    // byte: length is checked before the charset.
    let overlong_illegal = format!("{}!", "a".repeat(MAX_ID_LEN));
    assert_all_ids_reject!(&overlong_illegal, IdError::TooLong);

    // The same illegal byte at exactly MAX_ID_LEN bytes reports IllegalChar.
    let at_limit_illegal = format!("{}!", "a".repeat(MAX_ID_LEN - 1));
    assert_eq!(at_limit_illegal.len(), MAX_ID_LEN);
    assert_all_ids_reject!(&at_limit_illegal, IdError::IllegalChar);
}

#[test]
fn length_limit_counts_bytes_not_characters() {
    // 33 two-byte characters = 66 bytes: over the cap, so TooLong.
    let over = "é".repeat(33);
    assert!(over.chars().count() < MAX_ID_LEN);
    assert!(over.len() > MAX_ID_LEN);
    assert_all_ids_reject!(&over, IdError::TooLong);

    // 32 two-byte characters = exactly 64 bytes: within the cap, so the
    // non-ASCII bytes report IllegalChar.
    let at_limit = "é".repeat(32);
    assert_eq!(at_limit.len(), MAX_ID_LEN);
    assert_all_ids_reject!(&at_limit, IdError::IllegalChar);
}

#[test]
fn m0_revision_constant_is_zero_and_usable() {
    let m0: u32 = M0_REVISION;
    assert_eq!(m0, 0);

    let tool = ToolId::new("host_read", m0).expect("M0 tool id builds");
    assert_eq!(tool.revision(), 0);
    let same = ToolId::new("host_read", M0_REVISION).expect("valid");
    assert!(tool.is_compatible_with(&same));
}

#[test]
fn tool_id_name_validation_ignores_revision() {
    for revision in [M0_REVISION, 1, u32::MAX] {
        assert_eq!(
            ToolId::new("", revision),
            Err(IdError::Empty),
            "revision {revision}"
        );
        assert_eq!(
            ToolId::new("a".repeat(MAX_ID_LEN + 1), revision),
            Err(IdError::TooLong),
            "revision {revision}"
        );
        assert_eq!(
            ToolId::new("bad/name", revision),
            Err(IdError::IllegalChar),
            "revision {revision}"
        );
    }
}

#[test]
fn tool_id_accepts_max_len_name_at_any_revision() {
    let name = "t".repeat(MAX_ID_LEN);
    for revision in [M0_REVISION, 1, 42, u32::MAX] {
        let tool = ToolId::new(&name, revision).expect("valid tool id");
        assert_eq!(tool.name(), name.as_str());
        assert_eq!(tool.revision(), revision);
        assert_eq!(tool.to_string(), format!("{name}@{revision}"));
    }
}

#[test]
fn tool_id_compatibility_requires_exact_name_and_revision() {
    let m0 = ToolId::new("host_read", M0_REVISION).expect("valid");
    let same = ToolId::new("host_read", M0_REVISION).expect("valid");
    let newer = ToolId::new("host_read", M0_REVISION + 1).expect("valid");
    let highest = ToolId::new("host_read", u32::MAX).expect("valid");
    let renamed = ToolId::new("host_write", M0_REVISION).expect("valid");

    assert!(m0.is_compatible_with(&m0));
    assert!(m0.is_compatible_with(&same));
    assert!(same.is_compatible_with(&m0));

    assert!(!m0.is_compatible_with(&newer));
    assert!(!newer.is_compatible_with(&m0));
    assert!(!m0.is_compatible_with(&highest));
    assert!(!highest.is_compatible_with(&m0));
    assert!(!m0.is_compatible_with(&renamed));
    assert!(!renamed.is_compatible_with(&m0));

    assert_eq!(m0, same);
    assert_ne!(m0, newer);
    assert_ne!(m0, renamed);
}

#[test]
fn id_values_compare_and_hash_by_content() {
    let owned = SessionId::new(String::from("session-1")).expect("valid");
    let borrowed = SessionId::new("session-1").expect("valid");
    let other = SessionId::new("session-2").expect("valid");
    assert_eq!(owned, borrowed);
    assert_ne!(owned, other);

    let mut set = HashSet::new();
    set.insert(owned.clone());
    set.insert(borrowed);
    assert_eq!(set.len(), 1);
    assert!(set.contains(&owned));
}

#[test]
fn id_error_variants_display_explicit_rejection_reasons() {
    assert_eq!(IdError::Empty.to_string(), "identifier is empty");
    assert_eq!(
        IdError::TooLong.to_string(),
        "identifier exceeds 64 characters"
    );
    assert_eq!(
        IdError::IllegalChar.to_string(),
        "identifier contains illegal characters"
    );
    assert_ne!(IdError::Empty, IdError::TooLong);
    assert_ne!(IdError::TooLong, IdError::IllegalChar);

    let error: &dyn std::error::Error = &IdError::IllegalChar;
    assert!(error.source().is_none());
}
