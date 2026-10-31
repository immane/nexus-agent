# M0 Gates and Evidence

Status: Acceptance review in progress; **M0 is not complete**. Functional and
platform checks below have current evidence. Startup side-effect coverage,
complete buffer-limit evidence, and accepted P7 reference methodology and
measurements remain open. Historical RSS evidence is confounded and must not be
promoted to a baseline.

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
  test-only hook is absent from production builds. Linux measurement and a
  representative multi-run workload remain outstanding.
- The release-mode integration test
  `backpressure::paused_receivers_measure_queue_peaks_at_approval_gate` passed
  on both platforms. With 1,500 alternating 64-byte fragments and both receivers
  paused until the approval gate, it observed receiver queue peaks of 1,024 data
  events and 5 control events. This is an exact peak for that gated phase only;
  it excludes the internal control outbox and all later run phases.

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
