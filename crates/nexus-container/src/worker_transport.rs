//! Docker CLI transport for an already-running, trusted worker container.
//!
//! This module never creates or manages workloads. Killing the local Docker CLI
//! does not prove the remote worker stopped; a failed exchange may follow an
//! applied effect. Callers must quarantine or terminate the environment and
//! must not retry the request.
#![allow(dead_code)] // Intentionally uncomposed until trusted host wiring exists.

use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Sender},
    thread,
    time::{Duration, Instant},
};

use crate::worker_client::WorkerTransport;

const STDERR_LIMIT: usize = 1024;
const PROBE_LIMIT: usize = 4096;
const POLL: Duration = Duration::from_millis(10);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const CLEANUP_TIMEOUT: Duration = Duration::from_millis(500);
const WORKER: &str = "/usr/local/bin/container-worker";

use std::os::fd::AsFd;

#[derive(Debug)]
pub(crate) enum DockerExecError {
    InvalidConfiguration,
    Deadline,
    CancelledBeforeStart,
    ExecutionTimedOut,
    ExecutionUncertain,
    Io,
    Failed,
    RequestTooLarge,
    Oversized,
    CleanupUncertain,
}

impl fmt::Display for DockerExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidConfiguration => "Docker worker transport configuration is invalid",
            Self::Deadline => "Docker worker exchange deadline expired",
            Self::CancelledBeforeStart => "container command cancelled before dispatch",
            Self::ExecutionTimedOut => "container command deadline exceeded after dispatch",
            Self::ExecutionUncertain => "container command outcome is uncertain",
            Self::Io => "Docker worker exchange I/O failed",
            Self::Failed => "Docker worker process failed",
            Self::RequestTooLarge => "Docker worker request exceeded its limit",
            Self::Oversized => "Docker worker response exceeded its limit",
            Self::CleanupUncertain => "Docker worker process cleanup could not be confirmed",
        })
    }
}
impl Error for DockerExecError {}

pub(crate) struct DockerExecTransport {
    docker: PathBuf,
    config: Option<PathBuf>,
    container_id: String,
    reaper: Sender<Child>,
}

impl DockerExecTransport {
    /// Binds to a pre-existing worker in an explicitly trusted container.
    pub(crate) fn new(
        docker: &Path,
        container_id: &str,
        trusted_config: Option<&Path>,
    ) -> Result<Self, DockerExecError> {
        let docker = canonical_executable(docker).ok_or(DockerExecError::InvalidConfiguration)?;
        if container_id.len() != 64 || !container_id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(DockerExecError::InvalidConfiguration);
        }
        let config = trusted_config
            .map(canonical_directory)
            .transpose()?
            .flatten();
        let (reaper, receiver) = mpsc::channel::<Child>();
        thread::Builder::new()
            .name("docker-cli-reaper".into())
            .spawn(move || {
                while let Ok(mut child) = receiver.recv() {
                    loop {
                        if child.try_wait().ok().flatten().is_some() {
                            break;
                        }
                        thread::sleep(POLL);
                    }
                }
            })
            .map_err(|_| DockerExecError::Io)?;
        let transport = Self {
            docker,
            config,
            container_id: container_id.to_ascii_lowercase(),
            reaper,
        };
        transport.verify_default_context()?;
        Ok(transport)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.docker);
        command
            .env_clear()
            .env("DOCKER_CONTEXT", "default")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(config) = &self.config {
            command.env("DOCKER_CONFIG", config);
        }
        command
    }

    fn verify_default_context(&self) -> Result<(), DockerExecError> {
        let mut command = self.command();
        command.args([
            "context",
            "inspect",
            "default",
            "--format",
            "{{.Endpoints.docker.Host}}",
        ]);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command.spawn().map_err(|_| DockerExecError::Io)?;
        let (status, out, err) = run_child(
            child,
            None,
            PROBE_LIMIT,
            Instant::now() + PROBE_TIMEOUT,
            &self.reaper,
            false,
            None,
        )?;
        let _ = err;
        if !status.success() {
            return Err(DockerExecError::InvalidConfiguration);
        }
        let endpoint = String::from_utf8_lossy(&out);
        let Some(socket) = endpoint.trim().strip_prefix("unix://") else {
            return Err(DockerExecError::InvalidConfiguration);
        };
        if socket.is_empty() || !Path::new(socket).is_absolute() {
            return Err(DockerExecError::InvalidConfiguration);
        }
        Ok(())
    }

    /// Runs one argv directly in the already-running container. The caller
    /// supplies validated arguments; no shell command string is constructed.
    pub(crate) fn exec_argv(
        &mut self,
        argv: &[&str],
        output_limit: usize,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(i32, Vec<u8>, Vec<u8>), DockerExecError> {
        if argv.is_empty()
            || argv.len() > 256
            || argv.iter().any(|arg| arg.contains('\0'))
            || argv
                .iter()
                .try_fold(0usize, |total, arg| total.checked_add(arg.len()))
                .is_none_or(|total| total > 32 * 1024)
            || output_limit > 65_536
        {
            return Err(DockerExecError::InvalidConfiguration);
        }
        if Instant::now() >= deadline {
            return Err(DockerExecError::Deadline);
        }
        if cancelled() {
            return Err(DockerExecError::CancelledBeforeStart);
        }
        let mut command = self.command();
        command
            .args(["exec", "--workdir", "/workspace"])
            .arg(&self.container_id)
            .args(argv);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if Instant::now() >= deadline {
            return Err(DockerExecError::Deadline);
        }
        if cancelled() {
            return Err(DockerExecError::CancelledBeforeStart);
        }
        let child = command.spawn().map_err(|_| DockerExecError::Io)?;
        let (status, out, err) = match run_child(
            child,
            None,
            output_limit,
            deadline,
            &self.reaper,
            true,
            Some(cancelled),
        ) {
            Ok(result) => result,
            // Before spawn, `Deadline` guarantees no remote command was
            // submitted. Once the CLI exists, timeout makes remote state
            // uncertain even when local cleanup succeeds.
            Err(DockerExecError::Deadline) => return Err(DockerExecError::ExecutionTimedOut),
            Err(error) => return Err(error),
        };
        Ok((status.code().unwrap_or(-1), out, err))
    }
}

impl WorkerTransport for DockerExecTransport {
    type Error = DockerExecError;

    fn pre_dispatch_failure(
        error: &Self::Error,
    ) -> Option<crate::worker_client::PreDispatchFailure> {
        use crate::worker_client::PreDispatchFailure;
        match error {
            DockerExecError::Deadline => Some(PreDispatchFailure::Deadline),
            DockerExecError::InvalidConfiguration | DockerExecError::RequestTooLarge => {
                Some(PreDispatchFailure::InvalidRequest)
            }
            _ => None,
        }
    }

    fn exchange(
        &mut self,
        request: &[u8],
        max_response_bytes: usize,
        deadline: Instant,
    ) -> Result<Vec<u8>, Self::Error> {
        self.exchange_controlled(request, max_response_bytes, deadline, &|| false)
    }

    fn exchange_controlled(
        &mut self,
        request: &[u8],
        max_response_bytes: usize,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<u8>, Self::Error> {
        if request.len() > crate::worker_protocol::MAX_REQUEST_BYTES {
            return Err(DockerExecError::RequestTooLarge);
        }
        if max_response_bytes > crate::worker_protocol::MAX_RESPONSE_BYTES
            || max_response_bytes.checked_add(1).is_none()
        {
            return Err(DockerExecError::InvalidConfiguration);
        }
        if Instant::now() >= deadline {
            return Err(DockerExecError::Deadline);
        }
        let mut command = self.command();
        command
            .args(["exec", "--interactive", "--workdir", "/workspace"])
            .arg(&self.container_id)
            .arg(WORKER)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        if Instant::now() >= deadline {
            return Err(DockerExecError::Deadline);
        }
        let child = command.spawn().map_err(|_| DockerExecError::Io)?;
        exchange_child(
            child,
            request,
            max_response_bytes,
            deadline,
            &self.reaper,
            cancelled,
        )
    }
}

fn exchange_child(
    child: Child,
    request: &[u8],
    max_response_bytes: usize,
    deadline: Instant,
    reaper: &Sender<Child>,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<u8>, DockerExecError> {
    let (status, output, stderr) = match run_child(
        child,
        Some(request),
        max_response_bytes,
        deadline,
        reaper,
        false,
        Some(cancelled),
    ) {
        Ok(result) => result,
        // Once spawned, a timeout cannot establish that the remote worker did
        // not process the request.
        Err(DockerExecError::Deadline) => return Err(DockerExecError::ExecutionUncertain),
        Err(error) => return Err(error),
    };
    let _bounded_diagnostic = &stderr[..stderr.len().min(STDERR_LIMIT)];
    if !status.success() {
        return Err(DockerExecError::Failed);
    }
    Ok(output)
}

fn run_child(
    mut child: Child,
    input: Option<&[u8]>,
    stdout_limit: usize,
    deadline: Instant,
    reaper: &Sender<Child>,
    drain_overflow: bool,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), DockerExecError> {
    let Some(stdout) = child.stdout.take() else {
        cleanup(child, reaper)?;
        return Err(DockerExecError::Io);
    };
    let Some(stderr) = child.stderr.take() else {
        cleanup(child, reaper)?;
        return Err(DockerExecError::Io);
    };
    let mut stdin = if input.is_some() {
        child.stdin.take()
    } else {
        None
    };
    if input.is_some() && stdin.is_none() {
        cleanup(child, reaper)?;
        return Err(DockerExecError::Io);
    }
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    for fd in [
        Some(&stdout as &dyn AsFd),
        Some(&stderr as &dyn AsFd),
        stdin.as_ref().map(|f| f as &dyn AsFd),
    ]
    .into_iter()
    .flatten()
    {
        match fcntl_getfl(fd).and_then(|flags| fcntl_setfl(fd, flags | OFlags::NONBLOCK)) {
            Ok(()) => (),
            Err(_) => {
                cleanup(child, reaper)?;
                return Err(DockerExecError::Io);
            }
        }
    }
    let mut sent = 0;
    let mut out = Vec::with_capacity(stdout_limit.min(8192));
    let mut err = Vec::with_capacity(STDERR_LIMIT.min(8192));
    let mut out_eof = false;
    let mut err_eof = false;
    let mut status = None;
    let result = 'run: loop {
        if cancelled.is_some_and(|check| check()) {
            break Err(DockerExecError::ExecutionUncertain);
        }
        if Instant::now() >= deadline {
            break Err(DockerExecError::Deadline);
        }
        match child.try_wait() {
            Ok(s) => status = status.or(s),
            Err(_) => break Err(DockerExecError::Io),
        }
        let mut buf = [0u8; 8192];
        for (fd, target, limit, eof) in [
            (&stdout as &dyn AsFd, &mut out, stdout_limit, &mut out_eof),
            (&stderr as &dyn AsFd, &mut err, STDERR_LIMIT, &mut err_eof),
        ] {
            if *eof {
                continue;
            }
            let stream_limit = if drain_overflow { stdout_limit } else { limit };
            let amount = if drain_overflow {
                buf.len()
            } else {
                stream_limit
                    .saturating_add(1)
                    .saturating_sub(target.len())
                    .min(buf.len())
            };
            match rustix::io::read(fd, &mut buf[..amount]) {
                Ok(0) => *eof = true,
                Ok(n) => {
                    let retained = if drain_overflow {
                        stream_limit
                            .saturating_add(1)
                            .saturating_sub(target.len())
                            .min(n)
                    } else {
                        n
                    };
                    target.extend_from_slice(&buf[..retained]);
                    if target.len() > limit && !drain_overflow {
                        break 'run Err(DockerExecError::Oversized);
                    }
                    // One bounded read per stream per pass keeps cancellation
                    // checks and the other pipe responsive under continuous output.
                }
                Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => (),
                Err(_) => break 'run Err(DockerExecError::Io),
            }
        }
        if let (Some(data), Some(fd)) = (input, stdin.as_ref()) {
            if sent == data.len() {
                stdin.take();
            } else {
                match rustix::io::write(fd, &data[sent..]) {
                    Ok(n) => sent += n,
                    Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => (),
                    Err(_) => break 'run Err(DockerExecError::Io),
                }
            }
        }
        if let Some(status) = status
            && out_eof
            && err_eof
        {
            break Ok((status, out, err));
        }
        let timeout = rustix::event::Timespec {
            tv_sec: 0,
            tv_nsec: POLL.as_nanos() as i64,
        };
        let mut fds = Vec::with_capacity(3);
        if !out_eof {
            fds.push(rustix::event::PollFd::new(
                &stdout,
                rustix::event::PollFlags::IN,
            ));
        }
        if !err_eof {
            fds.push(rustix::event::PollFd::new(
                &stderr,
                rustix::event::PollFlags::IN,
            ));
        }
        if let Some(fd) = stdin.as_ref() {
            fds.push(rustix::event::PollFd::new(
                fd,
                rustix::event::PollFlags::OUT,
            ));
        }
        // Keep the bounded timeout even with no pipe fds, so child exit is polled.
        if let Err(e) = rustix::event::poll(&mut fds, Some(&timeout))
            && e != rustix::io::Errno::INTR
        {
            break 'run Err(DockerExecError::Io);
        }
    };
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            cleanup(child, reaper).map_err(|cleanup_error| {
                // Preserve the deadline cause even when CLI reaping remains
                // uncertain. Neither case establishes remote termination.
                if matches!(error, DockerExecError::Deadline) {
                    DockerExecError::ExecutionTimedOut
                } else {
                    cleanup_error
                }
            })?;
            Err(error)
        }
    }
}

fn cleanup(mut child: Child, reaper: &Sender<Child>) -> Result<(), DockerExecError> {
    #[cfg(unix)]
    {
        // `process_group(0)` gives the CLI a dedicated group; kill descendants too.
        if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
    }
    let _ = child.kill();
    let deadline = Instant::now() + CLEANUP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL),
            _ => {
                // If the receiver is gone, SendError returns ownership, but no
                // reaper remains. Dropping Child then cannot guarantee reaping.
                if reaper.send(child).is_err() {
                    return Err(DockerExecError::CleanupUncertain);
                }
                return Err(DockerExecError::CleanupUncertain);
            }
        }
    }
}

fn canonical_executable(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let canonical = std::fs::canonicalize(path).ok()?;
    if !canonical.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if canonical.metadata().ok()?.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    Some(canonical)
}

fn canonical_directory(path: &Path) -> Result<Option<PathBuf>, DockerExecError> {
    if !path.is_absolute() {
        return Err(DockerExecError::InvalidConfiguration);
    }
    let canonical =
        std::fs::canonicalize(path).map_err(|_| DockerExecError::InvalidConfiguration)?;
    if !canonical.is_dir() {
        return Err(DockerExecError::InvalidConfiguration);
    }
    Ok(Some(canonical))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new(script: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "nexus-docker-transport-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let docker = dir.join("docker");
            std::fs::write(&docker, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self(dir)
        }
        fn docker(&self) -> PathBuf {
            self.0.join("docker")
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn validates_context_and_runs_fixed_command_with_clean_environment_and_eof() {
        let f = Fixture::new(
            "if [ \"$1\" = context ]; then echo unix:///tmp/docker.sock; exit; fi\nprintf '%s|' \"$DOCKER_CONTEXT\"\nprintf '%s ' \"${DOCKER_CONFIG-unset}\"\nprintf '%s ' \"$@\"\nprintf '\\n'\ncat >/dev/null\nprintf done",
        );
        let config = f.0.join("config");
        std::fs::create_dir(&config).unwrap();
        let mut t = DockerExecTransport::new(&f.docker(), ID, Some(&config)).unwrap();
        let result = t
            .exchange(b"hello", 256, Instant::now() + Duration::from_secs(2))
            .unwrap();
        assert_eq!(
            result,
            format!(
                "default|{} exec --interactive --workdir /workspace {ID} {WORKER} \ndone",
                config.canonicalize().unwrap().display()
            )
            .as_bytes()
        );
    }

    #[test]
    fn rejects_remote_context_and_expired_deadline_without_exec() {
        use crate::worker_client::PreDispatchFailure;
        assert_eq!(
            <DockerExecTransport as WorkerTransport>::pre_dispatch_failure(
                &DockerExecError::RequestTooLarge
            ),
            Some(PreDispatchFailure::InvalidRequest)
        );
        assert_eq!(
            <DockerExecTransport as WorkerTransport>::pre_dispatch_failure(
                &DockerExecError::Oversized
            ),
            None
        );
        let f = Fixture::new(
            "if [ \"$1\" = context ]; then echo tcp://remote; exit; fi\ntouch \"$MARKER\"",
        );
        assert!(DockerExecTransport::new(&f.docker(), ID, None).is_err());
        let f = Fixture::new(
            "if [ \"$1\" = context ]; then echo unix:///tmp/docker.sock; exit; fi\ntouch \"$MARKER\"",
        );
        let mut t = DockerExecTransport::new(&f.docker(), ID, None).unwrap();
        assert!(matches!(
            t.exchange(b"", 4, Instant::now()),
            Err(DockerExecError::Deadline)
        ));
        assert!(matches!(
            t.exchange(
                b"",
                crate::worker_protocol::MAX_RESPONSE_BYTES + 1,
                Instant::now() + Duration::from_secs(1)
            ),
            Err(DockerExecError::InvalidConfiguration)
        ));
    }

    #[test]
    fn detects_oversized_output_and_times_out_process_group() {
        let f = Fixture::new(
            "if [ \"$1\" = context ]; then echo unix:///tmp/docker.sock; exit; fi\nhead -c 10 /dev/zero",
        );
        let mut t = DockerExecTransport::new(&f.docker(), ID, None).unwrap();
        assert!(matches!(
            t.exchange(b"", 4, Instant::now() + Duration::from_secs(2)),
            Err(DockerExecError::Oversized)
        ));
        let f = Fixture::new(
            "if [ \"$1\" = context ]; then echo unix:///tmp/docker.sock; exit; fi\nsleep 10 & wait",
        );
        let mut t = DockerExecTransport::new(&f.docker(), ID, None).unwrap();
        assert!(matches!(
            t.exchange(b"", 4, Instant::now() + Duration::from_millis(50)),
            Err(DockerExecError::ExecutionUncertain)
        ));
    }

    #[test]
    fn deadline_covers_blocked_request_writer() {
        let f = Fixture::new(
            "if [ \"$1\" = context ]; then echo unix:///tmp/docker.sock; exit; fi\nsleep 10",
        );
        let mut t = DockerExecTransport::new(&f.docker(), ID, None).unwrap();
        let start = Instant::now();
        assert!(matches!(
            t.exchange(
                &vec![b'x'; 256 * 1024],
                16,
                start + Duration::from_millis(100)
            ),
            Err(DockerExecError::ExecutionUncertain)
        ));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn stdout_and_stderr_overflow_terminate_a_live_group() {
        for output in ["head -c 1048576 /dev/zero", "head -c 1048576 /dev/zero >&2"] {
            let f = Fixture::new(&format!(
                "if [ \"$1\" = context ]; then echo unix:///tmp/docker.sock; exit; fi\n{output}\nsleep 10"
            ));
            let mut t = DockerExecTransport::new(&f.docker(), ID, None).unwrap();
            let start = Instant::now();
            assert!(matches!(
                t.exchange(b"", 16, start + Duration::from_secs(3)),
                Err(DockerExecError::Oversized)
            ));
            assert!(start.elapsed() < Duration::from_secs(2));
        }
    }

    #[test]
    fn continuous_output_does_not_starve_cancellation() {
        let f = Fixture::new(
            "if [ \"$1\" = context ]; then echo unix:///tmp/docker.sock; exit; fi\nyes output",
        );
        let mut transport = DockerExecTransport::new(&f.docker(), ID, None).unwrap();
        let started = Instant::now();
        assert!(matches!(
            transport.exec_argv(
                &["yes", "output"],
                8,
                started + Duration::from_secs(2),
                &|| { started.elapsed() >= Duration::from_millis(50) },
            ),
            Err(DockerExecError::ExecutionUncertain)
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn eof_stream_is_not_polled_while_child_remains_alive() {
        let f = Fixture::new(
            "if [ \"$1\" = context ]; then echo unix:///tmp/docker.sock; exit; fi\nexec 1>&-\nsleep 0.1\nprintf done >&2",
        );
        let mut t = DockerExecTransport::new(&f.docker(), ID, None).unwrap();
        let start = Instant::now();
        assert_eq!(
            t.exchange(b"", 16, start + Duration::from_secs(2)).unwrap(),
            b""
        );
        assert!(start.elapsed() >= Duration::from_millis(80));
    }

    #[test]
    fn context_probe_has_a_fixed_deadline_and_reaps_the_cli() {
        let f = Fixture::new("sleep 10");
        let start = Instant::now();
        assert!(matches!(
            DockerExecTransport::new(&f.docker(), ID, None),
            Err(DockerExecError::Deadline)
        ));
        assert!(start.elapsed() < Duration::from_secs(6));
    }

    #[test]
    fn argv_cancellation_is_not_started_or_is_uncertain_as_appropriate() {
        let marker = std::env::temp_dir().join(format!("nexus-exec-marker-{}", std::process::id()));
        let f = Fixture::new(&format!(
            "if [ \"$1\" = context ]; then echo unix:///tmp/docker.sock; exit; fi\ntouch '{}'",
            marker.display()
        ));
        let mut transport = DockerExecTransport::new(&f.docker(), ID, None).unwrap();
        assert!(matches!(
            transport.exec_argv(
                &["true"],
                64,
                Instant::now() + Duration::from_secs(1),
                &|| true,
            ),
            Err(DockerExecError::CancelledBeforeStart)
        ));
        assert!(!marker.exists());

        let f = Fixture::new(
            "if [ \"$1\" = context ]; then echo unix:///tmp/docker.sock; exit; fi\nsleep 10",
        );
        let mut transport = DockerExecTransport::new(&f.docker(), ID, None).unwrap();
        let started = Instant::now();
        assert!(matches!(
            transport.exec_argv(
                &["sleep", "10"],
                64,
                started + Duration::from_secs(2),
                &|| started.elapsed() >= Duration::from_millis(50),
            ),
            Err(DockerExecError::ExecutionUncertain)
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn argv_deadline_after_spawn_is_a_timeout_with_unknown_remote_termination() {
        let f = Fixture::new(
            "if [ \"$1\" = context ]; then echo unix:///tmp/docker.sock; exit; fi\nsleep 10",
        );
        let mut transport = DockerExecTransport::new(&f.docker(), ID, None).unwrap();
        assert!(matches!(
            transport.exec_argv(&["sleep", "10"], 64, Instant::now(), &|| false),
            Err(DockerExecError::Deadline)
        ));
        let started = Instant::now();
        assert!(matches!(
            transport.exec_argv(
                &["sleep", "10"],
                64,
                started + Duration::from_millis(50),
                &|| false,
            ),
            Err(DockerExecError::ExecutionTimedOut)
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
