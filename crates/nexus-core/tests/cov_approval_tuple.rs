//! Public-boundary hardening for the exact-tuple approval binding.
//!
//! The unit tests inside `nexus-core` exercise the same behavior from inside
//! the module; these tests pin the documented contract through the public API
//! only: constructor bounds for normalized arguments and approved scope,
//! accessor equality, field-by-field binding equality, exact tool
//! identity/revision and argument-text matching, policy revision equality,
//! and the reached-boundary expiry rule.
//!
//! Determinism: no sleeps and no wall-clock time. Expiry is a pure comparison
//! over supplied monotonic elapsed readings, so every assertion is
//! independent of the machine clock.

#![forbid(unsafe_code)]

use std::time::Duration;

use nexus_core::approval::MAX_SCOPE_BYTES;
use nexus_core::{
    AgentError, ApprovalBinding, ApprovalId, ApprovedScope, CallId, ErrorCategory, Limits,
    M0_REVISION, NormalizedArgs, RetryGuidance, RunId, ToolId,
};

const RUN: &str = "run-1";
const CALL: &str = "call-1";
const TOOL: &str = "host_read";
const ARGS: &str = r#"{"path":"src"}"#;
const SCOPE: &str = "project-read";
const EXPIRES: Duration = Duration::from_secs(120);

fn normalized(raw: &str) -> NormalizedArgs {
    NormalizedArgs::new(raw).expect("test argument text is valid")
}

fn approved_scope(raw: &str) -> ApprovedScope {
    ApprovedScope::new(raw).expect("test scope is within bound")
}

fn run_id(raw: &str) -> RunId {
    RunId::new(raw).expect("test run id is valid")
}

fn call_id(raw: &str) -> CallId {
    CallId::new(raw).expect("test call id is valid")
}

fn tool_at(revision: u32) -> ToolId {
    ToolId::new(TOOL, revision).expect("test tool identity is valid")
}

#[allow(clippy::too_many_arguments)]
fn binding_with(
    approval: &str,
    run: &str,
    call: &str,
    tool: &str,
    tool_revision: u32,
    args: &str,
    scope: &str,
    expires_at_elapsed: Duration,
    policy_revision: u32,
) -> ApprovalBinding {
    ApprovalBinding::new(
        ApprovalId::new(approval).expect("test approval id is valid"),
        RunId::new(run).expect("test run id is valid"),
        CallId::new(call).expect("test call id is valid"),
        ToolId::new(tool, tool_revision).expect("test tool identity is valid"),
        NormalizedArgs::new(args).expect("test argument text is valid"),
        ApprovedScope::new(scope).expect("test scope is within bound"),
        expires_at_elapsed,
        policy_revision,
    )
}

fn grant() -> ApprovalBinding {
    binding_with(
        "appr-1",
        RUN,
        CALL,
        TOOL,
        M0_REVISION,
        ARGS,
        SCOPE,
        EXPIRES,
        M0_REVISION,
    )
}

fn check_exact(binding: &ApprovalBinding, now_elapsed: Duration) -> Result<(), AgentError> {
    binding.check_valid_for_dispatch(
        &run_id(RUN),
        &call_id(CALL),
        &tool_at(M0_REVISION),
        &normalized(ARGS),
        M0_REVISION,
        now_elapsed,
    )
}

fn assert_invalid<T: std::fmt::Debug>(result: Result<T, AgentError>, message: &str) {
    let error = result.expect_err("invalid input must be rejected");
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(error.message(), message);
}

fn assert_denied(result: Result<(), AgentError>, message: &str) {
    let error = result.expect_err("a deviation must deny dispatch");
    assert_eq!(error.category(), ErrorCategory::PermissionDenied);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(error.message(), message);
}

#[test]
fn normalized_args_accepts_object_root_and_preserves_exact_text() {
    for raw in [
        r#"{}"#,
        r#"{"path":"src"}"#,
        r#"{ "a" : 1 }"#,
        "  {\"path\":\"src\"}\n",
    ] {
        let args = NormalizedArgs::new(raw).expect("object-root text is valid");
        assert_eq!(args.as_str(), raw, "the carrier keeps the exact text");
        assert_eq!(args, normalized(raw), "equal text compares equal");
    }
    assert_eq!(
        NormalizedArgs::new(String::from(ARGS))
            .expect("owned text is valid")
            .as_str(),
        ARGS
    );
}

#[test]
fn normalized_args_rejects_empty_shape_and_budget_violations() {
    assert_invalid(NormalizedArgs::new(""), "tool arguments are empty");
    for raw in [
        " ",
        "\n\t",
        "[1,2]",
        r#"{"path":"src""#,
        r#"{"path":"src"}x"#,
        "}",
    ] {
        assert_invalid(
            NormalizedArgs::new(raw),
            "tool arguments must be an object-root value",
        );
    }
    // The budget is checked before the shape: an oversize non-object still
    // reports the assembly budget, not the root-shape diagnostic.
    assert_invalid(
        NormalizedArgs::new("[".repeat(Limits::M0_TEST_ARG_ASSEMBLY_BYTES + 1)),
        "tool arguments exceed assembly budget",
    );
}

#[test]
fn normalized_args_accepts_exactly_the_assembly_budget() {
    let budget = Limits::M0_TEST_ARG_ASSEMBLY_BYTES;
    let filler = "a".repeat(budget - 8);
    let exact = format!(r#"{{"a":"{filler}"}}"#);
    assert_eq!(exact.len(), budget);
    let accepted = NormalizedArgs::new(exact.clone()).expect("the exact budget is accepted");
    assert_eq!(accepted.as_str(), exact);

    let over = format!(r#"{{"a":"{filler}x"}}"#);
    assert_eq!(over.len(), budget + 1);
    assert_invalid(
        NormalizedArgs::new(over),
        "tool arguments exceed assembly budget",
    );
}

#[test]
fn approved_scope_allows_empty_and_bounds_by_bytes() {
    let empty = ApprovedScope::new("").expect("empty scope means no extra resource");
    assert_eq!(empty.as_str(), "");
    assert_eq!(empty, approved_scope(""));

    let exact = "s".repeat(MAX_SCOPE_BYTES);
    assert_eq!(exact.len(), MAX_SCOPE_BYTES);
    assert_eq!(
        ApprovedScope::new(exact.clone())
            .expect("the exact bound is accepted")
            .as_str(),
        exact
    );
    assert_invalid(
        ApprovedScope::new("s".repeat(MAX_SCOPE_BYTES + 1)),
        "approved scope exceeds its bound",
    );

    // The bound counts bytes, not characters.
    let multibyte = "é".repeat(MAX_SCOPE_BYTES / 2);
    assert_eq!(multibyte.len(), MAX_SCOPE_BYTES);
    assert!(ApprovedScope::new(multibyte).is_ok());
    assert_invalid(
        ApprovedScope::new("é".repeat(MAX_SCOPE_BYTES / 2 + 1)),
        "approved scope exceeds its bound",
    );
}

#[test]
fn binding_accessors_expose_the_constructed_tuple() {
    let binding = grant();
    assert_eq!(
        binding.approval(),
        &ApprovalId::new("appr-1").expect("valid")
    );
    assert_eq!(binding.run(), &run_id(RUN));
    assert_eq!(binding.call(), &call_id(CALL));
    assert_eq!(binding.tool(), &tool_at(M0_REVISION));
    assert_eq!(binding.tool().name(), TOOL);
    assert_eq!(binding.tool().revision(), M0_REVISION);
    assert_eq!(binding.args(), &normalized(ARGS));
    assert_eq!(binding.args().as_str(), ARGS);
    assert_eq!(binding.scope(), &approved_scope(SCOPE));
    assert_eq!(binding.scope().as_str(), SCOPE);
    assert_eq!(binding.policy_revision(), M0_REVISION);
    assert_eq!(binding.expires_at_elapsed(), EXPIRES);
    assert_eq!(binding.clone(), binding);
}

#[test]
fn binding_equality_tracks_every_field() {
    let base = grant();
    let variants = [
        binding_with(
            "appr-2",
            RUN,
            CALL,
            TOOL,
            M0_REVISION,
            ARGS,
            SCOPE,
            EXPIRES,
            M0_REVISION,
        ),
        binding_with(
            "appr-1",
            "run-2",
            CALL,
            TOOL,
            M0_REVISION,
            ARGS,
            SCOPE,
            EXPIRES,
            M0_REVISION,
        ),
        binding_with(
            "appr-1",
            RUN,
            "call-2",
            TOOL,
            M0_REVISION,
            ARGS,
            SCOPE,
            EXPIRES,
            M0_REVISION,
        ),
        binding_with(
            "appr-1",
            RUN,
            CALL,
            "host_write",
            M0_REVISION,
            ARGS,
            SCOPE,
            EXPIRES,
            M0_REVISION,
        ),
        binding_with(
            "appr-1",
            RUN,
            CALL,
            TOOL,
            M0_REVISION + 1,
            ARGS,
            SCOPE,
            EXPIRES,
            M0_REVISION,
        ),
        binding_with(
            "appr-1",
            RUN,
            CALL,
            TOOL,
            M0_REVISION,
            r#"{"path":"other"}"#,
            SCOPE,
            EXPIRES,
            M0_REVISION,
        ),
        binding_with(
            "appr-1",
            RUN,
            CALL,
            TOOL,
            M0_REVISION,
            ARGS,
            "project-write",
            EXPIRES,
            M0_REVISION,
        ),
        binding_with(
            "appr-1",
            RUN,
            CALL,
            TOOL,
            M0_REVISION,
            ARGS,
            SCOPE,
            EXPIRES + Duration::from_secs(1),
            M0_REVISION,
        ),
        binding_with(
            "appr-1",
            RUN,
            CALL,
            TOOL,
            M0_REVISION,
            ARGS,
            SCOPE,
            EXPIRES,
            M0_REVISION + 1,
        ),
    ];
    for (index, variant) in variants.iter().enumerate() {
        assert_ne!(&base, variant, "variant {index} changes exactly one field");
    }
    assert_eq!(
        base,
        grant(),
        "independently built identical tuples are equal"
    );
}

#[test]
fn dispatch_accepts_only_the_exact_tuple_and_repeats_safely() {
    let binding = grant();
    check_exact(&binding, Duration::ZERO).expect("the exact tuple authorizes at issue time");
    check_exact(&binding, EXPIRES - Duration::from_millis(1))
        .expect("one tick before expiry authorizes");
    // A passing check neither consumes nor mutates the grant.
    check_exact(&binding, Duration::from_secs(1)).expect("repeats stay authorized");
    assert_eq!(binding, grant());
}

#[test]
fn dispatch_denies_each_deviation_with_its_own_diagnostic() {
    let binding = grant();
    assert_denied(
        binding.check_valid_for_dispatch(
            &run_id("run-2"),
            &call_id(CALL),
            &tool_at(M0_REVISION),
            &normalized(ARGS),
            M0_REVISION,
            Duration::ZERO,
        ),
        "approval run mismatch",
    );
    assert_denied(
        binding.check_valid_for_dispatch(
            &run_id(RUN),
            &call_id("call-2"),
            &tool_at(M0_REVISION),
            &normalized(ARGS),
            M0_REVISION,
            Duration::ZERO,
        ),
        "approval call mismatch",
    );
    assert_denied(
        binding.check_valid_for_dispatch(
            &run_id(RUN),
            &call_id(CALL),
            &ToolId::new("host_write", M0_REVISION).expect("valid"),
            &normalized(ARGS),
            M0_REVISION,
            Duration::ZERO,
        ),
        "approval tool identity or revision mismatch",
    );
    assert_denied(
        binding.check_valid_for_dispatch(
            &run_id(RUN),
            &call_id(CALL),
            &tool_at(M0_REVISION + 1),
            &normalized(ARGS),
            M0_REVISION,
            Duration::ZERO,
        ),
        "approval tool identity or revision mismatch",
    );
    assert_denied(
        binding.check_valid_for_dispatch(
            &run_id(RUN),
            &call_id(CALL),
            &tool_at(M0_REVISION),
            &normalized(r#"{"path":"other"}"#),
            M0_REVISION,
            Duration::ZERO,
        ),
        "approval arguments changed",
    );
    assert_denied(
        binding.check_valid_for_dispatch(
            &run_id(RUN),
            &call_id(CALL),
            &tool_at(M0_REVISION),
            &normalized(ARGS),
            M0_REVISION + 1,
            Duration::ZERO,
        ),
        "approval policy revision mismatch",
    );
    assert_denied(check_exact(&binding, EXPIRES), "approval expired");
}

#[test]
fn dispatch_denial_precedence_is_run_call_tool_args_policy_expiry() {
    let binding = grant();
    let exact_run = run_id(RUN);
    let exact_call = call_id(CALL);
    let exact_tool = tool_at(M0_REVISION);
    let exact_args = normalized(ARGS);
    let other_run = run_id("run-2");
    let other_call = call_id("call-2");
    let other_tool = tool_at(M0_REVISION + 1);
    let other_args = normalized(r#"{"path":"other"}"#);

    // Every field deviates: the run check is first.
    assert_denied(
        binding.check_valid_for_dispatch(
            &other_run,
            &other_call,
            &other_tool,
            &other_args,
            M0_REVISION + 1,
            EXPIRES,
        ),
        "approval run mismatch",
    );
    // Run fixed: call is next.
    assert_denied(
        binding.check_valid_for_dispatch(
            &exact_run,
            &other_call,
            &other_tool,
            &other_args,
            M0_REVISION + 1,
            EXPIRES,
        ),
        "approval call mismatch",
    );
    // Run and call fixed: tool identity wins over arguments.
    assert_denied(
        binding.check_valid_for_dispatch(
            &exact_run,
            &exact_call,
            &other_tool,
            &other_args,
            M0_REVISION + 1,
            EXPIRES,
        ),
        "approval tool identity or revision mismatch",
    );
    // Through tool fixed: arguments win over policy and expiry.
    assert_denied(
        binding.check_valid_for_dispatch(
            &exact_run,
            &exact_call,
            &exact_tool,
            &other_args,
            M0_REVISION + 1,
            EXPIRES,
        ),
        "approval arguments changed",
    );
    // Through arguments fixed: policy wins over expiry.
    assert_denied(
        binding.check_valid_for_dispatch(
            &exact_run,
            &exact_call,
            &exact_tool,
            &exact_args,
            M0_REVISION + 1,
            EXPIRES,
        ),
        "approval policy revision mismatch",
    );
    // Only expiry remains deviating.
    assert_denied(
        binding.check_valid_for_dispatch(
            &exact_run,
            &exact_call,
            &exact_tool,
            &exact_args,
            M0_REVISION,
            EXPIRES,
        ),
        "approval expired",
    );
}

#[test]
fn arguments_match_only_on_byte_identical_text() {
    let binding = binding_with(
        "appr-1",
        RUN,
        CALL,
        TOOL,
        M0_REVISION,
        r#"{"a":1,"b":2}"#,
        SCOPE,
        EXPIRES,
        M0_REVISION,
    );
    binding
        .check_valid_for_dispatch(
            &run_id(RUN),
            &call_id(CALL),
            &tool_at(M0_REVISION),
            &normalized(r#"{"a":1,"b":2}"#),
            M0_REVISION,
            Duration::ZERO,
        )
        .expect("byte-identical arguments authorize");

    for variant in [
        r#"{ "a":1,"b":2}"#,
        r#"{"a": 1,"b":2}"#,
        r#"{"b":2,"a":1}"#,
        r#"{"a":1,"b":2} "#,
        r#"{"a":1}"#,
    ] {
        assert_denied(
            binding.check_valid_for_dispatch(
                &run_id(RUN),
                &call_id(CALL),
                &tool_at(M0_REVISION),
                &normalized(variant),
                M0_REVISION,
                Duration::ZERO,
            ),
            "approval arguments changed",
        );
    }
}

#[test]
fn tool_identity_and_revision_match_exactly() {
    let binding = grant();
    let exact = tool_at(M0_REVISION);
    assert!(binding.tool().is_compatible_with(&exact));
    assert!(!binding.tool().is_compatible_with(&tool_at(M0_REVISION + 1)));
    assert!(
        !binding
            .tool()
            .is_compatible_with(&ToolId::new("host_write", M0_REVISION).expect("valid"))
    );

    binding
        .check_valid_for_dispatch(
            &run_id(RUN),
            &call_id(CALL),
            &exact,
            &normalized(ARGS),
            M0_REVISION,
            Duration::ZERO,
        )
        .expect("same name and revision authorize");

    for deviating in [
        tool_at(M0_REVISION + 1),
        ToolId::new("host_write", M0_REVISION).expect("valid"),
    ] {
        assert_denied(
            binding.check_valid_for_dispatch(
                &run_id(RUN),
                &call_id(CALL),
                &deviating,
                &normalized(ARGS),
                M0_REVISION,
                Duration::ZERO,
            ),
            "approval tool identity or revision mismatch",
        );
    }
}

#[test]
fn expiry_is_reached_boundary_inclusive() {
    let binding = grant();
    assert!(!binding.is_expired(Duration::ZERO));
    assert!(!binding.is_expired(EXPIRES - Duration::from_millis(1)));
    assert!(binding.is_expired(EXPIRES));
    assert!(binding.is_expired(EXPIRES + Duration::from_millis(1)));
    assert!(binding.is_expired(Duration::MAX));

    check_exact(&binding, EXPIRES - Duration::from_millis(1))
        .expect("just before expiry authorizes");
    assert_denied(check_exact(&binding, EXPIRES), "approval expired");

    let zero_lifetime = binding_with(
        "appr-0",
        RUN,
        CALL,
        TOOL,
        M0_REVISION,
        ARGS,
        SCOPE,
        Duration::ZERO,
        M0_REVISION,
    );
    assert!(zero_lifetime.is_expired(Duration::ZERO));
    assert_denied(
        check_exact(&zero_lifetime, Duration::ZERO),
        "approval expired",
    );
}

#[test]
fn scope_is_carried_but_not_compared_by_the_dispatch_check() {
    // `check_valid_for_dispatch` compares run, call, tool, arguments, policy
    // revision, and expiry. The approved scope travels with the grant for the
    // runtime's dispatch policy, so a wider scope on the same tuple still
    // passes this check; scope enforcement stays the runtime's job (P3).
    let wide = binding_with(
        "appr-1",
        RUN,
        CALL,
        TOOL,
        M0_REVISION,
        ARGS,
        "project-write",
        EXPIRES,
        M0_REVISION,
    );
    assert_eq!(wide.scope().as_str(), "project-write");
    check_exact(&wide, Duration::ZERO).expect("scope is carried, not compared here");
}
