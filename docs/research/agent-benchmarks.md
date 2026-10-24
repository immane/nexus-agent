# Agent benchmark options for Nexus

Research date: 2026-10-24. This is a recommendation for choosing and preparing
an evaluation, not a claim that Nexus has benchmark support or has been scored.
Benchmark versions, hosted services, and CLI syntax change; pin a dataset/task
release and the evaluator revision in any actual run.

## Recommendation

Use a staged evaluation rather than treating one number as an overall agent
score:

1. **Now: deterministic local regression checks.** Use the existing Rust test
   suite and `tools/perf/startup.py` for the behavior and process-startup
   measurements they actually cover. The headless binary is explicitly wired
   to scripted fakes; it is not a live coding-agent benchmark and its successful
   exit must not be counted as an issue solved.
2. **First external task benchmark: SWE-bench Lite, through a disposable,
   isolated Linux runner.** Its software-issue/patch outcome is closest to
   Nexus's coding-agent goal, and the official evaluation guide recommends Lite
   as the smaller starting subset. A minimal one-task server adapter is now in
   `tools/benchmarks/run_nexus_swe_task.py`; it emits the required `model_patch`
   JSONL prediction and per-run metadata. It remains an experimental adapter,
   not a stable benchmark product. Run the official Docker evaluator separately.
3. **Broader follow-up: Terminal-Bench 4.0 via Harbor.** This covers diverse
   terminal work rather than only repository issue repair. Harbor's documented
   ACP integration can run registry agents, but this repository does not
   currently expose an ACP agent. Implement or provide a narrowly scoped
   adapter before comparing Nexus; do not label a shell script around the
   scripted headless binary as a valid agent integration.

Do not use the experimental container mode as the benchmark security boundary:
the project documents native Linux isolation, network/DNS, metadata, and
egress validation as outstanding. Benchmark task repositories and model output
are untrusted. Use a disposable VM/runner with no credentials, no valuable
workspace mounts, restricted egress, and cleanup between tasks until isolation
has been independently validated.

## Candidate comparison

| Benchmark | Fit | Nexus integration | Cost / operational notes | Recommendation |
|---|---|---|---|---|
| [SWE-bench Lite](https://www.swebench.com/lite.html) | Real GitHub issue repair, judged by tests; narrower and more directly relevant to coding than general computer-use suites. | Not plug-and-play. Build a task adapter that launches a live Nexus agent, captures the final repository diff, and writes one prediction JSONL row per instance. Use the official harness for scoring. | Docker images, disk, CPU/RAM, and model tokens; start with a few pinned instances and one worker. Evaluator infrastructure is separate from agent success. | Best first external task benchmark after adapter and safe runner exist. |
| [Terminal-Bench](https://github.com/harbor-framework/terminal-bench) 4.0 | Diverse terminal tasks with explicit success criteria; useful for shell/tool-use breadth, not exclusively code changes. | Harbor documents a generic ACP integration. Nexus has no ACP entry point in this repository, so an ACP-compatible agent adapter is required. Alternatively implement a reviewed Harbor agent integration. | Some tasks use GPUs or multiple containers. The project README recommends testing oracle solutions repeatedly and describes hosted environments for scale. API and hosted compute can cost money. | Strong second benchmark; start with a small task subset and pinned release, not the whole moving `latest` dataset. |
| Existing `tools/perf/startup.py` | Fresh-process spawn-to-exit timing only. | Directly runnable against a built binary. | No model calls. Not first-interactive startup, end-to-end task quality, or reliable RSS attribution. | Run as a local performance metric, never as an agent-quality score. |

SWE-bench Verified is a human-validated 500-instance subset, but it is larger
than Lite and retains the same patch-generation/integration work. Consider it
only after the small-sample pipeline is stable. The complete SWE-bench suite
and Terminal-Bench's full evolving task set are not sensible first debugging
targets.

## Current project constraints

- `nexus-headless` is a one-shot scripted/fake-wired runner (see
  `crates/nexus-headless/src/main.rs` and `QUICKSTART.md`); it has no live model
  session or approval loop. Do not feed its output to SWE-bench as an agent
  result.
- `nexus-server` can bind sessions to configured live providers and exposes
  run/event/approval routes, but its own crate documentation labels the API
  unstable and test-only. This is a more automatable pilot seam than the TUI,
  not a stable benchmark integration. A small harness would still need to
  submit tasks, consume SSE events, handle/deny approval requests deliberately,
  collect the final diff, and write prediction rows.
- The TUI can also call live providers, but is not a benchmark harness. Its
  development tools can change the current workspace; demo mode is not a safe
  substitute and is not side-effect-free.
- `docs/design/05-performance.md` says live-model latency is an integration
  measurement, not a core benchmark, and that no benchmark may silently call
  paid APIs or launch unapproved plugins.
- No ACP implementation is present. A minimal SWE-bench one-task adapter has
  been added under `tools/benchmarks/`; it still relies on the server's
  explicitly unstable test API.
- Container execution is a prototype with an active workspace mount and
  unvalidated isolation. Do not treat Docker-in-Docker or this mode as proof of
  safe containment.

## Suggested bring-up and error accounting

Before measuring Nexus, validate the evaluation system independently:

1. Pin the benchmark dataset release/commit, evaluator version, agent commit,
   model identifier, prompt/configuration, task IDs, and resource limits. Keep
   credentials outside task containers and logs.
2. Run the benchmark's oracle/reference solution on each selected task at least
   five times, as Terminal-Bench recommends. If the oracle is flaky, the task
   or environment is not a valid sample yet; report and quarantine it rather
   than charging the agent with a miss.
3. For SWE-bench, run the official gold patch on a small selected instance
   first. A gold-patch failure means the image, checkout, evaluator, or test
   setup is broken; repair that before generating agent predictions.
4. Keep task outcomes and infrastructure outcomes separate. Count an agent
   failure only when the task environment started and the evaluator completed
   against the pinned task. Record setup/image-pull failures, runner crashes,
   evaluator errors, timeouts caused by infrastructure, and quarantined flaky
   tests as `invalid/infrastructure`, with evidence and a retry policy—not as
   solved or unsolved tasks.
5. Retry only known transient infrastructure errors, with a small predeclared
   retry limit. Do not rerun model failures selectively. Preserve every
   attempt, patch, exit status, evaluator log, and token/time budget; report
   invalid counts and retries alongside the success rate.
6. Use a unique SWE-bench `run_id` for every changed prediction set. The
   evaluator caches by run ID and instance; reusing a run ID can silently reuse
   an earlier result even when the patch changes.
7. Report solved / valid tasks, invalid infrastructure tasks, explicit agent
   failures, timeout counts, retries, token/API cost, wall time, and concurrency
   separately. Do not convert missing outputs, malformed patches, policy
   refusals, or agent timeouts into infrastructure exclusions: those are agent
   outcomes unless there is evidence the evaluator/runner failed.

For an initial pilot, select a small, fixed set of SWE-bench Lite instances,
run each once plus a small repeat subset to estimate stochastic variance, and
publish raw per-instance outcomes. Do not compare to leaderboard numbers unless
the agent interface, model, tools, budgets, task release, and evaluator are
materially comparable.

## Token and cost budget for a pilot

There is no defensible fixed token estimate for a Nexus SWE-bench task yet:
Nexus has not been connected to the task loop, and usage depends on model,
context/replay strategy, tool calls, retries, and task difficulty. Do not
multiply a chat prompt's token count by 300 and treat it as an agent estimate.
Agent runs repeatedly send context; input tokens can dominate, and published
SWE-bench Verified research reports up to 30x variation between runs of the
same task. That research uses a different agent stack and is evidence of
variance, not a Nexus per-task forecast.

For scale only, one public Terminal-Bench 4.0 Harbor run reported 330 trials,
5.68 billion total tokens (about 17.2 million/trial) and another reported
21.56 billion total (about 65.3 million/trial). These are specific full-run
model/configuration results, not SWE-bench Lite averages or a prediction for
Nexus. They show why full benchmark runs should not be the first experiment.

Recommended no-surprise ramp:

1. **Zero-billing preflight:** validate the selected task images, gold patch,
   runner, diff capture, and scoring path. This spends compute/disk but must not
   invoke a hosted model.
2. **One real task:** use a locally served model if available, or an explicitly
   authorized hosted model. Record provider-reported input/output/cached/reasoning
   tokens and cost, if exposed. Set a provider-side spend limit or stop after
   one task; do not assume the agent currently enforces a total-token budget.
3. **Three-task smoke:** proceed only if task setup and one-task accounting are
   correct. Keep concurrency at 1; save per-task usage and include failed runs.
4. **Five-to-ten-task pilot:** estimate a median and a high-percentile cost from
   observed runs, then decide whether a larger run is worth it. Repeat only a
   small predeclared subset; repeated trials multiply the model budget.

Budget arithmetic after the first measured task:

```text
estimated total = task count × measured per-task cost × repeat count
                 + evaluator/runner overhead
```

Use provider-reported cost rather than converting total tokens with one blended
rate: input, cached input, output, and reasoning tokens may have different
prices. Before using a provider without an explicit hard spend cap, set an
external budget alert and stop condition. If exact usage is unavailable, report
that limitation instead of inventing a token count.

### Minimal smoke-run record

Keep one row per attempt, including failures:

| Field | Example / meaning |
|---|---|
| Task and dataset revision | Exact instance ID and pinned dataset commit/release |
| Nexus/evaluator/model | Commit SHAs and immutable or dated model identifier |
| Outcome | solved / agent-failed / invalid-infrastructure, with evidence |
| Usage | input, cached input, output, reasoning tokens; unknown if not exposed |
| Cost and duration | Provider-reported cost and wall time |
| Retry | Attempt number and predeclared retry reason |
| Artifacts | Patch, evaluator result, and redacted logs; never credentials |

## Local pilot status (2026-10-24)

One SWE-bench Lite task, `sympy__sympy-20590`, was used to validate the path;
this is not a leaderboard score:

- The configured Ollama endpoint was reachable directly, but requests routed
  through the workstation HTTP proxy failed. Excluding the configured
  Tailscale-range endpoint from proxying fixed access. The model listed by the
  endpoint matched the Nexus provider configuration.
- Nexus requires its configured `OLLAMA_API_KEY` environment reference to be
  set even though the local Ollama endpoint did not require authentication. A
  process-only placeholder was used; no config or shell profile was changed.
- A minimal live Nexus-server/Ollama request completed. A direct one-token
  endpoint sanity request reported 56 input + 1 output token; this is not the
  agent task's token usage.
- The official gold-patch evaluation passed 1/1 in 24 seconds after pulling the
  task's amd64 image explicitly. Docker Desktop is linux/arm64, so this image
  ran under emulation. The first evaluation attempt used an obsolete
  `princeton-nlp/SWE-bench_Lite` dataset ID and failed before evaluation because
  its rows lack current harness fields; switching to
  `SWE-bench/SWE-bench_Lite` corrected that setup error.
- Two additional official gold-patch checks passed: Astropy
  `astropy__astropy-12907` and Django `django__django-11099` (2/2, no
  infrastructure failures). Their amd64 task images ran under emulation and
  took 150 seconds and 36 seconds respectively. The task images are cached
  locally now; the task containers were stopped and removed by the evaluator.
- Nexus was then run against the task in its opt-in container tool mode, with
  the task workspace isolated from the main checkout and owner state isolated
  for cleanup. The run ended `limit-reached` after 15 tool calls and produced
  no repository diff. This is an agent failure, not an infrastructure failure.
  The official prediction report therefore records one empty patch and skips
  test execution; it does **not** produce a solved/unsolved score. Per-run total
  token usage was not captured: the server usage events are per provider turn,
  and only the final usage event was retained in the smoke output.
- The current runtime is still explicitly M0-test. Its defaults are 8 model
  turns and a 300-second run duration; the terminal `limit-reached` reason was
  not retained, so this pilot cannot establish which limit ended the run. A
  benchmark-specific, finite higher-turn profile and per-turn usage recording
  are prerequisites for a meaningful retry. Do not raise product defaults or
  classify the empty prediction as infra-invalid to manufacture a score.
- The Nexus prototype workload image contains only BusyBox among common task
  tools; it has no `python`, `pytest`, `git`, `cargo`, `node`, or `npm`. It can
  provide bounded workspace file operations and shell basics, but it is not a
  representative SWE task image and cannot run typical project test suites.
  A meaningful agent benchmark needs an agent tool environment with the task's
  runtime/dependencies, not merely a Dockerized evaluator.
- A task-specific adapter image now has a reproducible recipe at
  `tools/benchmarks/swebench-task-image.Dockerfile`. It uses the selected
  official task image as its base and copies in statically linked Nexus
  worker/keeper binaries. The image was successfully built for the SymPy task
  and exercised in a live-model attempt on 2026-10-24. Container workload
  startup now preserves the task image's `PATH` and locale so image-provided
  interpreters such as Conda Python remain
  reachable; the adapter also replaces the inherited task-image healthcheck
  with the keeper healthcheck required by Nexus. Creating a real Nexus
  container session with this image now reaches `running/healthy`. The raw
  SymPy image's base Python did not expose `mpmath`, but its `testbed` Conda
  environment already contains Python 3.9 and `mpmath`; the adapter now puts
  that environment on `PATH` and sets its Conda identity variables. The task's
  evaluation setup still runs `pip install -e .` before scoring.
- Using the derived SymPy image with the same amd64 emulation, a read-only
  container root, a writable task-workspace mount, and no network, the task's
  existing `sympy/core/tests/test_basic.py` suite passed **22/22**. Imports
  resolved to `/workspace/sympy`, and `mpmath` resolved from the image's
  `testbed` environment. This validates the agent-side Python/test environment,
  not an agent-produced fix or SWE-bench score.
- `nexus-server --benchmark-pilot` now opts into finite 500-turn, 4,096-tool,
  24-hour limits and is rejected unless real container tools are selected. This
  matches the reported 500-iteration default in the OpenHands SDK while
  keeping Nexus's wall-clock and tool-call ceilings explicit. It does not
  change normal server limits or local execution defaults. The prior 24-turn
  cap applied to the failed attempt below; it has since been raised. The
  one-task adapter at `tools/benchmarks/run_nexus_swe_task.py` reads the server
  SSE stream, sums only `usage` events with `finality == "Final"`, refuses
  approvals, captures tracked and untracked patch files, and writes a prediction
  plus a per-run JSON record. Its wall-clock timeout is 24 hours plus a
  60-second cleanup grace period. The OpenAI-compatible provider now retries
  transient HTTP 408/425/429/5xx responses and pre-connect failures up to six
  times after the initial attempt, waiting 1/2/5/10/30/60 seconds. The active
  run deadline and cancellation stop backoff; failures after provisional output
  are not replayed. Safe provider retry diagnostics (category, retry guidance,
  static message, and numeric HTTP status when available) go to server stderr;
  bodies and credentials are not logged.
- The first measured run using that adapter targeted
  `sympy__sympy-20590` with local Ollama model
  `qwen3.8-27b-iq3m-92k:latest`. The container and task environment started
  successfully, but the agent exhausted the explicit 24-model-turn budget after
  35 tool calls / 416.639 seconds. It emitted 24 final usage events totaling
  313,411 input and 17,719 output tokens, and produced no patch. This is a
  valid agent budget failure, not an infrastructure failure; no official score
  was run because the prediction was empty. The revised 500-turn/24-hour
  budget was subsequently used for the retry attempt documented below. The
  attempt record is under
  `/Volumes/Nayuki/Development/Rust/nexus-benchmark-work/pilot-sympy-qwen-20261024/`.
- One new attempt was made after predeclaring the raised 500-turn/24-hour
  budget. It terminated after 12 final usage events and 21 tool calls at
  339.403 seconds with `Protocol: provider request failed`; usage was 401,143
  input and 10,895 output tokens. It produced no patch and was not sent to the
  evaluator. The checkout remained clean and Nexus-owned containers were
  cleaned. The server log contained no more specific failure data; direct
  Ollama model/loaded-model health checks succeeded after the run, which does
  not establish the cause of the failed request. Record this as a provider-side
  failure with cause unresolved, not a turn-budget failure. Artifacts are under
  `/Volumes/Nayuki/Development/Rust/nexus-benchmark-work/pilot-sympy-qwen-500turn-20261024/`.
- The retry-policy run used local `ollama-local/qwen3-27b` with the same task
  and 500-turn/24-hour pilot limits. It ran for 1,066.715 seconds, emitted 82
  final usage events (2,498,462 input / 38,955 output tokens), and made 109
  tool calls. It produced a 1,028-byte patch adding empty slots to SymPy's
  `Printable` base class and a regression test. The agent run then ended with
  `ResourceLimit: retained context budget exhausted`; this is distinct from
  provider failure and the retry diagnostics showed no provider retry events.
  The limiting bound is the core `MAX_CONVERSATION_ITEMS` ceiling of 256,
  which caps the pilot's configured 8,192 retained items. The official
  SWE-bench evaluator nevertheless marked `sympy__sympy-20590` **resolved**,
  with zero infrastructure failures, ambiguous failures, or evaluator errors.
  The checkout retains the generated patch for inspection; Nexus-owned
  containers were cleaned. The evaluator left its task image cached. Artifacts
  and report are under
  `/Volumes/Nayuki/Development/Rust/nexus-benchmark-work/pilot-sympy-qwen-retry-20261024/`;
  server diagnostics are in
  `/private/var/folders/qp/1943jsm56652bhm60grnl74m0000gn/T/opencode/nexus-swebench-qwen-retry-server.log`.

For a one-task run, first make a disposable checkout at the dataset row's
`base_commit`, then build that task's adapter image with its immutable registry
digest:

```sh
docker buildx build --load --platform linux/amd64 \
  --build-arg TASK_IMAGE='swebench/...@sha256:...' \
  -t nexus-swebench-task:pilot \
  -f tools/benchmarks/swebench-task-image.Dockerfile .
```

Start `nexus-server` with `--features container`, explicit `--execution
container`, real tools, immutable gateway/workload image references, and
`--benchmark-pilot`; put the 64-hex bearer token and configured local-provider
credential in environment variables, not command arguments. Then run
`tools/benchmarks/run_nexus_swe_task.py --task-json ROW.json --workspace
CHECKOUT --provider PROVIDER_ID --model MODEL_ID --output-dir RESULTS`. The
server must remain loopback-only. The runner writes artifacts but does not
launch the server, prepare task dependencies, or score predictions; use the
official SWE-bench evaluator after checking that dependencies required by the
task's evaluation script are available to the agent.

## Comparison with other agent loop caps

There is no standardized unit called a "turn" across coding agents: one
framework's iteration may include several model calls, tool actions, or
sub-agent steps. For scale, the OpenHands Software Agent SDK reports
`max_iteration_per_run` defaulting to 500; the Claude Agent SDK exposes a
configurable `maxTurns` option (its official example sets 3) rather than
establishing a cross-product default. Nexus therefore uses 500 model turns as a
generous benchmark-only ceiling, not as an equivalence claim. Its independent
24-hour wall-clock, 4,096 tool-call, context-item, and per-tool limits remain
active: "as unlimited as possible" is not literally unlimited, and no total
token ceiling is currently enforced.

References: [OpenHands SDK issue documenting the 500-iteration default](https://github.com/openhands/software-agent-sdk/issues/6104),
[Claude Agent SDK TypeScript reference](https://code.claude.com/docs/en/agent-sdk/typescript).

Artifacts (temporary, outside the repository checkout) are under
`/Volumes/Nayuki/Development/Rust/nexus-benchmark-work/`: the base-commit task
checkout, empty prediction, and official gold/prediction reports. No
benchmark-specific execution defaults were added to Nexus; daily execution
remains local by default. The Nexus workload/gateway prototypes were removed
after the run, and the benchmark task image remains cached in Docker for
possible later use.

## Primary references

- [SWE-bench evaluation guide](https://github.com/SWE-bench/SWE-bench/blob/main/docs/guides/evaluation.md): prediction format, Docker evaluation, Lite recommendation, cache behavior, and result artifacts.
- [SWE-bench quickstart](https://github.com/SWE-bench/SWE-bench/blob/main/docs/guides/quickstart.md): dataset variants and validating the setup with a gold patch.
- [SWE-bench Verified](https://www.swebench.com/verified.html): human-validated subset description.
- [Terminal-Bench repository](https://github.com/harbor-framework/terminal-bench): task benchmark and Harbor usage; README recommends running oracle solutions repeatedly before evaluating an agent.
- [Terminal-Bench 4.0 announcement](https://www.tbench.ai/news/terminal-bench-4-0): resource calibration, task fixes, saturated-task removal, and remaining sources of evaluation error.
- [Harbor ACP documentation](https://www.harborframework.com/docs/agents/acp): registry and local ACP agent integration, credential forwarding, and run artifacts.
- [Token-consumption study on SWE-bench Verified](https://arxiv.org/abs/2604.22750): agentic token usage, input-token dominance, model differences, and run-to-run variability; results are from OpenHands, not Nexus.
- [Public Terminal-Bench 4.0 Harbor run](https://hub.harborframework.com/jobs/3de0aee9-6de7-4d94-83c4-ac5e789fe757) and [second model run](https://hub.harborframework.com/jobs/a5758f1a-9ef2-4100-9893-34a99c99bd9c): observed tokens and cost for specific 330-trial runs.
- Project context: [`tools/perf/README.md`](../../tools/perf/README.md), [`docs/design/05-performance.md`](../design/05-performance.md), and [`QUICKSTART.md`](../../QUICKSTART.md).
