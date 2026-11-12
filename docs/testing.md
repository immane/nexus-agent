# Development tests

## Normal workflow

Release packaging has offline stdlib-only regression tests:
`python3 scripts/test-release.py`. These cover archive determinism, checksums,
installation, tampering and symlink rejection without compiling Rust. An actual
native artifact requires `python3 scripts/package-release.py --offline` and
an extracted-install smoke test on each intended platform; unit tests alone
do not establish binary compatibility.

Run checks for the affected packages, **sequentially**. Concurrent Cargo builds,
tests, and clippy commands share the target directory lock and do not accelerate
validation. Avoid checking the same code immediately before clippy (which already
checks it).

Small-context runtime regressions include metadata-only budget diagnostics for
pre-reduction, protected-floor, summary-candidate, retained-view, and final
request stages. `cargo nextest run -p nexus-runtime budget_ --locked --offline`
covers disjoint payload-byte accounting, previewed versus original results,
task/summary generation caps, unknown windows, and exclusion of private payload
text from diagnostics. These are heuristic estimates, not exact provider token
counts. `planning_output_reserve_tokens` is the task policy's headroom, including
on summary diagnostic lines; `request_max_output_tokens` is the separate cap on
that particular request. The retained view may be larger than the final
previewed request. Public event and persistence formats are unchanged.
For unknown windows, detailed budget diagnostics require a host-owned diagnostic
writer. The default scripted headless runner retains its existing stderr
contract; `nexus-headless` CLI tests cover separate and merged streams and a
closed stdout pipe without relaxing their output assertions.

Runtime output-projection regressions cover a 128 KiB old warning log under large
and unknown windows, an untouched latest complete non-command exchange,
preserved unknown effects/arguments/reasoning, unchanged source records, no
extra summary calls, opaque continuation bypass, stable first-exposure command
bounds, deduplicated diagnostic lines, and emergency truncation provenance.
`host_archive` coverage verifies exact bounded references, invalid or path-like
arguments, missing records, range and search reads, long-line truncation,
active-session binding, cross-session refusal, store reopen, original-source
retention, and end-to-end retrieval without re-execution.
Server JSON tests keep cache counters independent from input and preserve null
for unknown values. Runtime usage tests verify terminal cache merging. A
synthetic multi-turn replay compares
cumulative estimates, not measured provider usage. Prompt-cache regressions in
`nexus-openai::provider::prompt_cache_tests` compare the encoded system message,
tool definitions, and message prefix across growing transcripts for Chat
Completions and Anthropic Messages, and `nexus-context`/`nexus-runtime` tests
assert that per-turn estimates do not enter the note and that normal requests do
not rewrite already-sent tool output. DeepSeek's alternate
`prompt_cache_hit_tokens` counter is covered beside native cache fields; none of
these tests measures a live provider's cache hit rate. Existing reduction fixtures
retain non-output pressure so their uncertainty-pinning and actual compaction
assertions remain exercised.

The opt-in provider `task_reasoning_effort` setting has offline coverage in
`nexus-config`'s `task_reasoning_effort` test target, `nexus-openai`'s
`reasoning_effort_tests`, and the existing server configuration integration
module. These verify strict configuration roundtrips, preserved historical
reasoning, explicit summary-hint precedence, unchanged generation caps, endpoint
rejection before network I/O, and preservation/clearing through configuration
updates. They verify request encoding, not a live model's compliance or repair
quality. Benchmark launcher tests separately verify experiment metadata.
Layered-thinking regressions additionally cover model/provider/global precedence,
canonical inheritance, atomic rejection of incompatible model/provider changes,
global HTTP updates, inherited unsupported-adapter rejection in both Server and
TUI resolution, and per-run TUI snapshots. Web Node tests cover hydration without
writes, explicit setting/clearing, authentication, busy state and save errors.

```sh
cargo fmt --all --check
cargo nextest run -p nexus-tools -p nexus-container -p nexus-tui -p nexus-server --locked --offline
cargo clippy -p nexus-tools -p nexus-container -p nexus-tui -p nexus-server --all-targets --locked --offline -- -D warnings
```

Use only the relevant `-p` arguments. To test one integration module, use its
name (for example `cargo nextest run -p nexus-tools fs_patch --locked --offline`).
TUI, server, tools, and container integration tests share one `behavior` target
per package instead of linking an executable for every source file. New
integration modules in these packages must be added to `tests/behavior.rs`;
automatic test target discovery is disabled in their manifests.
The embedded Web client's regressions run separately with Node.js:
`node --test crates/nexus-server/tests/web_*.mjs`. They exercise settings
navigation, tool argument presentation, literal code content, bounded
replacement diffs, streaming/final output, disclosure states, and the install
prompt flow, saved-session rendering, live-thinking disclosure and bounded
streaming previews, and busy-state wiring with a small DOM
double. Provider settings coverage includes stored-token create/update, no
secret echo, deletion through the authenticated route, and a conflict when a
model still references the provider. Thinking also has a stylesheet contract
check for the clipped preview and bounded expanded panel; these do not replace
browser layout checks.
`nexus-openai`'s `runtime_keeps_wire_prefix_stable_as_occupancy_and_calibration_change`
regression captures loopback HTTP requests from the real runtime and adapter,
with a known window and usage-driven estimator calibration, rather than using
a hard-coded runtime note. It checks the generated system, tools, and message
prefix. Server saved-session
listing, history, restore, and continuation are covered by the server test
suite with a reopened file store.
The default Unix TUI suite also requires Python 3 for its pseudo-terminal
credential-startup regression; it uses an isolated FIFO and loopback listener.
The default `input_pressure` regression also uses Python 3, a real PTY and a
loopback-only SSE provider. It verifies that an 80-character burst (larger than
one input batch) appears intact while output continues, then checks draft
clearing and cancellation. Its ten-second watchdog is a functional deadlock
bound, not the 100 ms performance acceptance threshold. No live service or
user credentials are used.
On Linux the fixture also rejects native-readiness fallback and checks that the
observer does not change stdin's `O_NONBLOCK` bit. Linux-only `input_ready` unit
tests cover cancelled waits, stale readiness, later input, batch cooldown and
hangup errors. Native readiness requires the Linux Tokio I/O driver; other
platforms retain the periodic input path.

The default `credential_env` regression (macOS and Linux) also needs `cc`,
which Rust already requires as its linker. It compiles a temporary shim and
preloads it into the TUI (`DYLD_INSERT_LIBRARIES` / `LD_PRELOAD`) to record
the names, never the values, passed to libc `getenv`. Startup must not look up
the selected or an unselected provider's credential variable; a submission to
a loopback listener must look up the selected one. On macOS the shim also
records `open`, `connect` and process-creation calls: startup must open the
configuration but no `credentials.json`, internet connection or process. Calls
that bypass these libc entry points are not observed.

The TUI has an opt-in real-terminal rendering regression (Unix and Python 3):

```sh
cargo nextest run -j 1 -p nexus-tui --features demo session_commands_preserve_the_terminal_screen --run-ignored all --locked --offline
```

It connects stdout and stderr to the same pseudo-terminal and checks incremental
rendering at three sizes while opening commands, creating/listing/switching
sessions, and opening/closing help. It uses `--demo` with an empty configuration;
it does not call live providers or open the user's session store. Runtime
diagnostics must not write directly to a terminal owned by the TUI. Interactive
non-demo runs write bounded, metadata-only local logs automatically (see
[Quickstart diagnostics](../QUICKSTART.md#tui-diagnostics)). Demo runs do not open
the user's log directory; redirect stderr to retain their runtime diagnostics.
The `demo` feature is disabled by default. Run `cargo nextest run -p nexus-tui
--features demo --locked --offline` as well when changing demo behavior; the
default suite separately checks that `--demo` is rejected without the feature.

Development/test builds retain line-table debug information, debug assertions,
overflow checks, and incremental compilation. Dependencies omit debug information.
The test profile inherits these dev settings. Full local-variable/type debugging
is available with `cargo build --profile debugging` (or
`cargo test --profile debugging`). Release settings are unchanged. The first
build after changing profiles must rebuild cached artifacts; subsequent runs
reuse the new profile.

## Low-value category

Low value means **low additional regression coverage in everyday development**,
not permission to remove the test. These tests are preserved:

| Tests | Why optional | Default behavior |
| --- | --- | --- |
| TUI `cov_keys`, `cov_keys_edges` | Exhaustive census of the published **test-only** keymap; the executable's actual key handling is tested separately | `low_value` target requires `low-value-tests`; not compiled or run normally |
| TUI `terminal_entry_points_keep_their_signatures` | Compile-time signature pin, not terminal cleanup behavior | Ignored with a `low-value:` reason |
| TUI `crate_root_reexports_alias_the_decision_builders` | Root/module aliases, not approval binding behavior | Ignored with a `low-value:` reason |
| Fakes `root_reexports_alias_their_defining_modules` | Re-export aliases, not fake execution behavior | Ignored with a `low-value:` reason |
| Core `root_reexports_alias_defining_modules`, `root_traits_alias_defining_modules`, `root_constants_alias_defining_modules` | Re-export/type aliases; actual public API callers already compile | Ignored with a `low-value:` reason |
| Nine pre-existing TUI `keys::tests` checks | Already marked redundant with the optional keymap census | Remain ignored |

Do **not** classify all `cov_*` tests as low value. Approval/stale-grant checks,
filesystem confinement, cancellation, deadlines, quarantine, output budgets,
protocol validation, terminal restoration, and observable rendering regressions
remain in the default suite. Ignoring individual checks avoids running them but
does not avoid compiling their containing target; feature gating avoids both.

Run the low-value and ignored checks explicitly:

```sh
cargo nextest run --workspace --all-features --locked --offline --run-ignored all
```

CI runs the complete suite once, including default behavior tests. Routine local
validation does not enable `low-value-tests` or run ignored checks. Compile the
optional target when changing the test-only keymap or doing a complete review.

## Measuring slowness

Separate compilation, test discovery, and test execution. A nextest test summary
does not include build/discovery time. Use `cargo test --no-run --timings` to
inspect Cargo's report under `target/cargo-timings/`, and time `cargo nextest list`
separately. Compare the same package selection and cache state; do not label a
warm-cache number as a clean-build speedup. Do not delete the target cache merely
to measure a normal incremental workflow.

### macOS first-launch delay

If Cargo reports a completed build but nextest pauses before listing/running
tests, check [nextest's macOS notes](https://nexte.st/docs/installation/macos/).
XProtect/Gatekeeper can inspect freshly linked executables, so the same command
may be fast on its second invocation. This workspace reproduced long discovery
pauses after relinking, but has not proven the OS scanner to be the sole cause.
Reducing executable count helps reduce the work; Cargo profile settings alone
cannot eliminate an OS-level delay.

The documented **operator opt-in**, not a repository setting, is to enable
Developer Tools for the application actually launching Cargo (terminal, IDE, or
agent host). On systems where the pane is absent, the official instructions use
`sudo spctl developer-mode enable-terminal`, then System Settings → Privacy &
Security → Developer Tools, followed by restarting that application. This exempts
its child processes from some XProtect/Gatekeeper checks: understand that security
tradeoff first. Do not disable Gatekeeper globally, remove quarantine attributes,
or silently grant this permission from automation. No system security setting is
changed by this repository's test configuration.
