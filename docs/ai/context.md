# Nexus Agent — Session Context

> Purpose: everything a fresh session needs to continue this work without
> re-reading the whole history. Factual snapshot; update it when the facts
> change. Working tree is clean unless noted otherwise.

## Where we are

- Branch `main`, HEAD `5aa81cc` (pushed; tree clean).
- Stage: **M0 test-only, dev-versioned, with narrowed real boundaries**
  (plain-HTTP OpenAI-compatible adapter incl. Ollama reasoning fields,
  root-jailed file reader+writer defaulting to cwd, loopback HTTP server).
  **M0 acceptance is NOT complete** (`docs/tasks/04-m0-gates.md`
  still 8 unchecked / 0 checked).
- M0 lock amended: tool calls **64/run, 16/turn**, retained context **256**
  (`docs/tasks/06-m0-lock.md` + tripwire test updated in step; dependent
  caps raised coherently: tool-definition aggregate 2MB, headless
  mandatory bytes 64MB).
- Last verified: Rust **2051 passed / 0 failed / 9 skipped** (131 targets;
  skips are the intentionally-ignored keymap redundancy layer, CI runs it
  via `--run-ignored ignored-only`), Python `tools/perf` **253 passed**
  (1 Linux-only skip), `fmt --check` / workspace `clippy -D warnings` /
  `cargo doc -D warnings` clean. Test execution is ~5s under nextest;
  wall time is dominated by rebuilds after core changes.

## Architecture (what lives where)

| Crate | Role |
|---|---|
| `nexus-core` | Domain types, errors, limits, ports. `SubmitCommand.read_only`; M0 budgets 64 calls/run, 16/turn, 256 retained context; tool-definition aggregate cap 2MB; snapshot outcome bound follows the call-budget const. |
| `nexus-validation` | Closed M0 JSON/schema validator; strict duplicate-rejecting parse reused by config. |
| `nexus-config` | Typed user config (providers, models, favourites, recents, **agent modes**) + versioned JSON file persistence. Modes: built-in `plan` (read-only) / `build`, customs via optional `modes` + `default_mode`; unknown ids and dangling defaults are explicit errors. Secrets only as env-var references; never stored, logged, or echoed. |
| `nexus-fakes` | Scripted provider/tool/store doubles. Exhaustion fails explicitly, never idle-succeeds. |
| `nexus-permissions` | Shared canonical project/temp/protected-path rules for runtime decisions and real adapters; session grants remain runtime-owned and non-persistent. |
| `nexus-tools` | Real file tools and mandatory OS-sandboxed `host_exec`. Development adapters accept exact canonical runtime scopes; project/temp operations are automatic, external file access needs approval/session directory grants. Exec reads broadly except known protected paths, writes project/temp or declared approved `write_dir`, and denies network. `--strict-tools` retains jailed reads and confirmation for mutations/exec. Listing is direct-child only; search recursively matches UTF-8 text; patch requires one exact match. TOCTOU limits documented. |
| `nexus-openai` | Real OpenAI-compatible chat adapter over blocking std I/O (`http` direct, `https` via system `openssl s_client`; no vendored TLS). Per-turn aggregate cap **8MB** (answer + reasoning + tool args). Accepts both `reasoning_content` (DeepSeek) and `reasoning` (Ollama) thinking fields into the same accumulator. Streams SSE incrementally, aggregates to one validated batch. Opts into incremental streaming. |
| `nexus-runtime` | Single-active-run loop: scoped policy, live cancellation/deadlines, quarantined workers, contiguous per-run sequencing. **Read-only runs deny confirmation-required tools at dispatch** (no prompt, no grant); automatic reads still execute. `Runtime::try_new` validates everything up front. |
| `nexus-server` | Loopback HTTP frontend (`127.0.0.1` only, no auth): one `Runtime` per session, SSE event streams, exact-identity approve/deny, per-session provider selection, `--tools real|fake`. |
| `nexus-tui` | Interactive TUI: persistent multi-run loop, local session registry (16 slots, `/session new\|list\|switch`), modal approval card, slash commands (`/help /model /session /usage /quit`), **agent modes** (`Tab` cycles plan/build/customs; composer border color + title name the mode), **model variants** (`Ctrl+T` cycles default/minimal/low/medium/high/xhigh/max; display-only, `flash(max)` with bright-yellow variant), **Markdown rendering** (pulldown-cmark subset: emphasis/code/lists/quotes/tables/footnotes/math/deflists, air rows between blocks, box-drawing tables, CJK cell-accurate wrapping via `unicode-width`), **drag selection** (press/drag/release; copy on release and on `Ctrl+C` via platform helper first — `pbcopy`/`wl-copy`/`xclip`/`xsel` — OSC 52 fallback; header toast confirms 2s, transcript stays clean), **Cargo-metadata version branding**, **`--demo` gate** (scripted fakes + canned submission only when flagged; otherwise unconfigured runs fail not-configured), **cwd tool jail by default**. Deps added: `pulldown-cmark`, `unicode-width` (both already in lock via ratatui graph; release ~3.0MB). |
| `nexus-headless` | One-shot machine-output runner over scripted fakes. Takes `--mode plan\|build\|<custom>` (HEADLESS `HeadlessMode` + `run_task_with_mode`; `mode=` stderr line); mandatory byte reserve 64MB. Still fake-wired only; no live path. |
| `nexus-integration` | Cross-crate behavior + review regression tests. |

## Landed recently (newest first)

- `5aa81cc` CI fix: uniform slice casts (Linux-only `cfg` type error invisible on macOS) + fmt normalization.
- `8f65f69` M0 budgets 64/16/256 + coherent dependent caps + lock doc/table sync.
- `fc7081e` Ollama `reasoning` field support (verified live against operator box; throwaway test deleted).
- `fb3e614` per-turn response cap 1MB → 8MB (boundary test now derives from const).
- `13864e7` model variant cycle on `Ctrl+T` (display-only).
- `d9ab360` dev versioning: `v0.1.0` branding, `--demo` gate, cwd tool jail default.
- `a29990b` copy on release with fading header toast (`TOAST_TTL` 2s, tick-polled expiry).
- `0a51e3a` boxed tables + air rows between Markdown blocks.
- `e11d39a` README frontend-usage section (modes, presentation, `--mode`).
- `331eab6` CJK cell-width wrapping + cell→char selection mapping.
- `d3c4915` Markdown rendering + drag-to-select with clipboard chain.
- `6f65441` nextest runner, parallel CI jobs, 9 ignored keymap-redundancy tests.
- `d1d896e` plan/build modes with runtime read-only enforcement.

## Hard invariants (do not silently change)

- Approval = exact runtime tuple (`run`, `approval`, `call`); previews never authorize; no inferred identities; no persisted grants.
- Timeouts/cancellation keep workers owned (quarantine) until termination; effects honest (`Unknown`/`Uncertain`), never rollback claims.
- Unknown usage counters stay `null`/`None`, never zero. Truncation always flagged.
- Secrets: references only; static diagnostics; nothing secret-shaped in messages, logs, reports, or summaries.
- Test doubles fail explicitly on exhaustion; fakes never idle-succeed.
- M0 lock table (`docs/tasks/06-m0-lock.md`) and its tripwire test must move together with any constant change.
- One commit per theme, Conventional Commits, English; push to `origin/main` when asked.
- No full-workspace suite unless the change touches shared constants; package-scoped validation by default.

## Environment facts (verified, not assumed)

- Toolchain floats on `stable` via `rust-toolchain.toml`; currently **1.99.0**. `llvm-cov` broke at profile v11 vs Xcode v10 — re-measure coverage only after they agree.
- Operator terminal: VS Code integrated (`TERM_PROGRAM=vscode`). OSC 52 clipboard does NOT land there — clipboard delivery must go through platform helpers (`pbcopy` on this machine).
- `Cmd+C` never reaches a terminal app (terminal intercepts); `Ctrl+C` (0x03) is the only copy key the TUI can see.
- Stray-line-below-footer symptom observed twice; app layout model proven clean by a two-frame TestBackend invariant test. Prime suspects, in order: a second `nexus-tui` alive in the same pty (two PIDs on different ptys were observed once — harmless there, fatal if same pty), IME preedit echo, terminal resize artifacts. Hygiene: one instance per terminal, rebuild latest, reproduce with steps.
- `std::env::set_var/remove_var` are `unsafe` on this toolchain: tests use injection (`resolve_with`-style) instead of touching the environment.
- `set_var`-free rule + `#![forbid(unsafe_code)]` everywhere.
- macOS socket read timeouts surface as `WouldBlock`, not `TimedOut` (handle both).
- Offline: `--locked --offline` for all checks. Registry cache has NO http-client/TLS crates; tokio has no `net`/`io-util`, so all sockets are blocking std I/O. `pulldown-cmark` + `unicase` + `unicode-width` ARE cached (verified by use).
- `x86_64-unknown-linux-gnu` rust-std installed locally: cross-`cargo check` any `#[cfg(target_os)]` change, since macOS builds cannot see the other platform's cfg arms (bitten once: Linux-only array-type error).
- `cargo test` serializes test binaries (~4min); `cargo nextest run` parallelizes (~5s exec). Rebuilds dominate wall time after core changes; never rewrite files without content change (mtime churn forces cascade rebuilds).
- Operator config lives at `~/.config/nexus-agent/config.json` (do not invent other paths): providers `deepseek` + `ollama-local`, models `deepseek-flash` + `qwen3-27b` (wire name carries the version tag; ids are `[A-Za-z0-9_-]` only, favourites must resolve). Credentials via `DEEPSEEK_API_KEY` / `OLLAMA_API_KEY` (dummy non-empty value accepted for Ollama).

## Open gaps (prioritized, honest)

1. Model variants are display-only; no request field carries them until a provider needs one.
2. Budgets still hardcoded (64/16/256); making them configurable is the next step (needs config schema + docs).
3. Real filesystem tools are `host_read`/`host_list`/`host_search`/`host_write`/`host_patch`; headless still fake-only; no live path for it. No exec tool; discuss only after filesystem tools are settled.
4. Ephemeral store only; no durable sessions/resume; no auth/TLS on server (localhost-only by design).
5. Linux same-toolchain validation, exact toolchain pin, PTY/idle/streaming baselines still open.
6. `docs/contracts/05-configuration.md` ("no format selected") is stale; contract-doc edits need explicit approval.
7. Docs: README is current on frontends/modes; `docs/index.md` + gates file lag behind recent features.

## How to verify from scratch

```sh
cargo nextest run --workspace --locked --offline --no-fail-fast
cargo nextest run --workspace --locked --offline --no-fail-fast --run-ignored ignored-only
python3 -m unittest discover -s tools/perf -p 'test_*.py'
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --locked --offline
```

Live smoke (Ollama): TUI with `qwen3-27b` selected, or direct `curl` to the box's `/v1/chat/completions`.
