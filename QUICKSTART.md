# Nexus Agent — Quickstart

## Status snapshot

- See `git status` and `git log` for the current worktree and revision.
- Implementation stage: **0.2.3-alpha**, built on the M0 runtime:
  Chat Completions, OpenAI Responses, and Anthropic Messages adapters
  (`nexus-openai`, HTTP or
  HTTPS through the system `openssl` TLS bridge) and development-mode file
  tools (`host_read`/`host_list`/`host_search`/`host_write`/`host_patch`) plus
  sandboxed `argv` execution (`host_exec`: no shell, no network,
  `sandbox-exec` on macOS, `bwrap` on Linux) via `nexus-tools`, with shared
  project/temp/protected-path rules in `nexus-permissions`. Operations inside
  the project root and temp directories are automatic; other paths need
  approval, and `--strict-tools` restores project-jailed reads with per-call
  confirmation. No plugins, no durable history.
  **M0 acceptance is NOT complete** (see `docs/tasks/04-m0-gates.md`).
- The [migration record](docs/tasks/08-architecture-migration.md) documents
  implemented M4 consolidation slices and passing default-feature macOS
  workspace tests. Linux filesystem/metadata and sandbox-isolation validation
  remain blocked; escaped-descendant cleanup is still limited. Continued
  macOS-only development is not cross-platform security acceptance.
- Responses and Messages are covered by offline fixtures, not universal live
  compatibility evidence. The TUI and server can use configured live providers;
  headless remains scripted. See [Testing](docs/testing.md) for routine checks
  and the optional suite. CI's Ubuntu/macOS jobs do not replace the outstanding
  platform-specific security evidence.

## Prerequisites

- Rust stable (`rust-toolchain.toml` selects the floating `stable` channel,
  not an exact version; includes `rustfmt` + `clippy`)
- `cargo-nextest` for the normal test workflow; `cargo test` is a fallback
- Python 3 (stdlib only) for `tools/perf`
- System `openssl` on `PATH` for HTTPS providers; OS sandbox support for
  `host_exec` (`sandbox-exec` on macOS, `bwrap` on Linux; no unsandboxed fallback)
- Xcode Command Line Tools on macOS if you want coverage (`llvm-cov`/`llvm-profdata`)
- `--locked --offline` checks need the toolchain and locked dependencies
  already installed/cached. Initial setup may need network access. Cargo's
  `--offline` does **not** prevent runtime model API requests or billing.

## Start from source

```sh
cargo run -p nexus-headless --locked --offline -- "hello"
# Interactive terminal; configure a provider/model below before submitting:
cargo run -p nexus-tui --locked --offline
```

### Experimental container mode

An explicit prototype path is available with container-enabled builds. This
does **not** provide a validated isolation boundary: native Linux host/LAN,
metadata, IPv6/DNS, and public-egress checks remain outstanding. Do not use it
with secrets or valuable workspace data. The workspace is live-mounted and
container tool calls can modify it. See [`containers/README.md`](containers/README.md)
and the [container design](docs/design/11-container-execution.md) before opting
in. TUI mode switching is disabled in container execution; the writable
workspace mount remains active for the lifetime of the session. Local mode
remains the default.

```sh
# Replace both references with the exact immutable repo@sha256:digest values
# for locally available, reviewed development images.
cargo run -p nexus-tui --features container --locked --offline -- \
  --execution container --tools-root "$PWD" \
  --gateway-image 'gateway@sha256:<64-hex-digest>' \
  --container-image 'worker@sha256:<64-hex-digest>'

# The server additionally requires --tools real and its normal auth token.
cargo run -p nexus-server --features container --locked --offline -- \
  --tools real --execution container --tools-root "$PWD" \
  --gateway-image 'gateway@sha256:<64-hex-digest>' \
  --container-image 'worker@sha256:<64-hex-digest>' \
  --auth-token-env NEXUS_SERVER_TOKEN
```

These are experimental startup options, not a supported secure deployment
recipe. Images must already be available locally; startup does not pull them.
In container mode, `host_exec` accepts exactly one of `argv` (direct execution,
no shell parsing) or `shell_command` (run as `/bin/bash -c` inside the workload).
For example, use `{"shell_command":"pytest -q tests | head -60"}` for a pipeline;
prefix the command with `set -o pipefail;` when the pipeline should fail if an
earlier stage fails. Shell commands keep the same workspace, container
authorization, deadline, cancellation and output limits as `argv`; they do not
add host access. Each call starts a new shell, so shell state is not carried
between calls. This does not change host development-mode `host_exec`, which
remains argv-only behind its mandatory OS sandbox.

## Everyday commands

```sh
cargo fmt --all --check
cargo nextest run -p nexus-runtime --locked --offline
cargo clippy -p nexus-runtime --all-targets --locked --offline -- -D warnings
# Fallback without nextest:
cargo test -p nexus-runtime --locked --offline
# Tools/TUI/server modules are registered in a shared behavior target:
cargo test -p nexus-tools --test behavior fs_patch --locked --offline
python3 -m unittest discover -s tools/perf -p 'test_*.py'
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --locked --offline
```

Select affected packages and run Cargo commands sequentially against the shared
target directory. Routine checks use default features and skip ignored tests;
the full optional suite is documented in [Testing](docs/testing.md).

## Layout

| Path | What |
|---|---|
| `crates/nexus-core` | Domain types, errors, limits, ports. Dependency-free |
| `crates/nexus-validation` | Closed M0 JSON/schema validator for tool arguments |
| `crates/nexus-config` | Revision-2 provider/protocol/model/mode config, favourites/recents, revision-1 migration, file persistence |
| `crates/nexus-context` | Pure context estimation/calibration, safe reduction planning, and untrusted memory projection; no provider/tool I/O |
| `crates/nexus-fakes` | Scripted deterministic provider/tool/store doubles (test-only) |
| `crates/nexus-tools` | Real file tools and sandboxed `host_exec`: `host_read`/`host_list`/`host_search`/`host_write`/`host_patch` plus mandatory-OS-sandbox `argv` execution; development adapters accept exact canonical runtime scopes |
| `crates/nexus-container` | Opt-in container gateway/workload lifecycle, Docker worker transport, container tool composition, and worker/keeper binaries; depends on `nexus-tools` for filesystem executors |
| `crates/nexus-permissions` | Shared canonical project/temp/protected-path rules for runtime policy and real adapters; session grants stay runtime-owned and non-persistent |
| `crates/nexus-openai` | Streaming Chat Completions, Responses, and Messages adapters over HTTP/HTTPS |
| `crates/nexus-runtime` | Single-active-run loop: policy, cancellation, quarantine, ordered events |
| `crates/nexus-server` | Loopback HTTP frontend: sessions, SSE streams, config-gated selection |
| `crates/nexus-tui` | TUI with configured live providers or scripted demo wiring |
| `crates/nexus-headless` | One-shot machine-output runner over scripted fakes |
| `crates/nexus-integration` | Cross-crate behavior and regression tests |
| `tools/perf` | stdlib-only startup/readiness measurement harnesses + their tests |
| `docs/` | Design, contracts, task gates; start at `docs/index.md` |

## Scripted runs and demo fallback

```sh
cargo run -p nexus-headless --locked --offline -- "hello"
cargo run -p nexus-tui --locked --offline -- --demo
```

Headless uses scripted provider/tools, has no approval handler, and emits
percent-encoded `key=value` records, not JSON. A leading `--mode plan` selects
read-only mode; the default is `build`.

The TUI uses real development tools rooted at the working directory, even with
`--demo`: project/temp operations apply for real. Demo fallback is available
only when no configured model is selected; `--demo` does not override a live
model or guarantee a side-effect-free run. Without `--demo`, an unconfigured
submission fails explicitly instead of returning a fake reply. Neither
frontend has durable conversation storage.

## Use a configured model in the TUI

Configure a provider and model in the user configuration. Credentials can be
referenced by environment-variable name or stored separately by the config
helper. For environment references, export the variable **before starting the
TUI in the same shell**. No server is needed.

Configuration lookup uses `NEXUS_CONFIG` when nonempty, otherwise
`$XDG_CONFIG_HOME/nexus-agent/config.json`, falling back to
`$HOME/.config/nexus-agent/config.json` (also on macOS). The server additionally
accepts `--config PATH`, which takes precedence. The TUI intentionally has no
provider `--config` flag:

```sh
export NEXUS_CONFIG=/path/to/config.json
```

From the repository root, build and install the TUI, server, headless binary,
and interactive config helper to `~/.local/bin` (or pass another install
directory). Installation requires Cargo, Python 3, and the locked dependencies;
Cargo may use the network if they are not cached. The installed binaries use
default features; experimental container support is not included.

```sh
./scripts/install.sh
nexus-config
nexus-config provider
nexus-config model
nexus-tui
```

Run `nexus-config` for the interactive configuration menu. Provider presets
include DeepSeek, OpenCode Go, local Ollama, OpenAI, Anthropic, OpenRouter,
and custom endpoints.
Review the selected endpoint before supplying credentials. Deleting a provider
can also remove its associated models and their favourites/recent entries only
after confirmation; deletion of a stored key is a separate choice.
OpenCode Go requires an active subscription and an API key; Nexus identifies
its requests and sends a session identifier to the official Go endpoint. Model
availability and account privacy settings remain subject to the service.

To remove the installed commands while retaining configuration and credentials,
run `./scripts/uninstall.sh` and confirm. Pass the same install directory if you
used a custom one: `./scripts/uninstall.sh /path/to/bin`.

The helper uses the same `NEXUS_CONFIG`/XDG default path. It can save an API key
in a separate `$XDG_CONFIG_HOME/nexus-agent/credentials.json` file (or
`$HOME/.config/nexus-agent/credentials.json`), with file mode `0600`. Keys are
stored **in plaintext**, not encrypted; other processes running as the same
user, backups, and development tools with access to that path may read them.
Keep the credentials directory outside any workspace exposed to tools. The
helper can also retain an environment-variable reference instead. Python 3 is required at runtime;
the helper uses the installed `nexus-server` config validator before writing.
When editing a stored credential, leave the hidden API key prompt blank to keep
it, enter a new value to replace it, or enter `-` to delete it. A deleted key
must be replaced before that provider can authenticate. The credentials and
config files are saved separately; if the second save fails, the helper reports
that the credentials file was already updated.
Ensure the install directory is on `PATH`. Existing endpoint and token limits
can be cleared by entering `-`.
Avoid editing the same config concurrently; the helper detects changes made
while it is prompting, but cannot coordinate with other writers.
Provider and model entries can be selected by number or ID. When editing a
provider, the helper lists locally configured models and, if the endpoint and
credential environment variable are set, automatically requests the provider's
model list. This sends the credential to the configured endpoint, so use only a
trusted endpoint (HTTPS for remote services). Redirects are not followed;
unsupported or unavailable model-list APIs fall back to local/manual input.

Use the complete revision-2 example below. Revision 2 requires a provider
`protocol`: `chat_completions`, `openai_responses`, or `anthropic_messages`.
`adapter` (`direct` or `relay`) describes routing, not the protocol or an
implemented plugin system. Revision-1 files without `protocol` remain readable
as Chat Completions; saving writes revision 2. Missing config starts empty;
invalid config fails startup explicitly. Config saves do not create missing
parent directories, so create the chosen directory first.

### Layered operational settings

Runtime budgets are configured separately from the provider/model file, so TUI
usage saves cannot flatten operational overrides. Both TUI and server accept an
optional machine base (`/etc/nexus-agent/base.jsonc`, or `NEXUS_BASE_CONFIG`),
`--base-config PATH` to select a different base, and repeatable
`--config-overlay PATH` flags. Layers merge built-in profile defaults → base →
overlays in command-line order. Use `--check-config` to validate or
`--print-effective-config` to inspect resolved values without starting the UI or
server. Operational input uses JSONC (JSON with `//` and `/* */` comments);
effective output is plain JSON. Comments are recognized only outside strings,
so string contents are preserved. Trailing commas and other JSON5 syntax are
not supported. The separate provider/model `config.json` remains strict JSON
and is unchanged.

Complete example: [`config/base.jsonc`](config/base.jsonc). It lists every
configurable runtime budget, provider deadline/retry default, tool budget,
server lifecycle setting, and TUI preference. Its values
match the existing TUI and ordinary server defaults. Choose it explicitly:

```sh
cargo run -p nexus-tui --locked --offline -- --base-config config/base.jsonc
```

For example, a partial overlay can contain:

```jsonc
{
  "revision": 1,
  "defaults": {
    "runtime": {
      "max_tool_output_bytes": 131072
    }
  },
  "profiles": {
    "tui": {
      "runtime": {
        // Override only the TUI turn budget.
        "max_model_turns_per_run": 96
      }
    }
  }
}
```

The TUI and server profiles list every configurable core budget: turn and tool-call
limits, run/tool/approval timeouts, retained context, argument assembly and
tool output byte limits, event channel capacities, and concurrent operations.
Only `tui` and `server` profiles belong in the shared `base.jsonc`.
[`config/benchmark.jsonc`](config/benchmark.jsonc) is an ordinary settings file
with the same schema, provided as an example for benchmark budgets. Use it with
`--benchmark-pilot --base-config config/benchmark.jsonc`; the benchmark profile
also supplies these runtime limits by default. This file does **not** select
container execution, real tools, images, or credentials. A benchmark pilot
requires a container-enabled build, explicit container/real-tool selection,
immutable gateway and workload image references, and an auth-token environment
variable. For example, after preparing the images and provider config:

```sh
cargo run -p nexus-server --features container --locked --offline -- \
  --benchmark-pilot \
  --base-config config/benchmark.jsonc \
  --config /path/to/provider-config.json \
  --execution container --tools real \
  --gateway-image 'GATEWAY@sha256:DIGEST' \
  --container-image 'WORKLOAD@sha256:DIGEST' \
  --auth-token-env NEXUS_BENCH_SMOKE_TOKEN
```

Set `NEXUS_BENCH_SMOKE_TOKEN` and the provider credential referenced by the
selected user config in the environment of the server process; never put secret
values in config files or command arguments. Replace the image placeholders
with valid immutable references. See the [benchmark run notes](docs/research/agent-benchmarks.md)
for image preparation and workspace requirements. There is no benchmark-specific
config option, environment variable, or reserved file name. Provider sections
include `connect_timeout_seconds` (10),
`context_discovery_timeout_seconds` (2), `fallback_call_timeout_seconds` (600),
and `retry_backoff_seconds` ([1, 2, 5, 10, 30, 60]). Frontend presentation
overrides belong in top-level `tui`, `defaults.tui`, or `profiles.tui.tui`; nested partial
overrides inherit previous values. Unknown fields, invalid types, duplicate
keys, and invalid inactive profiles fail validation. Each file is limited to
65,536 bytes, with bounded reads. Plain operational JSON remains valid;
existing operational TOML must be converted to JSONC. Filename extensions do
not select the parser; operational settings have no TOML dependency.
Within each layer, defaults apply first, then top-level `tui`, then the selected
profile. Theme colors accept `default`, snake-case ANSI names, `#RRGGBB`, or
`index:0` through `index:255`; see `config/base.jsonc` for all presentation keys.
The effective snapshot reports merged settings, not a relaxation of existing
tool/runtime safety caps (for example, the core 262,144-byte tool-output cap).

Server lifecycle settings belong in `defaults.server` or `profiles.server.server`:
`max_connections` defaults to 64 (range 1–64), `max_sessions` to 128 (1–128),
`session_idle_seconds` to 1800, and `session_cleanup_interval_seconds` to 60
(both durations accept 1–86,400 seconds). Settings are applied at startup;
they do not change bind/authentication policy or tool permissions. Idle cleanup
still retains sessions with active work, streams, or external owners. Expiry is
checked periodically, so an eligible idle session may remain until the next pass.

Existing startup options can also have defaults under `defaults.launch` or the
selected profile's `launch`: `execution` (`local` or `container`),
`strict_tools`, `tools_root`, `server_port`, `server_tools` (`fake` or `real`),
`session_store`, and `auth_token_env` (an environment-variable **name**, never
the token). Container image references remain in the provider/model config or
CLI because that is where they are already stored. An explicit CLI option wins;
otherwise launch settings apply, then the existing built-in behavior. For
example, setting `execution` to `container` and the two image references allows
TUI startup without repeating `--execution container`; `--execution local`
still forces local execution. These are startup-only defaults, not hot reload or
permission-policy overrides. `strict_tools: true` can tighten execution; config
cannot disable strict mode selected by `--strict-tools`.

Tool budgets belong in `defaults.tools` or `profiles.tui.tools` /
`profiles.server.tools`: `list_entry_limit` defaults to 10,000 entries (range
1–10,000), and `search_file_bytes` to 1,048,576 bytes per file (1–1,048,576).
These startup settings apply to both strict/development host tools and container
file tools; they do not change approval or filesystem scope. Listing may return
a truncated subset when the cap is exceeded; directory enumeration happens
before sorting, so the subset is not guaranteed to be the first sorted entries.
Search examines a bounded prefix and reports truncation; an incomplete final
UTF-8 character at the byte boundary is excluded from that prefix.
Container workers now require protocol version 2. Rebuild workload images from
this revision before using container tools; old images fail explicitly rather
than ignoring configured budgets (see [container build instructions](containers/README.md#build-and-checks)).

```sh
cargo run -p nexus-tui --locked --offline -- --tools-root /path/to/project
```

`--tools-root` names the project root (default: the working directory) and
real tools are live by default in development mode: reads, listings,
searches, writes, patches, and sandboxed `host_exec` inside the project or
temp directories need no approval, while file access to other paths parks on
an approval card (`a` allows once, `s` grants that directory for the
session, `d` denies). `host_exec` takes an `argv` array with no shell, reads
broadly except protected credential paths, writes only the project/temp
roots or a declared approved directory, and has no network access.
`--strict-tools` restores the strict policy: project-jailed reads with
per-call confirmation for writes and exec. `--demo` only affects provider
wiring and transcript fallback, not tool mode. File tools now bind access to
descriptors and stage same-directory replacements: new-file creation is
no-clobber, multi-hardlink mutations are refused, and supported metadata must
be preserved or replacement fails closed. Atomic visibility is not crash
durability; external-writer conflict detection is best-effort, not a universal
race guarantee. These filesystem changes have macOS evidence only; Linux
validation remains blocked. See the [migration record](docs/tasks/08-architecture-migration.md).

`host_read` returns bounded UTF-8 **byte pages**, on both host and container
backends. Arguments are `path`, optional zero-based byte `offset` (default `0`),
and optional integer `limit` (default `8192`, range `4..16384` source bytes).
For example, `{"path":"CHANGELOG.md","offset":0,"limit":8192}` reads the
first page. When more bytes remain, the result has `truncated=true` and starts
with `[more; next_offset=N]`; continue with that exact `offset=N`. The header
is not file content. Pages may end mid-line, but never split UTF-8 characters
or skip unread bytes. Offsets are not line numbers; no line numbering or total
file size is reported. The output budget includes the continuation header and
may reduce the page size; a budget too small for a header plus one character
fails rather than emitting an unusable cursor. An offset inside a UTF-8
character is invalid; an offset at or beyond EOF returns empty content.
Reads do not snapshot the file, so concurrent edits can change later pages.
If later context compaction marks a result as a model-context projection, its
cursor still refers to the original page; re-read that page's original offset
to recover omitted middle content rather than treating the preview as complete.
Existing path scopes, protected-path restrictions, and jail checks still apply.

Changing the environment in another shell does not change a running TUI's
credentials; restart it after changing credentials.

Completed exchanges are retained in memory for follow-up tasks within the
same session. `:session new` starts isolated context; `:session list` and
`:session switch <target>` manage up to 16 in-memory sessions. Retention is bounded
by context-item limits and 1 MiB per runtime; oldest complete exchanges are
evicted when necessary, never individual tool-call/result pairs. Failed,
cancelled, and limited runs are not retained—even if a tool already changed
a file. Cancellation is not rollback; reread affected files before relying
on their state. Restarting loses this history; disk restore is not implemented.

Use `:model` (or `:model set <id>`) to select a configured model. Startup prefers
the first recent, then favourite, then configured model. `Tab` in the composer
cycles `build`, read-only `plan`, and custom modes; `default_mode` initializes
the TUI. Headless defaults to `build` unless a leading `--mode` is supplied.
`:help` shows commands and keys; `:q` exits. Slash-prefixed text is not a session
command.

The footer shows `idle` or `running`, the latest task request's estimated input
tokens and percentage of its known context window (for example `~12.3K (12%)`),
then the version at the right edge. `~` marks an estimate, not a live token
count; missing input or window information is shown as `--`. Summary requests
do not replace the task estimate.

### TUI diagnostics

Interactive non-demo runs automatically retain metadata-only runtime diagnostics:

- macOS: `~/Library/Application Support/nexus-agent/logs/tui-runtime.log`
- Linux/other Unix: `${XDG_STATE_HOME:-$HOME/.local/state}/nexus-agent/logs/tui-runtime.log`

The current file and one rotated file (`tui-runtime.log.1`) each hold at most
1 MiB; older records are overwritten on rotation, with no age-based retention.
An empty `tui-runtime.lock` serializes writes/rotation across local TUI processes.
Directories are owner-only (0700), files owner-only (0600); unsafe existing log
paths cause startup to fail rather than following symlinks or broadening access.
Records over 4 KiB are refused. Later write failures produce a TUI notice and
fall back to stderr only when it is redirected, never to the TUI-owned terminal.

Records contain timestamps, host-generated run/turn identifiers, byte/count
metrics, context budgets, output-limit recovery decisions, and terminal outcome,
error category, source-persistence status, duration, and delivery metadata.
They do **not** contain prompts, response/reasoning text, tool arguments/results,
provider payloads, credentials, or error-message text. Logging does not change
retry policy or budgets. Logs are not conversation archives or saved terminal
checkpoints: “source history saved” in the completion line refers only to source
history, not a saved terminal outcome. `--demo` does not open the user's logs;
redirect stderr to retain its diagnostics. Noninteractive behavior is unchanged.

### Automatic context management

All three provider protocols accept the same optional configuration fields.
At the document root, `context_management` defaults to:

```json
{
  "trigger_percent": 65,
  "target_percent": 10,
  "strategy": "summarize"
}
```

Percentages are integers and must satisfy
`0 < target_percent < trigger_percent < 100`; `target_percent: 5` is supported.
The other strategy value is `"truncate"`. A model entry may supply its own
`context_management` object, replacing the global policy. Omitted fields in
that object use the built-in defaults, not individual global fields.

A model entry may set `context_window_tokens`, for example `94208` for a
92 × 1024-token window. A provider may set the same field for its
`default_model` only; selecting another model does not inherit that limit.
Explicit model limits take precedence. Missing windows remain unknown;
zero, explicit null, and invalid types are rejected.

TUI/server pass these settings through the provider interface; the runtime
snapshots them at admission. A known window enables automatic reduction before
model requests. `summarize` uses an additional tool-free model turn to replace
eligible completed history with derived memory; `truncate` removes eligible
history without an extra model call. The target is best-effort: current tasks,
recent exchanges, and evidence of uncertain tool effects take precedence over
the requested percentage. Required instructions and tool definitions also
consume the window, so a 5% target may not be reachable.
Uncertain execution records remain intact, but do not block removal of later
safe, paired exchanges or separate completed tasks. If safe history reduction
still leaves window pressure, the model-only request view shortens older tool
output bodies to a UTF-8-safe head/tail preview (up to 2 KiB plus an explicit
omission notice). Call identities/arguments, status, effects, evidence, and
provider-required reasoning remain unchanged; the latest complete tool group
is normally not shortened. If that still leaves less than the normal generation
reserve, the request view also shortens the latest complete tool group's output
bodies as an emergency fallback, with the same metadata and omission markers.
Emergency previews shrink from 2 KiB down to 128 bytes of head/tail text per
result as needed; the omission notice is additional to that budget.
Projected output is marked truncated, and unknown effects
remain unresolved, never permission to retry. The source records are not
rewritten by this projection. Unknown windows and opaque provider continuations
do not enable it. If the retained evidence still exhausts the window, the run
stops rather than silently discarding it.

Input occupancy is an estimate, not a tokenizer-exact measurement or cumulative
run usage. It includes instructions, runtime notes, conversation/reasoning, and
tool schemas. Output tokens have a separate ceiling from response/tool bytes;
runtime reserves generation headroom rather than always requesting the legacy
byte-derived token limit. Normal turns reserve up to one quarter of the window,
capped at 16,384 output tokens; internal summaries are capped at 2,048 output
tokens and 8 KiB of retained text. A single generation can still reach its
output limit.

The initial input estimate has a 25% safety margin, increased when final
per-request provider input usage shows underestimation. Summary calls consume
model turns, usage, and run time like normal requests; their text is internal
derived memory, not a new user-facing answer or execution evidence. If a safe
summary cannot fit, fails validation, or leaves too little budget for continuing,
the runtime may fall back to safe truncation with an explicit omission notice.
Cancellation, deadline expiry, and exhausted limits do not authorize another
model call. Memory is bounded and session-local, not durable storage.

Window provenance distinguishes configured, provider-reported, and
model-metadata values. Configured `context_window_tokens` always wins. For
Ollama endpoints (port `11434` or a hostname containing `ollama`), the adapter
makes one bounded, best-effort read-only Ollama `/api/show` request when no
window is configured:
it prefers the model's `num_ctx` parameter, then falls back to an architecture
`context_length` field. This does not load a model or change Ollama settings.
Other protocols/providers, unsupported metadata, and discovery failures leave
the window unknown; no model-name inference is used. The discovered value is
cached for that selected model in the TUI process but is not written to config.
Unknown windows retain byte/item protections and do not claim token occupancy.
No live benchmark runs are started by this feature.

## Test a real LLM (local Ollama, end to end)

The deterministic tests live in `cargo test -p nexus-openai --locked --offline` (loopback
mock, no external API). To run a live model, use an OpenAI-compatible
endpoint such as Ollama. HTTPS endpoints require the system `openssl`
executable; certificate and hostname verification remain enabled.

```sh
ollama pull llama3.1
export OLLAMA_API_KEY=dummy   # local Ollama ignores it; the credential gate still requires it set
export NEXUS_SERVER_TOKEN="$(openssl rand -hex 32)"
export NEXUS_CONFIG="$(mktemp)"
cat > "$NEXUS_CONFIG" <<'EOF'
{
  "revision": 2,
  "providers": [
    {
      "id": "ollama",
      "display_name": "Ollama (local)",
      "adapter": "direct",
      "protocol": "chat_completions",
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
# TUI alternative: run this instead of the server in the same shell:
# cargo run -p nexus-tui --locked --offline -- --tools-root /path/to/project
cargo run -p nexus-server --locked --offline -- --port 8471 --config "$NEXUS_CONFIG" --auth-token-env NEXUS_SERVER_TOKEN
```

The temporary config is selected for this shell's runs; remove it after use,
or copy it into your chosen persistent config location. Live calls require
Ollama running locally. The server listens only on `127.0.0.1`; configured
providers or real tools require the 64-hex-character bearer token. This token
grants server-wide route access, including approvals; session IDs are not
credentials. Only the fake-only server demo may run without authentication.

In the server terminal, keep the generated token available through your
secret manager or another secret-safe channel. In the client terminal, read
it without echoing or putting it in shell history (then export it):

```sh
read -s NEXUS_SERVER_TOKEN
export NEXUS_SERVER_TOKEN
```

Session tools stay scripted fakes unless `--tools real` is also passed. This
creates a private, temporary curl config so the expanded credential is not
present in curl's command-line arguments. The trap removes the file on exit:

```sh
umask 077
AUTH_CONFIG=$(mktemp)
trap 'rm -f "$AUTH_CONFIG"; unset NEXUS_SERVER_TOKEN' EXIT
printf 'header = "Authorization: Bearer %s"\n' "$NEXUS_SERVER_TOKEN" > "$AUTH_CONFIG"
SID=$(curl -s --config "$AUTH_CONFIG" -X POST localhost:8471/sessions -d '{"provider":"ollama","model":"local"}' \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['session'])")
RID=$(curl -s --config "$AUTH_CONFIG" -X POST localhost:8471/sessions/$SID/runs -d '{"input":"say hi in five words"}' \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['run'])")
curl -sN --config "$AUTH_CONFIG" localhost:8471/sessions/$SID/runs/$RID/events
```

Expect the model's text, then a terminal `completed`. If a tool call requires
confirmation, an `approval-required` event appears: approve it with its
exact ids (`POST .../approve {"approval":"...","call":"..."}`) and the
run continues. External-path approvals additionally accept
`"scope":"session-directory"`, which grants the runtime-published directory
for that session only. Real file tools additionally need
`--tools real --tools-root <dir>` (development mode by default,
`--strict-tools` for the strict policy): project/temp operations are
automatic, other paths need approval, and paths to protected credential
files are refused.

Session provider/model selection is fixed at creation. The `model` field is a
configured model ID (`local` here), not arbitrary vendor text. Run submissions
may omit selection or repeat the binding; conflicting selections fail `400`.
The server does not expose the TUI/headless agent-mode selector.

API sanity check that does not invoke a model:

```sh
curl -s --config "$AUTH_CONFIG" -X POST localhost:8471/sessions -d '{"provider":"ghost"}'       # 400 unknown
```

To test the `503` credential gate, stop the server, unset `OLLAMA_API_KEY` in
its environment, restart it with the same config, and try to create the Ollama
session again. Session creation fails before any provider network request.

## Use a hosted model

Same configuration shape with an `https` endpoint; export the referenced
variable **before starting** (TUI or server) in the same shell. The value
is referenced by name only, never embedded in JSON. Replace the endpoint and
model placeholders with values supported by your service:

```json
{
  "revision": 2,
  "providers": [
    {
      "id": "hosted",
      "display_name": "Hosted provider",
      "adapter": "direct",
      "protocol": "chat_completions",
      "endpoint": "https://YOUR_PROVIDER_HOST/v1",
      "credential": {"env": "HOSTED_API_KEY"},
      "default_model": "YOUR_MODEL_NAME"
    }
  ],
  "models": [
    {"id": "hosted-model", "provider": "hosted", "name": "YOUR_MODEL_NAME"}
  ],
  "favourites": ["hosted-model"],
  "recent": []
}
```

Set `HOSTED_API_KEY` securely in the launching shell, then use
`:model set hosted-model` in the TUI; requests are billable. Follow-up tasks
reuse bounded context. Thinking/continuation data is protocol-specific;
unsupported replay is rejected, not silently rewritten.

For Responses or Messages, use `openai_responses` or `anthropic_messages` and
the corresponding service base URL/model. Chat Completions appends
`/chat/completions`; Responses and Messages append `/responses` or `/messages`
when the base ends in `/v1`, otherwise `/v1/responses` or `/v1/messages`.
Do not supply the complete request URL as the base. These adapters have offline
fixtures; verify the particular service/model before relying on compatibility.

## Perf harnesses

```sh
python3 tools/perf/startup.py <binary> --mode warm --json /tmp/perf.json -- --args
python3 tools/perf/readiness.py <binary> --help
```

Cold-cache runs need an explicit user-supplied `--purge-cmd`; without one they are labeled unprepared-cache, never genuinely cold. Cancel/input observations are observational markers only, not lifecycle proof. Details in `tools/perf/README.md`.

## Coverage (optional)

Coverage is measured on demand, not a published fixed percentage. Compile and
test with `RUSTFLAGS='-C instrument-coverage'`, directing `LLVM_PROFILE_FILE`
to a fresh private directory with a `%p.profraw` filename. Merge profiles with
`llvm-profdata`, then report against the corresponding instrumented test
executables with `llvm-cov`. Both LLVM tools must match the Rust compiler's
LLVM version; macOS `xcrun` tools are usable only when they match. Do not mix
profiles or executable artifacts from different builds. This is separate from
the routine default-feature test workflow.

## What NOT to expect

- Provider transport covers HTTP and HTTPS (HTTPS via the system `openssl`
  bridge) with protocol-specific streaming SSE; Gemini, external plugins,
  and durable history are not implemented (ephemeral store only).
- Recorded performance numbers are historical characterization, not acceptance evidence; PTY/idle/streaming baselines are still pending.
- No Linux security-readiness or complete descendant-cleanup claim: the
  migration record's platform blockers and cleanup limitations still apply.
- CI has Ubuntu/macOS checks; exact toolchain pinning is still open. Coverage
  tools must match the Rust compiler's LLVM version; Xcode tools may not match.
