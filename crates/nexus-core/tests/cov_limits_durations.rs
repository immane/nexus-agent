//! Public-boundary coverage for the three duration fields of
//! [`nexus_core::Limits`]: `run_duration`, `per_tool_timeout`, and
//! `approval_expiry`.
//!
//! `Limits::validate` treats those durations as independent per-field maxima:
//! each must be nonzero, each is capped at [`Limits::M0_TEST_MAX_DURATION`],
//! and no duration constrains another. The runtime composes the effective
//! deadline as the minimum of the run deadline and the operation deadline;
//! these tests pin only the `Limits` half of that contract through the public
//! API. All assertions are pure `Duration` arithmetic: no clocks, no sleeps,
//! no I/O.

#![forbid(unsafe_code)]

use std::time::Duration;

use nexus_core::{ErrorCategory, Limits, RetryGuidance};

const NONZERO_MESSAGE: &str = "limit duration must be nonzero";
const RUN_CAP_MESSAGE: &str = "run duration exceeds practical maximum";
const TOOL_CAP_MESSAGE: &str = "tool timeout exceeds practical maximum";
const APPROVAL_CAP_MESSAGE: &str = "approval expiry exceeds practical maximum";
const RUN_EXHAUSTED_MESSAGE: &str = "run duration exhausted";

fn with_durations(run: Duration, tool: Duration, approval: Duration) -> Limits {
    let mut limits = Limits::m0_test();
    limits.run_duration = run;
    limits.per_tool_timeout = tool;
    limits.approval_expiry = approval;
    limits
}

fn assert_duration_error(name: &str, limits: &Limits, expected_message: &str) {
    let error = limits
        .validate()
        .expect_err("a duration violation must be rejected");
    assert_eq!(error.category(), ErrorCategory::ResourceLimit, "{name}");
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry, "{name}");
    assert_eq!(error.message(), expected_message, "{name}");
}

#[test]
fn m0_test_durations_are_finite_nonzero_and_capped_at_twenty_four_hours() {
    assert_eq!(
        Limits::M0_TEST_MAX_DURATION,
        Duration::from_secs(24 * 60 * 60)
    );

    let limits = Limits::m0_test();
    for (name, duration) in [
        ("run_duration", limits.run_duration),
        ("per_tool_timeout", limits.per_tool_timeout),
        ("approval_expiry", limits.approval_expiry),
    ] {
        assert!(!duration.is_zero(), "{name} must be nonzero");
        assert!(
            duration <= Limits::M0_TEST_MAX_DURATION,
            "{name} must be within the practical ceiling"
        );
    }
    limits.validate().expect("M0-test durations are valid");
}

#[test]
fn each_duration_rejects_zero_with_the_duration_diagnostic() {
    let mut limits = Limits::m0_test();
    limits.run_duration = Duration::ZERO;
    assert_duration_error("run_duration", &limits, NONZERO_MESSAGE);

    let mut limits = Limits::m0_test();
    limits.per_tool_timeout = Duration::ZERO;
    assert_duration_error("per_tool_timeout", &limits, NONZERO_MESSAGE);

    let mut limits = Limits::m0_test();
    limits.approval_expiry = Duration::ZERO;
    assert_duration_error("approval_expiry", &limits, NONZERO_MESSAGE);
}

#[test]
fn zero_duration_is_rejected_even_when_the_other_durations_are_at_the_cap() {
    let cap = Limits::M0_TEST_MAX_DURATION;

    assert_duration_error(
        "run_duration",
        &with_durations(Duration::ZERO, cap, cap),
        NONZERO_MESSAGE,
    );
    assert_duration_error(
        "per_tool_timeout",
        &with_durations(cap, Duration::ZERO, cap),
        NONZERO_MESSAGE,
    );
    assert_duration_error(
        "approval_expiry",
        &with_durations(cap, cap, Duration::ZERO),
        NONZERO_MESSAGE,
    );
}

#[test]
fn each_duration_accepts_the_cap_and_one_nanosecond_below() {
    let cap = Limits::M0_TEST_MAX_DURATION;
    let below = cap - Duration::from_nanos(1);
    let default_tool = Duration::from_secs(60);
    let default_approval = Duration::from_secs(120);

    with_durations(below, default_tool, default_approval)
        .validate()
        .expect("run just below the cap is valid");
    with_durations(cap, default_tool, default_approval)
        .validate()
        .expect("run at the cap is valid");
    with_durations(default_tool, below, default_approval)
        .validate()
        .expect("tool timeout just below the cap is valid");
    with_durations(default_tool, cap, default_approval)
        .validate()
        .expect("tool timeout at the cap is valid");
    with_durations(default_tool, default_approval, below)
        .validate()
        .expect("approval expiry just below the cap is valid");
    with_durations(default_tool, default_approval, cap)
        .validate()
        .expect("approval expiry at the cap is valid");
    with_durations(below, below, below)
        .validate()
        .expect("all durations just below the cap are valid");
    with_durations(cap, cap, cap)
        .validate()
        .expect("all durations at the cap are valid");
}

#[test]
fn each_duration_rejects_one_nanosecond_above_the_cap_with_its_own_diagnostic() {
    let above = Limits::M0_TEST_MAX_DURATION + Duration::from_nanos(1);

    let mut limits = Limits::m0_test();
    limits.run_duration = above;
    assert_duration_error("run_duration", &limits, RUN_CAP_MESSAGE);

    let mut limits = Limits::m0_test();
    limits.per_tool_timeout = above;
    assert_duration_error("per_tool_timeout", &limits, TOOL_CAP_MESSAGE);

    let mut limits = Limits::m0_test();
    limits.approval_expiry = above;
    assert_duration_error("approval_expiry", &limits, APPROVAL_CAP_MESSAGE);
}

#[test]
fn cap_violation_is_rejected_while_the_other_durations_are_at_either_extreme() {
    let above = Limits::M0_TEST_MAX_DURATION + Duration::from_nanos(1);
    let below = Duration::from_nanos(1);
    let cap = Limits::M0_TEST_MAX_DURATION;

    assert_duration_error(
        "run_duration",
        &with_durations(above, below, cap),
        RUN_CAP_MESSAGE,
    );
    assert_duration_error(
        "run_duration",
        &with_durations(above, cap, below),
        RUN_CAP_MESSAGE,
    );
    assert_duration_error(
        "per_tool_timeout",
        &with_durations(below, above, cap),
        TOOL_CAP_MESSAGE,
    );
    assert_duration_error(
        "per_tool_timeout",
        &with_durations(cap, above, below),
        TOOL_CAP_MESSAGE,
    );
    assert_duration_error(
        "approval_expiry",
        &with_durations(below, cap, above),
        APPROVAL_CAP_MESSAGE,
    );
    assert_duration_error(
        "approval_expiry",
        &with_durations(cap, below, above),
        APPROVAL_CAP_MESSAGE,
    );
}

#[test]
fn each_duration_rejects_duration_max_while_the_others_are_minimal() {
    let minimal = Duration::from_nanos(1);

    assert_duration_error(
        "run_duration",
        &with_durations(Duration::MAX, minimal, minimal),
        RUN_CAP_MESSAGE,
    );
    assert_duration_error(
        "per_tool_timeout",
        &with_durations(minimal, Duration::MAX, minimal),
        TOOL_CAP_MESSAGE,
    );
    assert_duration_error(
        "approval_expiry",
        &with_durations(minimal, minimal, Duration::MAX),
        APPROVAL_CAP_MESSAGE,
    );
}

#[test]
fn durations_are_mutually_independent_within_bounds() {
    let values = [
        Duration::from_nanos(1),
        Duration::from_secs(1),
        Duration::from_secs(3_600),
        Limits::M0_TEST_MAX_DURATION,
    ];

    for run in values {
        for tool in values {
            for approval in values {
                with_durations(run, tool, approval)
                    .validate()
                    .expect("within-bounds durations never constrain each other");
            }
        }
    }
}

#[test]
fn short_run_with_longer_operation_durations_is_valid_and_the_run_budget_wins() {
    let limits = with_durations(
        Duration::from_millis(10),
        Duration::from_secs(60),
        Duration::from_secs(120),
    );
    limits
        .validate()
        .expect("a short run may carry the longer operation defaults");

    assert!(limits.check_run_elapsed(Duration::ZERO).is_ok());
    assert!(limits.check_run_elapsed(Duration::from_millis(9)).is_ok());

    let error = limits
        .check_run_elapsed(Duration::from_millis(10))
        .expect_err("elapsed equal to the run budget is exhausted");
    assert_eq!(error.category(), ErrorCategory::ResourceLimit);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(error.message(), RUN_EXHAUSTED_MESSAGE);

    assert!(
        limits.check_run_elapsed(Duration::from_secs(60)).is_err(),
        "the longer per-tool timeout does not extend the run"
    );
    assert!(
        limits
            .check_run_elapsed(Limits::M0_TEST_MAX_DURATION)
            .is_err(),
        "the longer approval expiry does not extend the run"
    );
}

#[test]
fn run_exhaustion_depends_only_on_run_duration() {
    let run_values = [
        Duration::from_nanos(1),
        Duration::from_secs(5),
        Limits::M0_TEST_MAX_DURATION,
    ];
    let elapsed_values = [
        Duration::ZERO,
        Duration::from_nanos(1),
        Duration::from_secs(4),
        Duration::from_secs(5),
        Limits::M0_TEST_MAX_DURATION,
    ];
    let operation_values = [Duration::from_nanos(1), Limits::M0_TEST_MAX_DURATION];

    for run in run_values {
        for tool in operation_values {
            for approval in operation_values {
                let limits = with_durations(run, tool, approval);
                limits
                    .validate()
                    .expect("within-bounds durations are valid");
                for elapsed in elapsed_values {
                    assert_eq!(
                        limits.check_run_elapsed(elapsed).is_ok(),
                        elapsed < run,
                        "run {run:?}, tool {tool:?}, approval {approval:?}, elapsed {elapsed:?}"
                    );
                }
            }
        }
    }
}
