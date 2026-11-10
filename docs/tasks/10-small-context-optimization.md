# Small-context optimization: 16K pressure profile

Date: 2026-11-11.

Status: proposed implementation and evaluation plan. No runtime changes or
measured improvements are claimed by this document. The owner requested a
detailed optimization proposal after the 16K and 20K experiments.

## 1. Outcome and scope

Make a 16,384-token context window more useful for multi-step coding tasks by
reducing avoidable input and making compaction fit the available budget. Preserve
execution evidence, provider protocol validity, and explicit failure when the
minimum safe request cannot fit. Completion at 16K is an experimental objective,
not a guarantee for arbitrary tasks.

Keep 32K as the currently demonstrated successful configuration for the observed
SymPy regression. Neither the successful 32K run nor the failed smaller-window
runs establish a minimum model capacity: trajectories differ, and the smaller
runs stopped on runtime budget/compaction paths.

Reuse the current context engine, runtime, source archive, tools, and benchmark
runner. No new memory service, vector database, agent orchestration, dependency,
or task-specific repair instructions are proposed. Do not change global defaults
until candidate behavior has independent evaluation evidence.

Implementation spans several crates; use the build agent for bounded sequential
slices, with review before compatibility-sensitive changes. This plan does not
authorize live inference, external costs, or model-server configuration changes.

Related documents:

- [Context management direction](../design/12-context-management.md).
- [Agent reliability follow-up](09-agent-reliability.md).
- [Benchmark observations](../research/agent-benchmarks.md).
- [Benchmark runner](../../tools/benchmarks/README.md).
- [Development tests](../testing.md).

The context design document contains a dated baseline. Its statements about
unwired persistence/restoration must not override the current implementation.

## 2. Evidence and limits of the existing experiments

The following local artifacts were inspected while preparing this plan. They are
outside the repository and are not portable published evidence. Run directories
are under the sibling `nexus-benchmark-work/` directory, prefixed with
`run-sympy__sympy-20590-` and the suffixes below.

| Run suffix | Window / task output cap | Agent outcome | Seconds | Tools | Patch | Highest reported input |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| `20261110T164003Z-8b761c59` | 32,768 / 4,096 | completed; official evaluator resolved | 820.424 | 33 | 597 B | 14,099 |
| `20261110T170610Z-ae402f05` | 16,384 / 2,048 | output limit | 283.105 | 10 | 0 B | 6,389 |
| `20261110T171140Z-99e9173a` | 16,384 / 4,096 | summary candidate cannot fit | 354.485 | 18 | 0 B | 6,073 |
| `20261110T172238Z-2af8558f` | 20,480 / 4,096 | protected floor cannot fit | 358.433 | 18 | 0 B | 8,378 |
| `20261110T181510Z-90e347d4` | 16,384 / 4,096; `--read-page-bytes 4096` | protected floor cannot fit | 329.346 | 13 | 0 B | 6,777 |
| `20261110T183604Z-3bf644e1` | 16,384 / 4,096; `--read-page-bytes 4096`; task effort `none` | completed; official evaluator resolved | 205.705 | 20 | 485 B | 7,685 |

The second-to-last row is the first Phase B observation: diagnostic instrumentation and
opt-in 4 KiB read-page guidance were active, with the prompt and global limits
otherwise unchanged. It produced **14 requests: 9 task and 5 summary**. The final
protected-floor breakdown was 18,693 reasoning bytes, 656 tool-result bytes,
3,027 retained-evidence bytes and 0 derived memory, estimated at 13,415 input
tokens plus 4,096 reserve. Read-page guidance did not by itself remove the floor:
replayed reasoning dominated this trajectory. Maximum provider input remained
6,777 tokens (41.4% of the window), so again no dispatched request approached
16K; the runtime refused the next candidate.

The last row confirms that the opt-in Ollama `task_reasoning_effort: "none"`
request was accepted and the response contained no reasoning bytes. It produced
**22 requests: 20 task and 2 summary**, maximum provider input 7,685 tokens
(46.9%), maximum estimated input plus output 14,348, and no context-limit stop.
The 485-byte patch added `__slots__ = ()` to `Printable`; the pinned official
evaluator resolved the instance. This is one additional attempt on an exposed
task, not independent task evidence or proof that one setting caused the
difference. Other trajectory and sampling effects remain possible.

All used the `ollama-local/qwen3-27b` binding, wire model
`qwen3.8-27b-iq3m-92k:latest`, a 60% trigger and 10% target. This is a previously
exposed SymPy task, not a blind sample. The first 16K attempt and its changed-cap
retry are distinct attempts; neither may be hidden in aggregate reporting.

Primary evidence is `launch.json`, `server.log`, `model-output.log`,
`artifacts/sympy__sympy-20590.result.json`, the original prediction, and the
32K run's `artifacts/official-evaluation/` report. Relevant facts include:

- The 32K log contains **38 request-start events: 31 task and 7 summary**.
  Four committed context reductions are not four summary invocations. The earlier
  conversational claim of 94 requests is not supported by these events.
- The 16K/2048 log shows two reasoning-only output-limited responses, with no
  accepted tool candidates; the existing bounded recovery was exhausted.
- The 16K/4096 candidate cost was 12,817 estimated input plus 4,096 reserve:
  **16,913 > 16,384**. This was a rejected candidate, not dispatched usage.
- The 20K protected floor cost was 18,370 estimated input plus 5,120 reserve:
  **23,490 > 20,480**. The runtime skipped a futile summary invocation.
- Request-start estimates and post-tool/pre-compaction estimates are different.
  For example, the 32K server log records a pre-compaction peak of 21,085, while
  request-start input estimates peak at 18,480.

Reported provider input was below the configured window, but that alone is not
proof of an exact hard total-token bound for every invocation. A complete claim
requires input and generated-output accounting, coverage of summary/retry calls,
and verified backend counting/context behavior. A model name containing `92k`
does not prove its serving window, and the Nexus setting does not itself
reconfigure Ollama's allocated context.

## 3. Current implementation and root-cause hypotheses

### Budget arithmetic

`ContextManagementSettings::output_reserve_tokens` in
`crates/nexus-core/src/context.rs` reserves:

```text
R = max(clamp(floor(W / 4), 1, 16384), configured_task_output_cap_or_zero)
I + R <= W
```

Thus 16K reserves 4,096 even with a 2,048 task cap; 20K reserves 5,120 with
a 4,096 cap; 32K reserves 8,192 with a 4,096 cap. This is conservative planning
headroom, not necessarily the output limit actually sent on a request.

Input estimation includes instructions, notes, tools, conversation, reasoning,
arguments, and results. The byte heuristic starts with a 1.25 safety factor and
already calibrates from usage. Task and summary estimators are separate. Do not
remove the safety margin just to obtain a passing budget check.

### Protected floor and replacement memory

`plan_summary_reduction` in `crates/nexus-context/src/lib.rs` retains the current
task, existing deterministic evidence, and the newest three complete tool groups.
A group can contain multiple parallel calls, long arguments, output, and required
reasoning. Older uncertain effects are represented by deterministic evidence,
not merely by model-written memory. Some groups must remain pinned.

`prepare_context_request` in
`crates/nexus-runtime/src/runtime/context_compaction.rs` plans with the maximum
accepted replacement memory: 8,192 bytes. At the initial estimate scale this is
approximately 3,414 tokens before framing, already above the 16K profile's
1,638-token target. The target is therefore best effort, not an attainable
postcondition. Reducing the target percentage alone does not fix this floor.

### Capabilities already available

| Existing capability | Implication for this proposal |
| --- | --- |
| UTF-8-safe old-output previews and emergency previews down to a 128-byte allowance | Improve ordering and budgeting; do not add a second truncation framework |
| Archived accepted source records and bounded source loading | Preserve originals; do not introduce another database |
| `host_read` byte `offset`/`limit`, default 8,192 bytes, maximum 16,384 | First experiment can use existing 2–4 KiB pages |
| Tool-free isolated summary requests, 24 KiB initial chunks, splitting on insufficient headroom | Size chunks against complete requests earlier rather than adding basic chunk support |
| Bounded empty/incomplete/output-limit summary recovery | Retain existing retry limits; do not silently add unbounded retries |
| Transactional candidate validation and protected-floor preflight | Keep fail-closed behavior and avoid spending calls on impossible candidates |

Output previews currently follow reduction planning in the main request path;
floor checks and summary acceptance also have emergency preview handling. A
projection-first experiment must account for these existing branches rather
than assume all previews are absent.

`host_search` currently offers literal path/query search without a result cursor
or match-limit argument. Richer bounded search controls are a possible later
extension, not a prerequisite to the first experiment.

### Hypotheses to test, not established causes

Large recent results, replayed reasoning, accumulated evidence/arguments, summary
memory allowance, and repeated summary work can each dominate a small window.
Component-level telemetry is needed to distinguish them. The logs do not justify
attributing all failures to large tool output or to model capability alone.

## 4. Safety and compatibility constraints

Every phase must retain these invariants:

1. Archive-dependent removal requires confirmed durable source persistence.
   Save failure, uncertain storage, or absent storage keeps the conservative path.
2. Preserve complete call/result pairing, parallel groups, provider references,
   required reasoning, current user requirements, and pending operations.
3. Unknown effects remain unknown. No omitted result or derived summary grants
   permission to retry a mutation, reuse an approval, or claim verification.
4. Derived memory and archived text remain untrusted data, not host instructions.
5. Invalid, incomplete, oversized, cancelled, or unfit summaries do not partially
   replace accepted conversation or memory. Multi-chunk publication is atomic.
6. All summary/recovery calls consume the same finite turn, time, and usage
   budgets. Cancellation and deadlines are checked before dispatch and commit.
7. Original outputs remain unchanged in the archive. Active-view previews state
   omissions and do not masquerade as full evidence.
8. Missing usage/windows remain unknown. Opaque continuations are not rewritten
   unless the provider contract explicitly supports that operation.
9. An irreducible valid request that exceeds the window stops with a precise
   reason. No optimization is allowed to pass by silently deleting protected facts.

The initial work must not change persistence formats, public configuration,
event schemas, or global defaults without an explicit compatibility review.

## 5. Proposed implementation sequence

### Phase A — Explain the budget before changing policy

Extend existing diagnostics and benchmark metrics with bounded metadata, not raw
prompt content. Reuse request identity and task/summary purpose correlation.

Report window, configured output cap, planning reserve, final wire output cap,
raw and calibrated estimates, preview level, reason for compaction, and minimum
protected floor. Provide a component breakdown for instructions/notes, tool
definitions, task text, assistant text, reasoning, call arguments, result bodies,
derived memory, and deterministic evidence. Component estimates must explicitly
account for framing/rounding residuals instead of pretending token counts are
perfectly additive.

Separate retained-view, rejected-candidate, and final-dispatch estimates. The
commit diagnostic currently estimates a view that may later be previewed; it
must not be mislabeled as the actual dispatched occupancy. Account for all
request purposes, retries, unavailable usage, summary commits, discarded output,
and persistence errors.

Primary files: runtime `context_compaction.rs` and `runtime.rs`,
`tools/benchmarks/run_nexus_swe_task.py`, and `tools/benchmarks/context_metrics.py`.
Prefer existing diagnostic channels; a new public event needs schema review.

Exit criterion: deterministic traces explain every limit and dispatched budget,
including one missing-usage case, without recording credentials or source text.

### Phase B — Reduce avoidable ingestion and project before summarizing

First run a configuration/instruction-only experiment using existing 2–4 KiB
`host_read.limit` pages, narrow searches, and bounded command output. Keep the
model, window, task cap, trigger, target, and repair prompt otherwise unchanged.
Do not globally reduce `max_tool_output_bytes`: that budget also participates in
provider response handling and can introduce a second failure mode.

Then change request preparation in one focused slice: try deterministic previews
of eligible older output before scheduling an LLM summary. Preserve the normal
recent-group protection initially. If the complete projected request is below
both the soft trigger and hard headroom ceiling, dispatch it without a summary.
If it still needs reduction, use the existing archived summary path with original
records as source. Never summarize an excerpt as if it were the original record.

Preview choices must be reapplied consistently while estimating planner
candidates, checking the protected floor, validating a summary, and preparing the
final request. Keep preview state derived and run-local; do not overwrite source
records or create divergent validation/dispatch paths.

Exit criterion: a large old-output fixture fits without any summary invocation,
recent metadata and original source remain unchanged, and a reread can recover
omitted detail. Measure extra rereads as well as saved summary calls.

### Phase C — Fit summary memory and chunks to the actual window

Use a window-aware accepted-memory allowance within the existing 8 KiB maximum.
Start with a 2 KiB candidate, then a separate 4 KiB experiment if needed. These
are byte limits, not token limits or task generation caps. Do not claim either
size is an established optimal default.

Derive the allowance from the complete protected request, reserve, memory
framing, and safety estimator. Planning and acceptance must use the same bound.
If even minimal useful memory cannot fit, fail before making a summary call.
Never byte-truncate a returned summary into validity; reject it or use an
explicitly bounded recovery under the existing lifecycle.

Keep the `Established / Ruled out / Open / Next` structure. Charge existing memory
when merging and ensure the final replacement fits as a whole. Protect exact
symbols, user corrections, negations, test state, and uncertain effects; compact
prose must not turn an earlier test into evidence for a later patch.

Build source chunks using the full estimated summary request, including rolling
memory, task relevance, framing, and generation allowance. Prefer complete
record/group boundaries. Preserve labelled UTF-8-safe fragments for individually
oversized records and existing split/retry bounds. Replace Rust `Debug` rendering
with a small explicit historical-data renderer only as a separate measured slice;
it must preserve chronology, identities, outcome facts, and trust labels.

The current relevance excerpt is the first 4,096 task bytes. Test late user
constraints/corrections before changing it; do not silently omit important tail
requirements in the name of smaller summaries.

Exit criterion: 16K fixtures accept a useful fitting replacement, every summary
dispatch fits its budget, and a second-chunk failure leaves the accepted view
unchanged. Compare semantic fidelity, summary calls, latency, and reread costs.

### Phase D — Resolve remaining protected-floor pressure, conditionally

Do not start by relaxing protection. Enter this phase only if component evidence
shows that B/C leave a material reasoning, recent-group, or reserve floor.

| Candidate | Proposed boundary | Required evidence before adoption |
| --- | --- | --- |
| Token-bounded recent retention | Prefer three full groups when affordable; consider compacting older completed groups while retaining the newest complete group and mandatory facts | Explicit design review of changed retention semantics; parallel/pending/uncertain cases remain valid |
| Task reasoning control | Provider-specific opt-in hint with current behavior as default | Verify the exact endpoint/model contract and inspect wire requests; a hint is not a hard reasoning-token bound |
| Optional reasoning replay | Omit historical reasoning only where the provider explicitly permits it | Adapter capability contract and protocol tests; archive originals; never apply to opaque/signed/required reasoning |
| Explicit-cap-aware reserve | Consider using a configured cap instead of the larger legacy quarter-window reserve | Separate compatibility experiment; explicit cap preserved on wire, summary budget isolated, no unbounded output |
| Compaction hysteresis | Avoid repeated low-savings work when protected floors exceed a soft target | Trace-confirmed churn; hard-limit checks always override cooldown |

The adapter currently replays accepted reasoning as `reasoning_content`; summary
calls request low reasoning effort only on detected Ollama Chat Completions
endpoints. Displaying less reasoning is not the same as sending fewer tokens.
Consult current provider documentation at implementation time before changing
these behaviors. No assumptions about thinking controls follow from the model
name alone.

Do not combine these factors in one benchmark candidate. A reserve change offers
no reduction for 16K with a 4,096 cap under today's formula; it matters for other
cap/window combinations and must not be sold as the primary fix for this run.

Exit criterion: the identified component shrinks without protocol violations,
lost uncertain effects, or weakened generation headroom. Otherwise retain the
current failure and document the supported operating range.

### Phase E — Default promotion and measured implementation overhead

After behavior is validated, profile candidate preparation if it is significant
relative to model latency. Cache immutable estimate components or reduce repeated
request reconstruction only when measured. Avoid refactoring the request
pipeline merely to reduce code duplication.

Promote only the independently useful stages. Keep experimental reasoning and
retention behavior opt-in until their compatibility gates pass. Update affected
configuration/design/testing documentation when public behavior changes.

## 6. Deterministic validation matrix

Extend existing tests rather than adding an alternate harness. The existing
shrinking-window runtime test covers 94,208, 65,536, and 32,768; include 20,480 and
16,384 explicitly. Some existing 16K fixtures do not constitute full 16K coverage.

| Area | Required cases and expected result |
| --- | --- |
| Arithmetic | `I + R == W` fits; one token above does not; explicit caps below/equal/above the legacy reserve; unknown window and invalid cap |
| Preview-first | Old large result needs no summary after preview; omission notice is charged; reread succeeds; original archive is byte-identical |
| Floor | Large required reasoning/arguments and many uncertain outcomes fail precisely before futile summaries; no identities are clipped |
| Memory | 2/4/8 KiB allowances; framing and calibration included; bound identical in plan/accept; oversized candidate rejected transactionally |
| Chunks | Multibyte records, large single group, rolling memory growth, late task correction, second-chunk failure, bounded output-limit splitting |
| Semantics | Corrections/negations survive; exact file/symbol references survive; pre-patch tests never become post-patch verification |
| Protocol | Parallel calls, denied calls, pending results, provider-required reasoning, opaque continuation, and malformed pairing |
| Lifecycle | Source-save failure, uncertain store, cancellation, deadline, final available turn, partial output, restore without execution/approval replay |
| Observability | Task/summary/retry counts reconcile; dispatch estimate matches final request; missing Final usage remains incomplete |
| Regression | 32K/64K/92K behavior, ephemeral conservative paths, and user/default configuration remain compatible |

Primary locations are `crates/nexus-context/src/lib.rs`, runtime
`context_compaction.rs`, `crates/nexus-integration/tests/context_compaction.rs`,
and the existing `nexus-openai` provider/conversation tests. Tool changes also
need native/container parity tests; integration modules in packages with a
`tests/behavior.rs` entry point must be registered there.

Run affected-package Cargo commands sequentially, following `docs/testing.md`.
For core/context/runtime changes, the starting command selection is:

```sh
cargo fmt --all --check
cargo nextest run -p nexus-core -p nexus-context -p nexus-runtime --locked --offline
cargo clippy -p nexus-core -p nexus-context -p nexus-runtime --all-targets --locked --offline -- -D warnings
```

Add `nexus-integration`, `nexus-openai`, or tools/container packages only when
their subject changes. Use the benchmark Python regression commands documented
in its README for metric/launcher edits. Do not enable ignored/low-value suites
as routine validation or treat a live-model run as a substitute for these tests.

## 7. Controlled live evaluation

Before inference, freeze executable/revision identity, effective configuration
and hash, model identity, image digests, task/base commit, network evidence,
budgets, and declared attempt count. Snapshot only redacted settings and secret
references; do not copy credentials into evidence. Validate configuration with
the same executable that will run the task.

Verify backend context configuration separately from the Nexus logical budget.
If the server cannot enforce/verify 16K, label the experiment a Nexus 16K budget
test, not a verified backend 16K allocation test. Detect backend truncation where
possible; silently truncated input invalidates the context-preservation result.
Do not unload/reconfigure shared models without authorization.

Use fresh checkouts and sessions. Keep the 16,384 window, 4,096 task output cap,
60% trigger, 10% target, 1,800-second and 4,096-tool budgets as the reference
profile. Vary only the named candidate factor. A 45–50% trigger can be a later
standalone experiment, not an undocumented adjustment alongside other changes.

Suggested experiment progression:

| Comparison | Purpose |
| --- | --- |
| Matched current baseline vs smaller existing read pages | Isolate ingestion cost without runtime changes |
| Baseline vs preview-first | Determine whether deterministic projection avoids summary work |
| Selected prior candidate vs adaptive memory/chunks | Test fitting summary candidates and cost/fidelity tradeoffs |
| Selected prior candidate vs one Phase D factor | Address only a measured residual floor |
| Final candidate vs baseline at 32K | Detect regressions outside the pressure profile |

Predeclare three attempts per arm for the exposed SymPy regression, subject to
user-approved inference cost. Do not stop after the first success or silently
rerun a failure with changed settings. For generalization, preselect at least
three tasks from other repositories/problem types before examining results;
declare any previous exposure. Repeated attempts on one task are not independent
task samples and do not justify a broad success-rate claim.

Evaluate captured predictions with the pinned official evaluator, including
nonempty partial patches from abnormal runs. Keep agent terminal outcome,
runner outcome, and evaluator result separate. Empty patches remain explicit;
do not imply a failed agent run resolved because no evaluator was invoked.

Report request counts by purpose, input/output coverage, maximum per-invocation
input plus output, estimated reserve occupancy, floor composition, summary calls
and commits, discarded generation, tool calls/rereads, latency, patch size,
source-save failures, and owned-resource cleanup. Aggregate usage is cost, not
context occupancy. Where exact usage is missing, mark hard-bound verification
incomplete rather than substituting estimates for measured tokens.

Launcher cold-start issues must not be worked around by repeatedly editing
tracked scripts. If a longer session-creation timeout is still necessary,
implement a separately tested explicit option or controlled wrapper, freeze it
for both arms, and distinguish startup failures from model attempts.

## 8. Acceptance, stopping, and rollback

Mandatory acceptance is preservation of all safety invariants, valid complete
requests within the estimated budget, honest usage coverage, and no regression
in deterministic 16K/20K/32K scenarios. Exact hard bounds require a verified
backend/token-count contract; heuristic success must remain labelled heuristic.

The performance objective is fewer avoidable floor/summary failures at 16K with
official repair quality maintained. Compare summary overhead, extra rereads, and
total time rather than optimizing input tokens alone. A candidate that lowers
tokens but worsens repair quality, uncertain-effect handling, or termination is
not an improvement. Report small-sample results descriptively, including every
failure; no universal 16K support claim follows from this pilot.

Stop promotion on lost evidence, malformed protocol, silent source truncation,
partial commit, budget bypass, new unbounded retry, or unexplained 32K regression.
Keep new policy scoped/opt-in and revert only the relevant implementation change;
retain all artifacts and user data. Persistent schemas should remain unchanged
so rollback needs no migration.

If the minimum valid request still cannot fit, preserve an actionable diagnostic
and recommend a larger window. That is a legitimate outcome. The practical goal
is a better bounded operating range, not forcing every task through 16K at the
expense of correctness.
