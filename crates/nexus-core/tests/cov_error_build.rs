//! Public-boundary coverage for [`AgentError`] construction, accessors, and
//! display.
//!
//! Every assertion goes through the crate's public API: no private fields and
//! no `pub(crate)` helpers. The tests are deterministic (no time, randomness,
//! environment, or I/O) and pin the display shape so diagnostics cannot
//! silently start leaking `Debug`-formatted internals or correlation data.

#![forbid(unsafe_code)]

use nexus_core::error::{
    MAX_CORRELATION_ENTRIES, MAX_CORRELATION_KEY_LEN, MAX_CORRELATION_VALUE_LEN, MAX_MESSAGE_LEN,
};
use nexus_core::{AgentError, CorrelationData, ErrorBuildError, ErrorCategory, RetryGuidance};

/// Every public category paired with its stable `as_str` label.
const ALL_CATEGORIES: [(ErrorCategory, &str); 13] = [
    (ErrorCategory::InvalidInput, "invalid-input"),
    (
        ErrorCategory::UnsupportedCapability,
        "unsupported-capability",
    ),
    (ErrorCategory::Authentication, "authentication"),
    (ErrorCategory::PermissionDenied, "permission-denied"),
    (ErrorCategory::RateLimited, "rate-limited"),
    (ErrorCategory::Protocol, "protocol"),
    (ErrorCategory::Timeout, "timeout"),
    (ErrorCategory::Cancelled, "cancelled"),
    (ErrorCategory::ResourceLimit, "resource-limit"),
    (ErrorCategory::ToolFailure, "tool-failure"),
    (ErrorCategory::StorageFailure, "storage-failure"),
    (ErrorCategory::UncertainOutcome, "uncertain-outcome"),
    (ErrorCategory::Internal, "internal"),
];

const ALL_RETRY_GUIDANCE: [RetryGuidance; 3] = [
    RetryGuidance::DoNotRetry,
    RetryGuidance::RetryAfterBackoff,
    RetryGuidance::SafeToRetry,
];

fn error(category: ErrorCategory, message: &str, retry: RetryGuidance) -> AgentError {
    AgentError::new(category, message, retry).expect("safe static diagnostic builds")
}

fn correlation(entries: &[(&str, &str)]) -> CorrelationData {
    let mut data = CorrelationData::new();
    for (key, value) in entries {
        data.push(*key, *value).expect("safe bounded entry builds");
    }
    data
}

#[test]
fn every_category_has_a_stable_unique_label() {
    let mut seen: Vec<&str> = Vec::new();
    for (category, expected) in ALL_CATEGORIES {
        let label = category.as_str();
        assert_eq!(label, expected, "label for {category:?} is public contract");
        assert!(!label.is_empty(), "label must not be empty");
        assert!(
            label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
            "label {label:?} must stay lowercase hyphenated"
        );
        assert!(!seen.contains(&label), "label {label:?} is duplicated");
        seen.push(label);
    }
    assert_eq!(seen.len(), 13, "every category is covered exactly once");
}

#[test]
fn every_category_round_trips_through_construction_and_display() {
    for (category, label) in ALL_CATEGORIES {
        let message = "operation failed";
        let built = error(category, message, RetryGuidance::DoNotRetry);

        assert_eq!(built.category(), category);
        assert_eq!(built.message(), message);
        assert_eq!(built.retry(), RetryGuidance::DoNotRetry);
        assert!(built.correlation().is_empty());
        assert_eq!(built.to_string(), format!("[{label}] {message}"));
        // The `Debug` name must never stand in for the stable label.
        assert!(!built.to_string().contains(&format!("{category:?}")));
    }
}

#[test]
fn every_retry_guidance_variant_round_trips_through_both_constructors() {
    for guidance in ALL_RETRY_GUIDANCE {
        let direct = error(ErrorCategory::Protocol, "transport failed", guidance);
        assert_eq!(direct.retry(), guidance);

        let with_data = AgentError::with_correlation(
            ErrorCategory::Protocol,
            "transport failed",
            correlation(&[("attempt", "1")]),
            guidance,
        )
        .expect("safe diagnostic builds");
        assert_eq!(with_data.retry(), guidance);
    }
}

#[test]
fn retry_guidance_is_retained_per_error_not_collapsed_to_a_default() {
    let mut built = Vec::new();
    for guidance in ALL_RETRY_GUIDANCE {
        built.push(error(ErrorCategory::Protocol, "transport failed", guidance));
    }

    assert_eq!(built.len(), ALL_RETRY_GUIDANCE.len());
    for (left_index, left) in built.iter().enumerate() {
        for (right_index, right) in built.iter().enumerate() {
            if left_index == right_index {
                continue;
            }
            assert_ne!(left.retry(), right.retry());
            assert_ne!(
                left, right,
                "errors differing only in retry guidance must not compare equal"
            );
        }
    }
}

#[test]
fn display_is_exactly_the_category_label_and_message() {
    let built = error(
        ErrorCategory::InvalidInput,
        "plain failure",
        RetryGuidance::SafeToRetry,
    );
    let display = built.to_string();

    assert_eq!(display, "[invalid-input] plain failure");
    assert_eq!(display, format!("{built}"));
    for leaked in [
        "ErrorCategory",
        "InvalidInput",
        "RetryGuidance",
        "SafeToRetry",
        "AgentError",
        "CorrelationData",
        "Some(",
        "Ok(",
        "Err(",
    ] {
        assert!(
            !display.contains(leaked),
            "display leaked {leaked:?}: {display:?}"
        );
    }
    assert_ne!(
        display,
        format!("{built:?}"),
        "Display must not collapse into Debug output"
    );
}

#[test]
fn display_preserves_message_bytes_without_debug_escaping() {
    let message = "line1\nline2\t\"quoted\" \\ backslash [brackets] é漢字 ";
    let built = error(ErrorCategory::Internal, message, RetryGuidance::DoNotRetry);
    let display = built.to_string();

    assert_eq!(display, format!("[internal] {message}"));
    assert!(display.contains('\n'), "newlines must survive verbatim");
    assert!(
        display.contains("\"quoted\""),
        "quotes must not be backslash-escaped"
    );
    assert!(
        !display.contains("\\n"),
        "Display must not escape like Debug"
    );
    assert!(
        !display.contains("\\\""),
        "Display must not escape like Debug"
    );
    assert!(
        display.ends_with(' '),
        "trailing whitespace must not be trimmed"
    );
}

#[test]
fn display_omits_correlation_keys_and_values() {
    let entries = [
        ("trace-id", "trace-9f3a"),
        ("run-id", "run-42"),
        ("adapter", "read-file"),
    ];
    let built = AgentError::with_correlation(
        ErrorCategory::ToolFailure,
        "tool exited non-zero",
        correlation(&entries),
        RetryGuidance::DoNotRetry,
    )
    .expect("safe diagnostic builds");

    let display = built.to_string();
    assert_eq!(display, "[tool-failure] tool exited non-zero");
    for (key, value) in entries {
        assert!(!display.contains(key), "display leaked key {key:?}");
        assert!(!display.contains(value), "display leaked value {value:?}");
    }
}

#[test]
fn display_ignores_caller_format_flags() {
    let built = error(
        ErrorCategory::Timeout,
        "deadline exceeded",
        RetryGuidance::RetryAfterBackoff,
    );
    let plain = format!("{built}");

    assert_eq!(format!("{built:#}"), plain);
    assert_eq!(format!("{built:>80}"), plain);
    assert_eq!(format!("{built:<80}"), plain);
    assert_eq!(format!("{built:^80}"), plain);
    assert_eq!(format!("{built:.3}"), plain);
}

#[test]
fn agent_error_implements_std_error_without_a_source() {
    let built = error(
        ErrorCategory::StorageFailure,
        "checkpoint write failed",
        RetryGuidance::SafeToRetry,
    );

    let boxed: Box<dyn std::error::Error> = Box::new(built.clone());
    assert_eq!(
        boxed.to_string(),
        "[storage-failure] checkpoint write failed"
    );
    assert!(boxed.source().is_none());

    let boxed_thread_safe: Box<dyn std::error::Error + Send + Sync> = Box::new(built);
    assert!(boxed_thread_safe.source().is_none());
}

#[test]
fn error_build_error_display_is_stable_and_debug_free() {
    let cases = [
        (ErrorBuildError::Empty, "error message is empty"),
        (
            ErrorBuildError::TooLong,
            "error diagnostic exceeds its bound",
        ),
        (
            ErrorBuildError::SuspectedSecret,
            "error diagnostic may contain secret material",
        ),
    ];

    for (build_error, expected) in cases {
        assert_eq!(build_error.to_string(), expected);
        assert_eq!(format!("{build_error}"), expected);
        assert_ne!(format!("{build_error:?}"), expected);
        for leaked in ["ErrorBuildError", "Empty", "TooLong", "SuspectedSecret"] {
            assert!(
                !expected.contains(leaked),
                "display leaked {leaked:?}: {expected:?}"
            );
        }

        let boxed: Box<dyn std::error::Error> = Box::new(build_error);
        assert_eq!(boxed.to_string(), expected);
        assert!(boxed.source().is_none());
    }
}

#[test]
fn documented_bounds_match_enforced_limits() {
    assert_eq!(MAX_MESSAGE_LEN, 1024);
    assert_eq!(MAX_CORRELATION_ENTRIES, 8);
    assert_eq!(MAX_CORRELATION_KEY_LEN, 64);
    assert_eq!(MAX_CORRELATION_VALUE_LEN, 256);
}

#[test]
fn message_bound_is_measured_in_utf8_bytes() {
    let ascii_at_bound = "a".repeat(MAX_MESSAGE_LEN);
    let at_bound = AgentError::new(
        ErrorCategory::Internal,
        ascii_at_bound.as_str(),
        RetryGuidance::DoNotRetry,
    )
    .expect("a message exactly at the byte bound must build");
    assert_eq!(at_bound.message(), ascii_at_bound);
    assert_eq!(at_bound.message().len(), MAX_MESSAGE_LEN, "no truncation");

    let ascii_over_bound = "a".repeat(MAX_MESSAGE_LEN + 1);
    assert_eq!(
        AgentError::new(
            ErrorCategory::Internal,
            ascii_over_bound,
            RetryGuidance::DoNotRetry
        ),
        Err(ErrorBuildError::TooLong)
    );

    let mut exact = "a".repeat(MAX_MESSAGE_LEN - 2);
    exact.push('é');
    assert_eq!(exact.len(), MAX_MESSAGE_LEN, "two-byte char lands on bound");
    let exact_error = AgentError::new(
        ErrorCategory::Internal,
        exact.as_str(),
        RetryGuidance::DoNotRetry,
    )
    .expect("a multi-byte message exactly at the byte bound must build");
    assert_eq!(exact_error.message().len(), MAX_MESSAGE_LEN);

    exact.push('a');
    assert_eq!(exact.len(), MAX_MESSAGE_LEN + 1);
    assert_eq!(
        AgentError::new(ErrorCategory::Internal, exact, RetryGuidance::DoNotRetry),
        Err(ErrorBuildError::TooLong)
    );

    let multibyte_over = "é".repeat(MAX_MESSAGE_LEN / 2 + 1);
    assert!(multibyte_over.len() > MAX_MESSAGE_LEN);
    assert_eq!(
        AgentError::new(
            ErrorCategory::Internal,
            multibyte_over,
            RetryGuidance::DoNotRetry
        ),
        Err(ErrorBuildError::TooLong)
    );
}

#[test]
fn empty_message_is_rejected_for_every_category() {
    for (category, _) in ALL_CATEGORIES {
        assert_eq!(
            AgentError::new(category, "", RetryGuidance::DoNotRetry),
            Err(ErrorBuildError::Empty),
            "empty message must be rejected for {category:?}"
        );
    }
}

#[test]
fn constructors_accept_borrowed_and_owned_messages() {
    let borrowed = error(
        ErrorCategory::Internal,
        "same text",
        RetryGuidance::DoNotRetry,
    );
    let owned = AgentError::new(
        ErrorCategory::Internal,
        String::from("same text"),
        RetryGuidance::DoNotRetry,
    )
    .expect("safe diagnostic builds");
    assert_eq!(borrowed, owned);
    assert_eq!(borrowed.message(), owned.message());

    let explicit = AgentError::with_correlation(
        ErrorCategory::Internal,
        String::from("owned text"),
        CorrelationData::new(),
        RetryGuidance::DoNotRetry,
    )
    .expect("safe diagnostic builds");
    assert_eq!(explicit.message(), "owned text");
}

#[test]
fn new_matches_with_correlation_when_correlation_is_empty() {
    let direct = error(
        ErrorCategory::RateLimited,
        "backoff required",
        RetryGuidance::RetryAfterBackoff,
    );
    let explicit = AgentError::with_correlation(
        ErrorCategory::RateLimited,
        "backoff required",
        CorrelationData::new(),
        RetryGuidance::RetryAfterBackoff,
    )
    .expect("safe diagnostic builds");
    assert_eq!(direct, explicit);
}

#[test]
fn with_correlation_rejects_invalid_messages_like_new() {
    let empty = AgentError::with_correlation(
        ErrorCategory::Internal,
        "",
        correlation(&[("key", "value")]),
        RetryGuidance::DoNotRetry,
    );
    assert_eq!(empty, Err(ErrorBuildError::Empty));

    let too_long = AgentError::with_correlation(
        ErrorCategory::Internal,
        "x".repeat(MAX_MESSAGE_LEN + 1),
        correlation(&[("key", "value")]),
        RetryGuidance::DoNotRetry,
    );
    assert_eq!(too_long, Err(ErrorBuildError::TooLong));

    let secret = AgentError::with_correlation(
        ErrorCategory::Authentication,
        "rejected password=hunter2",
        correlation(&[("key", "value")]),
        RetryGuidance::DoNotRetry,
    );
    assert_eq!(secret, Err(ErrorBuildError::SuspectedSecret));
}

#[test]
fn correlation_entries_are_exposed_in_insertion_order() {
    let entries = [("call", "call-1"), ("tool", "read-file"), ("attempt", "2")];
    let data = correlation(&entries);

    assert_eq!(data.len(), entries.len());
    assert!(!data.is_empty());
    let observed: Vec<(&str, &str)> = data
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    assert_eq!(observed, entries);
    assert_eq!(data, correlation(&entries));
    assert_eq!(data.clone(), data);

    assert!(CorrelationData::default().is_empty());
    assert_eq!(CorrelationData::default(), CorrelationData::new());
}

#[test]
fn with_correlation_exposes_exactly_the_supplied_data() {
    let entries = [("session", "s-1"), ("run", "r-7")];
    let data = correlation(&entries);
    let built = AgentError::with_correlation(
        ErrorCategory::UncertainOutcome,
        "effect state unknown",
        data.clone(),
        RetryGuidance::DoNotRetry,
    )
    .expect("safe diagnostic builds");

    assert_eq!(built.category(), ErrorCategory::UncertainOutcome);
    assert_eq!(built.message(), "effect state unknown");
    assert_eq!(built.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(built.correlation(), &data);
    assert_eq!(built.correlation().len(), entries.len());
}

#[test]
fn equality_distinguishes_every_public_field() {
    let base = AgentError::with_correlation(
        ErrorCategory::Protocol,
        "frame rejected",
        correlation(&[("peer", "adapter-a")]),
        RetryGuidance::RetryAfterBackoff,
    )
    .expect("safe diagnostic builds");
    assert_eq!(base, base.clone());

    let other_category = error(
        ErrorCategory::Timeout,
        "frame rejected",
        RetryGuidance::RetryAfterBackoff,
    );
    assert_ne!(base, other_category);

    let other_message = error(
        ErrorCategory::Protocol,
        "frame accepted",
        RetryGuidance::RetryAfterBackoff,
    );
    assert_ne!(base, other_message);

    let other_retry = error(
        ErrorCategory::Protocol,
        "frame rejected",
        RetryGuidance::SafeToRetry,
    );
    assert_ne!(base, other_retry);

    let other_correlation = AgentError::with_correlation(
        ErrorCategory::Protocol,
        "frame rejected",
        correlation(&[("peer", "adapter-b")]),
        RetryGuidance::RetryAfterBackoff,
    )
    .expect("safe diagnostic builds");
    assert_ne!(base, other_correlation);
}

#[test]
fn every_known_secret_marker_is_rejected_case_insensitively() {
    let markers = [
        "-----BEGIN",
        "Bearer ",
        "Sk-",
        "AKIA",
        "GHP_",
        "XOXB-",
        "PASSWORD=",
        "Passwd=",
        "SECRET=",
        "API_KEY=",
        "ApiKey=",
        "CLIENT_SECRET",
    ];

    for marker in markers {
        let message = format!("upstream rejected: {marker}");
        assert_eq!(
            AgentError::new(
                ErrorCategory::Authentication,
                message,
                RetryGuidance::DoNotRetry
            ),
            Err(ErrorBuildError::SuspectedSecret),
            "message marker {marker:?} must be rejected"
        );

        let mut data = CorrelationData::new();
        assert_eq!(
            data.push("marker", marker),
            Err(ErrorBuildError::SuspectedSecret)
        );
        assert!(data.is_empty(), "rejected values must not be stored");

        let mut data = CorrelationData::new();
        assert_eq!(
            data.push(marker, "value"),
            Err(ErrorBuildError::SuspectedSecret)
        );
        assert!(data.is_empty(), "rejected keys must not be stored");
    }
}

#[test]
fn marker_net_does_not_reject_benign_text() {
    for message in [
        "bearerless auth is unsupported",
        "task risk assessment failed",
        "client secrets rotation required",
    ] {
        assert!(
            AgentError::new(
                ErrorCategory::Authentication,
                message,
                RetryGuidance::DoNotRetry
            )
            .is_ok(),
            "benign text {message:?} must build; the marker net is best effort"
        );
    }
}

#[test]
fn correlation_bounds_are_enforced_on_push() {
    let mut data = CorrelationData::new();
    let key_at_bound = "k".repeat(MAX_CORRELATION_KEY_LEN);
    assert_eq!(data.push(key_at_bound.as_str(), "v"), Ok(()));
    let key_over_bound = "k".repeat(MAX_CORRELATION_KEY_LEN + 1);
    assert_eq!(
        data.push(key_over_bound, "v"),
        Err(ErrorBuildError::TooLong)
    );
    assert_eq!(data.len(), 1, "rejected keys must not be stored");

    let mut data = CorrelationData::new();
    let value_at_bound = "v".repeat(MAX_CORRELATION_VALUE_LEN);
    assert_eq!(data.push("k", value_at_bound.as_str()), Ok(()));
    let value_over_bound = "v".repeat(MAX_CORRELATION_VALUE_LEN + 1);
    assert_eq!(
        data.push("k", value_over_bound),
        Err(ErrorBuildError::TooLong)
    );
    assert_eq!(data.len(), 1, "rejected values must not be stored");

    // `Empty` is documented as covering an empty correlation key, but the
    // current boundary reports it as `TooLong`; pin the actual behavior so a
    // fix is deliberate.
    let mut data = CorrelationData::new();
    assert_eq!(data.push("", "v"), Err(ErrorBuildError::TooLong));
    assert_eq!(
        data.push("k", ""),
        Ok(()),
        "empty values are within the documented bound"
    );
    assert_eq!(data.len(), 1);
}

#[test]
fn correlation_entry_count_bound_is_enforced_without_partial_writes() {
    let mut data = CorrelationData::new();
    for index in 0..MAX_CORRELATION_ENTRIES {
        assert_eq!(
            data.push(format!("key-{index}"), format!("value-{index}")),
            Ok(())
        );
    }
    assert_eq!(data.len(), MAX_CORRELATION_ENTRIES);

    assert_eq!(
        data.push("overflow", "value"),
        Err(ErrorBuildError::TooLong)
    );
    assert_eq!(data.len(), MAX_CORRELATION_ENTRIES);

    let built = AgentError::with_correlation(
        ErrorCategory::Internal,
        "entry bound reached",
        data,
        RetryGuidance::DoNotRetry,
    )
    .expect("full correlation set builds");
    assert_eq!(built.correlation().len(), MAX_CORRELATION_ENTRIES);
}
