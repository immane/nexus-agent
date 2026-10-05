//! Public-boundary hardening for gated and delayed [`FakeTool`] workers.
//!
//! The in-crate unit tests exercise the same coordination from inside the
//! module; these tests pin the documented contract through the public API
//! only: an execution entry is recorded before any gate or delay wait, a
//! released gate unblocks its worker and stays released, live cancellation
//! that arrives while a worker is blocked is re-read after release, and an
//! uncancelled scripted delay still reports success.
//!
//! Determinism: no unbounded waits. Gate entry is observed through
//! [`FakeGate::wait_entered`] with a bounded timeout, the fake's own gate wait
//! is bounded by [`FakeGate::RELEASE_TIMEOUT`], delayed workers are observed
//! by polling the public execution log with a bounded deadline and then
//! joined, and the only time comparison is that a completed scripted sleep
//! cannot be shorter than its scripted duration.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use nexus_core::{
    ApprovedScope, CallId, CancellationToken, EffectState, Evidence, ExecutionStatus, M0_REVISION,
    NormalizedArgs, RunId, ToolCall, ToolContext, ToolId, ToolPort, TurnId,
};
use nexus_fakes::{FakeGate, FakeTool};

/// Upper bound for every cross-thread observation in this file.
const BOUNDED_WAIT: Duration = Duration::from_secs(5);

fn scope() -> ApprovedScope {
    ApprovedScope::new("project-read").expect("valid scope builds")
}

fn tool_call(call_id: &str, tool_name: &str, args: &str) -> ToolCall {
    ToolCall::new(
        RunId::new("run-1").expect("valid run id builds"),
        TurnId::new("turn-1").expect("valid turn id builds"),
        CallId::new(call_id).expect("valid call id builds"),
        ToolId::new(tool_name, M0_REVISION).expect("valid tool id builds"),
        NormalizedArgs::new(args).expect("valid args build"),
    )
}

fn tool_context(budget: usize, scope_label: &str) -> ToolContext {
    ToolContext::new(
        budget,
        Duration::from_secs(60),
        false,
        ApprovedScope::new(scope_label).expect("valid scope builds"),
    )
    .expect("valid context builds")
}

fn future_deadline() -> Instant {
    Instant::now()
        .checked_add(Duration::from_secs(60))
        .expect("test clock supports bounded deadlines")
}

fn gate_of(tool: &FakeTool) -> FakeGate {
    tool.gate().expect("gated tool exposes a gate handle")
}

/// Polls the public execution log with a bounded deadline.
fn wait_for_logged_entry(tool: &FakeTool, expected: usize) {
    let deadline = Instant::now() + BOUNDED_WAIT;
    while tool.execution_count() < expected && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        tool.execution_count() >= expected,
        "expected {expected} recorded execution(s) within the bounded wait"
    );
}

#[test]
fn gated_entry_is_recorded_before_the_worker_waits_for_release() {
    let tool = FakeTool::gated("host_read");
    let gate = gate_of(&tool);
    assert!(!gate.is_entered(), "no worker has reached the gate yet");
    assert!(!gate.is_released());
    assert_eq!(tool.execution_count(), 0);

    let call = tool_call("call-gate-entry", "host_read", r#"{"path":"src"}"#);
    let context = tool_context(1024, "project-read");
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| tool.execute(&call, &context));
        assert!(
            gate.wait_entered(BOUNDED_WAIT),
            "the worker must reach the gate within the bounded wait"
        );

        // Recording happens before the gate wait, so the entry is fully
        // inspectable while the worker is still blocked.
        let log = tool.log();
        assert_eq!(log.len(), 1, "entry is logged before the gate wait");
        assert_eq!(log[0].call.as_str(), "call-gate-entry");
        assert_eq!(log[0].args, r#"{"path":"src"}"#);
        assert_eq!(log[0].scope.as_str(), "project-read");
        assert!(gate.is_entered(), "the gate observed the blocked worker");
        assert!(!gate.is_released(), "the worker is still blocked");
        assert_eq!(tool.execution_count(), 1);

        gate.release();
        let outcome = worker.join().expect("gated worker joins after release");
        assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
        assert_eq!(outcome.effect(), EffectState::KnownNotApplied);
        assert_eq!(outcome.evidence(), Evidence::HostObserved);
        assert_eq!(outcome.content(), "gated ok");
    });
    assert!(gate.is_released());
    assert_eq!(tool.execution_count(), 1);
}

#[test]
fn release_before_entry_is_safe_and_stays_released() {
    let tool = FakeTool::gated("host_read");
    let gate = gate_of(&tool);
    assert!(!gate.is_entered());
    gate.release();
    gate.release();
    assert!(gate.is_released(), "release is idempotent");

    let call = tool_call("call-pre-release", "host_read", r#"{"path":"src"}"#);
    let context = tool_context(1024, "project-read");
    // A pre-released gate must never block: executing inline proves the
    // worker does not wait for a release that already happened.
    let outcome = tool.execute(&call, &context);
    assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    assert_eq!(outcome.effect(), EffectState::KnownNotApplied);
    assert!(gate.is_entered(), "the worker still announced entry");
    assert!(gate.is_released(), "release stays released");
    assert_eq!(tool.execution_count(), 1);

    let second = tool_call("call-pre-release-2", "host_read", r#"{"path":"other"}"#);
    let second_outcome = tool.execute(&second, &context);
    assert_eq!(second_outcome.status(), ExecutionStatus::Succeeded);
    assert_eq!(tool.execution_count(), 2, "the gate stays open");
}

#[test]
fn gated_live_cancellation_is_observed_after_release() {
    let tool = FakeTool::gated("host_read");
    let gate = gate_of(&tool);
    let token = CancellationToken::new();
    let context = tool_context(1024, "project-read").with_control(token.clone(), future_deadline());
    let call = tool_call("call-live-cancel", "host_read", r#"{"path":"src"}"#);
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| tool.execute(&call, &context));
        assert!(
            gate.wait_entered(BOUNDED_WAIT),
            "the worker owns the invocation before cancellation"
        );
        token.cancel();
        assert!(
            token.is_cancelled(),
            "cancellation was requested while blocked"
        );
        gate.release();
        let outcome = worker.join().expect("gated worker joins after release");
        assert_eq!(outcome.status(), ExecutionStatus::Cancelled);
        assert_eq!(
            outcome.effect(),
            EffectState::Unknown,
            "cancellation never rewrites effects as rolled back"
        );
        assert_eq!(outcome.evidence(), Evidence::Uncertain);
        assert_eq!(outcome.content(), "fake execution cancelled");
    });
    let log = tool.log();
    assert_eq!(log.len(), 1, "the blocked attempt stays recorded");
    assert_eq!(log[0].call.as_str(), "call-live-cancel");
    assert_eq!(log[0].args, r#"{"path":"src"}"#);
    assert_eq!(log[0].scope.as_str(), "project-read");
}

#[test]
fn gated_mutation_cancellation_after_release_keeps_effects_unknown() {
    let tool = FakeTool::gated_mutation("host_write");
    let gate = gate_of(&tool);
    let token = CancellationToken::new();
    let context =
        tool_context(1024, "project-write").with_control(token.clone(), future_deadline());
    let call = tool_call("call-gated-write-cancel", "host_write", r#"{"path":"dst"}"#);
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| tool.execute(&call, &context));
        assert!(
            gate.wait_entered(BOUNDED_WAIT),
            "the mutation worker reached the gate"
        );
        token.cancel();
        gate.release();
        let outcome = worker.join().expect("gated mutation worker joins");
        assert_eq!(outcome.status(), ExecutionStatus::Cancelled);
        assert_eq!(outcome.effect(), EffectState::Unknown);
        assert_ne!(
            outcome.effect(),
            EffectState::KnownNotApplied,
            "a cancelled mutation is inconclusive, never reported as not applied"
        );
        assert_eq!(outcome.evidence(), Evidence::Uncertain);
    });
    assert_eq!(tool.execution_count(), 1);
}

#[test]
fn gated_mutation_release_reports_known_applied() {
    let tool = FakeTool::gated_mutation("host_write");
    let gate = gate_of(&tool);
    let call = tool_call("call-gated-write-ok", "host_write", r#"{"path":"dst"}"#);
    let context = tool_context(1024, "project-write");
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| tool.execute(&call, &context));
        assert!(
            gate.wait_entered(BOUNDED_WAIT),
            "the mutation worker reached the gate"
        );
        gate.release();
        let outcome = worker.join().expect("gated mutation worker joins");
        assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
        assert_eq!(outcome.effect(), EffectState::KnownApplied);
        assert_eq!(outcome.evidence(), Evidence::HostObserved);
        assert_eq!(outcome.content(), "gated write ok");
    });
    assert_eq!(tool.execution_count(), 1);
}

#[test]
fn pre_cancelled_gated_context_never_enters_the_gate() {
    let tool = FakeTool::gated("host_read");
    let gate = gate_of(&tool);
    let call = tool_call("call-pre-cancelled", "host_read", r#"{"path":"src"}"#);
    let snapshot = ToolContext::new(1024, Duration::from_secs(60), true, scope())
        .expect("valid cancelled snapshot builds");
    let outcome = tool.execute(&call, &snapshot);
    assert_eq!(outcome.status(), ExecutionStatus::Cancelled);
    assert_eq!(outcome.effect(), EffectState::Unknown);
    assert_eq!(outcome.evidence(), Evidence::Uncertain);
    assert!(
        !gate.is_entered(),
        "a dispatch-time cancellation snapshot skips the gate entirely"
    );
    assert!(!gate.is_released());
    assert_eq!(tool.execution_count(), 1, "the skipped attempt is recorded");

    let token = CancellationToken::new();
    token.cancel();
    let live = tool_context(1024, "project-read").with_control(token.clone(), future_deadline());
    let second = tool_call(
        "call-live-pre-cancelled",
        "host_read",
        r#"{"path":"other"}"#,
    );
    let outcome = tool.execute(&second, &live);
    assert_eq!(outcome.status(), ExecutionStatus::Cancelled);
    assert!(token.is_cancelled());
    assert!(
        !gate.is_entered(),
        "a live token cancelled before dispatch also skips the gate"
    );
    assert_eq!(tool.execution_count(), 2);
}

#[test]
fn uncancelled_delay_still_succeeds() {
    let delay = Duration::from_millis(25);
    let tool = FakeTool::delayed("host_write", delay);
    let call = tool_call("call-delay-ok", "host_write", r#"{"path":"dst"}"#);
    let context = tool_context(1024, "project-write");
    let started = Instant::now();
    let outcome = tool.execute(&call, &context);
    assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    assert_eq!(outcome.effect(), EffectState::KnownApplied);
    assert_eq!(outcome.evidence(), Evidence::HostObserved);
    assert_eq!(outcome.content(), "delayed ok");
    assert!(!outcome.is_truncated());
    assert!(
        started.elapsed() >= delay,
        "a scripted sleep never returns before its duration"
    );
    let log = tool.log();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].call.as_str(), "call-delay-ok");
    assert_eq!(log[0].args, r#"{"path":"dst"}"#);
    assert_eq!(log[0].scope.as_str(), "project-write");
}

#[test]
fn delayed_entry_is_observable_while_the_worker_sleeps() {
    let delay = Duration::from_millis(500);
    let tool = FakeTool::delayed("host_read", delay);
    let call = tool_call("call-delay-entry", "host_read", r#"{"path":"src"}"#);
    let context = tool_context(1024, "project-read");
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| tool.execute(&call, &context));
        wait_for_logged_entry(&tool, 1);
        assert!(
            !worker.is_finished(),
            "the delayed worker is still in flight when its entry is observed"
        );
        let log = tool.log();
        assert_eq!(log.len(), 1, "entry is logged before the sleep");
        assert_eq!(log[0].call.as_str(), "call-delay-entry");
        assert_eq!(log[0].args, r#"{"path":"src"}"#);
        assert_eq!(log[0].scope.as_str(), "project-read");
        let outcome = worker.join().expect("delayed worker joins");
        assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
        assert_eq!(outcome.effect(), EffectState::KnownApplied);
    });
    assert_eq!(tool.execution_count(), 1);
}

#[test]
fn delayed_live_cancellation_after_entry_is_observed() {
    let delay = Duration::from_millis(500);
    let tool = FakeTool::delayed("host_write", delay);
    let token = CancellationToken::new();
    let context =
        tool_context(1024, "project-write").with_control(token.clone(), future_deadline());
    let call = tool_call("call-delay-cancel", "host_write", r#"{"path":"dst"}"#);
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| tool.execute(&call, &context));
        wait_for_logged_entry(&tool, 1);
        assert!(
            !worker.is_finished(),
            "the worker is still in its scripted sleep"
        );
        token.cancel();
        let outcome = worker.join().expect("delayed worker joins");
        assert_eq!(outcome.status(), ExecutionStatus::Cancelled);
        assert_eq!(outcome.effect(), EffectState::Unknown);
        assert_eq!(outcome.evidence(), Evidence::Uncertain);
        assert_eq!(outcome.content(), "fake execution cancelled");
    });
    let log = tool.log();
    assert_eq!(log.len(), 1, "the attempt is recorded before the sleep");
    assert_eq!(log[0].call.as_str(), "call-delay-cancel");
    assert_eq!(log[0].args, r#"{"path":"dst"}"#);
    assert_eq!(log[0].scope.as_str(), "project-write");
}

#[test]
fn zero_delay_delayed_tool_still_succeeds() {
    let tool = FakeTool::delayed("host_read", Duration::ZERO);
    let call = tool_call("call-zero-delay", "host_read", r#"{"path":"src"}"#);
    let outcome = tool.execute(&call, &tool_context(1024, "project-read"));
    assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    assert_eq!(outcome.effect(), EffectState::KnownApplied);
    assert_eq!(outcome.content(), "delayed ok");
    assert_eq!(tool.execution_count(), 1);
}

#[test]
fn wait_entered_times_out_without_a_worker() {
    let tool = FakeTool::gated("host_read");
    let gate = gate_of(&tool);
    assert!(
        !gate.wait_entered(Duration::from_millis(10)),
        "no worker ever arrives, so the bounded wait reports false"
    );
    assert!(!gate.is_entered());
    assert!(!gate.is_released());
    assert_eq!(tool.execution_count(), 0);
}

#[test]
fn only_gated_constructors_expose_a_gate() {
    assert!(FakeTool::read_only().gate().is_none());
    assert!(FakeTool::mutation().gate().is_none());
    assert!(FakeTool::command().gate().is_none());
    assert!(FakeTool::failing("host_write").gate().is_none());
    assert!(
        FakeTool::delayed("host_read", Duration::from_millis(1))
            .gate()
            .is_none()
    );
    assert!(FakeTool::oversized("host_read", 8).gate().is_none());
    assert!(FakeTool::gated("host_read").gate().is_some());
    assert!(FakeTool::gated_mutation("host_write").gate().is_some());
}

#[test]
fn gate_handles_share_one_release_state() {
    let tool = FakeTool::gated("host_read");
    let first = gate_of(&tool);
    let second = gate_of(&tool);
    assert!(!first.is_entered() && !second.is_entered());
    assert!(!first.is_released() && !second.is_released());
    first.release();
    assert!(second.is_released(), "clones observe the same gate state");
    assert!(first.is_released());
}
