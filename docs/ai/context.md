# Nexus Agent — Session Context

> Purpose: everything a fresh session needs to continue this work without
> re-reading the whole history. Factual snapshot; update it when the facts
> change. Working tree is clean unless noted otherwise.

## Where we are

- Branch `main`, HEAD `1ebef1d` (pushed; tree clean).
- Stage: **M0 test-only plus three deliberately narrow real boundaries**
  (plain-HTTP OpenAI-compatible adapter, root-jailed file reader, loopback
  HTTP server). **M0 acceptance is NOT complete** (`docs/tasks/04-m0-gates.md`
  still 8 unchecked / 0 checked).
- Last verified: Rust **1898 passed / 0 failed** (138 targets),
  Python `tools/perf` **253 passed** (1 Linux-only skip),
  `fmt --check` / `clippy --workspace --all-targets -D warnings` /
  `cargo doc --workspace -D warnings` clean.

## Architecture (what lives where)

| Crate | Role |
|---|---|
| `nexus-core` | Domain types, errors, limits, ports. Dependency-free. No behavior. `ProviderPort` offers opt-in incremental streaming: `stream_with_sink` (provisional text/usage only; candidates + terminals stay in the batch) with a replay-by-default, plus `supports_incremental_streaming` (default false). |
| `nexus-validation` | Closed M0 JSON/schema validator; strict duplicate-rejecting parse reused by config. |
| `nexus-validation` | Closed M0 JSON/schema validator; strict duplicate-rejecting parse reused by config. |
| `nexus-config` | Typed user config (providers, models, favourites, recents) + versioned JSON file persistence. Secrets only as env-var references; never stored, logged, or echoed. |
| `nexus-fakes` | Scripted provider/tool/store doubles. Exhaustion fails explicitly, never idle-succeeds. |
| `nexus-tools` | Real `host_read`@M0 against a canonicalized root jail. Escapes `Denied`/`NotStarted`; failures `Failed`/`Unknown`/`Uncertain`; budget-cut with flag; TOCTOU window documented. |
| `nexus-openai` | Real OpenAI-compatible chat adapter over blocking std I/O: `http` direct, `https` via the system `openssl s_client` TLS bridge (verified, no vendored TLS crate). Sends `stream:true`, parses SSE incrementally (`data:` chunks + `[DONE]`, chunked framing) with live text/usage sink delivery; candidates + terminal stay in the returned aggregated batch. Opts into incremental streaming. Cancellation/deadlines between read quanta; unknown usage stays unknown. |
| `nexus-runtime` | Single-active-run loop: scoped policy, live cancellation/deadlines, quarantined workers until termination, contiguous per-run sequencing, honest terminal outcomes. Publishes provisional provider prefixes live per run while the turn streams (text flushed at once, previews, usage estimates; candidates/terminals withheld to the validated batch; join-drain covers the channel race), then ingests the authoritative batch without replay. `Runtime::try_new` validates everything up front. |
| `nexus-server` | Loopback HTTP frontend (`127.0.0.1` only, no auth): one `Runtime` per session, SSE event streams (single subscriber, terminal-drain fix applied), exact-identity approve/deny, per-session provider selection, `--tools real|fake`. |
| `nexus-tui` | Interactive TUI: persistent multi-run loop, modal approval card (auto-focus on arrival, `i` inspect, `a`/`d` decide, `Esc` parks never cancels), slash commands (`/help /model /usage /quit`), viewport `m` model cycling, header shows project dir, composer shows model@provider, footer shows token counters (`?` never `0`). |
| `nexus-headless` | One-shot machine-output runner over scripted fakes. |
| `nexus-integration` | Cross-crate behavior + review regression tests. |

## Landed recently (newest first)

- `1ebef1d` QUICKSTART real-LLM flow (Ollama procedure verified live) + refreshed gates.
- `89c0261` server per-session provider selection (`POST /sessions {"provider","model"}`; 400/503/500 mapping; mock-backed e2e). Fixed a real SSE race: terminal drained predecessors before closing.
- `7523140` `nexus-openai` crate (14 mock-backed tests; caught a real macOS `WouldBlock`-vs-`TimedOut` bug).
- `7faeffe` TUI slash commands + composer placeholder (deliberately NOT `/always-approve`: violates exact-grant-only).
- `d5971b9` TUI chrome: project dir in header, model@provider in composer, ctx counters in footer.
- `ebc7c14` header project dir (= future tool jail scope, shared definition).
- `ce91e93` / `8624515` real jailed reads + server opt-in (`--tools real --tools-root`; default fake).
- `16d1395` / `a2e9a29` / `3b9fc51` / `bbfd4db` user config + server/TUI wiring.
- `9e0dda3` rename `nexus-web` → `nexus-server` (dir name kept out of wire ids: `sess-web-`, `req-web-` unchanged).
- `8d3ea98` `nexus-server` crate itself (hand-rolled HTTP/1.1 + SSE, zero new deps).

## Hard invariants (do not silently change)

- Approval = exact runtime tuple (`run`, `approval`, `call`); previews never authorize; no inferred identities; no persisted grants.
- Timeouts/cancellation keep workers owned (quarantine) until termination; effects honest (`Unknown`/`Uncertain`), never rollback claims.
- Unknown usage counters stay `null`/`None`, never zero. Truncation always flagged.
- Secrets: references only; static diagnostics; nothing secret-shaped in messages, logs, reports, or summaries.
- Test doubles fail explicitly on exhaustion; fakes never idle-succeed.
- One commit per theme, Conventional Commits, English; push to `origin/main` when asked (recent flow: commit+push proactively for asked work).

## Environment facts (verified, not assumed)

- Toolchain floats on `stable` via `rust-toolchain.toml`; it moved 1.94 → **1.99** mid-session (broke `llvm-cov` compat: rustc emits profile v11, Xcode tool expects v10 — re-measure coverage only after they agree).
- Offline: `--locked --offline` for all checks. Registry cache has NO http-client/TLS crates; tokio has no `net`/`io-util` available, so all sockets are blocking std I/O driven via `block_on` on a multi-thread runtime.
- `std::env::set_var/remove_var` are `unsafe` on this toolchain: tests use injection (`resolve_with`-style) instead of touching the environment.
- `set_var`-free rule + `#![forbid(unsafe_code)]` everywhere.
- macOS socket read timeouts surface as `WouldBlock`, not `TimedOut` (already bitten once; handle both).
- `cargo test --workspace --locked --offline --no-fail-fast` is the gate; Python via `python3 -m unittest discover -s tools/perf -p 'test_*.py'`.

## Open gaps (prioritized, honest)

1. Real provider streams incrementally end to end (wire SSE → provisional TUI/server deltas). Remaining provider work: per-run model override for real execution (session-level only; needs `ModelRequest` change).
2. No real tools besides `host_read`; no write/exec tools; TUI still fake-only for tools, and still a single in-process session (server multi-session picker wiring not started).
3. Ephemeral store only; no durable sessions/resume; no auth/TLS on server (localhost-only by design).
4. Linux same-toolchain validation, exact toolchain pin, PTY/idle/streaming baselines still open (M0).
5. `docs/contracts/05-configuration.md` ("no format selected") is now stale; contract-doc edits need explicit approval.
6. Docs: QUICKSTART is current; `docs/index.md` + gates file lag behind recent features.

## How to verify from scratch

```sh
cargo test --workspace --locked --offline --no-fail-fast
python3 -m unittest discover -s tools/perf -p 'test_*.py'
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --locked --offline
```

Live smoke (Ollama): see QUICKSTART.md “Test a real LLM”.
