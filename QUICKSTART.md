# Nexus Agent — Quickstart

## Status snapshot

- Working tree: clean at `main` (`328e05f`), no uncommitted work.
- Implementation stage: **M0 test-only** — scripted fakes, ephemeral store; no real providers, tools, or plugins. **M0 acceptance is NOT complete** (see `docs/tasks/04-m0-gates.md`).
- Last verified gates (macOS, stable toolchain per `rust-toolchain.toml`):
  - Rust: **1684 passed / 0 failed** across 117 test targets
  - Python (`tools/perf`): **253 passed** (1 Linux-only test skipped on macOS)
  - `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo doc --workspace -- -D warnings`: clean
  - Measured line coverage of production `src/`: **97.5%** (`rustc -C instrument-coverage` + Xcode `llvm-cov`; remaining gaps need a real PTY or unreachable defensive branches)

## Prerequisites

- Rust stable (pinned via `rust-toolchain.toml`; needs `rustfmt` + `clippy` components)
- Python 3 (stdlib only) for `tools/perf`
- Xcode Command Line Tools on macOS if you want coverage (`llvm-cov`/`llvm-profdata`)
- No network needed: all checks run with `--locked --offline`

## 60-second start

```sh
cargo test --workspace --locked --offline --no-fail-fast
python3 -m unittest discover -s tools/perf -p 'test_*.py'
```

## Everyday commands

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo test -p <crate> --locked --offline          # one crate, e.g. nexus-runtime
cargo test -p <crate> --test <target> --locked --offline   # one test file
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --locked --offline
```

## Layout

| Path | What |
|---|---|
| `crates/nexus-core` | Domain types, errors, limits, ports. Dependency-free |
| `crates/nexus-validation` | Closed M0 JSON/schema validator for tool arguments |
| `crates/nexus-fakes` | Scripted deterministic provider/tool/store doubles (test-only) |
| `crates/nexus-runtime` | Single-active-run loop: policy, cancellation, quarantine, ordered events |
| `crates/nexus-tui` | Test-only TUI shell over scripted fakes |
| `crates/nexus-headless` | One-shot machine-output runner over scripted fakes |
| `crates/nexus-integration` | Cross-crate behavior and regression tests |
| `tools/perf` | stdlib-only startup/readiness measurement harnesses + their tests |
| `docs/` | Design, contracts, task gates; start at `docs/index.md` |

## Run the demos (fake-wired, no side effects)

```sh
cargo run -p nexus-headless --locked --offline -- "hello"
cargo run -p nexus-tui --locked --offline
```

Both print a test-only banner: no credentials, no network, no stored sessions.

## Perf harnesses

```sh
python3 tools/perf/startup.py <binary> --mode warm --json /tmp/perf.json -- --args
python3 tools/perf/readiness.py <binary> --help
```

Cold-cache runs need an explicit user-supplied `--purge-cmd`; without one they are labeled unprepared-cache, never genuinely cold. Cancel/input observations are observational markers only, not lifecycle proof. Details in `tools/perf/README.md`.

## Coverage (optional)

```sh
rm -f /tmp/nexus-cov/*.profraw
LLVM_PROFILE_FILE='/tmp/nexus-cov/%p.profraw' RUSTFLAGS='-C instrument-coverage' \
  cargo test --workspace --locked --offline --no-fail-fast
xcrun llvm-profdata merge -sparse /tmp/nexus-cov/*.profraw -o /tmp/nexus-cov/merged.profdata
# then: xcrun llvm-cov report --instr-profile /tmp/nexus-cov/merged.profdata \
#   $(for f in target/debug/deps/*; do ...) --ignore-filename-regex='/\.cargo/|/rustc/|registry/src'
```

## What NOT to expect

- No real model provider, no plugins, no durable history (ephemeral store only).
- Recorded performance numbers are historical characterization, not acceptance evidence; PTY/idle/streaming baselines are still pending.
- Same-toolchain Linux validation and exact toolchain pinning are still open blockers.
