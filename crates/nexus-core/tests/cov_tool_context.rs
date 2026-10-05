//! Public-boundary hardening for `ToolContext` control paths.
//!
//! The unit tests inside `nexus-core` exercise the same behavior from inside
//! the module; these tests pin the documented contract through the public API
//! only: finite output budgets, explicit cancellation snapshots, live token
//! observation, evaluable monotonic deadlines, and legacy contexts that carry
//! an elapsed reading but no evaluable deadline.
//!
//! Determinism: no sleeps and no wall-clock time. Past deadlines are derived
//! with `Instant::checked_sub` and future deadlines with `Instant::checked_add`;
//! the only time-dependent assertions compare monotonic instants, which never
//! move backwards.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use nexus_core::{
    AgentError, ApprovedScope, CancellationToken, ErrorCategory, Limits, RetryGuidance, ToolContext,
};

const MAX_BUDGET: usize = Limits::M0_TEST_TOOL_OUTPUT_BYTES;

fn scope() -> ApprovedScope {
    ApprovedScope::new("project-read").expect("valid scope builds")
}

fn legacy(budget: usize, elapsed: Duration, cancelled: bool) -> ToolContext {
    ToolContext::new(budget, elapsed, cancelled, scope()).expect("valid context builds")
}

fn past_instant() -> Instant {
    Instant::now()
        .checked_sub(Duration::from_millis(1))
        .expect("test clock has history")
}

fn future_instant() -> Instant {
    Instant::now()
        .checked_add(Duration::from_secs(60))
        .expect("test clock supports bounded deadlines")
}

fn assert_state_error(error: &AgentError, category: ErrorCategory) {
    assert_eq!(error.category(), category);
    assert_eq!(
        error.retry(),
        RetryGuidance::DoNotRetry,
        "dispatch state failures are not retryable"
    );
}

#[test]
fn output_budget_is_finite_and_bounded_by_the_m0_maximum() {
    let zero =
        ToolContext::new(0, Duration::ZERO, false, scope()).expect_err("zero budget is rejected");
    assert_state_error(&zero, ErrorCategory::InvalidInput);

    let above = ToolContext::new(MAX_BUDGET + 1, Duration::ZERO, false, scope())
        .expect_err("above the M0 maximum is rejected");
    assert_state_error(&above, ErrorCategory::InvalidInput);

    let oversized = ToolContext::new(usize::MAX, Duration::ZERO, false, scope())
        .expect_err("the largest representable budget is still rejected");
    assert_state_error(&oversized, ErrorCategory::InvalidInput);

    let unit = legacy(1, Duration::ZERO, false);
    assert_eq!(unit.output_budget_bytes(), 1);

    let boundary = legacy(MAX_BUDGET, Duration::ZERO, false);
    assert_eq!(boundary.output_budget_bytes(), MAX_BUDGET);
}

#[test]
fn legacy_snapshot_cancellation_is_explicit_but_never_cooperative() {
    let active = legacy(1024, Duration::from_secs(60), false);
    assert!(!active.is_cancelled());
    assert!(active.check_active().is_ok());
    assert!(active.check_not_cancelled().is_ok());

    let cancelled = legacy(1024, Duration::from_secs(60), true);
    assert!(cancelled.is_cancelled(), "the snapshot is explicit");
    assert_state_error(
        &cancelled.check_active().expect_err("snapshot cancels"),
        ErrorCategory::Cancelled,
    );
    assert_state_error(
        &cancelled
            .check_not_cancelled()
            .expect_err("snapshot cancels"),
        ErrorCategory::Cancelled,
    );
    // There is no live control behind a snapshot, so repeated reads stay
    // constant: the value can never be updated after construction.
    assert!(cancelled.is_cancelled());
}

#[test]
fn legacy_elapsed_reading_is_not_an_evaluable_deadline() {
    let context = legacy(1024, Duration::from_secs(60), false);
    assert_eq!(context.deadline(), None);
    assert_eq!(context.deadline_elapsed(), Duration::from_secs(60));

    // Even an elapsed reading beyond any practical timeout cannot expire a
    // legacy context: without an installed instant there is nothing to
    // evaluate, so the explicit cancellation check still passes.
    let huge = legacy(1024, Duration::MAX, false);
    assert_eq!(huge.deadline(), None);
    assert_eq!(huge.deadline_elapsed(), Duration::MAX);
    assert!(huge.check_active().is_ok());
    assert!(huge.check_not_cancelled().is_ok());

    let zero = legacy(1024, Duration::ZERO, false);
    assert_eq!(zero.deadline(), None);
    assert_eq!(zero.deadline_elapsed(), Duration::ZERO);
    assert!(zero.check_active().is_ok());
}

#[test]
fn live_token_cancellation_is_re_read_after_construction() {
    let token = CancellationToken::new();
    let context =
        legacy(1024, Duration::from_secs(300), false).with_control(token.clone(), future_instant());
    let observer = context.clone();

    assert!(!context.is_cancelled());
    assert!(!observer.is_cancelled());
    assert!(context.check_active().is_ok());
    assert!(context.check_not_cancelled().is_ok());

    token.cancel();

    assert!(context.is_cancelled(), "live control is re-read");
    assert!(observer.is_cancelled(), "clones share the live control");
    assert_state_error(
        &context.check_active().expect_err("live cancellation"),
        ErrorCategory::Cancelled,
    );
    assert_state_error(
        &context
            .check_not_cancelled()
            .expect_err("live cancellation"),
        ErrorCategory::Cancelled,
    );
}

#[test]
fn snapshot_cancellation_composes_with_live_control() {
    let token = CancellationToken::new();
    let context =
        legacy(1024, Duration::from_secs(60), true).with_control(token.clone(), future_instant());

    assert!(
        context.is_cancelled(),
        "the snapshot survives control installation"
    );
    assert_state_error(
        &context.check_active().expect_err("snapshot cancels"),
        ErrorCategory::Cancelled,
    );
    assert!(context.check_not_cancelled().is_err());
    // An uncancelled live token cannot clear an explicit dispatch-time
    // snapshot: cancellation is the union of both sources.
    assert!(!token.is_cancelled());
}

#[test]
fn installed_deadline_is_evaluable_on_the_monotonic_clock() {
    let deadline = future_instant();
    let context = legacy(1024, Duration::from_secs(300), false)
        .with_control(CancellationToken::new(), deadline);
    assert_eq!(context.deadline(), Some(deadline));
    assert_eq!(context.deadline_elapsed(), Duration::from_secs(300));
    assert!(context.check_active().is_ok());

    let past = past_instant();
    let expired =
        legacy(1024, Duration::from_secs(300), false).with_control(CancellationToken::new(), past);
    assert_eq!(expired.deadline(), Some(past));
    assert!(!expired.is_cancelled());
    assert_state_error(
        &expired.check_active().expect_err("deadline passed"),
        ErrorCategory::Timeout,
    );
    // The deadline check is exclusive to `check_active`; the explicit
    // cancellation check stays cancellation-only.
    assert!(expired.check_not_cancelled().is_ok());

    let reached =
        legacy(1024, Duration::ZERO, false).with_control(CancellationToken::new(), Instant::now());
    assert_state_error(
        &reached
            .check_active()
            .expect_err("a reached deadline is elapsed"),
        ErrorCategory::Timeout,
    );
}

#[test]
fn cancellation_precedes_an_elapsed_deadline() {
    let token = CancellationToken::new();
    let context = legacy(1024, Duration::ZERO, false).with_control(token.clone(), past_instant());
    token.cancel();
    assert_state_error(
        &context
            .check_active()
            .expect_err("both cancellation and timeout apply"),
        ErrorCategory::Cancelled,
    );

    let snapshot =
        legacy(1024, Duration::ZERO, true).with_control(CancellationToken::new(), past_instant());
    assert_state_error(
        &snapshot
            .check_active()
            .expect_err("snapshot cancellation wins"),
        ErrorCategory::Cancelled,
    );
}

#[test]
fn with_control_replaces_any_previously_installed_control() {
    let first = CancellationToken::new();
    let installed =
        legacy(1024, Duration::from_secs(60), false).with_control(first.clone(), past_instant());
    first.cancel();
    assert_state_error(
        &installed
            .check_active()
            .expect_err("installed control cancels"),
        ErrorCategory::Cancelled,
    );

    let second = CancellationToken::new();
    let replaced = installed
        .clone()
        .with_control(second.clone(), future_instant());

    // The old token is no longer observed and the old deadline no longer
    // evaluates; the legacy elapsed reading is untouched.
    assert!(!replaced.is_cancelled());
    assert!(replaced.check_active().is_ok());
    assert_eq!(replaced.deadline_elapsed(), Duration::from_secs(60));
    assert!(replaced.deadline().is_some_and(|at| at > Instant::now()));

    // The replacement token becomes the live control; the clone that was
    // not rebuilt keeps observing the first token.
    second.cancel();
    assert!(replaced.is_cancelled(), "the replacement token is live");
    assert!(
        installed.is_cancelled(),
        "a clone that was not rebuilt keeps its own control"
    );
}

#[test]
fn scope_and_budget_survive_control_installation() {
    let context = ToolContext::new(
        7,
        Duration::from_millis(5),
        false,
        ApprovedScope::new("project-write").expect("valid scope builds"),
    )
    .expect("valid context builds")
    .with_control(CancellationToken::new(), future_instant());

    assert_eq!(context.output_budget_bytes(), 7);
    assert_eq!(context.deadline_elapsed(), Duration::from_millis(5));
    assert_eq!(context.scope().as_str(), "project-write");

    let empty_scope = ToolContext::new(
        1,
        Duration::ZERO,
        false,
        ApprovedScope::new("").expect("empty scope is valid"),
    )
    .expect("valid context builds");
    assert_eq!(empty_scope.scope().as_str(), "");
}

#[test]
fn clones_share_control_and_equality_tracks_it() {
    let token = CancellationToken::new();
    let deadline = future_instant();
    let context =
        legacy(1024, Duration::from_secs(60), false).with_control(token.clone(), deadline);
    let clone = context.clone();
    assert_eq!(clone, context);

    token.cancel();
    assert!(clone.is_cancelled());
    assert!(context.is_cancelled());
    assert_eq!(clone, context, "clones remain one shared control");

    let independent = legacy(1024, Duration::from_secs(60), false)
        .with_control(CancellationToken::new(), deadline);
    assert_ne!(
        independent, context,
        "independent tokens are not interchangeable"
    );

    let legacy_only = legacy(1024, Duration::from_secs(60), false);
    assert_ne!(
        legacy_only, context,
        "a legacy snapshot differs from installed control"
    );
}
