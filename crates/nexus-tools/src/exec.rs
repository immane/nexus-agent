//! Approval-gated argv execution behind mandatory OS sandbox backends.

use std::io::{Read, Result as IoResult};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use nexus_core::{
    EffectState, ErrorCategory, Evidence, ExecutionStatus, M0_REVISION, RetryGuidance, ToolCall,
    ToolContext, ToolId, ToolOutcome, ToolPort, ToolSpec,
};

pub const HOST_EXEC_TOOL: &str = "host_exec";
const OUTPUT_LIMIT: usize = 65_536;
const EXEC_SCHEMA: &str = r#"{"type":"object","properties":{"argv":{"type":"array","items":{"type":"string","minLength":1},"minItems":1}},"required":["argv"],"additionalProperties":false}"#;

/// Approval-gated executor used by the real-tools composition roots.
pub struct SandboxedExecutor {
    root: PathBuf,
    backend: Backend,
    spec: ToolSpec,
}

#[derive(Clone)]
enum Backend {
    #[cfg(target_os = "macos")]
    MacOs(PathBuf),
    #[cfg(target_os = "linux")]
    Linux(PathBuf),
    Unavailable(SandboxFailure),
}

#[derive(Clone, Copy)]
enum SandboxFailure {
    Missing,
    LaunchFailed,
    ProbeFailed,
}

impl SandboxFailure {
    fn message(self) -> &'static str {
        match self {
            Self::Missing => "required exec sandbox program was not found",
            Self::LaunchFailed => "exec sandbox program was found but could not be started",
            Self::ProbeFailed => {
                "exec sandbox program was found but its initialization probe failed"
            }
        }
    }
}

impl SandboxedExecutor {
    pub fn with_root(root: &Path) -> Result<Self, nexus_core::AgentError> {
        let root = std::fs::canonicalize(root)
            .map_err(|_| error(ErrorCategory::InvalidInput, "exec root is invalid"))?;
        if !root.is_dir() {
            return Err(error(ErrorCategory::InvalidInput, "exec root is invalid"));
        }
        let backend = find_backend();
        let spec = ToolSpec::new(
            ToolId::new(HOST_EXEC_TOOL, M0_REVISION).expect("static tool id"),
            "Run argv without a shell inside a mandatory OS sandbox (project read/write, no network). Requires approval.",
            EXEC_SCHEMA,
        )
        .map_err(|_| error(ErrorCategory::Internal, "exec description is invalid"))?;
        let mut executor = Self {
            root,
            backend,
            spec,
        };
        if executor.sandbox_available() {
            let mut probe = executor.command(&["/usr/bin/true"]);
            configure_environment(&mut probe, &executor.root);
            match probe
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
            {
                Ok(status) if status.success() => {}
                Ok(_) => executor.backend = Backend::Unavailable(SandboxFailure::ProbeFailed),
                Err(_) => executor.backend = Backend::Unavailable(SandboxFailure::LaunchFailed),
            }
        }
        Ok(executor)
    }

    #[must_use]
    pub fn sandbox_available(&self) -> bool {
        !matches!(self.backend, Backend::Unavailable(_))
    }

    fn run(&self, call: &ToolCall, context: &ToolContext) -> Result<(String, bool), Failure> {
        if let Backend::Unavailable(reason) = self.backend {
            return Err(Failure::Unavailable(reason));
        }
        context.check_active().map_err(|_| {
            if context.is_cancelled() {
                Failure::Cancelled
            } else {
                Failure::TimedOut
            }
        })?;
        let args: serde_json::Value =
            serde_json::from_str(call.args().as_str()).map_err(|_| Failure::Invalid)?;
        if !args
            .as_object()
            .is_some_and(|object| object.len() == 1 && object.contains_key("argv"))
        {
            return Err(Failure::Invalid);
        }
        let argv = args
            .get("argv")
            .and_then(serde_json::Value::as_array)
            .filter(|v| !v.is_empty())
            .ok_or(Failure::Invalid)?;
        let mut strings = Vec::with_capacity(argv.len());
        for arg in argv {
            let value = arg
                .as_str()
                .filter(|s| !s.is_empty() && !s.contains('\0'))
                .ok_or(Failure::Invalid)?;
            strings.push(value);
        }

        let mut command = self.command(&strings);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        configure_environment(&mut command, &self.root);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|_| Failure::Spawn)?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let out_reader = thread::spawn(move || read_bounded(stdout, OUTPUT_LIMIT));
        let err_reader = thread::spawn(move || read_bounded(stderr, OUTPUT_LIMIT));
        let status = loop {
            if context.check_active().is_err() {
                let cancelled = context.is_cancelled();
                terminate_process_group(&mut child);
                let _ = child.wait();
                let _ = out_reader.join();
                let _ = err_reader.join();
                return Err(if cancelled {
                    Failure::Cancelled
                } else {
                    Failure::TimedOut
                });
            }
            if let Some(status) = child.try_wait().map_err(|_| Failure::Wait)? {
                break status;
            }
            thread::sleep(Duration::from_millis(20));
        };
        let (out, out_cut) = out_reader
            .join()
            .map_err(|_| Failure::Output)?
            .map_err(|_| Failure::Output)?;
        let (err, err_cut) = err_reader
            .join()
            .map_err(|_| Failure::Output)?
            .map_err(|_| Failure::Output)?;
        let mut text = String::from_utf8_lossy(&out).into_owned();
        if !err.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&String::from_utf8_lossy(&err));
        }
        let mut truncated = out_cut || err_cut;
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
        if status.success() {
            Ok((text, truncated))
        } else {
            Err(Failure::Exit(text, truncated))
        }
    }

    fn command(&self, argv: &[&str]) -> Command {
        match &self.backend {
            #[cfg(target_os = "macos")]
            Backend::MacOs(sandbox) => {
                let profile = mac_profile(&self.root);
                let mut command = Command::new(sandbox);
                command.arg("-p").arg(profile).arg("--").args(argv);
                command
            }
            #[cfg(target_os = "linux")]
            Backend::Linux(bwrap) => {
                let mut command = Command::new(bwrap);
                command.args([
                    "--die-with-parent",
                    "--new-session",
                    "--unshare-all",
                    "--proc",
                    "/proc",
                    "--dev",
                    "/dev",
                ]);
                for path in ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc"] {
                    if Path::new(path).exists() {
                        command.args(["--ro-bind", path, path]);
                    }
                }
                let mut parents: Vec<_> = self.root.ancestors().collect();
                parents.reverse();
                for parent in parents
                    .into_iter()
                    .skip(1)
                    .take_while(|path| *path != self.root)
                {
                    command.args(["--dir"]).arg(parent);
                }
                command
                    .args(["--bind"])
                    .arg(&self.root)
                    .arg(&self.root)
                    .args(["--chdir"])
                    .arg(&self.root)
                    .args(["--", argv[0]])
                    .args(&argv[1..]);
                command
            }
            Backend::Unavailable(_) => unreachable!("unavailable sandboxes never build commands"),
        }
    }
}

fn configure_environment(command: &mut Command, root: &Path) {
    command
        .current_dir(root)
        .env_clear()
        .env(
            "PATH",
            "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        )
        .env("HOME", "/nonexistent");
}

fn read_bounded(reader: impl Read, limit: usize) -> IoResult<(Vec<u8>, bool)> {
    let mut bytes = Vec::with_capacity(limit.min(8192));
    reader.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    let cut = bytes.len() > limit;
    bytes.truncate(limit);
    Ok((bytes, cut))
}

#[cfg(target_os = "macos")]
fn mac_profile(root: &Path) -> String {
    let root = sbpl_quote(&root.to_string_lossy());
    // macOS process startup needs access to the root directory itself.
    // A literal grants only that directory, not its children (subpath "/").
    format!(
        "(version 1)\n(deny default)\n(allow process*)\n(allow sysctl-read)\n(allow mach-lookup)\n(allow file-read* (literal \"/\"))\n(allow file-read* (subpath \"/System\"))\n(allow file-read* (subpath \"/usr\"))\n(allow file-read* (subpath \"/bin\"))\n(allow file-read* (subpath \"/sbin\"))\n(allow file-read* (subpath \"/Library\"))\n(allow file-read* (subpath \"/opt/homebrew\"))\n(allow file-read* (subpath \"/usr/local\"))\n(allow file-read* (subpath \"/private/var/db\"))\n(allow file-read* (subpath \"/private/tmp\"))\n(allow file-read* (subpath \"/dev\"))\n(allow file-read* (subpath \"{root}\"))\n(allow file-write* (subpath \"{root}\"))\n"
    )
}

#[cfg(target_os = "macos")]
fn sbpl_quote(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(unix)]
fn terminate_process_group(child: &mut std::process::Child) {
    let group = format!("-{}", child.id());
    let killer = if cfg!(target_os = "macos") {
        "/bin/kill"
    } else {
        "/usr/bin/kill"
    };
    let _ = Command::new(killer).args(["-KILL", "--", &group]).status();
    let _ = child.kill();
}

#[cfg(not(unix))]
fn terminate_process_group(child: &mut std::process::Child) {
    let _ = child.kill();
}

fn find_backend() -> Backend {
    #[cfg(target_os = "macos")]
    if Path::new("/usr/bin/sandbox-exec").is_file() {
        return Backend::MacOs(PathBuf::from("/usr/bin/sandbox-exec"));
    }
    #[cfg(target_os = "macos")]
    let (name, make) = ("sandbox-exec", Backend::MacOs as fn(PathBuf) -> Backend);
    #[cfg(target_os = "linux")]
    let (name, make) = ("bwrap", Backend::Linux as fn(PathBuf) -> Backend);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return Backend::Unavailable(SandboxFailure::Missing);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let Some(binary) = std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
        else {
            return Backend::Unavailable(SandboxFailure::Missing);
        };
        make(binary)
    }
}

enum Failure {
    Unavailable(SandboxFailure),
    Invalid,
    Spawn,
    Wait,
    Output,
    Cancelled,
    TimedOut,
    Exit(String, bool),
}
fn error(category: ErrorCategory, message: &'static str) -> nexus_core::AgentError {
    nexus_core::AgentError::new(category, message, RetryGuidance::DoNotRetry).expect("static error")
}

impl ToolPort for SandboxedExecutor {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }
    fn execute(&self, call: &ToolCall, context: &ToolContext) -> ToolOutcome {
        let result = self.run(call, context);
        let (status, effect, evidence, content, truncated) = match result {
            Ok((output, truncated)) => (
                ExecutionStatus::Succeeded,
                EffectState::Unknown,
                Evidence::HostObserved,
                output,
                truncated,
            ),
            Err(Failure::Cancelled) => (
                ExecutionStatus::Cancelled,
                EffectState::Unknown,
                Evidence::Uncertain,
                "command cancelled; effects may have occurred".to_owned(),
                false,
            ),
            Err(Failure::TimedOut) => (
                ExecutionStatus::TimedOut,
                EffectState::Unknown,
                Evidence::Uncertain,
                "command timed out; effects may have occurred".to_owned(),
                false,
            ),
            Err(Failure::Exit(output, truncated)) => (
                ExecutionStatus::Failed,
                EffectState::Unknown,
                Evidence::HostObserved,
                output,
                truncated,
            ),
            Err(Failure::Invalid) => (
                ExecutionStatus::Failed,
                EffectState::NotStarted,
                Evidence::HostObserved,
                "argv is invalid".to_owned(),
                false,
            ),
            Err(Failure::Unavailable(reason)) => (
                ExecutionStatus::Denied,
                EffectState::NotStarted,
                Evidence::HostObserved,
                reason.message().to_owned(),
                false,
            ),
            Err(Failure::Spawn) => (
                ExecutionStatus::Failed,
                EffectState::NotStarted,
                Evidence::HostObserved,
                "sandboxed command could not start".to_owned(),
                false,
            ),
            Err(_) => (
                ExecutionStatus::Failed,
                EffectState::Unknown,
                Evidence::Uncertain,
                "sandboxed command failed".to_owned(),
                false,
            ),
        };
        ToolOutcome::from_bounded_content(status, effect, evidence, content, truncated)
            .and_then(|o| o.enforce_budget(context.output_budget_bytes()))
            .unwrap_or_else(|_| {
                ToolOutcome::new(
                    ExecutionStatus::Failed,
                    EffectState::Unknown,
                    Evidence::Uncertain,
                    "command output could not be bounded",
                    false,
                )
                .expect("static outcome")
            })
    }
}
