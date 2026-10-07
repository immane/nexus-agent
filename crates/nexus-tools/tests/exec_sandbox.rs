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
    #[cfg(target_os = "macos")]
    assert!(
        executor.sandbox_available(),
        "the installed macOS sandbox must pass its startup probe"
    );
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
        assert_eq!(result.effect(), nexus_core::EffectState::NotStarted);
        assert!(matches!(
            result.content(),
            "required exec sandbox program was not found"
                | "exec sandbox program was found but could not be started"
                | "exec sandbox program was found but its initialization probe failed"
        ));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn mac_sandbox_allows_project_writes_but_denies_other_project_reads_and_writes() {
    let root = TempRoot::new();
    let outside = TempRoot::new();
    let outside_file = outside.0.join("private.txt");
    std::fs::write(&outside_file, "outside-secret").unwrap();
    let executor = SandboxedExecutor::with_root(&root.0).unwrap();
    assert!(executor.sandbox_available());

    let run = |argv: Vec<String>| {
        executor.execute(
            &call(&serde_json::json!({"argv": argv}).to_string()),
            &context(),
        )
    };
    let written = root.0.join("created.txt");
    let outcome = run(vec!["/usr/bin/touch".to_owned(), "created.txt".to_owned()]);
    assert_eq!(
        outcome.status(),
        ExecutionStatus::Succeeded,
        "{}",
        outcome.content()
    );
    assert!(written.exists());

    let outcome = run(vec![
        "/bin/cat".to_owned(),
        outside_file.to_string_lossy().into_owned(),
    ]);
    assert_eq!(outcome.status(), ExecutionStatus::Failed);
    assert!(!outcome.content().contains("outside-secret"));
    let refused = outside.0.join("refused.txt");
    let outcome = run(vec![
        "/usr/bin/touch".to_owned(),
        refused.to_string_lossy().into_owned(),
    ]);
    assert_eq!(outcome.status(), ExecutionStatus::Failed);
    assert!(!refused.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn mac_sandbox_denies_network_access() {
    let root = TempRoot::new();
    let executor = SandboxedExecutor::with_root(&root.0).unwrap();
    assert!(executor.sandbox_available());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let args =
        serde_json::json!({"argv": ["/usr/bin/curl", "--noproxy", "*", "--max-time", "1", url]});
    let result = executor.execute(&call(&args.to_string()), &context());
    assert_eq!(result.status(), ExecutionStatus::Failed);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
