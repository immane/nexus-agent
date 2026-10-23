# Optional Container Execution

Status: Draft implementation design, not implemented. The project owner requested an optional container backend, explicit startup selection, a mapped workspace, network access, and automatic development operations inside the container. Concrete runtime, image, network enforcement, limits, and protocol choices below are proposals, not accepted security guarantees. Existing local defaults remain unchanged.

## Purpose and Scope

Provide two build variants: local-only by default, and local plus container support through an additive Cargo feature. At startup, select the execution backend independently of the agent's build/plan mode. Keep the provider, frontend, and authoritative runtime on the host; execute every registered container file/process tool inside the session's isolated environment.

The isolation objective is to expose only the selected host workspace, not to prevent access to the container's own operating-system files. Shells and compilers must read the image's libraries and toolchain. Writable container-local home, temporary storage, and caches are permitted. Container execution is not privileged execution and is not a guarantee against kernel, runtime, or VM vulnerabilities.

The first release covers Linux containers through a local Docker Engine endpoint on Linux or Docker Desktop on macOS, for validated arm64/amd64 images. It does not add Podman, remote engines, Windows, Kubernetes, arbitrary extra mounts, port publishing, host credentials, external plugins, or live-provider headless support. Rootless Engine is desirable but must pass the same capability tests before being advertised as supported.

This is a cross-cutting implementation; use the build agent for execution. Do not combine it with unrelated architecture migration work.

## Current Implementation and Integration Boundaries

| Existing location | Current responsibility | Proposed change |
| --- | --- | --- |
| `crates/nexus-core/src/tool.rs` | `ToolPort`, bounded `ToolContext`, cancellation/deadline and approved scope | Preserve the tool execution port; add only a narrow container-scope representation if existing scope cannot encode it safely |
| `crates/nexus-runtime/src/policy.rs` | Strict/development authorization; development resolves host paths | Add explicit container authorization without host filesystem canonicalization or host directory grants |
| `crates/nexus-tools/src/development.rs` | Shared local tool registration for TUI/server | Keep existing factories unchanged; add a feature-gated container factory |
| `crates/nexus-tools/src/fs.rs`, `fs_access/` | Descriptor-bound file operations | Reuse inside a Linux worker rooted at `/workspace`, not on the host |
| `crates/nexus-tools/src/exec.rs` | Mandatory local OS sandbox | Leave local behavior unchanged; container commands use a separate adapter |
| `crates/nexus-tui/src/main.rs` | Startup parsing, session composition, notices | Select backend and own one environment per live session |
| `crates/nexus-server/src/main.rs`, `server.rs` | CLI, `ToolsMode`, per-session composition/cleanup | Add container wiring under existing authenticated real-tools admission |
| `crates/nexus-headless/src/main.rs` | Scripted fake runner | Preserve fake-only behavior; reject container selection rather than implying real execution |
| `crates/nexus-config` | User/provider/mode configuration | No persistent container configuration required in the first release |

The dependency direction remains frontend composition → concrete tools and runtime; runtime → domain types/shared validation. Runtime must not import Docker adapters. Do not introduce a universal execution framework, plugin registry, or permission DSL.

```text
Host: TUI / authenticated loopback server
                 |
         Runtime authorization
                 |
               ToolPort
            /             \
     existing local    container adapters
                              |
                  bounded Docker CLI runner
                              |
                  session workload container
                  /workspace <- selected host directory
                              |
                  isolated network gateway
                              |
                         public internet
```

The gateway is proposed specifically to enforce outbound restrictions that a normal bridge does not provide. It is not a generic orchestration service.

## Build and Distribution

Local remains an unconditional capability; `container` adds a backend rather than replacing local code. Preserve all existing features. Add `container = []` to `nexus-tools`; forward `container` from `nexus-tui` and `nexus-server` to `nexus-tools/container` and the small runtime authorization feature if required. A frontend must gate its own wiring, not assume a dependency's feature implies that frontend was built with support.

Add a separate `container-worker` feature and required-feature Linux worker binary in the existing `nexus-tools` package. It reuses file operations without adding a production crate solely for a helper. Do not enable this worker target through the host frontend's `container` feature. Build the Linux worker during the image build; a macOS host executable cannot run in a Linux image.

Proposed commands, valid only after implementation:

```sh
# Local-only artifacts, unchanged default capability.
cargo build --release -p nexus-tui -p nexus-server --locked --offline

# Both backends. Package-qualified features make forwarding explicit.
cargo build --release -p nexus-tui -p nexus-server --locked --offline \
  --features nexus-tui/container,nexus-server/container
```

Cargo features are additive and may unify across workspace dependencies. Produce and measure local-only and combined release artifacts in separate target directories; do not infer a local-only artifact from a mixed workspace build. Test each frontend independently as well as together.

Use `std::process::Command` with a fixed argument vector to invoke an independently installed Docker CLI; do not add a Docker SDK initially. Never embed Docker Desktop, a VM, or image layers in the Nexus executable, and never download them during Cargo builds. No container check, daemon contact, or image download occurs when the selected backend is local.

Record executable size, dependency closure, build time, warm local startup, container readiness time, image compressed/unpacked size, VM footprint, and cache use separately. No numerical size/startup promise is justified before measurements. Ship named local-only and container-capable artifacts; do not create a container-only build variant initially.

## Startup Interface and Compatibility

Proposed CLI:

```sh
nexus-tui --execution local --tools-root /absolute/project
nexus-tui --execution container --tools-root /absolute/project

nexus-server --tools real --execution container \
  --tools-root /absolute/project --auth-token-env NEXUS_SERVER_TOKEN
```

`--execution local|container` defaults to `local`. Omission preserves existing local, fake/demo, and strict-tools behavior. Container selection requires an explicitly supplied existing `--tools-root` in the first release, so an implicit working directory is never accidentally exposed. The server additionally requires `--tools real`; `--tools fake --execution container` is a usage error. `--strict-tools --execution container` is a usage error initially, rather than an ignored safety flag. Unknown values, missing values, and contradictory/repeated execution selections are rejected before side effects.

A local-only binary recognizes a container request and returns a clear unsupported-build error with the feature/build remedy. It never substitutes local execution or fake tools. Container-only options with local execution are rejected.

Add `--container-image IMAGE@sha256:DIGEST` for an operator-selected compatible workload image. Without it, use a project-maintained digest-pinned default once that artifact exists. Do not publish an example digest as a usable default. The gateway image is pinned by the release, not model-selectable. Validate the image's worker protocol, architecture, user, and expected layout before workspace access. Do not execute a custom image's default entrypoint; supply the controlled worker/keeper entrypoint explicitly. Custom images are trusted operator choices and must not weaken enforced create options.

Missing images fail with preparation instructions by default. An explicit `--container-pull` permits bounded startup pulls of the pinned workload/gateway images after the image names and download/storage impact are displayed. The server accepts this only from its startup operator, never from a session request. No automatic image build, daemon installation, context switching, or privilege escalation occurs.

The TUI shows a persistent `LOCAL` or `CONTAINER` indicator and container readiness/failure state. Its startup notice explains the workspace mapping, network policy, live-write behavior, and possible data upload. The server exposes an additive safe session summary with backend, readiness, and network policy; it must not expose host paths, image credentials, or daemon endpoints to ordinary responses. Supply the model with `/workspace` path instructions, not the host path. The backend cannot change within a live session.

## Filesystem Exposure and Paths

The only user-data bind is the canonical selected directory at `/workspace`, writable for build sessions. Reject the host root, the user's home itself, container-engine control directories, and broad system directories. Validate an existing absolute directory and present the canonical selection before preparation. A project may still contain credentials; canonicalization is not secret detection.

Prefer explicit `--mount` construction, reject input that cannot be safely represented by the selected CLI encoding, and test commas, spaces, quotes, newlines, and leading dashes. Do not interpolate paths into a shell. Disable recursive inclusion of nested host mounts where supported; otherwise fail the isolation capability check. Keep private propagation. Reject unsafe socket/device exposure inside the workspace, including daemon/agent sockets, and do not claim a one-time traversal prevents concurrent insertion. Strict protection against a hostile host concurrently inserting sockets, special files, mounts, or hard links requires a staged workspace or stronger mediation and is outside the live-bind guarantee.

Reject remote Docker endpoints. Resolve and pin one operator-authorized local context/endpoint for the session; Docker Desktop's local VM is supported only through its validated host-sharing mechanism. Do not inherit arbitrary `DOCKER_HOST`, `DOCKER_CONTEXT`, proxy, or credential-helper settings from model/tool input. Reuse only explicitly trusted host Docker configuration needed for local endpoint authentication and image pulls. Never pass that configuration into the workload.

File tool paths are project-relative or explicitly `/workspace/...`; normalize lexically, reject `..` escape, NUL, over-budget/non-UTF-8 input, and absolute paths outside that root. Resolution and descriptor-bound opening happen inside the worker. Runtime must not call host `DirectoryPolicy` on container paths or translate a tool path back into a host file operation. Host absolute paths returned by old history are denied, not guessed or rewritten.

The worker runs read/list/search/write/patch rooted at `/workspace`, retaining bounded UTF-8 output, regular-file restrictions, symlink/race defenses, exact-match patching, metadata rules, and cancellation as far as the current implementation supports. Exec can access container system files and writable home/temp; file tools remain workspace-scoped. There is no external-host directory approval or `write_dir` expansion in container mode. Container exec has its own truthful schema/revision without the local development `write_dir` field; do not silently reinterpret it.

Workspace changes immediately affect the host and survive container removal. No rollback is promised. Two sessions on the same workspace can race; warn about concurrent mappings, retain current patch conflict semantics, and do not claim session containers isolate shared files. Recommend separate worktrees for parallel work. Git worktrees whose `.git` file points outside the mapping may not work; report the limitation rather than mounting the external Git directory silently.

Known local protected-path guards are not a secret barrier in this mode: arbitrary exec can read any file exposed at `/workspace`, including `.env`. This is an explicit backend-specific behavior difference requiring a startup warning and security documentation, not a silent relaxation of local policy. Do not advertise file-tool-only filtering as protection against exec. Host hard-link aliases, repository hooks, socket insertion, malicious dependencies, and host processes consuming changed project files are residual risks. Stronger protection requires a separate staged/copy workspace design.

## Runtime Authorization

Add an explicit container policy bound by trusted composition to a live environment identity and immutable workspace mapping. It auto-authorizes only the host-registered, supported revisions of the six built-in read/list/search/write/patch/exec tools after schema and scope validation. Neither a tool name alone, an external descriptor, nor a model argument can opt into it.

| Agent mode / tool | Container policy |
| --- | --- |
| Build: valid built-in workspace reads and mutations | Automatic |
| Build: valid built-in argv exec in the environment | Automatic, including an explicitly requested `/bin/sh -lc` argv |
| Plan/read-only: built-in reads/list/search | Automatic |
| Plan/read-only: mutation/process/unclassified | Denied without prompting, unchanged |
| Unknown tools or external executors | Not registered by this backend; deny rather than blanket auto-approve |
| Any request to expand host mounts or alter isolation | Denied; requires a new operator-configured session |

Continue enforcing runtime limits, stale-call protection, output sanitization, deadlines, cancellation, and quarantine. Automatic authorization is not removal of validation. Use a bounded domain scope bound to environment identity and logical workspace target; no Docker IDs, host mount strings, secrets, or platform structs belong in core messages. If tool revision changes are needed, update schemas, registry lookup, policy validation, and compatibility tests together; do not weaken the existing M0 revision check globally.

Read-only mode is an admission policy, not historical rollback. Before switching a container session from build to plan, stop and verify removal of the workload and gateway to terminate background commands, then recreate with a read-only workspace bind. Refuse the transition if termination cannot be confirmed. Container-local caches are lost in this transition. A later build switch similarly creates a writable environment with a new identity; old scopes are invalid. Reads must remain usable in a read-only workspace without writing worker metadata there.

## Workload and Image Profile

Run as a fixed non-root user, mapped to a compatible host UID/GID on Linux where supported. Test Docker Desktop ownership separately. Never recursively chown the host workspace, enable sudo, or make root the fallback for a permission error. Explain mismatches and require an operator-selected compatible image/configuration.

Use a read-only image root filesystem, all workload capabilities dropped, no-new-privileges, default seccomp plus available host LSM protection, separate PID/IPC namespaces, no host network, no devices, no published ports, no Docker/SSH-agent sockets, and no extra hosts alias granting host access. Keep the trusted worker executable on the read-only image, outside `/workspace`. The image contains the worker and keeper, common CA certificates, a shell, and a deliberately documented minimal toolchain. No promise that every language/compiler is included.

Provide bounded writable tmpfs mounts at `/tmp` and a container-local home/cache location; cache tools must use these paths. Dependency installation means unprivileged project/user packages (for example project dependencies), not apt/system package installation. System tools belong in an operator-built compatible image. Configure the image PATH, HOME, TMPDIR, locale, and network resolver explicitly. Do not inherit host PATH, provider credentials, shell startup files, proxy secrets, or cloud credentials. Providers continue using host credentials through the existing provider adapter, never through tool environment inheritance.

Proposed initial configurable-at-code defaults are 2 CPUs, 4 GiB memory, no additional swap allowance, 256 PIDs, 512 MiB `/tmp`, and 1 GiB home/cache. Verify actual enforcement on supported engines; unsupported essential controls fail startup. These defaults need workload testing before release. They are not limits on host workspace growth: builds can fill the mapped filesystem. Writable bind quotas are not portable; explicitly document this gap. Keep the keeper idle and logs disabled or tightly bounded so untrusted stdout cannot fill daemon storage outside Nexus's output budget.

## Network Access and Enforcement

Network access is required, but a default Docker bridge is not an internet-only boundary. It may reach host gateways, private services, metadata endpoints, or other containers. Not publishing ports is not proof that the host cannot connect to container services either.

Proposed enforcement is a release-pinned, trusted Linux gateway container per session. The workload shares the gateway's network namespace, while retaining separate mount/PID/IPC namespaces and its non-root capability-free profile. The gateway owns a dedicated bridge endpoint, has no workspace/host-user-data mount, and installs restrictive nftables rules before the workload is created. Only this trusted gateway receives namespace-scoped NET_ADMIN; the workload never does. It must not be privileged or receive the engine socket. This additional component is justified by arbitrary subprocess networking and consistent Linux/Desktop enforcement, not by an imagined plugin requirement.

The gateway initializes a deny-by-default policy, validates the ruleset, then reports readiness through host-controlled bounded inspection. Its filesystem/configuration and policy generation are pinned; no workload-accessible management listener exists. Do not run repository code before policy readiness. A watchdog/health check invalidates the environment if gateway health or policy cannot be verified. Network namespace sharing and firewall behavior are release gates, not assumed portable guarantees.

Policy requirements:

- Allow outbound TCP/UDP to globally routable public destinations, with established reply traffic. Keep normal workload loopback for its own test/dev services; it is not host loopback.
- Deny private, loopback-destination routing to the host, link-local, multicast, unspecified, reserved/non-global addresses, cloud metadata destinations, host gateway addresses, daemon/VM management endpoints, host interface addresses (including public addresses), and sibling session/container networks. Handle IPv4-mapped IPv6 and special transition routes rather than checking only textual prefixes.
- Provide a controlled resolver in the gateway namespace with explicitly public upstreams. Disable unmediated Docker/host DNS forwarding; constrain DNS egress, and apply destination filtering to every connection after resolution so DNS rebinding cannot bypass it. DNS itself remains an exfiltration channel for workspace data.
- Apply equivalent IPv6 filtering or disable IPv6 and verify it cannot bypass the IPv4 rules. Prefer verified IPv4-only operation for the first release rather than an untested dual-stack claim.
- Do not forward unsolicited inbound connections. Keep workload services unpublished and block non-loopback unsolicited ingress, including direct host-to-bridge access where supported.
- Do not modify global host firewall, daemon settings, VPN configuration, or system security settings automatically. If the namespace profile is unavailable or the isolation probe fails, refuse container startup.

Capture relevant host/VM gateway and interface addresses through trusted host inspection, not model input. Network topology/VPN changes can invalidate that snapshot; revalidate before dispatch and suspend/recreate on detected changes. Do not promise perfect detection of transient changes. Public proxies/tunnels and compromised public services can relay traffic elsewhere; this is not absolute isolation from every indirectly reachable private service.

Validate gateway destination classification against a maintained, reviewed non-global address policy. Do not claim nftables accepts a hostname denylist as a complete policy. Implement rules in the gateway image/scripts and test them with controlled listeners and a rebinding DNS fixture. Select/pin exact image dependencies during implementation and record their licenses and maintenance responsibility. If a prototype cannot satisfy these requirements on both platforms, the network design must be reviewed before shipping; silently switching to unrestricted bridge networking is not permitted.

No network-layer rule can prevent uploading files to an allowed public destination. Display that risk on startup. Private registries, corporate proxies, host databases, SSH agents, arbitrary inbound preview ports, and private-network exceptions are out of scope initially; report failures without widening policy. Pulling images is a separate trusted host control-plane operation and may follow operator Docker configuration; it does not grant those credentials or routes to the workload.

## Tool Transport and Bounded Execution

Use one shared session environment owner referenced by the six `ToolPort` adapters. A small container module contains validated create/inspect/exec/stop/remove operations, immutable options, and a bounded CLI runner; it is not a general Docker command passthrough. Resolve the trusted Docker executable once from the operator environment, pin its absolute path, control its environment, and never resolve it from `/workspace` or accept CLI options from tool input.

For file tools, invoke the fixed Linux worker through non-TTY `docker exec` and send a single length-bounded JSON request on stdin. The worker executes one supported operation and returns one versioned, bounded JSON response. Include protocol version, operation, normalized arguments, and output allowance; keep lifecycle/scope authority on the host. Reject duplicate keys, trailing messages, invalid UTF-8, oversized frames, unknown operations, and unsupported versions. Content goes over stdin, not shell interpolation or command-line arguments. Drain stdout/stderr concurrently while retaining only bounded output. Protocol violations fail the call and invalidate the environment when integrity is uncertain.

Exec passes the validated argv array through the CLI's argument boundary with fixed container/user/workdir flags; never join it into a shell command. An explicit shell argv is permitted in build mode. Preserve `/workspace` as working directory per call; shell `cd` or exported variables do not mutate subsequent calls. Files and container-local home persist during a healthy session. Match existing stdout/stderr/output budgets and safe diagnostics. Model-produced IDs or paths never select a Docker resource.

Runtime cancellation/deadline is authoritative. Killing a `docker exec` CLI process does not establish termination of its remote/container process. On cancellation, timeout, worker protocol failure, or uncertain exec transport failure, stop/kill and inspect the entire session workload and gateway under a finite cleanup deadline. Confirm no workload remains before reuse. If cleanup succeeds, mark the old environment invalid and prepare a fresh environment only before a new explicitly submitted run. Report lost container-local state; preserve workspace changes. Never replay the interrupted command.

If the engine becomes unavailable, retain an explicit unknown/quarantined state and owned resource IDs for reconciliation. Do not report rollback, success, or verified termination. The runtime's existing worker quarantine must not be bypassed; additional dispatch is denied until reconciliation. Bound CLI startup, inspect, pull, preparation, execution, output-drain, and cleanup separately, using the remaining call deadline where applicable.

Proposed preparation limits: 2 seconds for local CLI/daemon probes, 120 seconds for image preparation without pull, 10 minutes for explicit pulls, and 5 seconds for cleanup attempts. Timeout means unknown until inspected if a create/start might have reached the daemon. Runtime tool deadlines remain the current configured limits; preparation never extends an executing call silently.

## Ownership, Lifecycle, and Resource Accounting

The frontend session owns one environment handle; adapters share references but cannot create new mounts or transfer ownership. TUI creates it during session preparation after provider/config admission. The server checks feature/endpoint/root prerequisites at startup and prepares environments at authenticated session creation with bounded concurrency, not at anonymous health checks. Failed session creation cleans partial resources before admitting work.

Environment state is explicit: unprepared, preparing, ready, invalidated, cleaning, removed, or unknown/quarantined. Bind every authorization to an environment generation; never reuse old scopes after recreation. Record the actual IDs returned by create and inspect them; resource names are random host-generated correlation labels, not session IDs used directly as paths or authority.

Apply release/installation ownership labels and a per-environment nonce to the workload, gateway, and dedicated network. Reconcile a timed-out create by these exact identities before retrying. A bounded local ownership record may store only cleanup metadata in application storage, never conversation data or credentials. Use exclusive ownership/locking so a second Nexus process cannot clean a live first process's resources. Crashes leave recoverable records; labels alone are insufficient to decide that a live session is abandoned.

Session closure, expiry, application exit, and partially failed preparation trigger bounded explicit cleanup in reverse dependency order: workload, gateway, network, ownership record. `Drop` may request best-effort cleanup but is not the sole lifecycle mechanism. Delete only inspected resources belonging to the exact owned environment. Never invoke global prune, remove user images, or delete the host workspace. On restart, report owned stale resources and offer targeted reconciliation/removal; do not kill resources owned by a live process.

The server currently permits up to 128 runtime sessions; this must not imply 128 workloads at 4 GiB each. Propose a distinct default cap of two live/preparing container environments and one concurrent pull/preparation, with bounded wait or explicit capacity refusal. Configurability and limits must remain operator-owned. Container session expiry releases its environment through the existing cleanup path. Saturated or uncertain cleanup consumes its slot rather than allowing unbounded replacement creation.

No shared persistent dependency volume in the first release: it risks cross-session contamination and hidden disk growth. Reuse pinned immutable image layers; home/cache tmpfs disappears with the environment. Build output deliberately remains in the mapped workspace unless the operator selects another project layout. Document Linux artifacts versus native macOS artifacts and architecture mismatches.

## Implementation Sequence and Review Gates

Start with feature forwarding, CLI validation, explicit backend notices, and a deterministic fake CLI runner. This establishes compatibility without requiring Docker or any downloads. Local regression tests must pass before container wiring is admitted.

Next implement bounded ownership/create/inspect/cleanup and the pinned minimal worker image, then workspace-only file tools and exec behind `ToolPort`. Exercise them in an isolated test environment without advertising release readiness. Runtime container authorization must land with those adapters, never as a blanket allow-all policy ahead of enforcement.

Implement and validate the network gateway before enabling production container startup. Then integrate TUI session transitions and authenticated server session lifecycle, capacity limits, cleanup, and safe metadata. The first shippable backend includes both entry points; fake-only headless stays unchanged.

Release requires separate Linux and macOS evidence for mount/ownership/isolation, networking, cancellation, cleanup, and architecture support. Measure local-only versus combined artifacts and publish exact runtime/image versions and known limitations. Update user-facing defaults/security documentation only when the corresponding behavior exists; this draft does not supersede accepted local policy.

## Validation and Acceptance

Default development tests remain independent of Docker. Use an injected narrow CLI runner/fake process boundary to test argument vectors, controlled environments, malformed/oversized output, partial creates, unavailable daemon, deadlines, reconciliation, and resource cleanup. Do not build an abstraction around hypothetical runtimes. Register new tools/TUI/server integration modules in their existing `tests/behavior.rs` entry points.

Live container tests must be explicit opt-in, use preloaded pinned fixture images by default, avoid paid services, and distinguish unavailable backend denial from actual isolation success. Network-positive tests use explicitly configured controlled public fixtures; isolation tests use deterministic local/namespace fixtures, never real metadata secrets or uncontrolled internal services. Missing Docker must not count as an isolation pass.

| Area | Required evidence |
| --- | --- |
| Build variants | Default local-only and container-capable TUI/server compile independently; helper is not built into host artifacts; feature forwarding/unification is verified |
| Local compatibility | Existing fake, strict, development, provider, plan, approval, server authentication, and sandbox tests remain unchanged in meaning |
| CLI and UI | All option conflicts reject before daemon contact; unsupported feature is explicit; backend indicator matches actual wiring; preparation/pull cancellation is visible |
| Files | All five file tools operate in the container; traversal/symlink swaps and host absolute paths fail; special files, nested mounts, socket exposure, Unicode and patch conflicts covered |
| Scope | No container policy path reaches host file adapters; unknown tool/revision, stale environment, external `write_dir`, mount expansion, and plan exec are denied |
| Credentials | Sentinel provider/host credentials, environment values, agents, home files, engine socket and config do not enter the workload |
| Network | Public TCP/UDP succeeds as promised; private/host/gateway/metadata/sibling destinations fail for direct IP and DNS/rebinding paths; IPv6 bypass and unsolicited ingress fail |
| Processes | Shell, compiler and project package installation work unprivileged; fork/PID/memory pressure is bounded; workspace disk exhaustion remains explicitly unguaranteed |
| Cancellation | Detached child, retained pipes, CLI death, deadline, daemon loss, and concurrent exit produce verified removal or explicit unknown/quarantine, never automatic replay |
| Lifecycle | Partial setup and repeated cleanup are safe; crash recovery never deletes unrelated/live resources; capacity includes preparing/quarantined environments |
| Mode changes | Build-to-plan terminates background work, replaces writable bind with read-only bind, rejects stale scopes, and keeps reads working |
| Platforms | Real Linux Engine and macOS Desktop behavior verified separately; supported image architecture/UID and VM sharing documented |
| Footprint | Executable/dependency delta, unchanged local startup, image/VM/cache cost and cold/warm container readiness measured separately |

Follow [development testing](../testing.md): run affected-package Cargo commands sequentially. Proposed post-implementation validation commands:

```sh
cargo fmt --all --check
cargo nextest run -p nexus-runtime -p nexus-tools -p nexus-tui -p nexus-server --locked --offline
cargo clippy -p nexus-runtime -p nexus-tools -p nexus-tui -p nexus-server \
  --all-targets --locked --offline -- -D warnings
cargo nextest run -p nexus-runtime -p nexus-tools -p nexus-tui -p nexus-server \
  --features nexus-tui/container,nexus-server/container --locked --offline
cargo clippy -p nexus-runtime -p nexus-tools -p nexus-tui -p nexus-server \
  --features nexus-tui/container,nexus-server/container \
  --all-targets --locked --offline -- -D warnings
```

Add explicit worker-target compilation/testing inside each supported Linux image build and dedicated required-backend isolation jobs. Do not use `--all-features` for routine work, since existing low-value features are unrelated. Image/network test job creation and external downloads require the normal repository/operator authorization.

## Documentation and Remaining Approval Boundaries

At implementation time update README, QUICKSTART, SECURITY, testing guidance, component/security designs, affected tool/config contracts, and the accepted defaults decision where appropriate. Explain backend selection, build features, image preparation, internet/data-upload exposure, container-only path/schema behavior, credential differences, resource limits, live-bind risks, recovery, and unsupported workflows. Keep all draft examples clearly separate from working commands until implemented.

Before release, obtain explicit approval for the proposed Docker-only scope, pinned default image/toolchain, trusted gateway with namespace NET_ADMIN, internet-only restrictions versus private-network workflows, non-root/user-package-only installation, local secret-guard behavior difference, and mode-switch loss of container-local state. These are material product/security choices that were not all decided by the initial request. If a choice changes, revise the relevant enforcement and acceptance criteria, not just the wording.

## References

- [Component boundaries](01-components.md)
- [Execution lifecycle](02-execution.md)
- [Security and current local policy](06-security.md)
- [Tool contract](../contracts/02-tool.md)
- [First-release defaults](decisions/01-first-release-defaults.md)
- [Docker Engine security](https://docs.docker.com/engine/security/)
- [Docker bind mounts, recursive mounts, and daemon-host semantics](https://docs.docker.com/engine/storage/bind-mounts/)
- [Docker bridge network behavior](https://docs.docker.com/engine/network/drivers/bridge/)
- [Cargo features and feature unification](https://doc.rust-lang.org/cargo/reference/features.html)

External references were consulted when drafting; actual CLI flags, kernel controls, endpoint handling, and image dependencies must be verified against the selected release versions during implementation. No container execution or isolation tests were performed for this documentation-only change.
