# M0 Gates and Evidence

Status: Acceptance review in progress; **M0 is not complete**. Functional and
platform checks below have current evidence. Startup side-effect coverage,
complete buffer-limit evidence, and accepted P7 reference methodology and
measurements remain open. Historical RSS evidence is confounded and must not be
promoted to a baseline.

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
| Startup side effects | Isolated PTY tests on both platforms; Linux file/network/process traces for one normal and two corrupt-metadata cases | Sufficient evidence for the other startup/recovery and credential-access paths, including macOS |
| P7 reference and measurements | Two-platform warm composer observations, limited queue/dispatch and idle observations | Accepted reference environments and methodology; verified Linux cold-cache startup and remaining representative resource, throughput and buffer measurements |

Default workspace tests, Clippy and format checks were last run on both
platforms before the latest runtime **test-only** additions; the affected
runtime suite and Clippy were then rerun on both. This evidence is not a
full optional-feature run or a production security certification. A passing
warm-cache sample is not a cold-cache result or an accepted baseline.

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
unittest suite passed 278 tests with one
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
