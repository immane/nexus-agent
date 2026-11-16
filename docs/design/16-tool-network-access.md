# Default Tool Network Access

Date: 2026-11-17.

Status: owner-confirmed product direction; implemented in the current worktree. The owner
approved default networking for local command execution and container tasks.
This document specifies behavior, migration, enforcement, and acceptance
criteria. Local online/offline checks passed on macOS; an opt-in Docker Desktop
check passed for offline gateway readiness and a capability-free process sharing
its namespace. Native Linux sandbox/DNS and container isolation validation are
not established by those results. This change is not installed or released just
because its implementation is present in the worktree.

## 1. Decision and Scope

Make ordinary network-dependent development work possible without disabling the
filesystem sandbox: dependency downloads, public Git operations, HTTP requests,
and connections to development services where the backend permits them.

Separate three independent decisions:

- **Network access:** whether tool subprocesses can establish connections.
- **Filesystem authority:** which files they can read or modify.
- **Execution approval:** whether the runtime authorizes this particular call.

Enabling networking must not imply full host access, broader file grants,
automatic credential inheritance, or automatic approval of every command.

The change applies to real local `host_exec` and the optional container backend
used by TUI and server sessions. Local execution remains the default backend;
container execution remains explicitly selected and feature-gated. Scripted
headless execution remains fake-only. Provider requests and discovery are a
separate host capability and are not disabled by the tool-network switch.

`nexus-server` continues listening only on `127.0.0.1`, with existing bearer,
Host/Origin, and browser security checks. Remote Web access, CORS expansion,
public listeners, and port publishing are outside this proposal.

## 2. Existing Behavior and Compatibility

Baseline before this change, compared with the implementation:

| Component | Previous behavior | Implemented change |
| --- | --- | --- |
| Local macOS exec | Mandatory Seatbelt sandbox; profiles do not allow network operations | Allow outgoing networking by default while retaining filesystem rules |
| Local Linux exec | Mandatory bubblewrap sandbox using `--unshare-all` | Share the host network namespace when enabled; retain other isolation |
| Container tasks | Share a session gateway namespace; policy permits public TCP/UDP and restricts other destinations | Preserve this topology and public connectivity; add an explicit offline mode |
| Exec environment | Cleared environment, controlled PATH, nonexistent HOME; development temp setup | Preserve credential isolation and existing environment behavior |
| Runtime authorization | Development/strict policies and exact-call approvals | Preserve existing rules independently of network selection |

Relevant implementation boundaries are `nexus-tools/src/exec.rs`,
`nexus-tools/src/development.rs`, `nexus-container/src/gateway.rs`, and
`nexus-container/src/workload.rs`. The container policy and its tests live under
`containers/`. Container native Linux isolation validation remains outstanding;
passing policy unit tests does not prove effective network isolation.

The previous security design described disabled local networking. The updated
[Security and Trust Boundaries](06-security.md),
[Optional Container Execution](11-container-execution.md), the configuration
contract, tool descriptions, and operational documentation record the change
together. Older released binaries retain their original behavior.

Missing network configuration will change local behavior from offline to online.
This is an intentional compatibility/security change, not a transparent fix.
Release notes must identify the change and show how to preserve offline behavior
before an operator upgrades. Existing sessions retain their original policy;
restarting the process creates sessions with the new resolved default.

## 3. Reference Agents and Adopted Principles

The following observations are based on official documentation inspected during
design preparation, not a claim about every historical version or deployment.

| Agent | Observed model | Adopt for Nexus |
| --- | --- | --- |
| OpenCode V2 | Shell runs with the host user's filesystem, process, and network authority; permission rules govern tool/command authorization | Network usability without attempting to recognize every networking executable |
| Claude Code | Local shell sandbox is optional and off by default; when enabled, a proxy and domain rules control network access separately from filesystem restrictions | Keep network and filesystem policies independent; consider domain mediation separately |
| Codex CLI | Workspace-write supports an independent network-access setting, disabled by default; approval policy is separate | Enable networking without disabling the sandbox or bypassing approvals |

Nexus deliberately chooses **default enabled** rather than copying Codex's
offline default. It also keeps its existing mandatory local filesystem sandbox
rather than copying OpenCode's full host shell authority. It does not adopt
Claude Code's proxy, credential injection, automatic unsandboxed retries, or
domain approvals in this first implementation.

References:

- [OpenCode V2 permissions](https://opencode.ai/v2/docs/permissions)
- [Claude Code sandboxing](https://code.claude.com/docs/en/sandboxing)
- [Codex network and approval notes](https://github.com/openai/codex/blob/main/codex-rs/skills/src/assets/samples/imagegen/references/codex-network.md)
- [Codex sandbox configuration types](https://github.com/openai/codex/blob/main/codex-rs/config/src/types.rs)

These are moving references. Recheck platform-specific backend APIs during
implementation; documentation about another agent is not evidence that Nexus
enforces the same boundary.

## 4. Effective Policy

Use a small two-state operator setting, not a new permission language:

- `enabled`: default; permit networking according to the selected backend.
- `disabled`: deny external IP networking for tool subprocesses.

The states intentionally do not promise identical address reachability:

| Destination or capability | Local, enabled | Container, enabled | Disabled |
| --- | --- | --- | --- |
| Public DNS and outgoing TCP/UDP | Host routing and OS policy | Existing gateway policy | No external IP access |
| Host loopback / development services | Reachable subject to OS policy | Workload/gateway loopback only, not host loopback | Sandbox-local loopback may remain; host loopback must not be reachable |
| LAN, VPN-private services, host services | Reachable subject to host routing | Retain existing host/LAN restrictions | Blocked |
| Cloud metadata endpoints | No separate Nexus network restriction | Retain current non-global/metadata restrictions | Blocked |
| IPv6 | Host capability and routing | Remains denied by the gateway | No external IPv6 access |
| Incoming services | Local namespace limitation described below | No published ports; keep gateway input restrictions | No externally reachable listener |

For local Linux, sharing the host network namespace also permits a subprocess to
bind listeners wherever the host OS permits. This first implementation must not
claim outbound-only enforcement or automatic loopback-only binding. Tools should
be instructed to bind development servers to loopback, but instructions are not
an OS boundary. macOS policy should permit outgoing connections without adding
blanket inbound permission; document and test the resulting platform difference.
If a command needs inbound service support that the backend cannot grant, fail
explicitly rather than rerun unsandboxed. Cross-platform inbound control is a
separate follow-up, not a reason to remove namespace/filesystem protection.

Default networking permits arbitrary readable project data to leave the machine.
Known-path credential masking does not prevent upload of project source, custom
secrets, or values obtained from reachable services. Local online mode is for a
trusted-user development environment, not hostile code containment. Operators
needing a restricted network should select offline mode or an externally
controlled environment; container isolation remains experimental.

## 5. Configuration, Ownership, and Lifetime

`tools.network_access` is a boolean in the existing operational settings
layers, defaulting to `true`. It is not inserted into
the provider/model configuration merely because that JSON is edited by Web UI.

Operational overlay:

```json
{
  "revision": 1,
  "defaults": {
    "tools": { "network_access": false }
  }
}
```

The equivalent explicit CLI selection for TUI/server is
`--network enabled|disabled`. Use existing settings precedence: built-in default,
system base, ordered explicit overlays, then the CLI override. Reject invalid
values and conflicting duplicate selections rather than treating them as online.
Document that this is a tool-network setting, not an application-wide offline
mode. Do not introduce an environment-variable override in the first version.

Resolve the value once at frontend startup and bind it to each session's tool
registrations and container lifetime. No mutable global switch or per-call
model-supplied network flag is needed. No hot reload or Web mutation endpoint is
required. Changing the selection requires restart and new sessions; restoring a
transcript uses the new session policy, never restores old permission grants,
and displays the current effective policy.

Only explicitly selected operator settings files participate. Do not discover
and load an arbitrary repository policy that could turn networking back on.
Existing overlays are configuration, not tamper-proof enterprise enforcement;
do not advertise this precedence as a managed-policy lock.

Render the effective backend and network mode in startup diagnostics and session
information, for example `local / network enabled (host)` or
`container / network enabled (public egress policy, experimental)`. Keep these
labels metadata-only, without tokens, proxy URLs, or request contents.

## 6. Local Backend Changes

Extend existing sandbox construction with the resolved network value. Preserve
existing constructor callers through deliberate migration rather than an
implicit fallback that silently ignores the setting.

On macOS, add the narrowly required outgoing network permissions to strict and
development profiles when enabled, including the platform DNS mechanism.
Disabled profiles retain the existing denial. Probe initialization with the same
filesystem/environment policy used for execution. Do not contact a public host
in the startup probe; functional connectivity tests use local fixtures.

On Linux, enabled execution may use `--unshare-all` with the documented explicit
network-sharing override, after verifying its behavior against the supported
bubblewrap version. Preserve PID isolation, session/process cleanup,
`--die-with-parent`, bind ordering, writable roots, and protected-path masks.
Disabled execution keeps a private network namespace without an external route.
Check DNS resolver and certificate availability without granting broad writes
to `/etc` or importing the user's credential/configuration directories.

Both modes retain the mandatory sandbox availability probe and fail closed when
the required backend cannot initialize. A network or package-manager failure
must not trigger unsandboxed fallback, a broader filesystem grant, or an
automatic replay of the command.

## 7. Container Backend Changes

Keep gateway ownership, workload namespace sharing, health/readiness gates,
read-only resolver mounts, capability dropping, and session cleanup. Online mode
does not use `--network host`, publish ports, mount the Docker socket, or inject
host credentials. Preserve DNS mediation, host/LAN deny ordering, IPv6 denial,
and the existing narrow Docker Desktop routing exception.

Offline mode makes external access unavailable from both workload and
gateway. The gateway starts with `--network none`; the existing local
`gateway.test` DNS and nftables readiness checks do not require external
DNS/public connectivity, so no gateway image or worker-protocol change is needed.
Do not merely tell the workload not to connect while its shared namespace still
has an external route. Validate both IP families, direct IP, DNS, and gateway
address access. No fallback to an unrestricted bridge is allowed on failure.

The resolved mode belongs to the session gateway/workload owner. Never mutate
the gateway of an active session to apply another session's setting. If a
rebuilt gateway image is required, document image/protocol compatibility,
require the matching image explicitly, and reject incompatible admission. Do not
silently pull images or rebuild infrastructure at session startup. Container
offline mode must land and pass its gates before advertising a shared switch
that claims to work across both backends.

## 8. Credentials, Proxies, and Developer Workflows

Preserve the cleared exec environment and known-path credential protections.
Do not forward server bearer tokens, provider keys, cloud credentials,
`SSH_AUTH_SOCK`, or the whole host environment. Do not automatically mount
credential directories or weaken protected-path masking for private Git access.

Initial supported workflows are public/anonymous operations within existing
filesystem grants. Private repositories, authenticated registries, enterprise
certificate stores, and proxy authentication need explicit later credential
design. Network availability alone is not authentication availability.

Do not automatically inherit `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, or
`NO_PROXY`. Local host routing/VPN still applies; container DNS remains the
gateway's policy. Proxy-only environments may therefore need additional
operator configuration in a later change. Diagnose this limitation honestly
rather than silently importing proxy credentials or bypassing verification.

Package managers can require writable caches outside the project. Use existing
project/temp locations and approved write scopes; do not grant the entire home
directory. Downloads may execute lifecycle scripts, update lockfiles, consume
disk space, or make billable requests. Network enablement is not proof that an
installation is harmless, reversible, or authorized by the task.

## 9. Authorization and Failure Semantics

Development-mode exec remains automatic within its existing scope; strict-mode
exec still requires exact-call approval. Offline mode is not overridden by a
normal execution approval. Read-only agent mode must not gain command execution
merely because a command looks like a download or because networking is enabled.

Do not implement network enforcement using executable names, URL regexes, or
shell-prefix approvals. An interpreter, build script, or subprocess can use the
network too. OS/namespace policy owns enforcement; runtime owns call approval.

Preserve bounded output, deadlines, cancellation, and worker admission. A failed
connection may be DNS, TLS, routing, remote rejection, or policy denial; report
only what is actually known. Never claim a firewall blocked a timeout without
evidence. Keep existing execution-result schemas unless a concrete diagnostic
requires an additive field.

Cancellation does not recall uploaded data, roll back downloads, or prove a
remote write did not happen. Preserve uncertain effects and do not automatically
retry package publication, pushes, API writes, or any other uncertain command.
No per-destination traffic logger or HTTPS interception is introduced.

## 10. Validation and Release Gates

Configuration tests must cover the online default, explicit offline overlay,
CLI precedence, invalid input, partial old settings, effective-config output,
and session-policy immutability. Provider selection and Web listener behavior
must remain unchanged. Add integration modules to existing behavior entrypoints.

Use deterministic local TCP/UDP fixtures for ordinary regression tests. For
local enabled mode, verify the actual sandbox can reach a host listener and
that networking also works in a spawned child. For disabled mode, prove the host
listener is reachable by a control process but unreachable by the sandbox. Do
not use an unbound port as proof of denial. Exercise IPv4 and IPv6 where
available; distinguish environmental absence from policy rejection.

For both local modes, retain adversarial coverage for protected paths,
project/external writes, symlinks, missing backends, cancellation, and detached
descendant uncertainty. Verify strict approvals are still required and model
arguments cannot widen the mode. Check Linux resolver mounts and macOS DNS;
never disable TLS verification to make a test pass.

Container checks must cover online policy preservation, offline route absence,
readiness, namespace ownership, gateway failure, cross-session separation, and
cleanup. Host/LAN/metadata/IPv6/DNS isolation claims require actual supported
host tests, not only command-argument assertions or Docker Desktop smoke tests.
Public DNS/HTTPS probes and image builds remain explicit opt-in operations;
default tests do not upload private code or require paid services.

Run affected-package checks sequentially per [testing guidance](../testing.md),
including configuration, tools, container, TUI, and server packages actually
changed. Use optional container features when modifying those paths. Release
evidence must list the host/platform, backend versions, network mode, commands,
results, skips, and limitations. Passing on macOS does not validate Linux;
passing on Docker Desktop does not validate native Linux Engine isolation.

Implementation proceeds through the existing boundaries: configuration and
composition, local backend enforcement, container offline enforcement, then
documentation and platform validation. Publish the default change only when the
offline escape hatch works for every advertised backend. Preserve a rollback
path through an explicit offline setting or the previous release; do not mutate
an active process's network policy to perform a rollback.

## 11. Deferred Work and Completion Criteria

### Worktree validation snapshot (2026-11-17)

- `cargo nextest run -p nexus-config -p nexus-tools -p nexus-container -p nexus-server -p nexus-tui --locked --offline -j 4`: 1,027 passed; 13 ignored/opt-in tests skipped.
- The same affected-package suite with `--features nexus-server/container,nexus-tui/container`: 1,035 passed; 13 skipped.
- A separate opt-in `offline_gateway_and_shared_namespace` run passed on Docker
  Desktop with a cached immutable gateway image; owned gateways were removed.
- macOS checks covered strict/development TCP and UDP success/denial, IPv6 TCP,
  spawned-child policy inheritance, and protected development credential reads.
- A Linux amd64 Debian container passed affected configuration/tools/container
  `cargo check --all-targets` and the network-namespace argument-construction
  regression. It did not have a provisioned bubblewrap backend; this is compile
  and construction evidence, not native Linux sandbox enforcement evidence.
- Affected-package Clippy with container features and `-D warnings`, formatting,
  39 Web Node tests, and 10 container policy Python tests passed.
- Public DNS/HTTPS, native Linux sandbox/resolver behavior, native Linux Engine
  isolation, authenticated registries/proxies, and exhaustive container
  host/LAN/metadata denial were not validated by this change. No installation,
  server restart, release publication, or live model request was performed.

### Follow-up boundary

Domain allowlists, an authenticated proxy, TLS interception, credential brokers,
per-call network escalation, live session policy edits, container LAN exceptions,
port forwarding, application-wide offline mode, and enterprise policy locks are
separate proposals. Do not add them speculatively to this implementation.

The change is complete when both advertised backends have working online and
offline selections, existing filesystem/approval constraints still hold, the
effective policy is visible, migration documentation names the new default, and
the platform-specific tests substantiate every advertised guarantee. Remaining
container isolation gaps and local host-network reachability must stay explicit.
