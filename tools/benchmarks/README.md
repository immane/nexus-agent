# Local SWE-bench runner

Run from the Nexus repository root. Python 3 (standard library), Git, Cargo,
Docker Desktop/Engine and the cached task adapter images are required. The
default configuration is `~/.config/nexus-agent/config.json`. This is a local
model pilot, not a cloud benchmark launcher or a validated security boundary.

```sh
# Discover dataset rows and matching adapter images already on this machine.
python3 tools/benchmarks/swebench.py list

# One command: build server, prepare clean checkout/session, disconnect task
# networking, verify it, run the agent, save artifacts and clean up containers.
python3 tools/benchmarks/swebench.py run sympy__sympy-20590

# Exercise the same preparation and cleanup without invoking the model.
python3 tools/benchmarks/swebench.py run sympy__sympy-20590 --prepare-only

# Experiment with a larger per-request task generation cap, without changing
# the user's shared configuration. Context preparation reserves matching space.
python3 tools/benchmarks/swebench.py run sphinx-doc__sphinx-9461 --model-output-tokens 32768
```

`run` explicitly authorizes one live-model attempt. It never retries the whole
task. Select another ID from `list`; missing adapter images are shown rather
than silently pulled or substituted. All currently registered tasks have prior
exposure and **must not be reported as blind evaluation samples**.

## Environment and defaults

`local_profiles.json` records the reusable local selection, not secrets or
absolute machine paths. Change profiles or pass CLI overrides for another host.
The launcher reads dataset metadata from the sibling `../nexus-benchmark-work`
directory by default (`--work-root` overrides it). It examines only known task
metadata paths, not repositories or arbitrary model artifacts.

| Setting | Default / behavior |
| --- | --- |
| Provider / configured model ID | `ollama-local` / `qwen3-27b`; use `--provider`, `--model` to override |
| Wire model / endpoint | Resolved from selected user config, not hardcoded; the current pilot uses `qwen3.8-27b-iq3m-92k:latest` on a private Ollama endpoint |
| Credential | Default Ollama needs no user token; its model-list probe sends no Authorization header when unset. An internal dummy satisfies the existing server gate. Other providers require their configured credential; none is synthesized |
| Proxy | Append model endpoint and loopback to child process `NO_PROXY`/`no_proxy`; model identity preflight bypasses proxies |
| Server | Build release with `container` feature, locked/offline; `--no-build` reuses the binary |
| Listener | Free loopback port; `--port` overrides; existing servers are untouched |
| Gateway / workload | Cached profile tags resolved to immutable image IDs before launch; override via `--gateway-image`, `--container-image` |
| Model turns | Server's `--benchmark-pilot`: 500 |
| Tool calls / wall time | 4,096 / 86,400 seconds; lower with `--max-tool-calls`, `--max-run-seconds` |
| Token caps | None; provider final usage accounting is recorded, not a hard spend cap |
| Per-request output | Model config `max_output_tokens`, or legacy window-based default. `--model-output-tokens` writes a private run-local config; this is not a cumulative run token budget |
| Context window reference | 94,208 tokens from local profile; reference-only, not a tokenizer or provider configuration |
| Approval | Existing container policy; runner denies any approval request by default |
| Output | Server and runner stdout/stderr stream to the terminal **and** their log files, including partial model output; no separate `tail` required |
| Tool content logs | Launcher enables up to 8,192 bytes per tool event; `--tool-log-bytes 0` disables previews, maximum 65,536 |

The cached list currently covers SymPy 20590, Astropy 12907/13398, Django 11099,
Sphinx 9461 and pytest 10051. A task is available only when both its dataset row
and adapter image are present. To add another installed task, register its
instance ID, ordered cached image candidates and exposure note in the profile,
and put its canonical task row in `<work-root>/<id>.task.json` (or an existing
pilot's `task.json`). No dataset download or image build is hidden in `list`.

## Network split

Docker image builds, image pulls, and shallow task checkout preparation may
use networking. All dependencies needed by tests must already be in the task
adapter image; execution cannot install dependencies from the network.

Before **any task/model submission**, the launcher creates the session, verifies
its container IDs, labels, shared namespace and workspace mount, disconnects its
gateway from Docker networks, then checks there are no network attachments,
no IPv4 routes, and only loopback is UP. Inactive kernel tunnel devices may be
present. The workload has no NET_ADMIN capability, Docker socket, or host network.
The host server's connection to local Ollama remains available. The runner reuses
this exact session with `--session-id`; it does not create a new online session.
Verification failure stops before inference and cleans up only this launch.

Do not disable Docker's global networking or disconnect unrelated containers.
Disconnecting the session namespace is temporary: containers are removed at the
end, and later ordinary sessions retain their normal networking. This is not a
claim of independently validated native Linux isolation or resistance to every
container escape.

## Artifacts and cleanup

Each invocation creates a unique `<work-root>/run-<task>-<UTC>-<nonce>` directory:

- `launch.json`: OS/architecture, Python/Rust/Docker versions, source revision/diff fingerprint, binary/config
  hashes, resolved image IDs, exact server/runner argv, budgets, exposure label,
  network verification and cleanup outcome. Tokens are process-only, never argv.
- `task.json`: agent-safe metadata without gold patches or hidden evaluator data.
- `workspace/`: standalone shallow checkout at the task base, no upstream remote,
  future refs or external Git alternates. Old patched checkouts are not reused.
- `state/`: isolated container ownership records; never shared with other servers.
- `server.log`, `model-output.log`, `artifacts/`: diagnostics, result/prediction,
  per-run event log, manifest and derived metrics from the existing runner.
- `session-store/`: private, complete original interaction records, including
  reasoning, tool arguments and results. The launcher explicitly enables this
  archive so old unknown-effect exchanges can be summarized without losing
  their source evidence. Compact uncertainty facts and the latest three complete
  tool groups remain in active context. This archive is not redacted; treat it
  as sensitive even when bounded console previews have been redacted.
- `model-config.json`, when overriding task output tokens: private effective
  configuration used by this run; the original user config is not changed.
  Both configuration hashes and the selected cap are recorded in `launch.json`.
- `launcher-events.jsonl`: UTC-timestamped checkout, server, session, network,
  runner and cleanup stages. Structured runner events also carry UTC and elapsed
  timestamps, identities, tool status/duration/size/truncation, request budgets,
  usage, context reductions, approvals and terminal outcomes. Model output remains
  streamed rather than waiting for whole responses or newline boundaries.

Do not attach these logs publicly without review: task/model/tool output is not
guaranteed free of sensitive material. Workspaces and artifacts are preserved,
including failures and interrupted runs. Exit 0 means agent completion with a
nonempty patch, **not official resolution**. `--prepare-only` exit 0 means only
environment/network preparation succeeded. Cleanup errors exit nonzero and leave
identity evidence for manual reconciliation; no global Docker prune is used.
SIGINT/SIGTERM attempts bounded child shutdown and label-verified cleanup;
SIGKILL/power loss cannot guarantee cleanup. No new run takes over old resources.
Tool previews redact known credential environment values and Bearer tokens before
byte truncation, and explicitly mark log truncation separately from tool output
truncation. This is best-effort redaction, **not** a guarantee against arbitrary
secrets in task/model text or credentials split across streamed events. HTTP
headers, environment dumps and raw provider request bodies are not logged.
The standalone task runner defaults to omitting tool content unless explicitly
enabled; the isolated launcher enables the bounded previews for diagnosis.
Closed terminal pipes do not stop disk capture. Capture failures are recorded in
`launch.json.log_errors` rather than silently claiming complete logs.

## Scoring is separate

The launcher does not silently install or invoke the official evaluator. Score
the saved `artifacts/<id>.prediction.jsonl` with the pinned SWE-bench 5.0.2 harness
and the original canonical dataset (not the stripped `task.json`), with a **new
run ID**, one worker and the cached official task image. Preserve its report.
An empty patch, missing terminal result or evaluator failure is not a solved task.
See [the existing evaluation procedure and evidence](../../docs/research/agent-benchmarks.md).

```sh
python3 tools/benchmarks/run_nexus_swe_task.py summarize \
  --result RUN/artifacts/sympy__sympy-20590.result.json \
  --evaluation-report OFFICIAL_REPORT.json
python3 -B -m unittest discover -s tools/benchmarks -p 'test_*.py'
```

## Validation on this host

On macOS arm64 with Docker Desktop 29.4.3, `list` detected six installed task
profiles. A real `run sympy__sympy-20590 --prepare-only --no-build` created the
cached amd64 task workload, disconnected its network, verified the namespace and
cleaned up the exact gateway/workload pair without model inference. This does not
establish task success, correctness of every cached image, or Linux-host support.
The default build-enabled `--prepare-only` path was also exercised successfully.
