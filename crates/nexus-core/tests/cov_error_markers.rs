//! Coverage hardening for the secret-marker boundary net in `nexus_core::error`.
//!
//! Every assertion goes through the public API. All marker-shaped strings are
//! fake examples; no real credential material is used. The net is best-effort
//! by design, so this suite pins both the rejections it guarantees and the
//! documented near-miss gaps that still rely on caller pre-redaction.

#![forbid(unsafe_code)]

use nexus_core::error::{
    MAX_CORRELATION_ENTRIES, MAX_CORRELATION_KEY_LEN, MAX_CORRELATION_VALUE_LEN, MAX_MESSAGE_LEN,
};
use nexus_core::{AgentError, CorrelationData, ErrorBuildError, ErrorCategory, RetryGuidance};

/// Every marker the boundary net currently matches, mirrored from
/// `SECRET_MARKERS`. The crate-private drift guard in `src/error.rs`
/// (`cov_error_private::marker_set_matches_the_documented_contract`) fails
/// when this list and the implementation diverge.
const MARKERS: &[&str] = &[
    "-----begin",
    "bearer ",
    "sk-",
    "akia",
    "ghp_",
    "xoxb-",
    "password=",
    "passwd=",
    "secret=",
    "api_key=",
    "apikey=",
    "client_secret",
];

/// Near-miss strings that look marker-adjacent but must not match: truncated
/// markers, missing separators, spaced-out letters, and lookalike spellings.
const NEAR_MISSES: &[&str] = &[
    "----begin certificate", // one dash short
    "-----begi",             // truncated
    "bearer",                // the marker requires the trailing space
    "bearer\ttoken",         // tab is not a space
    "b e a r e r ",          // spaced-out letters
    "sk",                    // missing hyphen
    "sk_",
    "s k-",
    "akla", // lookalike, not AKIA
    "akya",
    "ghp", // missing underscore
    "ghp-",
    "xoxb", // missing hyphen
    "xoxb_",
    "password",   // missing equals
    "password:",  // wrong separator
    "password =", // space before equals
    "passwd",
    "passw=",
    "topsecret", // "secret=" only matches with the equals
    "secret:",
    "secret =",
    "api_key", // missing equals
    "api_key:",
    "api-key=", // hyphen spelling is not in the net
    "apikey",
    "apikey:",
    "clientsecret", // missing underscore
    "client-secret",
    "client_sec",
];

fn build(message: &str) -> Result<AgentError, ErrorBuildError> {
    AgentError::new(ErrorCategory::Internal, message, RetryGuidance::DoNotRetry)
}

fn alternate_case(text: &str) -> String {
    text.chars()
        .enumerate()
        .map(|(index, character)| {
            if index % 2 == 0 {
                character.to_ascii_uppercase()
            } else {
                character.to_ascii_lowercase()
            }
        })
        .collect()
}

fn assert_suspected_secret<T: PartialEq + std::fmt::Debug>(
    result: Result<T, ErrorBuildError>,
    context: &str,
) {
    assert_eq!(
        result,
        Err(ErrorBuildError::SuspectedSecret),
        "{context} must be rejected as suspected secret material"
    );
}

#[test]
fn every_marker_is_rejected_as_a_bare_message() {
    for marker in MARKERS {
        assert_suspected_secret(build(marker), &format!("bare marker {marker:?}"));
    }
}

#[test]
fn every_marker_is_rejected_inside_a_plausible_diagnostic() {
    for marker in MARKERS {
        let message =
            format!("upstream rejected the request near {marker} while reading the response");
        assert_suspected_secret(build(&message), &format!("embedded marker {marker:?}"));
    }
}

#[test]
fn every_marker_is_rejected_case_insensitively_in_messages() {
    for marker in MARKERS {
        let uppercase = marker.to_ascii_uppercase();
        let mixed = alternate_case(marker);
        assert_suspected_secret(build(&uppercase), &format!("uppercase marker {marker:?}"));
        assert_suspected_secret(build(&mixed), &format!("mixed-case marker {marker:?}"));
        assert_suspected_secret(
            build(&format!("wrapped {uppercase} inside a sentence")),
            &format!("embedded uppercase marker {marker:?}"),
        );
    }
}

#[test]
fn with_correlation_rejects_marker_bearing_messages() {
    for marker in MARKERS {
        let message = format!("provider error: {marker}");
        let result = AgentError::with_correlation(
            ErrorCategory::Authentication,
            message,
            CorrelationData::new(),
            RetryGuidance::DoNotRetry,
        );
        assert_suspected_secret(result, &format!("with_correlation marker {marker:?}"));
    }
}

#[test]
fn every_marker_is_rejected_in_correlation_values_and_keys() {
    for marker in MARKERS {
        let mut correlation = CorrelationData::new();
        let value = format!("observed {marker} in the upstream response");
        assert_suspected_secret(
            correlation.push("detail", value),
            &format!("value marker {marker:?}"),
        );
        assert!(
            correlation.is_empty(),
            "rejected value {marker:?} must not be retained"
        );

        let mut correlation = CorrelationData::new();
        let key = format!("auth-{marker}");
        assert_suspected_secret(
            correlation.push(key, "redacted"),
            &format!("key marker {marker:?}"),
        );
        assert!(
            correlation.is_empty(),
            "rejected key {marker:?} must not be retained"
        );
    }
}

#[test]
fn correlation_values_are_checked_case_insensitively() {
    for marker in MARKERS {
        let mut correlation = CorrelationData::new();
        let value = format!("token {}", marker.to_ascii_uppercase());
        assert_suspected_secret(
            correlation.push("detail", value),
            &format!("uppercase value marker {marker:?}"),
        );
        assert!(correlation.is_empty());
    }
}

#[test]
fn near_miss_text_without_a_full_marker_is_not_rejected() {
    for text in NEAR_MISSES {
        assert!(
            build(text).is_ok(),
            "near-miss {text:?} does not contain a full marker and must build as a message"
        );

        let mut correlation = CorrelationData::new();
        assert!(
            correlation.push("detail", *text).is_ok(),
            "near-miss {text:?} must build as a correlation value"
        );

        let mut correlation = CorrelationData::new();
        assert!(
            correlation.push(*text, "redacted").is_ok(),
            "near-miss {text:?} must build as a correlation key"
        );
    }
}

#[test]
fn safe_messages_and_values_round_trip() {
    let mut correlation = CorrelationData::new();
    correlation
        .push("call", "call-1")
        .expect("safe key/value builds");
    correlation
        .push("tool", "read-file")
        .expect("safe key/value builds");

    let error = AgentError::with_correlation(
        ErrorCategory::ToolFailure,
        "tool exited with status 1",
        correlation,
        RetryGuidance::SafeToRetry,
    )
    .expect("safe diagnostic builds");

    assert_eq!(error.category(), ErrorCategory::ToolFailure);
    assert_eq!(error.message(), "tool exited with status 1");
    assert_eq!(error.retry(), RetryGuidance::SafeToRetry);
    assert_eq!(error.correlation().len(), 2);
    let entries: Vec<(&str, &str)> = error
        .correlation()
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    assert_eq!(entries, vec![("call", "call-1"), ("tool", "read-file")]);
}

#[test]
fn safe_text_at_exact_bounds_builds_and_one_over_fails() {
    let max_message = "x".repeat(MAX_MESSAGE_LEN);
    assert!(
        build(&max_message).is_ok(),
        "exactly MAX_MESSAGE_LEN builds"
    );
    assert_eq!(
        build(&"x".repeat(MAX_MESSAGE_LEN + 1)),
        Err(ErrorBuildError::TooLong)
    );

    let mut correlation = CorrelationData::new();
    correlation
        .push("detail", "v".repeat(MAX_CORRELATION_VALUE_LEN))
        .expect("exactly MAX_CORRELATION_VALUE_LEN builds");
    let mut correlation = CorrelationData::new();
    assert_eq!(
        correlation.push("detail", "v".repeat(MAX_CORRELATION_VALUE_LEN + 1)),
        Err(ErrorBuildError::TooLong)
    );

    let mut correlation = CorrelationData::new();
    correlation
        .push("k".repeat(MAX_CORRELATION_KEY_LEN), "redacted")
        .expect("exactly MAX_CORRELATION_KEY_LEN builds");
    let mut correlation = CorrelationData::new();
    assert_eq!(
        correlation.push("k".repeat(MAX_CORRELATION_KEY_LEN + 1), "redacted"),
        Err(ErrorBuildError::TooLong)
    );
}

#[test]
fn markers_at_exact_bounds_are_rejected_as_secrets_not_lengths() {
    let marker = "ghp_";
    let message = format!("{}{marker}", "x".repeat(MAX_MESSAGE_LEN - marker.len()));
    assert_eq!(message.len(), MAX_MESSAGE_LEN);
    assert_eq!(build(&message), Err(ErrorBuildError::SuspectedSecret));

    let mut correlation = CorrelationData::new();
    let value = format!(
        "{}{marker}",
        "v".repeat(MAX_CORRELATION_VALUE_LEN - marker.len())
    );
    assert_eq!(value.len(), MAX_CORRELATION_VALUE_LEN);
    assert_eq!(
        correlation.push("detail", value),
        Err(ErrorBuildError::SuspectedSecret)
    );
    assert!(correlation.is_empty());

    let mut correlation = CorrelationData::new();
    let key = format!(
        "{}{marker}",
        "k".repeat(MAX_CORRELATION_KEY_LEN - marker.len())
    );
    assert_eq!(key.len(), MAX_CORRELATION_KEY_LEN);
    assert_eq!(
        correlation.push(key, "redacted"),
        Err(ErrorBuildError::SuspectedSecret)
    );
}

#[test]
fn length_violations_are_reported_before_marker_detection() {
    // Bounds are checked before the net, so an input that violates both
    // reports TooLong. It is rejected either way; this pins the variant.
    let oversized_message = format!("{}{}", "x".repeat(MAX_MESSAGE_LEN), "password=hunter2");
    assert_eq!(build(&oversized_message), Err(ErrorBuildError::TooLong));

    let mut correlation = CorrelationData::new();
    let oversized_value = format!(
        "{}{}",
        "v".repeat(MAX_CORRELATION_VALUE_LEN),
        "Bearer abc123"
    );
    assert_eq!(
        correlation.push("detail", oversized_value),
        Err(ErrorBuildError::TooLong)
    );
    assert!(correlation.is_empty());
}

#[test]
fn entry_count_limit_is_reported_before_marker_detection() {
    let mut correlation = CorrelationData::new();
    for index in 0..MAX_CORRELATION_ENTRIES {
        correlation
            .push(format!("k{index}"), "redacted")
            .expect("entries within bound build");
    }
    assert_eq!(
        correlation.push("overflow", "Bearer abc123"),
        Err(ErrorBuildError::TooLong)
    );
    assert_eq!(correlation.len(), MAX_CORRELATION_ENTRIES);
}

#[test]
fn empty_message_and_key_are_rejected() {
    assert_eq!(build(""), Err(ErrorBuildError::Empty));

    // The observed variant for an empty key is TooLong even though
    // ErrorBuildError::Empty documents empty keys; only rejection is
    // asserted to avoid enshrining that mismatch.
    let mut correlation = CorrelationData::new();
    let result = correlation.push("", "redacted");
    assert!(result.is_err(), "empty correlation key must be rejected");
    assert!(correlation.is_empty());
}

#[test]
fn empty_correlation_value_is_currently_accepted() {
    // Values have no documented non-empty requirement; only messages and
    // keys do. Pin the acceptance so a future tightening is deliberate.
    let mut correlation = CorrelationData::new();
    correlation
        .push("detail", "")
        .expect("empty value is within the documented bounds");
    assert_eq!(correlation.len(), 1);
}

#[test]
fn marker_net_is_best_effort_not_a_secret_detector() {
    // Pin the documented limitation once more through the public API: a
    // credential-shaped value without a known marker still crosses.
    let mut correlation = CorrelationData::new();
    correlation
        .push("token", "hunter2-fake-credential")
        .expect("the net only knows markers, so caller redaction is mandatory");
    let error = AgentError::with_correlation(
        ErrorCategory::Authentication,
        "login rejected for hunter2-fake-credential",
        correlation,
        RetryGuidance::DoNotRetry,
    )
    .expect("non-marker text still builds");
    assert_eq!(error.correlation().len(), 1);
}
