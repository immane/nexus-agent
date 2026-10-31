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
    context_with_scope("command:/usr/bin/printf sandbox-ok")
}

fn context_with_scope(scope: &str) -> ToolContext {
    ToolContext::new(
        4096,
        Duration::ZERO,
        false,
        ApprovedScope::new(scope).unwrap(),
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

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires an explicitly provisioned Linux sandbox and curl"]
fn linux_sandbox_confines_files_and_network() {
    let root = TempRoot::new();
    let outside = TempRoot::new();
    let secret = outside.0.join("secret.txt");
    std::fs::write(&secret, "outside-secret").unwrap();
    let executor = SandboxedExecutor::with_root(&root.0).unwrap();
    assert!(
        executor.sandbox_available(),
        "Linux sandbox backend is required"
    );
    let run = |argv: Vec<String>| {
        executor.execute(
            &call(&serde_json::json!({"argv": argv}).to_string()),
            &context(),
        )
    };

    let written = run(vec!["/usr/bin/touch".into(), "created.txt".into()]);
    assert_eq!(
        written.status(),
        ExecutionStatus::Succeeded,
        "{}",
        written.content()
    );
    assert!(root.0.join("created.txt").exists());

    let read = run(vec!["/bin/cat".into(), secret.display().to_string()]);
    assert_eq!(read.status(), ExecutionStatus::Failed);
    assert!(!read.content().contains("outside-secret"));
    let refused = outside.0.join("refused.txt");
    let write = run(vec!["/usr/bin/touch".into(), refused.display().to_string()]);
    assert_eq!(write.status(), ExecutionStatus::Failed);
    assert!(!refused.exists());

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    assert!(std::path::Path::new("/usr/bin/curl").is_file());
    let url = format!("http://{}", listener.local_addr().unwrap());
    let network = run(vec![
        "/usr/bin/curl".into(),
        "--noproxy".into(),
        "*".into(),
        "--max-time".into(),
        "1".into(),
        url,
    ]);
    assert_eq!(network.status(), ExecutionStatus::Failed);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires an explicitly provisioned Linux sandbox"]
fn linux_development_sandbox_masks_project_credentials() {
    let root = TempRoot::new();
    std::fs::write(root.0.join(".env"), "sandbox-secret").unwrap();
    let executor = SandboxedExecutor::development(&root.0).unwrap();
    assert!(
        executor.sandbox_available(),
        "Linux sandbox backend is required"
    );
    let result = executor.execute(
        &call(r#"{"argv":["/bin/cat",".env"]}"#),
        &context_with_scope("exec:workspace"),
    );
    match result.status() {
        ExecutionStatus::Succeeded => assert!(result.content().is_empty()),
        ExecutionStatus::Failed => assert!(result.content().contains("Permission denied")),
        status => panic!("unexpected credential read status: {status:?}"),
    }
    assert!(
        !result.content().contains("sandbox-secret"),
        "credential was exposed"
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires an explicitly provisioned Linux sandbox"]
fn linux_development_external_write_requires_approved_scope() {
    let root = TempRoot::new();
    let outside = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .unwrap();
    let file = outside.join(format!("nexus-sandbox-write-{}", std::process::id()));
    assert!(!file.exists());
    let executor = SandboxedExecutor::development(&root.0).unwrap();
    assert!(executor.sandbox_available());
    let args = serde_json::json!({"argv":["/usr/bin/touch",file]});
    let denied = executor.execute(
        &call(&args.to_string()),
        &context_with_scope("exec:workspace"),
    );
    assert_eq!(
        denied.status(),
        ExecutionStatus::Failed,
        "{}",
        denied.content()
    );
    assert!(!file.exists());
    let args = serde_json::json!({"argv":["/usr/bin/touch",file], "write_dir":outside});
    let denied = executor.execute(
        &call(&args.to_string()),
        &context_with_scope("exec:workspace"),
    );
    assert_eq!(denied.status(), ExecutionStatus::Denied);
    let approved = context_with_scope(&format!("exec-directory:{}", outside.display()));
    let result = executor.execute(&call(&args.to_string()), &approved);
    let exists = file.exists();
    if exists {
        std::fs::remove_file(&file).unwrap();
    }
    assert_eq!(
        result.status(),
        ExecutionStatus::Succeeded,
        "{}",
        result.content()
    );
    assert!(exists);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires an explicitly provisioned Linux sandbox"]
fn linux_sandbox_timeout_cleans_up_and_accepts_another_call() {
    let root = TempRoot::new();
    let executor = SandboxedExecutor::with_root(&root.0).unwrap();
    assert!(executor.sandbox_available());
    let started = std::time::Instant::now();
    let context = context().with_control(
        nexus_core::CancellationToken::new(),
        started + Duration::from_millis(100),
    );
    let result = executor.execute(
        &call(r#"{"argv":["/bin/sh","-c","printf started; exec /bin/sleep 30"]}"#),
        &context,
    );
    assert_eq!(
        result.status(),
        ExecutionStatus::TimedOut,
        "{}",
        result.content()
    );
    assert_eq!(result.effect(), nexus_core::EffectState::Unknown);
    assert!(started.elapsed() < Duration::from_secs(2));
    let next = executor.execute(
        &call(r#"{"argv":["/usr/bin/printf","sandbox-ok"]}"#),
        &context_with_scope("command:/usr/bin/printf sandbox-ok"),
    );
    assert_eq!(
        next.status(),
        ExecutionStatus::Succeeded,
        "{}",
        next.content()
    );
    assert_eq!(next.content(), "sandbox-ok");
}

#[cfg(target_os = "macos")]
#[test]
fn expired_command_cleans_up_and_executor_accepts_the_next_call() {
    let root = TempRoot::new();
    let executor = SandboxedExecutor::with_root(&root.0).expect("valid root binds");
    assert!(executor.sandbox_available());
    let started = std::time::Instant::now();
    let context = context().with_control(
        nexus_core::CancellationToken::new(),
        started + Duration::from_millis(100),
    );
    let result = executor.execute(
        &call(r#"{"argv":["/bin/sh","-c","printf started; exec /bin/sleep 30"]}"#),
        &context,
    );
    assert_eq!(
        result.status(),
        ExecutionStatus::TimedOut,
        "{}",
        result.content()
    );
    assert_eq!(result.effect(), nexus_core::EffectState::Unknown);
    assert!(started.elapsed() < Duration::from_secs(2));
    let next = executor.execute(
        &call(r#"{"argv":["/usr/bin/printf","sandbox-ok"]}"#),
        &context_with_scope("command:/usr/bin/printf sandbox-ok"),
    );
    assert_eq!(
        next.status(),
        ExecutionStatus::Succeeded,
        "{}",
        next.content()
    );
    assert_eq!(next.content(), "sandbox-ok");
}

#[cfg(target_os = "macos")]
#[test]
fn descendant_holding_output_pipe_does_not_extend_cleanup_unboundedly() {
    let root = TempRoot::new();
    let executor = SandboxedExecutor::with_root(&root.0).expect("valid root binds");
    assert!(executor.sandbox_available());
    let started = std::time::Instant::now();
    let result = executor.execute(
        &call(
            r#"{"argv":["/bin/sh","-c","(/usr/bin/touch spawned; /bin/sleep 5) & while [ ! -e spawned ]; do /bin/sleep 0.01; done; exit 0"]}"#,
        ),
        &context(),
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "cleanup exceeded its bound"
    );
    assert_eq!(
        result.status(),
        ExecutionStatus::Succeeded,
        "{}",
        result.content()
    );
    assert!(
        root.0.join("spawned").exists(),
        "descendant handshake completed"
    );
    assert_eq!(result.effect(), nexus_core::EffectState::Unknown);
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

#[cfg(target_os = "macos")]
#[test]
fn development_python_runs_with_stdlib_temp_files_and_protected_credentials() {
    let root = TempRoot::new();
    std::fs::write(root.0.join(".env"), "sandbox-secret").unwrap();
    let executor = SandboxedExecutor::development(&root.0).unwrap();
    assert!(executor.sandbox_available());
    let mut interpreters = 0;
    for python in ["python3", "python"] {
        if !std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .any(|path| path.join(python).is_file())
        {
            continue;
        }
        interpreters += 1;
        let script = "import pathlib,tempfile,ssl,json,socket; pathlib.Path('ok.txt').write_text('ok'); f=tempfile.TemporaryFile(); f.write(b'ok'); f.close();\ntry: pathlib.Path('.env').read_text(); raise AssertionError('credential readable')\nexcept PermissionError: pass\ntry: s=socket.socket(); s.connect(('127.0.0.1',9)); raise AssertionError('network permitted')\nexcept PermissionError: pass\nprint('python-ok')";
        let result = executor.execute(
            &call(&serde_json::json!({"argv":[python,"-c",script]}).to_string()),
            &context_with_scope("exec:workspace"),
        );
        assert_eq!(
            result.status(),
            ExecutionStatus::Succeeded,
            "{python}: {}",
            result.content()
        );
        assert!(result.content().contains("python-ok"));
    }
    assert!(
        interpreters > 0,
        "Python regression requires an installed interpreter"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn development_sandbox_allows_dev_null_but_not_other_device_writes() {
    let root = TempRoot::new();
    let executor = SandboxedExecutor::development(&root.0).unwrap();
    assert!(executor.sandbox_available());
    let context = context_with_scope("exec:workspace");

    let null = executor.execute(&call(r#"{"argv":["/usr/bin/tee","/dev/null"]}"#), &context);
    assert_eq!(
        null.status(),
        ExecutionStatus::Succeeded,
        "{}",
        null.content()
    );

    let zero = executor.execute(&call(r#"{"argv":["/usr/bin/tee","/dev/zero"]}"#), &context);
    assert_eq!(zero.status(), ExecutionStatus::Failed, "{}", zero.content());
}

#[cfg(target_os = "macos")]
#[test]
fn development_external_writes_require_a_bound_declared_directory() {
    let root = TempRoot::new();
    let executor = SandboxedExecutor::development(&root.0).unwrap();
    assert!(executor.sandbox_available());
    // A project outside both /tmp and the process temp directory.
    let outside = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .unwrap();
    let file = outside.join(format!("nexus-sandbox-write-{}", std::process::id()));
    assert!(!file.exists());
    let args = serde_json::json!({"argv":["/usr/bin/touch",file]});
    let workspace_context = context_with_scope("exec:workspace");
    let refused = executor.execute(&call(&args.to_string()), &workspace_context);
    assert_eq!(
        refused.status(),
        ExecutionStatus::Failed,
        "{}",
        refused.content()
    );
    assert!(!file.exists());
    let args = serde_json::json!({"argv":["/usr/bin/touch",file], "write_dir":outside});
    let refused = executor.execute(&call(&args.to_string()), &workspace_context);
    assert_eq!(refused.status(), ExecutionStatus::Denied);
    let context = ToolContext::new(
        4096,
        Duration::ZERO,
        false,
        ApprovedScope::new(format!("exec-directory:{}", outside.display())).unwrap(),
    )
    .unwrap();
    let allowed = executor.execute(&call(&args.to_string()), &context);
    let exists = file.exists();
    if exists {
        std::fs::remove_file(&file).unwrap();
    }
    assert_eq!(
        allowed.status(),
        ExecutionStatus::Succeeded,
        "{}",
        allowed.content()
    );
    assert!(exists);
}
