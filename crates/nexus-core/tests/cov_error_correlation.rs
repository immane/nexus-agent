//! Coverage hardening for the error-correlation boundary and the bounded
//! safe-text paths in `nexus-core`.
//!
//! Public API only: [`nexus_core::error`] and its `ApprovalNotice` consumer
//! in [`nexus_core::commands`]. Every secret-shaped string here is a
//! documentation example (`hunter2`, `AKIAIOSFODNN7EXAMPLE`, ...), never a
//! live credential. The tests are deterministic and touch no clock, file,
//! network, or process state.

#![forbid(unsafe_code)]

use std::time::Duration;

use nexus_core::commands::{ApprovalNotice, MAX_SUMMARY_BYTES};
use nexus_core::error::{
    AgentError, CorrelationData, ErrorBuildError, ErrorCategory, MAX_CORRELATION_ENTRIES,
    MAX_CORRELATION_KEY_LEN, MAX_CORRELATION_VALUE_LEN, MAX_MESSAGE_LEN, RetryGuidance,
};
use nexus_core::{ApprovalId, CallId, PersistenceState, RunFinished, RunOutcome};

/// Known marker shapes from the best-effort net, written as documentation
/// examples. The net matches case-insensitively, so each shape is exercised
/// in both cases. All strings stay well below the key and summary bounds so
/// a rejection here can only come from the marker net.
const SECRET_SHAPED: &[&str] = &[
    "-----BEGIN PRIVATE KEY-----",
    "authorization: Bearer abc123",
    "key sk-live-0000",
    "credential AKIAIOSFODNN7EXAMPLE",
    "token ghp_0000000000000000000000000000000000",
    "slack xoxb-0000-0000",
    "password=hunter2",
    "passwd=hunter2",
    "secret=AAAA",
    "api_key=AAAA",
    "apikey=AAAA",
    "client_secret=AAAA",
];

fn approval() -> ApprovalNotice {
    ApprovalNotice::new(
        ApprovalId::new("appr-1").expect("valid example approval id"),
        CallId::new("call-1").expect("valid example call id"),
        "delete directory",
        "project scope",
        Duration::from_secs(120),
    )
    .expect("marker-free bounded notice builds")
}

fn correlation(entries: &[(&str, &str)]) -> CorrelationData {
    let mut data = CorrelationData::new();
    for (key, value) in entries {
        data.push(*key, *value)
            .expect("fixture entry is bounded and marker-free");
    }
    data
}

#[test]
fn public_bounds_are_the_documented_values() {
    // The boundary tests below derive from these constants; pinning the
    // values catches accidental drift in the published contract.
    assert_eq!(MAX_MESSAGE_LEN, 1024);
    assert_eq!(MAX_CORRELATION_ENTRIES, 8);
    assert_eq!(MAX_CORRELATION_KEY_LEN, 64);
    assert_eq!(MAX_CORRELATION_VALUE_LEN, 256);
    assert_eq!(MAX_SUMMARY_BYTES, MAX_MESSAGE_LEN);
}

#[test]
fn message_bound_is_byte_exact() {
    for len in [1, MAX_MESSAGE_LEN - 1, MAX_MESSAGE_LEN] {
        let message = "m".repeat(len);
        let error = AgentError::new(ErrorCategory::Internal, message, RetryGuidance::DoNotRetry)
            .expect("message within the byte bound builds");
        assert_eq!(error.message().len(), len);
    }

    let error = AgentError::new(
        ErrorCategory::Internal,
        "m".repeat(MAX_MESSAGE_LEN + 1),
        RetryGuidance::DoNotRetry,
    )
    .expect_err("one byte past the message bound is rejected");
    assert_eq!(error, ErrorBuildError::TooLong);
}

#[test]
fn empty_message_is_rejected_as_empty() {
    assert_eq!(
        AgentError::new(ErrorCategory::Internal, "", RetryGuidance::DoNotRetry),
        Err(ErrorBuildError::Empty)
    );
}

#[test]
fn message_bound_counts_utf8_bytes_not_chars() {
    // U+00E9 is two UTF-8 bytes, so 512 chars exactly fill the 1024-byte
    // bound while carrying only half the character count.
    let exact = "é".repeat(MAX_MESSAGE_LEN / 2);
    assert_eq!(exact.len(), MAX_MESSAGE_LEN);
    assert_eq!(exact.chars().count(), MAX_MESSAGE_LEN / 2);
    let error = AgentError::new(ErrorCategory::Internal, exact, RetryGuidance::DoNotRetry)
        .expect("byte-exact multibyte message builds");
    assert_eq!(error.message().len(), MAX_MESSAGE_LEN);

    let over = "é".repeat(MAX_MESSAGE_LEN / 2 + 1);
    assert_eq!(over.len(), MAX_MESSAGE_LEN + 2);
    assert_eq!(
        AgentError::new(ErrorCategory::Internal, over, RetryGuidance::DoNotRetry),
        Err(ErrorBuildError::TooLong)
    );
}

#[test]
fn correlation_key_bound_is_byte_exact() {
    for len in [1, MAX_CORRELATION_KEY_LEN - 1, MAX_CORRELATION_KEY_LEN] {
        let mut data = CorrelationData::new();
        data.push("k".repeat(len), "value")
            .expect("key within the byte bound builds");
        assert_eq!(data.len(), 1);
        let (key, value) = data.iter().next().expect("one entry is present");
        assert_eq!(key.len(), len);
        assert_eq!(value, "value");
    }

    let mut data = CorrelationData::new();
    assert_eq!(
        data.push("k".repeat(MAX_CORRELATION_KEY_LEN + 1), "value"),
        Err(ErrorBuildError::TooLong)
    );
    assert!(data.is_empty(), "a rejected key leaves the data untouched");
}

#[test]
fn empty_correlation_key_is_rejected_with_too_long() {
    // `ErrorBuildError::Empty` documents an empty correlation key, but
    // `CorrelationData::push` maps every key-shape failure, including an
    // empty key, to `TooLong`. This pins the implemented behavior; aligning
    // the variant with its documentation must update this assertion.
    let mut data = CorrelationData::new();
    assert_eq!(data.push("", "value"), Err(ErrorBuildError::TooLong));
    assert!(data.is_empty());
}

#[test]
fn correlation_value_bound_is_byte_exact_and_empty_values_are_allowed() {
    let mut data = CorrelationData::new();
    data.push("empty", "")
        .expect("an empty value carries no secret material");
    let (_, value) = data.iter().next().expect("one entry is present");
    assert!(value.is_empty());

    for len in [1, MAX_CORRELATION_VALUE_LEN - 1, MAX_CORRELATION_VALUE_LEN] {
        let mut data = CorrelationData::new();
        data.push("key", "v".repeat(len))
            .expect("value within the byte bound builds");
        let (_, value) = data.iter().next().expect("one entry is present");
        assert_eq!(value.len(), len);
    }

    let mut data = CorrelationData::new();
    assert_eq!(
        data.push("key", "v".repeat(MAX_CORRELATION_VALUE_LEN + 1)),
        Err(ErrorBuildError::TooLong)
    );
    assert!(data.is_empty());
}

#[test]
fn correlation_bounds_count_utf8_bytes_not_chars() {
    let exact_value = "é".repeat(MAX_CORRELATION_VALUE_LEN / 2);
    assert_eq!(exact_value.len(), MAX_CORRELATION_VALUE_LEN);
    let mut data = CorrelationData::new();
    data.push("é".repeat(MAX_CORRELATION_KEY_LEN / 2), exact_value)
        .expect("byte-exact multibyte key and value build");
    let (key, value) = data.iter().next().expect("one entry is present");
    assert_eq!(key.len(), MAX_CORRELATION_KEY_LEN);
    assert_eq!(value.len(), MAX_CORRELATION_VALUE_LEN);

    let mut data = CorrelationData::new();
    assert_eq!(
        data.push("é".repeat(MAX_CORRELATION_KEY_LEN / 2 + 1), "value"),
        Err(ErrorBuildError::TooLong)
    );
    assert_eq!(
        data.push("key", "é".repeat(MAX_CORRELATION_VALUE_LEN / 2 + 1)),
        Err(ErrorBuildError::TooLong)
    );
}

#[test]
fn entry_count_bound_covers_per_entry_overhead() {
    // One-byte keys and empty values keep text bytes minimal, but each entry
    // still owns two `String`s and a tuple slot. The count bound caps that
    // object overhead independently of the byte bounds.
    let mut data = CorrelationData::new();
    for index in 0..MAX_CORRELATION_ENTRIES {
        data.push(format!("k{index}"), "")
            .expect("entry within the count bound builds");
    }
    assert_eq!(data.len(), MAX_CORRELATION_ENTRIES);
    assert!(!data.is_empty());
    let before: Vec<(String, String)> = data.iter().cloned().collect();

    assert_eq!(
        data.push("extra", ""),
        Err(ErrorBuildError::TooLong),
        "the count bound rejects the next entry regardless of tiny payloads"
    );
    assert_eq!(data.len(), MAX_CORRELATION_ENTRIES);
    let after: Vec<(String, String)> = data.iter().cloned().collect();
    assert_eq!(after, before, "a rejected entry does not consume capacity");
}

#[test]
fn correlation_iteration_preserves_insertion_order() {
    let data = correlation(&[("first", "1"), ("second", "2"), ("third", "3")]);
    assert_eq!(data.len(), 3);
    assert_eq!(data.iter().len(), data.len());
    let keys: Vec<&str> = data.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(keys, ["first", "second", "third"]);
}

#[test]
fn maximum_diagnostic_accounts_for_every_carried_byte() {
    // Worst case carried text: one maximum message plus eight entries at the
    // key and value bounds. Per-entry object overhead is covered by the
    // count bound, so this total is finite and fully accounted.
    let message = "m".repeat(MAX_MESSAGE_LEN);
    let key = "k".repeat(MAX_CORRELATION_KEY_LEN);
    let value = "v".repeat(MAX_CORRELATION_VALUE_LEN);
    let mut data = CorrelationData::new();
    for _ in 0..MAX_CORRELATION_ENTRIES {
        data.push(key.clone(), value.clone())
            .expect("maximum-size entry builds");
    }

    let error = AgentError::with_correlation(
        ErrorCategory::UncertainOutcome,
        message.clone(),
        data.clone(),
        RetryGuidance::RetryAfterBackoff,
    )
    .expect("the maximum bounded diagnostic builds");

    assert_eq!(error.message(), message);
    assert_eq!(error.category(), ErrorCategory::UncertainOutcome);
    assert_eq!(error.retry(), RetryGuidance::RetryAfterBackoff);
    assert_eq!(error.correlation(), &data);
    assert_eq!(error.correlation().iter().len(), MAX_CORRELATION_ENTRIES);

    let carried: usize = error.message().len()
        + error
            .correlation()
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum::<usize>();
    assert_eq!(
        carried,
        MAX_MESSAGE_LEN
            + MAX_CORRELATION_ENTRIES * (MAX_CORRELATION_KEY_LEN + MAX_CORRELATION_VALUE_LEN)
    );
    assert_eq!(carried, 3_584);
}

#[test]
fn with_correlation_enforces_message_and_count_bounds() {
    let safe = correlation(&[("call", "call-1")]);
    assert_eq!(
        AgentError::with_correlation(
            ErrorCategory::Internal,
            "",
            safe.clone(),
            RetryGuidance::DoNotRetry
        ),
        Err(ErrorBuildError::Empty)
    );
    assert_eq!(
        AgentError::with_correlation(
            ErrorCategory::Internal,
            "m".repeat(MAX_MESSAGE_LEN + 1),
            safe.clone(),
            RetryGuidance::DoNotRetry
        ),
        Err(ErrorBuildError::TooLong)
    );
    assert_eq!(
        AgentError::with_correlation(
            ErrorCategory::Authentication,
            "password=hunter2",
            safe,
            RetryGuidance::DoNotRetry
        ),
        Err(ErrorBuildError::SuspectedSecret)
    );

    let full = correlation(&[
        ("a", "1"),
        ("b", "2"),
        ("c", "3"),
        ("d", "4"),
        ("e", "5"),
        ("f", "6"),
        ("g", "7"),
        ("h", "8"),
    ]);
    let error = AgentError::with_correlation(
        ErrorCategory::Internal,
        "bounded diagnostic",
        full,
        RetryGuidance::SafeToRetry,
    )
    .expect("eight correlation entries are accepted");
    assert_eq!(error.correlation().len(), MAX_CORRELATION_ENTRIES);
}

#[test]
fn push_checks_count_and_bounds_before_the_marker_net() {
    let mut data = CorrelationData::new();
    let oversized_secret_value = format!("password={}", "x".repeat(MAX_CORRELATION_VALUE_LEN));
    assert_eq!(
        data.push("key", oversized_secret_value),
        Err(ErrorBuildError::TooLong),
        "an oversized secret-shaped value reports the bound, not the marker"
    );
    let oversized_secret_key = format!("sk-{}", "x".repeat(MAX_CORRELATION_KEY_LEN));
    assert_eq!(
        data.push(oversized_secret_key, "value"),
        Err(ErrorBuildError::TooLong)
    );

    for index in 0..MAX_CORRELATION_ENTRIES {
        data.push(format!("k{index}"), "v")
            .expect("entry within the count bound builds");
    }
    assert_eq!(
        data.push("password=", "v"),
        Err(ErrorBuildError::TooLong),
        "the count bound wins over the marker net"
    );
    assert_eq!(
        data.push("k", "Bearer abc123"),
        Err(ErrorBuildError::TooLong)
    );
}

#[test]
fn message_marker_net_matches_known_shapes_case_insensitively() {
    for secret in SECRET_SHAPED {
        for candidate in [secret.to_string(), secret.to_uppercase()] {
            assert_eq!(
                AgentError::new(
                    ErrorCategory::Authentication,
                    candidate.as_str(),
                    RetryGuidance::DoNotRetry
                ),
                Err(ErrorBuildError::SuspectedSecret),
                "marker-shaped message {candidate:?} must not cross the boundary"
            );
        }
    }
}

#[test]
fn correlation_marker_net_covers_keys_and_values() {
    for secret in SECRET_SHAPED {
        let mut data = CorrelationData::new();
        assert_eq!(
            data.push("safe-key", *secret),
            Err(ErrorBuildError::SuspectedSecret),
            "marker-shaped value {secret:?} must not enter correlation data"
        );
        assert_eq!(
            data.push(*secret, "safe-value"),
            Err(ErrorBuildError::SuspectedSecret),
            "marker-shaped key must not enter correlation data"
        );
        assert!(data.is_empty(), "rejected entries never persist");
    }
}

#[test]
fn marker_net_false_positives_and_false_negatives_are_pinned() {
    let mut data = CorrelationData::new();
    // False positive: "sk-" matches inside ordinary hyphenated words.
    assert_eq!(
        data.push("task-1", "call-1"),
        Err(ErrorBuildError::SuspectedSecret)
    );
    assert_eq!(
        data.push("risk-free", "v"),
        Err(ErrorBuildError::SuspectedSecret)
    );
    // False negative: the net does not recognize a secret without a marker.
    assert!(
        AgentError::new(
            ErrorCategory::Authentication,
            "login rejected for hunter2",
            RetryGuidance::DoNotRetry
        )
        .is_ok(),
        "a passing check never proves the text is secret-free"
    );
    // "secret=" is a marker, bare "secret" is not; "client_secret" needs no
    // "=" because the marker is the whole word.
    data.push("secret", "rotation complete")
        .expect("bare 'secret' is not a marker shape");
    assert_eq!(
        data.push("client_secret", "v"),
        Err(ErrorBuildError::SuspectedSecret)
    );
}

#[test]
fn message_bounds_are_checked_before_the_marker_net() {
    let oversized = format!("password={}", "x".repeat(MAX_MESSAGE_LEN));
    assert_eq!(
        AgentError::new(
            ErrorCategory::Authentication,
            oversized,
            RetryGuidance::DoNotRetry
        ),
        Err(ErrorBuildError::TooLong)
    );
    assert_eq!(
        AgentError::new(
            ErrorCategory::Authentication,
            "password=hunter2",
            RetryGuidance::DoNotRetry
        ),
        Err(ErrorBuildError::SuspectedSecret)
    );
}

#[test]
fn error_build_error_display_and_trait_are_stable() {
    assert_eq!(ErrorBuildError::Empty.to_string(), "error message is empty");
    assert_eq!(
        ErrorBuildError::TooLong.to_string(),
        "error diagnostic exceeds its bound"
    );
    assert_eq!(
        ErrorBuildError::SuspectedSecret.to_string(),
        "error diagnostic may contain secret material"
    );
    let as_error: &dyn std::error::Error = &ErrorBuildError::SuspectedSecret;
    assert!(as_error.source().is_none());
}

#[test]
fn every_category_has_a_unique_kebab_case_label() {
    let categories = [
        ErrorCategory::InvalidInput,
        ErrorCategory::UnsupportedCapability,
        ErrorCategory::Authentication,
        ErrorCategory::PermissionDenied,
        ErrorCategory::RateLimited,
        ErrorCategory::Protocol,
        ErrorCategory::Timeout,
        ErrorCategory::Cancelled,
        ErrorCategory::ResourceLimit,
        ErrorCategory::ToolFailure,
        ErrorCategory::StorageFailure,
        ErrorCategory::UncertainOutcome,
        ErrorCategory::Internal,
    ];
    let labels: Vec<&str> = categories
        .iter()
        .map(|category| category.as_str())
        .collect();
    assert_eq!(
        labels,
        [
            "invalid-input",
            "unsupported-capability",
            "authentication",
            "permission-denied",
            "rate-limited",
            "protocol",
            "timeout",
            "cancelled",
            "resource-limit",
            "tool-failure",
            "storage-failure",
            "uncertain-outcome",
            "internal",
        ]
    );
    let unique: std::collections::BTreeSet<&str> = labels.iter().copied().collect();
    assert_eq!(unique.len(), categories.len());
}

#[test]
fn retry_guidance_is_preserved_for_every_variant() {
    for guidance in [
        RetryGuidance::DoNotRetry,
        RetryGuidance::RetryAfterBackoff,
        RetryGuidance::SafeToRetry,
    ] {
        let error = AgentError::new(ErrorCategory::RateLimited, "rate limited", guidance)
            .expect("bounded diagnostic builds");
        assert_eq!(error.retry(), guidance);
        assert!(error.correlation().is_empty());
    }
}

#[test]
fn agent_error_display_clone_and_equality_are_stable() {
    let data = correlation(&[("call", "call-1")]);
    let error = AgentError::with_correlation(
        ErrorCategory::Timeout,
        "tool deadline exceeded",
        data,
        RetryGuidance::DoNotRetry,
    )
    .expect("bounded diagnostic builds");
    assert_eq!(error.to_string(), "[timeout] tool deadline exceeded");

    let cloned = error.clone();
    assert_eq!(cloned, error);
    assert_eq!(cloned.correlation(), error.correlation());

    let without = AgentError::new(
        ErrorCategory::Timeout,
        "tool deadline exceeded",
        RetryGuidance::DoNotRetry,
    )
    .expect("bounded diagnostic builds");
    assert_ne!(without, error, "correlation participates in equality");

    let as_error: &dyn std::error::Error = &error;
    assert!(as_error.source().is_none());
    assert_eq!(as_error.to_string(), "[timeout] tool deadline exceeded");
}

#[test]
fn approval_notice_summary_and_scope_share_the_safe_text_boundary() {
    for len in [1, MAX_SUMMARY_BYTES - 1, MAX_SUMMARY_BYTES] {
        let summary = "s".repeat(len);
        let scope = "c".repeat(len);
        let notice = ApprovalNotice::new(
            ApprovalId::new("appr-1").expect("valid example approval id"),
            CallId::new("call-1").expect("valid example call id"),
            summary.clone(),
            scope.clone(),
            Duration::from_secs(120),
        )
        .expect("bounded marker-free summary and scope build");
        assert_eq!(notice.summary, summary);
        assert_eq!(notice.scope_summary, scope);
    }

    let error = ApprovalNotice::new(
        ApprovalId::new("appr-1").expect("valid"),
        CallId::new("call-1").expect("valid"),
        "s".repeat(MAX_SUMMARY_BYTES + 1),
        "project scope",
        Duration::from_secs(120),
    )
    .expect_err("one byte past the summary bound is rejected");
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(error.message(), "approval summary is invalid");
    assert!(error.correlation().is_empty());

    let error = ApprovalNotice::new(
        ApprovalId::new("appr-1").expect("valid"),
        CallId::new("call-1").expect("valid"),
        "delete directory",
        "c".repeat(MAX_SUMMARY_BYTES + 1),
        Duration::from_secs(120),
    )
    .expect_err("one byte past the scope bound is rejected");
    assert_eq!(error.message(), "approval scope is invalid");

    assert!(
        ApprovalNotice::new(
            ApprovalId::new("appr-1").expect("valid"),
            CallId::new("call-1").expect("valid"),
            "",
            "project scope",
            Duration::from_secs(120),
        )
        .is_err(),
        "an empty summary is rejected"
    );
    assert!(
        ApprovalNotice::new(
            ApprovalId::new("appr-1").expect("valid"),
            CallId::new("call-1").expect("valid"),
            "delete directory",
            "",
            Duration::from_secs(120),
        )
        .is_err(),
        "an empty scope is rejected"
    );
}

#[test]
fn approval_notice_diagnostics_never_echo_rejected_text() {
    let error = ApprovalNotice::new(
        ApprovalId::new("appr-1").expect("valid"),
        CallId::new("call-1").expect("valid"),
        "password=hunter2",
        "project scope",
        Duration::from_secs(120),
    )
    .expect_err("marker-shaped summary is rejected");
    assert_eq!(
        error.to_string(),
        "[invalid-input] approval summary is invalid"
    );
    assert!(!error.to_string().contains("hunter2"));
    assert!(!error.to_string().contains("password"));

    let error = approval()
        .with_args_preview("api_key=AAAA")
        .expect_err("marker-shaped preview is rejected");
    assert_eq!(
        error.to_string(),
        "[invalid-input] approval args preview is invalid"
    );
    assert!(!error.to_string().contains("AAAA"));
}

#[test]
fn approval_notice_args_preview_boundary_matrix() {
    let base = approval();
    assert_eq!(base.args_preview(), None, "preview is absent by default");

    let exact = "p".repeat(MAX_SUMMARY_BYTES);
    let notice = base
        .clone()
        .with_args_preview(exact.clone())
        .expect("preview at the byte bound attaches");
    assert_eq!(notice.args_preview(), Some(exact.as_str()));
    assert_eq!(notice.approval, base.approval);
    assert_eq!(notice.call, base.call);
    assert_eq!(notice.summary, base.summary);
    assert_eq!(notice.scope_summary, base.scope_summary);
    assert_eq!(notice.expires_at_elapsed, base.expires_at_elapsed);

    for invalid in [
        String::new(),
        "p".repeat(MAX_SUMMARY_BYTES + 1),
        "secret=AAAA".to_owned(),
    ] {
        assert!(
            base.clone().with_args_preview(invalid).is_err(),
            "invalid previews never attach"
        );
    }
    assert_eq!(
        base.args_preview(),
        None,
        "a failed attach leaves the source notice unchanged"
    );
}

#[test]
fn approval_notice_validate_rechecks_public_fields() {
    let mut notice = approval();
    assert!(notice.validate().is_ok());

    notice.summary = String::new();
    assert!(
        notice.validate().is_err(),
        "empty mutated summary is rejected"
    );
    notice.summary = "s".repeat(MAX_SUMMARY_BYTES + 1);
    assert!(
        notice.validate().is_err(),
        "oversized mutated summary is rejected"
    );
    notice.summary = "api_key=AAAA".to_owned();
    assert!(
        notice.validate().is_err(),
        "marker-shaped mutated summary is rejected"
    );
    notice.summary = "delete directory".to_owned();
    assert!(notice.validate().is_ok());

    notice.scope_summary = "client_secret".to_owned();
    assert!(
        notice.validate().is_err(),
        "marker-shaped mutated scope is rejected"
    );
    notice.scope_summary = "project scope".to_owned();
    assert!(notice.validate().is_ok());

    notice.args_preview = Some(String::new());
    assert!(
        notice.validate().is_err(),
        "empty mutated preview is rejected"
    );
    notice.args_preview = Some("p".repeat(MAX_SUMMARY_BYTES + 1));
    assert!(
        notice.validate().is_err(),
        "oversized mutated preview is rejected"
    );
    notice.args_preview = Some("password=hunter2".to_owned());
    assert!(
        notice.validate().is_err(),
        "marker-shaped mutated preview is rejected"
    );
    notice.args_preview = None;
    assert!(
        notice.validate().is_ok(),
        "clearing the preview restores validity"
    );
}

#[test]
fn approval_notice_rejects_every_known_marker_in_every_field() {
    for secret in SECRET_SHAPED {
        assert!(
            ApprovalNotice::new(
                ApprovalId::new("appr-1").expect("valid"),
                CallId::new("call-1").expect("valid"),
                *secret,
                "project scope",
                Duration::from_secs(120),
            )
            .is_err(),
            "marker-shaped summary {secret:?} is rejected"
        );
        assert!(
            ApprovalNotice::new(
                ApprovalId::new("appr-1").expect("valid"),
                CallId::new("call-1").expect("valid"),
                "delete directory",
                *secret,
                Duration::from_secs(120),
            )
            .is_err(),
            "marker-shaped scope {secret:?} is rejected"
        );
        assert!(
            approval().with_args_preview(*secret).is_err(),
            "marker-shaped preview {secret:?} is rejected"
        );

        // The net is case-insensitive.
        let upper = secret.to_uppercase();
        assert!(
            approval().with_args_preview(upper.clone()).is_err(),
            "uppercase marker {upper:?} is rejected"
        );
    }
}

#[test]
fn safe_text_bound_counts_utf8_bytes_in_summary_scope_and_preview() {
    let exact = "é".repeat(MAX_SUMMARY_BYTES / 2);
    assert_eq!(exact.len(), MAX_SUMMARY_BYTES);
    assert_eq!(exact.chars().count(), MAX_SUMMARY_BYTES / 2);

    let notice = ApprovalNotice::new(
        ApprovalId::new("appr-1").expect("valid"),
        CallId::new("call-1").expect("valid"),
        exact.clone(),
        exact.clone(),
        Duration::from_secs(120),
    )
    .expect("byte-exact multibyte summary and scope build");
    let notice = notice
        .with_args_preview(exact.clone())
        .expect("byte-exact multibyte preview attaches");
    assert_eq!(notice.summary.len(), MAX_SUMMARY_BYTES);
    assert_eq!(notice.scope_summary.len(), MAX_SUMMARY_BYTES);
    assert_eq!(notice.args_preview().map(str::len), Some(MAX_SUMMARY_BYTES));

    let over = "é".repeat(MAX_SUMMARY_BYTES / 2 + 1);
    assert_eq!(over.len(), MAX_SUMMARY_BYTES + 2);
    assert!(
        ApprovalNotice::new(
            ApprovalId::new("appr-1").expect("valid"),
            CallId::new("call-1").expect("valid"),
            over.clone(),
            "project scope",
            Duration::from_secs(120),
        )
        .is_err(),
        "a two-byte character past the bound is rejected"
    );
    assert!(approval().with_args_preview(over).is_err());

    // A single two-byte character crosses the bound even though the
    // character count is one below it.
    let mixed = "a".repeat(MAX_SUMMARY_BYTES - 1) + "é";
    assert_eq!(mixed.len(), MAX_SUMMARY_BYTES + 1);
    assert!(approval().with_args_preview(mixed).is_err());
}

#[test]
fn correlation_survives_the_run_finished_carrier_unchanged() {
    let data = correlation(&[("call", "call-1"), ("attempt", "3")]);
    let error = AgentError::with_correlation(
        ErrorCategory::UncertainOutcome,
        "effect state inconclusive",
        data,
        RetryGuidance::DoNotRetry,
    )
    .expect("bounded diagnostic builds");
    let finished = RunFinished::new(RunOutcome::Failed, PersistenceState::Ephemeral, None)
        .expect("ephemeral failed record builds")
        .with_error(error.clone());

    let carried = finished.error().expect("attached error is retained");
    assert_eq!(carried, &error);
    assert_eq!(carried.correlation().len(), 2);
    let keys: Vec<&str> = carried
        .correlation()
        .iter()
        .map(|(key, _)| key.as_str())
        .collect();
    assert_eq!(keys, ["call", "attempt"]);
}
