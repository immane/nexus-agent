# Performance and Measurement

Status: Scoped M0/P7 baseline accepted on 2026-11-02; broader measurement design remains draft. Ordinary warm/prepared launches retain a 150 ms per-sample ceiling; verified Linux strict cold ELF has its separate 200 ms criterion and recorded owner exception. The documented macOS first-execution security-scan sample is separately retained by owner scope adjustment, not claimed to pass 150 ms. Linux TUI-process RSS has a 128 MiB cap, excluding tool children; no macOS cap or allocator-specific guarantee is implied. See the [M0 acceptance record](../tasks/04-m0-gates.md) for exact profiles, artifacts, methods and limitations. Historical confounded spawn-to-exit/RSS measurements are not accepted readiness baselines. Acceptance is not production/security certification or real-model/full-session validation.

## Startup Definition

Measure from the external launcher starting the host process to the first usable interface: a terminal frame is visible and local user input can be accepted, or a headless entry point can accept a request.

Include local configuration validation, runtime setup, and terminal initialization. Do not count a splash screen, blocked spinner, or buffered input that the application cannot yet handle as ready.

Exclude build time, shell startup before process launch, network completion, model first-token latency, and optional plugin readiness. Report those separately; do not hide mandatory local initialization behind a readiness marker.

On the documented Linux and macOS reference environments, aim for first-interactive startup around 100 ms. User-approved acceptance clarification (2026-11-01): ordinary warm/prepared launches must each be at or below 150 ms. User-approved Linux strict cold ELF exception (2026-11-02): only the separately identified profile that verifies all target ELF dependency pages nonresident may use a 200 ms per-sample ceiling. The 200 ms exception does not apply to ordinary launches or unverified/prepared-cache runs. Report sample count, p50, p95, and observed maximum, retaining every attempt; any valid sample over its profile's hard ceiling fails that profile. These ceilings are not universal hardware guarantees.

## Evidence Status

Final review on 2026-11-02 corrected the numerical summary, verified default-build
provenance and produced native archives with architecture validation. The new
30-run macOS batch initially classified as ordinary warm has one valid 815.573 ms composer response
against the 150 ms ceiling (p50/p95 34.166/114.635 ms). All samples remain in
`docs/tasks/evidence/readiness-macos-final-warm-default-30.json`; build provenance
is recorded beside it. M0 closure was requested conditionally on successful
correction and verification; this failure initially prevented closure. It is not
covered by the Linux strict-cold exception or the macOS cold-cache deferral.
That describes the initially blocked review. After exact-path/PID scanner logs
identified about 741 ms of Gatekeeper/XProtect scanning, the owner explicitly
approved reporting this first execution separately from ordinary warm startup.
The 815.573 ms sample remains valid and unchanged, not an invalid/outlier-deleted
sample. A subsequent independent 30-run batch of the same already-scanned
binary passed the unchanged 150 ms ceiling for every attempt (p50/p95/max
32.326/37.895/38.804 ms), with normal cleanup and compiler/build provenance.
This fulfills the owner's conditional M0 closure authorization. Ordinary
over-limit launches cannot be excluded merely because they are isolated;
the separation applies to the documented scanner-correlated first execution.
No security controls were bypassed, and initial execution remains without a
sub-150-ms guarantee. Raw post-scan evidence is
`docs/tasks/evidence/readiness-macos-post-scan-warm-default-30.json`.

The historical macOS and Linux spawn-to-exit runs below must not be promoted to
baselines or acceptance evidence:

- The harness measures wall time to process exit, not the first-interactive definition above, so results are not comparable with the 100 ms target.
- RSS is a cumulative `RUSAGE_CHILDREN` high-water mark that includes other harness children such as the cache-purge command; it cannot be attributed to the binary. The unreproduced 514 MB reading is retained as an unresolved anomaly, not dismissed.
- The macOS "cold" runs used no cache purge and are unprepared-cache runs. Linux `drop_caches` runs exist, but with small samples and high variance.
- The scoped current PTY, idle/resource/wakeup, synthetic streaming, per-layer queue and fake-dispatch observations are now accepted in the M0 record; broader real-model/full-session and complete full-path coverage are not claimed.
- Compiler versions were whatever each environment had; no exact compiler pin has been selected yet (the toolchain choice is still pending).

Re-run relevant profiles when the measured implementation or environment changes;
do not promote the historical confounded numbers to the accepted M0 baseline.

## Measurement Profiles

### Accepted reference profile (owner-approved 2026-11-01)

For review, use this reproducible default-build PTY profile rather than
comparing unqualified launch timings: Linux VM Ubuntu 26.04.1 x86_64 (4 vCPUs,
7.3 GiB RAM, kernel 7.0.0-38) and this macOS host Darwin 25.3.0 arm64 (Apple
M4, 10 logical CPUs, 16 GiB); rustc 1.99.0; locked default features and the
release profile; 120x40 PTY; isolated empty HOME/XDG/config; no credentials,
network, cache purge, demo feature, or provider call. Run 30 fresh processes,
retain every attempt and cleanup outcome, and report spawn-to-observed composer
response plus frame-ready and input-response times. The 2026-11-02 owner
adjustment reports the documented first-execution security scan separately;
the ordinary warm baseline uses the same already-scanned binary without
disabling security checks. Retain both profiles and every attempt. Report warm-cache and
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

For distribution size, record uncompressed installed-file payload and a
compressed archive/image separately only when an actual release artifact and
its packaging format exist; do not synthesize a compression format to fill the
metric. If the default profile has no external plugin enabled, mark plugin
process-tree memory not applicable for that profile rather than substituting
the frontend's RSS. Deterministic loopback provider/tool workflows are useful
composed integration evidence, but must remain labeled synthetic and do not
replace real-model or complete-session measurements.

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
