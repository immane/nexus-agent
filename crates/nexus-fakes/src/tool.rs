//! Deterministic scripted tool double.
//!
//! A [`FakeTool`] only runs when the runtime calls [`execute`](ToolPort::execute):
//! approval stays the runtime's job. Every execution appends its host
//! [`CallId`](nexus_core::CallId), argument text, and received scope to an
//! inspectable log. Interruption (a cancelled context, including after a
//! scripted delay) reports `Cancelled` with `Unknown` effects and `Uncertain`
//! evidence via the core outcome types, never a rewritten success.

use std::sync::Mutex;
use std::time::Duration;

use nexus_core::{
    ApprovedScope, CallId, EffectState, Evidence, ExecutionStatus, M0_REVISION, ToolCall,
    ToolContext, ToolId, ToolOutcome, ToolPort, ToolSpec,
};

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
        }
    }

    /// Tool whose output exceeds the budget, exercising truncation: content
    /// is cut to the context budget with the truncation flag set.
    pub fn oversized(tool_name: &str, content_len: usize) -> Self {
        Self {
            spec: spec_for(tool_name, "fake oversized output"),
            log: Mutex::new(Vec::new()),
            behavior: FakeToolBehavior::Oversized { content_len },
            delay: Duration::ZERO,
        }
    }

    /// Succeeding tool that sleeps `delay` before answering, for timeout and
    /// mid-delay cancellation tests.
    pub fn delayed(tool_name: &str, delay: Duration) -> Self {
        let mut tool = Self::succeeding(tool_name, "fake delayed", "delayed ok", true);
        tool.delay = delay;
        tool
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
        if context.is_cancelled() {
            self.record(call, context);
            return cancelled_outcome();
        }
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        self.record(call, context);
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
                let full = "x".repeat(*content_len);
                let budget = context.output_budget_bytes();
                if full.len() > budget {
                    ToolOutcome::new(
                        ExecutionStatus::Succeeded,
                        EffectState::KnownApplied,
                        Evidence::HostObserved,
                        full[..budget].to_owned(),
                        true,
                    )
                } else {
                    ToolOutcome::new(
                        ExecutionStatus::Succeeded,
                        EffectState::KnownApplied,
                        Evidence::HostObserved,
                        full,
                        false,
                    )
                }
            }
        };
        outcome.expect("fake outcome builds")
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::{Limits, NormalizedArgs, RunId, TurnId};
    use std::time::Duration;

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
        let tool = FakeTool::oversized("host_read", budget + 8);
        let outcome = tool.execute(
            &tool_call("call-5", "host_read", r#"{"path":"src"}"#),
            &tool_context(budget, false, "project-read"),
        );
        assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
        assert_eq!(outcome.content().len(), budget);
        assert!(outcome.is_truncated());
        assert!(budget <= Limits::M0_TEST_TOOL_OUTPUT_BYTES);
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
}
