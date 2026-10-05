#![forbid(unsafe_code)]

//! Coverage hardening for the [`FakeTool`] double through its public surface.
//!
//! Pins the scripted-tool contract from outside the crate: read, mutation,
//! and command executions record exact argument bytes and the received
//! approved scope in call order; oversized output is either a budget-cut
//! prefix explicitly flagged incomplete or a complete, globally valid JSON
//! string document; scripted failure reports unknown effects with uncertain
//! evidence; and cancellation reports unknown effects, never a rollback or a
//! rewritten success. Deterministic: fixed inputs, no randomness, no sleeps;
//! the one worker-coordination test uses [`FakeGate`](nexus_fakes::FakeGate)
//! entry/release with scoped threads.

use std::time::{Duration, Instant};

use nexus_core::{
    ApprovedScope, CallId, CancellationToken, EffectState, Evidence, ExecutionStatus, Limits,
    M0_REVISION, NormalizedArgs, RunId, ToolCall, ToolContext, ToolId, ToolPort, TurnId,
};
use nexus_fakes::{FakeTool, FakeToolCallRecord};

/// Globally valid JSON string document of exactly `len` bytes: a quoted run
/// of ASCII `x` (`len >= 2`).
fn json_document(len: usize) -> String {
    assert!(len >= 2, "a JSON string document needs both quotes");
    format!("\"{}\"", "x".repeat(len - 2))
}

/// First `len` bytes of [`json_document`]: an opening quote and `len - 1`
/// payload bytes, without the closing quote (`len >= 1`).
fn json_prefix(len: usize) -> String {
    assert!(len >= 1, "a prefix carries at least the opening quote");
    format!("\"{}", "x".repeat(len - 1))
}

/// Structural validity check for the fake's JSON string fixture without a
/// JSON parser dependency: both quotes present, only `x` payload bytes.
fn is_fake_json_string_document(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 2
        && bytes[0] == b'"'
        && bytes[bytes.len() - 1] == b'"'
        && bytes[1..bytes.len() - 1].iter().all(|byte| *byte == b'x')
}

fn tool_call(call_id: &str, tool_name: &str, args: &str) -> ToolCall {
    ToolCall::new(
        RunId::new("run-cov").expect("valid run id"),
        TurnId::new("turn-cov").expect("valid turn id"),
        CallId::new(call_id).expect("valid call id"),
        ToolId::new(tool_name, M0_REVISION).expect("valid tool id"),
        NormalizedArgs::new(args).expect("valid normalized args"),
    )
}

fn tool_context(budget: usize, cancelled: bool, scope: &str) -> ToolContext {
    ToolContext::new(
        budget,
        Duration::from_secs(60),
        cancelled,
        ApprovedScope::new(scope).expect("valid scope builds"),
    )
    .expect("valid context builds")
}

/// Returns the single recorded attempt, asserting exactly one was observed.
fn only_record(tool: &FakeTool) -> FakeToolCallRecord {
    let mut log = tool.log();
    assert_eq!(log.len(), 1, "exactly one attempt is recorded");
    log.pop().expect("one record was asserted")
}

#[test]
fn read_records_exact_args_and_scope_and_reports_not_applied() {
    let tool = FakeTool::read_only();
    let spec = tool.describe();
    assert_eq!(spec.id().name(), "host_read");
    assert_eq!(spec.id().revision(), M0_REVISION);
    assert_eq!(spec.description(), "fake read");
    assert_eq!(spec.input_schema_json(), r#"{"type":"object"}"#);

    // Raw bytes survive verbatim, including surrounding whitespace and
    // non-ASCII payload bytes: recording never trims or canonicalizes.
    let args = "{ \"path\": \"src/ünïcode\", \"n\": 1 }\n";
    let outcome = tool.execute(
        &tool_call("call-read", "host_read", args),
        &tool_context(1024, false, "project-read"),
    );
    assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    assert_eq!(outcome.effect(), EffectState::KnownNotApplied);
    assert_eq!(outcome.evidence(), Evidence::HostObserved);
    assert_eq!(outcome.content(), "read ok");
    assert!(!outcome.is_truncated());

    let entry = only_record(&tool);
    assert_eq!(entry.call.as_str(), "call-read");
    assert_eq!(entry.args, args, "argument text is recorded byte-for-byte");
    assert_eq!(entry.scope.as_str(), "project-read");
    assert_eq!(tool.execution_count(), 1);
}

#[test]
fn mutation_and_command_record_exact_args_and_scope_in_call_order() {
    let mutation = FakeTool::mutation();
    let command = FakeTool::command();
    assert_eq!(mutation.describe().id().name(), "host_write");
    assert_eq!(command.describe().id().name(), "host_exec");

    let first = tool_call("call-write-1", "host_write", r#"{"path":"dst/one"}"#);
    let second = tool_call("call-write-2", "host_write", r#"{"path":"dst/two"}"#);
    let exec = tool_call("call-exec", "host_exec", r#"{"argv":["run","--fast"]}"#);

    let first_outcome = mutation.execute(&first, &tool_context(2048, false, "project-write"));
    let second_outcome = mutation.execute(&second, &tool_context(2048, false, "project-write"));
    let exec_outcome = command.execute(&exec, &tool_context(2048, false, "command-scope"));

    for outcome in [&first_outcome, &second_outcome, &exec_outcome] {
        assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
        assert_eq!(outcome.effect(), EffectState::KnownApplied);
        assert_eq!(outcome.evidence(), Evidence::HostObserved);
        assert!(!outcome.is_truncated());
    }
    assert_eq!(first_outcome.content(), "write ok");
    assert_eq!(exec_outcome.content(), "exec ok");

    let log = mutation.log();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].call.as_str(), "call-write-1");
    assert_eq!(log[0].args, r#"{"path":"dst/one"}"#);
    assert_eq!(log[0].scope.as_str(), "project-write");
    assert_eq!(log[1].call.as_str(), "call-write-2");
    assert_eq!(log[1].args, r#"{"path":"dst/two"}"#);
    assert_eq!(log[1].scope.as_str(), "project-write");

    let command_log = command.log();
    assert_eq!(command_log.len(), 1, "each fake owns an isolated log");
    assert_eq!(command_log[0].call.as_str(), "call-exec");
    assert_eq!(command_log[0].args, r#"{"argv":["run","--fast"]}"#);
    assert_eq!(command_log[0].scope.as_str(), "command-scope");
    assert_eq!(mutation.execution_count(), 2);
    assert_eq!(command.execution_count(), 1);
}

#[test]
fn empty_scope_is_recorded_exactly() {
    let tool = FakeTool::read_only();
    let outcome = tool.execute(
        &tool_call("call-empty-scope", "host_read", r#"{"path":"src"}"#),
        &tool_context(64, false, ""),
    );
    assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    assert_eq!(only_record(&tool).scope.as_str(), "");
}

#[test]
fn log_snapshots_are_independent_of_the_tool() {
    let tool = FakeTool::read_only();
    tool.execute(
        &tool_call("call-snapshot", "host_read", r#"{"path":"src"}"#),
        &tool_context(64, false, "project-read"),
    );
    let mut snapshot = tool.log();
    snapshot.clear();
    assert!(snapshot.is_empty());
    assert_eq!(
        tool.log().len(),
        1,
        "the returned log is an independent copy"
    );
    assert_eq!(tool.execution_count(), 1);
}

#[test]
fn scripted_failure_reports_unknown_effects_and_records_attempt() {
    let tool = FakeTool::failing("host_write");
    let spec = tool.describe();
    assert_eq!(spec.id().name(), "host_write");
    assert_eq!(spec.id().revision(), M0_REVISION);
    assert_eq!(spec.description(), "fake failure");

    let args = r#"{"path":"dst"}"#;
    let outcome = tool.execute(
        &tool_call("call-fail-1", "host_write", args),
        &tool_context(1024, false, "project-write"),
    );
    assert_eq!(outcome.status(), ExecutionStatus::Failed);
    assert_eq!(outcome.effect(), EffectState::Unknown);
    assert_eq!(outcome.evidence(), Evidence::Uncertain);
    assert_eq!(outcome.content(), "fake failure");
    assert!(!outcome.is_truncated());
    assert_ne!(
        outcome.effect(),
        EffectState::KnownNotApplied,
        "failure is no rollback"
    );
    assert_ne!(
        outcome.effect(),
        EffectState::KnownApplied,
        "failure is no success"
    );
    assert_ne!(outcome.evidence(), Evidence::HostObserved);

    let entry = only_record(&tool);
    assert_eq!(entry.call.as_str(), "call-fail-1");
    assert_eq!(entry.args, args);
    assert_eq!(entry.scope.as_str(), "project-write");

    // The script is deterministic: a second call repeats the same report.
    let second = tool.execute(
        &tool_call("call-fail-2", "host_write", args),
        &tool_context(1024, false, "project-write"),
    );
    assert_eq!(second, outcome);
    let log = tool.log();
    assert_eq!(log.len(), 2);
    assert_eq!(log[1].call.as_str(), "call-fail-2");
}

#[test]
fn cancelled_snapshot_reports_unknown_not_rollback_for_every_behavior() {
    let cases = [
        (
            FakeTool::read_only(),
            "host_read",
            r#"{"path":"src"}"#,
            "project-read",
        ),
        (
            FakeTool::mutation(),
            "host_write",
            r#"{"path":"dst"}"#,
            "project-write",
        ),
        (
            FakeTool::command(),
            "host_exec",
            r#"{"argv":["run"]}"#,
            "command-scope",
        ),
        (
            FakeTool::failing("host_write"),
            "host_write",
            r#"{"path":"dst"}"#,
            "project-write",
        ),
        (
            FakeTool::oversized("host_read", 4096),
            "host_read",
            r#"{"path":"src"}"#,
            "project-read",
        ),
    ];
    for (index, (tool, name, args, scope)) in cases.into_iter().enumerate() {
        let call_id = format!("call-cancel-{index}");
        let outcome = tool.execute(
            &tool_call(&call_id, name, args),
            &tool_context(256, true, scope),
        );
        assert_eq!(outcome.status(), ExecutionStatus::Cancelled, "{name}");
        assert_eq!(outcome.effect(), EffectState::Unknown, "{name}");
        assert_eq!(outcome.evidence(), Evidence::Uncertain, "{name}");
        assert_eq!(outcome.content(), "fake execution cancelled");
        assert!(!outcome.is_truncated(), "cancellation is not truncation");
        assert_ne!(
            outcome.effect(),
            EffectState::KnownNotApplied,
            "cancellation never rolls back"
        );
        assert_ne!(
            outcome.effect(),
            EffectState::KnownApplied,
            "cancellation never rewrites a success"
        );

        let entry = only_record(&tool);
        assert_eq!(entry.call.as_str(), call_id);
        assert_eq!(entry.args, args);
        assert_eq!(entry.scope.as_str(), scope);
    }
}

#[test]
fn pre_cancelled_context_never_enters_the_gate() {
    let tool = FakeTool::gated_mutation("host_write");
    let gate = tool.gate().expect("gated tool exposes its gate");
    let outcome = tool.execute(
        &tool_call("call-pre-cancel", "host_write", r#"{"path":"dst"}"#),
        &tool_context(1024, true, "project-write"),
    );
    assert_eq!(outcome.status(), ExecutionStatus::Cancelled);
    assert_eq!(outcome.effect(), EffectState::Unknown);
    assert!(
        !gate.is_entered(),
        "cancellation short-circuits before the gate wait"
    );
    assert!(!gate.is_released());
    assert_eq!(tool.execution_count(), 1, "the attempt is still recorded");
}

#[test]
fn live_cancellation_while_gated_reports_unknown_not_rollback() {
    let tool = FakeTool::gated_mutation("host_write");
    let gate = tool.gate().expect("gated tool exposes its gate");
    let token = CancellationToken::new();
    let context = tool_context(1024, false, "project-write")
        .with_control(token.clone(), Instant::now() + Duration::from_secs(60));
    let call = tool_call("call-live-cancel", "host_write", r#"{"path":"dst"}"#);
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| tool.execute(&call, &context));
        assert!(
            gate.wait_entered(Duration::from_secs(5)),
            "the mutation worker reaches the gate"
        );
        let entry = only_record(&tool);
        assert_eq!(entry.call.as_str(), "call-live-cancel");
        assert_eq!(entry.args, r#"{"path":"dst"}"#);
        assert_eq!(entry.scope.as_str(), "project-write");
        assert!(!gate.is_released());

        token.cancel();
        gate.release();
        let outcome = worker.join().expect("gated worker joins");
        assert_eq!(outcome.status(), ExecutionStatus::Cancelled);
        assert_eq!(
            outcome.effect(),
            EffectState::Unknown,
            "a raced mutation reports unknown, never a rollback"
        );
        assert_ne!(
            outcome.effect(),
            EffectState::KnownApplied,
            "cancellation rewrites no success"
        );
        assert_eq!(outcome.evidence(), Evidence::Uncertain);
        assert_eq!(outcome.content(), "fake execution cancelled");
        assert!(!outcome.is_truncated());
    });
    assert!(gate.is_released());
    assert_eq!(tool.execution_count(), 1, "exactly one attempt is recorded");
}

#[test]
fn oversized_output_is_truncated_with_flag_and_never_the_full_document() {
    let budget = 64;
    let declared = 4096;
    assert!(budget <= Limits::M0_TEST_TOOL_OUTPUT_BYTES);
    let tool = FakeTool::oversized("host_read", declared);
    let context = tool_context(budget, false, "project-read");
    assert_eq!(context.output_budget_bytes(), budget);
    let outcome = tool.execute(
        &tool_call("call-over", "host_read", r#"{"path":"src"}"#),
        &context,
    );
    assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    assert_eq!(outcome.effect(), EffectState::KnownApplied);
    assert_eq!(outcome.evidence(), Evidence::HostObserved);
    assert!(
        outcome.is_truncated(),
        "an over-budget fixture is flagged incomplete"
    );
    assert_eq!(outcome.content().len(), budget);
    assert_eq!(outcome.content(), json_prefix(budget));
    assert_eq!(outcome.content(), &json_document(declared)[..budget]);
    assert!(outcome.content().starts_with('"'));
    assert!(
        !outcome.content().ends_with('"'),
        "the cut prefix has no closing quote"
    );
    assert!(
        !is_fake_json_string_document(outcome.content()),
        "the emitted prefix is not a complete JSON document"
    );
    assert_eq!(tool.execution_count(), 1);
}

#[test]
fn oversized_output_at_exact_budget_is_complete_valid_json() {
    let budget = 128;
    let tool = FakeTool::oversized("host_read", budget);
    let outcome = tool.execute(
        &tool_call("call-over-exact", "host_read", r#"{"path":"src"}"#),
        &tool_context(budget, false, "project-read"),
    );
    assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    assert!(!outcome.is_truncated(), "content fits the effective budget");
    assert_eq!(outcome.content(), json_document(budget));
    assert_eq!(outcome.content().len(), budget);
    assert!(is_fake_json_string_document(outcome.content()));
}

#[test]
fn oversized_output_below_budget_is_complete_valid_json() {
    let declared = 24;
    let tool = FakeTool::oversized("host_read", declared);
    let outcome = tool.execute(
        &tool_call("call-over-below", "host_read", r#"{"path":"src"}"#),
        &tool_context(1024, false, "project-read"),
    );
    assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    assert!(!outcome.is_truncated());
    assert_eq!(outcome.content(), json_document(declared));
    assert_eq!(outcome.content().len(), declared);
    assert!(is_fake_json_string_document(outcome.content()));
}

#[test]
fn oversized_one_byte_over_budget_is_truncated() {
    let budget = 64;
    let declared = budget + 1;
    let tool = FakeTool::oversized("host_read", declared);
    let outcome = tool.execute(
        &tool_call("call-over-one", "host_read", r#"{"path":"src"}"#),
        &tool_context(budget, false, "project-read"),
    );
    assert!(outcome.is_truncated(), "one byte over the budget is cut");
    assert_eq!(outcome.content().len(), budget);
    assert_eq!(outcome.content(), &json_document(declared)[..budget]);
}

#[test]
fn oversized_declared_below_minimum_clamps_to_two_byte_document() {
    for declared in [0usize, 1] {
        let tool = FakeTool::oversized("host_read", declared);
        let outcome = tool.execute(
            &tool_call("call-over-min", "host_read", r#"{"path":"src"}"#),
            &tool_context(16, false, "project-read"),
        );
        assert!(!outcome.is_truncated(), "clamped fixture fits the budget");
        assert_eq!(outcome.content(), "\"\"");
        assert_eq!(outcome.content().len(), 2);
        assert!(is_fake_json_string_document(outcome.content()));
    }
}

#[test]
fn oversized_huge_declared_length_materializes_only_the_budget_prefix() {
    // A declared length near the pointer-width ceiling would abort the test
    // process if the fake allocated it before cutting; only the budget-sized
    // prefix may be allocated.
    let budget = 32;
    let tool = FakeTool::oversized("host_read", usize::MAX);
    let outcome = tool.execute(
        &tool_call("call-over-huge", "host_read", r#"{"path":"src"}"#),
        &tool_context(budget, false, "project-read"),
    );
    assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    assert!(outcome.is_truncated());
    assert_eq!(outcome.content().len(), budget);
    assert_eq!(outcome.content(), json_prefix(budget));
    assert!(!is_fake_json_string_document(outcome.content()));
}

#[test]
fn oversized_with_single_byte_budget_emits_only_the_opening_quote() {
    let tool = FakeTool::oversized("host_read", 2);
    let context = tool_context(1, false, "project-read");
    assert_eq!(context.output_budget_bytes(), 1);
    let outcome = tool.execute(
        &tool_call("call-over-tiny", "host_read", r#"{"path":"src"}"#),
        &context,
    );
    assert!(outcome.is_truncated());
    assert_eq!(outcome.content(), "\"");
    assert_eq!(outcome.content().len(), context.output_budget_bytes());
    assert!(!is_fake_json_string_document(outcome.content()));
}
