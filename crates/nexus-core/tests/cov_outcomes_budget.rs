#![forbid(unsafe_code)]

//! Coverage hardening for outcome budget enforcement.
//!
//! These tests exercise the public `ToolOutcome` boundary only:
//! [`ToolOutcome::from_bounded_content`] and [`ToolOutcome::enforce_budget`].
//! The contract under test:
//! - content is cut to the byte budget on a UTF-8 character boundary and
//!   never splits a character;
//! - a cut is always visible through `is_truncated`, an existing `true` flag
//!   is never cleared, and a flag is never fabricated without a cut;
//! - `status`, `effect`, and `evidence` are preserved verbatim, including the
//!   denied-implies-not-started invariant;
//! - budgets outside `1..=Limits::M0_TEST_TOOL_OUTPUT_BYTES` are rejected as
//!   invalid input instead of being silently widened.
//!
//! Everything is deterministic: fixed strings, fixed budgets, no clocks,
//! randomness, or I/O.

use nexus_core::{
    AgentError, EffectState, ErrorCategory, Evidence, ExecutionStatus, Limits, RetryGuidance,
    ToolOutcome,
};

/// The global M0-test output cap that both constructors enforce.
const CAP: usize = Limits::M0_TEST_TOOL_OUTPUT_BYTES;

fn build(
    status: ExecutionStatus,
    effect: EffectState,
    evidence: Evidence,
    content: impl Into<String>,
    truncated: bool,
) -> ToolOutcome {
    ToolOutcome::new(status, effect, evidence, content, truncated).expect("outcome builds")
}

fn assert_invalid(error: &AgentError, needle: &str) {
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert!(
        error.message().contains(needle),
        "message {:?} should contain {needle:?}",
        error.message()
    );
}

/// Reference implementation of a maximal character-boundary prefix; the
/// production cut must match it for every budget.
fn longest_prefix_within(text: &str, max_bytes: usize) -> &str {
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        let next = index + ch.len_utf8();
        if next > max_bytes {
            break;
        }
        end = next;
    }
    &text[..end]
}

/// A 2-, 3-, or 4-byte character starting one byte short of the global cap
/// must not be split: the cut backs off to the character's first byte.
#[test]
fn from_bounded_content_backs_off_straddling_multibyte_characters_at_the_global_cap() {
    for ch in ['é', '€', '𝄞'] {
        let width = ch.len_utf8();
        let mut raw = "a".repeat(CAP - (width - 1));
        raw.push(ch);
        raw.push_str("tail");
        let outcome = ToolOutcome::from_bounded_content(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            raw.clone(),
            false,
        )
        .expect("bounded outcome builds");

        let expected = "a".repeat(CAP - (width - 1));
        assert_eq!(outcome.content(), expected, "width={width}");
        assert!(raw.starts_with(outcome.content()), "width={width}");
        assert!(outcome.content().is_char_boundary(outcome.content().len()));
        assert!(outcome.content().len() <= CAP);
        assert!(outcome.is_truncated(), "width={width}");
        assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
        assert_eq!(outcome.effect(), EffectState::KnownApplied);
        assert_eq!(outcome.evidence(), Evidence::HostObserved);
    }
}

/// Content of exactly the cap stays whole and a caller-provided flag survives
/// unchanged; only an actual cut flips the flag to `true`.
#[test]
fn from_bounded_content_keeps_exact_cap_and_preserves_caller_truncation_flag() {
    let exact = "x".repeat(CAP);
    let kept = ToolOutcome::from_bounded_content(
        ExecutionStatus::Failed,
        EffectState::Unknown,
        Evidence::Uncertain,
        exact.clone(),
        false,
    )
    .expect("exact-cap content builds");
    assert_eq!(kept.content(), exact);
    assert!(!kept.is_truncated(), "a fitting flag stays false");
    assert_eq!(kept.status(), ExecutionStatus::Failed);
    assert_eq!(kept.effect(), EffectState::Unknown);
    assert_eq!(kept.evidence(), Evidence::Uncertain);

    let mut over = exact.clone();
    over.push('y');
    let cut = ToolOutcome::from_bounded_content(
        ExecutionStatus::Failed,
        EffectState::Unknown,
        Evidence::Uncertain,
        over,
        false,
    )
    .expect("over-cap content is bounded, not rejected");
    assert_eq!(cut.content(), exact);
    assert!(cut.is_truncated(), "an actual cut sets the flag");
    assert_eq!(cut.status(), ExecutionStatus::Failed);
    assert_eq!(cut.effect(), EffectState::Unknown);
    assert_eq!(cut.evidence(), Evidence::Uncertain);

    let flagged = ToolOutcome::from_bounded_content(
        ExecutionStatus::Succeeded,
        EffectState::KnownApplied,
        Evidence::HostObserved,
        "abc",
        true,
    )
    .expect("flagged outcome builds");
    assert!(flagged.is_truncated(), "caller true is preserved");

    let empty = ToolOutcome::from_bounded_content(
        ExecutionStatus::Succeeded,
        EffectState::KnownNotApplied,
        Evidence::HostObserved,
        "",
        false,
    )
    .expect("empty content builds");
    assert!(empty.content().is_empty());
    assert!(!empty.is_truncated());

    assert!(
        ToolOutcome::new(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            "x".repeat(CAP + 1),
            false,
        )
        .is_err(),
        "new rejects over-cap content; from_bounded_content is the bounded path"
    );
}

/// Denied outcomes may use the bounded constructor, but never claim started
/// effects; over-cap denial content is still cut, not rejected.
#[test]
fn from_bounded_content_enforces_denied_implies_not_started_invariant() {
    let denied = ToolOutcome::from_bounded_content(
        ExecutionStatus::Denied,
        EffectState::NotStarted,
        Evidence::HostObserved,
        "x".repeat(CAP + 1),
        false,
    )
    .expect("denied over-cap content still bounds");
    assert_eq!(denied.status(), ExecutionStatus::Denied);
    assert_eq!(denied.effect(), EffectState::NotStarted);
    assert_eq!(denied.content().len(), CAP);
    assert!(denied.is_truncated());

    for effect in [
        EffectState::KnownNotApplied,
        EffectState::KnownApplied,
        EffectState::Unknown,
    ] {
        let error = ToolOutcome::from_bounded_content(
            ExecutionStatus::Denied,
            effect,
            Evidence::HostObserved,
            "tiny",
            false,
        )
        .expect_err("denial must not claim started effects");
        assert_invalid(&error, "denied");
    }
}

/// Sweeping every small budget over a mixed-width string must produce the
/// maximal character-boundary prefix, preserve all fields, and set the flag
/// exactly when bytes were removed.
#[test]
fn enforce_budget_cuts_on_character_boundaries_for_every_small_budget() {
    let text = "aé€𝄞bc";
    assert_eq!(text.len(), 12);
    for budget in 1..=text.len() {
        let outcome = build(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            text,
            false,
        );
        let cut = outcome
            .enforce_budget(budget)
            .expect("in-range budget accepts");
        let expected = longest_prefix_within(text, budget);
        assert_eq!(cut.content(), expected, "budget={budget}");
        assert_eq!(
            cut.is_truncated(),
            expected.len() < text.len(),
            "budget={budget}"
        );
        assert!(cut.content().len() <= budget, "budget={budget}");
        assert!(cut.content().is_char_boundary(cut.content().len()));
        assert!(text.starts_with(cut.content()), "budget={budget}");
        if cut.content().len() < text.len() {
            let next = text[cut.content().len()..]
                .chars()
                .next()
                .expect("remaining text");
            assert!(
                cut.content().len() + next.len_utf8() > budget,
                "cut must be maximal: budget={budget}"
            );
        }
        assert_eq!(cut.status(), ExecutionStatus::Succeeded);
        assert_eq!(cut.effect(), EffectState::KnownApplied);
        assert_eq!(cut.evidence(), Evidence::HostObserved);
    }
}

/// A budget too small for the leading character yields empty content with an
/// explicit truncation flag, never a split character.
#[test]
fn enforce_budget_below_leading_character_yields_empty_truncated_content() {
    let outcome = build(
        ExecutionStatus::TimedOut,
        EffectState::Unknown,
        Evidence::Uncertain,
        "𝄞abc",
        false,
    );
    for budget in 1..=3 {
        let cut = outcome
            .clone()
            .enforce_budget(budget)
            .expect("in-range budget accepts");
        assert!(cut.content().is_empty(), "budget={budget}");
        assert!(cut.is_truncated(), "budget={budget}");
        assert_eq!(cut.status(), ExecutionStatus::TimedOut);
        assert_eq!(cut.effect(), EffectState::Unknown);
        assert_eq!(cut.evidence(), Evidence::Uncertain);
    }

    let cut = outcome
        .clone()
        .enforce_budget(4)
        .expect("leading char fits");
    assert_eq!(cut.content(), "𝄞");
    assert!(cut.is_truncated());
    assert_eq!(cut.status(), ExecutionStatus::TimedOut);
    assert_eq!(cut.effect(), EffectState::Unknown);
    assert_eq!(cut.evidence(), Evidence::Uncertain);

    let kept = outcome.enforce_budget(7).expect("full content fits");
    assert_eq!(kept.content(), "𝄞abc");
    assert!(!kept.is_truncated(), "no cut means no fabricated flag");
}

/// Every execution status, including denial and cancellation with raced
/// effects, keeps its status, effect, and evidence across a budget cut.
#[test]
fn enforce_budget_preserves_status_effect_and_evidence_for_all_statuses() {
    let cases = [
        (
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
        ),
        (
            ExecutionStatus::Failed,
            EffectState::KnownNotApplied,
            Evidence::PluginReported,
        ),
        (
            ExecutionStatus::Denied,
            EffectState::NotStarted,
            Evidence::HostObserved,
        ),
        (
            ExecutionStatus::Cancelled,
            EffectState::Unknown,
            Evidence::Uncertain,
        ),
        (
            ExecutionStatus::Cancelled,
            EffectState::KnownApplied,
            Evidence::PluginReported,
        ),
        (
            ExecutionStatus::TimedOut,
            EffectState::Unknown,
            Evidence::Uncertain,
        ),
    ];
    for (status, effect, evidence) in cases {
        let outcome = build(status, effect, evidence, "abcdef", false);
        let cut = outcome.enforce_budget(3).expect("in-range budget accepts");
        assert_eq!(cut.status(), status);
        assert_eq!(cut.effect(), effect);
        assert_eq!(cut.evidence(), evidence);
        assert_eq!(cut.content(), "abc");
        assert!(cut.is_truncated());
        assert_eq!(cut, build(status, effect, evidence, "abc", true));
    }
}

/// Zero, above-cap, and `usize::MAX` budgets are rejected as invalid input;
/// the cap itself is accepted and never widens content.
#[test]
fn enforce_budget_rejects_invalid_budgets_without_widening() {
    let outcome = build(
        ExecutionStatus::Succeeded,
        EffectState::KnownApplied,
        Evidence::HostObserved,
        "abc",
        false,
    );
    for budget in [0, CAP + 1, usize::MAX] {
        let error = outcome
            .clone()
            .enforce_budget(budget)
            .expect_err("out-of-range budget is rejected");
        assert_invalid(&error, "budget");
    }

    let kept = outcome
        .enforce_budget(CAP)
        .expect("the global cap is accepted");
    assert_eq!(kept.content(), "abc");
    assert!(!kept.is_truncated());
    assert_eq!(kept.status(), ExecutionStatus::Succeeded);
    assert_eq!(kept.effect(), EffectState::KnownApplied);
    assert_eq!(kept.evidence(), Evidence::HostObserved);
}

/// The truncation flag is set only when a cut happens, and a `true` flag is
/// never cleared, including for empty content.
#[test]
fn enforce_budget_sets_truncation_only_on_cut_and_never_clears_it() {
    let cases = [
        ("abc", true, 3, "abc", true),
        ("abc", false, 3, "abc", false),
        ("abc", false, 2, "ab", true),
        ("abc", true, 2, "ab", true),
        ("", false, 1, "", false),
        ("", true, 1, "", true),
        ("é", false, 2, "é", false),
        ("é", false, 1, "", true),
    ];
    for (content, flag, budget, expected, truncated) in cases {
        let cut = build(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            content,
            flag,
        )
        .enforce_budget(budget)
        .expect("in-range budget accepts");
        assert_eq!(
            cut.content(),
            expected,
            "content={content:?} budget={budget}"
        );
        assert_eq!(
            cut.is_truncated(),
            truncated,
            "content={content:?} budget={budget}"
        );
    }
}
