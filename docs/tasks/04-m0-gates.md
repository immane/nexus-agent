# M0 Gates and Evidence

Status: Acceptance review in progress; **M0 is not complete**. Functional and
platform checks below have current evidence. Startup side-effect coverage,
complete buffer-limit evidence, and portions of the accepted P7 measurement set
remain open. Historical RSS evidence is confounded and must not be promoted to
a baseline.

**Owner-approved scope adjustment, 2026-11-01:** macOS cold-cache startup
measurement is deferred from M0 acceptance to M5; it remains unverified and is
not a passing result. This does not defer Linux cold-cache evidence, macOS or
Linux warm-cache interactive startup, reference-method acceptance, or any other
M0 buffer, startup-side-effect, or performance requirement.

## Acceptance decision — 2026-11-01

**Not accepted.** The macOS cold-cache deferral is in effect and is not a
remaining M0 blocker. The following independent gates remain open:

| Gate | Current evidence | Remaining requirement |
| --- | --- | --- |
| Buffer bounds and visible loss | Data-channel/frontend tests and synthetic control-outbox count/byte saturation, including finalized snapshot checks | Whole-run high-water evidence and visibility for the remaining retention and control-delivery paths |
| Startup side effects | Isolated PTY tests on both platforms, including create/update/delete recovery; Linux traces for normal, corrupt-metadata and successful recovery fixtures | Remaining startup/credential-access evidence, including macOS observation limitations |
| P7 reference and measurements | Owner-approved reference profile; two-platform warm observations, idle/queue/dispatch samples, Linux executable-page cache verification | Linux cache-prepared response exceeds 100 ms; complete cold-cache scope and remaining representative resource, throughput and buffer measurements |

The bounded-buffer inventory has finite limits in the implementation, but that
alone is not acceptance evidence: runtime data/control receivers are 1,024/128
events; the control outbox is capped at 256 entries and 1 MiB with terminal
reserve; provisional usage publication is separately rate/count bounded; HTTP
pending SSE delivery is capped at 4,096 events/1 MiB with a bounded oversized
event slot, and each session retains at most two stream delivery records; TUI
reordering is bounded to the combined channel capacities plus 256 and retained
presentation has per-entry and total-item limits. Existing tests cover many
individual limits and loss notices. Full-run high-water evidence across these
paths and proof that every transport/frontend surfaces each loss mode remain
open.

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
remain blockers; measurements have not been accepted as a P7 baseline.

The data-channel saturation gap is addressed in the current frontend changes:
headless and TUI query the finalized runtime snapshot and surface potentially
incomplete content; an unavailable or inconsistent snapshot is an explicit
error or unknown-completeness notice. TUI preserves the draft and defers a new
submission until the previous terminal has been applied and checked. Separate
tests exercise real bounded-channel saturation and the non-truncated case.
This does not establish that every other buffer or control-delivery failure is
visible in every frontend, so the broader buffer checklist remains open.
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

Control-delivery evidence remains layered rather than end-to-end for every
consumer: the outbox saturation tests inject events directly into the transport;
`control_budget_exhaustion_marks_active_snapshot_truncated` checks the active
snapshot separately. On Linux and macOS, `control_byte_exhaustion_is_visible_in_finalized_run_snapshot`
pauses a provider and injects synthetic control events into a live run; it
verifies byte exhaustion, a `LimitReached` terminal, and a finalized snapshot
marked truncated. This does not establish that a normal tool workload generates
the same pressure or that a frontend displays it. The updated runtime suite
passed 378 tests with 2 skipped or ignored, all-target Clippy and formatting
passed on each platform. Runtime integration tests in `cov_topup` also establish that
control-consumer disconnection is reflected in finalized snapshots. Server snapshot
JSON exposes `content_truncated`, and `json_encoding` checks both flag values;
the existing HTTP flow checks the untruncated case. SSE clients must request the
snapshot themselves; no end-to-end HTTP/SSE test currently proves visibility of
an outbox-exhaustion truncation. These observations do not close the buffer gate.
The TUI now also retains a session-level incompleteness summary for frontend
merge gaps or rejected presentation events, waits for both event channels to
close, and initiates cancellation of active sessions concurrently before
reconciling them. Reconciliation and snapshot queries are still processed per
session with individual timeouts, not one global exit deadline; total exit
latency can grow with the number of sessions.
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
Linux first-command latency and the full release-mode comparison remain open.
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
The same macOS executable is 5,111,168 bytes (uncompressed file size), SHA-256
`bef2923c62ea1dde5e5415e3f88b320e8028a6d53a14233b3dbb52e6f02ce10f`; the
distribution/archive size is not measured. This is one host/build, not a
cross-platform size budget or accepted reference.
The same macOS release TUI had 10/10 observed three-second idle windows after
the composer frame. Sampled process CPU was 0.000–0.333% (median 0.166%); 118
live RSS samples had a median of per-run medians of 4,014,080 bytes and a
maximum sample of 4,505,600 bytes. These are sampled idle working-set points,
not peak startup or peak workload memory. Context switches were unavailable
and wakeups are `unsupported`. All ten sessions exited without cleanup
escalation after the explicit Ctrl+C sequence. The new sampled RSS summary
fields are covered by a performance-harness unit test.
A preliminary idle invocation that retained the harness's default extra
`m0-test` marker timed out 10/10 and is invalid, not counted as idle evidence.
The valid rerun required only the actual composer marker.

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
  cost than the 5 ms trial, but the harness cannot measure wakeups. The macOS
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
history access, or plugin activity. Idle wakeups, whole-run/internal buffer
high-water, attributable dispatch overhead, first-interactive
timing, and representative performance workloads remain open. Raw measurement
records are held in the temporary validation environment and must be retained
or reproduced before accepting a baseline.
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
