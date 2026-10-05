#![forbid(unsafe_code)]

//! Bounded-shutdown coverage hardening for the headless composition root,
//! through the public API only.
//!
//! The headless entry point builds a runtime, consumes one run to a terminal
//! record, and tears the runtime down under a finite wait. Every bound on
//! that path is published (`SHUTDOWN_TIMEOUT`, `WATCHDOG_SLACK`,
//! `RECONCILE_TIMEOUT`) but the wait itself is crate-private, so these tests
//! pin the two things a caller can actually observe: the bounds are finite,
//! nonzero, and arithmetically safe, and real work stays far inside them.
//! A stuck blocking worker, an unbounded teardown join, or a watchdog that
//! fired on a live run would each trip a bound instead of hanging the suite.
//!
//! Covered contract:
//! - teardown and post-cancel reconciliation are finite, nonzero budgets, and
//!   the worst-case error path (cancel request, terminal drain, teardown) stays
//!   short and overflow-free;
//! - `run_task` captures its result before teardown, so a bounded teardown
//!   still yields a complete report, returns inside a bounded wall clock, and
//!   leaves no state that blocks the next call, including after a failure;
//! - the watchdog window is the runtime's remaining run-duration budget plus
//!   `WATCHDOG_SLACK`: the slack always survives, elapsed budget only shrinks
//!   the window, and a scripted M0 run finishes far inside it;
//! - a rejected runtime registration is a typed `Err` on the public
//!   registration boundary that the headless mapping turns into the operation
//!   class, never a usage mistake and never an unwind;
//! - `exit_code_for` covers every `RunOutcome` and every denial count, the
//!   whole process exit-code space is exactly `0..=6`, and a real report's
//!   `exit_code` agrees with the mapping;
//! - usage failures (empty, oversize) and operation failures stay disjoint,
//!   value-free, comparable, and usable as `std::error::Error`.
//!
//! Determinism: every wait is a bound on real work, never a sleep, and no
//! test depends on a rejected input being echoed back.
//!
//! Deliberately not covered here, because the public API exposes no seam for
//! it and a crate-private test is the only place it can be observed: a
//! blocking worker that is genuinely stuck at teardown (the runtime is built
//! inside `run_task`, so a test cannot park one of *its* workers), the private
//! `watchdog_budget` composition (only the published `WATCHDOG_SLACK` is
//! visible, and a scripted M0 run never approaches the run deadline), and the
//! `Operation` mapping of a registration failure (the composition always
//! registers cleanly, so `run_task` cannot reach that branch). Those three
//! need a crate-internal test or a public composition seam, not a black-box
//! one.

use std::error::Error;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nexus_core::commands::MAX_INPUT_BYTES;
use nexus_core::{ErrorCategory, Limits, ProviderPort, RunOutcome, ToolPort};
use nexus_fakes::{FakeProvider, FakeTool};
use nexus_headless::{
    EXIT_STDOUT_CLOSED, HeadlessError, RECONCILE_TIMEOUT, SHUTDOWN_TIMEOUT, USAGE, WATCHDOG_SLACK,
    exit_code_for, outcome_name, run_task,
};
use nexus_runtime::{Policy, Runtime, RuntimeConfig};

/// Bounded wall-clock ceiling for one `run_task` call. A scripted M0 run
/// settles in milliseconds; this only bounds a defect (an unbounded teardown
/// join, or a watchdog that never fires).
const RUN_BOUND: Duration = Duration::from_secs(10);

/// Sequential runs in the teardown-reuse test. Repeated calls catch a bound
/// that leaks per-call state; a leak shows up as a bound trip, not as a hang.
/// [`Duration`] scales by `u32`, so the count is typed accordingly.
const REPEAT_RUNS: u32 = 6;

/// Ceiling for the composed worst-case error path. The watchdog path requests
/// cancellation (bounded by `RECONCILE_TIMEOUT`), reconciles for the terminal
/// record (again `RECONCILE_TIMEOUT`), then tears the runtime down
/// (`SHUTDOWN_TIMEOUT`).
const ERROR_PATH_BOUND: Duration = Duration::from_secs(5);

/// The exact runtime configuration the headless composition uses, including
/// the absent approval handler that makes confirmation-required calls denied
/// rather than auto-approved or hung.
fn headless_runtime_config() -> RuntimeConfig {
    RuntimeConfig {
        limits: Limits::m0_test(),
        policy: Policy::m0_test(),
        has_approval_handler: false,
    }
}

fn scripted_provider() -> Arc<dyn ProviderPort + Send + Sync> {
    Arc::new(FakeProvider::new(Vec::new()))
}

/// The tool set the M0 composition registers: three distinct names at the M0
/// revision, so registration cannot fail on duplicates.
fn headless_composition_tools() -> Vec<Arc<dyn ToolPort + Send + Sync>> {
    vec![
        Arc::new(FakeTool::read_only()),
        Arc::new(FakeTool::mutation()),
        Arc::new(FakeTool::command()),
    ]
}

/// The teardown wait is a finite, nonzero budget. It exists to stop waiting
/// for a blocking worker, so it must never degenerate into "wait forever", and
/// it must never collapse to zero, which would skip the wait instead of
/// bounding it.
#[test]
fn shutdown_and_reconcile_bounds_are_finite_nonzero_and_overflow_free() {
    assert!(
        !SHUTDOWN_TIMEOUT.is_zero(),
        "teardown still waits a finite interval"
    );
    assert!(
        !RECONCILE_TIMEOUT.is_zero(),
        "post-cancel reconciliation still waits a finite interval"
    );

    let run_duration = Limits::m0_test().run_duration;
    for (name, bound) in [
        ("SHUTDOWN_TIMEOUT", SHUTDOWN_TIMEOUT),
        ("RECONCILE_TIMEOUT", RECONCILE_TIMEOUT),
    ] {
        assert!(
            bound <= run_duration,
            "{name} ({bound:?}) never dominates the run budget"
        );
        assert!(
            bound <= Limits::M0_TEST_MAX_DURATION,
            "{name} stays inside the validated duration ceiling"
        );
        assert!(
            u32::try_from(bound.as_millis()).is_ok(),
            "{name} is a small budget, not hours: {bound:?}"
        );
    }

    // Worst-case error path: one cancel request, one terminal drain, one
    // teardown. It must stay short and must stay representable.
    let worst_case = SHUTDOWN_TIMEOUT
        .checked_add(RECONCILE_TIMEOUT.saturating_mul(2))
        .expect("composed error-path bounds add without overflow");
    assert!(
        worst_case <= ERROR_PATH_BOUND,
        "the error path stays short: {worst_case:?}"
    );
}

/// Teardown bounds the wait, so repeated calls each return inside the bound
/// and the entry point keeps working afterwards: the future's result is
/// captured before teardown, so a bounded wait still yields a full report and
/// never a leaked or poisoned runtime.
///
/// A parked worker cannot be injected here (the runtime is built inside
/// `run_task`), so this pins the two halves of the bound that a black-box
/// caller can see: the wait is applied on every call rather than skipped, and
/// no call exceeds its own budget. A teardown that waited on a stuck worker
/// indefinitely would trip the per-call bound instead of hanging the suite.
#[test]
fn repeated_runs_return_inside_a_bounded_wall_clock() {
    let suite = Instant::now();
    for index in 0..REPEAT_RUNS {
        let started = Instant::now();
        let report =
            run_task("hello").unwrap_or_else(|error| panic!("run {index} completes: {error}"));
        let elapsed = started.elapsed();
        assert!(
            elapsed < RUN_BOUND,
            "run {index} returned inside the bound: {elapsed:?}"
        );
        assert_eq!(report.outcome, RunOutcome::Completed);
        assert_eq!(report.exit_code, 0);
        assert!(
            report.lines.len() >= 2,
            "run {index} carried event records plus the result line"
        );
    }
    assert!(
        suite.elapsed() < RUN_BOUND * REPEAT_RUNS,
        "no run in the sequence exceeded its own share of the bound"
    );
}

/// The error path tears its runtime down like any other path, so a rejected
/// input cannot leave process exit waiting or block the next call.
#[test]
fn a_usage_failure_does_not_block_the_next_run() {
    let usage = run_task("").err().expect("empty task is refused");
    assert!(usage.is_usage(), "empty task is a usage failure");
    let report = run_task("hello").expect("a later run still completes");
    assert_eq!(report.exit_code, 0);
}

/// The consume watchdog is the runtime's remaining run-duration budget plus
/// the slack. The slack is what keeps it from ever truncating a run the
/// runtime still considers live, so it must be real, added, and bounded.
#[test]
fn watchdog_window_is_the_run_duration_plus_positive_slack() {
    let run_duration = Limits::m0_test().run_duration;
    assert!(!WATCHDOG_SLACK.is_zero(), "the slack is real, not zero");
    assert!(
        WATCHDOG_SLACK < run_duration,
        "the slack stays a tail, not a second budget"
    );

    let budget = run_duration
        .checked_add(WATCHDOG_SLACK)
        .expect("the watchdog budget adds without overflow");
    assert_eq!(budget, run_duration + WATCHDOG_SLACK);
    assert!(
        budget > run_duration,
        "the watchdog outlives the runtime deadline"
    );
    assert!(
        budget <= Limits::M0_TEST_MAX_DURATION,
        "the window stays inside the validated duration ceiling"
    );
    assert!(
        u32::try_from(budget.as_millis()).is_ok(),
        "the window is a bounded budget: {budget:?}"
    );

    // A fully spent budget collapses to the slack alone, never to an
    // immediate expiry and never to an unbounded wait.
    assert_eq!(
        Duration::ZERO.saturating_add(WATCHDOG_SLACK),
        WATCHDOG_SLACK
    );
}

/// Time already spent can only shrink the watchdog window, and the slack
/// always survives, so the window can never fall below the slack or rise
/// above the full budget.
///
/// This re-derives the window from the published `WATCHDOG_SLACK` and the
/// runtime's run budget, which is the contract a caller can rely on. The
/// private `watchdog_budget` composition itself is pinned crate-internally.
#[test]
fn watchdog_window_never_drops_below_the_slack_and_never_exceeds_the_full_budget() {
    let run_duration = Limits::m0_test().run_duration;
    let full = run_duration.saturating_add(WATCHDOG_SLACK);
    for elapsed in [
        Duration::ZERO,
        Duration::from_millis(1),
        Duration::from_secs(1),
        run_duration / 2,
        run_duration,
        run_duration.saturating_add(WATCHDOG_SLACK.saturating_mul(2)),
        Duration::MAX,
    ] {
        let window = run_duration
            .saturating_sub(elapsed)
            .saturating_add(WATCHDOG_SLACK);
        assert!(
            window <= full,
            "an elapsed budget only shrinks the window: {elapsed:?}"
        );
        assert!(
            window >= WATCHDOG_SLACK,
            "the slack always survives: {elapsed:?}"
        );
    }
}

/// The watchdog is a backstop, not the normal completion path: a scripted M0
/// run finishes with orders of magnitude left in the run budget, so the
/// window never truncates a live run.
#[test]
fn scripted_runs_finish_far_inside_the_watchdog_window() {
    let started = Instant::now();
    let report = run_task("deny: write it").expect("denial script runs");
    let elapsed = started.elapsed();
    assert_eq!(
        report.exit_code, 3,
        "denied calls exit with the denial code"
    );
    assert!(
        elapsed.saturating_mul(10) < Limits::m0_test().run_duration,
        "the run finished well inside its run budget: {elapsed:?}"
    );
}

/// An invalid runtime registration is a typed `Err` on the public
/// registration boundary, so the headless mapping can turn it into an
/// operation error instead of unwinding or blaming the caller.
///
/// The mapping itself is crate-private and the composition always registers
/// cleanly, so what a black-box test can pin is the shape the mapping depends
/// on: the registration error is a value with a category and a static message
/// (so it is always mappable and never needs to unwind), and the operation
/// class it maps to is never the usage class.
#[test]
fn runtime_registration_failure_is_a_normal_operation_error() {
    let composed = Runtime::try_new(
        headless_runtime_config(),
        scripted_provider(),
        headless_composition_tools(),
    );
    assert!(
        composed.is_ok(),
        "the headless composition registers three distinct tools cleanly"
    );

    let duplicate: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![
        Arc::new(FakeTool::read_only()),
        Arc::new(FakeTool::read_only()),
    ];
    let registration = Runtime::try_new(headless_runtime_config(), scripted_provider(), duplicate)
        .err()
        .expect("a duplicate tool registration is rejected");
    assert_eq!(
        registration.category(),
        ErrorCategory::InvalidInput,
        "registration failure is a typed input error"
    );
    assert!(
        registration
            .message()
            .contains("duplicate tool registration"),
        "the diagnostic names the cause: {}",
        registration.message()
    );

    let mapped = HeadlessError::Operation(format!("runtime registration failed: {registration}"));
    assert!(
        !mapped.is_usage(),
        "a broken composition is not a usage mistake"
    );
    assert_eq!(mapped.exit_code(), HeadlessError::OPERATION_EXIT_CODE);
    assert_ne!(mapped.exit_code(), HeadlessError::USAGE_EXIT_CODE);
}

/// Every outcome maps to a code, and the denial count only splits
/// `Completed`: a failed or refused run stays operational even when calls were
/// denied, so denial never masks a failure.
#[test]
fn exit_code_mapping_covers_every_outcome_and_denied_count() {
    for (outcome, denied, expected) in [
        (RunOutcome::Completed, 0_usize, 0),
        (RunOutcome::Completed, 1, 3),
        (RunOutcome::Completed, 7, 3),
        (RunOutcome::Failed, 0, 1),
        (RunOutcome::Failed, 3, 1),
        (RunOutcome::Refused, 0, 1),
        (RunOutcome::Refused, 2, 1),
        (RunOutcome::Cancelled, 0, 4),
        (RunOutcome::Cancelled, 4, 4),
        (RunOutcome::LimitReached, 0, 5),
        (RunOutcome::LimitReached, 9, 5),
    ] {
        assert_eq!(
            exit_code_for(outcome, denied),
            expected,
            "{} with {denied} denied calls",
            outcome_name(outcome)
        );
    }

    // Only a clean completed run reports success.
    assert_eq!(exit_code_for(RunOutcome::Completed, 0), 0);
    assert_ne!(exit_code_for(RunOutcome::Completed, 1), 0);
    // Cancellation and limit exhaustion stay distinguishable from each other.
    assert_ne!(
        exit_code_for(RunOutcome::Cancelled, 0),
        exit_code_for(RunOutcome::LimitReached, 0)
    );
    // Every outcome name is a distinct, static, lowercase wire token.
    let names: Vec<&str> = [
        RunOutcome::Completed,
        RunOutcome::Refused,
        RunOutcome::Failed,
        RunOutcome::Cancelled,
        RunOutcome::LimitReached,
    ]
    .into_iter()
    .map(outcome_name)
    .collect();
    assert_eq!(names.len(), 5);
    for name in &names {
        assert!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'-'),
            "static lowercase wire name: {name}"
        );
    }
    let mut unique_names = names.clone();
    unique_names.sort_unstable();
    unique_names.dedup();
    assert_eq!(
        unique_names.len(),
        names.len(),
        "outcome names are distinct"
    );
}

/// The process exit-code space is exactly `0..=6`, every code is a valid exit
/// status byte, and only code 1 is intentionally shared between a failed run
/// and an operational entry-point failure.
#[test]
fn process_exit_codes_are_unique_narrow_and_status_byte_safe() {
    let reachable = [
        exit_code_for(RunOutcome::Completed, 0),
        exit_code_for(RunOutcome::Completed, 1),
        exit_code_for(RunOutcome::Failed, 0),
        exit_code_for(RunOutcome::Refused, 0),
        exit_code_for(RunOutcome::Cancelled, 0),
        exit_code_for(RunOutcome::LimitReached, 0),
        EXIT_STDOUT_CLOSED,
        HeadlessError::USAGE_EXIT_CODE,
        HeadlessError::OPERATION_EXIT_CODE,
    ];
    for code in reachable {
        assert!(
            u8::try_from(code).is_ok(),
            "exit code {code} fits the process status byte"
        );
    }

    let mut unique = reachable.to_vec();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique,
        vec![0, 1, 2, 3, 4, 5, 6],
        "the exit-code space is fully covered and nothing else is reachable"
    );

    assert_eq!(HeadlessError::USAGE_EXIT_CODE, 2, "usage is exit 2");
    assert_eq!(HeadlessError::OPERATION_EXIT_CODE, 1, "operation is exit 1");
    assert_eq!(EXIT_STDOUT_CLOSED, 6, "a closed consumer is distinct");
    assert_ne!(
        EXIT_STDOUT_CLOSED,
        exit_code_for(RunOutcome::Completed, 0),
        "a closed consumer never looks like success"
    );
    assert_ne!(
        EXIT_STDOUT_CLOSED,
        HeadlessError::USAGE_EXIT_CODE,
        "a closed consumer never looks like a usage mistake"
    );
}

/// A real report carries exactly the code the public mapping produces, and its
/// result line carries the same outcome name as the report field.
#[test]
fn report_exit_codes_agree_with_the_public_mapping() {
    let completed = run_task("hello").expect("completed script runs");
    assert_eq!(
        completed.exit_code,
        exit_code_for(completed.outcome, completed.denied_calls)
    );
    assert_eq!(completed.exit_code, 0);
    let result = completed.lines.last().expect("a result line");
    assert!(
        result.contains(&format!("outcome={}", outcome_name(completed.outcome))),
        "the result line names the outcome: {result}"
    );
    assert!(result.contains("denied=0"), "{result}");

    let denied = run_task("deny: write it").expect("denial script runs");
    assert_eq!(denied.outcome, RunOutcome::Completed);
    assert_eq!(denied.denied_calls, 1);
    assert_eq!(
        denied.exit_code,
        exit_code_for(denied.outcome, denied.denied_calls)
    );
    assert_eq!(
        denied.exit_code, 3,
        "a completed run with denied calls is distinct from success"
    );

    let refused = run_task("refuse: no").expect("refusal script runs");
    assert_eq!(
        refused.exit_code,
        exit_code_for(refused.outcome, refused.denied_calls)
    );
    assert_eq!(
        refused.exit_code, 1,
        "a refused run is an operational failure"
    );
}

/// Usage and operation failures are disjoint classes: each has its own exit
/// code, both are comparable without matching message text, and neither
/// echoes the rejected input back to the caller.
#[test]
fn usage_and_operation_failures_map_to_distinct_codes() {
    let empty = run_task("").err().expect("empty task is refused");
    assert!(empty.is_usage());
    assert_eq!(empty.exit_code(), HeadlessError::USAGE_EXIT_CODE);
    assert_eq!(empty.exit_code(), 2);
    assert!(!empty.message().is_empty(), "a diagnostic exists to read");

    let sentinel = "SENTINEL_ARGV_SECRET";
    let oversize = run_task(&sentinel.repeat(MAX_INPUT_BYTES))
        .err()
        .expect("oversize task is refused");
    assert!(oversize.is_usage());
    assert_eq!(oversize.exit_code(), 2);
    assert!(
        !oversize.message().contains(sentinel),
        "the rejected input is never echoed: {}",
        oversize.message()
    );

    let operation = HeadlessError::Operation("submit was not accepted".to_owned());
    assert!(!operation.is_usage());
    assert_eq!(operation.exit_code(), HeadlessError::OPERATION_EXIT_CODE);
    assert_ne!(operation.exit_code(), empty.exit_code());
    assert_ne!(empty, operation, "the classes are distinguishable by value");
    let copied = empty.clone();
    assert_eq!(empty, copied, "a caller can branch without string matching");
}

/// Usage rejection is exactly the empty-or-oversize class: a non-empty task is
/// never a usage failure, and the boundary is one byte over the input budget,
/// not one byte under it.
#[test]
fn usage_rejection_is_exactly_empty_or_oversize_input() {
    let at_budget = "x".repeat(MAX_INPUT_BYTES);
    let accepted = run_task(&at_budget).expect("input at the budget is a task");
    assert_eq!(accepted.exit_code, 0);

    assert!(
        run_task(&"x".repeat(MAX_INPUT_BYTES + 1))
            .err()
            .is_some_and(|error| error.is_usage()),
        "one byte over the budget is a usage failure"
    );
    assert!(
        run_task("").err().is_some_and(|error| error.is_usage()),
        "the empty task is a usage failure"
    );
    assert!(
        run_task(" ").err().is_none(),
        "a non-empty task is never a usage failure"
    );
}

/// Both failure classes are ordinary `std` errors for the binary's stderr
/// diagnostics, and the usage hint is a static, value-free string.
#[test]
fn failure_classes_are_usable_as_std_errors_by_callers() {
    let usage: Box<dyn Error> = Box::new(HeadlessError::Usage("bad input".to_owned()));
    let operation: Box<dyn Error> = Box::new(HeadlessError::Operation("bad runtime".to_owned()));
    assert_eq!(usage.to_string(), "bad input");
    assert_eq!(operation.to_string(), "bad runtime");
    assert!(
        operation.source().is_none(),
        "the class carries its own message and no leaked inner error"
    );

    assert!(USAGE.starts_with("usage:"), "static usage hint: {USAGE}");
    assert!(
        USAGE.contains("deny:"),
        "the hint documents the routing prefix"
    );
    assert!(
        USAGE.bytes().all(|byte| byte.is_ascii()),
        "the hint stays ASCII and value-free"
    );
}
