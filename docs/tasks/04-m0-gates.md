# M0 Gates and Evidence

Status: Acceptance review in progress; **M0 is not complete**. Functional and
platform checks below have current evidence. The scoped buffer bounds/visible-
loss and startup side-effect gates were accepted by the owner on 2026-11-02 with
stated measurement limitations. Portions of the accepted P7 measurement set
remain open. Historical RSS evidence is confounded and must not be promoted to
a baseline.

**Owner-approved scope adjustment, 2026-11-01:** macOS cold-cache startup
measurement is deferred from M0 acceptance to M5; it remains unverified and is
not a passing result. This does not defer Linux cold-cache evidence, macOS or
Linux warm-cache interactive startup, reference-method acceptance, or any other
M0 buffer, startup-side-effect, or performance requirement.

**User-approved startup threshold clarification, 2026-11-01:** around 100 ms
remains the first-interactive target. Ordinary warm and prepared launches have
a hard observed-launch ceiling of 150 ms on the accepted reference environments;
every valid ordinary-launch sample must meet it. A p95 above 100 ms is no
longer by itself a failure. Report p50/p95/maximum and preserve every attempt.
Historical notes below that classify samples above 100 ms as failures record
the criterion in effect at that time; this clarification supersedes it for the
current acceptance review. The separately approved strict cold ELF exception is
recorded below and does not waive other P7 evidence.

**User-approved Linux cold ELF threshold, 2026-11-02:** the separately reported
strict cold-ELF profile may use a 200 ms hard ceiling per valid sample, provided
`mincore` verifies all target ELF dependency pages nonresident immediately
before each launch. This exception applies only to that profile; ordinary warm
and prepared launches retain the 150 ms ceiling. Preserve and report all
attempts, and do not treat this threshold change as proof that the default
launcher or filesystem metadata coldness has been validated.

**Owner acceptance exception, 2026-11-02:** the Linux strict cold ELF-page gate
is accepted for M0 based on the verified 30-run in-chroot `setpriv` profile.
This acceptance explicitly waives the direct-user chroot profile's single
202.644 ms maximum against the 200 ms criterion, and accepts the chroot profile
as scoped ELF-page evidence without claiming default-host-launcher equivalence
or cold filesystem metadata. The 202.644 ms measurement remains an over-limit
sample in the record; it is not reclassified as meeting 200 ms. This exception
does not change the ordinary-launch 150 ms ceiling or accept any other open M0
gate.

**Owner acceptance, 2026-11-02:** the startup side-effect gate is accepted for
the recorded M0 scope: Linux default no-`--tools-root` and configured-
`--tools-root` startup traces, Linux credential access fixtures, macOS libc
observation with a submission positive control, and cross-platform PTY history
recovery cases. This does not claim coverage of raw syscalls, direct environment
enumeration, every configuration/profile, or effects after tool submission;
those are accepted scope limitations, not evidence of absence.

## Acceptance decision — 2026-11-02

**Not accepted.** The macOS cold-cache deferral is in effect and is not a
remaining M0 blocker. The following independent gates remain open:

| Gate | Current evidence | Remaining requirement |
| --- | --- | --- |
| Buffer bounds and visible loss | Owner-accepted scoped pass (2026-11-02): implemented per-layer logical caps, individual/whole-run queue peaks, terminal reserve, and real-tool outbox loss visible through runtime, HTTP SSE+snapshot, headless, and TUI on Linux/macOS | No further requirement for the scoped M0 gate; allocator-level memory and simultaneous all-layer high-water were not measured and are not claimed |
| Startup side effects | Owner-accepted scoped pass (2026-11-02): isolated PTY tests and recovery on both platforms; Linux credential fixture and current-release traces with/without `--tools-root`; macOS libc hooks with submission positive control | No further requirement for the scoped M0 gate; raw syscall/environment enumeration, tool-submission behavior, and every config/profile are not claimed |
| P7 reference and measurements | Owner-approved reference profile; two-platform warm observations, idle CPU/RSS, macOS per-process wakeup counters, queue/dispatch samples; Linux ordinary prepared-cache batches below 150 ms; strict Linux cold ELF gate accepted by owner exception; Linux and macOS loopback-stream resource observations, including paired finite 480-chunk and Rust patch-style visible-completion measurements; per-layer buffer high-water evidence accepted within the scoped buffer gate; macOS uncompressed installed payload measured | Remaining representative workload resource/throughput coverage and complete P7 baseline review, including a compressed release artifact size (no archive currently exists); patch-style fixture is synthetic and does not exercise tool execution or a real model; simultaneous all-layer buffer peaks, cold filesystem metadata, and default-host-launcher equivalence are accepted limitations of the scoped evidence |

### P7 closure work versus accepted limitations

The remaining P7 work is to assemble a comparable, reviewable baseline from
evidence that is currently split across independent probes, and to fill the
following gaps without treating fixtures as production traffic:

- Run a bounded, deterministic coding workflow that combines the TUI, a local
  provider fixture, at least one read-only local-tool dispatch, and the final
  streamed response. Record the workflow outcome and timing plus TUI RSS on both
  reference platforms. The current Rust patch-shaped stream exercises rendering
  only; separate fake-dispatch tests do not measure the composed workflow.
- Measure input responsiveness and cancellation as part of that output-loaded
  workflow (or explicitly identify a paired existing observation); current
  stream completion and cancellation probes are separate profiles.
- Record the install payload for Linux using the actual installer build feature
  set. macOS currently has an uncompressed installed-file total; the repository
  has no compressed release artifact, so report that metric as unavailable
  unless a real artifact is produced rather than inventing an archive format.
- Assemble per-platform provenance and the required P7 metrics into one baseline
  review, preserving platform-specific RSS/wakeup methods and declaring metrics
  that do not apply to the default shipped profile.

These do not reopen the owner-accepted buffer and startup-side-effect gates.
Simultaneous all-layer buffer peaks, allocator high-water, macOS cold-cache
startup, Linux cold filesystem metadata, and default-host-launcher equivalence
remain explicit accepted limitations/deferred scope, not hidden claims of this
baseline.

The bounded-buffer inventory has finite limits in the implementation, but that
alone is not acceptance evidence: runtime data/control receivers are 1,024/128
events; the control outbox is capped at 256 entries and 1 MiB with terminal
reserve; provisional usage publication is separately rate/count bounded; HTTP
pending SSE delivery is capped at 4,096 events/1 MiB with a bounded oversized
event slot, and each session retains at most two stream delivery records; TUI
reordering is bounded to the combined channel capacities plus 256 and retained
presentation has per-entry and total-item limits. Existing tests cover many
individual limits and loss notices. Under the owner-accepted M0 scope, these
logical caps and exercised loss notices satisfy the bounded-buffer gate.
Simultaneous full-run high-water across these paths and proof that every
transport/frontend surfaces every possible loss mode were not measured and are
not claimed.

The TUI presentation store itself is capped at 128 entries, 8,192 bytes per
entry, and 256 KiB total retained presentation bytes; titles are capped at 256
bytes. These are logical retained-content bounds, not allocator/RSS ceilings:
container overhead, active render buffers, event payload copies, and the other
queues are not included. Queue capacities are likewise not a measured
simultaneous full-run high-water. The real-tool exhaustion tests prove visible
terminal/truncation behavior at the exercised failure boundary, but do not
replace representative stream-load measurements or an end-to-end high-water
trace across all layers. Those measurement limits are accepted for the scoped
buffer gate; representative P7 workload coverage remains independently open.

Linux representative streaming resource observation (same release binary
SHA-256 as the ELF experiment) ran 10 valid 3-second samples. A local loopback
Chat Completions SSE fixture delivered 2,551–2,628 chunks per run (850–875
fixture chunks/s); TUI process RSS high-water observations were 6,944–7,052
KiB, CPU was 10.0–12.6% over the sample windows, and all runs reached the
observed cancellation terminal. The sampler read `/proc/<pid>/status` and
`/proc/<pid>/stat`. In this recorded batch, continuous PTY readability caused
opportunistic sampling more often than the requested 20 ms interval; RSS
high-water uses kernel `VmHWM`, while current RSS is sampled. CPU uses 100-Hz
process ticks across the stream window, so short-window CPU percentages have
10-ms accounting granularity. The provider chunk rate is
the local fixture's emission rate, not a measured application event/render
throughput. This is process RSS, not allocator-specific peak, and excludes any
separate child processes. The observed maximum (7,052 KiB) is below the
user-approved Linux TUI-process RSS cap recorded below; this only passes the
measured workload against that scoped cap. Raw JSON is retained as
`evidence/stream-resource-linux-10x3s.json`; the opt-in local-only harness is
`tools/perf/measure_tui_stream_resources.py`. This observation is not a complete
P7 baseline. On 2026-11-02 the owner approved a 128 MiB hard RSS cap for
the Linux TUI process (about 1.7% of the 7.3-GiB reference VM), providing
substantial headroom over this stream-only observation while remaining a small
share of the reference host. The cap is process RSS, not allocator memory, does
not apply to macOS, and excludes tool-child memory, which needs a separate
budget/measurement. The Linux loopback stream sample is below the cap; full P7
resource acceptance remains open pending broader representative workloads.

The same loopback SSE workload was measured on macOS using the release TUI
binary (`adcec2e3fb2b8d24751185195f6ddaa2132b43561041c9c1640d1486ce3c2c91`):
10/10 valid 3-second windows, each followed by an observed cancellation
terminal. With 100 ms sampling, each run produced 28–29 `/bin/ps` samples;
per-run sampled RSS maxima ranged from 5,968 to 6,592 KiB. Fixture emission
ranged from 719.0 to 789.6 chunks/s. macOS `ps %cpu` readings were retained in
the JSON but are not a CPU delta over the window. As on Linux, fixture emission
is not measured TUI consumption/render throughput; RSS is sampled process RSS,
not a kernel or allocator high-water mark, and no macOS cap is inferred from
the Linux budget. The final sampler version was separately smoke-tested; its
five sample gaps were 108.9–114.0 ms for a requested 100 ms interval. Raw batch
JSON is retained at
`docs/tasks/evidence/stream-resource-macos-10x3s-paced.json` (SHA-256
`64ea177a45205d8ed64a45b6faf72bbc5a22f6741ee944c7cefee9071ddb0d11`). An
earlier unpaced batch is retained outside the repository as invalid diagnostic
data and is excluded. These observations add macOS process-resource evidence,
but do not establish representative application throughput or close the full
P7 baseline.

A follow-up instrumented the PTY screen for monotonically increasing numbered
stream content. In 10/10 valid 3-second macOS runs, the visible marker stopped
at `resource-000509` while the loopback fixture continued emitting; the
first-to-last visible-marker advance rates were 665–756 chunks/s (median 735),
but covered only the first 0.61–0.69 seconds of each window. The original
diagnostic recorded a combined truncation flag that did not distinguish the
`presentation-truncated` viewport banner from the per-entry byte-cap marker, so
it cannot establish which bound was reached. The run demonstrates visible
progress only up to the observed marker; it is not sustained application
throughput, and the marker rate must not be extrapolated over the full 3-second
stream. The batch and diagnostic JSON are retained as
`docs/tasks/evidence/stream-resource-macos-visible-10x3s.json` and
`docs/tasks/evidence/stream-resource-macos-visible-truncation-smoke.json`
(SHA-256 `ac34a22d30a31ccc1ce627cbd89e4c5683bccbdcdc1be4e7e66b6d63ad268d6c`
and `d00dd8e7d9b77eae0ba2a4d9f26d74c4165ba20f70cec215f6d918240cb4d5ef`,
respectively). Sustained application-consumed throughput remains unmeasured
and P7 remains open.

To measure a complete finite response without a detected presentation cutoff,
a separate profile sent exactly
128 numbered chunks in one request, terminated it with the protocol end marker,
and waited for chunk 127 to appear on the PTY. On macOS, 10/10 runs rendered
that final marker without a presentation-truncation notice. First-fixture-chunk
to-final-marker times were 169–206 ms (median 180 ms), corresponding to
620.8–758.3 visible-completion chunks/s (median 712.7). Sampled RSS maxima were
5,856–6,144 KiB. These are end-to-end observations for a small synthetic
payload below the 8 KiB per-entry presentation limit, not live-provider or
general sustained throughput, and do not establish a universal performance
threshold. The raw batch is
`docs/tasks/evidence/stream-resource-macos-finite-128-10x.json` (SHA-256
`f646affe083b124d9581c1e16bd6eecadc993661a19362f66947735242950ff6`). The
first finite-mode smoke had one valid and two cleanup failures (`PermissionError`
while escalating process-group termination); it is retained but excluded at
`docs/tasks/evidence/stream-resource-macos-finite-128-cleanup-invalid-smoke.json`.
Cleanup now falls back to signaling the owned direct child if group signaling is
denied. The corrected 10-run batch passed 10/10, and the final sampler field
naming/cleanup was separately verified by a successful one-run smoke. This
narrows the stream evidence gap but
does not close representative P7 workload coverage or the full baseline review.

The same finite 128-chunk profile was measured on the Linux reference VM using
a current-source default-feature release TUI, built offline with
`cargo +1.99.0 build -p nexus-tui --release --locked --offline`. The binary is
5,985,480 bytes, SHA-256
`e167381a61ede02e3aff791093acfa98803244165b918bf6a76552417485191a`. All 10/10
runs rendered the final marker without presentation truncation. First-chunk to
final-marker times were 153.2–169.8 ms (median 155.5 ms), or 753.9–835.6
visible-completion chunks/s (median 823.2). Kernel process RSS high-water was
6,784–6,952 KiB. The 50 ms sampling interval yielded only 3–4 samples during
these short windows; the 100-Hz `/proc` CPU tick delta is too coarse for a
useful CPU conclusion here. Raw JSON is retained at
`docs/tasks/evidence/stream-resource-linux-finite-128-10x.json` (SHA-256
`a1443d3662321e9f13eecf733e96b05858248dcb0b9f321b7c7b4260158891c0`). The
Linux and macOS results are both scoped synthetic observations, not a direct
hardware comparison or a general sustained-throughput guarantee. A larger
finite profile then sent 480 numbered chunks (approximately 7.7 kB of content)
in one request, taking about 0.6–0.7 seconds to render the final marker. Both
platforms passed 10/10 with the final marker visible and
`entry_output_truncation_visible: false`; the viewport scrollback banner was
visible in all runs because the text exceeded the terminal height. On Linux,
completion times were 580.2–597.2 ms (median 586.6 ms), corresponding to
803.8–827.3 visible-completion chunks/s (median 818.3), and sampled/kernel RSS
high-water observations were 6,884–7,036 KiB. On macOS, times were 640.3–711.3
ms (median 653.2 ms), rates were 674.8–749.7 chunks/s (median 734.9), and
sampled `/bin/ps` RSS maxima were 6,336–6,592 KiB. The distinct Linux kernel
high-water and macOS sampled-RSS methods are not directly comparable. Raw JSON
is retained at `docs/tasks/evidence/stream-resource-linux-finite-480-10x.json`
(SHA-256 `f30059dbbace5942e428968d84ae62f523517a72d48ecb0040a71547efe6bf2e`)
and `docs/tasks/evidence/stream-resource-macos-finite-480-10x.json` (SHA-256
`4301163fb466dec5c4085c335f67f7c69c59cc57ec558d606816bd5b750a9916`); initial
one-run smoke records are also retained beside each batch. This closes the paired
finite-loopback application-visible sample for this payload; more representative
coding workloads and the complete P7 baseline remain open.

A deterministic Rust patch-style response was then added as a separate finite
profile: 4,470 bytes of multiline Markdown/Rust across 128 SSE chunks, one POST,
and a final marker after the code. On both platforms 10/10 runs rendered the
marker and had `entry_output_truncation_visible: false`. Linux first-chunk to
final-marker time was 155.6–165.8 ms (median 157.2 ms), corresponding to
771.8–822.7 visible-completion chunks/s (median 814.4); sampled/kernel RSS
high-water was 6,828–6,896 KiB. macOS time was 169.9–283.1 ms (median 179.6 ms),
corresponding to 452.2–753.3 chunks/s (median 712.7); sampled RSS maxima were
6,016–6,336 KiB. The macOS maximum is retained, not discarded. RSS methods are
platform-specific, and these short runs yielded only 3–4 RSS/CPU samples each;
macOS `ps %cpu` is not a window delta, while Linux's 100-Hz CPU tick resolution
is coarse for these durations. Raw batches are
`docs/tasks/evidence/stream-resource-linux-coding-128-10x.json` (SHA-256
`484557208eae6c277af6cb7fe93154d64ac7c1b95e403decbd92fe24046284fc`) and
`docs/tasks/evidence/stream-resource-macos-coding-128-10x.json` (SHA-256
`45c8fc8f10f647e6c40a643a0f31459a035422aff9ea28719eced0c328994326`). This
improves coding-shaped presentation coverage but remains a deterministic local
fixture: it does not execute tools, exercise real model behavior, or represent a
complete coding session.

Earlier larger/more fragmented coding fixtures rendered the explicit per-entry
`[output truncated to presentation bound]` marker before their final sequence,
so those runs are retained as invalid diagnostics and excluded from throughput:
`stream-resource-macos-coding-256-invalid.json`,
`stream-resource-macos-coding-256-bound-invalid.json`, and
`stream-resource-macos-coding-64-bound-invalid.json` under
`docs/tasks/evidence/`. They show that logical line overhead as well as payload
bytes affects the presentation cap; reducing the number of SSE chunks alone did
not make the 6,276-byte multiline fixture fit.

Owner approval (2026-11-01) now accepts the reference machines and methodology
in `docs/design/05-performance.md`. The owner confirmed the Linux ED25519
fingerprint through a trusted channel; the reference VM presented the matching
key and strict host verification succeeded. Root public-key authentication
initially failed, then succeeded after the owner installed the local public key.
Remote validation has resumed against a separate current-source snapshot,
preserving the previous project and evidence. No password was written to a file
or used in a command.

Default workspace tests, all-target Clippy, formatting and release rebuilds
passed on both platforms after the experimental policy reuse was withdrawn,
including the current startup and transport regression tests. This evidence is not a
full optional-feature run or a production security certification. A passing
warm-cache sample is not a cold-cache result or an accepted baseline.

### Linux input-feedback optimization — 2026-11-01

The current TUI separates processed terminal input feedback from stream redraw
coalescing. Input batches request an immediate frame; streaming/maintenance
updates retain the configured 33 ms default interval. Input polling remains
20 ms, with the existing per-batch bound and no new timer or thread. A successful
input frame restarts the stream budget, and a failed draw retains its immediate
request. Sandbox probing, authorization and default build features are unchanged.
This shared frontend behavior also applies on macOS.

Before the additional sustained-pressure regression, affected config/TUI suites
passed 643 tests with 11 skipped/ignored on each platform; all-target package
Clippy and formatting passed, and Linux default release built successfully.
The added deterministic regression models 1,001 stream updates and 51 processed
input batches over one second: exactly 51 frames are drawn, no duplicate frame
remains ready, and stream-only throttling resumes after input stops. This models
gate behavior, not measured scheduler cadence or end-to-end typing under load.
The latest macOS affected-package rerun passed 644 tests with 11 skipped;
all seven refresh-loop tests and all-target package Clippy passed.
Linux subsequently passed all seven refresh-loop tests, including the new
pressure model, followed by affected-package all-target Clippy and formatting.

Linux default release is 5,920,400 bytes, SHA-256
`fd8f9cb6993086baa8765859c6ef03318dda652725effc6f58f7a506633e37c4`.
The same-method serial comparison retained both launchers and all samples:

| Launcher/profile | Responses | Spawn-to-response p50/p95/max (ms) | Frame-ready p50/p95/max (ms) |
| --- | --- | --- | --- |
| Named-user `setpriv`, warm | 30/30 | 30.711/33.109/39.867 | 11.118/13.559/19.202 |
| Named-user `setpriv`, cache-prepared | 30/30 | 113.911/121.130/122.358 | 94.110/101.473/102.281 |
| Minimal permanent-drop helper, warm | 30/30 | 28.774/30.013/30.961 | 9.169/10.310/11.417 |
| Minimal permanent-drop helper, cache-prepared | 30/30 | 85.372/98.104/103.149 | 65.508/78.441/84.341 |

Named-user warm p95 improved from 52.528 to 33.109 ms; cache-prepared p95
improved from 138.432 to 121.130 ms, but all 30 prepared responses still exceed
100 ms. Minimal-launcher prepared p95 improved from 112.380 to 98.104 ms, with
one response still above 100 ms. This diagnostic method does not replace the
original launcher or establish M0 acceptance. Observed input-response p95 was
19.800–20.589 ms across the four batches, versus the prior approximately
38–40 ms post-frame response. Frame-ready changes between separate batches are
not attributed to this input-only scheduling change.

Each prepared batch verified all 1,446 executable pages nonresident before each
of 30 attempts. This remains executable-page verification, not proof of every
library, metadata, CPU or device cache state. All 140 startup/idle sessions
reported successful outcomes and no cleanup escalation. Raw JSON, logs and
residency records are retained under Linux
`evidence/m0-resume-measurements-input-priority{,-minimal}` and copied to the
local session temporary evidence directory; preceding baselines are preserved.

Ten three-second idle windows per launcher sampled 120 RSS values each.
Named-user CPU ranged 0.333–1.329% (median 0.333%); RSS median of per-run medians
was 6,027,264 bytes, maximum sample 6,123,520 bytes. Minimal-launcher CPU ranged
0.333–0.667% (median 0.500%); corresponding RSS values were 5,992,448 and
6,139,904 bytes. The CPU outlier is retained. These short coarse-resolution
observations do not establish a resource improvement or peak-workload budget.
An isolated tracefs rerun recorded 225 target-TID scheduler wakeups in each of
three three-second idle windows (74.989–74.991/s), consistent with the preceding
approximately 74.99/s observation. Thread snapshots were unchanged, trace-loss
counters did not increase, all targets were reaped, and temporary homes and the
trace instance were removed. Raw `evidence/m0-input-priority-wakeups.json` is
also copied locally. This is not a whole-system or short-lived-thread census.
Representative peak resources and the independent buffer/startup-side-effect
gates remain unverified. A subsequent bounded PTY pressure fixture is recorded
below; it does not cover every sustained workload.

### Follow-up startup and input-pressure checks — 2026-11-01

An instrumented named-user-launcher prepared-cache run used the input-priority
default executable above. It completed with no cleanup escalation; observed
frame/response were 218.820/239.510 ms. Tracing itself adds substantial overhead,
so these are diagnostic observations, not acceptance timings. In that trace,
target `execve` began about 45.969 ms after launcher `execve`; the target's first
configuration-file open occurred about 32.757 ms later. Those intervals include
instrumentation, page faults and scheduling, not just measured CPU work. The
trace and sample remain under `evidence/m0-resume-measurements-input-priority-trace`
and were copied locally; a recursive copy timed out, but the local trace contains
the final successful process exit and the JSON/log records are present.

An evidence-only build tried `CARGO_PROFILE_RELEASE_LTO=thin` and
`CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1`, leaving dependencies, features, panic
handling and security behavior unchanged. It built in 56 seconds and reduced
the executable from 5,920,400 to 4,636,696 bytes (SHA-256
`ff14df47f5499694d76591e97dbefb0c2e56221698f2af0b9504e93b44e468eb`).

| ThinLTO diagnostic profile | Responses | Spawn-to-response p50/p95/max (ms) |
| --- | --- | --- |
| Named-user launcher, warm | 30/30 | 31.041/34.659/37.100 |
| Named-user launcher, cache-prepared | 30/30 | 109.430/116.502/120.108 |
| Minimal launcher, warm | 30/30 | 28.966/31.213/31.260 |
| Minimal launcher, cache-prepared | 30/30 | 81.795/99.444/99.996 |

All 140 startup/idle sessions succeeded without cleanup escalation. Each
prepared batch verified 1,133 executable pages nonresident in all 30 attempts;
this does not verify all other cache state. This does
not close the original launcher timing gate: prepared p95 remains 116.502 ms,
and the diagnostic minimal-launcher p95 did not improve versus 98.104 ms.
The release-profile experiment was **not adopted**. The experimental executable
and raw samples are preserved under `evidence/m0-input-priority-thin-lto-nexus-tui`
and `evidence/m0-resume-measurements-input-priority-lto{,-minimal}`, with the
measurement directories copied locally. The default release was rebuilt after
the additional test-only source changes.

`input_pressure::composer_input_and_cancel_progress_under_streaming_output`
now runs in the registered default TUI behavior target on Unix. Its isolated
loopback SSE provider emits bounded traffic while a real 120x40 PTY receives an
80-character random burst, exceeding the 32-key batch limit. It verifies the
whole ordered token with PTY echo disabled, subsequent visible stream progress,
draft clearing and a visible `Cancelled` outcome. The provider is a local test
double, not a live-model call. The watchdog is a functional bound, not a 100 ms
performance assertion. macOS affected-package nextest passed 645 tests with
11 skipped; Linux affected-package tests passed 645 with 11 ignored. Both
all-target package Clippy and format checks passed. Linux default-release PTY
pressure also passed (one burst 51.764 ms, cancellation feedback 59.680 ms), not
an accepted latency percentile. Full workspace/optional-feature suites have not
been rerun for these additional test-only changes.

An alternating ten-attempt-per-profile Linux default-release comparison then
confirmed intact input and visible cancellation for all 20 attempts, but exposed
a presentation tradeoff: input-only priority improved the 80-character burst
from roughly 67 ms to 51 ms while typical cancellation feedback changed from
roughly 35 ms to 60 ms. Raw `evidence/m0-input-pressure-comparison.json` is
retained remotely and locally. The input-only binary in that comparison was
5,920,448 bytes, SHA-256
`6195ecbdffabc222a263e9078f0891ac2b2be69eacabd5bbe165684661f11a52`;
the preceding default build was rebuilt after test-only source line changes.

To avoid that regression, newly merged terminal outcomes now request an
immediate frame as well, both on event absorption and maintenance gap flushing.
Preceding events are still drained/merged first, normal streaming keeps its
33 ms budget, and no extra timer/thread or channel bypass is introduced. A
deterministic regression covers a terminal frame immediately following input,
failed-frame retry, no duplicate draw and resumption of ordinary throttling.
macOS affected config/TUI tests passed 646 with 11 skipped, followed by package
all-target Clippy and diff checks.
Linux subsequently passed the same 646 affected-package tests with 11 ignored,
all-target package Clippy, formatting and the default release rebuild. macOS
default release built and its PTY pressure fixture passed as well (one burst
59.621 ms, cancellation feedback 26.177 ms).

The subsequent Linux alternating three-profile comparison retained ten attempts
per profile, all passing:

| Default-release profile | 80-character burst median/max (ms) | Cancellation feedback median/max (ms) |
| --- | --- | --- |
| Before input priority | 67.280/67.825 | 35.944/54.514 |
| Input priority only | 51.643/52.903 | 59.956/60.559 |
| Input and terminal priority | 51.531/52.201 | 21.937/22.546 |

These are bounded loopback-traffic observations, not a universal throughput or
latency guarantee. Raw `evidence/m0-input-pressure-comparison-terminal-priority.json`
contains all 30 attempts and executable hashes, and is copied locally. A final
Linux process-name check found no surviving `nexus-tui` processes.

The latest Linux default executable is 5,920,392 bytes, SHA-256
`f8f79fc74c916943505a87c4f2c25019fa25e87c7f42b9bf47e8bd2de4e3f150`.
Named-user-launcher warm startup observed 30/30 responses with spawn-to-response
p50/p95/max 30.900/32.986/39.652 ms, frame-ready 11.349/13.154/19.177 ms.
Prepared-cache startup observed 30/30 responses with corresponding times
114.099/119.654/122.309 ms and 94.476/101.065/102.635 ms. Every preparation again
verified zero resident pages out of 1,446 executable pages. Ten three-second
idle windows had CPU 0.333–0.667% (median 0.667%); 120 RSS samples had median of
per-run medians 5,982,208 bytes and maximum 6,037,504 bytes. All 70 sessions
succeeded with no cleanup escalation. Raw files under
`evidence/m0-resume-measurements-input-priority-terminal-priority` are copied
locally. **The prepared response still exceeds 100 ms; M0 remains not accepted.**
These reruns do not close complete cold-cache scope, representative peak resource
coverage, or independent buffer/startup-side-effect gates. Full workspace and
optional-feature suites have not been rerun after the terminal-priority change.

### Linux native-input readiness work — 2026-11-01

The current candidate supplements periodic input with Linux terminal readiness
on the existing current-thread Tokio reactor. Crossterm remains the only reader
and parser. The observer owns a separate nonblocking descriptor, preserves
stdin's file flags, and starts no input thread. Native notifications and
processed batches defer further native notifications until the next 20 ms tick;
the existing periodic path drains parser-retained events and remains the
explicit-notice fallback. Stream and terminal-frame semantics are unchanged.
The safe syscall boundary uses the already-locked `rustix` crate; Linux Tokio
I/O support adds locked `mio` 1.2.0 and `socket2` 0.6.5 (Crossterm still uses
`mio` 0.8.11). No existing dependency was upgraded. Missing cache artifacts were
fetched before locked/offline validation resumed.

The first candidate reopened `/proc/self/fd/0` only. It passed Linux native-wait
unit tests and the unprivileged PTY pressure case, but failed to accelerate the
reference launcher: named-user warm responses were p50/p95/max
30.521/31.841/32.611 ms; prepared responses were 112.459/118.971/120.732 ms
(30/30 each, no cleanup escalation). Executable size was 5,982,576 bytes,
SHA-256 `23b5dc680f146325d458af17f298e5a72c1ab60fcd605e77c8a041344bb18eec`.
An instrumented run confirmed `EACCES` reopening the root-owned PTY after
privilege drop: inherited stdin remained usable, but native readiness fell back
to polling. These are retained failed optimization observations, not passing
native-readiness evidence. Raw batches/trace are under
`evidence/m0-resume-measurements-native-input{,-trace}` and copied locally;
the executable is preserved as `evidence/m0-native-input-proc-only-nexus-tui`.

The corrected observer checks whether stdin is the controlling terminal. It
opens `/dev/tty` in that case and verifies the session identity; a PTY without a
controlling session uses the stdin alias instead. Terminal ownership, launcher
privileges and reference measurement method are not changed. The registered
Linux pressure fixture now checks both controlling and non-controlling PTYs.
Three Linux-only unit tests cover cancelled/stale waits, later input, batch
cooldown, and hangup errors. An initial macOS assertion comparing *all* tty file
flags also failed against the preserved pre-change executable (flags 2 versus
65,538); it was corrected to check the observer-relevant shared `O_NONBLOCK`
bit, without accepting a change to that bit. A Linux Clippy `needless_return`
failure was fixed before the subsequent successful package checks.

### Buffer visibility and startup credential evidence — 2026-11-01

This section was recorded while the Linux reference VM was unreachable from the
macOS host (SSH connection timeout), so every result below is macOS-only
unless stated otherwise. The Linux-target `cargo check` and all-target Clippy
of the affected packages passed by cross-compilation; that is not Linux
runtime evidence.

**Control outbox under a real tool workload.** Earlier outbox evidence injected
events into the transport. The new tests drive one model turn of eight
`host_read` calls whose fake tool returns the full 262,144-byte output budget,
with `event_control_capacity = 4` and both receivers paused until the run
finalizes. In
`review_transport_bounds::real_tool_outputs_exhaust_the_control_outbox_with_a_visible_limit`
the control channel peaked at its 4 events, the outbox retained 8 more, 3
full-size outcomes were delivered and 4 calls executed before the budget stopped
dispatch. The run ended `LimitReached` with "control event budget exhausted",
exactly one terminal, contiguous sequences, and a finalized snapshot marked
truncated. The same workload is checked at each frontend:

- `server::tests::http_sse_and_snapshot_surface_real_control_outbox_exhaustion`:
  an SSE client attaching over real loopback HTTP receives 12 gap-free frames
  ending in one `run-finished` frame with `limit-reached` and the budget error;
  a subsequent snapshot GET returns `content_truncated: true`.
- `tests::real_tool_outputs_exhausting_the_control_outbox_report_a_truncated_limit`
  (headless): outcome `limit-reached`, exit code 5, `truncated` report.
- `tests::real_control_outbox_exhaustion_shows_limit_and_truncation` (TUI):
  the transcript shows `LimitReached` and the truncation notice; the exit
  warning names truncation.

`largest_terminal_fits_the_reserved_outbox_capacity` shows that a terminal with
two maximum-length (1,024-byte) error messages is accounted well below the
65,536-byte terminal reserve and fits after non-terminal exhaustion. While the
consumer stays attached, outbox exhaustion therefore cannot strand a run
without its terminal; a closed control channel remains the separate,
already-reported `Cancelled`/truncated path.

SSE frames carry no truncation field, and delivered sequences stay contiguous
when the runtime drops events. The server API documentation now states that
clients must query `/snapshot` after the terminal and treat
`content_truncated: true` as an incomplete record. No wire format changed.

Whole-run peaks are bounded by construction (Tokio channel capacities and the
outbox admission check) and observed at those bounds in: data receiver 1,024
(`backpressure`), control receiver 128 with 40 outbox-retained events
(`paused_control_receiver_reaches_capacity_over_the_complete_run`), the
real-workload byte exhaustion above, synthetic outbox 256 events/byte budget,
and server pending store 4,097 events/511,048 bytes. No runtime counter
records allocator-level memory high-water; these are event and accounted-byte
peaks.

**Startup credential and side-effect observation.** The new default
`credential_env` TUI behavior test preloads a temporary shim compiled with
`cc` (`DYLD_INSERT_LIBRARIES` on macOS, `LD_PRELOAD` on Linux). The shim
records only the names passed to libc `getenv`. Its configuration has a
selected provider (`NEXUS_TEST_SELECTED_KEY`) and an unselected one
(`NEXUS_TEST_UNSELECTED_KEY`), both pointing at a loopback listener. On macOS,
through the first composer frame and one further idle second, neither
credential variable was looked up and the listener was not contacted.
Observed lookups included `HOME`, `NEXUS_CONFIG`, `NEXUS_BASE_CONFIG`, `PATH`,
`TMPDIR`, `NO_COLOR` and system-library logging variables. Typing one task
then connected to the listener and looked up the selected variable only,
which shows the hook was active on the credential path. On macOS, where
`strace` is unavailable and DTrace needs privileges, the shim also records
`open`/`openat`, `connect` and `posix_spawn`/`fork`/`execve`. Startup opened
`config.json`, `tui-runtime.lock`, `tui-runtime.log` and `session-store.lock`
under the isolated HOME, but no `credentials.json`. It made no internet
`connect` and created no process. The submission produced the expected
`connect inet`. Debug and default-release builds both passed.

These observations do not cover calls that bypass the hooked libc entry
points: direct `environ` access, raw syscalls, statically linked code, or calls
internal to one system library. The workspace has no `std::env::vars`
enumeration, and Rust std reads variables through `getenv`. The stored
credential file remains covered by the existing FIFO fixtures, plus the Linux
`strace` traces recorded above.

Validation on macOS: workspace format check; all-target Clippy for
nexus-runtime, nexus-integration, nexus-server, nexus-headless and nexus-tui
(native and `x86_64-unknown-linux-gnu`); and nextest for the same packages,
1,422 passed with 14 skipped. The uncommitted Linux native-input readiness
candidate is included in that tree. Its corrected `/dev/tty` observer is now
measured on Linux below; two prepared-cache batches have different p95 values,
but every observed response in both remains below the clarified 150 ms ceiling.

### Linux current-source acceptance rerun — 2026-11-01

The synchronized source snapshot was tested on the owner-approved Linux VM with
the default build features. Since `cargo-nextest` is not installed there, the
equivalent serialized Cargo test command was used; no tool was installed. Tests
for nexus-runtime, nexus-integration, nexus-server, nexus-headless and nexus-tui
passed 1,425 with 14 ignored. Workspace all-target Clippy, formatting and the
default release TUI build passed. Linux release PTY pressure passed for both a
non-controlling PTY and a controlling PTY: 80-character input bursts were
complete, streaming progressed, cancellation completed, and native-readiness
fallback was not shown. The observed burst/cancel responses were
43.598/22.508 ms and 42.828/22.843 ms respectively; these two observations are
functional pressure evidence, not latency percentiles.

The real-tool control-outbox exhaustion cases passed end to end on Linux in the
runtime integration, HTTP SSE plus snapshot, headless, and TUI tests. They
verified one contiguous terminal delivery, `LimitReached`, and visible
truncation/incompleteness at each applicable consumer. The Linux `credential_env`
test also passed: startup did not look up either selected or unselected provider
key; its loopback submission positive control looked up only the selected key.
The Linux shim reports no file/network/process hooks, so those side effects are
covered separately by the trace described below.

The Linux workspace all-features run passed 2,694 tests with 27 ignored;
workspace all-target Clippy with all features and formatting passed. Ignored
tests were not force-run. This is feature-enabled validation, not a live-service
or production security certification.

To add optimized-build buffer-path coverage, the Linux release-mode
`backpressure` and `review_transport_bounds` integration targets passed 10
tests total with one opt-in local performance test ignored. This includes the
paused-receiver queue-capacity case and real-tool control-outbox exhaustion.
Three additional release-mode unit tests passed sequentially: runtime control
outbox exhaustion/terminal reserve, server pending-store count saturation at
4,097 events/511,048 accounted bytes, and byte saturation at 16 events/1,050,982
accounted bytes (including its bounded overflow reserve). The commands were:

```sh
cargo test --release -p nexus-integration --test backpressure --locked --offline -- --test-threads=1
cargo test --release -p nexus-integration --test review_transport_bounds --locked --offline -- --test-threads=1
cargo test --release -p nexus-runtime --lib control_outbox_exhaustion --locked --offline -- --test-threads=1
cargo test --release -p nexus-server --lib pending_events_respect_the_count_budget_and_use_one_overflow_reserve --locked --offline -- --nocapture --test-threads=1
cargo test --release -p nexus-server --lib pending_event_bytes_respect_the_byte_budget_and_retain_the_overflow --locked --offline -- --nocapture --test-threads=1
```

This confirms the same bounded-path behavior in release mode; it does not
provide allocator high-water or a representative production workload.

The real-tool control-outbox loss case also passed in release mode at all three
frontend boundaries: HTTP SSE plus snapshot, headless report, and TUI transcript.
Together with the runtime integration test above, this checks one end-to-end
bounded workload and its visible `LimitReached`/truncation outcome under
optimized code. Commands:

```sh
cargo test --release -p nexus-server --lib http_sse_and_snapshot_surface_real_control_outbox_exhaustion --locked --offline -- --nocapture --test-threads=1
cargo test --release -p nexus-headless --lib real_tool_outputs_exhausting_the_control_outbox_report_a_truncated_limit --locked --offline -- --nocapture --test-threads=1
cargo test --release -p nexus-tui --bin nexus-tui real_control_outbox_exhaustion_shows_limit_and_truncation --locked --offline -- --nocapture --test-threads=1
```

Validation-command corrections retained in the remote logs: an initial
integration invocation named a nonexistent `nexus-integration` `behavior`
target and aborted before tests; initial server byte-budget and TUI invocations
used filters/targets matching zero tests. None is counted as a pass. The exact
server test names and TUI `--bin nexus-tui` target above were then run and each
executed one passing test.

One direct release test-process resource observation ran the real-tool
outbox-exhaustion integration case under `/usr/bin/time -v`. It executed 8
fake `host_read` calls (4 executed before the control budget stopped dispatch),
delivered 3 full 262,144-byte outputs, saturated a 4-event control channel, and
reported `LimitReached`, terminal buffered, and `content_truncated=true`. The
test process reported a 6,216 KiB maximum RSS; wall time rounded to 0.00 s at
the timer's precision. This is one sampled test-harness process, not isolated
application memory or allocator high-water, has no accepted budget for
comparison, and is not representative production workload evidence. The exact
test binary was 3,124,776 bytes, SHA-256
`b5ad3f4465be180b4940b9eec4580319ddd07a3a16517753ae68afd3a6ff4c39`; raw output
is retained as `evidence/linux-real-tool-outbox-rss.log`.

The current Linux release opt-in
`cargo test --release -p nexus-runtime --lib measure_fake_tool_dispatch --locked
--offline -- --ignored --nocapture --test-threads=1` completed one run of 16
sequential fake `host_read` calls. Submission-to-worker-entry
p50/p95/max were 15.701/40.617/48.250 µs; fake execution was
0.406/1.430/2.199 µs. The 16 calls are correlated within one process; these
test-hook observations include scheduling and instrumentation, exclude policy
validation and real tool I/O, and are not a representative dispatch baseline.
The run passed with no failed calls; raw per-call values are retained in
`evidence/linux-current-release-fake-dispatch.log`.

The exact default-feature validation commands were:

```sh
cargo test -p nexus-runtime -p nexus-integration -p nexus-server -p nexus-headless -p nexus-tui --locked --offline -- --test-threads=1
cargo clippy -p nexus-runtime -p nexus-integration -p nexus-server -p nexus-headless -p nexus-tui --all-targets --locked --offline -- -D warnings
cargo fmt --all --check
cargo build --release -p nexus-tui --locked --offline
python3 crates/nexus-tui/tests/input_pressure.py /home/nexus-test/project/target/release/nexus-tui --both
```

The all-features commands were `cargo test --workspace --all-features
--locked --offline -- --test-threads=1` and `cargo clippy --workspace
--all-targets --all-features --locked --offline -- -D warnings`, followed by
`cargo fmt --all --check`. Performance batches were run from the privileged
root harness with the target still launched as the unprivileged named user:
`python3 /home/nexus-test/evidence/run_measurements.py --native-tty` and the
single instrumented trace used `--native-tty --trace`. The independent
prepared-cache repeat used the same harness and launcher with
`--native-tty --repeat-prepared`; it retained a separate evidence directory.

The corrected readiness observer uses `/dev/tty` for a controlling PTY and
verifies the session identity; a non-controlling PTY uses the inherited-stdin
alias. Both real PTY cases passed without changing stdin's `O_NONBLOCK` bit. A
single `strace -f -ttt -T` default-release startup observation (instrumented
timings are not performance evidence) recorded `/dev/tty` opened read-only and
nonblocking for readiness and showed reactor `epoll_wait` wakeups before input
polling. Through the first frame it recorded no `credentials.json`, internet
connect, fork/clone, or process exec beyond the approved `setpriv` launcher and
TUI. The pre-TUI `setpriv` process made AF_UNIX name-service lookups; those are
distinguished from target network activity. The TUI did inspect the configured
`bwrap` path, but this trace recorded no sandbox-probe process launch. This is
one isolated missing-config startup, not proof for every configuration or
startup path. Raw trace and sample are retained under
`evidence/m0-resume-measurements-native-tty-rerun-root-trace`.

A second current-release trace explicitly configured local tools with
`--tools-root` pointing at the synchronized source tree, exercising the startup
path that constructs real file/exec tools. The harness observed a composer frame
and input response. The TUI inspected `/usr/local/bin/bwrap` and `/usr/bin/bwrap`
with `statx`, but the complete traced launch had exactly two `execve` calls
(`setpriv` and `nexus-tui`), no clone/fork/vfork, no `/usr/bin/true`, no
`credentials.json`, and no internet connect. The only `connect` calls were the
pre-TUI launcher's AF_UNIX name-service lookups. This confirms no sandbox probe
or extra process before the first frame for this configured-local-tools case;
it does not cover subsequent command submission or every config. Because the
run was traced and cache-prepared, its 236.074 ms frame observation is not
startup latency evidence. An initial harness attempt omitted `--tools-root`
from its traced argv and is retained but excluded; the corrected trace and
sample are under
`evidence/m0-resume-measurements-native-tty-rerun-root-local-tools-corrected-trace`.

A third current-release trace used an isolated empty HOME and the normal VM PATH,
with no `--tools-root`. The TUI observed the composer and a random-token input
response. Trace review found exactly two target `execve` calls (`setpriv` and
`nexus-tui`), no clone/fork/vfork, no `credentials.json`, and no internet
socket/connect. The launcher made five AF_UNIX name-service/userdb connects.
The TUI checked `/usr/local/bin/bwrap` and `/usr/bin/bwrap` with `statx`, but
launched neither `bwrap` nor `/usr/bin/true` before the first frame. The
instrumented frame-ready observation was 148.863 ms and the input response was
2.357 ms after the frame; the combined 151.316 ms is not startup timing evidence
because `strace` was active. Raw JSON and trace are retained under
`evidence/m0-side-effects-default-no-tools-20261102/`; trace SHA-256 is
`3bfc0c01e3725288269484e403686b710b9b0644e91fac0dc29ca69913e019ef`. Together
with the configured-`--tools-root` trace above, this adds current-release
coverage for the two local-tool startup configurations. It still does not
cover later tool submission, every setting/profile, or libc/syscall paths
outside the trace hooks.

The default-release binary was 5,985,208 bytes, SHA-256
`030d3d30f9900a4592add9214cd88f4c191a1c16b81f28a7c02b8ec7f0d8fe49`. Under the
original root harness plus named-user `setpriv --init-groups` launcher, 30/30
warm runs observed a composer response: spawn-to-response p50/p95/max
12.394/13.933/15.510 ms; frame-ready 11.457/13.141/14.612 ms. In the separate
30-run prepared-cache batch, all 30 preparations verified zero resident pages
out of 1,462 executable pages and all 30 sessions completed without cleanup
escalation. Spawn-to-response p50/p95/max were 93.225/98.721/100.561 ms; frame
ready was 92.403/97.535/99.607 ms. This batch is near the 100 ms target and
within the 150 ms observed-launch ceiling; retain the maximum sample and do not
generalize the result to a full-system cold cache. Ten idle windows
of three seconds observed CPU 0.333–0.667% (median 0.667%); 120 sampled RSS
values had median of per-run medians 6,029,312 bytes and maximum 6,148,096 bytes.
Wakeups were unsupported in this historical batch; these are idle samples, not peak
workload resource measurements.

Two failed measurement batches are retained and excluded from timing summaries.
The first corrected-observer run had one cache-preparation failure because four
executable pages were resident. A subsequent attempted rerun mistakenly ran the
measurement harness as `nexus-test` instead of root: all 30 launches exited
before readiness (status 127) and all 30 cache preparations failed permission
checks on `/proc/sys/vm/drop_caches`. Those results are harness failures, not
product outcomes. The successful rerun restored the original root-harness and
unprivileged-target arrangement; raw batches remain in
`evidence/m0-resume-measurements-native-tty{,-rerun,-rerun-root}`. The successful
measurement results do not close broader cold-cache scope, representative peak
resources, throughput, or layered buffer high-water evidence. M0 remains not
accepted.

A repeat prepared-cache batch was then run with the same binary, VM, launcher,
isolated HOME, PTY, and per-attempt cache-preparation/mincore helper, in a
separate evidence directory. All 30 preparations again observed zero resident
pages out of 1,462; all 30 composer responses completed and no cleanup
escalation occurred. This batch's spawn-to-response p50/p95/max were
95.710/106.960/112.951 ms, and frame-ready p50/p95/max were
94.728/105.933/111.924 ms. The `mincore` evidence applies to executable pages;
the harness correctly labels broader cache state as unknown. Compared with the
preceding 30-run batch (response p95 98.721 ms, max 100.561 ms), this repeat has
a higher p95. Both complete batches are retained; neither result is discarded
or averaged to conceal the variance. Both batches meet the
clarified 150 ms observed-launch ceiling; their p95 difference remains useful
variability evidence, not a latency-gate failure. The larger sample maximum is
112.951 ms. These prepared-cache observations still do not establish complete
cold-cache behavior. Raw repeat evidence is under
`evidence/m0-resume-measurements-native-tty-repeat-prepared`.

To test the stricter Linux cold-cache scope, a separate root-side `mincore`
inventory followed `sync` and `drop_caches=3` for the release TUI and its ELF
dependencies. The TUI executable was 0/1,462 pages resident, but the dynamic
loader was 59/63 pages resident, `libc.so.6` 526/535, and `libgcc_s.so.1` 44/46.
Thus the existing runner/VM keeps shared-library pages hot even when the target
executable is verified nonresident. This is direct evidence that the current
prepared-cache profile is not a complete cold-cache launch; it is not a failed
product launch and is not folded into startup percentiles. An earlier inventory
missed the whitespace-indented ELF interpreter line and is retained but
superseded by the corrected dependency list. Raw output is retained as
`evidence/linux-elf-dependency-residency-corrected.log` (the incomplete first
attempt is `evidence/linux-elf-dependency-residency.log`). A strict Linux
binary-only preparation is not a full cold-cache result. A separate profile
that verifies all staged target ELF dependency pages nonresident is recorded
below; it does not establish cold filesystem metadata.

A stricter ELF-page experiment now stages byte-identical copies of the TUI
release binary, interpreter, `libc`, and `libgcc_s` in an isolated chroot and
uses `mincore` immediately before each launch to require all 2,106 pages to be
nonresident. The binary SHA-256 remained
`030d3d30f9900a4592add9214cd88f4c191a1c16b81f28a7c02b8ec7f0d8fe49`. The
30-run chroot batch completed 30/30 with composer and input response, but
spawn-to-response p50/p95/max was 185.790/199.965/202.644 ms and frame-ready
max was 201.647 ms; the batch includes a valid response above the 200 ms strict
cold ELF hard ceiling. This is a valid strict ELF-page-cold candidate failure,
not a default `setpriv` profile result: the chroot launcher and copied ELF paths
differ from the default execution environment. It closes the ELF-page
nonresidency measurement gap only; filesystem metadata and non-ELF runtime
files are not proven cold.
All samples remain in
`evidence/m0-resume-measurements-elf-cold-rootfs/`; the immediate residency
checks are `evidence/elf-residency.jsonl`. The initial smoke invocation exited
127 because it targeted the chroot helper instead of the staged TUI and is
retained but excluded from the 30-run batch.

A follow-up added a byte-identical `setpriv` executable and its `libcap-ng`
dependency to a separate rootfs, plus minimal `passwd`/`group` fixtures, so the
launcher can perform the usual named-user/group drop inside the chroot. The
same TUI binary hash was used. Before each of 30 launches, `mincore` verified
that all 2,125 pages across the TUI, loader, `libc`, `libgcc_s`, `setpriv`, and
`libcap-ng` were nonresident; all 30 preparations succeeded. All 30 launches
observed the composer and a response to input. Frame-ready p50/p95/max were
163.774/174.451/175.075 ms (minimum 156.378 ms); composer input responses were
0.647–1.023 ms. Even conservatively summing the maximum frame-ready and maximum
input-response observations gives 176.099 ms, below the newly approved 200 ms
strict cold ELF ceiling (while above the ordinary-launch 150 ms ceiling). This
batch meets the numeric cold-profile ceiling, but remains an
isolated-chroot comparison, not the default host `setpriv` environment, and
does not verify cold filesystem metadata. Raw samples are
`evidence/elf-cold-setpriv-30.json` and
`evidence/elf-setpriv-residency.jsonl`.

Exploratory chroot attempts with an incorrect executable path or incomplete
target environment exited before composer and are retained as individual
`evidence/cold-setpriv-*-smoke.json` files; those attempts are invalid and
excluded from the timing batches. A corrected explicit-environment setpriv
smoke was valid at 159.626 ms with an input response and is retained separately;
the subsequent 30-run batch also used that isolated HOME/XDG/config environment
and had 30/30 ready and input responses.

## Per-Stage Exit Criteria

- P0: accepted M0 semantic subset recorded; deferred items listed; no code reviewed against unaccepted semantics.
- P1: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` pass on Linux and macOS.
- P2: core unit tests cover identity scoping, bounds, complete versus partial records, error sanitization, and limit exhaustion.
- P3: fake-driven run tests prove one terminal outcome per run, no execution from partial calls, exact-call approval binding, and headless denial without a handler.
- P4: fragmented UTF-8/JSON, malformed frames, duplicate references, truncation, refusals, and continuation identity all have fixtures and tests.
- P5A: headless task runs offline with structured output separated from diagnostics.
- P5B: TUI starts offline, keeps the composer fixed, bounds viewport work, folds entries, and restores the terminal on normal and error exits.
- P6: cancellation under output load, slow consumers, disconnect handling, stale-run rejection, and duplicate-approval safety pass deterministically.
- P7: startup, binary size, memory, idle CPU, streaming, buffer high-water marks, and dispatch overhead recorded for both platforms with methodology.

## M0 Acceptance Checklist

Checked items are verified for the scoped M0 fake paths and the platform runs
documented below, not a production security certification. Unchecked items
remain blockers; a complete P7 baseline has not been accepted.

The data-channel saturation gap is addressed in the current frontend changes:
headless and TUI query the finalized runtime snapshot and surface potentially
incomplete content; an unavailable or inconsistent snapshot is an explicit
error or unknown-completeness notice. TUI preserves the draft and defers a new
submission until the previous terminal has been applied and checked. Separate
tests exercise real bounded-channel saturation and the non-truncated case.
This evidence alone does not cover every failure mode; the owner-accepted M0
scope and its residual limitations are recorded below.
On macOS, `runtime::tests::control_outbox_exhaustion_still_reserves_and_delivers_terminal`
also verifies the outbox's accounted bytes after every buffered usage event and
at the 256-event peak, including the reserved terminal. Its accounting stays
within the 1,048,576-byte budget and the terminal is delivered after draining.
This is a deterministic synthetic saturation case, not a measurement of actual
heap consumption, every payload shape, or the high-water mark over a full run.
The updated count-saturation assertion and runtime suite passed on both macOS
and Linux. A further synthetic test uses maximum-size tool outcomes to exhaust
the byte budget before the event-count limit, then verifies that the reserved
terminal is retained and delivered in sequence. The updated default runtime
suite passed 377 tests with 2 skipped or ignored on each platform, and runtime
all-target Clippy passed on both; the workspace format check passed on macOS.

Control-delivery evidence initially relied on transport-level outbox injection
and separate snapshot checks. Later real-tool exhaustion tests close one concrete
loss path end to end on Linux and macOS: a real loopback HTTP/SSE client receives
the terminal and then confirms truncation from `/snapshot`; headless reports
`LimitReached`/truncation; and the TUI displays the limit and incompleteness
notice. The runtime integration case also verifies terminal retention and the
finalized truncated snapshot. See
`http_sse_and_snapshot_surface_real_control_outbox_exhaustion`,
`real_tool_outputs_exhausting_the_control_outbox_report_a_truncated_limit`, and
`real_control_outbox_exhaustion_shows_limit_and_truncation` above. Synthetic
count/byte saturation and disconnection tests cover additional boundaries.
This establishes end-to-end visibility for the exercised exhaustion path, not
for every loss mode, and does not measure simultaneous high-water across all
runtime, transport, server, and presentation layers. The owner accepted the
scoped M0 gate based on the covered capacity boundaries and real-tool loss path;
allocator-level and simultaneous all-layer peaks remain explicitly unclaimed.
The TUI also retains a session-level incompleteness summary for frontend merge
gaps or rejected presentation events, waits for both event channels to close,
and initiates cancellation of active sessions concurrently before reconciling
them. Reconciliation and snapshot queries are still processed per session with
individual timeouts, not one global exit deadline; total exit latency can grow
with the number of sessions.
If a terminal event never arrives, an interactive session can remain responsive
while further submissions remain gated and their drafts retained indefinitely.
In an artificially half-closed stream the same applies while the other channel
stays open; the normal runtime holds both senders, so that particular closure
scenario has not been established as reachable in production. A safe recovery
protocol for missing terminal outcomes remains unverified; a short timeout alone
could misclassify legitimate long-running work or discard a late terminal event.

`history_startup::interactive_startup_does_not_restore_historical_body` passed
on Linux and macOS against a private PTY fixture with a valid metadata index and
an invalid, sentinel-bearing saved body. Startup succeeded without displaying
the sentinel or opening the credential FIFO. This covers that clean-start case;
it does not prove that no low-level file reads occur or cover every recovery
and terminal failure path.
On the Linux VM, the same isolated historical-body fixture was additionally run
against a default debug TUI under `strace -f -e trace=%file,%network,%process`,
from process launch through roughly one second after the first frame. The trace
contained no calls naming the synthetic credential FIFO or historical body, no
`connect`/network socket calls, and no additional exec or process creation. It
did contain one `AF_UNIX` socketpair, plus expected index and lock-file access.
The trace and fixture cover this successful startup only, not environment reads,
other recovery/failure paths, macOS, or all possible indirect file access.
That fixture does not set `PATH`, so it does not exercise an available `bwrap`.
A later Linux trace with the normal VM PATH did observe the default TUI launching
`bwrap` and `/usr/bin/true` for the exec-sandbox initialization probe before
the first composer frame. This is an OS-sandbox probe, not an external plugin,
but it is startup process activity and is outside the earlier no-process fixture.
`startup_failure::corrupt_session_metadata_fails_without_recovery_or_provider_access`
also passed on both platforms for corrupt index and journal fixtures: startup
failed explicitly, kept the staged and saved fixture files intact, and the
credential FIFO/loopback listener observed no access. FIFO polling cannot rule
out every short nonblocking open; other startup failures and destinations are
not covered by those fixtures.
The two Linux corrupt-index/journal fixtures were also traced with
`strace -f -e trace=%file,%network,%process`. Both failed as expected; neither
trace named the synthetic credential FIFO or saved body, nor contained a socket,
connect, additional exec, or clone call. This strengthens only these failure
fixtures and does not establish absence of environment reads or cover other
startup paths. The raw synthetic trace logs are retained with the Linux evidence.

The TUI startup fixture now additionally exercises valid pending create, update,
and delete transactions on macOS. Each case reaches an interactive startup
frame and remains alive for a one-second observation window without contacting
the synthetic loopback provider or opening the credential FIFO as observed by
the fixture's nonblocking writer checks. It verifies the recovered metadata,
published/deleted body, removed staging/journal, and absence of historical or
staged body sentinels from terminal output. Both startup tests passed via
`cargo test -p nexus-tui --test behavior startup_failure --locked --offline --
--test-threads=1`; TUI all-target Clippy and formatting passed. The subsequent
serialized default TUI nextest run passed 580 tests with 11 skipped. This covers these
three successful recovery paths, not every credential read or startup effect;
the FIFO observation cannot exclude brief nonblocking opens. Both startup tests
then passed on Linux, along with TUI all-target Clippy and formatting, using
the resumed snapshot and the same commands.
Linux `strace -f -ttt -T -e trace=%file,%network,%process` observations of all
three recovery fixtures subsequently found no credential-file mentions, network
connects, additional execs, or clone/fork calls, and no `openat` of the saved
body. Create/update used the expected staged-body rename and delete unlinked
the affected body. AF_UNIX socketpairs remained present. These fixtures omit
PATH and do not establish absence of credential-environment lookups or every
indirect access. The first traced fixture batch exposed wrapper-only teardown
leaving three TUI children alive; those exact children were terminated and the
fixture now starts a dedicated process group and terminates that group. Both
startup tests passed again on macOS, and Linux corrupt-metadata plus all three
traced recovery cases passed with no surviving TUI processes. The completed
Linux traces are `m0-resume-recovery.149178/149181/149184.trace` under `evidence`.

After these frontend changes, the serialized default workspace suite, all-target
workspace Clippy and formatting passed on both platforms: Linux 2,620 passed,
25 ignored; macOS 2,626 passed, 19 ignored. Headless and TUI targeted tests and
Clippy also passed on each platform. The Linux demo PTY regression passed at
all three recorded sizes, as did the macOS rerun. These checks do not include
all optional features.

After the workspace run, a real-TUI historical-body fixture and explicit
closed-stream completeness handling were added. On each platform, default TUI
tests passed 572 with 11 ignored; demo-feature tests passed 572 with 12 ignored;
both variants' all-target Clippy and the three-size demo PTY regression passed.
The headless package passed 122 tests and all-target Clippy on each platform.
The full workspace suite was not rerun after those last TUI test additions.

After the later closed-stream and exit-warning fixes, the default workspace
suite, all-target workspace Clippy and formatting were rerun: Linux 2,624
passed/25 ignored and macOS 2,630 passed/19 ignored. Default/demo TUI tests and
Clippy passed separately on both systems, as did the three-size demo PTY
regression. These do not include the optional/all-features suite.

After the subsequent per-session cancellation, channel closure, and sticky
presentation-gap fixes, each platform reran one strictly sequential Cargo
validation sequence: default workspace tests and all-target Clippy, demo TUI
tests and all-target Clippy, the opt-in three-size demo PTY test, then format
check. Linux workspace: 2,629 passed/25 ignored; macOS: 2,635 passed/19 ignored.
The demo TUI suite passed 579 with 12 ignored on each platform; both PTY runs
passed. An earlier overlapping pair of Cargo sequences caused PTY failures
when a default binary replaced the demo binary; those failed runs are not used
as evidence. This does not include the optional/all-features suite.

On 2026-11-01, a rebuilt default-feature release TUI was measured on macOS in
an isolated HOME and config environment, with no cache purge. All 30 composer
probes observed a response; spawn-to-response p50/p95/max were
64.381/86.575/171.700 ms. All 30 cleanup sequences escalated beyond the quit
key after the input probe; the numbers do not validate graceful exit. The
observation and cache-method limitations below remain, and reference hardware
and methodology have not been accepted. A preceding run with inherited user
environment did not reach the composer and is invalid, not a performance pass.
The corresponding isolated Linux run observed 30/30 responses with
spawn-to-response p50/p95/max 59.169/60.859/61.046 ms; its 30 cleanup sequences
also escalated after the default quit key. Both runs are warm-cache observations,
not accepted reference or cold measurements. The performance harness's Python
unittest suite passed 279 tests with one
skipped on macOS; this is harness coverage, not performance acceptance.
The harness default quit key is Ctrl+D, which is not the TUI's quit sequence.
A separate three-sample probe on each platform used `--quit-hex 0303` to clear
the probe draft and then quit with Ctrl+C; all six exited zero without cleanup
escalation. These small samples check that cleanup path, not startup percentiles.

Before the later closed-stream change, the default-feature release TUI was
measured with the
warm-cache composer probe: all 30 samples produced observed responses on each
platform. Spawn-to-response p50/p95/max were 77.533/90.184/838.002 ms on macOS
and 58.556/59.546/59.740 ms on Linux. The first macOS sample remains included;
  the probe's observation and cache-method limits below still apply.
For that Linux release binary (SHA-256
`8ef6566e0451334a596b88ccc43d8870b85ec4c40a672e5f0a8ddd4b435cfff4`),
20/20 observations after per-sample `sync` and `drop_caches` preparation used
the unprivileged `setpriv` launcher: p50/p95/max were
149.089/152.454/218.027 ms, including that launcher. Preparation succeeded in
all samples, but cache state was not independently verified; these are not
directly comparable to the direct-launch warm samples or a proven cold baseline.

On 2026-11-01, a further Linux default-release run used an isolated HOME,
`sync` plus VM-wide `drop_caches` before each launch, then checked the target
executable with `mincore` without touching its mapped pages. All 20 preparations
reported zero resident pages out of 1,445 immediately after preparation, and
20/20 composer responses were observed. Spawn-to-response p50/p95/max were
148.954/155.225/168.831 ms, including `/usr/bin/setpriv` to launch as the
unprivileged user; all runs exited normally with the explicit Ctrl+C sequence.
The target SHA-256 was
`297019f1010281e85be8e2aaab33405aad504202c8913a56430adaec78663b6b`.
The probe verifies only that file's reported page residency at that instant,
not dynamic libraries, filesystem metadata, subsequent cache activity, or a
genuinely cold full system. The helper and `setpriv` may warm dependencies.
This is not an accepted reference baseline; the observed p95 exceeds the
proposed 100 ms regression gate and must not be reported as passing it.

For a same-launcher warm comparison, 20/20 observations using `setpriv` and the
same isolated environment yielded spawn-to-response p50/p95/max
61.224/63.841/65.269 ms; these separate batches are observational, not an
accepted controlled baseline. One subsequent prepared-cache `strace -f -ttt
-T` run including terminal writes observed `setpriv` exec at 1793467341.594520,
TUI exec at 1793467341.646370, sandbox-probe `bwrap` exec at 1793467341.757601,
successful probe reap at 1793467341.821216, and composer-frame output at
1793467341.824875. Thus this traced run confirms probe ordering, not the probe's
untraced cost: tracing and privilege handling materially alter timings. The
TUI input poll is 20 ms and the observed probe-write-to-screen response was
roughly 39–40 ms in these samples, but those observations do not independently
attribute delay to input polling or drawing. Raw synthetic traces and samples
are retained with the Linux evidence.

An experimental deferred sandbox probe was subsequently measured using the
same VM, isolated environment and `setpriv` launcher. In 20 warm launches,
20/20 responses were observed; spawn-to-response p50/p95/max were
50.708/51.563/52.423 ms, versus 61.224/63.841/65.269 ms in the earlier
warm batch. These are separate observational batches, not an accepted baseline.
In a prepared-cache batch, only 18/20 launches occurred: the helper reported
4 and 128 resident executable pages on the other two preparations and refused
to launch. The 18 observed responses had p50/p95/max
126.424/140.861/140.861 ms. The failures are retained, not excluded from a
claim of successful cold measurement; the helper does not verify full-system
cache state. The optimization's first-command cost is measured on macOS below;
the later 20/20 Linux first-command follow-up is recorded below as well. These
probe-plus-command results are scoped observations, not a complete release-mode
comparison or accepted baseline.
On the experimental code, Linux default workspace tests passed 2,635 with 25
ignored and workspace all-target Clippy passed. Four explicitly selected Linux
Bubblewrap confinement, protected-credential, external-write and timeout tests
also passed. The tools tests include concurrent probe failure, probe timeout,
and cancellation/deadline during a successful probe; these do not measure
Linux first-command latency or independently characterize every cancellation
race.
One warm isolated startup trace of the experimental release build with the
normal PATH observed only the launcher and TUI execs and no clone/fork calls
through the harness's response and exit. This confirms the absence of the
previous startup probe in that fixture, not in every startup configuration or
after a command is submitted; tracing is not a performance measurement.
An attempted macOS default workspace nextest run on the experimental code
stopped after two `nexus-container` worker transport tests timed out; 2,153
tests did not run and workspace Clippy was not reached. A lower-concurrency
rerun passed 2,637 tests with 19 skipped; workspace all-target Clippy and
formatting also passed. The cause of the earlier failures has not been
established; a successful serial rerun does not prove the original cause.
After adding the deferred-probe measurement and whole-run control-queue sample,
the macOS serialized default workspace rerun passed 2,638 tests with 20 skipped;
all-target workspace Clippy and formatting passed. Separately, the performance
harness unittest suite passed 279 tests with one skipped after adding sampled
RSS summary statistics. The owner has now approved the reference profile in
`docs/design/05-performance.md`.
After SSH access was restored, the Linux current-source snapshot passed
`cargo test --workspace --locked --offline -- --test-threads=1`: 2,636 ordinary
tests plus one doctest passed, with 26 ignored. Workspace all-target Clippy and
formatting passed. The source archive SHA-256 was
`ef563fc165297cb634cb31e62550df588b664ed777001c0992f08448e8dc1950`;
the remote log is `evidence/m0-resume-workspace-20261101.log`. This snapshot
includes the latest control receiver and server pending-store pressure tests,
but predates the subsequently added successful TUI recovery fixture; that
fixture is being validated separately.
On macOS Darwin 25.3 arm64, a current default-feature release TUI built from the
deferred-probe code was measured with the isolated PTY harness (30 warm launches,
first launch included): 30/30 composer responses, frame-ready p50/p95/max
10.116/16.221/1,662.615 ms, and spawn-to-composer-response
52.856/60.053/1,703.186 ms. The startup outlier is retained. A separate ignored
release-mode tools measurement ran 20 sandboxed `/usr/bin/printf` commands with
a fresh executor each time; first-command elapsed p50/p95/max were
29.347/31.044/31.624 ms. This includes probe plus actual command, is a short
single-process sequence rather than independent startup samples, and is not a
baseline. Resumed Linux first-command observations follow.
After access resumed, the same ignored release-mode tools measurement on Linux
completed 20/20 first commands with probe-plus-command p50/p95/max
21.172/23.059/32.683 ms. Like the macOS sequence, it uses fresh executors in one
test process, not independent fresh host launches.
The same earlier macOS executable was 5,111,168 bytes (uncompressed file size),
SHA-256 `bef2923c62ea1dde5e5415e3f88b320e8028a6d53a14233b3dbb52e6f02ce10f`;
the distribution/archive size was not measured for that build. A later
installer-aligned build on this macOS host (release, locked, container features
enabled for TUI and server) measured 12,665,391 bytes total uncompressed across
the three installed executables and the `nexus-config` script. This is the
installed-file payload size, not a compressed archive or container image. The
per-file sizes and SHA-256 values are retained in
`docs/tasks/evidence/distribution-install-payload-macos-release-container.json`.
No release archive artifact exists in the repository, so compressed download
size remains unmeasured. These are one-host/build observations, not a
cross-platform size budget or accepted reference.
The same macOS release TUI had 10/10 observed three-second idle windows after
the composer frame. Sampled process CPU was 0.000–0.333% (median 0.166%); 118
live RSS samples had a median of per-run medians of 4,014,080 bytes and a
maximum sample of 4,505,600 bytes. These are sampled idle working-set points,
not peak startup or peak workload memory. Context switches were unavailable
and wakeups were `unsupported` in this historical batch. All ten sessions exited without cleanup
escalation after the explicit Ctrl+C sequence. The new sampled RSS summary
fields are covered by a performance-harness unit test.
A preliminary idle invocation that retained the harness's default extra
`m0-test` marker timed out 10/10 and is invalid, not counted as idle evidence.
The valid rerun required only the actual composer marker.

The readiness sampler now reads macOS `RUSAGE_INFO_V4` through
`proc_pid_rusage` and reports deltas for `ri_pkg_idle_wkups` and
`ri_interrupt_wkups`. These are explicitly per-process package-idle/interrupt
counters, not Linux scheduler trace events or proof that the TUI caused a
machine wakeup. The API and field offsets were smoke-tested on this host, and
the deterministic test pins the v4 layout and delta arithmetic. A fresh
isolated-environment 10-run batch observed the composer frame and a full
three-second idle window in all runs; Ctrl+C cleanup needed no escalation.
Sampled CPU was 0.000–0.333% (median 0.332%). Per-run wakeup deltas were 11–29
package-idle (median 24.5) and 357–364 interrupt (median 363), totaling 215 and
3,617 respectively across the ten windows. These totals are descriptive only;
they are not scheduler-event counts and must not be compared with Linux
tracefs. Sampled `/bin/ps` RSS maxima were 3,932,160–4,685,824 bytes (median of
per-run maxima 4,595,712 bytes); this is neither kernel high-water nor peak
workload RSS. Ready times were 6.771–19.913 ms (median 12.272 ms). Raw JSON is
`docs/tasks/evidence/readiness-macos-idle-proc-pid-rusage-10x.json` (SHA-256
`49caf94f9483c6ff180e3ef5204ac0d04dfe365770b3f6257ea83a78f0003391`). An
earlier inherited-environment attempt exited before the composer marker and is
retained but excluded at
`docs/tasks/evidence/readiness-macos-idle-proc-pid-rusage-10x-invalid-unready.json`;
isolation of HOME/XDG configuration was required. This fresh batch supersedes
the historical batch's unsupported-wakeup field, not its older CPU/RSS values.

Additional opt-in macOS release-profile fake measurements on the current source
completed without failed samples: 16 worker submissions in one run had
submission-to-worker-entry p50/p95/max 1.834/3.000/3.000 µs and fake execution
0.083/0.333/0.333 µs; 10 independent 1,938-byte/512-fragment streams completed
in 331/671/671 µs (p50/p95/max), and 10 cancellation-to-terminal observations
were 98/196/196 µs. A separate 10-call fake integration run observed
ToolStarted-to-ToolFinished p50/p95/max 20/51/51 µs. These include runtime,
scheduling and event delivery as described by each harness; they are synthetic
and do not close a representative real-I/O workload, full queue high-water, or
Linux/macOS paired baseline requirement.

The new multi-turn fake-run integration test pauses the control receiver for the
entire run, polls until finalization, and observes its exact 128-event channel
capacity. Resuming drains 168 control events total (40 retained in the runtime
outbox), including all 136 usage updates and the terminal event; the finalized
snapshot does not claim content truncation for these control events. This is
direct end-to-end evidence for runtime control-channel saturation/outbox drain
and terminal delivery under this synthetic workload. It does not measure
outbox byte high-water or prove visibility through HTTP SSE and every frontend.

The server pending-store count/byte pressure tests now drain every retained
event through `SseDelivery` and inspect the resulting SSE sequence IDs. The
count workload peaks at 4,097 retained events / 511,048 bytes (4,096 ordinary
slots plus the single reserve), with a real terminal event in the reserve; every sequence and
the terminal reach the writer, no ordering gaps are reported, and retained byte
accounting returns to zero. The byte-pressure workload peaks at 16 events /
1,050,982 bytes and likewise drains its oversized reserve without a sequence
gap. Both assert the total retained byte
ceiling, including the reserve. On macOS, `cargo nextest run -p nexus-server
--locked --offline` passed all 175 tests; package all-target Clippy, workspace
formatting, and `git diff --check` passed. These are synthetic pending-store to
SSE-writer tests, not a complete runtime-to-HTTP-client pressure measurement.

`server::tests::http_snapshot_surfaces_real_runtime_data_saturation` additionally
runs the real runtime with a paused data consumer, contrasting 16-fragment
clean output against 1,300 alternating-item fragments. The latter fills the
1,024-event receiver and drops presentation traffic. A real loopback HTTP GET
then returns `content_truncated: true` with the completed outcome, whereas the
clean control returns `false`. macOS server tests passed 176/176 plus all-target
Clippy and formatting. Linux subsequently passed all 176 server tests, package
all-target Clippy and formatting as well. This
closes the runtime-to-HTTP snapshot truncation visibility fixture, not every
SSE reconnect, frontend rendering, or buffer retention mode.

Resumed Linux default-release observations used executable size 5,920,424 bytes,
SHA-256 `78495b0f7ad32ae96592bcd7815b8492f98f37f444a4cb32bc0f9e409feddc37`,
an isolated HOME/XDG environment, 120x40 PTY, and a root harness launching the
target unprivileged via named-user `setpriv --init-groups`. Warm runs observed
30/30 composer responses with spawn-to-response p50/p95/max
50.859/52.528/52.580 ms; frame-ready was 11.203/12.615/12.663 ms. Ten three-second
idle windows had CPU 0.333–0.667% (median 0.667%); 120 RSS samples had median of
per-run medians 6,062,080 bytes and maximum sampled RSS 6,119,424 bytes. Wakeups
were unsupported. All sessions exited without cleanup escalation.

In a separate 30-attempt cache-prepared batch, a privileged dedicated-VM helper
ran `sync`, wrote `3` to `drop_caches`, and verified zero resident pages out of
1,446 executable pages with `mincore` before every attempt. All 30 preparations
and composer responses succeeded. Frame-ready p50/p95/max were
93.415/98.569/100.249 ms, but spawn-to-composer-response was
132.762/138.432/140.041 ms: this does not pass the 100 ms response target. The
verification covers executable page residency only, not shared libraries,
metadata, CPU or storage-device caches; privilege-launcher cost remains included.
Raw JSON, preparation logs and helpers are retained under Linux
`evidence/m0-resume-measurements`, with a local copy in the session's approved
temporary evidence directory. A separate numeric-UID/GID launcher comparison
using `--reuid=1000 --regid=1000 --clear-groups` yielded 30/30 warm responses
p50/p95/max 50.147/51.379/51.780 ms and 30/30 prepared-cache responses
131.044/137.391/141.336 ms. All 30 executable-page preparation checks again
reported zero resident pages, and no cleanup escalation occurred. This variant
also exceeds 100 ms after cache preparation; these separate batches do not
establish named-user/group lookup as the dominant delay. Both raw batches are
retained, not replaced by a selected faster result.

A third diagnostic launcher permanently clears supplementary groups and sets
real/effective/saved UID/GID to the verified nexus-test IDs (1,000), then execs
the same unmodified target. It is an evidence-only C helper with no setuid bit,
not a product change. Identity checks passed and it refuses non-root callers.
It avoids the larger `setpriv` initialization path; it does not bypass sandbox
checks. Its 30 warm responses were p50/p95/max 49.195/50.808/51.482 ms, and
30 prepared-cache responses were 106.543/112.380/118.886 ms; frame-ready for the
latter was 66.665/72.764/79.543 ms. All 30 executable-residency checks again
reported zero pages resident, and cleanup never escalated. The approximately
38–40 ms post-frame composer response remains visible in both warm/prepared
batches. This launcher changes methodology and does not establish a product
speedup; its prepared-cache response still does not pass 100 ms. The original
two batches remain part of the evidence. Files are retained under
`evidence/m0-resume-measurements-minimal` and copied to the local session
temporary evidence directory.

The resumed Linux release fake measurements also completed without failed
samples: 16 submissions in one run had submission-to-worker-entry p50/p95/max
17.932/601.017/601.017 µs and fake execution 0.531/1.244/1.244 µs. Ten
1,938-byte/512-fragment streams completed in 826/1,515/1,515 µs, ten blocked-
stream cancellation-to-terminal observations were 148/181/181 µs, and the
separate ten-call ToolStarted-to-ToolFinished observations were 44/72/72 µs.
The dispatch outlier is retained; these short synthetic workloads do not
establish peak workload memory or full-run queue high-water.

The existing opt-in isolated-instance tracefs helper was run on the same Linux
release executable (`--user nexus-test --runs 3`). Each three-second idle
window recorded 225 `sched_wakeup` events for the initial target TID (about
74.99 events/s), unchanged before/after thread sets, and no trace-loss-counter
increments. Targets were reaped, temporary homes removed, tracing disabled,
and the unique trace instance removed. This is a scheduler-wakeup observation
for those TIDs, not system wakeups, wakeups caused by the target, or evidence
about short-lived threads missed between snapshots. Raw output is
`evidence/m0-resume-wakeups.json`, also copied locally. macOS wakeups remain
unmeasured.

An additional experiment reuses one immutable validated `DirectoryPolicy`
snapshot for the development file and command tool set instead of resolving
the same protected aliases twice. Standalone constructors still validate their
own policy, and sandbox probing/fail-closed execution was unchanged. In that
experimental snapshot, the tools construction test verified that root,
protected paths, temporary paths and the deferred-probe state survived policy
installation. macOS affected-package tests
passed 828 with 12 skipped, followed by all-target Clippy and formatting. Linux
affected-package tests passed 826 with 18 ignored, followed by six explicitly
selected exec-sandbox tests (including real confinement, credential masking,
approved external writes and timeout cleanup); package Clippy and formatting
passed. The Linux experimental executable was 5,920,728 bytes, SHA-256
`f488c5825641b6909c660777f530c131357104536df5955dd9cad497b4aaf894`.
Named-user-launcher warm p50/p95/max were 50.354/57.594/59.015 ms and prepared
132.929/139.101/141.248 ms (30/30 each). Minimal-launcher warm observations were
48.685/49.338/49.411 ms and prepared 105.543/115.963/116.701 ms (30/30 each).
No cleanup escalation occurred. These separate batches do not demonstrate a
material prepared-cache improvement or close the 100 ms response gate.
The macOS default release warm rerun observed 30/30 responses with
spawn-to-response p50/p95/max 50.640/88.414/1,495.904 ms and frame-ready
6.709/58.353/1,453.288 ms, with zero cleanup escalation. All outliers remain.
Executable size was 5,111,024 bytes, SHA-256
`9047e8df145b22f9bd7e7260bd33ea9ad849c734c5ed6f932e3f1b3a65183138`.
This separate batch does not establish a speedup versus the earlier macOS
observations. The shared-policy experiment and its policy-installation-only
assertions were therefore withdrawn using targeted edits; the original
deferred-probe implementation and its regression tests remain. Experimental
binaries, affected sources and raw samples were retained separately. Linux
restored-source workspace validation passed 2,638 ordinary tests plus one
doctest, with 26 ignored; the separately selected six exec-sandbox tests passed
again. Workspace all-target Clippy, formatting and the default release rebuild
passed. macOS restored-source validation passed 2,640 tests with 20 skipped;
workspace all-target Clippy, formatting, diff checks and the default release
rebuild passed. No performance improvement or acceptance is claimed.

An earlier macOS serialized default workspace run including the HTTP
snapshot and successful-recovery tests passed 2,640 tests with 20 skipped;
workspace all-target Clippy, formatting and diff checks passed. The later
Python-only process-group cleanup adjustment was rerun via both startup tests
on macOS and the Linux corrupt-metadata/traced-recovery fixtures before the
restored-source complete workspace runs recorded above.

- [x] One model/tool/model cycle completes with fake integrations.
- [x] Read-like fake calls proceed under scoped policy; mutation/command-like calls require approval or headless denial.
- [x] Cancellation stops dispatch, reports uncertain effects honestly, and never claims rollback.
- [ ] Bounded queues, output, context, and event buffers have finite defaults and visible truncation.
- [ ] Startup performs bounded local work only, including the local configuration
  loading accepted in the 2026-10-31 amendment to the [M0 lock](06-m0-lock.md);
  no network, credentials, plugin processes, or automatic conversation restore.
  A bounded session metadata index and unfinished-transaction recovery are
  allowed by the 2026-10-31 history amendment; recovery may access the affected
session body. Credential access remains unverified and was not waived.
- [x] TUI and headless paths share one runtime and policy boundary.
- [x] Linux and macOS validation recorded separately.
- [x] Limitations of the M0 fake validation path are documented, including its
  ephemeral store and scripted provider. Current product capabilities are stated
  separately: configured live providers and optional persistent storage exist;
  external plugins remain unsupported.

## Evidence Format

For each gate, record:

- Exact commands and results.
- Changed paths and test names.
- Platforms, hardware, terminal/PTY setup, build profile, and enabled features.
- Performance sample counts with p50, p95, and maximum where applicable.
- Remaining blockers and deferred work.

Use synthetic fixtures by default. Live-model checks are out of scope for M0 and require separate authorization if ever added.

## Provisional cross-platform evidence — 2026-10-31

This is not acceptance: the source tree was changing during these measurements,
some tests and measurements overlapped, and the coordinator has not closed the
remaining gates. Both systems used rustc 1.99.0 and `Cargo.lock` SHA-256
`1c51d6805a06bee540458b2ec03ce2ae91d9f92102fba0a6be5a39e019882509`.
The Linux host was Ubuntu 26.04.1 x86_64, kernel 7.0.0-38, 4 vCPUs and 7.3 GiB
RAM; macOS was Darwin 25.3.0 arm64. Both used the default release profile and
the TUI's explicit `demo` feature for PTY measurements, with isolated empty
configuration and synthetic scripted inputs. The demo profile is not the
default production configuration.

- After the demo feature, credential-startup and input-poll changes, serialized
  default workspace tests passed: macOS 2,619 passed/18 ignored; Linux 2,613 passed/22
  ignored. Both ran `cargo test --workspace --locked --offline -- --test-threads=1`
  and all-target workspace Clippy and formatting. Different platform-specific
  tests account for different counts. TUI `demo` feature tests passed 567 on
  each platform (12 ignored), and its all-target Clippy passed; the opt-in real
  PTY regression passed at 80x24, 120x32 and 174x40 on both platforms.
- Warm-cache headless spawn-to-exit, 50/50 successful samples (not readiness):
  macOS p50 3.627 ms, p95 8.425 ms, maximum 1,192.629 ms; Linux p50 2.416 ms,
  p95 2.855 ms, maximum 3.367 ms. The macOS outlier is retained. Linux `wait4`
  RSS can include the harness's pre-exec fork footprint. Neither profile proves
  the 100 ms first-interactive target.
- PTY observed composer frame and subsequent command-panel response: 20/20
  observed on each platform. The frame p50/p95/max were 29.601/87.324/968.842 ms
  on macOS and 18.957/35.535/40.633 ms on Linux. The probe waits 350 ms between
  Esc and `:`; it confirms an observed response, not first-interactive latency.
  These trials overlapped other work and are not an accepted baseline.
- A subsequent warm-cache `--probe-composer` run on the updated release-demo
  binaries observed 30/30 responses on each platform. Spawn-to-observed-response
  p50/p95/max were 81.779/124.984/817.403 ms on macOS and
  68.951/70.411/70.826 ms on Linux. This probe checks that PTY echo is disabled
  and observes a random token in the rendered screen, but echo is sampled only
  around the write and the screen reconstruction is partial; it remains an
  external observation, not a formally accepted methodology. The macOS p95
  exceeds the proposed 100 ms p95 gate, and the outlier is retained. The demo
  results must not be presented as measurements of the default build.
- The same probe on default-feature release TUI binaries, with empty isolated
  configuration and warm cache, also observed 30/30 responses per platform.
  Spawn-to-response p50/p95/max were 135.130/179.325/1,248.712 ms on macOS
  and 118.181/120.354/121.612 ms on Linux. Both observed p95 values exceed the
  proposed 100 ms gate; the first macOS sample is retained. These figures share
  the probe limitations above and are not an accepted reference baseline.
- After immediately drawing the initialized first frame and introducing a 5 ms
  input poll, repeating the default-build probe produced 30/30 responses on each
  platform: p50/p95/max 62.012/100.164/811.945 ms on macOS and
  53.751/54.428/54.449 ms on Linux. The macOS p95 remains slightly above the
  proposed threshold and its first-run outlier remains included. The extra
  periodic input polling can increase idle wakeups; its resource tradeoff and
  methodology acceptance remain open.
- With the input poll subsequently changed to 20 ms, the same default-build
  warm-cache probe observed 30/30 responses: p50/p95/max
  74.542/89.152/813.877 ms on macOS and 58.344/59.298/59.305 ms on Linux.
  Five subsequent 3-second idle samples each observed approximately 0.33%
  process CPU on both hosts. These short, separate runs suggest a lower idle
  cost than the 5 ms trial, but the harness at that time did not measure
  wakeups. The macOS
  first-run outlier, probe limitations, missing cold-cache evidence and the
  need to accept reference hardware and methodology remain.
- Five 2-second demo idle samples observed 0–0.498% process CPU on macOS and
  0% at the Linux sampler's resolution. Wakeups are unsupported by the harness;
  the short window and sampling resolution do not prove zero CPU or wakeups.
  After the 5 ms input-poll change, five 3-second **default-build** idle
  samples observed 0.332–0.666% on macOS and 1.000–1.333% on Linux. The two
  measurements use different profiles and are not a controlled comparison;
  actual wakeups remain unmeasured and this resource cost needs review.
- Opt-in fake streaming and backpressure/cancellation measurements ran on both
  platforms with 10 samples of 512 fragments/1,938 bytes. They check complete
  output or a cancelled terminal; timing includes runtime, scheduling and event
  delivery. The fake-call event interval is not isolated dispatch overhead, and
  internal queue high-water marks are not instrumented. The runs overlapped
  other work and are not a baseline.
- A subsequent macOS release-mode opt-in test,
  `cargo test --release -p nexus-runtime --lib measure_fake_tool_dispatch
  --locked --offline -- --ignored --nocapture --test-threads=1`, instrumented
  the blocking-worker submission and entry inside the test build. For 16
  sequential fake tool calls in one run, submission-to-worker-entry p50/p95/max
  were 4.375/6.500/6.500 µs; fake execution was 0.209/0.875/0.875 µs. These
  are not independent process samples and include scheduler and probe effects;
  they exclude earlier validation/policy, event delivery, and actual I/O. The
  test-only hook is absent from production builds. Linux and multi-run results
  were subsequently recorded below; a representative workload remains open.
- A follow-up ran the same release-mode test in 20 separate processes on each
  platform, with 16 sequential calls per process and no failed runs. Across
  the resulting 320 correlated call samples, submission-to-worker-entry
  p50/p95/max were 3.125/7.584/32.250 µs on macOS and
  14.558/36.110/207.226 µs on Linux. Fake execution p50/p95/max were
  0.084/0.459/1.292 µs on macOS and 0.262/1.158/2.187 µs on Linux. Calls
  within each process are not independent; these figures retain probe and
  scheduler overhead and do not measure policy validation or real tool I/O.
- The release-mode integration test
  `backpressure::paused_receivers_measure_queue_peaks_at_approval_gate` passed
  on both platforms. With 1,500 alternating 64-byte fragments and both receivers
  paused until the approval gate, it observed receiver queue peaks of 1,024 data
   events and 5 control events. This is an exact peak for that gated phase only;
   it excludes the internal control outbox and all later run phases.
- Subsequent Linux and macOS debug-mode runs of the same workload assert that its data
  receiver reaches its fixed 1,024-event capacity before approval. Since that
  bounded channel cannot exceed its capacity, 1,024 is also the exact data
  receiver event-count peak for this complete run, not merely the gated phase.
  The control receiver's five-event observation remains limited to the gated
  phase; the internal outbox and other workloads are not measured. The Linux
  integration package and all-target Clippy passed on both platforms; macOS
  nextest reported 154 passed and one skipped. Formatting passed on macOS.
- `review_transport_bounds::provisional_usage_burst_is_bounded_and_terminal_survives_resume`
  now records the paused control receiver length after the bounded usage burst
  has completed, then checks that the terminal-inclusive drain matches it: an
  exact whole-run peak of 21 control events for this workload. It exercises the
  16-update provisional usage cap, not a full control-channel saturation or
  internal outbox peak. The integration package passed 154 tests with one
  skipped, plus Clippy and formatting on macOS.

The cache was not purged or independently verified as cold. TUI startup now
defers credential resolution until run submission. Isolated PTY tests cover
normal and demo startup with configured stored/environment references; a Linux
`strace -f -e trace=connect` run of those fixtures observed no connect calls.
These cases do not prove all configurations or the absence of environment reads,
history access, or plugin activity. Linux TUI-thread scheduler wakeups have a
scoped tracefs observation below; macOS wakeups use separate per-process
`proc_pid_rusage` counters (not scheduler trace events), and neither
measurement attributes causality to the TUI. Fake-worker dispatch timing is
available, but full validation/policy/event-delivery attribution remains open.
Simultaneous whole-run/internal buffer high-water was not measured but is an
accepted limitation of the scoped buffer gate. The first-interactive reference
baseline and representative performance workload coverage remain open.
Historical raw records are held in the temporary validation environment;
newer streaming evidence is retained under `docs/tasks/evidence/`.
The owner declined a macOS system-cache purge during this review; macOS cold
startup remains unverified, is deferred to M5, and must not be inferred from
warm observations.

Following the history amendment, the macOS `nexus-session` suite passed 25 tests
and all-target Clippy. `opening_a_clean_directory_reads_index_without_loading_session_bodies`
checks that an invalid saved body is not decoded while opening a clean store,
but is rejected when explicitly loaded. The separate
`directory_transactions_recover_create_update_and_delete_without_loading_bodies`
covers recovery cases. These tests do not prove absence of every filesystem read;
the new test was subsequently included in the Linux workspace run below.

### Subsequent Linux re-verification

On a newly provisioned Ubuntu 26.04.1 x86_64 VM (Linux 7.0.0-38, 4 vCPUs,
7.3 GiB RAM, ext4, Rust 1.99.0, Bubblewrap 0.11.1), the snapshot of commit
`be006dd` plus the current `nexus-session` and `nexus-runtime` test changes
passed `cargo +1.99.0 test --workspace --locked -- --test-threads=1` (2,615
passed, 23 ignored); all-target workspace Clippy and formatting passed. The
demo-feature TUI suite passed 567 tests with 12 ignored; its real PTY regression
passed at 80x24, 120x32, and 174x40. Four ignored Linux Bubblewrap tests and
the release-mode gated queue peak test also passed. The new session-body test
was included in the workspace run.

The release-mode `measure_fake_tool_dispatch` test recorded 16 sequential
calls in a single run: submission-to-worker-entry p50/p95/max
12.697/22.996/22.996 µs and fake execution 0.243/1.336/1.336 µs. These are
instrumented observations, not independent runs or total dispatch cost; they
exclude preceding validation/policy, event delivery, and real I/O. Repeated
independent runs and an accepted reference methodology remain necessary.

Two subsequently added, explicitly run Linux filesystem tests passed on ext4:
`linux_replacements_preserve_user_xattrs_and_metadata` and
`linux_replacement_refuses_unrepresentable_xattr_without_mutation`. They cover
user xattrs, uid/gid/mode, and one fail-closed path, not ACLs or all metadata.
Tools all-target Clippy passed after their addition. Raw logs are copied to the
local temporary validation directory; the full workspace run preceded these
two ignored test additions.

After registering those filesystem tests, the default-feature workspace suite,
all-target workspace Clippy and formatting were rerun on both platforms:
Linux 2,615 passed/25 ignored; macOS 2,621 passed/19 ignored. The two Linux
xattr tests remain explicitly ignored by default and were separately run as
described above. These results do not run the complete optional/all-features
suite and are not evidence for real providers or experimental containers.

An attempted Linux cache-preparation run wrote `3` to `drop_caches` before
each of 20 harness invocations. It is **invalid as cold-start evidence**:
the PTY harness subsequently hashed the executable before spawning it, which
read and potentially warmed the target. All 20 observed a composer response,
but their timings must not be used as cold measurements. Cache preparation
must happen after all harness reads of the target and still needs independent
verification of cache state.

An opt-in preparation hook was then run after harness provenance collection
and before each target launch, using root to execute `sync` and write `3` to
`drop_caches`. All 20 preparation commands succeeded and all 20 composer
responses were observed. The target was launched as the unprivileged test
user through `/usr/bin/setpriv`; spawn-to-response p50/p95/max were
148.799/163.446/194.896 ms, including the privilege-drop launcher. The target
SHA-256 was `34120443f45ec22dc59d685a6b9f94bae2a75e3e1b39d49c1390cbbcfc1f55c2`.
`setpriv` and its dependencies execute after cache preparation, so this is not
directly comparable to the previous direct launch. No independent cache-state
probe was performed: report these as prepared-cache observations, not verified
cold-start evidence. The observed p95 exceeds the proposed 100 ms gate.
A subsequent warm-cache run with the same `setpriv` launcher observed 30/30
composer responses, with p50/p95/max 60.855/62.599/74.125 ms. These are separate
small samples, not an attribution of the difference to any single cause.

On the same VM, an isolated tracefs instance filtered `sched_wakeup` by the
TUI thread IDs present after the default release TUI's composer appeared. Three
3-second idle samples observed 216, 227, and 226 events (72.0, 75.7, and
75.3 events/s); no new TIDs were seen at the end of the sample and the trace
overrun/dropped counters did not increase. This counts scheduler wakeup events
for the observed TUI threads, not whole-system wakeups, all possible short-lived
threads, or wakeups caused by the TUI. Tracefs filtering and sampling have
overhead; no reference budget or before-change comparison was accepted.
The reproducible `tools/perf/tracefs_wakeup.py` experiment was then run on the
same VM against the default release TUI as the non-root test user. Three more
3-second samples reported 226, 225, and 225 events (75.32, 74.99, and 74.99
events/s), unchanged observed thread sets, unchanged loss counters, and removed
its private tracefs instance. The same scope limitations apply.

### Functional test mapping

The workspace runs above include these default-feature tests; this mapping is
review evidence, not a claim that untested paths or a whole gate are complete.

- P2: core `approval::tests::stale_run_call_policy_or_expiry_are_denied`,
  `cov_commands_validate::submit_bounds_count_bytes_not_characters`,
  `cov_content_turns::partial_turn_is_never_completed_and_reports_invalid_input`,
  `cov_error_correlation::approval_notice_diagnostics_never_echo_rejected_text`,
  and `cov_limits_context::exhaustion_maps_to_resource_limit_without_retry`.
- P3: integration `cov_multi_turn::model_tool_model_cycle_completes_with_automatic_read`
  and `mutation_requires_grant_before_dispatch_then_cycle_completes`; runtime
  `cov_approval_dispatch::exact_tuple_approve_dispatches_the_bound_call_once`;
  headless `denial_without_handler_exits_three_with_no_tool_started`.
- P4: fake provider `fragmented_text_and_two_calls_reassemble`,
  `malformed_json_emits_no_candidate`, `duplicate_reference_ends_in_failure`,
  `refusal_carries_unknown_usage`, `truncated_stream_is_incomplete_never_stop`,
  and `continuation_round_trip_preserves_refs_and_bytes`. The fake fragment
  helper is not a wire parser; separate OpenAI SSE tests include
  `sse_tool_fragments_assemble_per_index_before_admission` and
  `sse_without_termination_is_a_truncation_not_a_stop`.
- P5: integration `history_and_headless::headless_stdout_records_carry_no_diagnostics_or_escapes`;
  TUI `cov_render::composer_and_footer_survive_every_focus_and_the_smallest_frame`
  and `cov_state_sequences::a_folded_tail_never_absorbs_a_later_fragment`; the
  opt-in `pty_render::session_commands_preserve_the_terminal_screen` is recorded
  separately above.
- P6: integration `backpressure::control_stays_responsive_when_data_channel_saturates`,
  `prompt_cancellation_under_output_load`,
  `disconnect_and_snapshot::disconnect_while_tool_active_finalizes_once_with_bounded_snapshot`,
  and `stale_and_duplicates::duplicate_approve_never_redispatches`; runtime
  `cov_quarantine::cancelled_blocked_tool_records_cancelled_unknown_uncertain_and_quarantines`.

## Failure Handling

A failed gate returns to its owning slice with the failing command, output, and reproduction. Do not bypass the gate with extra fallback logic, relaxed lints, deleted tests, or changed expectations. If the gate itself is wrong, the main agent updates the relevant task or contract doc first, then reruns the gate.

## Related Documents

- [Overview](00-overview.md)
- [Pipeline](01-pipeline.md)
- [Workflow](02-workflow.md)
- [Implementation readiness](../design/09-implementation-readiness.md)
- [Performance](../design/05-performance.md)
