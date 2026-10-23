//! Uncomposed file-tool adapters for a shared container worker session.
//!
//! Cancellation after dispatch is not yet active: a worker exchange may
//! continue until its deadline. Keep these ports uncomposed until that
//! lifecycle is integrated with trusted host-side cancellation handling.

use std::sync::Arc;
use std::time::{Duration, Instant};

use nexus_core::{
    EffectState, Evidence, ExecutionStatus, M0_REVISION, ToolCall, ToolContext, ToolId,
    ToolOutcome, ToolPort, ToolSpec,
};
use serde_json::Value;

use crate::container_session::ContainerSession;
use crate::container_worker_client::{
    ClientError, ContainerWorkerClient, PreDispatchFailure, WorkerTransport,
};
use crate::fs::{
    HOST_LIST_TOOL, HOST_PATCH_TOOL, HOST_READ_TOOL, HOST_SEARCH_TOOL, HOST_WRITE_TOOL,
    LIST_SCHEMA, PATCH_SCHEMA, READ_SCHEMA, SEARCH_SCHEMA, WRITE_SCHEMA,
};

const WORKSPACE_SCOPE: &str = "/workspace";

/// The five independent ports backed by one serialized worker session.
pub struct ContainerFileTools<T> {
    pub read: ContainerFileTool<T>,
    pub list: ContainerFileTool<T>,
    pub search: ContainerFileTool<T>,
    pub write: ContainerFileTool<T>,
    pub patch: ContainerFileTool<T>,
}

/// One fixed-operation container-backed file tool.
pub struct ContainerFileTool<T> {
    session: Arc<ContainerSession<T>>,
    operation: &'static str,
    spec: ToolSpec,
}

/// Creates uncomposed `host_read`, `host_list`, `host_search`, `host_write`,
/// and `host_patch` ports sharing one synchronized client session.
///
/// This factory starts no workload, makes no authorization decision, and does
/// not make any port automatically allowed. In-flight cancellation is not
/// implemented, so these ports must not yet be composed into a runtime.
pub fn container_file_tools<T: WorkerTransport>(
    client: ContainerWorkerClient<T>,
) -> ContainerFileTools<T> {
    let session = Arc::new(ContainerSession::new(client.into_transport()));
    let port = |operation, description, schema| ContainerFileTool {
        session: Arc::clone(&session),
        operation,
        spec: ToolSpec::new(
            ToolId::new(operation, M0_REVISION).expect("static tool identity builds"),
            description,
            schema,
        )
        .expect("static container tool description builds"),
    };
    ContainerFileTools {
        read: port(
            HOST_READ_TOOL,
            "Read a file inside the container workspace",
            READ_SCHEMA,
        ),
        list: port(
            HOST_LIST_TOOL,
            "List direct children inside the container workspace",
            LIST_SCHEMA,
        ),
        search: port(
            HOST_SEARCH_TOOL,
            "Search text files inside the container workspace",
            SEARCH_SCHEMA,
        ),
        write: port(
            HOST_WRITE_TOOL,
            "Write a file inside the container workspace, creating it when missing",
            WRITE_SCHEMA,
        ),
        patch: port(
            HOST_PATCH_TOOL,
            "Replace one unique exact text occurrence in a file inside the container workspace",
            PATCH_SCHEMA,
        ),
    }
}

impl<T> ContainerFileTools<T> {
    #[allow(dead_code)]
    pub(crate) fn exec_adapter(&self) -> crate::container_exec::ContainerExecTool<T>
    where
        T: crate::container_exec::ExecTransport,
    {
        crate::container_exec::ContainerExecTool::from_session(Arc::clone(&self.read.session))
    }
}

impl<T: WorkerTransport> ToolPort for ContainerFileTool<T> {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn execute(&self, call: &ToolCall, context: &ToolContext) -> ToolOutcome {
        if call.tool() != self.spec.id() {
            return not_started(
                ExecutionStatus::Denied,
                "tool identity does not match adapter",
            );
        }
        if context.scope().as_str() != WORKSPACE_SCOPE {
            return not_started(
                ExecutionStatus::Denied,
                "container workspace scope is required",
            );
        }
        let Some(deadline) = context.deadline() else {
            return not_started(
                ExecutionStatus::Failed,
                "an evaluable tool deadline is required",
            );
        };
        if let Err(outcome) = active_outcome(context) {
            return outcome;
        }

        let mut session = loop {
            match self.session.inner.try_lock() {
                Ok(session) => break session,
                Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                    let mut session = poisoned.into_inner();
                    session.poisoned = true;
                    return not_started(
                        ExecutionStatus::Failed,
                        "container worker session is poisoned",
                    );
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    if let Err(outcome) = active_outcome(context) {
                        return outcome;
                    }
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return not_started(
                            ExecutionStatus::TimedOut,
                            "tool deadline exceeded waiting for session",
                        );
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(1)));
                }
            }
        };
        if session.poisoned {
            return not_started(
                ExecutionStatus::Failed,
                "container worker session is poisoned",
            );
        }
        if let Err(outcome) = active_outcome(context) {
            return outcome;
        }
        if Instant::now() >= deadline {
            return not_started(
                ExecutionStatus::TimedOut,
                "tool deadline exceeded before dispatch",
            );
        }
        let arguments: Value = match serde_json::from_str(call.args().as_str()) {
            Ok(arguments) => arguments,
            Err(_) => return not_started(ExecutionStatus::Failed, "tool arguments are invalid"),
        };
        match ContainerWorkerClient::<T>::execute_using(
            &mut session.transport,
            self.operation,
            &arguments,
            context.output_budget_bytes(),
            deadline,
        ) {
            Ok(outcome) => outcome,
            Err(ClientError::InvalidRequest) => {
                not_started(ExecutionStatus::Failed, "worker request was not started")
            }
            Err(ClientError::DeadlineExpired) => not_started(
                ExecutionStatus::TimedOut,
                "tool deadline exceeded before dispatch",
            ),
            Err(ClientError::Transport(error)) => match T::pre_dispatch_failure(&error) {
                Some(PreDispatchFailure::Deadline) => not_started(
                    ExecutionStatus::TimedOut,
                    "tool deadline exceeded before dispatch",
                ),
                Some(PreDispatchFailure::InvalidRequest) => {
                    not_started(ExecutionStatus::Failed, "worker request was not started")
                }
                None => {
                    session.poisoned = true;
                    uncertain("container worker result is uncertain; do not retry")
                }
            },
            Err(ClientError::InvalidResponse) => {
                session.poisoned = true;
                uncertain("container worker result is uncertain; do not retry")
            }
        }
    }
}

fn active_outcome(context: &ToolContext) -> Result<(), ToolOutcome> {
    context.check_active().map_err(|_| {
        not_started(
            if context.is_cancelled() {
                ExecutionStatus::Cancelled
            } else {
                ExecutionStatus::TimedOut
            },
            if context.is_cancelled() {
                "tool call was cancelled before dispatch"
            } else {
                "tool deadline exceeded before dispatch"
            },
        )
    })
}

fn not_started(status: ExecutionStatus, message: &'static str) -> ToolOutcome {
    ToolOutcome::new(
        status,
        EffectState::NotStarted,
        Evidence::HostObserved,
        message,
        false,
    )
    .expect("static not-started outcome builds")
}

fn uncertain(message: &'static str) -> ToolOutcome {
    ToolOutcome::new(
        ExecutionStatus::Failed,
        EffectState::Unknown,
        Evidence::Uncertain,
        message,
        false,
    )
    .expect("static uncertain outcome builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::{ApprovedScope, CallId, CancellationToken, NormalizedArgs, RunId, TurnId};
    use std::{
        io,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    struct Fake {
        calls: Arc<AtomicUsize>,
        operations: Arc<Mutex<Vec<String>>>,
        response: Vec<u8>,
        fail: bool,
        deadline_before_dispatch: bool,
        entered: Option<std::sync::mpsc::SyncSender<()>>,
        release: Option<Arc<std::sync::Barrier>>,
    }
    impl WorkerTransport for Fake {
        type Error = io::Error;
        fn exchange(
            &mut self,
            request: &[u8],
            _: usize,
            _: Instant,
        ) -> Result<Vec<u8>, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let request: Value = serde_json::from_slice(request).unwrap();
            self.operations
                .lock()
                .unwrap()
                .push(request["operation"].as_str().unwrap().to_owned());
            if let Some(entered) = self.entered.take() {
                entered.send(()).unwrap();
                self.release.as_ref().unwrap().wait();
            }
            if self.deadline_before_dispatch {
                Err(io::Error::from(io::ErrorKind::TimedOut))
            } else if self.fail {
                Err(io::Error::other("lost response"))
            } else {
                Ok(self.response.clone())
            }
        }

        fn pre_dispatch_failure(error: &Self::Error) -> Option<PreDispatchFailure> {
            (error.kind() == io::ErrorKind::TimedOut).then_some(PreDispatchFailure::Deadline)
        }
    }
    impl crate::container_exec::ExecTransport for Fake {
        fn run(
            &mut self,
            _: &[&str],
            _: usize,
            _: Instant,
            _: &dyn Fn() -> bool,
        ) -> Result<(i32, Vec<u8>, Vec<u8>), crate::container_worker_transport::DockerExecError>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(crate::container_worker_transport::DockerExecError::Io)
            } else {
                Ok((0, b"exec".to_vec(), Vec::new()))
            }
        }
    }
    fn context(scope: &str, cancelled: bool, deadline: Instant) -> ToolContext {
        let token = CancellationToken::new();
        if cancelled {
            token.cancel();
        }
        ToolContext::new(
            4096,
            Duration::ZERO,
            false,
            ApprovedScope::new(scope).unwrap(),
        )
        .unwrap()
        .with_control(token, deadline)
    }
    fn call(tool: &str) -> ToolCall {
        let arguments = match tool {
            HOST_READ_TOOL | HOST_LIST_TOOL => r#"{"path":"note.txt"}"#,
            HOST_SEARCH_TOOL => r#"{"path":".","query":"needle"}"#,
            HOST_WRITE_TOOL => r#"{"path":"note.txt","content":"x"}"#,
            HOST_PATCH_TOOL => r#"{"path":"note.txt","old_text":"x","new_text":"y"}"#,
            _ => unreachable!(),
        };
        ToolCall::new(
            RunId::new("run").unwrap(),
            TurnId::new("turn").unwrap(),
            CallId::new("call").unwrap(),
            ToolId::new(tool, M0_REVISION).unwrap(),
            NormalizedArgs::new(arguments).unwrap(),
        )
    }
    fn exec_call() -> ToolCall {
        ToolCall::new(
            nexus_core::RunId::new("run").unwrap(),
            nexus_core::TurnId::new("turn").unwrap(),
            nexus_core::CallId::new("call").unwrap(),
            ToolId::new(crate::exec::HOST_EXEC_TOOL, M0_REVISION + 1).unwrap(),
            nexus_core::NormalizedArgs::new(r#"{"argv":["x"]}"#).unwrap(),
        )
    }
    fn success() -> Vec<u8> {
        br#"{"version":1,"result":{"status":"Succeeded","effect":"KnownApplied","content":"ok","truncated":false}}"#.to_vec()
    }
    fn setup(
        fail: bool,
    ) -> (
        ContainerFileTools<Fake>,
        Arc<AtomicUsize>,
        Arc<Mutex<Vec<String>>>,
    ) {
        setup_with(fail, false)
    }

    fn setup_with(
        fail: bool,
        deadline_before_dispatch: bool,
    ) -> (
        ContainerFileTools<Fake>,
        Arc<AtomicUsize>,
        Arc<Mutex<Vec<String>>>,
    ) {
        let calls = Arc::new(AtomicUsize::new(0));
        let operations = Arc::new(Mutex::new(Vec::new()));
        (
            container_file_tools(ContainerWorkerClient::new(Fake {
                calls: calls.clone(),
                operations: operations.clone(),
                response: success(),
                fail,
                deadline_before_dispatch,
                entered: None,
                release: None,
            })),
            calls,
            operations,
        )
    }

    #[test]
    fn exact_descriptors_and_single_fixed_operations() {
        let (tools, calls, operations) = setup(false);
        for (tool, name, schema) in [
            (&tools.read, HOST_READ_TOOL, READ_SCHEMA),
            (&tools.list, HOST_LIST_TOOL, LIST_SCHEMA),
            (&tools.search, HOST_SEARCH_TOOL, SEARCH_SCHEMA),
            (&tools.write, HOST_WRITE_TOOL, WRITE_SCHEMA),
            (&tools.patch, HOST_PATCH_TOOL, PATCH_SCHEMA),
        ] {
            let spec = tool.describe();
            assert_eq!(spec.id().name(), name);
            assert_eq!(spec.id().revision(), M0_REVISION);
            assert_eq!(spec.input_schema_json(), schema);
            assert!(spec.description().contains("container workspace"));
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        for (tool, name) in [
            (&tools.read, HOST_READ_TOOL),
            (&tools.list, HOST_LIST_TOOL),
            (&tools.search, HOST_SEARCH_TOOL),
            (&tools.write, HOST_WRITE_TOOL),
            (&tools.patch, HOST_PATCH_TOOL),
        ] {
            let result = tool.execute(&call(name), &context(WORKSPACE_SCOPE, false, deadline));
            assert_eq!(result.status(), ExecutionStatus::Succeeded);
            assert_eq!(result.effect(), EffectState::KnownApplied);
            assert_eq!(result.evidence(), Evidence::PluginReported);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        assert_eq!(
            *operations.lock().unwrap(),
            [
                HOST_READ_TOOL,
                HOST_LIST_TOOL,
                HOST_SEARCH_TOOL,
                HOST_WRITE_TOOL,
                HOST_PATCH_TOOL
            ]
        );
    }

    #[test]
    fn rejects_identity_scope_and_inactive_calls_before_dispatch() {
        let (tools, calls, _) = setup(false);
        let deadline = Instant::now() + Duration::from_secs(1);
        for (invocation, ctx) in [
            (
                call(HOST_READ_TOOL),
                context(WORKSPACE_SCOPE, false, deadline),
            ),
            (
                call(HOST_WRITE_TOOL),
                context("/workspace/sub", false, deadline),
            ),
            (
                call(HOST_WRITE_TOOL),
                context(WORKSPACE_SCOPE, false, Instant::now()),
            ),
            (
                call(HOST_WRITE_TOOL),
                context(WORKSPACE_SCOPE, true, deadline),
            ),
        ] {
            assert_eq!(
                tools.write.execute(&invocation, &ctx).effect(),
                EffectState::NotStarted
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn uncertain_exchange_poisons_session_without_retry() {
        let (tools, calls, _) = setup(true);
        let ctx = context(
            WORKSPACE_SCOPE,
            false,
            Instant::now() + Duration::from_secs(1),
        );
        let first = tools.write.execute(&call(HOST_WRITE_TOOL), &ctx);
        let second = tools.read.execute(&call(HOST_READ_TOOL), &ctx);
        assert_eq!(first.effect(), EffectState::Unknown);
        assert_eq!(first.evidence(), Evidence::Uncertain);
        assert_eq!(second.effect(), EffectState::NotStarted);
        assert_eq!(second.status(), ExecutionStatus::Failed);
        assert_eq!(second.evidence(), Evidence::HostObserved);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn worker_and_exec_share_transport_and_poison_bidirectionally() {
        let (tools, calls, _) = setup(true);
        let exec = tools.exec_adapter();
        let deadline = Instant::now() + Duration::from_secs(1);
        assert_eq!(
            tools
                .write
                .execute(
                    &call(HOST_WRITE_TOOL),
                    &context(WORKSPACE_SCOPE, false, deadline)
                )
                .effect(),
            EffectState::Unknown
        );
        assert_eq!(
            exec.execute(&exec_call(), &context(WORKSPACE_SCOPE, false, deadline))
                .effect(),
            EffectState::NotStarted
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let (tools, calls, _) = setup(true);
        let exec = tools.exec_adapter();
        assert_eq!(
            exec.execute(&exec_call(), &context(WORKSPACE_SCOPE, false, deadline))
                .effect(),
            EffectState::Unknown
        );
        assert_eq!(
            tools
                .read
                .execute(
                    &call(HOST_READ_TOOL),
                    &context(WORKSPACE_SCOPE, false, deadline)
                )
                .effect(),
            EffectState::NotStarted
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let (tools, calls, _) = setup(false);
        let exec = tools.exec_adapter();
        assert_eq!(
            tools
                .read
                .execute(
                    &call(HOST_READ_TOOL),
                    &context(WORKSPACE_SCOPE, false, deadline)
                )
                .status(),
            ExecutionStatus::Succeeded
        );
        assert_eq!(
            exec.execute(&exec_call(), &context(WORKSPACE_SCOPE, false, deadline))
                .status(),
            ExecutionStatus::Succeeded
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn pre_dispatch_transport_deadline_does_not_poison_shared_session() {
        let (tools, calls, _) = setup_with(false, true);
        let deadline = Instant::now() + Duration::from_secs(1);
        let file = tools.write.execute(
            &call(HOST_WRITE_TOOL),
            &context(WORKSPACE_SCOPE, false, deadline),
        );
        assert_eq!(file.status(), ExecutionStatus::TimedOut);
        assert_eq!(file.effect(), EffectState::NotStarted);

        let exec = tools.exec_adapter();
        let next = exec.execute(&exec_call(), &context(WORKSPACE_SCOPE, false, deadline));
        assert_eq!(next.status(), ExecutionStatus::Succeeded);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn lock_wait_observes_cancellation_and_deadline_without_dispatch() {
        for cancel in [true, false] {
            let calls = Arc::new(AtomicUsize::new(0));
            let operations = Arc::new(Mutex::new(Vec::new()));
            let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
            let release = Arc::new(std::sync::Barrier::new(2));
            let tools = container_file_tools(ContainerWorkerClient::new(Fake {
                calls: calls.clone(),
                operations: operations.clone(),
                response: success(),
                fail: false,
                deadline_before_dispatch: false,
                entered: Some(entered_tx),
                release: Some(release.clone()),
            }));
            let first_tools = tools.read;
            let first = std::thread::spawn(move || {
                first_tools.execute(
                    &call(HOST_READ_TOOL),
                    &context(
                        WORKSPACE_SCOPE,
                        false,
                        Instant::now() + Duration::from_secs(2),
                    ),
                )
            });
            entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            let second_tool = tools.write;
            let token = CancellationToken::new();
            let deadline = if cancel {
                Instant::now() + Duration::from_secs(1)
            } else {
                Instant::now() + Duration::from_millis(20)
            };
            let ctx = ToolContext::new(
                4096,
                Duration::ZERO,
                false,
                ApprovedScope::new(WORKSPACE_SCOPE).unwrap(),
            )
            .unwrap()
            .with_control(token.clone(), deadline);
            let waiter =
                std::thread::spawn(move || second_tool.execute(&call(HOST_WRITE_TOOL), &ctx));
            if cancel {
                std::thread::sleep(Duration::from_millis(10));
                token.cancel();
            }
            let result = waiter.join().unwrap();
            assert_eq!(result.effect(), EffectState::NotStarted);
            assert_eq!(result.evidence(), Evidence::HostObserved);
            assert_eq!(
                result.status(),
                if cancel {
                    ExecutionStatus::Cancelled
                } else {
                    ExecutionStatus::TimedOut
                }
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            release.wait();
            assert_eq!(first.join().unwrap().status(), ExecutionStatus::Succeeded);
        }
    }
}
