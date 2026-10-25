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

### Near-term delivery boundary

Deliver three small capabilities before expanding the architecture:

- **Trustworthy observation:** correlate requests and usage, report committed
  reductions and missing observations honestly, and retain caller-side JSONL.
- **Simple source/view separation:** preserve accepted original records through
  the existing session-storage boundary while keeping a replaceable active view.
- **One request-preparation path:** reuse existing budget, summary, truncation,
  and preview functions; validate a candidate before replacing active state.

Do not make the complete evidence model or all eight benchmark metrics a gate
for these capabilities. General artifact identities/provenance, paginated
retrieval, independent view checkpoints, multi-revision concurrency control,
automatic redundant-work analysis, and intermediate patch evaluation are
deferred until a demonstrated workload needs them. Preserve correctness
invariants now without implementing their most general possible representation.

## 2. Current Implementation and Gaps

Baseline inspected: context fixes through `06b5bea`. Later implementation must
recheck these locations rather than assume this snapshot remains current.

| Existing component | Reusable behavior | Remaining distinction |
| --- | --- | --- |
| `crates/nexus-context/src/lib.rs` | Pure estimation/calibration, paired-exchange selection, bounded history projection, summary validation, UTF-8-safe tool-output previews | Selection and derived representations are not an independent source-history store |
| `crates/nexus-runtime/src/runtime/context_compaction.rs` | Request construction, isolated tool-free summarization, validation/commit, generation headroom checks, emergency previews | Summary failure preserves the previous conversation and terminates; full source/view separation remains unimplemented |
| `crates/nexus-runtime/src/runtime.rs` | Agent loop, accepted results, cancellation, usage, active conversation, bounded session cache | Both hard-limit projection and compaction replace active conversation; the cache can evict records |
| `crates/nexus-session/src/lib.rs` | Revision-2 bounded JSON snapshot, ordered source history, and explicit recovery assembler; private Unix directory/file, atomic replacement, and OS single-writer lock | Host runtime construction is not wired; active-view and source-history lifecycle are not coordinated |
| `nexus-core` request/result types | Call identities, status/effect/evidence, provider continuation, settings | Reuse domain facts; do not add algorithm-specific memory types to the core |
| `tools/benchmarks/run_nexus_swe_task.py` | Tool/usage/reduction logs, result metadata, prediction export, evaluator separation | Structured event retention, complete compaction classification, and comparable derived metrics are not yet present |

The output previews preserve their source records, but the complete existing
history pipeline is **not** an append-only archive: safely removable exchanges
can leave active conversation and the session cache. Do not describe current
compaction as preserving every original interaction indefinitely.

Existing safeguards include unknown windows staying unknown, explicit settings
overriding discovery, provider-usage calibration, and disabling incompatible
history rewriting with opaque continuations. Retain them.

### Session recovery boundary at this baseline

The runtime's `State.history` is an in-memory, bounded cache of completed
exchanges and adopted derived memory. It can supply context to a later run in
the same session and process; it is not durable original history. Failed or
cancelled runs do not commit ordinary partial conversation. Uncertain execution
evidence receives special retention, with fail-closed behavior when it cannot
be carried forward safely.

The runtime has an opt-in `try_new_with_session_store` constructor that
initializes the selected session, appends accepted user input and complete
original context items in batches, durably writes tool intents before dispatch,
and records observed operation outcomes afterward. Host applications do not
yet use it; explicit list/restore commands remain unwired. The file-backed
implementation has versioned source records for complete original
`ModelContextItem` values, including tool content and execution uncertainty,
alongside the existing checkpoint and operation records. However, the runtime
does not write or load these records; `ListSessions` and `RestoreSession` are
still rejected, and terminal records report `PersistenceState::Ephemeral`.
Restarting the runtime therefore cannot restore its sessions. The source port
also does not yet preserve all provider continuation state or coordinate the
active-view lifecycle. Benchmark logs are inspection artifacts, not a supported
session recovery format.

### Observed summary limitation

The exposed `pytest-dev__pytest-10051` summary-fix-v2 replay used
`ollama-local/qwen3-27b`, a 94,208-token reference-only window and a 65% trigger.
Two LLM summaries succeeded; the third ended at the 4,096-token output limit
and produced 14,917 bytes, exceeding the 8,192-byte memory ceiling. The run
stopped without a patch and preserved the previous conversation. These are
debugging observations, not a blind benchmark or validated provider window.

Successful summaries also left large exchanges pinned by `effect=Unknown`.
Summary completion, reduced request occupancy, and reaching the target are
separate outcomes. Increasing generation limits alone does not address either
the summary byte limit or the retained-history growth.

### Archived-history summary increment

For persistent runs whose accepted sources have all been saved, summarization
may replace old complete exchanges even when their outcomes have unknown effects.
The current task and the newest three complete tool-call groups remain verbatim
in the summary candidate; parallel calls and their results count as one group.
Earlier ordinary discussion and verbose reasoning go into rolling, untrusted
LLM memory rather than permanently occupying the active request.

Uncertain execution retains a separate deterministic context projection with
original identities, arguments, status/effect/evidence, truncation metadata, a
bounded output preview, and a no-blind-retry warning. These records are not LLM
interpretations or tool authorization; full original records remain in the
session store. Later summary passes and ordinary eviction must not discard the
unresolved projections. This is not a general semantic evidence classifier.

The planner budgets replacement memory as well as retained context. Summary
acceptance checks the complete candidate including generation reserve before
commit. The target remains a soft goal: protected task/recent exchanges and
execution facts may exceed it. Emergency output previews can still shorten
recent tool bodies if needed to preserve generation headroom; they do not
rewrite source history. If protected context still cannot fit, fail explicitly.

Ephemeral runs, incomplete source saves, and plain truncation retain the previous
certainty-based removal rules. Failed, invalid, or cancelled summaries never
partially replace the old active view. Diagnostics report planned removals,
estimated occupancy, target, window, and output reserve; these are not tokenizer
measurements or evidence of benchmark repair quality.

### OpenCode V2 reference: separate storage from model context

Official documentation reviewed on 2026-10-25 describes these boundaries:

| Concern | Documented OpenCode V2 behavior | Relevance to Nexus |
| --- | --- | --- |
| Session ownership | A background service owns sessions; TUI/web clients connect to it | A frontend's lifetime need not determine stored history's lifetime; a background service is not required for the Nexus storage change |
| Local persistence | The default database is `~/.local/share/opencode/opencode.db`, overridable with `OPENCODE_DB` | Persist outside the project; do not copy another application's path or require its database choice |
| Active context | Summary compaction replaces older context while keeping approximately 15,000 recent tokens by default; earlier messages remain stored | Archive retention and request selection must have different lifecycles |
| Native compaction | Supported OpenAI Responses deployments can return opaque encrypted checkpoints with provider/model restrictions | Keep provider-independent text summaries; do not assume native compression is available for Qwen |
| File rollback | Best-effort snapshots use a separate internal Git object database | Session restoration is not filesystem rollback or reversal of external effects |

A drop from roughly 200K tokens to roughly 20K is compatible with a summary
plus recent context and fixed instructions. It is not proof of lossless
compression, a universal 50% trigger, or the mechanism used by a particular
session. The documentation does not establish transaction boundaries, per-write
crash durability, or exactly-once tool execution; those require implementation
inspection and tests. Experimental background summaries held only in memory are
also distinct from committed session history.

References: [service and storage](https://opencode.ai/v2/docs/troubleshooting/),
[session API](https://opencode.ai/v2/docs/api/),
[compaction](https://opencode.ai/v2/docs/compaction/), and
[file snapshots](https://opencode.ai/v2/docs/snapshots/).

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

Initially, session-scoped sequence positions plus existing run/call identities
can identify source records; a new global identity or revision subsystem is not
required. A sequential file-backed history and a bounded active context are
sufficient. Random-access indexes, paging, lazy materialization, and independent
active-view checkpoints are later optimizations, not prerequisites. Until then,
use explicit supported history/materialization limits rather than promising
unbounded history loading.

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
derived summaries, preview choices, and any pending candidate.
An active view may omit source records without destroying them.

Start with concrete fields and functions, not a general artifact container or
strategy interface. A summary needs its text and covered source boundary;
arbitrary provenance graphs and composable derived representations are deferred.

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

Maintain these logical distinctions; the table is not a requirement to create
four generic record types in the initial implementation:

| Information | Meaning |
| --- | --- |
| Original record | What the host accepted or observed, with stable identity |
| Derived artifact | Initially summary text plus its covered source boundary; independent identity, source revisions, and richer derivation metadata are deferred |
| Active view | Ordered source references and derived replacements used to construct a request |
| Candidate | Uncommitted replacement built from the current source/view state |

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
capture current source/view state -> select replaceable ranges
  -> deterministic candidate or tool-free summary call
  -> build full candidate request -> validate invariants and budgets
  -> recheck state ownership/cancellation/deadline -> replace active view atomically
```

Here, transactional means that an invalid or abandoned candidate cannot partially
replace active state. It does not require a database transaction, MVCC, or
multiple checkpoint generations. Preserve the current single-active-run
ownership model; add an explicit revision check only where state can actually
change while a candidate is being prepared.

The summary input and output require their own budgets; summarization cannot
require dispatching an already oversized request. Summary calls consume model
turns, tokens, and time, including failed attempts. They must be identifiable
separately from normal task calls.

Prefer safe exchange selection and a bounded summary when configured. Failed or
incomplete summaries preserve the previous conversation and terminate; do not
fall back to deleting the summary's source history. Old output previews and
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
small file-backed implementation, not a separate context database. Session/runtime
owns durability, tool-intent recording, and restoration; Context Management
must not introduce a second journal or tool-recovery protocol. Keep source
history distinct from the active view, but defer independent view checkpoints
and on-demand range loading until simple bounded loading is insufficient.

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

### Confirmed implementation direction (2026-10-25)

The project owner selected local persistent history for this increment, rather
than a runtime-only in-memory archive. The first version uses current-user-only
filesystem access without application-level encryption. Require private
directories and Unix files created with mode `0600`; other platforms need an
explicit equivalent protection boundary, not a claim that Unix modes apply
everywhere. Conversation, reasoning, arguments, results, and snapshots can be
sensitive even when configured credentials are excluded.

Keep the implementation and dependency footprint minimal. Prefer the standard
library, existing workspace dependencies, and concrete functions over new
frameworks or generic storage abstractions. Start with small file-backed records;
do not introduce a database engine, ORM, event-sourcing framework, vector store,
or encryption library without a demonstrated requirement. Any new dependency
must justify why existing capabilities are insufficient, including its transitive
dependency and maintenance cost. Minimal size must not weaken validation,
private access, durability, or uncertain-execution guarantees.

Use the existing session-storage responsibility for original records and
bounded inspection/recovery. Validate stored inputs on load. Specify format
revision, write/flush and atomic-publication protocol, writer conflicts, quotas,
and retention before claiming crash recovery. No storage failure may silently
enable ephemeral operation or permit compaction to discard unarchived sources.

The intended active request is current instructions and task/corrections, an
adopted summary with a covered source boundary, unresolved execution facts, and
recent complete exchanges. Once original sources are recoverable, represent old
uncertain execution with accurate call identities, status/effect/evidence,
arguments or recoverable references, and a no-blind-retry constraint. Do not pin
unrelated long reasoning or repeated file bodies merely because they share an
exchange with an uncertain call. Preserve protocol-required groups and reasoning
for retained native calls; unsupported rewriting still fails explicitly. The
archived-history increment above implements compact text projections for removed
groups while retaining their complete original records in storage.

Summary reliability must distinguish generation budget, requested text size,
and the hard memory-byte ceiling. A proposed repair policy allows at most one
additional summary attempt for output-limit or oversized-text failures, using
the original sources rather than continuing an incomplete summary. Both attempts
consume normal turn/time/usage budgets and recheck cancellation and deadlines.
Protocol failures are not length retries. Never truncate text into an accepted
summary; exhausted attempts leave the old view and memory unchanged.

Acceptance requires repeated compaction with recoverable sources, exact
uncertainty preservation, failure/cancellation without partial publication, and
measured request-size changes. A summary success or an official task resolution
alone does not establish these properties. Keep provider usage distinct from
runtime estimates and do not pursue a fixed 5% occupancy at the expense of
protected content. Persistence and execution changes span multiple components;
implement and verify them incrementally before another exposed Pytest replay.

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

This is the eventual reproducibility envelope, not a requirement to automate
every field before useful comparisons. Initially pin the task/base, model,
runtime window and policy, execution limits, and candidate/baseline identity;
declare unavailable provenance explicitly rather than building a general
experiment-management platform.

Events carry schema version, run identity, sequence, monotonic elapsed time,
model invocation ID/purpose (`task` or `summary`), tool call IDs, compaction ID,
and relevant source/view revision. Record budget decisions, estimates, final
usage, summary attempts/results, committed reductions, output previews,
tool outcomes, patch snapshots, and termination classification.

For initial delivery, use existing run/turn/call identities and observation
sequence numbers. Separate compaction IDs, source/view revisions, detailed
decision traces, and patch-snapshot events are only needed when their consumers
are implemented; the paragraph above describes eventual coverage.

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

### Initial measurement subset

The eight definitions above remain the long-term evaluation vocabulary, not an
implementation checklist that blocks source/view separation. Initially compare
final evaluator outcome, normal completion, reported token cost, peak input,
and observed committed reductions, with explicit coverage. Inspect recovery
manually on a small sample and label accepted-response observations as such.
Automatic progress attribution, environment-aware redundant-call analysis,
intermediate patch snapshots/evaluation, and cross-run statistical reporting are
deferred. Unknown advanced metrics must remain unknown, not estimated from tool
counts or patch existence.

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

Initially compare the measurement subset at the tiers under investigation;
complete all-eight-metric matrices are a later evaluation capability, not a
routine development gate. Watch for repeated compression
with no progress and smaller windows increasing total tokens through rework.
Use repeated trials when making model-quality claims, show paired task outcomes,
and disclose small-sample uncertainty rather than inventing significance.

Safety gates are non-negotiable at every tier. The first 32K milestone is safe,
diagnosable degradation; the final goal is useful repair capability approaching
the 92K baseline with acceptable cost. Define numeric quality/cost tolerances
before confirmatory runs after measuring an initial baseline; do not retroactively
choose thresholds to label a regression successful.

## 11. Development Workflow and Delivery Phases

Use risk-proportional validation. Ordinary fixes require affected deterministic
regressions; changes to selection, compression policy, or request preparation
also warrant bounded synthetic probes. Use paired fixed-task live runs and
independent final-patch evaluation when assessing repair quality or cost. Do not
require all eight metrics, all three windows, or live SWE runs for every change.
Routine documentation changes do not trigger application tests or live
benchmarks. Follow [testing guidance](../testing.md) and never run concurrent
Cargo commands against the same target directory.

**Phase A — Measurement foundation (partially implemented).** Runtime events now
expose model request start/protocol-validation metadata and a typed method for
committed context reductions. The SWE caller writes per-run manifest/event JSONL
and derives offline metrics; event capture omits assistant text, tool bodies,
progress previews, and approval argument previews. Unit fixtures cover
sanitization, usage accounting, sequence gaps, completion, reductions, and
recovery association. Preserve existing prediction/evaluator separation.

Finish this phase as a minimal trustworthy observer, not a complete evaluation
platform. Protocol validation and summary adoption are separate observations;
Final usage is reconciled by request turn, with later counters replacing earlier
ones. Telemetry-write failures disable further writes without interrupting the
run, while SSE interruption (including an incomplete HTTP read) produces
incomplete artifacts and never treats an in-progress workspace patch as a final
result. Runner terminal observations and collection errors are persisted in
`result.json` and used by both online and offline metric derivation. Offline
replay tolerates an incomplete final JSONL record only when a telemetry error
was recorded. Committed reductions use the
bounded control channel; control-capacity exhaustion is a resource-limit failure
and makes reduction coverage incomplete. Avoid expanding frontend presentation
or public event fields solely to complete speculative metrics.

The initial implementation is deliberately incomplete: it counts committed
reduction events rather than every attempted decision; an event's projected
tool-output value is an occurrence count over requests, not unique source calls;
recovery means a subsequent provider batch passed protocol validation, not that
a summary was adopted or task progress was made. First-valid-patch evaluation, repeated-call review, automated sweeps
of the synthetic stress lane, a caller-driven live-provider pressure harness, and
cross-run comparison reports remain future Phase A work. A deterministic runtime
regression now checks oversized parallel results at the exact 94,208, 65,536,
and 32,768 token budget tiers; it does not establish provider-reported occupancy
or task success. The initial observer makes later architecture changes
measurable; it does not yet implement all eight metrics or the complete
evaluation system.

Run the offline analyzer on one completed benchmark artifact set with:

```sh
python3 tools/benchmarks/context_metrics.py \
  --events /path/to/run/events.jsonl \
  --manifest /path/to/run/manifest.json \
  --result /path/to/run/result.json \
  --output /path/to/run/metrics.json
```

An optional `--evaluation` takes the normalized result from
`run_nexus_swe_task.py summarize`; it never calls the evaluator or model.

**Phase B — Simple source/view separation.** Preserve original accepted history
using existing identities and ordered source positions, a replaceable active
view, and explicit capacity limits. Integrate with accepted session persistence,
or clearly label temporary ephemeral validation. Do not require a generic
History Store API, indexed range retrieval, independent view checkpoints, or
general derived artifacts. Source preservation and failure behavior are gates
before claiming the new architecture.

**Phase C — Unified transactional preparation.** Reuse the existing budget,
summary, truncation, and preview code behind one lifecycle; validate complete
requests and atomically replace active state under existing ownership. Add
revision machinery only for demonstrated interleaving. Keep tool execution and
policy untouched.

**Deferred extensions — Evidence and advanced evaluation.** After the small
source/view and preparation path demonstrates a concrete limitation, consider
richer source inspection, artifact provenance, paging/checkpoint optimization,
progress attribution, redundant-call review, and optional patch snapshot
evaluation. These are independent extensions, not prerequisites for window
validation. Storage-failure and interruption safety remain required whenever
the relevant persistence or lifecycle behavior is implemented.

**Window progression — Incremental validation.** Run fixed synthetic probes and
small paired SWE samples at 92K, 64K, then 32K as warranted; this can proceed
without the deferred extensions. Classify regressions and change one major
variable at a time.
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
retention, minimal source access, event semantics, and numeric comparison
tolerances when needed. Duplicate-call state fingerprinting, snapshot frequency,
advanced recovery horizons, and broader trial/reporting automation are deferred
choices, not current delivery blockers. Resolve these at their actual boundaries;
do not introduce a generic database, plugin system, or semantic-memory framework
merely to leave room for later work.

Related documents: [components](01-components.md),
[execution](02-execution.md), [provider contract](../contracts/01-provider.md),
[session store](../contracts/04-session-store.md),
[agent benchmarks](../research/agent-benchmarks.md), and
[compiler research](../research/task-context-compiler.md).
