//! Coverage hardening for store capacity and boundary paths.
//!
//! Integration-level companion to the unit tests in `nexus_core::store`:
//! finite positive M0-test caps, checkpoint profile and message metadata
//! bounds, retained-message bounds, format-revision exact equality, and the
//! store-boundary recheck on constructed checkpoints. Public API only and
//! deterministic: no clock, randomness, or external I/O.

#![forbid(unsafe_code)]

use nexus_core::limits::Limits;
use nexus_core::provider;
use nexus_core::store::{
    MAX_INTENT_RECORDS, MAX_OUTCOME_RECORDS, MAX_PROFILE_LEN, MAX_SESSIONS, MAX_STORED_MESSAGES,
    StoredMessage, check_checkpoint_bounds, check_format_revision,
};
use nexus_core::{
    AgentError, ErrorCategory, RetryGuidance, STORE_FORMAT_REVISION, SessionCheckpoint, SessionId,
};

/// Fixed 64-byte source-label bound enforced by the store's internal
/// `check_stored_parts`; it is not a public constant, so the boundary test
/// pins the literal.
const MESSAGE_SOURCE_BOUND: usize = 64;

fn message(source: &str, text: &str) -> StoredMessage {
    StoredMessage {
        source: source.to_owned(),
        text: text.to_owned(),
        complete: true,
    }
}

fn checkpoint(
    messages: Vec<StoredMessage>,
    profile: &str,
) -> Result<SessionCheckpoint, AgentError> {
    SessionCheckpoint::new(
        SessionId::new("sess-1").expect("valid session id"),
        STORE_FORMAT_REVISION,
        1,
        messages,
        profile,
    )
}

fn assert_invalid_input(error: AgentError) {
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
}

#[test]
fn store_test_caps_are_finite_positive_and_pinned() {
    let caps = [
        ("MAX_STORED_MESSAGES", MAX_STORED_MESSAGES),
        ("MAX_SESSIONS", MAX_SESSIONS),
        ("MAX_INTENT_RECORDS", MAX_INTENT_RECORDS),
        ("MAX_OUTCOME_RECORDS", MAX_OUTCOME_RECORDS),
        ("MAX_PROFILE_LEN", MAX_PROFILE_LEN),
    ];
    for (name, cap) in caps {
        assert!(cap > 0, "{name} must be positive");
        assert_ne!(
            cap,
            usize::MAX,
            "{name} must be finite, never the unbounded sentinel"
        );
    }

    // Pin the documented M0-test choices so a cap change is a conscious test
    // update, not silent drift.
    assert_eq!(MAX_STORED_MESSAGES, 128);
    assert_eq!(MAX_SESSIONS, 64);
    assert_eq!(MAX_INTENT_RECORDS, 256);
    assert_eq!(MAX_OUTCOME_RECORDS, 256);
    assert_eq!(MAX_PROFILE_LEN, 128);

    // Alignment with the retained-context budget and the provider
    // profile-name bound.
    assert_eq!(MAX_STORED_MESSAGES, Limits::M0_TEST_RETAINED_CONTEXT_ITEMS);
    assert_eq!(MAX_PROFILE_LEN, provider::MAX_PROFILE_LEN);
    assert_eq!(STORE_FORMAT_REVISION, nexus_core::M0_REVISION);
}

#[test]
fn checkpoint_profile_metadata_bound_is_byte_exact() {
    let messages = || vec![message("user", "hello")];

    assert_invalid_input(checkpoint(messages(), "").expect_err("empty profile is rejected"));

    // The bound counts bytes, not characters: a multi-byte profile at exactly
    // MAX_PROFILE_LEN bytes is accepted.
    let multibyte = "é".repeat(MAX_PROFILE_LEN / 2);
    assert_eq!(multibyte.len(), MAX_PROFILE_LEN);
    let built = checkpoint(messages(), &multibyte).expect("profile at the byte bound is accepted");
    assert_eq!(built.profile(), multibyte);

    // One ASCII byte beyond the bound is rejected.
    let over = format!("{multibyte}x");
    assert_eq!(over.len(), MAX_PROFILE_LEN + 1);
    assert_invalid_input(checkpoint(messages(), &over).expect_err("one byte over is rejected"));
    assert_invalid_input(
        checkpoint(messages(), &"p".repeat(MAX_PROFILE_LEN + 1))
            .expect_err("ASCII profile over the bound is rejected"),
    );
}

#[test]
fn checkpoint_message_metadata_bounds_reject_invalid_parts() {
    // Empty source labels carry no identity.
    assert_invalid_input(
        checkpoint(vec![message("", "hello")], "p").expect_err("empty source is rejected"),
    );

    // Source label at the fixed 64-byte bound is accepted; one byte over is
    // rejected.
    let source_at_bound = "s".repeat(MESSAGE_SOURCE_BOUND);
    checkpoint(vec![message(&source_at_bound, "hello")], "p")
        .expect("source at the 64-byte bound is accepted");
    assert_invalid_input(
        checkpoint(
            vec![message(&"s".repeat(MESSAGE_SOURCE_BOUND + 1), "hello")],
            "p",
        )
        .expect_err("source over the 64-byte bound is rejected"),
    );

    // Message text at the tool-output byte bound is accepted; one byte over
    // is rejected.
    let text_at_bound = "t".repeat(Limits::M0_TEST_TOOL_OUTPUT_BYTES);
    let built = checkpoint(vec![message("user", &text_at_bound)], "p")
        .expect("text at the output-byte bound is accepted");
    assert_eq!(
        built.messages()[0].text.len(),
        Limits::M0_TEST_TOOL_OUTPUT_BYTES
    );
    assert_invalid_input(
        checkpoint(
            vec![message(
                "user",
                &"t".repeat(Limits::M0_TEST_TOOL_OUTPUT_BYTES + 1),
            )],
            "p",
        )
        .expect_err("text over the output-byte bound is rejected"),
    );

    // One invalid message rejects the whole checkpoint, even among valid
    // siblings.
    assert_invalid_input(
        checkpoint(vec![message("user", "ok"), message("", "bad")], "p")
            .expect_err("any invalid message rejects the checkpoint"),
    );

    // The store carries caller completeness labels and empty text as-is:
    // partial output is labeled by the caller, and content emptiness is a
    // runtime concern, not a store bound.
    let partial = StoredMessage {
        source: "assistant".to_owned(),
        text: String::new(),
        complete: false,
    };
    let built = checkpoint(vec![partial], "p").expect("carrier-level message is accepted");
    assert!(!built.messages()[0].complete);
    assert!(built.messages()[0].text.is_empty());
}

#[test]
fn retained_message_bound_accepts_exactly_max_and_rejects_one_more() {
    let at_bound: Vec<StoredMessage> = (0..MAX_STORED_MESSAGES)
        .map(|index| message("user", &format!("message {index}")))
        .collect();
    let built = checkpoint(at_bound, "p").expect("exactly the retained-message bound is accepted");
    assert_eq!(built.messages().len(), MAX_STORED_MESSAGES);
    assert_eq!(built.messages()[0].text, "message 0");
    assert_eq!(
        built.messages()[MAX_STORED_MESSAGES - 1].text,
        format!("message {}", MAX_STORED_MESSAGES - 1)
    );

    let over: Vec<StoredMessage> = (0..=MAX_STORED_MESSAGES)
        .map(|index| message("user", &format!("message {index}")))
        .collect();
    assert_invalid_input(
        checkpoint(over, "p").expect_err("one message over the bound is rejected"),
    );

    // Zero retained messages is a valid checkpoint.
    let empty = checkpoint(Vec::new(), "p").expect("zero messages are accepted");
    assert!(empty.messages().is_empty());
}

#[test]
fn store_boundary_recheck_accepts_constructed_checkpoints() {
    // A checkpoint sitting exactly on the message-count and profile bounds
    // must pass the store-boundary recheck unchanged and repeatably.
    let messages: Vec<StoredMessage> = (0..MAX_STORED_MESSAGES)
        .map(|index| message("user", &format!("message {index}")))
        .collect();
    let profile = "p".repeat(MAX_PROFILE_LEN);
    let built = checkpoint(messages, &profile).expect("at-bound checkpoint builds");
    check_checkpoint_bounds(&built).expect("at-bound checkpoint passes the boundary recheck");
    check_checkpoint_bounds(&built).expect("the recheck is repeatable and stateless");

    // Accessors preserve the constructed identity, revision, and bounded
    // parts exactly.
    assert_eq!(built.session().as_str(), "sess-1");
    assert_eq!(built.format_revision(), STORE_FORMAT_REVISION);
    assert_eq!(built.logical_revision(), 1);
    assert_eq!(built.messages().len(), MAX_STORED_MESSAGES);
    assert_eq!(built.profile(), profile);

    // Minimal and incomplete-labeled checkpoints pass the same recheck.
    let minimal = checkpoint(vec![message("user", "hello")], "p").expect("minimal checkpoint");
    check_checkpoint_bounds(&minimal).expect("minimal checkpoint passes");
    let empty = checkpoint(Vec::new(), "p").expect("zero-message checkpoint builds");
    check_checkpoint_bounds(&empty).expect("zero-message checkpoint passes");
    let partial = checkpoint(
        vec![StoredMessage {
            source: "assistant".to_owned(),
            text: "partial".to_owned(),
            complete: false,
        }],
        "p",
    )
    .expect("incomplete label is carried");
    check_checkpoint_bounds(&partial).expect("incomplete-labeled checkpoint passes");
}

#[test]
fn format_revision_exact_equality_at_the_boundary() {
    assert_eq!(STORE_FORMAT_REVISION, 0, "M0 revision is exactly zero");
    check_format_revision(STORE_FORMAT_REVISION).expect("current revision is accepted");

    for revision in [STORE_FORMAT_REVISION + 1, u32::MAX] {
        assert_invalid_input(
            check_format_revision(revision).expect_err("only exact equality is accepted"),
        );
        assert_invalid_input(
            SessionCheckpoint::new(
                SessionId::new("sess-1").expect("valid session id"),
                revision,
                1,
                vec![message("user", "hello")],
                "p",
            )
            .expect_err("construction rejects a non-current revision"),
        );
    }
}
