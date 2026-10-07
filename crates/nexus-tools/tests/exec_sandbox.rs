#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use nexus_core::{
    ApprovedScope, CallId, ExecutionStatus, M0_REVISION, NormalizedArgs, RunId, ToolCall,
    ToolContext, ToolId, ToolPort, TurnId,
};
use nexus_tools::SandboxedExecutor;

static ROOTS: AtomicUsize = AtomicUsize::new(0);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "nexus-exec-test-{}-{}",
            std::process::id(),
            ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("test root is created");
        Self(path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn call(args: &str) -> ToolCall {
    ToolCall::new(
        RunId::new("run-exec").unwrap(),
        TurnId::new("turn-exec").unwrap(),
        CallId::new("call-exec").unwrap(),
        ToolId::new("host_exec", M0_REVISION).unwrap(),
        NormalizedArgs::new(args).unwrap(),
    )
}

fn context() -> ToolContext {
    ToolContext::new(
        4096,
        Duration::ZERO,
        false,
        ApprovedScope::new("command:/usr/bin/printf sandbox-ok").unwrap(),
    )
    .unwrap()
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn argv_runs_inside_the_configured_sandbox_root() {
    let root = TempRoot::new();
    let executor = SandboxedExecutor::with_root(&root.0).expect("valid root binds");
    let result = executor.execute(
        &call(r#"{"argv":["/usr/bin/printf","sandbox-ok"]}"#),
        &context(),
    );
    if executor.sandbox_available() {
        assert_eq!(
            result.status(),
            ExecutionStatus::Succeeded,
            "{}",
            result.content()
        );
        assert_eq!(result.content(), "sandbox-ok");
    } else {
        assert_eq!(result.status(), ExecutionStatus::Denied);
        assert_eq!(result.content(), "required exec sandbox is unavailable");
    }
}
