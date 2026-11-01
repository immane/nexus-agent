# Performance and Measurement

Status: Draft measurement design; M0 acceptance review in progress. First-release startup should be around 100 ms; ordinary observed launches have a 150 ms hard ceiling, with the owner-approved 200 ms ceiling applying only to verified Linux strict cold ELF-page samples. No complete accepted baseline exists. Owner-approved resource budget (2026-11-02): Linux TUI-process RSS must remain at or below 128 MiB for the scoped process; this is not an allocator-specific limit, excludes tool child processes, and does not define a macOS budget. Historical spawn-to-exit characterization runs are recorded in the [documentation index](../index.md), but they are not readiness evidence: they do not measure the first-interactive definition below and their RSS method is confounded. New PTY, idle-CPU, synthetic streaming, queue-pressure, and dispatch observations are recorded in the [M0 gate evidence](../tasks/04-m0-gates.md); they are not accepted baselines and do not yet satisfy the complete P7 set.

## Startup Definition

Measure from the external launcher starting the host process to the first usable interface: a terminal frame is visible and local user input can be accepted, or a headless entry point can accept a request.

Include local configuration validation, runtime setup, and terminal initialization. Do not count a splash screen, blocked spinner, or buffered input that the application cannot yet handle as ready.

Exclude build time, shell startup before process launch, network completion, model first-token latency, and optional plugin readiness. Report those separately; do not hide mandatory local initialization behind a readiness marker.

On the documented Linux and macOS reference environments, aim for first-interactive startup around 100 ms. User-approved acceptance clarification (2026-11-01): ordinary warm/prepared launches must each be at or below 150 ms. User-approved Linux strict cold ELF exception (2026-11-02): only the separately identified profile that verifies all target ELF dependency pages nonresident may use a 200 ms per-sample ceiling. The 200 ms exception does not apply to ordinary launches or unverified/prepared-cache runs. Report sample count, p50, p95, and observed maximum, retaining every attempt; any valid sample over its profile's hard ceiling fails that profile. These ceilings are not universal hardware guarantees.

## Evidence Status

The recorded macOS and Linux runs characterize spawn-to-exit and must not be promoted to baselines or acceptance evidence:

- The harness measures wall time to process exit, not the first-interactive definition above, so results are not comparable with the 100 ms target.
- RSS is a cumulative `RUSAGE_CHILDREN` high-water mark that includes other harness children such as the cache-purge command; it cannot be attributed to the binary. The unreproduced 514 MB reading is retained as an unresolved anomaly, not dismissed.
- The macOS "cold" runs used no cache purge and are unprepared-cache runs. Linux `drop_caches` runs exist, but with small samples and high variance.
- Current-source PTY first-interactive, idle CPU/RSS (plus macOS per-process wakeup counters), synthetic streaming throughput, runtime control-channel high-water, and fake dispatch have observations under the owner-approved reference profile below, but complete representative/full-path coverage and accepted baselines remain open. See the M0 evidence record.
- Compiler versions were whatever each environment had; no exact compiler pin has been selected yet (the toolchain choice is still pending).

Re-run the profiles and validation plan below once the implementation stabilizes; do not record any number as a baseline before then.

## Measurement Profiles

### Accepted reference profile (owner-approved 2026-11-01)

For review, use this reproducible default-build PTY profile rather than
comparing unqualified launch timings: Linux VM Ubuntu 26.04.1 x86_64 (4 vCPUs,
7.3 GiB RAM, kernel 7.0.0-38) and this macOS host Darwin 25.3.0 arm64 (Apple
M4, 10 logical CPUs, 16 GiB); rustc 1.99.0; locked default features and the
release profile; 120x40 PTY; isolated empty HOME/XDG/config; no credentials,
network, cache purge, demo feature, or provider call. Run 30 fresh processes,
retain every attempt and cleanup outcome, and report spawn-to-observed composer
response plus frame-ready and input-response times. Report warm-cache and
prepared-cache separately; preparation is not “cold” unless the relevant cache
state is verified, and macOS cold preparation remains deferred by owner decision.
Keep idle/resource, fake streaming, queue pressure, and dispatch as distinct
profiles. The owner accepted this reference profile and confirmed the candidate
Linux ED25519 fingerprint through a trusted channel on 2026-11-01. The reference
VM presented that exact key. After the owner installed the local
public key, strict-host-verified public-key login succeeded and remote
validation resumed. Acceptance does not authorize bypassing SSH host-key
verification.

| Profile | What it isolates |
| --- | --- |
| Minimal host/headless | Runtime and core overhead without the terminal or external plugins |
| Coding TUI | Shipped coding tools, provider adapters, and terminal setup |
| External plugin enabled | Host overhead plus startup, idle, and peak usage of the plugin process tree |

Report warm-cache fresh-process startup and cold-cache measurements as distinct profiles. A new process does not imply a cold filesystem cache. Document cache preparation rather than silently using privileged cache resets. **Owner-approved M0 scope adjustment (2026-11-01):** macOS cold-cache measurement is deferred to M5 and remains unverified; Linux strict cold ELF-page evidence and both platforms' warm-cache interactive measurements remain in M0. ELF-page nonresidency alone does not establish cold filesystem metadata. This does not waive reference-method acceptance or the first-release startup target.

## Required Metrics

- Startup p50, p95, and observed maximum, with sample count.
- Uncompressed executable size, distribution size, and build features.
- Idle and peak host memory, identifying the platform-specific measurement method.
- Plugin process-tree memory separately and in the total.
- Idle CPU and background wakeups.
- Input responsiveness, streaming throughput, event-buffer high-water marks, and cancellation latency under output load.
- Local model/tool dispatch overhead with deterministic fake integrations.

Record OS, architecture, hardware, terminal or PTY setup, compiler, build profile, enabled features, configuration size, and workload. Compare like-for-like results; platform memory metrics may not be directly interchangeable.

## Optimization Rules

Keep startup offline and bounded. Defer session materialization, plugin processes, discovery, and indexing. Disabled optional integrations must not initialize background work.

Measure the accepted default: a new conversation, no automatic restoration, no browser login, and no relay probing or local proxy startup. Include the local setup needed for the configured session store; do not hide mandatory persistence work behind a misleading readiness signal. Explicit history restoration and optional relay startup are separate measurements.

Reuse connections and immutable configuration. Prefer direct built-in tool calls, bounded buffers, and ownership transfer over repeated whole-history cloning. Batch presentation deltas without losing text or delaying control traffic.

TUI streaming and maintenance redraws use the configured minimum redraw
interval (33 ms by default). Processed terminal input batches and newly applied
terminal outcomes request an immediate frame instead of waiting for that stream
budget. Terminal outcomes are merged/drained in order before presentation;
completion and cancellation feedback must not wait behind a preceding input
frame. Linux also observes terminal readability through the existing Tokio
reactor, avoiding a full polling interval before an idle user's first key.
Crossterm remains the sole input reader/parser. A separate nonblocking file
description observes stdin without changing its flags; unavailable native
readiness produces a notice and retains periodic polling. No extra input thread
is started. Periodic 20 ms polling remains on both platforms to drain parsed
events, including those left after the 32-event batch bound. Linux input batches
restart that interval and suppress native readiness until the next tick, keeping
continuous input bounded instead of spinning on a readable terminal. macOS
continues to use periodic polling only.
After an input frame, the normal stream budget starts from that frame, avoiding
a duplicate stream redraw in the same iteration. A busy input source can
therefore cause more frames than the stream-only rate; measure that workload
separately from idle and output-only rendering.

Do not introduce a dependency per model vendor, a runtime per component, or an internal service to connect built-in modules. Avoid full-history parsing or layout for every token. Establish a regression baseline before changing allocator, dispatch, or compiler strategies.

Evaluate stripping, link-time optimization, feature reduction, allocator choices, and dispatch representation with measurements. A smaller binary is not automatically faster, and aggressive compiler settings can increase development cost or regress runtime behavior.

Keep optimized code readable. Do not build redundant checking layers, speculative fast-path/fallback trees, or macros merely to make code appear small. Consolidate immutable validation at boundaries without weakening dispatch-time safety.

## Resource Budgets

The owner-approved Linux TUI-process RSS ceiling is 128 MiB for M0 review. It
does not cover the process tree, allocator-specific memory, or macOS. Other
binary-size and platform/process-tree budgets remain unset until representative
complete baselines exist. This does not permit unbounded queues, retained
history, output, or concurrency: the runtime must still have finite limits.

Choose numerical defaults using realistic coding workloads, then document them in the [configuration contract](../contracts/05-configuration.md). Track increases and their functional justification.

## Validation Plan

Build deterministic local workloads for streaming, tool dispatch, slow consumers, and cancellation. Use a PTY-aware startup measurement when a TUI exists. Validate on both Linux and macOS; a successful macOS build is not Linux runtime evidence.

Live model latency is an integration measurement, not the core performance benchmark. No benchmark may silently call paid APIs or launch unapproved plugins.

## Related Documents

- [Execution](02-execution.md)
- [Components](01-components.md)
- [Commands and events](../contracts/03-command-events.md)
- [TUI and headless frontends](07-tui.md)
- [Implementation readiness](09-implementation-readiness.md)
