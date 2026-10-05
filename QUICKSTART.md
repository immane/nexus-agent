# Nexus Agent — Quickstart

## Status snapshot

- See `git status` and `git log` for the current worktree and revision.
- Implementation stage: **M0 test-only with opt-in live integrations**:
  an incremental OpenAI-compatible chat adapter (`nexus-openai`, HTTP or
  HTTPS through the system `openssl` TLS bridge) and root-jailed file
  reads/writes (`nexus-tools`). No plugins, no durable history.
  **M0 acceptance is NOT complete** (see `docs/tasks/04-m0-gates.md`).
- Historical gates (macOS; these counts predate the live integration fixes):
  - Rust: **1898 passed / 0 failed** across 138 test targets
  - Python (`tools/perf`): **253 passed** (1 Linux-only test skipped on macOS)
  - `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo doc --workspace -- -D warnings`: clean
  - Measured line coverage of production `src/`: **~97%** (`rustc -C instrument-coverage` + Xcode `llvm-cov`; re-measure after toolchain moves, as the two versions must agree; remaining gaps need a real PTY or unreachable defensive branches)

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
| `crates/nexus-config` | Typed user config (providers, models, favourites, recents) + file persistence |
| `crates/nexus-fakes` | Scripted deterministic provider/tool/store doubles (test-only) |
| `crates/nexus-tools` | Opt-in root-jailed `host_read` and approval-gated `host_write` |
| `crates/nexus-openai` | Streaming OpenAI-compatible chat adapter over HTTP/HTTPS |
| `crates/nexus-runtime` | Single-active-run loop: policy, cancellation, quarantine, ordered events |
| `crates/nexus-server` | Loopback HTTP frontend: sessions, SSE streams, config-gated selection |
| `crates/nexus-tui` | TUI with configured live providers or scripted demo wiring |
| `crates/nexus-headless` | One-shot machine-output runner over scripted fakes |
| `crates/nexus-integration` | Cross-crate behavior and regression tests |
| `tools/perf` | stdlib-only startup/readiness measurement harnesses + their tests |
| `docs/` | Design, contracts, task gates; start at `docs/index.md` |

## Run the demos (fake-wired, no side effects)

```sh
cargo run -p nexus-headless --locked --offline -- "hello"
cargo run -p nexus-tui --locked --offline
```

The headless binary uses fakes. The TUI uses fakes only when no configured
model is selected; a configured model can send paid requests. Neither has
durable session storage.

## Use a configured model in the TUI

Configure a provider and model in the user configuration, with credentials
referenced by environment-variable name, not embedded in JSON. Export that
variable **before starting the TUI in the same shell**. No server is needed.

```sh
cargo run -p nexus-tui --locked --offline -- --tools-root /path/to/project
```

With `--tools-root`, `host_read` reads real UTF-8 files and `host_write`
creates or replaces real files. Writes require exact-call approval (`a` in
the approval card); parent directories must already exist. Without the
flag, tools remain scripted and do not modify files. Directory listing,
search, and shell execution are not implemented. The path jail is not an
OS sandbox and retains a check/open race against concurrent path changes.

Changing the environment in another shell does not change a running TUI's
credentials; restart it after changing credentials.

Completed exchanges are retained in memory for follow-up tasks within the
same session. `/session new` starts isolated context. Retention is bounded
by context-item limits and 1 MiB per runtime; oldest complete exchanges are
evicted when necessary, never individual tool-call/result pairs. Failed,
cancelled, and limited runs are not retained—even if a tool already changed
a file. Cancellation is not rollback; reread affected files before relying
on their state. Restarting loses this history; disk restore is not implemented.

## Test a real LLM (local Ollama, end to end)

The deterministic proof lives in `cargo test -p nexus-openai` (loopback
mock, no external API). To run a live model, use an OpenAI-compatible
endpoint such as Ollama. HTTPS endpoints require the system `openssl`
executable; certificate and hostname verification remain enabled.

```sh
ollama pull llama3.1
export OLLAMA_API_KEY=dummy   # local Ollama ignores it; the credential gate still requires it set
cat > /tmp/nexus-config.json <<'EOF'
{
  "revision": 1,
  "providers": [
    {
      "id": "ollama",
      "display_name": "Ollama (local)",
      "adapter": "direct",
      "endpoint": "http://localhost:11434/v1",
      "credential": {"env": "OLLAMA_API_KEY"},
      "default_model": "llama3.1"
    }
  ],
  "models": [
    {"id": "local", "provider": "ollama", "name": "llama3.1"}
  ],
  "favourites": ["local"],
  "recent": []
}
EOF
cargo run -p nexus-server --locked --offline -- --port 8471 --config /tmp/nexus-config.json
```

In another terminal (session bound to the configured provider; tools
stay scripted fakes unless `--tools real` is also passed):

```sh
SID=$(curl -s -X POST localhost:8471/sessions -d '{"provider":"ollama","model":"local"}' \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['session'])")
RID=$(curl -s -X POST localhost:8471/sessions/$SID/runs -d '{"input":"say hi in five words"}' \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['run'])")
curl -sN localhost:8471/sessions/$SID/runs/$RID/events
```

Expect the model's text, then a terminal `completed`. If the model
calls a tool, an `approval-required` event appears: approve it with its
exact ids (`POST .../approve {"approval":"...","call":"..."}`) and the
run continues. Real file tools additionally need
`--tools real --tools-root <dir>`: reads are automatic within policy scope,
writes require approval, and paths outside the root are refused.

Sanity checks that need no model at all:

```sh
curl -s -X POST localhost:8471/sessions -d '{"provider":"ghost"}'       # 400 unknown
curl -s -X POST localhost:8471/sessions/$SID/runs -d '{"input":"x"}'    # fake demo path still works
```

Unset `OLLAMA_API_KEY` and submit with `"provider":"ollama"` to see the
`503` credential gate: no socket ever opens without a referenced secret.

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

- Provider transport is plain HTTP only (no TLS, no streaming SSE from the
  model side); plugins and durable history do not exist (ephemeral store only).
- Recorded performance numbers are historical characterization, not acceptance evidence; PTY/idle/streaming baselines are still pending.
- Same-toolchain Linux validation and exact toolchain pinning are still open blockers.
