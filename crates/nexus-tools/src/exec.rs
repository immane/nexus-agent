//! Runtime-authorized argv execution behind mandatory OS sandbox backends.

use std::io::{Read, Result as IoResult};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nexus_core::{
    EffectState, ErrorCategory, Evidence, ExecutionStatus, M0_REVISION, RetryGuidance, ToolCall,
    ToolContext, ToolId, ToolOutcome, ToolPort, ToolSpec,
};

pub const HOST_EXEC_TOOL: &str = "host_exec";
const OUTPUT_LIMIT: usize = 65_536;
const CLEANUP_GRACE: Duration = Duration::from_millis(250);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const DRAIN_QUANTUM: usize = 64 * 1024;
const EXEC_SCHEMA: &str = r#"{"type":"object","properties":{"argv":{"type":"array","items":{"type":"string","minLength":1},"minItems":1}},"required":["argv"],"additionalProperties":false}"#;

/// Sandboxed executor used by the real-tools composition roots.
pub struct SandboxedExecutor {
    root: PathBuf,
    backend: Backend,
    spec: ToolSpec,
    development: Option<nexus_permissions::DirectoryPolicy>,
    path: std::ffi::OsString,
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
        let mut executor = Self::unprobed(root)?;
        executor.probe();
        Ok(executor)
    }

    fn unprobed(root: &Path) -> Result<Self, nexus_core::AgentError> {
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
        Ok(Self {
            root,
            backend,
            spec,
            development: None,
            path: "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin".into(),
        })
    }

    /// Development execution keeps the host PATH but never inherits secrets.
    /// Reads are broad with protected paths; writes stay bounded by the OS.
    pub fn development(root: &Path) -> Result<Self, nexus_core::AgentError> {
        let mut executor = Self::unprobed(root)?;
        executor.development = Some(
            nexus_permissions::DirectoryPolicy::new(root)
                .map_err(|_| error(ErrorCategory::InvalidInput, "permissions root is invalid"))?,
        );
        executor.path = std::env::var_os("PATH").unwrap_or_else(|| executor.path.clone());
        executor.spec = ToolSpec::new(ToolId::new(HOST_EXEC_TOOL, M0_REVISION).expect("static tool id"),
            "Run argv in the development OS sandbox, no shell or network. Project and temporary writes are automatic; declare write_dir to request an external writable directory. Other files are broadly readable except protected paths.",
            r#"{"type":"object","properties":{"argv":{"type":"array","items":{"type":"string","minLength":1},"minItems":1},"write_dir":{"type":"string","minLength":1}},"required":["argv"],"additionalProperties":false}"#)
            .map_err(|_| error(ErrorCategory::Internal, "exec description is invalid"))?;
        executor.probe();
        Ok(executor)
    }

    // Verify only the selected profile. No fallback to another mode or to
    // unsandboxed execution if isolation fails.
    fn probe(&mut self) {
        if self.sandbox_available() {
            let mut probe = self.command(&["/usr/bin/true"], None);
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                probe.process_group(0);
            }
            self.configure_environment(&mut probe);
            match probe
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(mut child) => {
                    let deadline = Instant::now() + PROBE_TIMEOUT;
                    loop {
                        match child.try_wait() {
                            Ok(Some(status)) if status.success() => break,
                            Ok(Some(_)) => {
                                self.backend = Backend::Unavailable(SandboxFailure::ProbeFailed);
                                break;
                            }
                            Err(_) => {
                                terminate_process_group(&mut child);
                                thread::spawn(move || retain_probe_child_until_reaped(child));
                                self.backend = Backend::Unavailable(SandboxFailure::ProbeFailed);
                                break;
                            }
                            Ok(None) if Instant::now() < deadline => {
                                thread::sleep(Duration::from_millis(10))
                            }
                            Ok(None) => {
                                terminate_process_group(&mut child);
                                if wait_bounded(&mut child, CLEANUP_GRACE).is_none() {
                                    thread::spawn(move || retain_probe_child_until_reaped(child));
                                }
                                self.backend = Backend::Unavailable(SandboxFailure::ProbeFailed);
                                break;
                            }
                        }
                    }
                }
                Err(_) => self.backend = Backend::Unavailable(SandboxFailure::LaunchFailed),
            }
        }
    }

    fn configure_environment(&self, command: &mut Command) {
        configure_environment(command, &self.root);
        command.env("PATH", &self.path);
        if let Some(policy) = &self.development {
            if let Some(temporary) = policy.temporary().last() {
                command.env("TMPDIR", temporary);
            }
            command.env("PYTHONDONTWRITEBYTECODE", "1");
        }
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
        if !args.as_object().is_some_and(|object| {
            object.contains_key("argv")
                && object
                    .keys()
                    .all(|key| key == "argv" || self.development.is_some() && key == "write_dir")
        }) {
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

        let write_dir = if let Some(value) = args.get("write_dir") {
            let policy = self.development.as_ref().ok_or(Failure::Invalid)?;
            let path = policy
                .resolve(value.as_str().ok_or(Failure::Invalid)?)
                .map_err(|_| Failure::Permission)?;
            if !path.is_dir()
                || context.scope().as_str() != format!("exec-directory:{}", path.to_string_lossy())
            {
                return Err(Failure::Permission);
            }
            Some(path)
        } else {
            None
        };
        let mut command = self.command(&strings, write_dir.as_deref());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        self.configure_environment(&mut command);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Resolving an external write directory can block on the host
        // filesystem; cancellation/deadlines may change before spawning.
        context.check_active().map_err(|_| {
            if context.is_cancelled() {
                Failure::Cancelled
            } else {
                Failure::TimedOut
            }
        })?;
        let mut child = command.spawn().map_err(|_| Failure::Spawn)?;
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let stdout_nonblocking = set_nonblocking(&stdout).is_ok();
        let stderr_nonblocking = set_nonblocking(&stderr).is_ok();
        if !stdout_nonblocking || !stderr_nonblocking {
            terminate_process_group(&mut child);
            retain_child_until_reaped(&mut child, &mut stdout, &mut stderr);
            return Err(Failure::Output);
        }
        let mut out = CapturedOutput::new(OUTPUT_LIMIT);
        let mut err = CapturedOutput::new(OUTPUT_LIMIT);
        let mut status = None;
        let mut cleanup_deadline = None;
        let mut interruption = None;
        loop {
            if interruption.is_none() && context.check_active().is_err() {
                interruption = Some(if context.is_cancelled() {
                    Failure::Cancelled
                } else {
                    Failure::TimedOut
                });
                terminate_process_group(&mut child);
            }
            if status.is_none() {
                status = match child.try_wait() {
                    Ok(status) => status,
                    Err(_) => {
                        terminate_process_group(&mut child);
                        retain_child_until_reaped(&mut child, &mut stdout, &mut stderr);
                        return Err(Failure::Wait);
                    }
                };
                if status.is_some() {
                    // Stop remaining members of the original group as soon as the
                    // direct leader exits; pipe closure alone cannot establish that
                    // a descendant has stopped executing.
                    terminate_process_group(&mut child);
                    cleanup_deadline = Some(Instant::now() + CLEANUP_GRACE);
                }
            }
            if drain_pipe(&mut stdout, &mut out).is_err()
                || drain_pipe(&mut stderr, &mut err).is_err()
            {
                terminate_process_group(&mut child);
                retain_child_until_reaped(&mut child, &mut stdout, &mut stderr);
                return Err(Failure::Output);
            }
            if status.is_some() && out.eof && err.eof {
                break;
            }
            if cleanup_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                // A descendant may have escaped the process group while keeping a
                // pipe open. Keep this tool worker alive (and quarantinable) until
                // the inherited descriptors close; do not abandon pipe ownership.
                while !out.eof || !err.eof {
                    let _ = drain_pipe(&mut stdout, &mut out);
                    let _ = drain_pipe(&mut stderr, &mut err);
                    if !out.eof || !err.eof {
                        thread::sleep(Duration::from_millis(10));
                    }
                }
                break;
            }
            let timeout = rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 10_000_000,
            };
            let mut fds = Vec::with_capacity(2);
            if !out.eof {
                fds.push(rustix::event::PollFd::new(
                    &stdout,
                    rustix::event::PollFlags::IN,
                ));
            }
            if !err.eof {
                fds.push(rustix::event::PollFd::new(
                    &stderr,
                    rustix::event::PollFlags::IN,
                ));
            }
            if fds.is_empty() {
                thread::sleep(Duration::from_millis(10));
            } else if let Err(error) = rustix::event::poll(&mut fds, Some(&timeout))
                && error != rustix::io::Errno::INTR
            {
                terminate_process_group(&mut child);
                retain_child_until_reaped(&mut child, &mut stdout, &mut stderr);
                return Err(Failure::Output);
            }
        }
        let status = status.ok_or(Failure::Wait)?;
        if let Some(interruption) = interruption {
            return Err(interruption);
        }
        if !out.eof || !err.eof {
            return Err(Failure::Output);
        }
        let mut text = String::from_utf8_lossy(&out.bytes).into_owned();
        if !err.bytes.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&String::from_utf8_lossy(&err.bytes));
        }
        let mut truncated = out.truncated || err.truncated || !out.eof || !err.eof;
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

    fn command(&self, argv: &[&str], write_dir: Option<&Path>) -> Command {
        match &self.backend {
            #[cfg(target_os = "macos")]
            Backend::MacOs(sandbox) => {
                let profile = self.development.as_ref().map_or_else(
                    || mac_profile(&self.root),
                    |policy| mac_development_profile(policy, write_dir),
                );
                let mut command = Command::new(sandbox);
                command.arg("-p").arg(profile).arg("--").args(argv);
                command
            }
            #[cfg(target_os = "linux")]
            Backend::Linux(bwrap) => {
                let mut command = Command::new(bwrap);
                if self.development.is_some() {
                    command.args(["--ro-bind", "/", "/"]);
                }
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
                    if self.development.is_none() && Path::new(path).exists() {
                        command.args(["--ro-bind", path, path]);
                    }
                }
                if let Some(policy) = &self.development {
                    for path in policy
                        .temporary()
                        .iter()
                        .map(PathBuf::as_path)
                        .chain(write_dir)
                    {
                        command.arg("--bind").arg(path).arg(path);
                    }
                }
                let mut parents: Vec<_> = self.root.ancestors().collect();
                parents.reverse();
                for parent in parents
                    .into_iter()
                    .skip(1)
                    .take_while(|path| *path != self.root)
                {
                    if self.development.is_none() {
                        command.args(["--dir"]).arg(parent);
                    }
                }
                command.args(["--bind"]).arg(&self.root).arg(&self.root);
                // Masks must follow every writable bind, or a later project/
                // directory mount could expose credentials again.
                if let Some(policy) = &self.development {
                    for path in policy.protected() {
                        if path.is_dir() {
                            command
                                .arg("--tmpfs")
                                .arg(path)
                                .arg("--remount-ro")
                                .arg(path);
                        } else if path.is_file() {
                            command.arg("--ro-bind").arg("/dev/null").arg(path);
                        }
                    }
                }
                command
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

struct CapturedOutput {
    bytes: Vec<u8>,
    limit: usize,
    truncated: bool,
    eof: bool,
}

impl CapturedOutput {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(8192)),
            limit,
            truncated: false,
            eof: false,
        }
    }
}

#[cfg(unix)]
fn set_nonblocking(file: &impl std::os::fd::AsFd) -> rustix::io::Result<()> {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    let flags = fcntl_getfl(file)?;
    fcntl_setfl(file, flags | OFlags::NONBLOCK)
}

#[cfg(not(unix))]
fn set_nonblocking(_: &impl std::os::fd::AsFd) -> IoResult<()> {
    Ok(())
}

fn drain_pipe(reader: &mut impl Read, output: &mut CapturedOutput) -> IoResult<()> {
    let mut buffer = [0_u8; 8192];
    let mut drained = 0;
    while drained < DRAIN_QUANTUM {
        match reader.read(&mut buffer) {
            Ok(0) => {
                output.eof = true;
                return Ok(());
            }
            Ok(count) => {
                drained += count;
                let remaining = output.limit.saturating_sub(output.bytes.len());
                let retain = remaining.min(count);
                output.bytes.extend_from_slice(&buffer[..retain]);
                output.truncated |= retain < count;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn wait_bounded(
    child: &mut std::process::Child,
    grace: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => return None,
        }
    }
}

fn retain_child_until_reaped(
    child: &mut std::process::Child,
    stdout: &mut impl Read,
    stderr: &mut impl Read,
) {
    loop {
        let mut out = CapturedOutput::new(0);
        let mut err = CapturedOutput::new(0);
        let _ = drain_pipe(stdout, &mut out);
        let _ = drain_pipe(stderr, &mut err);
        match child.try_wait() {
            Ok(Some(_)) if out.eof && err.eof => return,
            _ => thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn retain_probe_child_until_reaped(mut child: std::process::Child) {
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            _ => thread::sleep(Duration::from_millis(10)),
        }
    }
}

#[cfg(target_os = "macos")]
fn mac_profile(root: &Path) -> String {
    let root = sbpl_quote(&root.to_string_lossy());
    // macOS process startup needs access to the root directory itself.
    // A literal grants only that directory, not its children (subpath "/").
    format!(
        "(version 1)\n(deny default)\n(allow process*)\n(allow sysctl-read)\n(allow mach-lookup)\n(allow file-read* (literal \"/\"))\n(allow file-read-metadata (literal \"/opt\"))\n(allow file-read* (subpath \"/System\"))\n(allow file-read* (subpath \"/usr\"))\n(allow file-read* (subpath \"/bin\"))\n(allow file-read* (subpath \"/sbin\"))\n(allow file-read* (subpath \"/Library\"))\n(allow file-read* (subpath \"/opt/homebrew\"))\n(allow file-read* (subpath \"/usr/local\"))\n(allow file-read* (subpath \"/private/var/db\"))\n(allow file-read* (subpath \"/private/tmp\"))\n(allow file-read* (subpath \"/dev\"))\n(allow file-read* (subpath \"{root}\"))\n(allow file-write* (subpath \"{root}\"))\n"
    )
}

#[cfg(target_os = "macos")]
fn mac_development_profile(
    policy: &nexus_permissions::DirectoryPolicy,
    write_dir: Option<&Path>,
) -> String {
    let mut profile = "(version 1)\n(deny default)\n(allow process*)\n(allow sysctl-read)\n(allow mach-lookup)\n(allow file-read*)\n".to_owned();
    for path in std::iter::once(policy.root())
        .chain(policy.temporary().iter().map(PathBuf::as_path))
        .chain(write_dir)
    {
        profile.push_str(&format!(
            "(allow file-write* (subpath \"{}\"))\n",
            sbpl_quote(&path.to_string_lossy())
        ));
    }
    for path in policy.protected() {
        profile.push_str(&format!(
            "(deny file-read* file-write* (subpath \"{}\"))\n",
            sbpl_quote(&path.to_string_lossy())
        ));
    }
    profile
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
    let group = rustix::process::Pid::from_child(child);
    let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
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
    Permission,
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
            Err(Failure::Permission) => (
                ExecutionStatus::Denied,
                EffectState::NotStarted,
                Evidence::HostObserved,
                "exec write directory is not authorized".to_owned(),
                false,
            ),
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

#[cfg(test)]
mod tests {
    use super::{CapturedOutput, drain_pipe};
    use std::io::{self, Cursor, Read};

    #[test]
    fn output_limit_caps_retention_but_drain_continues_to_eof() {
        let input = Cursor::new(b"abcdefghij".to_vec());
        let mut output = CapturedOutput::new(4);
        let mut input = input;
        drain_pipe(&mut input, &mut output).unwrap();
        assert_eq!(output.bytes, b"abcd");
        assert!(output.truncated);
        assert!(output.eof);
        assert_eq!(input.position(), 10);
    }

    struct EndlessReader(usize);

    impl Read for EndlessReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.0 += 1;
            buffer.fill(b'x');
            Ok(buffer.len())
        }
    }

    #[test]
    fn busy_pipe_drain_yields_after_its_fairness_quantum() {
        let mut reader = EndlessReader(0);
        let mut output = CapturedOutput::new(4);
        drain_pipe(&mut reader, &mut output).unwrap();
        assert_eq!(reader.0, 8);
        assert_eq!(output.bytes, b"xxxx");
        assert!(output.truncated);
        assert!(!output.eof);
    }
}
