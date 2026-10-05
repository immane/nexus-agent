# Nexus Agent Documentation

Nexus Agent is a Rust coding agent designed around very fast startup, low resource usage, and replaceable integrations. General-purpose capabilities can be added through plugins without turning the core into a large framework.

## Implementation Status

Status: documentation repair in progress. The M0 test-only implementation
exists, but **M0 acceptance is not complete**; the checks and measurements
below are recorded historical context, not accepted readiness evidence,
until the coordinator completes verification.

The workspace contains a test-only M0 implementation driven by scripted
fakes (`nexus-fakes`) through one `nexus-runtime` policy boundary.
`cargo build --workspace --release` produces two release binaries over that
shared runtime: `nexus-headless` (1,006,464 bytes macOS / 1,169,136 bytes
Linux) and `nexus-tui` (1,502,336 bytes macOS / 1,719,984 bytes Linux).
The repository also contains engineering guidance and the
[MIT license](../LICENSE). Opt-in OpenAI-compatible live providers and
root-jailed file reads/writes are now implemented; see
[QUICKSTART.md](../QUICKSTART.md) for current wiring and limitations.
External plugins are not implemented, and persistence is an in-memory
ephemeral store that self-identifies as non-durable. Historical
`cargo test --workspace`, `cargo fmt --check`, and clippy runs were recorded
as passing on macOS and Linux (137 tests at the time, 0 failures); because
implementation slices are still landing, those checks are pending
re-verification and are not current evidence.

The numbers below are historical spawn-to-exit characterization runs
(`tools/perf/startup.py`, warm `n=50` per mode unless noted), not the
first-interactive startup definition in the performance design; they cannot
be compared with the 100 ms target and are not acceptance evidence. The
macOS environment was Mac16,10 Apple M4, Darwin 25.3.0 arm64, rustc 1.94.0,
default release profile, no crate features. The Linux numbers were recorded
in a 6.12.76-linuxkit aarch64 container with rustc 1.99.0; no bare-metal
Linux run exists. Compiler versions are environment records only: no exact
compiler pin has been selected yet (the toolchain choice is still pending),
so they establish no stable pin or support matrix.

`nexus-headless "hello"` warm p50 4.7 ms / p95 6.0 ms / max 7.0 ms and
unprepared-cache p50 4.9 ms / p95 5.9 ms / max 10.8 ms; `nexus-tui`
(non-terminal fallback transcript; no PTY here) warm p50 5.4 ms / p95
11.1 ms / max 11.3 ms and unprepared-cache p50 5.0 ms / p95 10.4 ms / max
38.7 ms. The "cold" runs used no cache purge, so they are
unprepared-cache runs, not genuine cold-cache results. RSS results are
method-confounded and not baseline evidence: the harness reports the
cumulative `RUSAGE_CHILDREN` high-water mark, which includes other child
processes such as the cache-purge command and can charge unrelated memory
to the binary. The one-off `/usr/bin/time -l` readings (1,998,848 bytes
headless / 2,195,456 bytes TUI, 0 page faults) are single samples, and the
unreproduced 514 MB harness reading during the first Linux cold batch is
retained as an unresolved anomaly rather than discarded: it shows the
method cannot yet attribute RSS reliably.

Linux numbers (same harness and method, release build in-container):
`nexus-headless "hello"` warm p50 2.0 ms / p95 2.0 ms / max 2.1 ms
(`n=50`) and cold via `drop_caches` p50 2.8 ms / p95 19.8 ms / max 21.2 ms
(`n=20`); `nexus-tui` bare warm p50 2.0 ms / p95 5.0 ms / max 37.5 ms
(`n=50`) and cold p50 6.5-6.8 ms / p95 ~70-73 ms / max up to 169 ms (two
`n=20` runs). Linux child max RSS ≈ 12 MB by the harness and `time -v` on a
cold TUI launch reports 2,136 KB; both carry the RSS confound and small
samples, so neither is a baseline. PTY/TUI-readiness, idle-CPU, streaming,
buffer high-water, and dispatch-overhead numbers do not exist yet. Known
limits: ephemeral-only storage, no real provider/plugin support,
`RunFinished(Failed)` carries no provider detail, disconnect surfaces as
`LimitReached`, and CJK width handling is approximate. No verified
provider/platform compatibility claims beyond this.

## Document Status and Authority

- **Accepted project direction** records requirements confirmed by the project owner, not implemented behavior.
- **Draft design** proposes how to satisfy those requirements.
- **Draft contract** proposes interfaces and invariants to review before implementation. Type names describe semantics, not an existing Rust API, ABI, or wire format.
- **Implemented behavior** must be identified with its implementation and tested scope when it exists.

Follow explicit user requirements and applicable repository guidance. Accepted decisions and contracts take precedence over draft proposals. A newer draft does not silently supersede an accepted decision. Update related documents together when an authorized change affects their assumptions.

## Design

| Document | Purpose | Status |
| --- | --- | --- |
| [00 - Overview](design/00-overview.md) | Goals, scope, and first-release boundaries | Draft design; confirmed requirements identified |
| [01 - Components](design/01-components.md) | Responsibilities, dependency direction, and extension boundaries | Draft design |
| [02 - Execution](design/02-execution.md) | Startup, agent loop, state ownership, and shutdown | Draft design |
| [03 - Model integration](design/03-model-integration.md) | Broad provider coverage without vendor SDK proliferation | Draft design |
| [04 - Tool plugins](design/04-tool-plugins.md) | Built-in Rust tools and cross-language external plugins | Draft design |
| [05 - Performance](design/05-performance.md) | Startup acceptance, resource measurement, and optimization | Draft design; startup goal confirmed |
| [06 - Security](design/06-security.md) | Trust, authorization, side effects, and separate isolation work | Draft design |
| [07 - TUI and headless frontends](design/07-tui.md) | Grok Build-style layout, approvals, history, and script entry point | Draft design; product defaults accepted |
| [08 - API relay](design/08-api-relay.md) | Relay provider plugins and existing-service/local-executor boundaries | Draft design; integration scope accepted |
| [09 - Implementation readiness](design/09-implementation-readiness.md) | Engineering gates, M0 acceptance, and concise-code review | Draft engineering gates |
| [10 - Pi architecture reference](design/10-pi-reference.md) | Observed Pi patterns adopted in principle and explicitly excluded | Draft reference note |

## Contracts

| Document | Purpose | Status |
| --- | --- | --- |
| [00 - Common](contracts/00-common.md) | Shared identifiers, data ownership, errors, limits, and compatibility | Draft contract |
| [01 - Provider](contracts/01-provider.md) | Requests, normalized streaming events, and continuation correctness | Draft contract |
| [02 - Tool](contracts/02-tool.md) | Tool registration, execution, authorization, and outcomes | Draft contract |
| [03 - Commands and events](contracts/03-command-events.md) | Frontend/runtime interaction, ordering, and backpressure | Draft contract |
| [04 - Session store](contracts/04-session-store.md) | Persistence, durability, interrupted operations, and recovery | Draft contract |
| [05 - Configuration](contracts/05-configuration.md) | Configuration snapshots, credentials, permissions, and bounded defaults | Draft contract |

## Decisions

- [00 - Project foundation](design/decisions/00-project-foundation.md): accepted project direction.
- [01 - First-release defaults](design/decisions/01-first-release-defaults.md): accepted permissions, UI, sessions, entry points, authentication, relay scope, and code-simplicity requirements.

## Phase 1 Tasks

| Document | Purpose | Status |
| --- | --- | --- |
| [00 - Overview](tasks/00-overview.md) | M0 scope, task map, and operating rules | Draft task plan |
| [01 - Pipeline](tasks/01-pipeline.md) | Stage order, dependencies, and parallel lanes | Draft task plan |
| [02 - Workflow](tasks/02-workflow.md) | Orchestration steps, handoffs, and verification loops | Draft task plan |
| [03 - Subagents](tasks/03-subagents.md) | Logical roles mapped to `explore` and `general` subagents | Draft task plan |
| [04 - M0 gates](tasks/04-m0-gates.md) | Entry/exit criteria and completion evidence | Draft task plan |
| [05 - TUI brief](tasks/05-tui-brief.md) | Grok Build / Pi interaction research for P5B | Draft research brief |
| [06 - M0 lock](tasks/06-m0-lock.md) | Accepted M0 semantics for P2-P5 (P0 exit) | Accepted for M0; test-only |

## Reading Order

1. Read the overview and accepted decisions for confirmed scope and defaults.
2. Read component and execution designs for boundaries and lifecycle.
3. Read common contracts before the relevant component contract.
4. Read security and performance requirements before introducing integrations or dependencies.

## Before Implementation

Follow [implementation readiness](design/09-implementation-readiness.md) and the [Phase 1 task plan](tasks/00-overview.md). Review the relevant draft contracts before relying on their detailed semantics; accepted product defaults do not automatically accept every proposed interface. Select the toolchain and dependency features for M0, then choose storage/schema/external-protocol details before their respective integrations. Establish Linux and macOS reference environments for measurements. Do not add placeholder crates, runbooks, or validators just because a document mentions a future component.
