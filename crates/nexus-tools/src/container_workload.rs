//! Feature-gated workload-container lifecycle owner.
//!
//! Keeper health gates tool construction. Runtime authorization is supplied
//! by the explicit container policy; network isolation remains unvalidated.

use std::time::Duration;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use nexus_core::{
    ToolAccess, ToolCall, ToolContext, ToolOutcome, ToolPort, ToolRegistration, ToolSpec,
};

const OWNER_LABEL: &str = "io.nexus-agent.workload-owner";
const RUNNER_OUTPUT_LIMIT: usize = 64 * 1024;
const CREATE_TIMEOUT: Duration = Duration::from_secs(120);
const START_TIMEOUT: Duration = Duration::from_secs(20);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(30);
const INSPECT_TIMEOUT: Duration = Duration::from_secs(5);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const HEALTH_POLL: Duration = Duration::from_millis(200);
const PROCESS_CLEANUP_GRACE: Duration = Duration::from_millis(250);
const MAX_LIVE_SESSIONS: usize = 2;

static LIVE_SESSIONS: AtomicUsize = AtomicUsize::new(0);
static QUARANTINED: OnceLock<Mutex<Vec<QuarantinedEnvironment>>> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Unprepared,
    Creating,
    Created,
    Starting,
    ReadinessPending,
    Ready,
    Cleaning,
    Removed,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ResultBytes {
    status: Option<i32>,
    stdout: Vec<u8>,
    timed_out: bool,
}

trait Runner: Send + Sync {
    fn run(&self, args: &[String], timeout: Duration, limit: usize) -> Result<ResultBytes, ()>;
}

/// Fixed executable Docker CLI runner. Endpoint selection is default-context
/// only; the only inherited config permitted is the operator's canonical config.
struct DockerRunner {
    executable: PathBuf,
    config: Option<PathBuf>,
    reaper: std::sync::mpsc::Sender<std::process::Child>,
}

impl DockerRunner {
    fn new(executable: &Path, config: Option<&Path>) -> Result<Self, ()> {
        if !executable.is_absolute() || !executable.is_file() {
            return Err(());
        }
        let executable = std::fs::canonicalize(executable).map_err(|_| ())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if executable.metadata().map_err(|_| ())?.permissions().mode() & 0o111 == 0 {
                return Err(());
            }
        }
        let config = config
            .map(std::fs::canonicalize)
            .transpose()
            .map_err(|_| ())?;
        if config.as_ref().is_some_and(|p| !p.is_dir()) {
            return Err(());
        }
        let (reaper, receiver) = std::sync::mpsc::channel::<std::process::Child>();
        std::thread::Builder::new()
            .name("container-cli-reaper".into())
            .spawn(move || {
                while let Ok(mut child) = receiver.recv() {
                    loop {
                        match child.try_wait() {
                            Ok(Some(_)) => break,
                            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                            Err(_) => {
                                let _ = child.kill();
                                let _ = child.wait();
                                break;
                            }
                        }
                    }
                }
            })
            .map_err(|_| ())?;
        let runner = Self {
            executable,
            config,
            reaper,
        };
        let probe = runner.run(
            &[
                "context".into(),
                "inspect".into(),
                "default".into(),
                "--format".into(),
                "{{.Endpoints.docker.Host}}".into(),
            ],
            Duration::from_secs(5),
            4096,
        )?;
        let endpoint = String::from_utf8(probe.stdout).map_err(|_| ())?;
        let Some(socket) = endpoint.trim().strip_prefix("unix://") else {
            return Err(());
        };
        if !probe.status.is_some_and(|s| s == 0)
            || probe.timed_out
            || socket.is_empty()
            || !Path::new(socket).is_absolute()
        {
            return Err(());
        }
        Ok(runner)
    }
}

impl Runner for DockerRunner {
    fn run(&self, args: &[String], timeout: Duration, limit: usize) -> Result<ResultBytes, ()> {
        use std::os::fd::AsFd;
        use std::process::{Command, Stdio};
        use std::time::Instant;

        let cap = limit.checked_add(1).ok_or(())?;
        if timeout.is_zero() {
            return Ok(ResultBytes {
                status: None,
                stdout: Vec::new(),
                timed_out: true,
            });
        }
        let deadline = Instant::now().checked_add(timeout).ok_or(())?;
        let mut cmd = Command::new(&self.executable);
        cmd.env_clear()
            .env("DOCKER_CONTEXT", "default")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .args(args);
        if let Some(config) = &self.config {
            cmd.env("DOCKER_CONFIG", config);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(|_| ())?;
        let Some(stdout) = child.stdout.take() else {
            cleanup_child(child, &self.reaper);
            return Err(());
        };
        use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
        if fcntl_getfl(&stdout)
            .and_then(|flags| fcntl_setfl(&stdout, flags | OFlags::NONBLOCK))
            .is_err()
        {
            cleanup_child(child, &self.reaper);
            return Err(());
        }
        let mut output = Vec::with_capacity(cap.min(8192));
        let mut eof = false;
        let mut exit = None;
        let mut failure = false;
        loop {
            if exit.is_none() {
                match child.try_wait() {
                    Ok(status) => exit = status,
                    Err(_) => {
                        failure = true;
                        break;
                    }
                }
            }
            if eof && exit.is_some() {
                break;
            }
            if Instant::now() >= deadline {
                cleanup_child(child, &self.reaper);
                return Ok(ResultBytes {
                    status: None,
                    stdout: output,
                    timed_out: true,
                });
            }
            if !eof {
                let mut buf = [0u8; 8192];
                let amount = cap.saturating_sub(output.len()).min(buf.len());
                match rustix::io::read(stdout.as_fd(), &mut buf[..amount]) {
                    Ok(0) => eof = true,
                    Ok(n) => {
                        output.extend_from_slice(&buf[..n]);
                        if output.len() > limit {
                            cleanup_child(child, &self.reaper);
                            return Err(());
                        }
                    }
                    Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => (),
                    Err(_) => {
                        failure = true;
                        break;
                    }
                }
            }
            let timeout = rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 10_000_000,
            };
            let mut fds = Vec::new();
            if !eof {
                fds.push(rustix::event::PollFd::new(
                    &stdout,
                    rustix::event::PollFlags::IN,
                ));
            }
            if let Err(error) = rustix::event::poll(&mut fds, Some(&timeout))
                && error != rustix::io::Errno::INTR
            {
                failure = true;
                break;
            }
        }
        if failure {
            cleanup_child(child, &self.reaper);
            return Err(());
        }
        let status = exit.and_then(|s| s.code());
        Ok(ResultBytes {
            status,
            stdout: output,
            timed_out: false,
        })
    }
}

fn cleanup_child(
    mut child: std::process::Child,
    reaper: &std::sync::mpsc::Sender<std::process::Child>,
) {
    #[cfg(unix)]
    if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.kill();
    let deadline = std::time::Instant::now() + Duration::from_millis(250);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => break,
        }
    }
    let _ = reaper.send(child);
}

#[derive(Clone)]
pub(crate) struct Config {
    workspace: PathBuf,
    resolver_file: PathBuf,
    image: String,
    owner: String,
    name: String,
    gateway_id: String,
}

impl Config {
    fn new(
        workspace: &Path,
        resolver_file: &Path,
        image: &str,
        nonce: &str,
        gateway_id: &str,
    ) -> Result<Self, ()> {
        let workspace = std::fs::canonicalize(workspace).map_err(|_| ())?;
        if !workspace.is_dir() || !workspace.is_absolute() || workspace.parent().is_none() {
            return Err(());
        }
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .and_then(|p| std::fs::canonicalize(p).ok());
        if is_protected_workspace(&workspace, home.as_deref())
            || !valid_image(image)
            || !valid_nonce(nonce)
            || !valid_id(gateway_id)
            || workspace.to_string_lossy().contains(',')
            || workspace.to_string_lossy().chars().any(char::is_control)
            || !resolver_file.is_absolute()
            || resolver_file.to_string_lossy().contains(',')
            || resolver_file
                .to_string_lossy()
                .chars()
                .any(char::is_control)
        {
            return Err(());
        }
        Ok(Self {
            workspace,
            resolver_file: resolver_file.to_owned(),
            image: image.to_owned(),
            owner: format!("workload-{nonce}"),
            name: format!("nexus-workload-{nonce}"),
            gateway_id: gateway_id.to_ascii_lowercase(),
        })
    }
}

fn is_protected_workspace(workspace: &Path, home: Option<&Path>) -> bool {
    if home.is_some_and(|home| home == workspace || home.starts_with(workspace)) {
        return true;
    }

    let protected = [
        "/etc", "/var", "/usr", "/bin", "/sbin", "/lib", "/lib64", "/boot", "/dev", "/proc",
        "/sys", "/run", "/root",
    ];
    #[cfg(target_os = "macos")]
    let protected = protected
        .into_iter()
        .chain([
            "/System/Library",
            "/System/Applications",
            "/System/DriverKit",
            "/System/Volumes/Preboot",
            "/System/Volumes/Update",
            "/System/Volumes/iSCPreboot",
            "/Library",
            "/Applications",
            "/private/etc",
            "/private/var",
        ])
        .collect::<Vec<_>>();

    let mut paths: Vec<PathBuf> = protected.into_iter().map(PathBuf::from).collect();
    paths.extend([
        PathBuf::from("/var/lib/docker"),
        PathBuf::from("/var/run/docker"),
        PathBuf::from("/run/docker"),
    ]);
    if let Some(home) = home {
        paths.extend([
            home.join(".docker"),
            home.join(".config/docker"),
            home.join(".local/share/docker"),
            home.join(".local/state/docker"),
        ]);
    }

    paths
        .into_iter()
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .any(|path| workspace.starts_with(&path) || path.starts_with(workspace))
}

struct Workload<R: Runner> {
    runner: R,
    config: Config,
    ownership: Option<Arc<crate::container_ownership::OwnedRecord>>,
    id: Option<String>,
    state: State,
    cleanup_attempted: bool,
}

impl<R: Runner> Workload<R> {
    #[cfg(test)]
    fn new(runner: R, config: Config) -> Self {
        Self::new_with_ownership(runner, config, None)
    }

    fn new_with_ownership(
        runner: R,
        config: Config,
        ownership: Option<Arc<crate::container_ownership::OwnedRecord>>,
    ) -> Self {
        Self {
            runner,
            config,
            ownership,
            id: None,
            state: State::Unprepared,
            cleanup_attempted: false,
        }
    }

    fn prepare(&mut self) -> Result<(), ()> {
        if let Some(ownership) = &self.ownership {
            ownership.update(|record| {
                record.workload_name = Some(self.config.name.clone());
                record.resolver_file = Some(self.config.resolver_file.clone());
            })?;
        }
        self.state = State::Creating;
        let args = self.create_args();
        let result = self.runner.run(&args, CREATE_TIMEOUT, RUNNER_OUTPUT_LIMIT);
        match result {
            Ok(r) if r.status == Some(0) && !r.timed_out => {
                let id = String::from_utf8(r.stdout)
                    .ok()
                    .map(|s| s.trim().to_owned());
                if !id.as_deref().is_some_and(valid_id) {
                    self.state = State::Unknown;
                    self.reconcile_create();
                    return Err(());
                }
                let id = id.unwrap();
                self.id = Some(id);
                self.state = State::Created;
                self.persist_workload_id()
            }
            _ => {
                self.state = State::Unknown;
                self.reconcile_create();
                Err(())
            }
        }
    }

    fn persist_workload_id(&self) -> Result<(), ()> {
        if let (Some(ownership), Some(id)) = (&self.ownership, &self.id) {
            ownership.update(|record| record.workload_id = Some(id.clone()))?;
        }
        Ok(())
    }

    fn start(&mut self) -> Result<(), ()> {
        let id = self.id.clone().ok_or(())?;
        self.state = State::Starting;
        let r = self
            .runner
            .run(&["start".into(), id], START_TIMEOUT, RUNNER_OUTPUT_LIMIT);
        match r {
            Ok(v) if v.status == Some(0) && !v.timed_out => {
                self.state = State::ReadinessPending;
                self.wait_until_healthy()
            }
            _ => {
                self.state = State::Unknown;
                let _ = self.close();
                Err(())
            }
        }
    }

    fn wait_until_healthy(&mut self) -> Result<(), ()> {
        let deadline = std::time::Instant::now()
            .checked_add(HEALTH_TIMEOUT)
            .ok_or(())?;
        let id = self.id.clone().ok_or(())?;
        let inspect = vec![
            "inspect".into(),
            "--format".into(),
            format!(
                "{{{{.Id}}}}\t{{{{index .Config.Labels \"{OWNER_LABEL}\"}}}}\t{{{{.State.Running}}}}\t{{{{if .State.Health}}}}{{{{.State.Health.Status}}}}{{{{else}}}}none{{{{end}}}}"
            ),
            id.clone(),
        ];

        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                self.state = State::Unknown;
                let _ = self.close();
                return Err(());
            }
            if let Ok(result) = self.runner.run(
                &inspect,
                remaining.min(INSPECT_TIMEOUT),
                RUNNER_OUTPUT_LIMIT,
            ) && !result.timed_out
                && result.status == Some(0)
            {
                let output = String::from_utf8_lossy(&result.stdout);
                let fields: Vec<_> = output.trim().split('\t').collect();
                if fields.len() == 4 {
                    if fields[0] != id || fields[1] != self.config.owner || fields[2] != "true" {
                        self.state = State::Unknown;
                        let _ = self.close();
                        return Err(());
                    }
                    match fields[3] {
                        "healthy" => {
                            self.state = State::Ready;
                            return Ok(());
                        }
                        "unhealthy" | "none" => {
                            self.state = State::Unknown;
                            let _ = self.close();
                            return Err(());
                        }
                        "starting" => {}
                        _ => {
                            self.state = State::Unknown;
                            let _ = self.close();
                            return Err(());
                        }
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                self.state = State::Unknown;
                let _ = self.close();
                return Err(());
            }
            std::thread::sleep(
                HEALTH_POLL.min(deadline.saturating_duration_since(std::time::Instant::now())),
            );
        }
    }

    fn create_args(&self) -> Vec<String> {
        let mut a = vec![
            "create".into(),
            "--pull=never".into(),
            "--name".into(),
            self.config.name.clone(),
            "--label".into(),
            format!("{OWNER_LABEL}={}", self.config.owner),
            "--network".into(),
            format!("container:{}", self.config.gateway_id),
            "--init".into(),
            "--user".into(),
            "1000:1000".into(),
            "--cap-drop=ALL".into(),
            "--security-opt=no-new-privileges:true".into(),
            "--read-only".into(),
            "--pids-limit=256".into(),
            "--memory=4g".into(),
            "--memory-swap=4g".into(),
            "--cpus=2".into(),
            "--tmpfs".into(),
            "/tmp:rw,noexec,nosuid,nodev,size=512m,uid=1000,gid=1000,mode=0700".into(),
            "--tmpfs".into(),
            "/home/worker:rw,noexec,nosuid,nodev,size=1g,uid=1000,gid=1000,mode=0700".into(),
            "--mount".into(),
            format!(
                "type=bind,src={},dst=/workspace,bind-propagation=rprivate,bind-recursive=disabled",
                self.config.workspace.display()
            ),
            // Keep Linux dependency trees and Next build artifacts off the
            // host bind mount. Besides avoiding cross-OS native packages,
            // this sidesteps Docker Desktop's non-POSIX rename behavior that
            // makes npm fail with ENOTEMPTY while replacing package folders.
            "--tmpfs".into(),
            "/workspace/node_modules:rw,exec,nosuid,nodev,size=1g,uid=1000,gid=1000,mode=0755"
                .into(),
            "--tmpfs".into(),
            "/workspace/.next:rw,nosuid,nodev,size=1g,uid=1000,gid=1000,mode=0755".into(),
            "--mount".into(),
            format!(
                "type=bind,src={},dst=/etc/resolv.conf,readonly,bind-propagation=rprivate",
                self.config.resolver_file.display()
            ),
            "--workdir=/workspace".into(),
            "--env=PATH=/usr/local/bin:/usr/bin:/bin".into(),
            "--env=HOME=/home/worker".into(),
            "--env=TMPDIR=/tmp".into(),
            "--env=LC_ALL=C.UTF-8".into(),
            "--entrypoint".into(),
            "/usr/local/bin/nexus-keeper".into(),
            self.config.image.clone(),
            "run".into(),
        ];
        // Image entrypoint is explicitly overridden; no image defaults execute.
        a.shrink_to_fit();
        a
    }

    fn reconcile_create(&mut self) {
        let args = vec![
            "ps".into(),
            "-a".into(),
            "--no-trunc".into(),
            "--filter".into(),
            format!("name=^{}$", self.config.name),
            "--filter".into(),
            format!("label={OWNER_LABEL}={}", self.config.owner),
            "--format".into(),
            "{{.ID}}".into(),
        ];
        if let Ok(r) = self.runner.run(&args, INSPECT_TIMEOUT, RUNNER_OUTPUT_LIMIT)
            && r.status == Some(0)
            && !r.timed_out
        {
            let output = String::from_utf8_lossy(&r.stdout);
            let ids: Vec<_> = output.lines().collect();
            if ids.len() == 1 && valid_id(ids[0]) {
                self.id = Some(ids[0].into());
                self.state = State::Unknown;
                let _ = self.persist_workload_id();
            }
        }
    }

    fn close(&mut self) -> Result<(), ()> {
        if self.state == State::Removed {
            return Ok(());
        }
        if self.cleanup_attempted {
            return Err(());
        }
        self.cleanup_attempted = true;
        let Some(id) = self.id.clone() else {
            return if self.state == State::Unknown {
                Err(())
            } else {
                self.state = State::Removed;
                Ok(())
            };
        };
        self.state = State::Cleaning;
        let deadline = std::time::Instant::now() + CLEANUP_TIMEOUT;
        let inspect = vec![
            "inspect".into(),
            "--format".into(),
            format!("{{{{.Id}}}}\t{{{{index .Config.Labels \"{OWNER_LABEL}\"}}}}"),
            id.clone(),
        ];
        let verified = self
            .run_cleanup(&inspect, deadline, INSPECT_TIMEOUT, RUNNER_OUTPUT_LIMIT)
            .ok()
            .is_some_and(|r| {
                !r.timed_out
                    && r.status == Some(0)
                    && String::from_utf8_lossy(&r.stdout).trim()
                        == format!("{}\t{}", id, self.config.owner)
            });
        if !verified {
            self.state = State::Unknown;
            return Err(());
        }
        let stop = self.run_cleanup(
            &["stop".into(), "--time=1".into(), id.clone()],
            deadline,
            CLEANUP_TIMEOUT,
            RUNNER_OUTPUT_LIMIT,
        );
        if stop.is_err()
            || stop
                .as_ref()
                .is_ok_and(|r| r.timed_out || r.status != Some(0))
        {
            self.state = State::Unknown;
            return Err(());
        }
        let rm = self.run_cleanup(
            &["rm".into(), id],
            deadline,
            CLEANUP_TIMEOUT,
            RUNNER_OUTPUT_LIMIT,
        );
        if rm.is_err()
            || rm.as_ref().is_ok_and(|r| r.timed_out)
            || rm.as_ref().is_ok_and(|r| r.status != Some(0))
        {
            self.state = State::Unknown;
            return Err(());
        }
        self.state = State::Removed;
        self.id = None;
        if let Some(ownership) = &self.ownership
            && ownership
                .update(|record| {
                    record.workload_name = None;
                    record.workload_id = None;
                    record.resolver_file = None;
                })
                .is_err()
        {
            self.state = State::Unknown;
            return Err(());
        }
        Ok(())
    }

    fn run_cleanup(
        &self,
        args: &[String],
        deadline: std::time::Instant,
        operation_limit: Duration,
        output_limit: usize,
    ) -> Result<ResultBytes, ()> {
        let remaining = deadline
            .saturating_duration_since(std::time::Instant::now())
            .saturating_sub(PROCESS_CLEANUP_GRACE);
        if remaining.is_zero() {
            return Ok(ResultBytes {
                status: None,
                stdout: Vec::new(),
                timed_out: true,
            });
        }
        self.runner
            .run(args, remaining.min(operation_limit), output_limit)
    }
}

/// Owns one workload and creates tool adapters only after the keeper passes
/// Docker's bounded health gate.
pub(crate) struct ContainerWorkloadSession {
    workload: Arc<Mutex<Workload<DockerRunner>>>,
    resources: Arc<Mutex<LifecycleResources>>,
    slot: Option<LifecycleSlot>,
    quarantined: bool,
    pub(crate) files: crate::container_file_tools::ContainerFileTools<
        crate::container_worker_transport::DockerExecTransport,
    >,
    pub(crate) exec: crate::container_exec::ContainerExecTool<
        crate::container_worker_transport::DockerExecTransport,
    >,
}

#[derive(Clone, Copy)]
enum OwnedPortKind {
    Read,
    List,
    Search,
    Write,
    Patch,
    Exec,
}

struct OwnedContainerPort {
    owner: Arc<Mutex<ContainerWorkloadSession>>,
    kind: OwnedPortKind,
}

impl ToolPort for OwnedContainerPort {
    fn describe(&self) -> ToolSpec {
        let owner = lock_container_session(&self.owner);
        match self.kind {
            OwnedPortKind::Read => owner.files.read.describe(),
            OwnedPortKind::List => owner.files.list.describe(),
            OwnedPortKind::Search => owner.files.search.describe(),
            OwnedPortKind::Write => owner.files.write.describe(),
            OwnedPortKind::Patch => owner.files.patch.describe(),
            OwnedPortKind::Exec => owner.exec.describe(),
        }
    }

    fn execute(&self, call: &ToolCall, context: &ToolContext) -> ToolOutcome {
        let owner = lock_container_session(&self.owner);
        match self.kind {
            OwnedPortKind::Read => owner.files.read.execute(call, context),
            OwnedPortKind::List => owner.files.list.execute(call, context),
            OwnedPortKind::Search => owner.files.search.execute(call, context),
            OwnedPortKind::Write => owner.files.write.execute(call, context),
            OwnedPortKind::Patch => owner.files.patch.execute(call, context),
            OwnedPortKind::Exec => owner.exec.execute(call, context),
        }
    }
}

fn lock_container_session(
    owner: &Mutex<ContainerWorkloadSession>,
) -> MutexGuard<'_, ContainerWorkloadSession> {
    owner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Prepares one gateway/workload pair and returns host-registered container
/// tools. Every port retains the shared owner; dropping the final registration
/// closes the workload before the gateway, or quarantines uncertain resources.
pub fn container_tool_registrations(
    workspace: &Path,
    gateway_image: &str,
    workload_image: &str,
) -> Result<Vec<ToolRegistration>, crate::container::ContainerError> {
    crate::container::validate_image_reference(gateway_image)?;
    crate::container::validate_image_reference(workload_image)?;
    let docker = resolve_docker_executable()
        .ok_or(crate::container::ContainerError::InvalidDockerExecutable)?;
    let blocked_cidrs = crate::container::discover_host_ipv4_cidrs()?;
    let gateway = crate::container::ContainerEnvironment::prepare(
        workspace,
        &docker,
        gateway_image,
        &blocked_cidrs,
    )?;
    let nonce = gateway
        .identity()
        .name()
        .strip_prefix("nexus-gw-")
        .ok_or(crate::container::ContainerError::DockerFailure)?
        .to_owned();
    let session = ContainerWorkloadSession::prepare(gateway, workspace, workload_image, &nonce)
        .map_err(crate::container::ContainerError::WorkloadPreparation)?;
    let owner = Arc::new(Mutex::new(session));
    let registration = |kind, access| {
        let port: Arc<dyn ToolPort + Send + Sync> = Arc::new(OwnedContainerPort {
            owner: Arc::clone(&owner),
            kind,
        });
        match access {
            ToolAccess::Read => ToolRegistration::read(port),
            ToolAccess::Mutation => ToolRegistration::mutation(port),
            ToolAccess::Process => ToolRegistration::process(port),
            ToolAccess::Unknown => unreachable!("container built-ins are classified"),
        }
    };
    Ok(vec![
        registration(OwnedPortKind::Read, ToolAccess::Read),
        registration(OwnedPortKind::List, ToolAccess::Read),
        registration(OwnedPortKind::Search, ToolAccess::Read),
        registration(OwnedPortKind::Write, ToolAccess::Mutation),
        registration(OwnedPortKind::Patch, ToolAccess::Mutation),
        registration(OwnedPortKind::Exec, ToolAccess::Process),
    ])
}

fn resolve_docker_executable() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join("docker");
        let Ok(canonical) = std::fs::canonicalize(candidate) else {
            continue;
        };
        if !canonical.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if !canonical
                .metadata()
                .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
            {
                continue;
            }
        }
        return Some(canonical);
    }
    None
}

impl ContainerWorkloadSession {
    pub(crate) fn prepare(
        gateway: crate::container::ContainerEnvironment,
        workspace: &Path,
        image: &str,
        nonce: &str,
    ) -> Result<Self, &'static str> {
        if !gateway.is_healthy() {
            return Err("gateway readiness");
        }
        let mut slot = Some(LifecycleSlot::reserve().map_err(|_| "session capacity")?);
        let gateway_id = gateway.container_id().ok_or("gateway identity")?;
        let resolver_file = ResolverConfigFile::new(nonce).map_err(|_| "resolver file")?;
        let config = Config::new(workspace, &resolver_file.path, image, nonce, gateway_id)
            .map_err(|_| "workload configuration")?;
        let docker = gateway.docker_executable();
        let trusted_config = gateway.trusted_docker_config();
        let ownership = gateway.ownership_record().ok_or("ownership record")?;
        let runner = DockerRunner::new(docker, trusted_config).map_err(|_| "Docker runner")?;
        let mut workload = Workload::new_with_ownership(runner, config, Some(ownership));
        if workload.prepare().is_err() {
            if workload.close().is_err() {
                retain_quarantined(
                    workload,
                    gateway,
                    resolver_file,
                    slot.take().expect("reserved lifecycle slot"),
                );
            }
            return Err("workload creation");
        }
        if workload.start().is_err() {
            if workload.state != State::Removed {
                retain_quarantined(
                    workload,
                    gateway,
                    resolver_file,
                    slot.take().expect("reserved lifecycle slot"),
                );
            }
            return Err("workload readiness");
        }
        let Some(id) = workload.id.as_deref() else {
            if workload.close().is_err() {
                retain_quarantined(
                    workload,
                    gateway,
                    resolver_file,
                    slot.take().expect("reserved lifecycle slot"),
                );
            }
            return Err("workload identity");
        };
        let transport = match crate::container_worker_transport::DockerExecTransport::new(
            docker,
            id,
            trusted_config,
        ) {
            Ok(transport) => transport,
            Err(_) => {
                if workload.close().is_err() {
                    retain_quarantined(
                        workload,
                        gateway,
                        resolver_file,
                        slot.take().expect("reserved lifecycle slot"),
                    );
                }
                return Err("worker transport");
            }
        };
        let workload = Arc::new(Mutex::new(workload));
        let cleanup_workload = Arc::clone(&workload);
        let resources = Arc::new(Mutex::new(LifecycleResources {
            gateway: Some(gateway),
            resolver_file,
        }));
        let cleanup_resources = Arc::clone(&resources);
        let poison_handler: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let mut workload = lock_workload(&cleanup_workload);
            if workload.close().is_err() {
                quarantine_resources(&cleanup_resources);
            }
        });
        let files = crate::container_file_tools::container_file_tools_with_handler(
            crate::container_worker_client::ContainerWorkerClient::new(transport),
            Some(poison_handler),
        );
        let exec = files.exec_adapter();
        Ok(Self {
            workload,
            resources,
            slot,
            quarantined: false,
            files,
            exec,
        })
    }

    pub(crate) fn close(&mut self) -> Result<(), ()> {
        self.files.quarantine();
        if lock_workload(&self.workload).close().is_err() {
            quarantine_resources(&self.resources);
            self.quarantined = true;
            return Err(());
        }
        let gateway = lock_resources(&self.resources).gateway.take();
        match gateway {
            Some(gateway) => match gateway.close_for_workload() {
                Ok(()) => Ok(()),
                Err(gateway) => {
                    let mut resources = lock_resources(&self.resources);
                    resources.gateway = Some(*gateway);
                    resources.resolver_file.preserve();
                    self.quarantined = true;
                    Err(())
                }
            }
            .inspect(|()| {
                self.slot.take();
            }),
            None => {
                self.slot.take();
                Ok(())
            }
        }
    }
}

impl Drop for ContainerWorkloadSession {
    fn drop(&mut self) {
        let cleanup_failed = self.close().is_err();
        if cleanup_failed || self.quarantined {
            quarantine_resources(&self.resources);
            if let Some(slot) = self.slot.take() {
                retain_quarantined_shared(
                    Arc::clone(&self.workload),
                    Arc::clone(&self.resources),
                    slot,
                );
            }
        } else {
            self.slot.take();
        }
    }
}

fn lock_workload(
    workload: &Mutex<Workload<DockerRunner>>,
) -> MutexGuard<'_, Workload<DockerRunner>> {
    workload
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct LifecycleResources {
    gateway: Option<crate::container::ContainerEnvironment>,
    resolver_file: ResolverConfigFile,
}

struct LifecycleSlot;

impl LifecycleSlot {
    fn reserve() -> Result<Self, ()> {
        let mut current = LIVE_SESSIONS.load(Ordering::Acquire);
        loop {
            if current >= MAX_LIVE_SESSIONS {
                return Err(());
            }
            match LIVE_SESSIONS.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(Self),
                Err(actual) => current = actual,
            }
        }
    }
}

impl Drop for LifecycleSlot {
    fn drop(&mut self) {
        LIVE_SESSIONS.fetch_sub(1, Ordering::AcqRel);
    }
}

struct QuarantinedEnvironment {
    _workload: Arc<Mutex<Workload<DockerRunner>>>,
    _resources: Arc<Mutex<LifecycleResources>>,
    _slot: LifecycleSlot,
}

fn retain_quarantined(
    workload: Workload<DockerRunner>,
    mut gateway: crate::container::ContainerEnvironment,
    mut resolver_file: ResolverConfigFile,
    slot: LifecycleSlot,
) {
    gateway.quarantine();
    resolver_file.preserve();
    retain_quarantined_shared(
        Arc::new(Mutex::new(workload)),
        Arc::new(Mutex::new(LifecycleResources {
            gateway: Some(gateway),
            resolver_file,
        })),
        slot,
    );
}

fn retain_quarantined_shared(
    workload: Arc<Mutex<Workload<DockerRunner>>>,
    resources: Arc<Mutex<LifecycleResources>>,
    slot: LifecycleSlot,
) {
    QUARANTINED
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(QuarantinedEnvironment {
            _workload: workload,
            _resources: resources,
            _slot: slot,
        });
}

fn lock_resources(resources: &Mutex<LifecycleResources>) -> MutexGuard<'_, LifecycleResources> {
    resources
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn quarantine_resources(resources: &Mutex<LifecycleResources>) {
    let mut resources = lock_resources(resources);
    if let Some(gateway) = resources.gateway.as_mut() {
        gateway.quarantine();
    }
    resources.resolver_file.preserve();
}

struct ResolverConfigFile {
    path: PathBuf,
    preserve: bool,
}

impl ResolverConfigFile {
    fn new(nonce: &str) -> Result<Self, ()> {
        if !valid_nonce(nonce) {
            return Err(());
        }
        let directory = std::fs::canonicalize(std::env::temp_dir()).map_err(|_| ())?;
        let path = directory.join(format!("nexus-resolver-{nonce}"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).map_err(|_| ())?;
        use std::io::Write;
        if file.write_all(b"nameserver 127.0.0.1\n").is_err() || file.sync_all().is_err() {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err(());
        }
        Ok(Self {
            path,
            preserve: false,
        })
    }

    fn preserve(&mut self) {
        self.preserve = true;
    }
}

impl Drop for ResolverConfigFile {
    fn drop(&mut self) {
        if !self.preserve {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl<R: Runner> Drop for Workload<R> {
    fn drop(&mut self) {
        if !self.cleanup_attempted
            && (self.state != State::Unknown || self.id.is_some())
            && self.state != State::Removed
        {
            let _ = self.close();
        }
    }
}

fn valid_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}
fn valid_nonce(n: &str) -> bool {
    (16..=64).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_hexdigit())
}
fn valid_image(image: &str) -> bool {
    crate::container::validate_image_reference(image).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workload_config_accepts_local_image_ids_and_repository_digests() {
        let base = config();
        let nonce = test_nonce();
        for image in [
            format!("sha256:{}", "a".repeat(64)),
            format!("example/workload@sha256:{}", "b".repeat(64)),
        ] {
            let config = Config::new(
                &base.workspace,
                &base.resolver_file,
                &image,
                &nonce,
                &base.gateway_id,
            )
            .expect("workload accepts the same immutable references as the gateway");
            assert_eq!(config.image, image);
        }
        for image in ["example/workload:latest", "sha256:abc", "-bad@sha256:abc"] {
            assert!(
                Config::new(
                    &base.workspace,
                    &base.resolver_file,
                    image,
                    &nonce,
                    &base.gateway_id,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn workload_config_rejects_system_and_docker_control_workspaces() {
        let image = format!("example/workload@sha256:{}", "a".repeat(64));
        let resolver = std::env::temp_dir().join("nexus-resolver-test");
        let id = "f".repeat(64);
        let nonce = test_nonce();

        #[cfg(unix)]
        {
            let system_dir = std::fs::canonicalize("/etc").expect("system config directory");
            assert!(Config::new(&system_dir, &resolver, &image, &nonce, &id).is_err());
        }

        let root = PathBuf::from(std::env::var_os("HOME").expect("test home directory"))
            .join(format!(".nexus-workspace-guard-{}", std::process::id()));
        let home = root.join("home");
        let project = home.join("project");
        let docker = home.join(".docker");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&docker).unwrap();
        let home = std::fs::canonicalize(home).unwrap();
        let project = std::fs::canonicalize(project).unwrap();
        let docker = std::fs::canonicalize(docker).unwrap();

        assert!(is_protected_workspace(&home, Some(&home)));
        assert!(is_protected_workspace(home.parent().unwrap(), Some(&home)));
        assert!(is_protected_workspace(&docker, Some(&home)));
        assert!(!is_protected_workspace(&project, Some(&home)));
        std::fs::remove_dir_all(root).unwrap();
    }

    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    fn test_nonce() -> String {
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        format!(
            "{:016x}{:016x}",
            u64::from(std::process::id()),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[cfg(unix)]
    struct FakeDocker(PathBuf);

    #[cfg(unix)]
    impl FakeDocker {
        fn new(script: &str) -> Self {
            use std::os::unix::fs::PermissionsExt;
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "nexus-workload-docker-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let executable = dir.join("docker");
            std::fs::write(&executable, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self(executable)
        }

        fn executable(&self) -> &Path {
            &self.0
        }
    }

    #[cfg(unix)]
    impl Drop for FakeDocker {
        fn drop(&mut self) {
            if let Some(parent) = self.0.parent() {
                let _ = std::fs::remove_dir_all(parent);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn docker_runner_uses_default_context_and_preserves_argument_boundaries() {
        let docker = FakeDocker::new(
            "if [ \"$1\" = context ]; then echo unix:///tmp/fake.sock; exit 0; fi\nprintf '%s|%s|' \"$DOCKER_CONTEXT\" \"${DOCKER_HOST-unset}\"\nfor arg in \"$@\"; do printf '<%s>' \"$arg\"; done",
        );
        let runner = DockerRunner::new(docker.executable(), None).unwrap();
        let result = runner
            .run(
                &["argv with spaces".into(), "--literal".into()],
                Duration::from_secs(1),
                128,
            )
            .unwrap();
        assert_eq!(result.status, Some(0));
        assert_eq!(
            result.stdout,
            b"default|unset|<argv with spaces><--literal>"
        );
    }

    #[cfg(unix)]
    #[test]
    fn docker_runner_rejects_remote_context_and_bounds_hang_and_output() {
        let remote = FakeDocker::new("if [ \"$1\" = context ]; then echo tcp://remote; exit 0; fi");
        assert!(DockerRunner::new(remote.executable(), None).is_err());

        let docker = FakeDocker::new(
            "if [ \"$1\" = context ]; then echo unix:///tmp/fake.sock; exit 0; fi\nif [ \"$1\" = hang ]; then exec 1>&-; sleep 5; fi\nhead -c 129 /dev/zero",
        );
        let runner = DockerRunner::new(docker.executable(), None).unwrap();
        let started = std::time::Instant::now();
        let timeout = runner
            .run(&["hang".into()], Duration::from_millis(50), 128)
            .unwrap();
        assert!(timeout.timed_out);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            runner
                .run(&["flood".into()], Duration::from_secs(1), 128)
                .is_err()
        );
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<FakeState>>);
    #[derive(Default)]
    struct FakeState {
        calls: Vec<Vec<String>>,
        replies: Vec<ResultBytes>,
    }
    impl Runner for Fake {
        fn run(&self, args: &[String], _: Duration, _: usize) -> Result<ResultBytes, ()> {
            let mut s = self.0.lock().unwrap();
            s.calls.push(args.to_vec());
            if s.replies.is_empty() {
                return Err(());
            }
            Ok(s.replies.remove(0))
        }
    }
    fn ok(out: &str) -> ResultBytes {
        ResultBytes {
            status: Some(0),
            stdout: out.as_bytes().to_vec(),
            timed_out: false,
        }
    }
    fn config() -> Config {
        let path = std::env::current_dir().unwrap();
        let resolver = std::env::temp_dir().join("nexus-resolver-test");
        Config::new(
            &path,
            &resolver,
            &format!("example/workload@sha256:{}", "a".repeat(64)),
            &test_nonce(),
            &"f".repeat(64),
        )
        .unwrap()
    }
    #[test]
    fn create_argv_uses_fixed_security_and_escaped_mount() {
        let f = Fake::default();
        f.0.lock()
            .unwrap()
            .replies
            .push(ok(&format!("{}\n", "b".repeat(64))));
        let mut w = Workload::new(f.clone(), config());
        w.prepare().unwrap();
        let calls = &f.0.lock().unwrap().calls;
        let a = &calls[0];
        assert!(a.contains(&"--cap-drop=ALL".into()));
        assert!(a.contains(&"--read-only".into()));
        assert!(a.contains(&"--init".into()));
        assert!(a.contains(&"--memory-swap=4g".into()));
        assert!(a.contains(&"/usr/local/bin/nexus-keeper".into()));
        assert!(a.iter().any(|arg| {
            arg == "/workspace/node_modules:rw,exec,nosuid,nodev,size=1g,uid=1000,gid=1000,mode=0755"
        }));
        assert!(a.iter().any(|arg| {
            arg == "/workspace/.next:rw,nosuid,nodev,size=1g,uid=1000,gid=1000,mode=0755"
        }));
        assert_eq!(
            &a[a.len() - 4..],
            &[
                "--entrypoint",
                "/usr/local/bin/nexus-keeper",
                &w.config.image,
                "run"
            ]
        );
        assert!(a.contains(&format!("container:{}", "f".repeat(64))));
        assert!(a.iter().any(|x| x.starts_with("type=bind,src=")
            && x.ends_with(",dst=/workspace,bind-propagation=rprivate,bind-recursive=disabled")));
        assert!(
            a.iter()
                .any(|x| x.contains("dst=/etc/resolv.conf,readonly"))
        );
        assert_eq!(w.state, State::Created);
        w.id = None;
        w.state = State::Removed;
    }

    #[test]
    fn resolver_file_is_private_and_removed_when_owner_drops() {
        let nonce = test_nonce();
        let resolver = ResolverConfigFile::new(&nonce).unwrap();
        assert_eq!(
            std::fs::read(&resolver.path).unwrap(),
            b"nameserver 127.0.0.1\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&resolver.path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let path = resolver.path.clone();
        drop(resolver);
        assert!(!path.exists());
    }

    #[test]
    fn lifecycle_slots_bound_active_and_quarantined_environments() {
        let first = LifecycleSlot::reserve().unwrap();
        let second = LifecycleSlot::reserve().unwrap();
        assert!(LifecycleSlot::reserve().is_err());
        drop(first);
        let replacement = LifecycleSlot::reserve().unwrap();
        drop((second, replacement));
    }
    #[test]
    fn start_failure_cleans_up_in_verified_order() {
        let f = Fake::default();
        let id = "a".repeat(64);
        let config = config();
        let owner = config.owner.clone();
        f.0.lock().unwrap().replies.extend([
            ok(&format!("{id}\n")),
            ResultBytes {
                status: Some(1),
                stdout: vec![],
                timed_out: false,
            },
            ok(&format!("{id}\t{owner}\n")),
            ok(""),
            ok(""),
        ]);
        let mut w = Workload::new(f.clone(), config);
        w.prepare().unwrap();
        assert!(w.start().is_err());
        let calls = &f.0.lock().unwrap().calls;
        assert_eq!(
            calls.iter().map(|c| c[0].as_str()).collect::<Vec<_>>(),
            ["create", "start", "inspect", "stop", "rm"]
        );
        w.state = State::Removed;
    }

    #[test]
    fn inspect_error_never_removes_and_remove_timeout_is_not_retried_on_drop() {
        let f = Fake::default();
        let id = "a".repeat(64);
        f.0.lock().unwrap().replies.extend([
            ok(&format!("{id}\n")),
            ResultBytes {
                status: None,
                stdout: vec![],
                timed_out: true,
            },
        ]);
        let mut w = Workload::new(f.clone(), config());
        w.prepare().unwrap();
        assert!(w.close().is_err());
        assert_eq!(
            f.0.lock()
                .unwrap()
                .calls
                .iter()
                .map(|c| c[0].as_str())
                .collect::<Vec<_>>(),
            ["create", "inspect"]
        );
        w.state = State::Removed;

        let f = Fake::default();
        let config = config();
        let owner = config.owner.clone();
        f.0.lock().unwrap().replies.extend([
            ok(&format!("{id}\n")),
            ok(&format!("{id}\t{owner}\n")),
            ok(""),
            ResultBytes {
                status: None,
                stdout: vec![],
                timed_out: true,
            },
        ]);
        let mut w = Workload::new(f.clone(), config);
        w.prepare().unwrap();
        assert!(w.close().is_err());
        assert_eq!(w.state, State::Unknown);
        let before_drop = f.0.lock().unwrap().calls.len();
        drop(w);
        assert_eq!(f.0.lock().unwrap().calls.len(), before_drop);
    }

    #[test]
    fn malformed_create_output_reconciles_and_workspace_comma_is_rejected() {
        let f = Fake::default();
        let id = "b".repeat(64);
        f.0.lock()
            .unwrap()
            .replies
            .extend([ok("not-an-id\n"), ok(&format!("{id}\n"))]);
        let mut w = Workload::new(f.clone(), config());
        assert!(w.prepare().is_err());
        assert_eq!(w.id.as_deref(), Some(id.as_str()));
        assert_eq!(f.0.lock().unwrap().calls[1][0], "ps");
        w.state = State::Removed;

        let dir = std::env::temp_dir().join(format!("nexus,workload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(
            Config::new(
                &dir,
                &std::env::temp_dir().join("nexus-resolver-test"),
                &format!("example/workload@sha256:{}", "a".repeat(64)),
                &test_nonce(),
                &"f".repeat(64)
            )
            .is_err()
        );
        std::fs::remove_dir(&dir).unwrap();
    }
    #[test]
    fn create_timeout_reconciles_exact_name_and_label() {
        let f = Fake::default();
        let config = config();
        let owner = config.owner.clone();
        let mut s = f.0.lock().unwrap();
        s.replies.extend([
            ResultBytes {
                status: None,
                stdout: vec![],
                timed_out: true,
            },
            ok(&format!("{}\n", "c".repeat(64))),
            ok(&format!("{}\t{owner}\n", "c".repeat(64))),
            ok(""),
            ok(""),
        ]);
        drop(s);
        let mut w = Workload::new(f.clone(), config);
        assert!(w.prepare().is_err());
        assert_eq!(
            w.id.as_deref(),
            Some("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc")
        );
        assert_eq!(w.state, State::Unknown);
        assert!(
            f.0.lock().unwrap().calls[1]
                .iter()
                .any(|x| x == &format!("label={OWNER_LABEL}={owner}"))
        );
        assert!(
            f.0.lock().unwrap().calls[1]
                .iter()
                .any(|x| x == "--no-trunc")
        );
        drop(w);
        assert_eq!(
            f.0.lock()
                .unwrap()
                .calls
                .iter()
                .map(|call| call[0].as_str())
                .collect::<Vec<_>>(),
            ["create", "ps", "inspect", "stop", "rm"]
        );
    }
    #[test]
    fn readiness_is_not_healthy_and_close_verifies_owner() {
        let f = Fake::default();
        let config = config();
        let owner = config.owner.clone();
        let mut s = f.0.lock().unwrap();
        let id = "d".repeat(64);
        s.replies.extend([
            ok(&format!("{id}\n")),
            ok("started\n"),
            ok(&format!("{id}\t{owner}\ttrue\tstarting\n")),
            ok(&format!("{id}\t{owner}\ttrue\thealthy\n")),
            ok(&format!("{id}\t{owner}\n")),
            ok("stopped\n"),
            ok(""),
        ]);
        drop(s);
        let mut w = Workload::new(f.clone(), config);
        w.prepare().unwrap();
        w.start().unwrap();
        assert_eq!(w.state, State::Ready);
        w.close().unwrap();
        assert_eq!(w.state, State::Removed);
        w.close().unwrap();
        assert_eq!(f.0.lock().unwrap().calls[2][0], "inspect");
    }

    #[test]
    fn unhealthy_keeper_is_cleaned_and_never_becomes_ready() {
        let f = Fake::default();
        let id = "d".repeat(64);
        let config = config();
        let owner = config.owner.clone();
        f.0.lock().unwrap().replies.extend([
            ok(&format!("{id}\n")),
            ok("started\n"),
            ok(&format!("{id}\t{owner}\ttrue\tunhealthy\n")),
            ok(&format!("{id}\t{owner}\n")),
            ok("stopped\n"),
            ok(""),
        ]);
        let mut w = Workload::new(f.clone(), config);
        w.prepare().unwrap();
        assert!(w.start().is_err());
        assert_eq!(w.state, State::Removed);
        assert_eq!(
            f.0.lock()
                .unwrap()
                .calls
                .iter()
                .map(|call| call[0].as_str())
                .collect::<Vec<_>>(),
            ["create", "start", "inspect", "inspect", "stop", "rm"]
        );
    }
    #[test]
    fn foreign_resource_is_never_removed() {
        let f = Fake::default();
        let mut s = f.0.lock().unwrap();
        s.replies.extend([
            ok(&format!("{}\n", "e".repeat(64))),
            ok("different\towner\n"),
        ]);
        drop(s);
        let mut w = Workload::new(f.clone(), config());
        w.prepare().unwrap();
        assert!(w.close().is_err());
        assert_eq!(w.state, State::Unknown);
        assert_eq!(f.0.lock().unwrap().calls.len(), 2);
        w.id = None;
        w.state = State::Removed;
    }
}
