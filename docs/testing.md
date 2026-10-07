# Development tests

## Normal workflow

Run checks for the affected packages, **sequentially**. Concurrent Cargo builds,
tests, and clippy commands share the target directory lock and do not accelerate
validation. Avoid checking the same code immediately before clippy (which already
checks it).

```sh
cargo fmt --all --check
cargo nextest run -p nexus-tools -p nexus-tui -p nexus-server --locked --offline
cargo clippy -p nexus-tools -p nexus-tui -p nexus-server --all-targets --locked --offline -- -D warnings
```

Use only the relevant `-p` arguments. To test one integration module, use its
name (for example `cargo nextest run -p nexus-tools fs_patch --locked --offline`).
TUI, server, and tools integration tests now share one `behavior` target per
package instead of linking an executable for every source file. New integration
modules in these packages must be added to `tests/behavior.rs`; automatic test
target discovery is disabled in their manifests.

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
