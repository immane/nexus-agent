//! Coverage hardening for context and output budget clamping in
//! [`nexus_core::Limits`].
//!
//! Every assertion goes through the public boundary. The suite pins the M0
//! lock-table values independently of the constants, proves the boundary
//! checks enforce the effective per-instance budget rather than a global
//! constant, covers the inclusive below-limit pass range for retained context
//! items and tool output bytes, and requires exhaustion to surface as an
//! explicit [`ErrorCategory::ResourceLimit`] with no retry guidance.
//!
//! Deterministic: no clock, no randomness, no I/O, no runtime.

#![forbid(unsafe_code)]

use std::time::Duration;

use nexus_core::error::MAX_MESSAGE_LEN;
use nexus_core::{AgentError, ApprovedScope, ErrorCategory, Limits, RetryGuidance, ToolContext};

/// Asserts the documented exhaustion shape and returns the error for
/// message-level checks.
fn resource_limit(result: Result<(), AgentError>, label: &str) -> AgentError {
    let error = result.expect_err(label);
    assert_eq!(error.category(), ErrorCategory::ResourceLimit, "{label}");
    assert_eq!(error.category().as_str(), "resource-limit", "{label}");
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry, "{label}");
    assert!(!error.message().is_empty(), "{label}");
    assert!(error.message().len() <= MAX_MESSAGE_LEN, "{label}");
    error
}

#[test]
fn lock_table_values_are_pinned_by_the_public_constants() {
    // Transcribed from docs/tasks/06-m0-lock.md section 1. The duplication is
    // deliberate: editing a constant without the lock table must fail here
    // instead of passing by comparing the constant with itself.
    assert_eq!(Limits::M0_TEST_MODEL_TURNS_PER_RUN, 8);
    assert_eq!(Limits::M0_TEST_TOOL_CALLS_PER_RUN, 16);
    assert_eq!(Limits::M0_TEST_TOOL_CALLS_PER_TURN, 8);
    assert_eq!(Limits::M0_TEST_RUN_DURATION_SECS, 300);
    assert_eq!(Limits::M0_TEST_PER_TOOL_TIMEOUT_SECS, 60);
    assert_eq!(Limits::M0_TEST_RETAINED_CONTEXT_ITEMS, 128);
    assert_eq!(Limits::M0_TEST_ARG_ASSEMBLY_BYTES, 65_536);
    assert_eq!(Limits::M0_TEST_TOOL_OUTPUT_BYTES, 262_144);
    assert_eq!(Limits::M0_TEST_EVENT_DATA_CAPACITY, 1_024);
    assert_eq!(Limits::M0_TEST_EVENT_CONTROL_CAPACITY, 128);
    assert_eq!(Limits::M0_TEST_MAX_CONCURRENT_OPS, 8);
    assert_eq!(Limits::M0_TEST_APPROVAL_EXPIRY_SECS, 120);
    assert_eq!(Limits::M0_TEST_MAX_DURATION, Duration::from_secs(86_400));
}

#[test]
fn m0_test_profile_is_computed_from_the_locked_constants() {
    let expected = Limits {
        max_model_turns_per_run: Limits::M0_TEST_MODEL_TURNS_PER_RUN,
        max_tool_calls_per_run: Limits::M0_TEST_TOOL_CALLS_PER_RUN,
        max_tool_calls_per_turn: Limits::M0_TEST_TOOL_CALLS_PER_TURN,
        run_duration: Duration::from_secs(Limits::M0_TEST_RUN_DURATION_SECS),
        per_tool_timeout: Duration::from_secs(Limits::M0_TEST_PER_TOOL_TIMEOUT_SECS),
        retained_context_items: Limits::M0_TEST_RETAINED_CONTEXT_ITEMS,
        max_arg_assembly_bytes: Limits::M0_TEST_ARG_ASSEMBLY_BYTES,
        max_tool_output_bytes: Limits::M0_TEST_TOOL_OUTPUT_BYTES,
        event_data_capacity: Limits::M0_TEST_EVENT_DATA_CAPACITY,
        event_control_capacity: Limits::M0_TEST_EVENT_CONTROL_CAPACITY,
        max_concurrent_ops: Limits::M0_TEST_MAX_CONCURRENT_OPS,
        approval_expiry: Duration::from_secs(Limits::M0_TEST_APPROVAL_EXPIRY_SECS),
    };
    assert_eq!(Limits::m0_test(), expected);
    Limits::m0_test()
        .validate()
        .expect("M0-test budgets are valid");
}

#[test]
fn derived_public_caps_agree_with_the_locked_context_and_turn_values() {
    // The runtime computes effective bounds as a minimum of the configured
    // budget and these public caps; a drift here silently changes the
    // effective context and per-turn budgets.
    assert_eq!(
        nexus_core::provider::MAX_CONVERSATION_ITEMS,
        Limits::M0_TEST_RETAINED_CONTEXT_ITEMS
    );
    assert_eq!(
        nexus_core::store::MAX_STORED_MESSAGES,
        Limits::M0_TEST_RETAINED_CONTEXT_ITEMS
    );
    assert_eq!(
        nexus_core::provider::MAX_TOOL_DEFINITIONS,
        Limits::M0_TEST_TOOL_CALLS_PER_TURN as usize
    );
}

#[test]
fn effective_context_budget_is_the_configured_value() {
    let tight = Limits {
        retained_context_items: 3,
        ..Limits::m0_test()
    };
    tight.validate().expect("a reduced context budget is valid");

    for count in 0..=tight.retained_context_items {
        assert!(tight.check_context_items(count).is_ok(), "count {count}");
    }
    let error = resource_limit(
        tight.check_context_items(tight.retained_context_items + 1),
        "context above the effective budget",
    );
    assert!(
        error.message().contains("context"),
        "message {:?} names the exhausted context budget",
        error.message()
    );
    resource_limit(
        tight.check_context_items(usize::MAX),
        "context at usize::MAX",
    );

    // The M0 profile is untouched: the reduction changed the effective
    // budget, not the locked constant.
    assert!(Limits::m0_test().check_context_items(128).is_ok());
}

#[test]
fn effective_output_budget_is_the_configured_value() {
    let tight = Limits {
        max_tool_output_bytes: 8,
        ..Limits::m0_test()
    };
    tight.validate().expect("a reduced output budget is valid");

    for len in 0..=tight.max_tool_output_bytes {
        assert!(tight.check_tool_output_bytes(len).is_ok(), "len {len}");
    }
    let error = resource_limit(
        tight.check_tool_output_bytes(tight.max_tool_output_bytes + 1),
        "output above the effective budget",
    );
    assert!(
        error.message().contains("output"),
        "message {:?} names the exhausted output budget",
        error.message()
    );
    resource_limit(
        tight.check_tool_output_bytes(usize::MAX),
        "output at usize::MAX",
    );

    assert!(Limits::m0_test().check_tool_output_bytes(262_144).is_ok());
}

#[test]
fn below_limit_values_pass_and_the_limit_itself_is_inclusive() {
    let limits = Limits::m0_test();
    for count in 0..=limits.retained_context_items {
        assert!(
            limits.check_context_items(count).is_ok(),
            "retained context {count} is at or below the budget"
        );
    }
    let output_samples = [
        0,
        1,
        limits.max_tool_output_bytes / 2,
        limits.max_tool_output_bytes - 1,
        limits.max_tool_output_bytes,
    ];
    for len in output_samples {
        assert!(
            limits.check_tool_output_bytes(len).is_ok(),
            "output {len} is at or below the budget"
        );
    }

    resource_limit(
        limits.check_context_items(limits.retained_context_items + 1),
        "one context item above the limit",
    );
    resource_limit(
        limits.check_tool_output_bytes(limits.max_tool_output_bytes + 1),
        "one output byte above the limit",
    );
}

#[test]
fn exhaustion_maps_to_resource_limit_without_retry() {
    let limits = Limits::m0_test();
    let context = resource_limit(
        limits.check_context_items(limits.retained_context_items + 1),
        "context exhaustion",
    );
    let output = resource_limit(
        limits.check_tool_output_bytes(limits.max_tool_output_bytes + 1),
        "output exhaustion",
    );

    assert!(
        context.message().contains("context budget"),
        "message {:?}",
        context.message()
    );
    assert!(
        output.message().contains("output budget"),
        "message {:?}",
        output.message()
    );
    assert_ne!(
        context.message(),
        output.message(),
        "context and output exhaustion stay distinguishable"
    );
}

#[test]
fn zero_is_never_infinity_for_any_budget_or_duration() {
    fn check(name: &str, mutate: fn(&mut Limits)) {
        let mut limits = Limits::m0_test();
        mutate(&mut limits);
        let error = limits.validate().expect_err(name);
        assert_eq!(error.category(), ErrorCategory::ResourceLimit, "{name}");
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry, "{name}");
        assert!(
            error.message().contains("nonzero"),
            "{name}: message {:?}",
            error.message()
        );
    }
    check("model turns", |limits| limits.max_model_turns_per_run = 0);
    check("tool calls per run", |limits| {
        limits.max_tool_calls_per_run = 0;
    });
    check("tool calls per turn", |limits| {
        limits.max_tool_calls_per_turn = 0;
    });
    check("retained context", |limits| {
        limits.retained_context_items = 0
    });
    check("arg assembly", |limits| limits.max_arg_assembly_bytes = 0);
    check("tool output", |limits| limits.max_tool_output_bytes = 0);
    check("event data", |limits| limits.event_data_capacity = 0);
    check("event control", |limits| limits.event_control_capacity = 0);
    check("concurrent ops", |limits| limits.max_concurrent_ops = 0);
    check("run duration", |limits| {
        limits.run_duration = Duration::ZERO
    });
    check("per-tool timeout", |limits| {
        limits.per_tool_timeout = Duration::ZERO;
    });
    check("approval expiry", |limits| {
        limits.approval_expiry = Duration::ZERO;
    });
}

#[test]
fn argument_assembly_budget_is_clamped_at_validation_time() {
    let mut at_cap = Limits::m0_test();
    at_cap.max_arg_assembly_bytes = Limits::M0_TEST_ARG_ASSEMBLY_BYTES;
    at_cap
        .validate()
        .expect("the M0 assembly cap itself is accepted");

    let mut above_cap = Limits::m0_test();
    above_cap.max_arg_assembly_bytes = Limits::M0_TEST_ARG_ASSEMBLY_BYTES + 1;
    let error = resource_limit(above_cap.validate(), "assembly above the M0 cap");
    assert!(
        error.message().contains("assembly"),
        "message {:?}",
        error.message()
    );
}

#[test]
fn output_budget_above_the_global_cap_is_clamped_at_the_context_boundary() {
    let limits = Limits {
        max_tool_output_bytes: Limits::M0_TEST_TOOL_OUTPUT_BYTES + 1,
        ..Limits::m0_test()
    };
    // Validation caps argument assembly, not output bytes; the effective
    // output budget is the minimum of the configured value and the global M0
    // cap, applied when an operation context is built.
    limits
        .validate()
        .expect("output bytes are clamped at the boundary, not at validation");

    let scope = ApprovedScope::new("").expect("an empty scope is valid");
    let error = ToolContext::new(
        limits.max_tool_output_bytes,
        Duration::from_secs(1),
        false,
        scope.clone(),
    )
    .expect_err("an output budget above the global cap is rejected");
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);

    let at_cap = ToolContext::new(
        Limits::M0_TEST_TOOL_OUTPUT_BYTES,
        Duration::from_secs(1),
        false,
        scope,
    )
    .expect("the global cap itself is accepted");
    assert_eq!(
        at_cap.output_budget_bytes(),
        Limits::M0_TEST_TOOL_OUTPUT_BYTES
    );
    assert_eq!(
        Limits::m0_test().max_tool_output_bytes,
        Limits::M0_TEST_TOOL_OUTPUT_BYTES,
        "the M0 profile sits exactly at the global cap"
    );
}
