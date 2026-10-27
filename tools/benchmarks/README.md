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

**Local pilot comparison baseline:** Qwen3.8-27B, **IQ3_M quantization**, and a
configured **92K context window (94,208 tokens, input plus output)** through
Ollama (`qwen3.8-27b-iq3m-92k:latest`), not BF16 or 128K context. This describes
the local pilot setup, not a launcher requirement or a guarantee for overridden
models; use each run's recorded configuration when comparing results.

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
| Per-tool deadline | 3,600 seconds in `--benchmark-pilot` mode, clamped to the remaining runtime run budget; timeout still preserves uncertain-effect handling and quarantine |
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
nonempty patch, **not official resolution**. Before inference (including in
`--prepare-only` mode), the launcher exercises a read-only paged `host_read`
against the detached checkout's `.git/HEAD` through the container worker.
Incompatible or broken workers stop preparation with a rebuild diagnostic; rebuilding
the server alone does not update workers embedded in cached task adapter images.
Successful evidence is recorded under `worker` in `launch.json`.
`--prepare-only` exit 0 means environment/network/worker preparation succeeded,
not that tests passed. Cleanup errors exit nonzero and leave
identity evidence for manual reconciliation; no global Docker prune is used.
SIGINT/SIGTERM attempts bounded child shutdown and label-verified cleanup;
SIGKILL/power loss cannot guarantee cleanup. No new run takes over old resources.
If runtime quarantine already removed a workload and cleared its durable ID,
the launcher verifies the exact container name is absent before continuing gateway
cleanup. A surviving container with a missing ID still requires manual inspection.
Tool previews redact known credential environment values and Bearer tokens before
byte truncation, and explicitly mark log truncation separately from tool output
truncation. This is best-effort redaction, **not** a guarantee against arbitrary
secrets in task/model text or credentials split across streamed events. HTTP
headers, environment dumps and raw provider request bodies are not logged.
The standalone task runner defaults to omitting tool content unless explicitly
enabled; the isolated launcher enables the bounded previews for diagnosis.
Closed terminal pipes do not stop disk capture. Capture failures are recorded in
`launch.json.log_errors` rather than silently claiming complete logs.
During idle SSE periods, server pings produce a rate-limited `[heartbeat]` report
(at most once per 30 seconds) with active call durations and time since the last
observed `tool-output` event. After 120 seconds without such an event, a call is
marked `quiet`, not failed or stalled. Heartbeats do not reset tool deadlines or
prove test progress; they are console metadata, not sequenced runtime events.
Configured runner budgets are checked on heartbeats too, so silent tools cannot
defer the runner's wall-clock cancellation until their completion.
Available `tool-output` byte counts are shown without content by default; raw
previews still require `--tool-log-bytes`. The container executor currently
collects stdout/stderr until completion: these reports do not add live stdout
streaming. A case log and framework stack diagnostics remain necessary to locate
a hung test; silence alone never triggers automatic cancellation or retry.

## Task test execution guidance

Benchmark-pilot and ordinary TUI/server calls share the runtime's timeout
handling. A per-tool deadline leaves up to two seconds for cooperative cleanup,
clamped to the remaining run budget; it does not extend execution authority.
A returned timeout is recorded as a tool result so the model can continue.
An unreturned worker after per-tool cleanup grace is quarantined and recorded as
`TimedOut` / `Unknown` / `Uncertain`, allowing the model to continue. New tool
dispatch is denied (`NotStarted`) while any quarantined worker remains;
background ownership is retained until completion. Cancellation and run-budget
exhaustion still end the run. Container transport expiry remains conservative:
the shared session is poisoned and owned
workload cleanup is requested, not silently replaced or declared reusable.
The runner follows the authoritative run terminal event; an intermediate
`tool-finished` with `TimedOut` does not itself cancel or fail a benchmark.

The task prompt follows each checkout's documented test runner, not an assumed
framework. It directs the agent to run the targeted regression first, then the
related suite, and the full suite only when justified by the change and remaining
budget. Test output must preserve the exact test process exit status: prefer a
stdout/stderr log with the status captured before inspecting it. A `tail`/`head`
pipeline must not hide failure; Bash pipelines need `pipefail` and the real test
status captured from `PIPESTATUS`, rather than the output filter's status.

Keep case names/results in a progress log so the last started/completed case is
identifiable. Runner heartbeats establish liveness, not actual test progress.
Use a bounded execution deadline and supported stack diagnostics before it. For
checkouts using a compatible installed pytest with its built-in faulthandler
plugin, `-vv -o faulthandler_timeout=120` records case names and dumps thread
stacks after a test exceeds 120 seconds; choose an interval below the execution
deadline. This diagnostic **does not kill a hung test** and requires no new plugin.
See pytest's official [verbosity documentation](https://docs.pytest.org/en/stable/how-to/output.html#verbosity)
and [fault handler documentation](https://docs.pytest.org/en/stable/how-to/failures.html#fault-handler)
(built in since pytest 5.0); verify support in the task's historical environment.

After a timeout, inspect logs/stacks, identify the stalled case or phase, and
narrow the diagnostic run rather than repeating the same full-suite command
unchanged. Report timeout/incomplete coverage, not a pass. This guidance does
not authorize weakening tests, skipping failing cases, changing expectations to
pass, enabling external networking, or downloading dependencies/plugins.

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

## Ten-task diagnostic suite

`swe_suite.json` fixes the diagnostic selection/order, dataset revision, evaluator
version, and initial budgets. This is a development set, not a representative
leaderboard sample or a claim of blind evaluation. The ten tasks cover config
compatibility, CLI failures, explicit empty values, nested redaction, missing
masks, redirect state, incorrectly skipped tests, array round trips, empty ML
pipeline branches, and implicit plotting state changes.

```sh
# Inspect the portable selection; no Docker, network, or model requests.
python3 tools/benchmarks/swe_suite.py list

# Fetch the pinned canonical metadata. Gold/tests remain outside task mounts.
python3 tools/benchmarks/prepare_swe_suite.py

# Explicitly pull official amd64 images and build Nexus adapters.
python3 tools/benchmarks/prepare_swe_suite.py --images

# After worker source changes, explicitly rebuild recorded adapters, verifying
# existing image identities first. No unrelated tags/images are overwritten.
python3 tools/benchmarks/prepare_swe_suite.py --images --rebuild-images

# Build the server once; sequentially verify each fresh checkout, isolated
# session, network disconnection, source imports, and cleanup. No inference.
python3 tools/benchmarks/swe_suite.py prepare

# Explicitly authorize one live attempt per selected task, sequentially.
# Defaults: 24 hours / 4096 tools per task, no automatic task retries.
python3 tools/benchmarks/swe_suite.py run

# Explicit tight-budget efficiency diagnostic; do not mix with normal runs.
python3 tools/benchmarks/swe_suite.py run --max-run-seconds 1800 --max-tool-calls 150

# Restrict preparation or inference to a suite member.
python3 tools/benchmarks/swe_suite.py run --task pallets__flask-4992

# Read existing batch accounting without any external operations.
python3 tools/benchmarks/swe_suite.py summarize --batch BATCH/batch.json
```

Use the same `--work-root` for metadata preparation and batches. Preparation
defaults to the separate sibling `../nexus-benchmark-work/lite-diagnostics-v1`
directory; the original single-task launcher retains its older work-root default.
Preparation
generates a host-local `suite-profiles.json`; `--profiles` can override it.
Each batch writes its own `batch.json`, `summary.json`, per-task launcher log,
and independent attempt directory. All selected tasks, including unstarted
ones, remain in accounting after interruption. Preparation checks the entire
selection for metadata/images before starting any model work. Cleanup
uncertainty stops the batch. Failed tasks are preserved and are never retried
automatically; repeating the command creates a new batch, not a resume.

The launcher import preflight verifies the mounted base commit, clean Git
status, and module location under `/workspace` inside the disconnected workload.
Suite adapters put `/workspace`, `/workspace/src`, and `/workspace/lib` on the
task interpreter's import path. Native extensions and narrowly selected generated
version/config files are retained from the official image before `/testbed` is
removed. They are restored only at the identical base commit, with content
hashes, no Git history, and no overwriting different workspace files. Git status
is checked again afterwards; unignored changes block preparation.
Official images can add setup commits changing permissions or dependency metadata.
The disposable adapter build resets `/testbed` to the canonical task base before
capture. Capture checks every tracked path/blob against that base (ignoring only
regular-file executable bits) and rejects remaining source-content changes.
It is **not** a public-test pass or official evaluator pass. The task's actual
test command still depends on its historical repository and installed tools.
Do not treat an installed `pytest` as proof every repository uses it.

Official evaluation remains explicit and separate:

```sh
# Environment/oracle preflight. Keep gold artifacts private and outside mounts.
python3 tools/benchmarks/evaluate_swe_suite.py --task pallets__flask-4992 --mode gold

# An empty submission validates evaluator input handling, not FAIL_TO_PASS
# execution: the official harness can skip empty patches entirely.
python3 tools/benchmarks/evaluate_swe_suite.py --task pallets__flask-4992 --mode empty

# Score the exact saved agent prediction, without another model invocation.
python3 tools/benchmarks/evaluate_swe_suite.py --task pallets__flask-4992 \
  --mode prediction --prediction RUN/artifacts/pallets__flask-4992.prediction.jsonl

# Sequential oracle preflight for all ten, or score a saved live batch.
python3 tools/benchmarks/swe_suite.py evaluate --mode gold
python3 tools/benchmarks/swe_suite.py evaluate --mode prediction --batch BATCH/batch.json
```

Evaluation batches write a separate summary linked to the source live batch and
its fingerprint; they never change the original attempt records. Missing
predictions remain explicit, and evaluator errors/timeouts stop the batch for
container inspection rather than starting another evaluation blindly.

Image preparation and scoring require external downloads and considerable disk,
CPU, and possibly amd64 emulation. They do not invoke a model. Live batches
reject public model endpoints by default. `--allow-hosted-inference` is an
explicit operator opt-in for a `run` action; it enables the configured public
endpoint but does not add a cumulative token or spend cap. Runtime token
accounting remains observational rather than a hard pre-generation spend cap.
The sequential batch's default wall budget can still consume up to ten days for
ten tasks, excluding preparation/evaluation. Use the opt-in only when hosted
inference and its potentially uncapped provider costs have been authorized.
Container isolation limitations and private unredacted archives described above
apply unchanged.

Preparation fingerprints the explicit Cargo/crates build inputs, including
untracked source files, and rejects stale adapter provenance on reuse. Official
scoring verifies canonical-row hashes and the prepared official image identity;
the evaluator receives its immutable repository digest rather than `latest`.
Gold preflight results are environment evidence, never agent-solving results.
