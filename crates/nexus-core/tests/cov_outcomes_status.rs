#![forbid(unsafe_code)]

//! Coverage hardening for `ToolOutcome` status/effect/evidence combinations.
//!
//! Public-API integration checks pinning the documented outcome matrix:
//! - every canonical status shape builds and preserves its exact
//!   status/effect/evidence triple;
//! - a denial requires [`EffectState::NotStarted`]; claiming a started effect
//!   is rejected as invalid input through both construction paths;
//! - cancellation and timeout outcomes carry [`EffectState::Unknown`] plus
//!   [`Evidence::Uncertain`], and no bounding step upgrades that uncertainty
//!   into fabricated certainty;
//! - effects known to have raced with cancellation stay recorded rather than
//!   being rewritten as not applied;
//! - success shapes distinguish applied from not-applied effects and
//!   host-observed from plugin-reported evidence;
//! - the content budget behaves identically for every status.
//!
//! The constructor's only cross-field admission rule is denied-implies-
//! not-started; `admission_rule_is_exact_over_the_full_matrix` pins that
//! boundary explicitly. Deterministic: fixed values only, no clocks,
//! randomness, threads, or I/O.

use nexus_core::{
    AgentError, EffectState, ErrorCategory, Evidence, ExecutionStatus, Limits, RetryGuidance,
    ToolOutcome,
};

/// Global M0-test output cap enforced by both constructors.
const CAP: usize = Limits::M0_TEST_TOOL_OUTPUT_BYTES;

/// Every execution status in the outcome vocabulary.
const ALL_STATUSES: [ExecutionStatus; 5] = [
    ExecutionStatus::Succeeded,
    ExecutionStatus::Failed,
    ExecutionStatus::Denied,
    ExecutionStatus::Cancelled,
    ExecutionStatus::TimedOut,
];

/// Every effect state in the outcome vocabulary.
const ALL_EFFECTS: [EffectState; 4] = [
    EffectState::NotStarted,
    EffectState::KnownNotApplied,
    EffectState::KnownApplied,
    EffectState::Unknown,
];

/// Every evidence class in the outcome vocabulary.
const ALL_EVIDENCE: [Evidence; 3] = [
    Evidence::HostObserved,
    Evidence::PluginReported,
    Evidence::Uncertain,
];

/// Canonical shapes produced by host adapters and fakes: success shapes, an
/// honest failure, a not-started denial, and uncertain cancellation/timeout
/// outcomes. Cancellation may additionally carry known raced effects; that
/// shape is pinned separately.
const CANONICAL_SHAPES: [(ExecutionStatus, EffectState, Evidence); 7] = [
    (
        ExecutionStatus::Succeeded,
        EffectState::KnownApplied,
        Evidence::HostObserved,
    ),
    (
        ExecutionStatus::Succeeded,
        EffectState::KnownNotApplied,
        Evidence::HostObserved,
    ),
    (
        ExecutionStatus::Failed,
        EffectState::Unknown,
        Evidence::Uncertain,
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
        ExecutionStatus::TimedOut,
        EffectState::Unknown,
        Evidence::Uncertain,
    ),
];

/// One representative canonical shape per status for content-bound sweeps.
const STATUS_REPRESENTATIVES: [(ExecutionStatus, EffectState, Evidence); 5] = [
    (
        ExecutionStatus::Succeeded,
        EffectState::KnownApplied,
        Evidence::HostObserved,
    ),
    (
        ExecutionStatus::Failed,
        EffectState::Unknown,
        Evidence::Uncertain,
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
        ExecutionStatus::TimedOut,
        EffectState::Unknown,
        Evidence::Uncertain,
    ),
];

fn try_build(
    status: ExecutionStatus,
    effect: EffectState,
    evidence: Evidence,
    content: impl Into<String>,
    truncated: bool,
) -> Result<ToolOutcome, AgentError> {
    ToolOutcome::new(status, effect, evidence, content, truncated)
}

fn build(
    status: ExecutionStatus,
    effect: EffectState,
    evidence: Evidence,
    content: impl Into<String>,
    truncated: bool,
) -> ToolOutcome {
    try_build(status, effect, evidence, content, truncated).expect("canonical outcome builds")
}

/// Asserts the full public error contract of an outcome rejection.
fn assert_invalid(error: &AgentError, message: &str) {
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(error.message(), message);
}

#[test]
fn canonical_shapes_build_and_preserve_every_field() {
    for (status, effect, evidence) in CANONICAL_SHAPES {
        let outcome = build(status, effect, evidence, "detail", false);
        assert_eq!(outcome.status(), status);
        assert_eq!(outcome.effect(), effect);
        assert_eq!(outcome.evidence(), evidence);
        assert_eq!(outcome.content(), "detail");
        assert!(!outcome.is_truncated());

        let cloned = outcome.clone();
        assert_eq!(cloned, outcome, "canonical shape survives a clone");
        assert_eq!(cloned, build(status, effect, evidence, "detail", false));
    }
}

#[test]
fn denied_requires_not_started_for_every_started_effect_and_evidence_class() {
    for effect in [
        EffectState::KnownNotApplied,
        EffectState::KnownApplied,
        EffectState::Unknown,
    ] {
        for evidence in ALL_EVIDENCE {
            let error = try_build(ExecutionStatus::Denied, effect, evidence, "x", false)
                .expect_err("denial must not claim started effects");
            assert_invalid(&error, "denied outcome must have not-started effects");
        }
    }

    // The bounded constructor enforces the same invariant before and after a
    // cut: validation never depends on the content fitting the budget.
    let contents = [String::from("x"), "x".repeat(CAP + 1)];
    for effect in [
        EffectState::KnownNotApplied,
        EffectState::KnownApplied,
        EffectState::Unknown,
    ] {
        for content in &contents {
            let error = ToolOutcome::from_bounded_content(
                ExecutionStatus::Denied,
                effect,
                Evidence::HostObserved,
                content,
                false,
            )
            .expect_err("bounded denial must not claim started effects");
            assert_invalid(&error, "denied outcome must have not-started effects");
        }
    }

    // Not-started denials are accepted with every evidence class.
    for evidence in ALL_EVIDENCE {
        let denied = build(
            ExecutionStatus::Denied,
            EffectState::NotStarted,
            evidence,
            "confirmation required",
            false,
        );
        assert_eq!(denied.status(), ExecutionStatus::Denied);
        assert_eq!(denied.effect(), EffectState::NotStarted);
        assert_eq!(denied.evidence(), evidence);
        assert_eq!(denied.content(), "confirmation required");
    }
}

#[test]
fn admission_rule_is_exact_over_the_full_matrix() {
    // The constructor enforces one cross-field invariant: a denial implies
    // not-started effects. Cancellation and timeout outcomes are canonically
    // Unknown+Uncertain at their call sites (runtime and fakes), but the
    // constructor itself admits other combinations; this sweep pins the
    // actual admission boundary so any future hardening is deliberate.
    for status in ALL_STATUSES {
        for effect in ALL_EFFECTS {
            for evidence in ALL_EVIDENCE {
                let denied_violation =
                    status == ExecutionStatus::Denied && effect != EffectState::NotStarted;
                match try_build(status, effect, evidence, "x", false) {
                    Ok(outcome) => {
                        assert!(
                            !denied_violation,
                            "{status:?}/{effect:?}/{evidence:?} must be rejected"
                        );
                        assert_eq!(outcome.status(), status);
                        assert_eq!(outcome.effect(), effect);
                        assert_eq!(outcome.evidence(), evidence);
                        assert_eq!(outcome.content(), "x");
                        assert!(!outcome.is_truncated());
                    }
                    Err(error) => {
                        assert!(
                            denied_violation,
                            "{status:?}/{effect:?}/{evidence:?} must be admitted"
                        );
                        assert_invalid(&error, "denied outcome must have not-started effects");
                    }
                }
            }
        }
    }
}

#[test]
fn cancelled_and_timed_out_carry_uncertain_effects_through_every_bounding_step() {
    for status in [ExecutionStatus::Cancelled, ExecutionStatus::TimedOut] {
        let outcome = build(
            status,
            EffectState::Unknown,
            Evidence::Uncertain,
            "interrupted",
            false,
        );
        assert_eq!(outcome.status(), status);
        assert_eq!(outcome.effect(), EffectState::Unknown);
        assert_eq!(outcome.evidence(), Evidence::Uncertain);
        assert_eq!(outcome.content(), "interrupted");
        assert!(!outcome.is_truncated());

        // Global bounding keeps the uncertain classification; only the
        // truncation flag changes.
        let bounded = ToolOutcome::from_bounded_content(
            status,
            EffectState::Unknown,
            Evidence::Uncertain,
            "x".repeat(CAP + 1),
            false,
        )
        .expect("bounded uncertain outcome builds");
        assert_eq!(bounded.status(), status);
        assert_eq!(bounded.effect(), EffectState::Unknown);
        assert_eq!(bounded.evidence(), Evidence::Uncertain);
        assert_eq!(bounded.content().len(), CAP);
        assert!(bounded.is_truncated());

        // A budget too small for the leading character still cannot upgrade
        // uncertainty into fabricated certainty.
        let cut = build(
            status,
            EffectState::Unknown,
            Evidence::Uncertain,
            "é",
            false,
        )
        .enforce_budget(1)
        .expect("in-range budget accepts");
        assert!(cut.content().is_empty());
        assert!(cut.is_truncated());
        assert_eq!(cut.status(), status);
        assert_eq!(cut.effect(), EffectState::Unknown);
        assert_eq!(cut.evidence(), Evidence::Uncertain);
    }
}

#[test]
fn cancellation_keeps_known_raced_effects_and_never_rewrites_them() {
    // The tool contract requires successful effects that raced with
    // cancellation to stay recorded, not rewritten as not applied.
    let raced = build(
        ExecutionStatus::Cancelled,
        EffectState::KnownApplied,
        Evidence::HostObserved,
        "write completed before cancel",
        false,
    );
    assert_eq!(raced.status(), ExecutionStatus::Cancelled);
    assert_eq!(raced.effect(), EffectState::KnownApplied);
    assert_ne!(raced.effect(), EffectState::NotStarted);
    assert_ne!(raced.effect(), EffectState::Unknown);
    assert_eq!(raced.evidence(), Evidence::HostObserved);

    let bounded = ToolOutcome::from_bounded_content(
        ExecutionStatus::Cancelled,
        EffectState::KnownApplied,
        Evidence::HostObserved,
        "x".repeat(CAP + 1),
        false,
    )
    .expect("bounded raced-effect outcome builds");
    assert_eq!(bounded.status(), ExecutionStatus::Cancelled);
    assert_eq!(bounded.effect(), EffectState::KnownApplied);
    assert_eq!(bounded.evidence(), Evidence::HostObserved);
    assert!(bounded.is_truncated());

    let cut = raced.enforce_budget(5).expect("in-range budget accepts");
    assert_eq!(cut.content(), "write");
    assert!(cut.is_truncated());
    assert_eq!(cut.status(), ExecutionStatus::Cancelled);
    assert_eq!(cut.effect(), EffectState::KnownApplied);
    assert_eq!(cut.evidence(), Evidence::HostObserved);
}

#[test]
fn success_and_failure_shapes_stay_distinct() {
    let applied = build(
        ExecutionStatus::Succeeded,
        EffectState::KnownApplied,
        Evidence::HostObserved,
        "applied",
        false,
    );
    let not_applied = build(
        ExecutionStatus::Succeeded,
        EffectState::KnownNotApplied,
        Evidence::HostObserved,
        "read only",
        false,
    );
    assert_eq!(applied.status(), ExecutionStatus::Succeeded);
    assert_eq!(not_applied.status(), ExecutionStatus::Succeeded);
    assert_ne!(applied.effect(), not_applied.effect());
    assert_ne!(
        applied, not_applied,
        "applied and not-applied successes are distinct"
    );

    let plugin_reported = build(
        ExecutionStatus::Succeeded,
        EffectState::KnownApplied,
        Evidence::PluginReported,
        "plugin reports applied",
        false,
    );
    assert_eq!(plugin_reported.effect(), EffectState::KnownApplied);
    assert_ne!(
        plugin_reported, applied,
        "the evidence class participates in equality"
    );

    let failed = build(
        ExecutionStatus::Failed,
        EffectState::Unknown,
        Evidence::Uncertain,
        "no result",
        false,
    );
    assert_eq!(failed.status(), ExecutionStatus::Failed);
    assert_eq!(failed.effect(), EffectState::Unknown);
    assert_eq!(failed.evidence(), Evidence::Uncertain);

    let failed_clean = build(
        ExecutionStatus::Failed,
        EffectState::KnownNotApplied,
        Evidence::PluginReported,
        "rejected before acting",
        false,
    );
    assert_eq!(failed_clean.status(), ExecutionStatus::Failed);
    assert_eq!(failed_clean.effect(), EffectState::KnownNotApplied);
    assert_eq!(failed_clean.evidence(), Evidence::PluginReported);
}

#[test]
fn content_budget_is_enforced_independently_of_status() {
    let exact = "x".repeat(CAP);
    let over = "x".repeat(CAP + 1);
    for (status, effect, evidence) in STATUS_REPRESENTATIVES {
        let empty = build(status, effect, evidence, "", false);
        assert!(empty.content().is_empty());
        assert!(!empty.is_truncated());

        let at_cap = build(status, effect, evidence, exact.clone(), false);
        assert_eq!(at_cap.content().len(), CAP);
        assert!(!at_cap.is_truncated());

        let error = try_build(status, effect, evidence, over.clone(), false)
            .expect_err("over-cap content is rejected for every status");
        assert_invalid(&error, "tool outcome exceeds output budget");

        let bounded =
            ToolOutcome::from_bounded_content(status, effect, evidence, over.clone(), false)
                .expect("from_bounded_content bounds instead of rejecting");
        assert_eq!(bounded.content().len(), CAP);
        assert!(bounded.is_truncated());
        assert_eq!(bounded.status(), status);
        assert_eq!(bounded.effect(), effect);
        assert_eq!(bounded.evidence(), evidence);
    }
}

#[test]
fn equality_and_clone_are_sensitive_to_every_outcome_field() {
    let base = build(
        ExecutionStatus::Succeeded,
        EffectState::KnownApplied,
        Evidence::HostObserved,
        "ok",
        false,
    );
    assert_eq!(base.clone(), base);

    assert_ne!(
        base,
        build(
            ExecutionStatus::Failed,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            "ok",
            false,
        ),
        "status participates in equality"
    );
    assert_ne!(
        base,
        build(
            ExecutionStatus::Succeeded,
            EffectState::KnownNotApplied,
            Evidence::HostObserved,
            "ok",
            false,
        ),
        "effect participates in equality"
    );
    assert_ne!(
        base,
        build(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::PluginReported,
            "ok",
            false,
        ),
        "evidence participates in equality"
    );
    assert_ne!(
        base,
        build(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            "ok!",
            false,
        ),
        "content participates in equality"
    );
    assert_ne!(
        base,
        build(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            "ok",
            true,
        ),
        "the truncation flag participates in equality"
    );

    let debug = format!("{base:?}");
    assert!(debug.contains("Succeeded"), "{debug}");
    assert!(debug.contains("KnownApplied"), "{debug}");
    assert!(debug.contains("HostObserved"), "{debug}");
    assert!(debug.contains("ok"), "{debug}");
}
