#![forbid(unsafe_code)]

//! Coverage hardening: `ToolSpec` validation, exact revision identity, and
//! the `ToolPort` describe/execute contract.
//!
//! Exercises the public tool surface only: object-root schema rejection,
//! description and schema byte bounds, exact-equality revision identity, and
//! the synchronous port contract including live cancellation and an
//! evaluable monotonic deadline. Deterministic: fixed inputs, no randomness,
//! no threads, and no wall-clock reads; deadline fixtures use monotonic
//! instants derived from `Instant::now()`.

use std::time::{Duration, Instant};

use nexus_core::tool::{MAX_SCHEMA_BYTES, MAX_TOOL_DESCRIPTION_LEN};
use nexus_core::{
    AgentError, ApprovedScope, CallId, CancellationToken, EffectState, ErrorCategory, Evidence,
    ExecutionStatus, Limits, M0_REVISION, NormalizedArgs, RetryGuidance, RunId, ToolCall,
    ToolContext, ToolId, ToolOutcome, ToolPort, ToolSpec, TurnId,
};

const TOOL_NAME: &str = "host_read";
const DESCRIPTION: &str = "read files";
const OBJECT_SCHEMA: &str = r#"{"type":"object"}"#;

fn tool_id(revision: u32) -> ToolId {
    ToolId::new(TOOL_NAME, revision).expect("valid tool id")
}

fn spec() -> ToolSpec {
    ToolSpec::new(tool_id(M0_REVISION), DESCRIPTION, OBJECT_SCHEMA).expect("valid spec builds")
}

fn scope() -> ApprovedScope {
    ApprovedScope::new("project-read").expect("valid scope builds")
}

fn context(output_budget_bytes: usize) -> ToolContext {
    ToolContext::new(output_budget_bytes, Duration::from_secs(60), false, scope())
        .expect("valid context builds")
}

fn admitted_call(arguments: &str) -> ToolCall {
    ToolCall::new(
        RunId::new("run-cov").expect("valid run id"),
        TurnId::new("turn-cov").expect("valid turn id"),
        CallId::new("call-cov").expect("valid call id"),
        tool_id(M0_REVISION),
        NormalizedArgs::new(arguments).expect("valid normalized args"),
    )
}

fn assert_invalid_input(error: AgentError, case: &str) {
    assert_eq!(error.category(), ErrorCategory::InvalidInput, "{case}");
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry, "{case}");
    assert!(!error.message().is_empty(), "{case}");
}

/// Minimal deterministic port: echoes the admitted call, or maps the
/// context's combined dispatch check to an explicit outcome.
struct EchoPort {
    spec: ToolSpec,
}

impl ToolPort for EchoPort {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn execute(&self, call: &ToolCall, context: &ToolContext) -> ToolOutcome {
        if let Err(error) = context.check_active() {
            let status = match error.category() {
                ErrorCategory::Cancelled => ExecutionStatus::Cancelled,
                ErrorCategory::Timeout => ExecutionStatus::TimedOut,
                _ => ExecutionStatus::Failed,
            };
            return ToolOutcome::new(
                status,
                EffectState::Unknown,
                Evidence::HostObserved,
                error.message(),
                false,
            )
            .expect("bounded status outcome builds");
        }
        ToolOutcome::new(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            format!("{} {}", call.tool().name(), call.args().as_str()),
            false,
        )
        .expect("bounded success outcome builds")
    }
}

#[test]
fn spec_bounds_match_documented_values() {
    assert_eq!(MAX_TOOL_DESCRIPTION_LEN, 1024);
    assert_eq!(MAX_SCHEMA_BYTES, 65_536);
}

#[test]
fn non_object_schema_roots_are_rejected() {
    let cases = [
        ("array", r#"[1,2]"#),
        ("empty array", "[]"),
        ("array of object", "[{}]"),
        ("string", r#""text""#),
        ("number", "42"),
        ("true", "true"),
        ("false", "false"),
        ("null", "null"),
        ("empty", ""),
        ("whitespace", "   "),
        ("unterminated object", r#"{"a":1"#),
        ("leading garbage", r#"x{"a":1}"#),
        ("trailing garbage", r#"{"a":1}x"#),
        ("mismatched close", "{}]"),
    ];
    for (case, raw) in cases {
        let error = ToolSpec::new(tool_id(M0_REVISION), DESCRIPTION, raw)
            .expect_err("non-object schema must be rejected");
        assert_invalid_input(error, case);
    }
}

#[test]
fn object_root_shape_is_accepted_and_preserved_verbatim() {
    let padded = "  { \"type\": \"object\" }  ";
    let built =
        ToolSpec::new(tool_id(M0_REVISION), DESCRIPTION, padded).expect("object root builds");
    assert_eq!(built.id(), &tool_id(M0_REVISION));
    assert_eq!(built.description(), DESCRIPTION);
    assert_eq!(
        built.input_schema_json(),
        padded,
        "the shape check must not rewrite the text"
    );
    assert_eq!(
        built,
        ToolSpec::new(tool_id(M0_REVISION), DESCRIPTION, padded).expect("equal spec builds")
    );
    assert!(ToolSpec::new(tool_id(M0_REVISION), DESCRIPTION, "{}").is_ok());
}

#[test]
fn description_bound_is_exact() {
    let error = ToolSpec::new(tool_id(M0_REVISION), "", OBJECT_SCHEMA).expect_err("empty rejected");
    assert_invalid_input(error, "empty description");

    let max = "d".repeat(MAX_TOOL_DESCRIPTION_LEN);
    let built = ToolSpec::new(tool_id(M0_REVISION), max.clone(), OBJECT_SCHEMA)
        .expect("exact max description builds");
    assert_eq!(built.description(), max);
    assert_eq!(built.description().len(), MAX_TOOL_DESCRIPTION_LEN);

    let error = ToolSpec::new(
        tool_id(M0_REVISION),
        "d".repeat(MAX_TOOL_DESCRIPTION_LEN + 1),
        OBJECT_SCHEMA,
    )
    .expect_err("over max description rejected");
    assert_invalid_input(error, "over max description");

    // The bound is in bytes for the description as well.
    let multibyte_max = "é".repeat(MAX_TOOL_DESCRIPTION_LEN / 2);
    assert_eq!(multibyte_max.len(), MAX_TOOL_DESCRIPTION_LEN);
    ToolSpec::new(tool_id(M0_REVISION), multibyte_max.clone(), OBJECT_SCHEMA)
        .expect("multi-byte description at the byte bound builds");
    let multibyte_over = format!("{multibyte_max}x");
    assert_eq!(multibyte_over.len(), MAX_TOOL_DESCRIPTION_LEN + 1);
    let error = ToolSpec::new(tool_id(M0_REVISION), multibyte_over, OBJECT_SCHEMA)
        .expect_err("multi-byte description over the byte bound rejected");
    assert_invalid_input(error, "multi-byte description over the byte bound");
}

#[test]
fn schema_byte_bound_is_exact() {
    let at_max = format!("{{\"pad\":\"{}\"}}", "x".repeat(MAX_SCHEMA_BYTES - 10));
    assert_eq!(at_max.len(), MAX_SCHEMA_BYTES);
    let built =
        ToolSpec::new(tool_id(M0_REVISION), DESCRIPTION, at_max).expect("exact max schema builds");
    assert_eq!(built.input_schema_json().len(), MAX_SCHEMA_BYTES);

    let over_max = format!("{{\"pad\":\"{}\"}}", "x".repeat(MAX_SCHEMA_BYTES - 9));
    assert_eq!(over_max.len(), MAX_SCHEMA_BYTES + 1);
    let error = ToolSpec::new(tool_id(M0_REVISION), DESCRIPTION, over_max)
        .expect_err("over max schema rejected");
    assert_invalid_input(error, "over max schema");

    // The bound is in bytes: a multi-byte character cannot sneak past it.
    let multibyte = format!(
        "{{\"pad\":\"x{}\"}}",
        "é".repeat((MAX_SCHEMA_BYTES - 10) / 2)
    );
    assert_eq!(multibyte.len(), MAX_SCHEMA_BYTES + 1);
    assert!(
        ToolSpec::new(tool_id(M0_REVISION), DESCRIPTION, multibyte).is_err(),
        "multi-byte schema above the byte bound is rejected"
    );
}

#[test]
fn revision_identity_is_exact_equality() {
    let current = tool_id(M0_REVISION);
    let same = tool_id(M0_REVISION);
    let newer = tool_id(M0_REVISION + 1);
    let renamed = ToolId::new("host_write", M0_REVISION).expect("valid tool id");

    assert!(current.is_compatible_with(&same));
    assert!(same.is_compatible_with(&current));
    assert!(!current.is_compatible_with(&newer));
    assert!(!newer.is_compatible_with(&current));
    assert!(!current.is_compatible_with(&renamed));
    assert_eq!(current.revision(), M0_REVISION);
    assert_eq!(newer.revision(), M0_REVISION + 1);
    assert_eq!(current.to_string(), "host_read@0");

    let registered = spec();
    assert_eq!(registered.id(), &current);
    assert!(!registered.id().is_compatible_with(&newer));
    let newer_spec = ToolSpec::new(newer, DESCRIPTION, OBJECT_SCHEMA).expect("valid spec builds");
    assert_ne!(
        registered, newer_spec,
        "revision participates in spec identity"
    );
    assert_eq!(newer_spec.id().revision(), M0_REVISION + 1);
}

#[test]
fn port_describe_returns_the_registered_revision_exactly() {
    let registered = spec();
    let port = EchoPort {
        spec: registered.clone(),
    };
    let first = port.describe();
    let second = port.describe();
    assert_eq!(first, registered);
    assert_eq!(first, second, "describe is idempotent");
    assert_eq!(first.id(), &tool_id(M0_REVISION));
    assert!(first.id().is_compatible_with(&tool_id(M0_REVISION)));
    assert!(!first.id().is_compatible_with(&tool_id(M0_REVISION + 1)));
}

#[test]
fn port_execute_is_deterministic_and_reads_the_admitted_call() {
    let port = EchoPort { spec: spec() };
    let call = admitted_call(r#"{"path":"src"}"#);
    let context = context(1024);
    let call_before = call.clone();
    let context_before = context.clone();

    let first = port.execute(&call, &context);
    let second = port.execute(&call, &context);
    assert_eq!(
        first, second,
        "same call and context produce the same outcome"
    );
    assert_eq!(
        call, call_before,
        "execute must not mutate the admitted call"
    );
    assert_eq!(
        context, context_before,
        "execute must not mutate the context"
    );
    assert_eq!(first.status(), ExecutionStatus::Succeeded);
    assert_eq!(first.effect(), EffectState::KnownApplied);
    assert_eq!(first.evidence(), Evidence::HostObserved);
    assert!(first.content().contains(r#"{"path":"src"}"#));
    assert!(!first.is_truncated());

    let other = admitted_call(r#"{"path":"other"}"#);
    assert_ne!(
        first,
        port.execute(&other, &context),
        "arguments are reflected in the outcome"
    );
}

#[test]
fn port_execute_observes_live_cancellation_and_expired_deadline() {
    let port = EchoPort { spec: spec() };
    let call = admitted_call(r#"{"path":"src"}"#);

    let token = CancellationToken::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    let controlled = context(1024).with_control(token.clone(), deadline);
    assert_eq!(controlled.deadline(), Some(deadline));
    assert_eq!(
        port.execute(&call, &controlled).status(),
        ExecutionStatus::Succeeded
    );

    token.cancel();
    let cancelled = port.execute(&call, &controlled);
    assert_eq!(cancelled.status(), ExecutionStatus::Cancelled);
    assert_eq!(
        cancelled.effect(),
        EffectState::Unknown,
        "cancellation never implies rollback"
    );
    assert_eq!(cancelled.evidence(), Evidence::HostObserved);

    let past = Instant::now()
        .checked_sub(Duration::from_millis(1))
        .expect("test clock has history");
    let expired = context(1024).with_control(CancellationToken::new(), past);
    let timed_out = port.execute(&call, &expired);
    assert_eq!(timed_out.status(), ExecutionStatus::TimedOut);
    assert_eq!(
        timed_out.effect(),
        EffectState::Unknown,
        "timeout effects stay uncertain"
    );
}

#[test]
fn port_is_object_safe_and_usable_through_a_trait_object() {
    let ports: Vec<Box<dyn ToolPort>> = vec![Box::new(EchoPort { spec: spec() })];
    let call = admitted_call(r#"{"path":"src"}"#);
    for port in &ports {
        assert_eq!(port.describe(), spec());
        assert_eq!(
            port.execute(&call, &context(1024)).status(),
            ExecutionStatus::Succeeded
        );
    }
}

#[test]
fn context_output_budget_bound_is_exact() {
    let error =
        ToolContext::new(0, Duration::ZERO, false, scope()).expect_err("zero budget rejected");
    assert_invalid_input(error, "zero budget");

    let max = context(Limits::M0_TEST_TOOL_OUTPUT_BYTES);
    assert_eq!(max.output_budget_bytes(), Limits::M0_TEST_TOOL_OUTPUT_BYTES);

    let error = ToolContext::new(
        Limits::M0_TEST_TOOL_OUTPUT_BYTES + 1,
        Duration::ZERO,
        false,
        scope(),
    )
    .expect_err("over budget rejected");
    assert_invalid_input(error, "over budget");
}

#[test]
fn with_control_replaces_previous_control() {
    let first = CancellationToken::new();
    let second = CancellationToken::new();
    let first_deadline = Instant::now() + Duration::from_secs(30);
    let second_deadline = Instant::now() + Duration::from_secs(60);
    let controlled = context(1024)
        .with_control(first.clone(), first_deadline)
        .with_control(second.clone(), second_deadline);
    assert_eq!(controlled.deadline(), Some(second_deadline));

    first.cancel();
    assert!(
        !controlled.is_cancelled(),
        "the replaced token is no longer observed"
    );
    assert!(controlled.check_active().is_ok());

    second.cancel();
    assert!(controlled.is_cancelled());
    assert_eq!(
        controlled
            .check_active()
            .expect_err("live cancellation")
            .category(),
        ErrorCategory::Cancelled
    );
}

#[test]
fn dispatch_checks_separate_cancellation_from_deadline() {
    let snapshot = ToolContext::new(1024, Duration::from_secs(60), true, scope())
        .expect("valid context builds")
        .with_control(
            CancellationToken::new(),
            Instant::now() + Duration::from_secs(60),
        );
    assert!(
        snapshot.is_cancelled(),
        "dispatch-time snapshot is honoured"
    );
    assert_eq!(
        snapshot
            .check_not_cancelled()
            .expect_err("snapshot cancels")
            .category(),
        ErrorCategory::Cancelled
    );

    let past = Instant::now()
        .checked_sub(Duration::from_millis(1))
        .expect("test clock has history");
    let expired = context(1024).with_control(CancellationToken::new(), past);
    assert!(!expired.is_cancelled());
    assert!(
        expired.check_not_cancelled().is_ok(),
        "the cancellation-only check ignores the deadline"
    );
    assert_eq!(
        expired
            .check_active()
            .expect_err("deadline passed")
            .category(),
        ErrorCategory::Timeout
    );

    let cancelled_token = CancellationToken::new();
    cancelled_token.cancel();
    let both = context(1024).with_control(cancelled_token, past);
    assert_eq!(
        both.check_active()
            .expect_err("cancellation wins over the deadline")
            .category(),
        ErrorCategory::Cancelled,
        "the combined check reports cancellation before timeout"
    );
}
