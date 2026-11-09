# Medium Task Benchmark Review

**Date:** 2026-11-09

**Code baseline:** `7c54a917694ad70ff01cbcab5edc6869f0922d03`, version `0.2.5-alpha`, plus the working-tree benchmark files present during the run.

**Scope:** A 30-task SWE-bench Verified campaign limited to the dataset's
`15 min - 1 hour` difficulty band. Five model workers ran concurrently with
dynamic refill; official scoring ran sequentially after all model attempts.
The model was OpenCode Go `deepseek-v4-1-flash`, with a temporary 120-second
provider first-response timeout and the standard 24-hour / 4,096-tool-call
budgets.

**Deliverable:** Findings and recommendations only. No product code, workflow,
permission, or deployment changes were made while preparing this review.

**Follow-up implementation (2026-11-09):** F1 and F3 have partial fixes,
exercised by the 30-task rerun below, including a bounded operation/source-batch journal, larger record limits,
merged-snapshot bytes/nodes reservations, a one-selected-session cache and explicit
backend state propagation. Offline regressions cover near-byte/node capacity,
pending outcomes across reopen, atomic source-batch tails, directory append-only
source writes and revisions 2/3/4 upgrading to 5. These are not a production or
cross-platform durability qualification. F2, F4,
F5, F6, and F7 remain open as described below; the original evidence is retained.

**Follow-up 30-task campaign (2026-11-09/10):** The same fixed medium selection,
model and five-worker shape were rerun on source baseline `39c6d24` plus the
then-working-tree fixes. All **30/30 gold-patch
preflights resolved**. Live attempts produced **29 completed, one failed, and 30
nonempty patches**. Official prediction evaluation produced **23 resolved, five
unresolved, and two ambiguous failures**; there were no empty patches or missing
predictions. Cumulative agent time was **26,562.283 seconds**, with **1,491 tool
calls** and **46,071,350 input / 759,602 output tokens** (the failed attempt's
final usage was incomplete/null).

The failed live attempt was `sphinx-doc__sphinx-10673`: a delayed worker returned
`BackendState::Unavailable`, and the runtime stopped with `ToolFailure` after
capturing a 7,817-byte patch. That patch was still officially resolved, so the
agent terminal state and the official result must remain separate. The five
unresolved tasks were Astropy 13033, Django 16877 and 16502, scikit-learn 25747
and Sphinx 7985. The ambiguous results were Sphinx 10614 (`no_tests_collected`)
and Pylint 4661 (`missing_module`); neither is counted as resolved.

This rerun is not a controlled A/B. The first medium run produced 20 resolved,
six unresolved, two empty patches, one preflight failure and one ambiguous
result; the rerun had no empty patch or preflight failure. Runtime, harness,
date and host conditions differed, so the higher count is not proof that a
specific change caused the improvement. F4-F7 remain open, and 23/30 remains a
selected diagnostic sample rather than a blind or representative score. See the
[merged campaign summary](../../../nexus-benchmark-work/verified-medium30-v2/campaign-summary.json).

Operational hardening exercised by the rerun: hosted-endpoint `prepare` now
accepts the explicit hosted-inference opt-in while still requiring
`--prepare-only`; preflight `git status` uses a 300-second bounded wait; and
Docker gateway preparation/cleanup waits are 120/15 seconds. These avoid
misclassifying slow Docker Desktop operations as task failures, but they do not
establish a safe concurrency level or production durability.

An offline debug-profile measurement using a fake provider and tool with the
real directory store took 1.52, 5.80, 33.74 and 111.98 seconds for 16, 64,
256 and 512 calls respectively on one local macOS run. These are single samples,
not production benchmarks. After the source journal and selected-session cache,
the same opt-in debug-profile measurement on this local macOS machine took
**0.317, 1.180, 7.058 and 17.453 seconds** respectively (19.8, 18.4, 27.6 and
34.1 ms/call). All four runs completed with saved source persistence. Compilation
(5.12 seconds) and setup/cleanup are excluded from these samples. These are
single warm-development samples, not a controlled production speedup claim;
runtime diagnostics and filesystem syncs remain inside the measured interval.
An intermediate source-journal-only sample still took 110.878 seconds at 512
calls, motivating the bounded selected-session cache rather than claiming that
append-only disk writes alone solved full-runtime scaling.

Remaining limits: cold loads still decode/replay the bounded snapshot/journal;
checkpoint publication still rewrites the full merged body; identity matching
uses scans, and pending-outcome accounting and cold replay can be quadratic.
The bytes/nodes budget reserves accepted records plus pending operation outcomes
against the current checkpoint, not arbitrary future checkpoint growth or tool
result/source content. Independent journal capacity may stop a run earlier.
There is no guarantee that 4,096 calls with arbitrary valid content fit the
bounded store. No Linux filesystem measurements or power-loss qualification were
performed in this follow-up.
The opt-in measurement is `directory_store_full_runtime_scaling` in
`crates/nexus-runtime/tests/directory_store_measurement.rs`.

Local follow-up validation (sequential Cargo invocations, locked/offline):
`cargo test -p nexus-session -p nexus-runtime --locked --offline` passed 41
session and 388 runtime tests; three existing/default ignored tests were skipped.
The directory-store measurement was run separately with `--ignored --nocapture`.
`cargo clippy -p nexus-session -p nexus-runtime --all-targets --locked --offline
-- -D warnings`, `cargo fmt --all --check` and `git diff --check` also passed.
Other packages and the complete optional/ignored suite were not rerun for this
session-store follow-up.

## 1. Outcome summary

The campaign processed all 30 tasks; one failed before model inference and one
evaluation ended ambiguously.
Five workers used dynamic refill rather than fixed batches. All 30 gold-patch
preflights had resolved before the model runs.

| Official outcome | Tasks |
| --- | ---: |
| Resolved | **20** |
| Unresolved | 6 |
| Empty patch | 2 |
| No prediction (preflight failure) | 1 |
| Ambiguous failure | 1 |

Cumulative agent time was **32,491.76 seconds (about 9h 02m)**, with **1,649**
tool calls and **zero provider retries**. The raw resolved rate is **20/30
(66.7%)**; excluding the three operational failures described below gives
**20/27 (74.1%)**. This retrospective subset is not an unbiased capability
estimate. These are selected diagnostic tasks; blind status was not established
and they do not yield a representative benchmark score.

The important result is not the percentage alone. The run exposed two
long-task runtime limits, one preflight timeout, and several genuine patch
correctness failures. The failures belong to different layers and should not
be collapsed into a single model-quality number.

## 2. Findings at a glance

| ID | Priority | Finding | Classification | Follow-up |
| --- | --- | --- | --- | --- |
| F1 | P1 | A single persistent session cannot record more than 256 tool intents/outcomes, below the advertised 4,096-call budget | Deterministic runtime cap | No capacity failure in the 30-task rerun; bounded journal and merged-snapshot reservations added. Arbitrary 4,096-call content is not guaranteed to fit. |
| F2 | P2 | A five-worker preflight timed out on `docker exec ... git status` | Operational/concurrency | No preflight failure in the rerun after a 300-second bounded status wait; resource contention was not separately measured. |
| F3 | P2 | The agent reported a search deadline followed by an unavailable/poisoned worker | Operational/tool behavior | The new fail-closed backend path triggered once; the run stopped with `ToolFailure` and its patch was officially resolved. The original search behavior is not proven eliminated. |
| F4 | P2 | The safe worker count for medium tasks on this Docker VM is unverified | Operational/configuration | Still unverified; five workers were used again without a controlled concurrency ladder. |
| F5 | P2 | Six official unresolved results and one ambiguous result show incomplete fixes or regressions | Agent/model correctness | Rerun: five unresolved and two ambiguous official results. |
| F6 | P3 | The pool summary reports `null`, `empty-patch`, and `ambiguous-failure` without a unified distinction between "not run" and "failed" | Reporting | Still open; ambiguous evaluation stopped the first batch and required a second batch plus a merged summary. |
| F7 | P3 | Bulk image preparation may require significant storage; measure before expansion | Operational/storage | Still open; 30 adapters were rebuilt twice during preparation. |

## 3. Detailed findings

### F1 — P1: Session operation history is capped at 256 records

**Evidence:** `astropy__astropy-13033` finished with 256 recorded tool calls and
reported:

```text
StorageFailure
session operation history is full; restart in a new session, then delete an old saved session with :session delete <id>
```

The batch recorded no patch, and the official evaluation was `empty-patch`.
The run log is preserved at
[`astropy__astropy-13033.run.log`](../../../nexus-benchmark-work/verified-medium30-v1/pool-20261109/astropy__astropy-13033.run.log).

The cap is explicit in
[`crates/nexus-core/src/store.rs`](../../crates/nexus-core/src/store.rs#L31):
`MAX_INTENT_RECORDS` and `MAX_OUTCOME_RECORDS` are both **256**. The store
rejects further intent/outcome records in
[`crates/nexus-session/src/lib.rs`](../../crates/nexus-session/src/lib.rs#L943).
A tool call records both an intent and an outcome, so a single persistent
session stops at about 256 calls even though the benchmark and runtime budgets
allow 4,096.

**Impact:** Medium and long tasks can be forced to fail for a storage-capacity
reason unrelated to the model's proposed fix. The failure is deterministic, not
a transient provider or concurrency issue. A lower concurrency setting will not
fix it.

**Recommendation:** Choose one explicit policy:

- use an ephemeral benchmark/session store without this history bound;
- rotate to a fresh session at a documented boundary while preserving the run;
- raise the per-session record bound to cover the accepted tool budget; or
- lower the advertised benchmark tool budget so it cannot exceed the store
  capacity.

Do not raise the bound without evaluating file size, compaction, write latency,
and recovery behavior. Add a regression that exceeds 256 calls and verifies
that either the run succeeds or the task fails before model work with an
explicit capacity diagnostic. The current message suggests restarting in a new
session, which a single benchmark attempt cannot do.

### F2 — P2: Preflight Docker command timed out under five workers

**Evidence:** `pylint-dev__pylint-6386` never entered the model. The launcher
recorded:

```text
benchmark launcher: Command '['docker', 'exec', '-w', '/workspace', ... 'git', 'status', '--porcelain']' timed out after 30 seconds
```

The log is at
[`pylint-dev__pylint-6386.run.log`](../../../nexus-benchmark-work/verified-medium30-v1/pool-20261109/pylint-dev__pylint-6386.run.log).
Docker I/O, CPU, or daemon contention while five workers were active is a
possible explanation, but no resource measurements or controlled comparison
establish the cause.

The timeout is set in
[`tools/benchmarks/swebench.py`](../../tools/benchmarks/swebench.py#L281).

**Impact:** A valid task is recorded as `preparation-failed` with no
prediction and no official score. This is an infrastructure failure, not a
model result, and it lowers the observable resolved count.

**Recommendation:** Keep model-attempt retries disabled, but allow a bounded
preflight-only retry or a longer preflight deadline before inference starts.
A preflight retry occurs before inference, but should be limited to operations
whose effects and cleanup have been verified; it differs from a model retry.
Consider lower concurrency and capture resource measurements during preflight
to investigate contention; a safe worker count has not been established.

### F3 — P2: Broad search exhausted the tool deadline and poisoned the worker

**Evidence:** `astropy__astropy-7671` completed the model run but produced no
patch after 4,350 seconds and 86 tool calls. Its final message states that a
broad filesystem search exceeded the execution deadline, the container worker
became unavailable/poisoned, and every subsequent `host_exec`, `host_read`,
`host_write`, `host_patch`, and `host_search` failed. The proposed patch was
never applied.

The run log is at
[`astropy__astropy-7671.run.log`](../../../nexus-benchmark-work/verified-medium30-v1/pool-20261109/astropy__astropy-7671.run.log).

**Impact:** The task is recorded as `empty-patch`, which understates the
model's investigation and attributes an infrastructure/tool failure to the
agent. The model reported a broad command as the trigger. Any contribution from
concurrent resource pressure remains unverified.

**Recommendation:** Keep the fail-closed worker quarantine, but make the
failure path more legible and less likely:

- direct agents toward bounded `search`/`list` tools rather than unrestricted
  filesystem commands;
- include the tool deadline and worker state in the final task outcome;
- distinguish `tool-deadline` from `model-no-patch` in benchmark accounting;
- consider a separate bounded-search tool or a lower per-task search budget.

Do not automatically retry a poisoned worker; uncertain effects remain
uncertain. A fresh benchmark attempt would be a new run, not a recovery of the
old one.

### F4 — P2: Safe concurrency for long medium tasks is unverified

**Evidence:** The 30-task pool used five workers on a Docker Desktop VM with
**10 vCPUs and 8,321,712,128 bytes (about 8 GiB) of memory**. Failures F2 and
F3 occurred during this run. The longest attempts were:

- `django__django-16502`: 5,339 seconds;
- `astropy__astropy-13033`: 4,571 seconds;
- `astropy__astropy-7671`: 4,350 seconds;
- `pydata__xarray-7393`: 2,456 seconds;
- `sphinx-doc__sphinx-7985`: 1,838 seconds.

The simple 30-task campaign used five workers successfully, but its tasks were
shorter and lighter. Medium tasks combine longer model loops, heavier test
suites, and more tool calls; the same concurrency level is not equivalent.

**Impact:** Concurrency can contaminate the measurement with preflight/tool
timeouts and can obscure the model's actual repair rate.

**Recommendation:** Consider a conservative lower worker count for medium tasks until the runtime
limits are fixed and the resource envelope is measured; 3-4 workers have not
been validated as safe. Run a controlled
concurrency ladder (1, 3, 4, 5) with `docker stats` sampling and compare
failures, wall time, and resolved count. Keep official scoring sequential or
at most one evaluator worker.

### F5 — P2: Six unresolved tasks and one ambiguous evaluation need investigation

The six unresolved reports have `infra_failure=false`. That flag alone does
not prove the cause; the ambiguous evaluator failure is not a clean unresolved
result and should be investigated separately.

| Task | Official failure |
| --- | --- |
| `django__django-16877` | All four FAIL_TO_PASS tests failed (`EscapeseqTests`). |
| `django__django-16950` | One PASS_TO_PASS test failed: `#24377 - Inlines with a model field default should ignore that default`. This is a regression. |
| `django__django-16502` | `test_no_body_returned_for_head_requests` failed. |
| `scikit-learn__scikit-learn-25747` | `test_set_output_pandas_keep_index` failed. |
| `sphinx-doc__sphinx-7985` | `test_defaults` and `test_anchors_ignored` failed. |
| `sphinx-doc__sphinx-10614` | `test_inheritance_diagram_svg_html` failed. |

`pylint-dev__pylint-4661` is additionally recorded as
`ambiguous-failure`: `tests/lint/unittest_lint.py::test_pylint_home` failed,
with no PASS_TO_PASS tests and `infra_failure=false`. It has a patch and a
completed model run, but the official report is not a clean resolved result.

**Impact:** The medium suite is producing real incomplete fixes and at least
one regression. Local tests passing is not a sufficient acceptance signal;
the agent needs better hidden-interface and regression coverage.

**Recommendation:** Treat these as model/agent reliability evidence. In
addition to resolved count, track:

- FAIL_TO_PASS failures;
- PASS_TO_PASS regressions;
- empty patches;
- interface/import failures;
- tool-call count and time to convergence.

Do not rerun these tasks merely to obtain a different result without a
predeclared reason and a fresh attempt record.

### F6 — P3: Benchmark accounting conflates "not run", "empty", and "ambiguous"

The pool summary currently reports:

```json
{
  "resolved": 20,
  "unresolved": 6,
  "empty-patch": 2,
  "null": 1,
  "ambiguous-failure": 1
}
```

`pylint-dev__pylint-6386` has no model prediction because preflight failed, but
its summary outcome is `null`; `pylint-dev__pylint-4661` has an ambiguous
official result. These are materially different states.

**Recommendation:** Normalize task states into at least:

- `resolved`;
- `unresolved` (completed test failure);
- `empty-patch`;
- `preflight-failed` / `not-run`;
- `ambiguous-failure`;
- `tool-failure` / `worker-poisoned`.

Include the reason and evidence path in the summary. Do not count `not-run` or
`tool-failure` as model correctness failures without an explicit caveat.

### F7 — P3: Bulk image preparation is disk-intensive

The environment currently has:

- about **141.8 GB** of Docker images;
- about **65.6 GB** of build cache;
- about **10 GB** of stopped containers;
- about **414 GB** free on the benchmark volume.

Adapter layers are small (often about 1.6 MB unique), but each task requires
its own official base environment (typically 2.7-7 GB logical size). Docker image layers can be shared, so these figures do not establish the
incremental space required for 155 tasks. Measure actual growth before a larger
campaign; rolling preparation may reduce peak usage.

Do not run `docker system prune` or delete images automatically. Reclaiming
old pilot images, stopped containers, and build cache requires explicit
authorization and should be recorded as an environment change.

## 4. What this run does not establish

- It is not a blind sample, a leaderboard score, or a general success rate.
- The 20/30 result is for selected `15 min - 1 hour` tasks, not all Verified
  tasks.
- The 120-second provider timeout was a temporary benchmark setting, not the
  product default.
- A five-worker run does not establish the safe concurrency ceiling for medium
  tasks. Lower concurrency is a precaution, not a validated safe setting.
- A local test pass is not proof of hidden-test compatibility; the unresolved
  Django and Sphinx cases demonstrate this.
- The run did not establish production readiness, crash recovery, sandbox
  security, or untrusted-plugin isolation.

## 5. Recommended follow-up order

1. Reconcile the 256-record session cap with the accepted benchmark/runtime
   tool budget; choose ephemeral storage, session rotation, or a higher bound
   with resource analysis.
2. Add a bounded preflight retry or longer preflight deadline, and separate
   preflight failures from model results.
3. Improve tool-timeout and worker-poison diagnostics; distinguish
   infrastructure/tool outcomes from empty model patches.
4. Run controlled lower-concurrency comparisons with resource sampling before claiming a
   concurrency or resolved-rate result.
5. Add hidden-interface and regression-focused validation to the agent
   workflow; review the six unresolved and one ambiguous case as correctness
   evidence.
6. For a larger task campaign, implement rolling image preparation and
   authorized cleanup rather than preparing all images at once.

## 6. Evidence index

- [Campaign summary](../../../nexus-benchmark-work/verified-medium30-v1/pool-20261109/summary.json)
- [Medium suite definition](../../tools/benchmarks/swe_suite_verified_medium30.json)
- [astropy-13033 run log](../../../nexus-benchmark-work/verified-medium30-v1/pool-20261109/astropy__astropy-13033.run.log)
- [astropy-7671 run log](../../../nexus-benchmark-work/verified-medium30-v1/pool-20261109/astropy__astropy-7671.run.log)
- [pylint-6386 run log](../../../nexus-benchmark-work/verified-medium30-v1/pool-20261109/pylint-dev__pylint-6386.run.log)
- [pylint-4661 evaluation log](../../../nexus-benchmark-work/verified-medium30-v1/pool-20261109/pylint-dev__pylint-4661.eval.log)
- [Store record bounds](../../crates/nexus-core/src/store.rs#L31)
- [Preflight timeout](../../tools/benchmarks/swebench.py#L281)
