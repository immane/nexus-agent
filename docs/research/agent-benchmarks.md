# Agent benchmark options for Nexus

For the Context Management development workflow, metric definitions, artifact
storage, and 92K -> 64K -> 32K comparison matrix, see the
[context management design](../design/12-context-management.md). That design
separates proposed measurement capabilities from the current experimental runner.

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
| [Harness-Bench](https://www.harness-bench.ai/) | Paired evaluation of model–harness configurations on broad, realistic workflows; includes 81 software-engineering tasks but is not a coding-only benchmark. | Nexus is not one of the published harness configurations. The upstream runner lists seven named harnesses and would need a dedicated Nexus integration, image, and trace/artifact mapping. | The paper reports 300 tasks and 20,454 configuration-task evaluations, with 200K input-context and 150-turn limits. Full reproduction is a substantial model/compute effort; begin with a pinned small pilot. | Valuable follow-up for harness effects and complete-delivery behavior, after integration and safe execution are independently validated. |
| Existing `tools/perf/startup.py` | Fresh-process spawn-to-exit timing only. | Directly runnable against a built binary. | No model calls. Not first-interactive startup, end-to-end task quality, or reliable RSS attribution. | Run as a local performance metric, never as an agent-quality score. |

SWE-bench Verified is a human-validated 500-instance subset, but it is larger
than Lite and retains the same patch-generation/integration work. Consider it
only after the small-sample pipeline is stable. The complete SWE-bench suite
and Terminal-Bench's full evolving task set are not sensible first debugging
targets.

## Harness-Bench assessment

**Research snapshot: 2026-10-24.** Harness-Bench is a third-party benchmark and
reusable evaluation pipeline for measuring how native agent harnesses affect
delivery across model backends. Its paper is an arXiv preprint (v2 dated
2026-10-15); the following description reflects the cited paper, project
repository, and website, not an independent replication.

### Suite and scoring

- The published suite contains 300 sandboxed workflow tasks: 274 general and
  26 multimodal, organized into six work categories and 17 subcategories. The
  general suite includes 81 software-engineering tasks (implementation/repair
  and repository maintenance); other categories cover business operations,
  systems engineering, research and analysis, web development, and content
  production.
- Each task defines resources, deliverables, and constraints. Task-specific
  hidden oracles check final artifacts/workspace state in weighted modules;
  safety and risk gates can zero the task score. The project describes
  deterministic module checks rather than a general LLM judge, with bounded
  semantic fallback only where a task explicitly declares one.
- The paper reports seven native harnesses and ten model backends on general
  tasks, and seven model backends on multimodal tasks: 20,454
  configuration-task evaluations. It reports mean score, the share scoring at
  least 0.80 (R0.8), full-credit rate (R1), trajectory/request/token usage, and
  provider-price estimates. Its execution settings use a 200K input-context
  threshold and a 150-turn limit; each configuration-task pair is run once.
- Published findings are third-party claims: harness rankings vary by model and
  delivery metric, and higher expenditure does not always produce better
  delivery. The paper also analyzes partial work that appears internally
  consistent but omits a required artifact or evidence.

### Relevance to Nexus

Harness-Bench complements SWE-bench Lite. SWE-bench tests repository issue
repair against project tests; Harness-Bench asks how a whole model–harness
configuration completes varied workflows and produces verifiable final
artifacts. Its full-credit metric and trajectory analysis are relevant to
Nexus's observed distinction between a correct patch and a normally completed,
cleanly delivered run. Its broader tasks could also reveal behavior not covered
by code-repair benchmarks.

It is **not** a drop-in score for this repository. The published lineup includes
a harness named OpenCode, but that result does not represent Nexus Agent and
must not be attributed to it. Nexus would need a first-class upstream-compatible
runner integration that preserves its native prompting, tools, context policy,
recovery behavior, and run limits while exporting the benchmark's required
artifacts and traces. A wrapper that substitutes the scripted `nexus-headless`
fake provider would not be a valid live-agent evaluation.

### Limitations and pilot recommendation

- The public leaderboard and paper summarize upstream configurations; they do
  not establish Nexus performance. In particular, their Qwen results are not a
  controlled comparison with Nexus's configured quantized local model, task
  selection, tool environment, or budget.
- A full run is large (20,454 reported evaluations), and the published paper
  includes provider-price estimates for hosted models. Do not launch a full
  reproduction without a pinned revision, explicit budget, and a validated
  runner. Start with task-oracle validation and one or a few preselected tasks;
  record valid, failed, and infrastructure-invalid runs separately.
- The upstream site/repository description is not independent evidence of task
  validity or reproducibility. Before use, pin the repository commit, task-set
  lock, harness images, evaluator, model identity, and configuration; verify
  licenses and task/oracle access requirements for the selected materials.
- The benchmark's sandbox does not validate Nexus's container prototype as a
  security boundary. Run untrusted tasks in a disposable, independently
  validated Linux VM/runner without credentials or valuable host mounts.

**Recommendation:** add Harness-Bench to the candidate set for a later
harness-level evaluation, not as a replacement for the current SWE-bench Lite
pilot. First stabilize Nexus task-runner completion, trace capture, and resource
cleanup. Then assess the upstream integration contract and licenses before
committing to even a small live-model pilot. No Harness-Bench tasks have been
run by Nexus as of this snapshot.

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

### Summary-request debugging (2026-10-25)

The `context-summaryfix-rerun-20261025` debug replay completed one summary
(1,366 output tokens), then rejected another after it used its 4,096-token
output allowance. The runtime preserved the original conversation and stopped;
this is not a successful task completion. Increasing the allowance alone did
not establish summary reliability. Summary requests now use isolated summarizer
instructions instead of coding-agent/environment instructions and end with an
explicit request to summarize, rather than the original task description.
Rejection diagnostics include protocol validity, finish reason, text bytes,
and the memory byte ceiling; truncated summaries remain ineligible for adoption.
The subsequent `context-summaryfix-v2-20261025` replay completed and the official
SWE-bench 5.0.2 evaluator resolved `sympy__sympy-20590` (1/1; `test_immutable`
and all 21 PASS_TO_PASS tests passed). That run made 38 task-model calls but
triggered no context reduction, so it validates task completion and scoring, not
the live summary request or compaction behavior. The selected task was already
exposed in prior runs and is not blind evidence. Reviewed artifacts are indexed
in [the evidence README](evidence/README.md).

### Ten-task diagnostics and Flask harness validation (2026-10-26)

A fixed ten-task SWE-bench Lite diagnostic set was added under
`tools/benchmarks/` with a pinned dataset revision
(`b0dde1093fe417d83b7184254edf8199c1f0dff5`) and SWE-bench `5.0.2`. The tasks are
selected for diagnostic breadth, not a representative sample. Preparation keeps
gold rows outside task mounts; batches run sequentially, record unstarted and
interrupted tasks, and never retry automatically. Official scoring is a separate,
explicit step and reports its own outcome rather than reusing agent completion.

Only `pallets__flask-4992` was run end to end. Two attempts are relevant:

- The first attempt stopped after 17 minutes and 18 turns with
  `LimitReached: context summary failed validation`. The provider batch was
  protocol-valid, the finish reason was `Stop`, and `text_bytes=0`: the summary
  contained no usable text. History was preserved, but the runtime had no
  recovery and exited for a cause that was not a turn or time limit.
- The second attempt used bounded 8 KiB paged reads, isolated task/summary
  calibration, and a one-shot empty-summary retry. It completed after 4,140 s
  with 99 model turns, 82 tool calls, 34 validated summary responses, and five
  committed reductions. Cumulative read output was about 31 KB versus about 100 KB
  before paging; provider-reported input peaked at 14,448 tokens before the first
  reduction and reached 34,879 by the final turn, still well below the
  94,208-token window. No empty summary occurred, so the retry path was covered by
  tests but not triggered live.

The submitted patch added `**open_args` to `Config.from_file()` and forwarded it
to `open()`. It applied cleanly, passed all 19 `PASS_TO_PASS` tests, and failed
the hidden `test_config_from_file_toml`, which calls
`from_file(..., text=False)`. `open()` has no `text` parameter, so the required
API must translate `text` into `"r"`/`"rb"` rather than forward it. The agent had
verified `mode="rb"` on its own, but not the expected interface. Official
evaluation therefore recorded `resolved: false`, without an infrastructure
failure. This is a convergence and interface-inference failure, not evidence
that the model failed to locate the defect.

Agent wall time is the task run only and excludes preparation and the separate
official evaluation.

### Earlier pilot observations

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
  At the time, the limiting bound was the core `MAX_CONVERSATION_ITEMS` ceiling
  of 256, which capped the pilot's configured 8,192 retained items. The official
  SWE-bench evaluator nevertheless marked `sympy__sympy-20590` **resolved**,
  with zero infrastructure failures, ambiguous failures, or evaluator errors.
  The checkout retains the generated patch for inspection; Nexus-owned
  containers were cleaned. The evaluator left its task image cached. Artifacts
  and report are under
  `/Volumes/Nayuki/Development/Rust/nexus-benchmark-work/pilot-sympy-qwen-retry-20261024/`;
  server diagnostics are in
  `/private/var/folders/qp/1943jsm56652bhm60grnl74m0000gn/T/opencode/nexus-swebench-qwen-retry-server.log`.

The subsequent runtime repair separates the 8,192-item request representation
ceiling from the normal 256-item policy. Active context evicts oldest complete
exchanges when effective item or aggregate-byte budgets are exceeded, without
splitting calls/results or losing the current task, newest complete exchange,
or pending calls. Opaque continuation prevents unsafe eviction. Numeric logs
report effective budgets and reductions. This is lossy deterministic eviction,
not semantic summarization, and it does not promise knowledge of evicted
investigation results. The aggregate context cap remains 1 MiB; this is not a
provider tokenizer or a guarantee that a model's token window will fit. The
Later live runs completed after this repair, as recorded below, but their saved
diagnostics did not demonstrate an eviction event. They therefore do not yet
validate eviction behavior under live context pressure.

New-task history admission uses the same transactional projection as later
turns. Unknown effects and uncertain evidence cannot be evicted (including
cancelled/timed-out outcomes whose effects remain uncertain). Uncertain results
from a non-completed run are retained only when the corresponding tool was
actually dispatched; pre-dispatch cancellation does not leak the proposed call
into the next run. Terminal-staged uncertain outcomes are materialized as
tool-result items when their call was dispatched; incomplete/unpaired calls are
never carried forward. Omission notices remain pending until a completed
exchange containing the notice is cached. Per-session omission markers are
capped at 128; overflow folds into a runtime-wide, fixed-size fallback notice.
A single model-visible omission notice is charged to the
budget; if it cannot fit safely, the run reaches its limit before calling the
provider. Reduction events report the actual post-deduplication item and byte
counts. Completed runs remain completed even if their final context cannot be
cached. Cache pressure evicts certain exchanges first; if uncertain evidence
cannot fit, a runtime-wide safety latch blocks subsequent model work with
`uncertain history retention budget exhausted`. This latch lasts for that
runtime instance and is not persistent recovery storage.

### Post-repair SymPy reruns and official evaluation (2026-10-24)

These attempts used the existing local Ollama service, not a paid cloud API.
The Nexus selection ID `ollama-local/qwen3-27b` maps to the actual wire model
`qwen3.8-27b-iq3m-92k:latest`; these are not two different evaluated models.
Tools ran with `--features container --execution container --benchmark-pilot`,
using the cached SymPy workload image and a dedicated task checkout. Container
isolation remains unvalidated; this result is not security validation.

#### Execution sequence and operational findings

1. The operator initially authorized runner limits of 30 minutes, 150 tool
   calls, 1,500,000 input tokens, and 50,000 output tokens, plus one-shot
   approvals for workspace tools in this task container. The runner gained
   streaming assistant-text logging and optional budget cancellation.
2. Preflight found that the configured Ollama endpoint needed proxy bypass.
   A first session request used the wire model name instead of the Nexus model
   ID and returned HTTP 400, before any model run. Selecting `qwen3-27b` fixed
   session creation without changing the actual Ollama model.
3. The budgeted run ended as `cancelled` after 1,213.954 seconds, 65 tool calls,
   and 39 final usage events: 1,549,703 input / 44,819 output tokens. The stop
   reason was `input-token budget reached`. Final-usage-based accounting
   exceeded the input threshold by 49,703 tokens; it is not a strict
   pre-generation token cap. A 476-byte prediction patch was preserved, adding
   `__slots__ = ()` to `Printable`. No official score was run for this attempt.
4. The operator then explicitly requested removal of runner token, time, and
   call limits and a fresh run. The server retained its finite pilot ceilings:
   500 model turns, 4,096 tool calls, and 24 hours. Optional runner limits were
   left unset rather than bypassing runtime validation.
5. The first fresh session request failed with HTTP 503 because the previous
   server shutdown left unresolved container ownership state. Inspection
   matched the ownership record, labels, container IDs, workspace mount, and a
   dead owner PID. Only those identified old resources were reconciled before
   retrying session creation. No model run began during that failed creation.
   Killing the server did not reliably clean up its container resources; this
   is a separate lifecycle issue, not evidence of model failure.
6. The fresh run completed normally in 2,127.538 seconds (35 minutes 28 seconds),
   with 153 tool calls and 128 final usage events: 4,930,308 input / 75,019 output
   tokens. There was no runner budget stop. The final 476-byte patch changed
   only `sympy/core/_print_helpers.py`, adding empty slots to `Printable`.
   `approved_calls` was zero: no approval event was handled, so the opt-in
   approval flag should not be credited with enabling progress in this run.
7. Official scoring was initially omitted from the agent-run workflow. The
   default Python had no `swebench` installation; a subsequent isolated
   `uv run --with swebench==5.0.2` environment supplied the evaluator. Its first
   invocation failed before testing because the local dataset JSON was a
   single object, whereas the loader expects an array. Wrapping the unchanged
   task row in a one-element array fixed the input format.
8. The official evaluator then ran the saved prediction against the task image
   with one worker and a 1,800-second test timeout. It completed in about 25
   seconds and reported **1 resolved / 1 completed**, with zero unresolved,
   infrastructure, ambiguous, or error instances and zero unstopped containers.
   One task image remained cached. This establishes resolution under the
   benchmark's tests, not correctness for every possible SymPy use case.

The fresh run's artifacts are under
`/Volumes/Nayuki/Development/Rust/nexus-benchmark-work/pilot-sympy-qwen-unbounded-20261024/`:

- `model-output.log`: streamed assistant text and final run result.
- `server.log`: startup and runtime diagnostics.
- `sympy__sympy-20590.result.json`: agent termination, usage, and budgets.
- `sympy__sympy-20590.prediction.jsonl`: exact prediction scored by the harness.
- `official-evaluation/dataset.json`: one-element task array for the loader.
- `official-evaluation/report/ollama-local__qwen3-27b.nexus-qwen-sympy-20590-unbounded-20261024.json`:
  official aggregate result.

The earlier budgeted attempt is preserved under the sibling directory
`pilot-sympy-qwen-context-20261024/`. Generated patches must not be discarded
when preparing another attempt. The fresh run's identified workload and gateway
containers were removed after completion; ownership reconciliation and orderly
server shutdown still need a durable operational solution.

Reviewed in-repository evidence is retained in the
[SymPy evidence archive](evidence/nexus-qwen-sympy-20590-unbounded-20261024/report.json):
`report.json` records the resolved result, the passing `test_immutable`
FAIL_TO_PASS test, and 21 passing PASS_TO_PASS tests;
[`patch.diff`](evidence/nexus-qwen-sympy-20590-unbounded-20261024/patch.diff) is the
exact patch applied by the evaluator. Generated files under `/logs/` are ignored,
not deleted. See the [evidence index](evidence/README.md) for reviewed artifacts
from subsequent runs.

#### Why convergence was slow: evidence and limits

The fresh run's assistant log identifies the right class by line 7, states the
root cause and minimal fix by lines 17–31, and claims the first successful
behavior check by lines 33–35. Most subsequent narration concerns validation
and cleanup rather than discovering or changing the final fix:

- Lines 37–67: missing pytest, exploration of SymPy's built-in runner, temporary
  scripts, misunderstood return values, and an incorrect self-written assertion.
- Lines 69–128: repeated MRO/attribute checks, broader regression suites, and
  reverting/restoring the fix to investigate an apparently pre-existing xpass.
- Lines 130–178: repeated checkout/diff confirmation, scratch-file cleanup, and
  multiple purported final checks after the source fix was already stable.

The container's `/workspace/.git` pointed to an unavailable macOS worktree
metadata path. The model consequently resorted to comparing `/workspace` with
the image's separate `/testbed` tree and repeatedly questioned which checkout
was authoritative. This is a concrete environment defect to address.
The task prompt also lacked an explicit validation stopping condition and did
not identify the available test runner.

As an offline environment-only check, a fresh independent clone at the task's
base commit was mounted into the existing Nexus SymPy adapter image with network
disabled. Python imported SymPy from `/workspace/sympy`, and the image's own
`bin/test -C --verbose` runner executed a public core test module: 52 passed and
2 expected failures in 18.50 seconds. This verifies that the container has a
working project runner and that imports resolve from the mounted checkout; it
does not verify the agent patch or evaluator result. The benchmark prompt now
states the generic `/workspace` authority and directs agents to follow test
instructions in the current repository rather than assuming pytest or a
particular framework. It does not inject this SymPy-specific command into other
tasks.

The assistant repeatedly alleged scratch-file rewriting or concurrent writes.
Those allegations are **unconfirmed**: saved assistant text is not a complete
tool trace. It does not preserve every command, result, exit status, or per-call
timestamp. Its reported test counts are not independently verified here, and
line positions cannot establish time-to-first-fix. The official resolved result
is stronger evidence than the assistant's test narrative.
No context-reduction event was found in saved diagnostics; repetition cannot
currently be attributed to eviction or missing semantic summaries. A single
successful task also cannot establish a general performance improvement.

#### Incremental optimization recommendations (not implemented)

The best first change is a **container-valid Git checkout**. Prefer a dedicated
checkout whose Git metadata resolves inside the container, rather than exposing
an unrelated host directory to make a worktree pointer resolve. Verify offline
that `git status`, `git diff`, and `git rev-parse HEAD` work from `/workspace`
and that modifications are exactly those captured by the host prediction
export. This addresses an observed defect, has a narrow scope, and can be
validated without another expensive model run.

Next, provide verified environment facts: `/workspace` is the modification root,
Python imports resolve there, and an available project-specific test command
works. Do not reveal gold patches or hidden evaluation tests. Clarifying the
environment must not expand tool permissions or install arbitrary dependencies
at model discretion.

Then make completion criteria explicit in the task instructions: reproduce,
apply the minimal fix, verify the reproduction, run relevant regression tests,
inspect the final diff, and finish. Repeat a passed check only after relevant
code/environment changes or new contradictory evidence; expand testing to
answer a named unresolved risk, not merely for another final confirmation.
This is guidance, not an automatic success declaration or forced early stop.

Before making causal claims about file anomalies or repetition, add a bounded,
redacted structured event trace alongside readable assistant output: call IDs,
tool names/arguments, start/end timestamps, results/exit status, truncation
markers, usage, and context reductions. Establish retention and output-size
limits so observability does not become unbounded or leak secrets.

Compare one change at a time on the same task/model/base commit, with explicit
operator authorization for each live run and no automatic whole-task retries.
Measure time to first valid patch, time from that patch to termination, repeated
checks, tool errors, usage, and official resolution. Preserve exact predictions
and run the official evaluator as a distinct required reporting step; agent
completion alone is not a score. Only consider advisory stagnation detection
after traces distinguish repeated work from legitimate verification. Semantic
summaries and complex runtime stopping heuristics are not the first experiment.

### Deferred task: semantic context summaries

Status: deferred; not implemented. First rerun a long task with deterministic
eviction and look for repeated file reads, repeated failed approaches, or lost
investigation conclusions. If these materially affect progress, implement a
bounded summary of evicted history that preserves established findings,
constraints, attempted approaches, validation results, and remaining work.
Treat summaries as lossy model-generated context, not authoritative evidence:
unknown effects and unfinished calls still require their original safety
records and must never be replayed based only on a summary. Define summary
generation budgets, cancellation and failure behavior, and frontend notices;
charge both generation and retained summaries to the applicable budgets.
Validate with deterministic regressions and an explicitly budgeted local-model
long-task comparison before enabling it by default. This is future work, not a
change to the current eviction behavior or authorization for a live model run.

### Contaminated Astropy challenge attempt (2026-10-24)

A single run of SWE-bench Verified task `astropy__astropy-13398` was stopped
after the agent log showed it had found and begun reconstructing the upstream
solution before completing the task. This is **contaminated and invalid**; it
has no agent score and must not be counted. The separate gold-patch preflight
resolved successfully, but only validated the evaluator environment.

The confirmed leak was the task checkout: it had been cloned from an older local
Astropy workspace whose `origin/main` refs included commits beyond the task's
base revision, including the solution. Public egress was also not independently
verified as blocked, so additional exposure cannot be ruled out. The run was
stopped before a patch was produced or official agent evaluation was run; final
usage is unknown/incomplete. Only the two containers owned by this run were
removed. Logs and the run manifest remain under
`/Volumes/Nayuki/Development/Rust/nexus-benchmark-work/pilot-astropy-13398-unbounded-20261024/`.

Do not reuse this task as a blind sample. Before another challenge run, create a
workspace with a fresh, minimal Git object/ref set anchored at the task base,
audit refs and reachable commits for future fixes, remove upstream remotes, and
restrict task egress before starting the model.

### Exposed Sphinx task attempts (2026-10-24)

The separate SWE-bench Verified task `sphinx-doc__sphinx-9461` was submitted in
two attempts. Attempt 1 ended after provider HTTP 500 errors (17 tool calls,
633.374 seconds); attempt 2 was an explicitly non-blind retry after the issue
statement had already been sent to the same model. Attempt 2 ended with
`ResourceLimit: model output limit reached` (41 tool calls, 819.489 seconds) and
an empty prediction. The official evaluator completed successfully and
classified it as an empty patch: 1 submitted, 0 completed/resolved/unresolved,
0 infrastructure failures, and 0 unstopped containers. The task image remains
as a cache entry. Neither attempt is a task score, and this task must not be
reused as a blind sample. The report and manifest are retained under
`/Volumes/Nayuki/Development/Rust/nexus-benchmark-work/pilot-sphinx-9461-isolated-20261024/`.

Although the pilot's dedicated gateway probe confirmed denied public TCP egress,
the task log also reported no network access and its server emitted a warning
that isolation was unvalidated on native Linux. This does not meet the owner's
requirement for network-capable task Docker environments. Resolve and verify the
intended task-network policy on the actual execution platform before another
agent run; successful image-build networking is not evidence of task-container
network access. The `model output limit reached` result alone does not establish
whether input context or generation output was the limiting factor.

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
server must remain loopback-only. `CHECKOUT` must be a standalone Git clone,
not a `git worktree` or a clone using external object alternates: only the
workspace directory is mounted into the container. For example:

```sh
git clone --no-hardlinks --no-checkout SOURCE_REPO CHECKOUT
git -C CHECKOUT checkout --detach BASE_COMMIT
```

For locally built images, pass the immutable image ID printed by
`docker image inspect IMAGE --format '{{.Id}}'` (the `sha256:...` value) to
`--gateway-image` and `--container-image`. Do not append that image ID to a tag
as `TAG@sha256:...`: locally built images commonly have no `RepoDigests`, so
Docker rejects that reference as missing even though the tagged image exists.

Before starting the server, verify that the process launching it actually has
both the bearer-token variable named by `--auth-token-env` (default
`NEXUS_BENCH_SMOKE_TOKEN`) and the selected provider credential variable
referenced by `config.json`. On macOS, non-interactive zsh/agent shells do not
necessarily source `.zshrc`; exporting a variable in a separate interactive
terminal does not make it available here. Check presence without printing the
value, for example:

```sh
test -n "${NEXUS_BENCH_SMOKE_TOKEN:-}" && echo bearer-token-present
test -n "${OLLAMA_API_KEY:-}" && echo provider-credential-present
```

Replace `OLLAMA_API_KEY` with the `credential.env` name for the selected
provider. If either variable is absent, explicitly load it into the server
launch process from the approved secret source; do not put credentials in
command arguments or benchmark artifacts. The runner does not launch the
server or recover credentials from the user's interactive shell.

The runner checks that `.git`, the common Git directory, and object storage are
self-contained before creating a provider session, so a broken host-only
worktree pointer fails fast without spending model tokens. The runner writes
artifacts but does not launch the server, prepare task dependencies, or score
predictions; use the official SWE-bench evaluator after checking that
dependencies required by the task's evaluation script are available to the
agent.

To classify a saved run independently of the agent's exit status, use the
offline summary command (omit `--evaluation-report` if scoring was not run):

```sh
python3 tools/benchmarks/run_nexus_swe_task.py summarize \
  --result RESULTS/INSTANCE.result.json \
  --evaluation-report RESULTS/official-report/REPORT.json
python3 -B -m unittest discover -s tools/benchmarks -p 'test_*.py'
```

This command makes no model/evaluator requests and does not rewrite artifacts.
It reports agent termination, patch size, official evaluation outcome, and
infrastructure classification separately. A resolved patch does not change an
agent's `limit-reached` status into `completed`; a process exit code alone does
not establish that the task was resolved. Report absence is `not-run`, and
evaluator errors and ambiguous infrastructure failures remain distinct.

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
- [Harness-Bench paper, arXiv:2605.27922v2](https://arxiv.org/abs/2605.27922v2): task construction, module-level scoring, model–harness comparisons, limits, and analysis.
- [Harness-Bench project website](https://www.harness-bench.ai/): task taxonomy, methodology summary, and published leaderboard.
- [Harness-Bench source repository](https://github.com/Qihoo360/harness-bench): runner, task catalog, evaluator, and operational documentation; pin a commit before any reproduction.
- [Token-consumption study on SWE-bench Verified](https://arxiv.org/abs/2604.22750): agentic token usage, input-token dominance, model differences, and run-to-run variability; results are from OpenHands, not Nexus.
- [Public Terminal-Bench 4.0 Harbor run](https://hub.harborframework.com/jobs/3de0aee9-6de7-4d94-83c4-ac5e789fe757) and [second model run](https://hub.harborframework.com/jobs/a5758f1a-9ef2-4100-9893-34a99c99bd9c): observed tokens and cost for specific 330-trial runs.
- Project context: [`tools/perf/README.md`](../../tools/perf/README.md), [`docs/design/05-performance.md`](../design/05-performance.md), and [`QUICKSTART.md`](../../QUICKSTART.md).
