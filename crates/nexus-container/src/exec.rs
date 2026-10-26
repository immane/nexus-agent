//! Uncomposed container-backed `host_exec` adapter for an existing container.
//!
//! Cancellation after dispatch only kills the local Docker CLI. The remote
//! process may continue, so uncertain outcomes poison this shared session.
//! The runtime composes it only in an explicit container session. Uncertain
//! outcomes poison the shared transport and invoke workload cleanup.

use std::{sync::Arc, time::Instant};

use nexus_core::{
    EffectState, Evidence, ExecutionStatus, M0_REVISION, ToolCall, ToolContext, ToolId,
    ToolOutcome, ToolPort, ToolSpec,
};

use crate::session::ContainerSession;
use crate::worker_transport::{DockerExecError, DockerExecTransport};

const TOOL: &str = "host_exec";
const REVISION: u32 = M0_REVISION + 2;
const LIMIT: usize = 65_536;
const MAX_ARGUMENT_BYTES: usize = 32 * 1024;
const SCHEMA: &str = r#"{"type":"object","properties":{"argv":{"type":"array","items":{"type":"string","minLength":1},"minItems":1},"shell_command":{"type":"string","minLength":1,"maxLength":32757}},"minProperties":1,"maxProperties":1,"additionalProperties":false}"#;
const DESCRIPTION: &str = "Run argv directly or shell_command via /bin/bash -c in the existing container at /workspace. Both use the same container boundary, deadline, cancellation, and output limit. No TTY; results may be uncertain if the Docker CLI is interrupted.";

pub(crate) struct ContainerExecTool<T = DockerExecTransport> {
    session: Arc<ContainerSession<T>>,
    spec: ToolSpec,
}

pub(crate) trait ExecTransport: crate::worker_client::WorkerTransport + Send {
    fn run(
        &mut self,
        argv: &[&str],
        limit: usize,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(i32, Vec<u8>, Vec<u8>), DockerExecError>;
}

impl ExecTransport for DockerExecTransport {
    fn run(
        &mut self,
        argv: &[&str],
        limit: usize,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(i32, Vec<u8>, Vec<u8>), DockerExecError> {
        self.exec_argv(argv, limit, deadline, cancelled)
    }
}

impl ContainerExecTool<DockerExecTransport> {
    #[allow(dead_code)]
    pub(crate) fn new(transport: DockerExecTransport) -> Self {
        Self::with_transport(transport)
    }
}

impl<T: ExecTransport> ContainerExecTool<T> {
    fn with_transport(transport: T) -> Self {
        Self {
            session: Arc::new(ContainerSession::new(transport)),
            spec: ToolSpec::new(
                ToolId::new(TOOL, REVISION).expect("static tool id"),
                DESCRIPTION,
                SCHEMA,
            )
            .expect("static tool spec"),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn from_session(session: Arc<ContainerSession<T>>) -> Self {
        Self {
            session,
            spec: ToolSpec::new(
                ToolId::new(TOOL, REVISION).expect("static tool id"),
                DESCRIPTION,
                SCHEMA,
            )
            .expect("static tool spec"),
        }
    }

    fn invoke(
        &self,
        call: &ToolCall,
        context: &ToolContext,
    ) -> Result<(i32, String, bool), OutcomeFailure> {
        if context.scope().as_str() != "/workspace" {
            return Err(OutcomeFailure::Denied);
        }
        if call.tool() != self.spec.id() {
            return Err(OutcomeFailure::Invalid);
        }
        active(context)?;
        let value: serde_json::Value =
            serde_json::from_str(call.args().as_str()).map_err(|_| OutcomeFailure::Invalid)?;
        let object = value.as_object().ok_or(OutcomeFailure::Invalid)?;
        if object.len() != 1 {
            return Err(OutcomeFailure::Invalid);
        }
        let argv = if let Some(args) = object.get("argv") {
            let args = args
                .as_array()
                .filter(|a| !a.is_empty() && a.len() <= 256)
                .ok_or(OutcomeFailure::Invalid)?;
            let mut argv = Vec::with_capacity(args.len());
            for arg in args {
                let s = arg
                    .as_str()
                    .filter(|s| !s.is_empty() && !s.contains('\0'))
                    .ok_or(OutcomeFailure::Invalid)?;
                argv.push(s);
            }
            argv
        } else if let Some(command) = object.get("shell_command") {
            let command = command
                .as_str()
                .filter(|s| !s.is_empty() && !s.contains('\0'))
                .ok_or(OutcomeFailure::Invalid)?;
            vec!["/bin/bash", "-c", command]
        } else {
            return Err(OutcomeFailure::Invalid);
        };
        if argv
            .iter()
            .try_fold(0usize, |total, arg| total.checked_add(arg.len()))
            .is_none_or(|total| total > MAX_ARGUMENT_BYTES)
        {
            return Err(OutcomeFailure::Invalid);
        }
        let deadline = context.deadline().ok_or(OutcomeFailure::TimedOut)?;
        let mut session = loop {
            active(context)?;
            match self.session.inner.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::WouldBlock) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(OutcomeFailure::TimedOut);
                    }
                    std::thread::sleep(remaining.min(std::time::Duration::from_millis(1)));
                    active(context)?;
                }
                Err(std::sync::TryLockError::Poisoned(_)) => return Err(OutcomeFailure::Uncertain),
            }
        };
        active(context)?;
        if session.poisoned {
            return Err(OutcomeFailure::Poisoned);
        }
        active(context)?;
        let cancelled = || context.is_cancelled();
        match session.transport.run(&argv, LIMIT, deadline, &cancelled) {
            Ok((code, out, err)) => {
                let mut bytes = out;
                if !err.is_empty() {
                    if !bytes.is_empty() {
                        bytes.push(b'\n');
                    }
                    bytes.extend(err);
                }
                let mut text = String::from_utf8_lossy(&bytes).into_owned();
                let mut truncated = bytes.len() > LIMIT;
                if text.len() > context.output_budget_bytes() {
                    let mut end = context.output_budget_bytes();
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    text.truncate(end);
                    truncated = true;
                }
                if truncated {
                    text.push_str("\n[output truncated]");
                }
                Ok((code, text, truncated))
            }
            Err(DockerExecError::Deadline) => Err(OutcomeFailure::TimedOut),
            Err(DockerExecError::CancelledBeforeStart) => Err(OutcomeFailure::Cancelled),
            Err(DockerExecError::InvalidConfiguration | DockerExecError::RequestTooLarge) => {
                Err(OutcomeFailure::Invalid)
            }
            Err(_) => {
                session.poison();
                Err(OutcomeFailure::Uncertain)
            }
        }
    }
}

fn active(context: &ToolContext) -> Result<(), OutcomeFailure> {
    context.check_active().map_err(|_| {
        if context.is_cancelled() {
            OutcomeFailure::Cancelled
        } else {
            OutcomeFailure::TimedOut
        }
    })
}

enum OutcomeFailure {
    Invalid,
    Denied,
    Cancelled,
    TimedOut,
    Uncertain,
    Poisoned,
}

impl<T: ExecTransport> ToolPort for ContainerExecTool<T> {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }
    fn execute(&self, call: &ToolCall, context: &ToolContext) -> ToolOutcome {
        let outcome = match self.invoke(call, context) {
            Ok((0, output, truncated)) => ToolOutcome::from_bounded_content(
                ExecutionStatus::Succeeded,
                EffectState::Unknown,
                Evidence::HostObserved,
                output,
                truncated,
            ),
            Ok((code, output, truncated)) => ToolOutcome::from_bounded_content(
                ExecutionStatus::Failed,
                EffectState::Unknown,
                Evidence::HostObserved,
                format!("exit status {code}\n{output}"),
                truncated,
            ),
            Err(OutcomeFailure::Denied) => ToolOutcome::new(
                ExecutionStatus::Denied,
                EffectState::NotStarted,
                Evidence::HostObserved,
                "container exec scope is not authorized",
                false,
            ),
            Err(OutcomeFailure::Invalid) => ToolOutcome::new(
                ExecutionStatus::Failed,
                EffectState::NotStarted,
                Evidence::HostObserved,
                "container command is invalid",
                false,
            ),
            Err(OutcomeFailure::Cancelled) => ToolOutcome::new(
                ExecutionStatus::Cancelled,
                EffectState::NotStarted,
                Evidence::HostObserved,
                "command cancelled before dispatch",
                false,
            ),
            Err(OutcomeFailure::TimedOut) => ToolOutcome::new(
                ExecutionStatus::TimedOut,
                EffectState::NotStarted,
                Evidence::HostObserved,
                "command deadline expired before dispatch",
                false,
            ),
            Err(OutcomeFailure::Poisoned) => ToolOutcome::new(
                ExecutionStatus::Failed,
                EffectState::NotStarted,
                Evidence::HostObserved,
                "container exec session is uncertain and unavailable",
                false,
            ),
            Err(OutcomeFailure::Uncertain) => ToolOutcome::new(
                ExecutionStatus::Failed,
                EffectState::Unknown,
                Evidence::Uncertain,
                "container command outcome is uncertain; do not retry",
                false,
            ),
        };
        outcome
            .and_then(|o| o.enforce_budget(context.output_budget_bytes()))
            .unwrap_or_else(|_| {
                ToolOutcome::new(
                    ExecutionStatus::Failed,
                    EffectState::Unknown,
                    Evidence::Uncertain,
                    "container exec output could not be bounded",
                    false,
                )
                .expect("static outcome")
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::{ApprovedScope, CallId, NormalizedArgs, RunId, TurnId};
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    struct Fake {
        calls: Arc<AtomicUsize>,
        fail: bool,
        invalid_before_dispatch: bool,
        seen: Arc<Mutex<Vec<String>>>,
    }
    impl ExecTransport for Fake {
        fn run(
            &mut self,
            argv: &[&str],
            _: usize,
            _: Instant,
            _: &dyn Fn() -> bool,
        ) -> Result<(i32, Vec<u8>, Vec<u8>), DockerExecError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.seen.lock().unwrap() = argv.iter().map(|s| s.to_string()).collect();
            if self.invalid_before_dispatch {
                self.invalid_before_dispatch = false;
                Err(DockerExecError::InvalidConfiguration)
            } else if self.fail {
                Err(DockerExecError::Io)
            } else {
                Ok((7, b"out".to_vec(), b"err".to_vec()))
            }
        }
    }
    impl crate::worker_client::WorkerTransport for Fake {
        type Error = std::io::Error;
        fn exchange(&mut self, _: &[u8], _: usize, _: Instant) -> Result<Vec<u8>, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(std::io::Error::other("lost response"))
            } else {
                Ok(br#"{"version":1,"result":{"status":"Succeeded","effect":"KnownApplied","content":"ok","truncated":false}}"#.to_vec())
            }
        }
    }
    fn ctx(scope: &str, deadline: Instant) -> ToolContext {
        ToolContext::new(
            4096,
            Duration::ZERO,
            false,
            ApprovedScope::new(scope).unwrap(),
        )
        .unwrap()
        .with_control(nexus_core::CancellationToken::new(), deadline)
    }
    fn call(args: &str) -> ToolCall {
        ToolCall::new(
            RunId::new("r").unwrap(),
            TurnId::new("t").unwrap(),
            CallId::new("c").unwrap(),
            ToolId::new(TOOL, REVISION).unwrap(),
            NormalizedArgs::new(args).unwrap(),
        )
    }

    #[test]
    fn registered_schema_compiles_and_requires_exactly_one_command_form() {
        let schema = nexus_validation::CompiledSchema::compile(SCHEMA).unwrap();
        let budget = nexus_core::Limits::M0_TEST_ARG_ASSEMBLY_BYTES;
        for args in [
            r#"{"argv":["printf","hello"]}"#,
            r#"{"shell_command":"printf hello | cat"}"#,
        ] {
            assert!(schema.validate(args, budget).is_ok(), "{args}");
        }
        for args in [
            r#"{}"#,
            r#"{"argv":["true"],"shell_command":"true"}"#,
            r#"{"unknown":"true"}"#,
            r#"{"shell_command":"true","extra":true}"#,
            r#"{"argv":[]}"#,
            r#"{"shell_command":""}"#,
            r#"{"shell_command":null}"#,
        ] {
            assert!(schema.validate(args, budget).is_err(), "{args}");
        }
    }

    #[test]
    fn descriptors_argv_and_completed_nonzero_status_are_truthful() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let tool = ContainerExecTool::with_transport(Fake {
            calls: calls.clone(),
            fail: false,
            invalid_before_dispatch: false,
            seen: seen.clone(),
        });
        let spec = tool.describe();
        assert_eq!(spec.id().name(), TOOL);
        assert_eq!(spec.id().revision(), REVISION);
        assert_eq!(spec.input_schema_json(), SCHEMA);
        let out = tool.execute(
            &call(r#"{"argv":["printf","hello world","; rm -rf /"]}"#),
            &ctx("/workspace", Instant::now() + Duration::from_secs(1)),
        );
        assert_eq!(
            *seen.lock().unwrap(),
            ["printf", "hello world", "; rm -rf /"]
        );
        assert_eq!(out.status(), ExecutionStatus::Failed);
        assert_eq!(out.effect(), EffectState::Unknown);
        assert_eq!(out.evidence(), Evidence::HostObserved);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn invalid_scope_and_commands_do_not_dispatch_and_uncertain_failure_poisons() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let tool = ContainerExecTool::with_transport(Fake {
            calls: calls.clone(),
            fail: true,
            invalid_before_dispatch: false,
            seen,
        });
        for (args, scope) in [
            (r#"{"argv":[]}"#, "/workspace"),
            (r#"{"argv":["x\u0000y"]}"#, "/workspace"),
            (r#"{"shell_command":""}"#, "/workspace"),
            (r#"{"shell_command":"x\u0000y"}"#, "/workspace"),
            (r#"{"argv":["x"],"shell_command":"y"}"#, "/workspace"),
            (r#"{"shell_command":"x","extra":true}"#, "/workspace"),
            (
                &format!(
                    r#"{{"shell_command":"{}"}}"#,
                    "x".repeat(MAX_ARGUMENT_BYTES)
                ),
                "/workspace",
            ),
            (r#"{"argv":["x"],"write_dir":"/tmp"}"#, "/workspace"),
            (r#"{"argv":["x"]}"#, "/workspace/sub"),
        ] {
            assert_eq!(
                tool.execute(
                    &call(args),
                    &ctx(scope, Instant::now() + Duration::from_secs(1))
                )
                .effect(),
                EffectState::NotStarted
            );
        }
        let valid = call(r#"{"argv":["x"]}"#);
        assert_eq!(
            tool.execute(
                &valid,
                &ctx("/workspace", Instant::now() + Duration::from_secs(1))
            )
            .effect(),
            EffectState::Unknown
        );
        assert_eq!(
            tool.execute(
                &valid,
                &ctx("/workspace", Instant::now() + Duration::from_secs(1))
            )
            .effect(),
            EffectState::NotStarted
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn shell_command_is_explicitly_dispatched_to_bash_inside_the_container() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let tool = ContainerExecTool::with_transport(Fake {
            calls: calls.clone(),
            fail: false,
            invalid_before_dispatch: false,
            seen: seen.clone(),
        });
        let command = "pytest -q tests | head -60";
        let output = tool.execute(
            &call(&serde_json::json!({"shell_command": command}).to_string()),
            &ctx("/workspace", Instant::now() + Duration::from_secs(1)),
        );
        assert_eq!(*seen.lock().unwrap(), ["/bin/bash", "-c", command]);
        assert_eq!(output.status(), ExecutionStatus::Failed);
        assert_eq!(output.effect(), EffectState::Unknown);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn pre_dispatch_transport_rejection_does_not_poison_session() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let tool = ContainerExecTool::with_transport(Fake {
            calls: calls.clone(),
            fail: false,
            invalid_before_dispatch: true,
            seen,
        });
        let ctx = ctx("/workspace", Instant::now() + Duration::from_secs(1));
        let first = tool.execute(&call(r#"{"argv":["x"]}"#), &ctx);
        assert_eq!(first.status(), ExecutionStatus::Failed);
        assert_eq!(first.effect(), EffectState::NotStarted);
        let second = tool.execute(&call(r#"{"argv":["x"]}"#), &ctx);
        assert_eq!(second.status(), ExecutionStatus::Failed);
        assert_eq!(second.effect(), EffectState::Unknown);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
