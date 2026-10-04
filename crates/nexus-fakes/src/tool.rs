//! Deterministic scripted tool double.
//!
//! A [`FakeTool`] only runs when the runtime calls [`execute`](ToolPort::execute):
//! approval stays the runtime's job. Every execution records its host
//! [`CallId`], argument text, and received scope to an
//! inspectable log, and that entry is recorded **before** any delay or gate
//! wait so observers can see which worker owns an in-flight execution.
//! Interruption (a cancelled context, including live `with_control`
//! cancellation or a scripted delay) reports `Cancelled` with `Unknown`
//! effects and `Uncertain` evidence via the core outcome types, never a
//! rewritten success. [`FakeTool::gated`] adds entered/release coordination
//! for deterministic cancellation worker ownership tests, and
//! [`FakeTool::oversized`] emits a budget-truncated prefix of a globally
//! valid JSON fixture without materializing the declared full length.

use std::sync::Mutex;
use std::time::Duration;

use crate::provider::FakeGate;
use nexus_core::{
    ApprovedScope, CallId, EffectState, Evidence, ExecutionStatus, M0_REVISION, ToolCall,
    ToolContext, ToolId, ToolOutcome, ToolPort, ToolSpec,
};

/// Minimum length of the JSON-string fixture built by [`FakeTool::oversized`].
const MIN_JSON_DOCUMENT_BYTES: usize = 2;

/// One observed execution: host identity, exact arguments, received scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeToolCallRecord {
    /// Host-issued call identity assigned by the runtime.
    pub call: CallId,
    /// Exact argument text the executor received.
    pub args: String,
    /// Approved scope the runtime attached at dispatch.
    pub scope: ApprovedScope,
}

#[derive(Debug, Clone)]
enum FakeToolBehavior {
    Succeed { content: String, applied: bool },
    Fail { message: String },
    Oversized { content_len: usize },
}

/// Scripted tool double. The log records every execution in call order.
pub struct FakeTool {
    spec: ToolSpec,
    log: Mutex<Vec<FakeToolCallRecord>>,
    behavior: FakeToolBehavior,
    delay: Duration,
    gate: Option<FakeGate>,
}

impl FakeTool {
    /// Read-like tool: succeeds with bounded output, known not applied.
    pub fn read_only() -> Self {
        Self::succeeding("host_read", "fake read", "read ok", false)
    }

    /// Mutation-like tool: succeeds, known applied.
    pub fn mutation() -> Self {
        Self::succeeding("host_write", "fake mutation", "write ok", true)
    }

    /// Command-like tool: succeeds, known applied.
    pub fn command() -> Self {
        Self::succeeding("host_exec", "fake command", "exec ok", true)
    }

    /// Tool that reports failure with honestly unknown effects.
    pub fn failing(tool_name: &str) -> Self {
        Self {
            spec: spec_for(tool_name, "fake failure"),
            log: Mutex::new(Vec::new()),
            behavior: FakeToolBehavior::Fail {
                message: "fake failure".to_owned(),
            },
            delay: Duration::ZERO,
            gate: None,
        }
    }

    /// Tool whose declared output exceeds the effective budget, exercising
    /// truncation: the fixture is a globally valid JSON string document of
    /// `content_len` bytes (at least two), but only the budget-sized prefix
    /// of it is ever materialized, and the truncation flag marks that
    /// emitted prefix as incomplete and therefore noncompliant.
    pub fn oversized(tool_name: &str, content_len: usize) -> Self {
        Self {
            spec: spec_for(tool_name, "fake oversized output"),
            log: Mutex::new(Vec::new()),
            behavior: FakeToolBehavior::Oversized { content_len },
            delay: Duration::ZERO,
            gate: None,
        }
    }

    /// Succeeding tool that sleeps `delay` before answering, for timeout and
    /// mid-delay cancellation tests. The entry is logged before the sleep.
    pub fn delayed(tool_name: &str, delay: Duration) -> Self {
        let mut tool = Self::succeeding(tool_name, "fake delayed", "delayed ok", true);
        tool.delay = delay;
        tool
    }

    /// Read-like tool that records its entry and then blocks at its
    /// entered/release gate, for deterministic cancellation worker ownership
    /// tests. The test waits for entry, performs its action, and releases the
    /// blocked worker.
    pub fn gated(tool_name: &str) -> Self {
        let mut tool = Self::succeeding(tool_name, "fake gated", "gated ok", false);
        tool.gate = Some(FakeGate::new());
        tool
    }

    /// Mutation-like counterpart of [`FakeTool::gated`]: succeeds with
    /// [`EffectState::KnownApplied`], so a cancellation that races the
    /// blocked worker must preserve the applied effect rather than rewrite
    /// it as not applied.
    pub fn gated_mutation(tool_name: &str) -> Self {
        let mut tool = Self::succeeding(tool_name, "fake gated mutation", "gated write ok", true);
        tool.gate = Some(FakeGate::new());
        tool
    }

    /// Returns the gate handle when this tool is gated.
    #[must_use]
    pub fn gate(&self) -> Option<FakeGate> {
        self.gate.clone()
    }

    /// Returns the observed executions in call order.
    pub fn log(&self) -> Vec<FakeToolCallRecord> {
        self.log.lock().expect("fake tool log readable").clone()
    }

    /// Returns the number of observed executions.
    pub fn execution_count(&self) -> usize {
        self.log.lock().expect("fake tool log readable").len()
    }

    fn succeeding(tool_name: &str, description: &str, content: &str, applied: bool) -> Self {
        Self {
            spec: spec_for(tool_name, description),
            log: Mutex::new(Vec::new()),
            behavior: FakeToolBehavior::Succeed {
                content: content.to_owned(),
                applied,
            },
            delay: Duration::ZERO,
            gate: None,
        }
    }

    fn record(&self, call: &ToolCall, context: &ToolContext) {
        self.log
            .lock()
            .expect("fake tool log readable")
            .push(FakeToolCallRecord {
                call: call.call().clone(),
                args: call.args().as_str().to_owned(),
                scope: context.scope().clone(),
            });
    }
}

impl ToolPort for FakeTool {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn execute(&self, call: &ToolCall, context: &ToolContext) -> ToolOutcome {
        // Entry is recorded before any delay or gate wait, so a concurrent
        // observer sees that this worker owns the in-flight execution.
        self.record(call, context);
        if context.is_cancelled() {
            return cancelled_outcome();
        }
        if let Some(gate) = &self.gate
            && !gate.enter_and_wait()
        {
            return gate_timeout_outcome();
        }
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        // Recheck mutable state after any wait: a `with_control` token is
        // live, so cancellation that arrived while this worker was blocked
        // surfaces through `is_cancelled` here without changing the fake.
        if context.is_cancelled() {
            return cancelled_outcome();
        }
        let outcome = match &self.behavior {
            FakeToolBehavior::Succeed { content, applied } => ToolOutcome::new(
                ExecutionStatus::Succeeded,
                if *applied {
                    EffectState::KnownApplied
                } else {
                    EffectState::KnownNotApplied
                },
                Evidence::HostObserved,
                content.clone(),
                false,
            ),
            FakeToolBehavior::Fail { message } => ToolOutcome::new(
                ExecutionStatus::Failed,
                EffectState::Unknown,
                Evidence::Uncertain,
                message.clone(),
                false,
            ),
            FakeToolBehavior::Oversized { content_len } => {
                let budget = context.output_budget_bytes();
                let declared = (*content_len).max(MIN_JSON_DOCUMENT_BYTES);
                if declared > budget {
                    // The declared fixture is globally valid JSON, but only
                    // the budget-sized prefix is materialized; the flag
                    // marks the emitted prefix as incomplete, never a
                    // silently accepted document.
                    ToolOutcome::new(
                        ExecutionStatus::Succeeded,
                        EffectState::KnownApplied,
                        Evidence::HostObserved,
                        json_document_prefix(budget),
                        true,
                    )
                } else {
                    ToolOutcome::new(
                        ExecutionStatus::Succeeded,
                        EffectState::KnownApplied,
                        Evidence::HostObserved,
                        json_document(declared),
                        false,
                    )
                }
            }
        };
        outcome.expect("fake outcome builds")
    }
}

/// Builds a globally valid JSON string document of exactly `len` bytes
/// (`len >= 2`): a quoted run of ASCII `x`.
fn json_document(len: usize) -> String {
    debug_assert!(len >= MIN_JSON_DOCUMENT_BYTES);
    let mut document = String::with_capacity(len);
    document.push('"');
    document.extend(std::iter::repeat_n('x', len - 2));
    document.push('"');
    document
}

/// Builds only the first `len` bytes of a [`json_document`]: an opening
/// quote and `len - 1` payload bytes, without the closing quote. Only the
/// emitted prefix is allocated; the declared full length is never
/// materialized just to be cut.
fn json_document_prefix(len: usize) -> String {
    debug_assert!(len >= 1);
    let mut prefix = String::with_capacity(len);
    prefix.push('"');
    prefix.extend(std::iter::repeat_n('x', len - 1));
    prefix
}

fn spec_for(tool_name: &str, description: &str) -> ToolSpec {
    ToolSpec::new(
        ToolId::new(tool_name, M0_REVISION).expect("fake tool name builds"),
        description,
        r#"{"type":"object"}"#,
    )
    .expect("fake spec builds")
}

fn cancelled_outcome() -> ToolOutcome {
    ToolOutcome::new(
        ExecutionStatus::Cancelled,
        EffectState::Unknown,
        Evidence::Uncertain,
        "fake execution cancelled",
        false,
    )
    .expect("static safe fake outcome builds")
}

fn gate_timeout_outcome() -> ToolOutcome {
    ToolOutcome::new(
        ExecutionStatus::Failed,
        EffectState::Unknown,
        Evidence::Uncertain,
        "fake gate was never released",
        false,
    )
    .expect("static safe fake outcome builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::{CancellationToken, Limits, NormalizedArgs, RunId, TurnId};
    use std::time::{Duration, Instant};

    fn tool_call(call_id: &str, tool_name: &str, args: &str) -> ToolCall {
        ToolCall::new(
            RunId::new("run-1").expect("valid"),
            TurnId::new("turn-1").expect("valid"),
            CallId::new(call_id).expect("valid"),
            ToolId::new(tool_name, M0_REVISION).expect("valid"),
            NormalizedArgs::new(args).expect("valid args build"),
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

    #[test]
    fn read_tool_succeeds_and_records_scope() {
        let tool = FakeTool::read_only();
        let call = tool_call("call-1", "host_read", r#"{"path":"src"}"#);
        let outcome = tool.execute(&call, &tool_context(1024, false, "project-read"));
        assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
        assert_eq!(outcome.effect(), EffectState::KnownNotApplied);
        assert!(!outcome.is_truncated());
        let log = tool.log();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].call.as_str(), "call-1");
        assert_eq!(log[0].args, r#"{"path":"src"}"#);
        assert_eq!(log[0].scope.as_str(), "project-read");
        assert_eq!(tool.describe().id().revision(), M0_REVISION);
    }

    #[test]
    fn mutation_and_command_record_exact_args() {
        let mutation = FakeTool::mutation();
        let command = FakeTool::command();
        let write = tool_call("call-2", "host_write", r#"{"path":"dst"}"#);
        let exec = tool_call("call-3", "host_exec", r#"{"argv":"run"}"#);
        let write_outcome = mutation.execute(&write, &tool_context(1024, false, "project-write"));
        let exec_outcome = command.execute(&exec, &tool_context(1024, false, "command-scope"));
        assert_eq!(write_outcome.effect(), EffectState::KnownApplied);
        assert_eq!(exec_outcome.effect(), EffectState::KnownApplied);
        assert_eq!(mutation.log()[0].args, r#"{"path":"dst"}"#);
        assert_eq!(command.log()[0].args, r#"{"argv":"run"}"#);
        assert_eq!(mutation.execution_count(), 1);
    }

    #[test]
    fn scripted_failure_reports_unknown_effects() {
        let tool = FakeTool::failing("host_write");
        let outcome = tool.execute(
            &tool_call("call-4", "host_write", r#"{"path":"dst"}"#),
            &tool_context(1024, false, "project-write"),
        );
        assert_eq!(outcome.status(), ExecutionStatus::Failed);
        assert_eq!(outcome.effect(), EffectState::Unknown);
        assert_eq!(outcome.evidence(), Evidence::Uncertain);
        assert_eq!(tool.execution_count(), 1);
    }

    #[test]
    fn oversized_output_is_truncated_with_flag() {
        let budget = 16;
        let declared = budget + 8;
        let tool = FakeTool::oversized("host_read", declared);
        let outcome = tool.execute(
            &tool_call("call-5", "host_read", r#"{"path":"src"}"#),
            &tool_context(budget, false, "project-read"),
        );
        assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
        assert_eq!(outcome.effect(), EffectState::KnownApplied);
        assert_eq!(outcome.content().len(), budget);
        assert_eq!(outcome.content(), json_document_prefix(budget));
        assert_ne!(
            outcome.content(),
            json_document(declared),
            "the emitted prefix is not the full fixture"
        );
        assert!(outcome.is_truncated());
        assert!(budget <= Limits::M0_TEST_TOOL_OUTPUT_BYTES);
        assert_eq!(tool.execution_count(), 1);
    }

    #[test]
    fn oversized_output_within_budget_is_complete_valid_json() {
        let declared = 24;
        let tool = FakeTool::oversized("host_read", declared);
        let outcome = tool.execute(
            &tool_call("call-5b", "host_read", r#"{"path":"src"}"#),
            &tool_context(1024, false, "project-read"),
        );
        assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
        assert!(!outcome.is_truncated(), "content fits the effective budget");
        assert_eq!(outcome.content(), json_document(declared));
        assert_eq!(outcome.content().len(), declared);
        assert!(
            outcome.content().starts_with('"') && outcome.content().ends_with('"'),
            "the full fixture is a globally valid JSON string document"
        );
    }

    #[test]
    fn oversized_fixture_never_materializes_the_declared_length() {
        // A declared length near the pointer-width ceiling would abort the
        // test process if the fake allocated it before truncating. Only the
        // budget-sized prefix may be allocated.
        let budget = 32;
        let tool = FakeTool::oversized("host_read", usize::MAX / 2);
        let outcome = tool.execute(
            &tool_call("call-5c", "host_read", r#"{"path":"src"}"#),
            &tool_context(budget, false, "project-read"),
        );
        assert_eq!(outcome.content().len(), budget);
        assert_eq!(outcome.content(), json_document_prefix(budget));
        assert!(outcome.is_truncated());
    }

    #[test]
    fn cancelled_context_reports_unknown_not_rollback() {
        let tool = FakeTool::delayed("host_write", Duration::from_millis(5));
        let outcome = tool.execute(
            &tool_call("call-6", "host_write", r#"{"path":"dst"}"#),
            &tool_context(1024, true, "project-write"),
        );
        assert_eq!(outcome.status(), ExecutionStatus::Cancelled);
        assert_eq!(outcome.effect(), EffectState::Unknown);
        assert_eq!(outcome.evidence(), Evidence::Uncertain);
        assert_eq!(tool.execution_count(), 1, "attempts are recorded");
    }

    #[test]
    fn uncancelled_delay_still_succeeds() {
        let tool = FakeTool::delayed("host_write", Duration::from_millis(5));
        let outcome = tool.execute(
            &tool_call("call-7", "host_write", r#"{"path":"dst"}"#),
            &tool_context(1024, false, "project-write"),
        );
        assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    }

    #[test]
    fn delayed_tool_logs_entry_before_sleeping() {
        let tool = FakeTool::delayed("host_read", Duration::from_millis(150));
        let call = tool_call("call-delay", "host_read", r#"{"path":"src"}"#);
        let context = tool_context(1024, false, "project-read");
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| tool.execute(&call, &context));
            let deadline = Instant::now() + Duration::from_secs(5);
            while tool.execution_count() == 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(
                tool.execution_count(),
                1,
                "entry is logged before the scripted delay elapses"
            );
            let outcome = worker.join().expect("delayed tool worker joins");
            assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
        });
    }

    #[test]
    fn gated_tool_records_entry_then_waits_for_release() {
        let tool = FakeTool::gated("host_read");
        let gate = tool.gate().expect("gated tool exposes a gate handle");
        assert!(!gate.is_entered());
        let call = tool_call("call-gate", "host_read", r#"{"path":"src"}"#);
        let context = tool_context(1024, false, "project-read");
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| tool.execute(&call, &context));
            assert!(
                gate.wait_entered(Duration::from_secs(5)),
                "tool worker reached the gate"
            );
            let log = tool.log();
            assert_eq!(log.len(), 1, "entry is observable while the worker blocks");
            assert_eq!(log[0].call.as_str(), "call-gate");
            assert_eq!(log[0].args, r#"{"path":"src"}"#);
            assert!(!gate.is_released());
            gate.release();
            let outcome = worker.join().expect("gated tool worker joins");
            assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
            assert_eq!(outcome.effect(), EffectState::KnownNotApplied);
        });
        assert!(gate.is_released());
        assert_eq!(tool.execution_count(), 1);
    }

    #[test]
    fn gated_mutation_reports_known_applied_after_release() {
        let tool = FakeTool::gated_mutation("host_write");
        let gate = tool.gate().expect("gated tool exposes a gate handle");
        let call = tool_call("call-gated-write", "host_write", r#"{"path":"dst"}"#);
        let context = tool_context(1024, false, "project-write");
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| tool.execute(&call, &context));
            assert!(
                gate.wait_entered(Duration::from_secs(5)),
                "mutation worker reached the gate"
            );
            gate.release();
            let outcome = worker.join().expect("gated mutation worker joins");
            assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
            assert_eq!(outcome.effect(), EffectState::KnownApplied);
        });
        assert_eq!(tool.execution_count(), 1);
    }

    #[test]
    fn gated_tool_observes_live_cancellation_after_release() {
        let tool = FakeTool::gated("host_read");
        let gate = tool.gate().expect("gated tool exposes a gate handle");
        let token = CancellationToken::new();
        let context = tool_context(1024, false, "project-read")
            .with_control(token.clone(), Instant::now() + Duration::from_secs(60));
        let call = tool_call("call-live", "host_read", r#"{"path":"src"}"#);
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| tool.execute(&call, &context));
            assert!(
                gate.wait_entered(Duration::from_secs(5)),
                "tool worker reached the gate"
            );
            token.cancel();
            gate.release();
            let outcome = worker.join().expect("gated tool worker joins");
            assert_eq!(outcome.status(), ExecutionStatus::Cancelled);
            assert_eq!(outcome.effect(), EffectState::Unknown);
            assert_eq!(outcome.evidence(), Evidence::Uncertain);
        });
        assert_eq!(tool.execution_count(), 1, "the blocked attempt is recorded");
    }
}
