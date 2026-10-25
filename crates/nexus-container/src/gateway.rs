//! Session-owned lifecycle for a dedicated network gateway.
//!
//! This manages gateway resources only; it does not provide container ToolPorts
//! or execute model-controlled commands. Public egress/isolation remains
//! unverified: lifecycle success is not evidence that an image enforces policy.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const PREPARE_TIMEOUT: Duration = Duration::from_secs(20);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(200);
const LABEL: &str = "io.nexus-agent.gateway-owner";
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(2);
const DISCOVERY_OUTPUT_LIMIT: usize = 256 * 1024;
const MAX_DISCOVERED_CIDRS: usize = 64;

/// Discovers host interface networks and best-effort non-default route networks.
/// Topology changes require revalidation; Windows is unsupported. Route discovery
/// is best-effort and must be checked before dispatch (it is not an egress policy).
pub fn discover_host_ipv4_cidrs() -> Result<Vec<String>, ContainerError> {
    #[cfg(target_os = "macos")]
    let cidrs = {
        let ifconfig = discovery_command("/sbin/ifconfig", &[])?;
        let routes = discovery_command("/usr/sbin/netstat", &["-rn", "-f", "inet"])?;
        parse_darwin_networks(&ifconfig, &routes)?
    };
    #[cfg(target_os = "linux")]
    let cidrs = {
        let addresses = discovery_command("/usr/sbin/ip", &["-j", "-4", "addr", "show"])?;
        let routes = discovery_command(
            "/usr/sbin/ip",
            &["-j", "-4", "route", "show", "table", "all"],
        )?;
        parse_linux_networks(&addresses, &routes)?
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let cidrs: Vec<String> = return Err(ContainerError::InvalidCidrs);
    let cidrs = collapse_ipv4_cidrs(&cidrs)?;
    if cidrs.is_empty() || cidrs.len() > MAX_DISCOVERED_CIDRS {
        return Err(ContainerError::InvalidCidrs);
    }
    Ok(cidrs)
}

/// Returns a minimal CIDR cover for the same IPv4 ranges. nftables interval
/// sets reject overlapping elements, and host interface/routes commonly
/// produce nested and adjacent networks.
fn collapse_ipv4_cidrs(cidrs: &[String]) -> Result<Vec<String>, ContainerError> {
    let mut ranges = Vec::with_capacity(cidrs.len());
    for cidr in cidrs {
        let (address, prefix) = cidr.split_once('/').ok_or(ContainerError::InvalidCidrs)?;
        let ip = address
            .parse::<std::net::Ipv4Addr>()
            .map_err(|_| ContainerError::InvalidCidrs)?;
        let prefix = prefix
            .parse::<u8>()
            .ok()
            .filter(|prefix| (1..=32).contains(prefix))
            .ok_or(ContainerError::InvalidCidrs)?;
        let start = u64::from(u32::from(ip));
        let size = 1u64 << (32 - prefix);
        ranges.push((start, start + size - 1));
    }
    ranges.sort_unstable_by_key(|range| range.0);

    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some((_, current_end)) if start <= current_end.saturating_add(1) => {
                *current_end = (*current_end).max(end);
            }
            _ => merged.push((start, end)),
        }
    }

    let mut result = Vec::new();
    for (mut start, end) in merged {
        while start <= end {
            let remaining = end - start + 1;
            let alignment_power = if start == 0 {
                32
            } else {
                u64::from((start as u32).trailing_zeros())
            };
            let size_power = u64::from(63 - remaining.leading_zeros());
            let power = alignment_power.min(size_power);
            let prefix = 32 - power as u8;
            if prefix == 0 {
                return Err(ContainerError::InvalidCidrs);
            }
            let address = std::net::Ipv4Addr::from(start as u32);
            result.push(format!("{address}/{prefix}"));
            start += 1u64 << power;
        }
    }
    Ok(result)
}

fn discovery_command(path: &str, args: &[&str]) -> Result<String, ContainerError> {
    let mut command = Command::new(path);
    command
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let out = run_output_bounded(command, args, DISCOVERY_TIMEOUT)?;
    String::from_utf8(out).map_err(|_| ContainerError::InvalidCidrs)
}

fn run_output_bounded(
    mut command: Command,
    args: &[&str],
    timeout: Duration,
) -> Result<Vec<u8>, ContainerError> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or(ContainerError::Timeout)?;
    command.args(args);
    let mut child = command.spawn().map_err(|_| ContainerError::InvalidCidrs)?;
    let stdout = child.stdout.take().ok_or(ContainerError::InvalidCidrs)?;
    let (tx, rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take((DISCOVERY_OUTPUT_LIMIT + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = tx.send(result);
    });
    let mut output: Option<std::io::Result<Vec<u8>>> = None;
    loop {
        if let Some(status) = child.try_wait().map_err(|_| {
            let _ = child.kill();
            let _ = child.wait();
            ContainerError::InvalidCidrs
        })? && let Some(result) = output
        {
            let bytes = result.map_err(|_| ContainerError::InvalidCidrs)?;
            return (status.success() && bytes.len() <= DISCOVERY_OUTPUT_LIMIT)
                .then_some(bytes)
                .ok_or(ContainerError::InvalidCidrs);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ContainerError::Timeout);
        }
        match rx.recv_timeout(POLL) {
            Ok(result) => output = Some(result),
            Err(mpsc::RecvTimeoutError::Disconnected) if output.is_some() => {
                thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ContainerError::InvalidCidrs);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => (),
        }
    }
}

fn canonical_network(ip: std::net::Ipv4Addr, prefix: u8) -> Option<String> {
    if prefix > 32 {
        return None;
    }
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    let network = std::net::Ipv4Addr::from(u32::from(ip) & mask);
    Some(format!("{network}/{prefix}"))
}
fn insert_network(
    set: &mut std::collections::BTreeSet<String>,
    ip: &str,
    prefix: u8,
) -> Result<(), ContainerError> {
    let parsed = ip
        .parse::<std::net::Ipv4Addr>()
        .map_err(|_| ContainerError::InvalidCidrs)?;
    if !parsed.is_loopback() {
        set.insert(canonical_network(parsed, prefix).ok_or(ContainerError::InvalidCidrs)?);
        if set.len() > MAX_DISCOVERED_CIDRS {
            return Err(ContainerError::InvalidCidrs);
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn parse_linux_networks(addresses: &str, routes: &str) -> Result<Vec<String>, ContainerError> {
    let addr_json: serde_json::Value =
        serde_json::from_str(addresses).map_err(|_| ContainerError::InvalidCidrs)?;
    let route_json: serde_json::Value =
        serde_json::from_str(routes).map_err(|_| ContainerError::InvalidCidrs)?;
    let mut set = std::collections::BTreeSet::new();
    for interface in addr_json.as_array().ok_or(ContainerError::InvalidCidrs)? {
        if interface.get("ifname").and_then(|v| v.as_str()) == Some("lo") {
            continue;
        }
        let infos = interface
            .get("addr_info")
            .and_then(|v| v.as_array())
            .ok_or(ContainerError::InvalidCidrs)?;
        for info in infos {
            let family = info
                .get("family")
                .and_then(|v| v.as_str())
                .ok_or(ContainerError::InvalidCidrs)?;
            if family != "inet" {
                continue;
            }
            let ip = info
                .get("local")
                .and_then(|v| v.as_str())
                .ok_or(ContainerError::InvalidCidrs)?;
            let prefix = info
                .get("prefixlen")
                .and_then(|v| v.as_u64())
                .filter(|p| *p <= 32)
                .ok_or(ContainerError::InvalidCidrs)? as u8;
            insert_network(&mut set, ip, prefix)?;
        }
    }
    for route in route_json.as_array().ok_or(ContainerError::InvalidCidrs)? {
        let Some(dst) = route.get("dst").and_then(|v| v.as_str()) else {
            return Err(ContainerError::InvalidCidrs);
        };
        if dst != "default" {
            let (ip, prefix) = match dst.split_once('/') {
                Some((ip, prefix)) => (
                    ip,
                    prefix
                        .parse::<u8>()
                        .map_err(|_| ContainerError::InvalidCidrs)?,
                ),
                // `ip route ... table all` includes local/broadcast host routes
                // with a bare destination. Treat these conservatively as /32.
                None => (dst, 32),
            };
            insert_network(&mut set, ip, prefix)?;
        }
        // The directly connected network does not necessarily contain its
        // gateway (e.g. a public /32 with an on-link gateway). Deny the gateway
        // address itself, including for the default route.
        if route.get("nhid").is_some() {
            // Resolving nexthop objects requires a second `ip nexthop` query;
            // do not silently omit their gateways from a security denylist.
            return Err(ContainerError::InvalidCidrs);
        }
        if let Some(gateway) = route.get("gateway") {
            insert_network(
                &mut set,
                gateway.as_str().ok_or(ContainerError::InvalidCidrs)?,
                32,
            )?;
        }
        if let Some(nexthops) = route.get("nexthops") {
            for nexthop in nexthops.as_array().ok_or(ContainerError::InvalidCidrs)? {
                if let Some(gateway) = nexthop.get("gateway") {
                    insert_network(
                        &mut set,
                        gateway.as_str().ok_or(ContainerError::InvalidCidrs)?,
                        32,
                    )?;
                }
            }
        }
    }
    Ok(set.into_iter().collect())
}

#[cfg(any(target_os = "macos", test))]
fn parse_darwin_networks(ifconfig: &str, routes: &str) -> Result<Vec<String>, ContainerError> {
    let mut set = std::collections::BTreeSet::new();
    let mut active = false;
    let mut found_interface = false;
    for line in ifconfig.lines() {
        if !line.starts_with(char::is_whitespace) {
            active = line.split(':').next().is_some_and(|name| name != "lo0");
        }
        if active && line.contains("inet ") {
            let fields: Vec<_> = line.split_whitespace().collect();
            let pos = fields
                .iter()
                .position(|v| *v == "inet")
                .ok_or(ContainerError::InvalidCidrs)?;
            let ip = *fields.get(pos + 1).ok_or(ContainerError::InvalidCidrs)?;
            let mask = fields
                .iter()
                .position(|v| *v == "netmask")
                .and_then(|p| fields.get(p + 1))
                .ok_or(ContainerError::InvalidCidrs)?;
            let mask = u32::from_str_radix(mask.trim_start_matches("0x"), 16)
                .map_err(|_| ContainerError::InvalidCidrs)?;
            let prefix = mask.leading_ones() as u8;
            if mask
                != (if prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - prefix)
                })
            {
                return Err(ContainerError::InvalidCidrs);
            }
            insert_network(&mut set, ip, prefix)?;
            found_interface = true;
        }
    }
    if !found_interface {
        return Err(ContainerError::InvalidCidrs);
    }
    let mut saw_routes_header = false;
    let mut saw_columns_header = false;
    for line in routes.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.is_empty()
            || fields[0] == "Routing"
            || fields[0] == "Internet:"
            || fields[0].starts_with("link#")
        {
            continue;
        }
        if fields[0] == "Destination" {
            if fields.get(1) != Some(&"Gateway") {
                return Err(ContainerError::InvalidCidrs);
            }
            saw_columns_header = true;
            continue;
        }
        if fields.len() < 2 {
            return Err(ContainerError::InvalidCidrs);
        }
        saw_routes_header = true;
        let is_default = fields[0] == "default";
        if !is_default {
            let (ip, prefix): (String, u8) = if fields[0].contains('/') {
                let (a, b) = fields[0]
                    .split_once('/')
                    .ok_or(ContainerError::InvalidCidrs)?;
                (
                    expand_darwin_ipv4(a)?,
                    b.parse::<u8>().map_err(|_| ContainerError::InvalidCidrs)?,
                )
            } else if fields[0].starts_with("link#") {
                continue;
            } else {
                let (ip, prefix) = parse_darwin_route_destination(fields[0])?;
                (ip, prefix)
            };
            insert_network(&mut set, &ip, prefix)?;
        }
        // Capture gateways as host routes as well as route destinations. This
        // includes the default gateway, which is often outside a host /32.
        if fields[1].starts_with("link#") {
            continue;
        }
        let gateway = match fields[1].parse::<std::net::Ipv4Addr>() {
            Ok(gateway) => gateway,
            // Darwin's routing table includes resolved neighbor/ARP entries
            // with a link-layer address in the Gateway column. Their IPv4
            // destination was already added above; the MAC is not a route
            // gateway CIDR and must not invalidate the complete snapshot.
            Err(_) if is_darwin_mac_address(fields[1]) => continue,
            Err(_) => return Err(ContainerError::InvalidCidrs),
        };
        insert_network(&mut set, &gateway.to_string(), 32)?;
    }
    if !saw_routes_header || !saw_columns_header {
        return Err(ContainerError::InvalidCidrs);
    }
    Ok(set.into_iter().collect())
}

#[cfg(any(test, target_os = "macos"))]
fn is_darwin_mac_address(value: &str) -> bool {
    let octets: Vec<_> = value.split(':').collect();
    octets.len() == 6
        && octets.iter().all(|octet| {
            (1..=2).contains(&octet.len()) && octet.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

#[cfg(any(test, target_os = "macos"))]
fn expand_darwin_ipv4(value: &str) -> Result<String, ContainerError> {
    let octets: Vec<_> = value.split('.').collect();
    if octets.is_empty() || octets.len() > 4 {
        return Err(ContainerError::InvalidCidrs);
    }
    let mut parsed = Vec::with_capacity(4);
    for octet in octets {
        if octet.is_empty() || (octet.len() > 1 && octet.starts_with('0')) {
            return Err(ContainerError::InvalidCidrs);
        }
        parsed.push(
            octet
                .parse::<u8>()
                .map_err(|_| ContainerError::InvalidCidrs)?,
        );
    }
    while parsed.len() < 4 {
        parsed.push(0);
    }
    Ok(parsed
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join("."))
}

#[cfg(any(test, target_os = "macos"))]
fn parse_darwin_route_destination(value: &str) -> Result<(String, u8), ContainerError> {
    let parts: Vec<_> = value.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return Err(ContainerError::InvalidCidrs);
    }
    let expanded = expand_darwin_ipv4(value)?;
    Ok((expanded, (parts.len() * 8) as u8))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerError {
    InvalidDockerExecutable,
    InvalidWorkspace,
    InvalidImage,
    InvalidCidrs,
    UnsafeDockerContext,
    OwnershipUnresolved,
    DockerFailure,
    WorkloadPreparation(&'static str),
    Timeout,
    CleanupUncertain { owner: String, container: String },
}

impl std::fmt::Display for ContainerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::WorkloadPreparation(stage) => {
                return write!(f, "container workload preparation failed at {stage}");
            }
            Self::InvalidDockerExecutable => {
                "Docker executable must be a trusted absolute executable path"
            }
            Self::InvalidWorkspace => "workspace must be an existing canonical directory",
            Self::InvalidImage => "gateway image must be an immutable image reference",
            Self::InvalidCidrs => "a non-empty bounded list of IPv4 CIDRs is required",
            Self::UnsafeDockerContext => "Docker must use a local default engine context",
            Self::OwnershipUnresolved => {
                "container ownership state is unresolved; explicit reconciliation is required"
            }
            Self::DockerFailure => "Docker gateway lifecycle operation failed",
            Self::Timeout => "Docker gateway operation timed out",
            Self::CleanupUncertain { .. } => {
                "gateway cleanup is uncertain; inspect the named container before retrying"
            }
        })
    }
}
impl std::error::Error for ContainerError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayIdentity {
    container: String,
}

impl GatewayIdentity {
    /// Opaque, generated identity intended only for host-side diagnostics.
    pub fn name(&self) -> &str {
        &self.container
    }
}

pub struct ContainerEnvironment {
    docker: PathBuf,
    docker_config: Option<PathBuf>,
    identity: GatewayIdentity,
    ownership: Option<std::sync::Arc<crate::ownership::OwnedRecord>>,
    container_id: Option<String>,
    healthy: bool,
    deadline: Instant,
    cleanup_done: bool,
}

impl ContainerEnvironment {
    /// Creates a gateway on Docker's default bridge and waits for its exact `healthy` state.
    /// The image is never pulled; callers must arrange for it to exist locally.
    pub fn prepare(
        workspace: &Path,
        docker: &Path,
        image: &str,
        blocked_cidrs: &[String],
    ) -> Result<Self, ContainerError> {
        if !workspace.is_absolute() {
            return Err(ContainerError::InvalidWorkspace);
        }
        let _workspace = std::fs::canonicalize(workspace)
            .ok()
            .filter(|p| {
                p.is_dir()
                    && p.parent().is_some()
                    && std::env::var_os("HOME")
                        .and_then(|home| std::fs::canonicalize(home).ok())
                        .as_deref()
                        != Some(p.as_path())
            })
            .ok_or(ContainerError::InvalidWorkspace)?;
        let docker = validate_docker_path(docker).ok_or(ContainerError::InvalidDockerExecutable)?;
        if !valid_image(image) {
            return Err(ContainerError::InvalidImage);
        }
        if blocked_cidrs.is_empty()
            || blocked_cidrs.len() > 64
            || blocked_cidrs.iter().any(|c| !valid_cidr(c))
        {
            return Err(ContainerError::InvalidCidrs);
        }
        if std::env::var_os("DOCKER_HOST").is_some()
            || std::env::var_os("DOCKER_CONTEXT").is_some()
            || std::env::var_os("DOCKER_TLS_VERIFY").is_some()
            || std::env::var_os("DOCKER_CERT_PATH").is_some()
        {
            return Err(ContainerError::UnsafeDockerContext);
        }
        let docker_config = std::env::var_os("DOCKER_CONFIG")
            .map(PathBuf::from)
            .map(|p| std::fs::canonicalize(p).map_err(|_| ContainerError::UnsafeDockerContext))
            .transpose()?;
        let mut env = Self {
            docker: docker.clone(),
            docker_config,
            identity: new_identity()?,
            ownership: None,
            container_id: None,
            healthy: false,
            deadline: Instant::now() + PREPARE_TIMEOUT,
            cleanup_done: true,
        };
        let docker_desktop_proxy = env.verify_local()?;
        let owner = env.identity.container.clone();
        let nonce = owner
            .strip_prefix("nexus-gw-")
            .ok_or(ContainerError::DockerFailure)?;
        let store =
            crate::ownership::OwnershipStore::process_default().map_err(|error| match error {
                crate::ownership::OwnershipError::StorageUnavailable => {
                    ContainerError::DockerFailure
                }
                crate::ownership::OwnershipError::UnresolvedOwnership => {
                    ContainerError::OwnershipUnresolved
                }
            })?;
        env.ownership = Some(
            store
                .create_record(crate::ownership::OwnershipRecord {
                    nonce: nonce.to_owned(),
                    gateway_name: owner.clone(),
                    gateway_id: None,
                    workload_name: None,
                    workload_id: None,
                    resolver_file: None,
                    docker: env.docker.clone(),
                    docker_config: env.docker_config.clone(),
                })
                .map_err(|_| ContainerError::DockerFailure)?,
        );
        env.cleanup_done = false;
        let mut mutation_failed = false;
        let result = (|| {
            let cidrs = blocked_cidrs.join(",");
            let create_result = env.run(&[
                "create",
                "--pull=never",
                "--name",
                &owner,
                "--label",
                &format!("{LABEL}={owner}"),
                "--env",
                &format!("GATEWAY_BLOCKED_IPV4_CIDRS={cidrs}"),
                "--env",
                if docker_desktop_proxy {
                    "GATEWAY_DOCKER_DESKTOP_PROXY=1"
                } else {
                    "GATEWAY_DOCKER_DESKTOP_PROXY=0"
                },
                "--network",
                "bridge",
                "--cap-drop",
                "ALL",
                "--cap-add",
                "NET_ADMIN",
                "--security-opt",
                "no-new-privileges:true",
                "--read-only",
                "--tmpfs",
                "/run:rw,noexec,nosuid,nodev,size=16m",
                "--pids-limit",
                "128",
                image,
            ]);
            if create_result.is_err() {
                mutation_failed = true;
            }
            create_result?;
            let start_result = env.run(&["start", &owner]);
            if start_result.is_err() {
                mutation_failed = true;
            }
            start_result?;
            env.wait_healthy()?;
            match env.inspect_owned("container", &env.identity.container) {
                Inspect::Owned(id) if valid_container_id(&id) => {
                    env.container_id = Some(id.clone());
                    env.ownership
                        .as_ref()
                        .ok_or(ContainerError::DockerFailure)?
                        .update(|record| record.gateway_id = Some(id))
                        .map_err(|_| ContainerError::DockerFailure)?;
                }
                _ => return Err(ContainerError::DockerFailure),
            }
            Ok(())
        })();
        if let Err(error) = result {
            if mutation_failed {
                // A timed-out/failed Docker mutation may still be completing in
                // the daemon. Keep the durable owner record for explicit
                // reconciliation rather than treating a momentary absence as
                // proof that no resource will appear later.
                env.cleanup_done = true;
                return Err(env.cleanup_uncertain());
            }
            let cleanup = env.cleanup();
            return match cleanup {
                Ok(()) => Err(error),
                Err(e) => Err(e),
            };
        }
        env.healthy = true;
        Ok(env)
    }

    pub fn identity(&self) -> &GatewayIdentity {
        &self.identity
    }
    pub fn is_healthy(&self) -> bool {
        self.healthy
    }

    pub(crate) fn container_id(&self) -> Option<&str> {
        self.container_id.as_deref()
    }

    pub(crate) fn docker_executable(&self) -> &Path {
        &self.docker
    }

    pub(crate) fn trusted_docker_config(&self) -> Option<&Path> {
        self.docker_config.as_deref()
    }

    pub(crate) fn ownership_record(&self) -> Option<std::sync::Arc<crate::ownership::OwnedRecord>> {
        self.ownership.as_ref().map(std::sync::Arc::clone)
    }

    pub(crate) fn quarantine(&mut self) {
        self.healthy = false;
        self.cleanup_done = true;
    }

    pub fn close(mut self) -> Result<(), ContainerError> {
        self.cleanup()
    }

    pub(crate) fn close_for_workload(mut self) -> Result<(), Box<Self>> {
        if self.cleanup().is_ok() {
            Ok(())
        } else {
            self.quarantine();
            Err(Box::new(self))
        }
    }

    fn base(&self) -> Command {
        let mut c = Command::new(&self.docker);
        c.env_clear()
            .env("DOCKER_CONTEXT", "default")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(config) = &self.docker_config {
            c.env("DOCKER_CONFIG", config);
        }
        c
    }
    fn run(&self, args: &[&str]) -> Result<(), ContainerError> {
        run_bounded(
            self.base(),
            args,
            self.deadline.saturating_duration_since(Instant::now()),
        )
        .map(|_| ())
    }
    fn verify_local(&self) -> Result<bool, ContainerError> {
        // An explicitly selected default context is the only accepted engine.
        let out = run_bounded(
            self.base(),
            &[
                "context",
                "inspect",
                "default",
                "--format",
                "{{.Endpoints.docker.Host}}",
            ],
            self.deadline.saturating_duration_since(Instant::now()),
        )?;
        let endpoint = String::from_utf8(out).map_err(|_| ContainerError::UnsafeDockerContext)?;
        let endpoint = endpoint.trim();
        if !endpoint.starts_with("unix://") && !endpoint.starts_with("npipe://./pipe/") {
            return Err(ContainerError::UnsafeDockerContext);
        }
        let out = run_bounded(
            self.base(),
            &["info", "--format", "{{.OperatingSystem}}"],
            self.deadline.saturating_duration_since(Instant::now()),
        )?;
        let operating_system =
            String::from_utf8(out).map_err(|_| ContainerError::UnsafeDockerContext)?;
        if operating_system.trim().is_empty() {
            return Err(ContainerError::UnsafeDockerContext);
        }
        Ok(is_docker_desktop(&operating_system))
    }
    fn wait_healthy(&self) -> Result<(), ContainerError> {
        let deadline = self.deadline;
        loop {
            let out = run_bounded(
                self.base(),
                &[
                    "inspect",
                    "--format",
                    "{{.State.Health.Status}}",
                    &self.identity.container,
                ],
                deadline.saturating_duration_since(Instant::now()),
            )?;
            if out.as_slice() == b"healthy\n" || out.as_slice() == b"healthy" {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(ContainerError::Timeout);
            }
            thread::sleep(POLL);
        }
    }
    fn cleanup(&mut self) -> Result<(), ContainerError> {
        if self.cleanup_done {
            return Ok(());
        }
        self.healthy = false;
        let mut uncertain = false;
        let (kind, name) = ("container", self.identity.container.as_str());
        match self.inspect_owned(kind, name) {
            Inspect::Owned(id) => {
                let id_matches = self.ownership.as_ref().is_none_or(|ownership| {
                    ownership
                        .snapshot()
                        .gateway_id
                        .as_deref()
                        .is_none_or(|expected| expected == id)
                });
                let recorded = self.ownership.as_ref().is_none_or(|ownership| {
                    ownership
                        .update(|record| record.gateway_id = Some(id.clone()))
                        .is_ok()
                });
                if !id_matches || !recorded {
                    uncertain = true;
                } else {
                    let args = vec!["rm", "--force", id.as_str()];
                    if run_bounded(self.base(), &args, CLEANUP_TIMEOUT).is_err() {
                        uncertain = true;
                    }
                }
            }
            Inspect::Absent => {}
            Inspect::Unowned | Inspect::Error => uncertain = true,
        }
        if !uncertain
            && let Some(ownership) = &self.ownership
            && (ownership.snapshot().workload_name.is_some() || ownership.remove().is_err())
        {
            uncertain = true;
        }
        // A failed cleanup is reported to the caller; Drop must not repeat a
        // potentially non-idempotent/ambiguous cleanup attempt.
        self.cleanup_done = true;
        if uncertain {
            Err(self.cleanup_uncertain())
        } else {
            Ok(())
        }
    }
    fn cleanup_uncertain(&self) -> ContainerError {
        ContainerError::CleanupUncertain {
            owner: self.identity.container.clone(),
            container: self.identity.container.clone(),
        }
    }
    fn inspect_owned(&self, kind: &str, name: &str) -> Inspect {
        let args = if kind == "container" {
            vec![
                "inspect",
                "--format",
                "{{.Id}} {{index .Config.Labels \"io.nexus-agent.gateway-owner\"}}",
                name,
            ]
        } else {
            return Inspect::Error;
        };
        let out = match run_bounded(self.base(), &args, CLEANUP_TIMEOUT) {
            Ok(out) => out,
            Err(_) => {
                // A failed inspect is not proof of absence: Docker also fails
                // when the daemon/context is unavailable. Confirm absence with
                // a successful exact-name listing before treating it as gone.
                let filter = format!("name=^{name}$");
                let listed: Vec<&str> = if kind == "container" {
                    vec![
                        "container",
                        "ls",
                        "-a",
                        "--filter",
                        &filter,
                        "--format",
                        "{{.Names}}",
                    ]
                } else {
                    return Inspect::Error;
                };
                return match run_bounded(self.base(), &listed, CLEANUP_TIMEOUT) {
                    Ok(out) if out.is_empty() || out == b"\n" => Inspect::Absent,
                    _ => Inspect::Error,
                };
            }
        };
        let Ok(text) = String::from_utf8(out) else {
            return Inspect::Error;
        };
        let Some((id, owner)) = text.trim().split_once(' ') else {
            return Inspect::Error;
        };
        if !valid_container_id(id) {
            Inspect::Error
        } else if owner == self.identity.container {
            Inspect::Owned(id.to_string())
        } else {
            Inspect::Unowned
        }
    }
}

fn is_docker_desktop(operating_system: &str) -> bool {
    operating_system
        .to_ascii_lowercase()
        .contains("docker desktop")
}

fn valid_container_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}
enum Inspect {
    Absent,
    Owned(String),
    Unowned,
    Error,
}
impl Drop for ContainerEnvironment {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn new_identity() -> Result<GatewayIdentity, ContainerError> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|_| ContainerError::DockerFailure)?;
    let suffix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    Ok(GatewayIdentity {
        container: format!("nexus-gw-{suffix}"),
    })
}
fn validate_docker_path(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let canonical = std::fs::canonicalize(path).ok()?;
    (canonical.is_absolute() && canonical.is_file() && {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            canonical
                .metadata()
                .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        {
            true
        }
    })
    .then_some(canonical)
}
fn valid_image(image: &str) -> bool {
    if image.is_empty() || image.bytes().any(|b| b.is_ascii_whitespace() || b == 0) {
        return false;
    }
    if let Some(digest) = image.strip_prefix("sha256:") {
        return digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit());
    }
    let (_, digest) = match image.split_once('@') {
        Some(v) => v,
        None => return false,
    };
    digest.starts_with("sha256:")
        && digest.len() == 71
        && digest[7..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// Validates an immutable image digest reference without contacting Docker.
pub fn validate_image_reference(image: &str) -> Result<(), ContainerError> {
    valid_image(image)
        .then_some(())
        .ok_or(ContainerError::InvalidImage)
}
fn valid_cidr(value: &str) -> bool {
    let Some((addr, prefix)) = value.split_once('/') else {
        return false;
    };
    if value.matches('/').count() != 1 {
        return false;
    }
    let Ok(ip) = addr.parse::<std::net::Ipv4Addr>() else {
        return false;
    };
    let Ok(prefix) = prefix.parse::<u8>() else {
        return false;
    };
    (1..=32).contains(&prefix) && ip.to_string() == addr
}
fn run_bounded(
    mut command: Command,
    args: &[&str],
    timeout: Duration,
) -> Result<Vec<u8>, ContainerError> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or(ContainerError::Timeout)?;
    if Instant::now() >= deadline {
        return Err(ContainerError::Timeout);
    }
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if Instant::now() >= deadline {
        return Err(ContainerError::Timeout);
    }
    let mut child = command.spawn().map_err(|_| ContainerError::DockerFailure)?;
    let stdout = child.stdout.take().ok_or_else(|| {
        let _ = child.kill();
        let _ = child.wait();
        ContainerError::DockerFailure
    })?;
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut output = Vec::new();
        let result = stdout.take(4097).read_to_end(&mut output);
        let _ = sender.send(result.map(|_| output));
    });
    let mut output: Option<std::io::Result<Vec<u8>>> = None;
    loop {
        let status = match child.try_wait() {
            Ok(status) => status,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ContainerError::DockerFailure);
            }
        };
        if let Some(status) = status
            && let Some(result) = output.take()
        {
            return match result {
                Ok(bytes) if bytes.len() <= 4096 && status.success() => Ok(bytes),
                _ => Err(ContainerError::DockerFailure),
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ContainerError::Timeout);
        }
        match receiver.recv_timeout(POLL) {
            Ok(result) => output = Some(result),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if output.is_none() {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ContainerError::DockerFailure);
                }
                thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn docker_desktop_detection_is_specific_and_case_insensitive() {
        assert!(is_docker_desktop("Docker Desktop"));
        assert!(is_docker_desktop("docker desktop (linux)"));
        assert!(!is_docker_desktop("Docker Engine - Community"));
        assert!(!is_docker_desktop("LinuxKit"));
    }
    #[test]
    fn parses_linux_public_and_private_networks_and_rejects_malformed_data() {
        let addresses = r#"[{"ifname":"eth0","addr_info":[{"family":"inet","local":"203.0.113.9","prefixlen":24}]},{"ifname":"tun0","addr_info":[{"family":"inet","local":"10.8.0.2","prefixlen":24}]}]"#;
        let routes = r#"[{"dst":"default","gateway":"203.0.113.1"},{"dst":"198.51.100.0/24"},{"dst":"10.9.0.0/16"},{"type":"local","dst":"203.0.113.9","dev":"eth0"},{"dst":"default","nexthops":[{"gateway":"203.0.113.2","dev":"eth0"}]}]"#;
        assert_eq!(
            parse_linux_networks(addresses, routes).unwrap(),
            vec![
                "10.8.0.0/24",
                "10.9.0.0/16",
                "198.51.100.0/24",
                "203.0.113.0/24",
                "203.0.113.1/32",
                "203.0.113.2/32",
                "203.0.113.9/32"
            ]
        );
        assert!(parse_linux_networks("not json", routes).is_err());
        assert!(parse_linux_networks(addresses, r#"[{"dst":"bad/99"}]"#).is_err());
        assert!(parse_linux_networks(addresses, r#"[{"dst":"default","nhid":7}]"#).is_err());
    }
    #[test]
    fn parses_darwin_interface_and_routes_and_rejects_malformed_data() {
        let interfaces = "en0: flags=8863<UP>\n\tinet 198.51.100.7 netmask 0xffffff00 broadcast 198.51.100.255\nutun4: flags=8051<UP>\n\tinet 10.8.0.2 --> 10.8.0.2 netmask 0xffffff00\nlo0: flags=8049<UP>\n\tinet 127.0.0.1 netmask 0xff000000\n";
        let routes = "Routing tables\nInternet:\nDestination Gateway Flags Netif\ndefault 203.0.113.1 UG en0\n203.0.113.0/24 198.51.100.1 UG en0\n10.9.0.0/16 10.8.0.1 UG utun4\n127 127.0.0.1 UCS lo0\n169.254 link#4 UCS en0\n192.168.1/24 link#4 UCS en0\n192.168.1.42 0:e0:4c:46:3c:9c UHLWI en0\n";
        assert_eq!(
            parse_darwin_networks(interfaces, routes).unwrap(),
            vec![
                "10.8.0.0/24",
                "10.8.0.1/32",
                "10.9.0.0/16",
                "169.254.0.0/16",
                "192.168.1.0/24",
                "192.168.1.42/32",
                "198.51.100.0/24",
                "198.51.100.1/32",
                "203.0.113.0/24",
                "203.0.113.1/32"
            ]
        );
        assert!(
            parse_darwin_networks(
                "en0: flags=1\n\tinet malformed netmask 0xffffff00\n",
                routes
            )
            .is_err()
        );
        assert!(parse_darwin_networks(interfaces, "truncated route output\n").is_err());
        assert!(parse_darwin_networks(
            interfaces,
            "Routing tables\nInternet:\nDestination Gateway Flags Netif\n192.168.1.42 invalid-gateway U en0\n"
        )
        .is_err());
    }
    #[test]
    fn cidrs_fail_closed() {
        assert!(valid_cidr("192.168.1.0/24"));
        for bad in [
            "",
            "0.0.0.0/0",
            "10.1.2.3",
            "10.0.0.0/33",
            "010.0.0.0/8",
            "::1/64",
        ] {
            assert!(!valid_cidr(bad), "{bad}");
        }
    }

    #[test]
    fn overlapping_and_adjacent_networks_collapse_without_widening() {
        assert_eq!(
            collapse_ipv4_cidrs(&[
                "192.168.1.0/24".to_owned(),
                "10.0.0.0/8".to_owned(),
                "192.168.0.0/24".to_owned(),
                "192.168.1.128/25".to_owned(),
                "192.168.3.0/24".to_owned(),
                "192.168.2.0/24".to_owned(),
            ])
            .expect("valid CIDRs collapse"),
            ["10.0.0.0/8", "192.168.0.0/22"]
        );
        assert!(collapse_ipv4_cidrs(&["0.0.0.0/1".to_owned(), "128.0.0.0/1".to_owned(),]).is_err());
    }
    #[test]
    fn image_requires_immutable_digest() {
        assert!(valid_image(&format!("gateway@sha256:{}", "a".repeat(64))));
        assert!(valid_image(&format!("sha256:{}", "b".repeat(64))));
        assert!(!valid_image("gateway:latest"));
    }
    #[cfg(unix)]
    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }
    #[cfg(unix)]
    #[test]
    fn bounded_command_waits_after_stdout_eof() {
        let started = Instant::now();
        let output = run_bounded(
            shell("exec 1>&-; sleep 0.15; exit 0"),
            &[],
            Duration::from_secs(2),
        );
        assert_eq!(output.unwrap(), b"");
        assert!(started.elapsed() >= Duration::from_millis(100));
    }
    #[cfg(unix)]
    #[test]
    fn bounded_command_zero_timeout_does_not_spawn() {
        let marker = std::env::temp_dir().join(format!(
            "nexus-container-marker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let command = shell("touch \"$0\"");
        assert_eq!(
            run_bounded(command, &[marker.to_str().unwrap()], Duration::ZERO),
            Err(ContainerError::Timeout)
        );
        assert!(!marker.exists());
    }
    #[cfg(unix)]
    #[test]
    fn bounded_command_rejects_output_overflow() {
        assert_eq!(
            run_bounded(shell("head -c 4097 /dev/zero"), &[], Duration::from_secs(2)),
            Err(ContainerError::DockerFailure)
        );
    }
    #[cfg(unix)]
    #[test]
    fn bounded_command_times_out_and_reaps() {
        assert_eq!(
            run_bounded(shell("sleep 5"), &[], Duration::from_millis(50)),
            Err(ContainerError::Timeout)
        );
    }
    #[test]
    fn rejects_remote_engine_endpoint() {
        assert!(
            !("tcp://example.invalid:2375".starts_with("unix://")
                || "tcp://example.invalid:2375".starts_with("npipe://./pipe/"))
        );
    }

    #[test]
    fn gateway_resource_identity_requires_a_full_container_id() {
        assert!(valid_container_id(&"a".repeat(64)));
        assert!(!valid_container_id(&"a".repeat(63)));
        assert!(!valid_container_id(&format!("{}g", "a".repeat(63))));
    }
}
