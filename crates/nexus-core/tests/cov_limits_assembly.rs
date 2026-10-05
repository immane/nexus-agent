//! Public-boundary hardening for the `Limits` argument-assembly budget.
//!
//! Every assertion goes through the `nexus-core` public API. The tests pin
//! the M0 hard maximum (the exact maximum is accepted, one byte over is
//! rejected), the rejection of zero budgets, the smallest nonzero budgets,
//! and the mapping of budget failures to `ErrorCategory::ResourceLimit` with
//! `RetryGuidance::DoNotRetry`. No clocks, randomness, or I/O are involved,
//! so the checks are deterministic.

#![forbid(unsafe_code)]

use std::time::Duration;

use nexus_core::{AgentError, ErrorCategory, Limits, RetryGuidance};

const M0_ASSEMBLY_MAX: usize = Limits::M0_TEST_ARG_ASSEMBLY_BYTES;

fn assert_resource_limit_without_retry(error: &AgentError) {
    assert_eq!(error.category(), ErrorCategory::ResourceLimit);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
}

#[test]
fn assembly_budget_accepts_the_m0_maximum_and_rejects_one_over() {
    let mut limits = Limits::m0_test();
    limits.max_arg_assembly_bytes = M0_ASSEMBLY_MAX;
    limits
        .validate()
        .expect("the exact M0 assembly maximum is accepted");

    limits.max_arg_assembly_bytes = M0_ASSEMBLY_MAX + 1;
    let error = limits
        .validate()
        .expect_err("one byte over the M0 assembly maximum is rejected");
    assert_resource_limit_without_retry(&error);
    assert!(
        error.message().contains("M0 maximum"),
        "unexpected message: {error}"
    );
}

#[test]
fn zero_assembly_budget_is_rejected_without_retry() {
    let mut limits = Limits::m0_test();
    limits.max_arg_assembly_bytes = 0;
    let error = limits
        .validate()
        .expect_err("a zero assembly budget must never mean infinity");
    assert_resource_limit_without_retry(&error);
    assert!(
        error.message().contains("nonzero"),
        "unexpected message: {error}"
    );
}

#[test]
fn minimum_nonzero_assembly_budget_is_accepted_and_checked_exactly() {
    let mut limits = Limits::m0_test();
    limits.max_arg_assembly_bytes = 1;
    limits
        .validate()
        .expect("the smallest nonzero assembly budget is valid");

    assert!(
        limits.check_arg_assembly_bytes(0).is_ok(),
        "empty assembly fits"
    );
    assert!(
        limits.check_arg_assembly_bytes(1).is_ok(),
        "exact assembly fits"
    );

    let error = limits
        .check_arg_assembly_bytes(2)
        .expect_err("one byte over the smallest assembly budget is rejected");
    assert_resource_limit_without_retry(&error);
    assert!(
        error.message().contains("assembly"),
        "unexpected message: {error}"
    );
}

#[test]
fn assembly_exhaustion_maps_to_resource_limit_without_retry() {
    let limits = Limits::m0_test();
    assert!(limits.check_arg_assembly_bytes(0).is_ok());
    assert!(limits.check_arg_assembly_bytes(M0_ASSEMBLY_MAX).is_ok());

    for len in [M0_ASSEMBLY_MAX + 1, M0_ASSEMBLY_MAX + 2, usize::MAX] {
        let error = limits
            .check_arg_assembly_bytes(len)
            .expect_err("over-budget assembly is rejected");
        assert_resource_limit_without_retry(&error);
    }
}

#[test]
fn every_zero_budget_is_rejected() {
    fn check(name: &str, zero_out: fn(&mut Limits)) {
        let mut limits = Limits::m0_test();
        zero_out(&mut limits);
        let error = limits
            .validate()
            .expect_err("a zero budget must never mean infinity");
        assert_eq!(error.category(), ErrorCategory::ResourceLimit, "{name}");
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry, "{name}");
    }
    check("max_model_turns_per_run", |l| l.max_model_turns_per_run = 0);
    check("max_tool_calls_per_run", |l| l.max_tool_calls_per_run = 0);
    check("max_tool_calls_per_turn", |l| l.max_tool_calls_per_turn = 0);
    check("retained_context_items", |l| l.retained_context_items = 0);
    check("max_arg_assembly_bytes", |l| l.max_arg_assembly_bytes = 0);
    check("max_tool_output_bytes", |l| l.max_tool_output_bytes = 0);
    check("event_data_capacity", |l| l.event_data_capacity = 0);
    check("event_control_capacity", |l| l.event_control_capacity = 0);
    check("max_concurrent_ops", |l| l.max_concurrent_ops = 0);
    check("run_duration", |l| l.run_duration = Duration::ZERO);
    check("per_tool_timeout", |l| l.per_tool_timeout = Duration::ZERO);
    check("approval_expiry", |l| l.approval_expiry = Duration::ZERO);
}

#[test]
fn every_minimum_nonzero_budget_is_accepted() {
    fn check(name: &str, minimize: fn(&mut Limits)) {
        let mut limits = Limits::m0_test();
        minimize(&mut limits);
        assert!(limits.validate().is_ok(), "{name}");
    }
    check("max_model_turns_per_run", |l| l.max_model_turns_per_run = 1);
    check("max_tool_calls_per_run", |l| l.max_tool_calls_per_run = 1);
    check("max_tool_calls_per_turn", |l| l.max_tool_calls_per_turn = 1);
    check("retained_context_items", |l| l.retained_context_items = 1);
    check("max_arg_assembly_bytes", |l| l.max_arg_assembly_bytes = 1);
    check("max_tool_output_bytes", |l| l.max_tool_output_bytes = 1);
    check("event_data_capacity", |l| l.event_data_capacity = 1);
    check("event_control_capacity", |l| l.event_control_capacity = 1);
    check("max_concurrent_ops", |l| l.max_concurrent_ops = 1);
    check("run_duration", |l| l.run_duration = Duration::from_nanos(1));
    check("per_tool_timeout", |l| {
        l.per_tool_timeout = Duration::from_nanos(1)
    });
    check("approval_expiry", |l| {
        l.approval_expiry = Duration::from_nanos(1)
    });
}
