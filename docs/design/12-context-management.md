# Context Management and Evidence-Preserving Evaluation

Date: 2026-10-25.

Status: project-owner-confirmed priority and direction, with a proposed detailed
design for review. This document records the requested architecture, evaluation
workflow, and shrinking-window objectives; it does not claim that the complete
subsystem, telemetry, or acceptance gates are implemented. Concrete Rust APIs,
storage formats, and statistical thresholds remain subject to implementation
review. Existing accepted security and session decisions still apply.

## 1. Priority, Outcome, and Scope

**Context Management is a central engineering priority for Nexus, not an
optional convenience or a benchmark-specific optimization.** A small execution
core must reliably carry task requirements and execution evidence through long
runs under constrained model windows. Quality, recoverability, and honest
failure take precedence over headline compression ratios.

The delivery target is a minimal, provider-independent subsystem that can:

- Separate original accepted interaction records from bounded model context.
- Budget input and generation capacity, compact transactionally, and recover
  from failed compaction without corrupting the active context.
- Preserve requirements, call/result consistency, and uncertain effects.
- Support conventional summarization now and traceable evidence representations
  later without changing tool authorization or the execution state machine.
- Demonstrate its value through repeatable tests at 92K, 64K, and ultimately 32K.

Logical responsibilities do not imply five crates, five traits, or a new
framework. Do not add a planner, reviewer, sub-agent orchestration, graph/vector
database, Semantic IR implementation, or LoRA training in this work.

## 2. Current Implementation and Gaps

Baseline inspected: context fixes through `40155e9`. Later implementation must
recheck these locations rather than assume this snapshot remains current.

| Existing component | Reusable behavior | Remaining distinction |
| --- | --- | --- |
| `crates/nexus-context/src/lib.rs` | Pure estimation/calibration, paired-exchange selection, bounded history projection, summary validation, UTF-8-safe tool-output previews | Selection and derived representations are not an independent source-history store |
| `crates/nexus-runtime/src/runtime/context_compaction.rs` | Request construction, staged summarization, validation/commit, truncation fallback, emergency previews | Consolidate ownership and expose structured decisions rather than scattered diagnostics |
| `crates/nexus-runtime/src/runtime.rs` | Agent loop, accepted results, cancellation, usage, active conversation, bounded session cache | Both hard-limit projection and compaction replace active conversation; the cache can evict records |
| `nexus-core` request/result types | Call identities, status/effect/evidence, provider continuation, settings | Reuse domain facts; do not add algorithm-specific memory types to the core |
| `tools/benchmarks/run_nexus_swe_task.py` | Tool/usage/reduction logs, result metadata, prediction export, evaluator separation | Structured event retention, complete compaction classification, and comparable derived metrics are not yet present |

The output previews preserve their source records, but the complete existing
history pipeline is **not** an append-only archive: safely removable exchanges
can leave active conversation and the session cache. Do not describe current
compaction as preserving every original interaction indefinitely.

Existing safeguards include unknown windows staying unknown, explicit settings
overriding discovery, provider-usage calibration, and disabling incompatible
history rewriting with opaque continuations. Retain them.

## 3. Ownership and Minimum Architecture

```text
runtime: authoritative lifecycle and execution
  |
  +-- History Store: original accepted records and bounded range reads
  |       +-- session storage adapter: persistence mechanics
  |
  +-- Context Manager: active view, pending candidate, request preparation
          +-- Token Budget: estimates, calibration, generation reserve
          +-- Compaction Strategy: selection and derived-view candidates
          +-- Context Builder: final request assembly and validation

nexus-context: pure budget/selection/projection logic; no external I/O
provider adapter: wire mapping and protocol compatibility
benchmark caller: structured observation, artifacts, and evaluation
```

### History Store

Maintain original accepted user messages and corrections, accepted assistant
interactions, tool intents/results, and run termination facts under stable
record IDs. Associate records with session/run/call identities and ordered
revisions. Record uncertainty explicitly; a failed run can still have effects.

Store the bounded result actually supplied by a tool, including its truncation
flag. This does not imply capturing unlimited raw stdout or recovering bytes
already omitted during tool collection. Reject or explicitly reference content
that cannot be recorded; never claim lossless archival of uncaptured data.

Compaction changes the active view, not original records. Provide bounded
read-by-ID/range access for inspection and later reconstruction. Initial access
need not be a model-facing tool or semantic retrieval service. Do not silently
reinsert archived history into every future request.

History is logically append-only during the declared retention lifetime. Quotas,
retention expiry, user deletion, and redaction must be explicit and must not
leave required recovery evidence pointing to silently missing content.

### Context Manager and Builder

The runtime owns active source references, protected records, recent exchanges,
derived artifacts with provenance, preview choices, and any pending candidate.
An active view may omit source records without destroying them.

Keep the builder as functions inside this responsibility initially. It combines
protected instructions, original task and subsequent corrections, marked
derived memory, selected exchanges, tool definitions, runtime notes, and
required provider round-trip fields into a complete request. Validate the final
request, not only transcript text.

User instructions remain distinguishable from untrusted repository/tool text
and derived summaries. Preserve the chronology of corrections rather than
silently declaring contradictory requirements resolved.

### Token Budget

Track three independent constraints: model input/generation capacity, request
byte/item ceilings, and archive/materialization capacity. A small model request
does not imply bounded archive memory.

Retain 65% trigger and 10% target defaults; targets including 5% remain valid
configuration, not promises that protected evidence always fits. Reserve model
output, include all request categories, and account for provider usage on the
matching invocation. Cumulative tokens are not current occupancy. Estimates
remain heuristics, never exact counts or guaranteed upper bounds.

Unknown windows stay unknown; do not infer capacity from model names. Explicit
configuration wins over discovery. Unknown windows still obey byte/item limits,
but cannot support an invented token occupancy percentage.

### Compaction Strategy

Use one narrow internal replacement boundary: read-only source/view information,
protected ranges, task relevance, and a budget target produce a candidate view
or a bounded request for summarization. These are semantic responsibilities,
not prescribed Rust signatures or a public plugin ABI.

The runtime executes provider calls, handles cancellation and accounting, then
validates the candidate. Strategies cannot dispatch tools, grant permissions,
change source outcomes, or directly commit runtime state. Start with existing
deterministic selection, textual summary, and output-preview mechanisms. Avoid
a general pass registry or strategy service until a concrete replacement needs it.

## 4. Record and Derived-View Invariants

Maintain a minimal distinction between:

| Information | Meaning |
| --- | --- |
| Original record | What the host accepted or observed, with stable identity |
| Derived artifact | Summary/interpretation with identity, source IDs/ranges, source revision, and derivation metadata |
| Active view | Ordered source references and derived replacements used to construct a request |
| Candidate | Uncommitted replacement based on a particular source/view revision |

A text summary is the first derived representation, not the permanent data
model of all memory. Future structured artifacts can render to the same model
request boundary without changing the execution loop.

Protect call/result associations and original status/effect/evidence. A successful
shell exit does not establish `KnownNotApplied`; uncertainty is not erased by
summarizing or shortening output. Original arguments and protocol-required
reasoning/identities remain protected. Do not split parallel groups into invalid
provider message sequences.

Output previews must explicitly say content was omitted and preserve the
original truncation/status/effect/evidence information. Mark projected results
truncated. Prefer recent complete output; use emergency previews of the latest
complete group only when required for generation headroom. Never project an
incomplete group as if all results had arrived.

## 5. Normal Execution, Compaction, and Recovery

Normal flow:

```text
accept user input -> record source -> prepare complete request -> budget check
  -> provider call -> accept/record interaction -> authorize and execute tools
  -> record actual outcomes -> prepare next request
```

Compaction flow:

```text
snapshot source/view revision -> select replaceable ranges
  -> deterministic candidate or tool-free summary call
  -> build full candidate request -> validate invariants and budgets
  -> recheck revision/cancellation/deadline -> commit active view atomically
```

The summary input and output require their own budgets; summarization cannot
require dispatching an already oversized request. Summary calls consume model
turns, tokens, and time, including failed attempts. They must be identifiable
separately from normal task calls.

Prefer safe exchange selection and a bounded summary when configured; if that
fails, validate a deterministic fallback separately. Old output previews and
then emergency latest-group previews can supply headroom without rewriting
source records. The current emergency mechanism reduces head/tail allowance
from 2 KiB to 128 bytes per result, with the omission notice additional. Reuse
it initially rather than invent a second preview implementation.

Failed candidates leave the previous active view unchanged. Cancellation or
timeout abandons candidates, not already executed effects. Incompatible opaque
continuations prohibit transcript rewriting unless an adapter contract explicitly
permits it. Protected metadata that still cannot fit causes a truthful limit
termination, not an orphaned result or an unbounded retry loop.

Use the current `prepare_context_request`, `ingest_summary`, and staged commit
paths as integration points. Source recording belongs at accepted input/turn
and tool intent/outcome boundaries, not in Sandbox implementations. Consolidate
hard byte/item and token handling behind the same request-preparation semantics
without conflating their limits.

## 6. Persistence, Quotas, and Accepted Session Defaults

[Decision 01](decisions/01-first-release-defaults.md) already requires automatic
local session saves outside the project and explicit restoration. This design
does not replace that requirement with default ephemeral history.

Reuse the [session store boundary](../contracts/04-session-store.md); prefer a
small file-backed implementation, not a separate context database. Keep source
history and active-view checkpoints distinct logically even if stored together.
Bound resident materialization and load archived ranges on demand.

An explicitly selected ephemeral mode or staged test implementation may use an
in-memory archive with a declared quota. It must disclose no crash recovery,
stop honestly at capacity, and must not be presented as satisfying persistent
session acceptance. Finite memory, unlimited execution, and complete retained
history cannot all be guaranteed without external storage or explicit limits.

Persistent mode must honor intent-before-dispatch durability and actual
outcome recording under the session contract. A storage error after effects
must not trigger re-execution. Restoring history never replays tools, resumes
an interrupted run automatically, or restores old approval grants. Do not claim
crash durability based on buffered writes or tests of an in-memory double.

## 7. Path to the Compiler-Inspired Evidence System

The README's [task-context compiler research](../research/task-context-compiler.md)
remains a future direction, not a dependency of conventional compaction.

| Initial mechanism | Future extension |
| --- | --- |
| Original records with stable IDs | Traceable source material for reinterpretation |
| Summary with source ranges | Typed Semantic IR with provenance |
| Selection/compaction boundary | Analysis, retrieval, and transformation passes |
| Context Builder | Rendering an evidence slice into model context |
| Revision validation and candidate commit | Publishing checked derived representations |

Future dependency, version, conflict, and applicability checks belong behind
this boundary. For example, test observation T1 for patch P1 remains historical
truth after P2 is created, but cannot establish that P2 passed. Store observations
separately from conclusions now; do not build a dependency graph prematurely.

Retrieval finds candidates, not truth or authorization. Graph/vector indexes,
specialized models, and LoRA can later replace internal selection/interpretation
mechanisms, but cannot override execution invariants. The extensibility promise
is no redesign of tool execution/authorization for a new memory strategy, not a
promise that every future capability needs zero configuration or interface work.

## 8. Benchmark Evidence and Git-Friendly Storage

Implement measurement in `tools/benchmarks`, reusing the existing SWE runner.
Short synthetic context probes and SWE adapters should emit the same small
versioned observation envelope and use the same offline metric derivation.
This is evaluation tooling, not a second runtime or a general telemetry platform.

Use append-only JSONL as the measurement fact source, human logs for inspection,
and derived JSON for summaries. SQLite is an optional, rebuildable local query
index once cross-run volume justifies it; do not make a binary database the
Git-reviewed source of truth or add a database dependency to Agent Core.

```text
tools/benchmarks/             tracked collectors, analyzers, fixtures, tests
external-benchmark-root/
  <unique-run-id>/
    manifest.json            configuration and provenance
    events.jsonl             structured observations
    metrics.json             derived values, coverage, metric revision
    runner.log               human diagnostics, not a parsing contract
    prediction.jsonl         existing benchmark prediction format
    patches/                 optional candidate snapshots and their identities
    evaluator/               independent authoritative evaluation artifacts
```

Keep raw runs outside the repository. Commit selected sanitized comparison
results and conclusions where useful, not private traces, credentials, database
files, or entire benchmark checkouts. Benchmark events are not a crash-recovery
journal and do not substitute for the History Store.

The manifest records Nexus commit and dirty-tree fingerprint, task/dataset and
base revisions, model digest when available, provider/adapter configuration,
requested and verified windows, compaction policy, generation settings, runtime
and runner limits, tool/image revisions, evaluator revision, platform, and
whether the task is exposed/debug or unexposed evaluation. Unknown facts remain
unknown. Separate agent workload and evaluator configuration and timing.

Events carry schema version, run identity, sequence, monotonic elapsed time,
model invocation ID/purpose (`task` or `summary`), tool call IDs, compaction ID,
and relevant source/view revision. Record budget decisions, estimates, final
usage, summary attempts/results, committed reductions, output previews,
tool outcomes, patch snapshots, and termination classification.

Expose missing runtime facts through bounded structured events and let the
caller write them; do not add routine runtime stderr logging. Associate usage
with its invocation and avoid double counting streamed/final representations.
Missing telemetry is `null` with a reason, never zero or invented success.
Preserve valid event prefixes on interruption and mark collection incomplete.

Do not log complete tool arguments or output by default. Use local normalized
argument fingerprints for duplicate candidates; hashes are not guaranteed
anonymization. Apply privacy/retention controls to snapshots, prompts, and
results. Failure of optional telemetry must be surfaced, not silently treated as
a complete run; it must never authorize retrying an executed operation.

## 9. Metric Definitions

| Metric | Required interpretation |
| --- | --- |
| Resolved Rate | Official evaluator result, not agent claims or patch existence. Report resolved/planned cases, evaluation coverage, unresolved, unevaluated, and infrastructure failures; do not silently remove failures from the denominator. |
| Compaction Count | Separate attempts, successful commits, summary failures, truncation fallback, old-output previews, and latest-group emergency previews. One summary fallback is not two successful compactions. Current `context-reduced` events alone are insufficient. |
| Post-compaction Recovery | Associate each committed compaction/preview with subsequent request success, tool-loop continuation, and independently evidenced progress. Report these separately; another tool call alone is not progress. State the observation horizon and classify end-of-run cases without observations as unknown/not applicable. |
| Repeated Tool Calls | Detect candidates by tool revision, normalized arguments, relevant repository/environment state, and earlier outcome. Re-reading after changes or rerunning validation can be legitimate. Distinguish exact repeats from reviewed redundant work; unknown effects never imply safe repetition. |
| Peak Context | Report maximum provider-reported per-invocation input tokens separately for task and summary calls, plus maximum runtime estimate and known window. Incomplete usage yields an observed lower bound, not a certified maximum. Provider usage semantics may differ across protocols. |
| Total Input Tokens | Sum known final usage for all model invocations, including summaries and failed attempts with reported usage. Include completeness and task/summary breakdown; also report output tokens, time, and available cached-token accounting. Lower input alone does not prove lower total compute or monetary cost. |
| Time to First Valid Patch | Timestamp candidate snapshots and evaluate them independently after the agent run. Report the earliest evaluated passing snapshot at snapshot resolution. Without snapshot evaluation report only time to first nonempty patch; never relabel it as valid. |
| Normal Completion | Agent stopped autonomously without a runtime/runner budget stop, cancellation, provider/infrastructure failure, or forced termination. Report separately from resolved status; an agent can complete normally with an incorrect patch. |

Keep evaluator infrastructure errors distinct from unresolved patches. Candidate
evaluation is opt-in because it can be expensive; pin snapshot capture boundaries
and verify snapshot identity without perturbing the live workspace. Never feed
official/hidden evaluator feedback into the running agent. If no valid snapshot
was observed, distinguish evaluator-confirmed failures from missing observations.

Reports must include definitions/metric revision, completeness, per-task values,
and paired baseline/candidate differences, not only aggregate means. Count
compression oscillation and repeated investigation as diagnostics. A run that
fails earlier can use fewer tokens; compare resource use alongside success and
termination, including resource use per resolved case where meaningful.

## 10. Shrinking-Window Validation: 92K -> 64K -> 32K

The fixed initial tiers are **94,208**, **65,536**, and **32,768** tokens. Record
exact counts rather than trusting marketing labels. Keep model digest fixed
where supported; changing the model introduces a separate experimental variable.

Distinguish two experiments:

1. Runtime-budget-only: constrain Nexus while provider capacity is larger;
   isolates selection/budget behavior, but is not actual small-window validation.
2. End-to-end: configure and verify the provider's effective window as well;
   record evidence of the effective setting and actual usage. A runner reference
   threshold or model name alone is not verification. Unsupported/unverifiable
   window control is a limitation, not a passed tier.

Hold task content, base checkout, execution budgets, tool/image configuration,
and generation settings constant across tiers. Keep 65%/10% policy initially;
tune policy in separate experiments. Record output reserve/cap changes that
follow window size so they are not mistaken for an unchanged configuration.
Use fresh runs and do not carry derived summaries between tiers. Fix seeds where
supported, but do not assume deterministic model/service behavior.

### Fast synthetic lane

Use inert local fixtures rather than software tasks to create near-threshold,
near-window, and over-budget input quickly. Scale data by window fraction,
calibrating provider usage where feasible, and cap time, calls, generated output,
and total tokens explicitly. Live tests remain opt-in and sequential per shared
model service to avoid misleading contention measurements or unbounded cost.

Cover sequential accumulation, a single parallel burst, repeated compactions,
summary output-limit/failure, UTF-8 previews, unknown effects, cancelled/timed-out
tools, incomplete groups, oversized protected metadata, and incompatible opaque
continuations. Deterministic tests cover failure cases without paid/live calls.

Require valid pairing and unchanged source records, bounded projected requests,
and truthful failure when protection alone exceeds capacity. Verify actual
provider invocation and response, not merely request admission. A near-full
native provider probe tests capacity; it does not by itself test Nexus compaction.

### Fixed SWE lane

Use a small pinned set of exposed/debug tasks for iterative paired regressions;
keep workload unchanged at smaller windows. Run the official evaluator separately.
Only expand after the short lane passes, under an explicitly chosen run budget.
Use unexposed tasks for later generalization checks; do not report exposed replay
results as blind benchmark scores.

Compare all eight metrics at every tier, including compaction recovery, cost of
summaries, repeated work, and normal completion. Watch for repeated compression
with no progress and smaller windows increasing total tokens through rework.
Use repeated trials when making model-quality claims, show paired task outcomes,
and disclose small-sample uncertainty rather than inventing significance.

Safety gates are non-negotiable at every tier. The first 32K milestone is safe,
diagnosable degradation; the final goal is useful repair capability approaching
the 92K baseline with acceptable cost. Define numeric quality/cost tolerances
before confirmatory runs after measuring an initial baseline; do not retroactively
choose thresholds to label a regression successful.

## 11. Development Workflow and Delivery Phases

Every meaningful context change follows: deterministic regressions, bounded
synthetic probes, paired fixed-task runs when warranted, independent evaluation,
and a recorded baseline/candidate conclusion. Routine documentation changes do
not trigger live benchmarks. Follow [testing guidance](../testing.md) and never
run concurrent Cargo commands against the same target directory.

**Phase A — Measurement foundation.** Add versioned caller-side JSONL collection,
offline metric derivation, fixture tests, run provenance, and explicit missing-data
handling. Preserve existing prediction/evaluator separation. This makes later
architecture changes measurable without waiting for advanced metrics.

**Phase B — Source/view separation.** Establish stable source IDs, bounded reads,
archive capacity semantics, and active-view checkpoints. Integrate with accepted
session persistence, or clearly label temporary ephemeral validation. Source
preservation and failure behavior are gates before claiming the new architecture.

**Phase C — Unified transactional preparation.** Reuse the existing budget,
summary, truncation, and preview code behind one lifecycle; validate complete
requests and revision-safe commits. Keep tool execution and policy untouched.

**Phase D — Recovery and comparison.** Add source inspection, structured decision
coverage, paired reports, and optional candidate snapshot evaluation. Exercise
storage failures and incomplete runs without replaying effects.

**Phase E — Window progression.** Run the fixed synthetic and SWE matrices at
92K, 64K, then 32K. Classify regressions and change one major variable at a time.
Implement future representations only after evidence shows a limitation that
conventional mechanisms cannot adequately address.

For each comparison retain the baseline/candidate identity, exact configuration,
sample set, metric completeness, result differences, failure explanation, and
decision to retain, revise, or reject the change. Prioritize correctness, recovery,
and autonomous completion before token/time reductions. Do not trade away evidence
integrity for a favorable compression ratio.

## 12. Evidence Boundaries and Open Implementation Choices

The earlier Qwen 92K development probes reported a native request of 88,516 input
tokens, sequential fixture recovery at 37,453 input tokens, and emergency recovery
of a six-result burst at 7,305 input tokens. These are small exposed development
observations, not SWE resolved rates, general provider acceptance, or 64K/32K
verification. They motivated burst handling and estimate calibration work; they
do not prove long-task correctness. Raw artifacts were local, not repository
test fixtures, and availability must be checked before reproducing a claim.

Implementation review still must settle file format/durability, quotas and
retention, safe source-read exposure, event schemas, duplicate-call state
fingerprinting, snapshot frequency, recovery observation horizons, trial counts,
and numeric comparison tolerances. Resolve these at their actual boundaries;
do not introduce a generic database, plugin system, or semantic-memory framework
merely to leave room for later work.

Related documents: [components](01-components.md),
[execution](02-execution.md), [provider contract](../contracts/01-provider.md),
[session store](../contracts/04-session-store.md),
[agent benchmarks](../research/agent-benchmarks.md), and
[compiler research](../research/task-context-compiler.md).
