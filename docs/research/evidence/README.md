# Reviewed benchmark evidence

Selected original artifacts from local SWE-bench Lite pilots. These are small,
selected experiments, not a leaderboard score. The original runner/server logs
and generated evaluator output remain local under ignored paths; this directory
contains deliberately reviewed evidence only.

**Model configuration for the results below:** Qwen3.8-27B with **IQ3_M
quantization** and a configured **92K context window (94,208 tokens, input plus
output)**, served through Ollama. The Nexus selection `ollama-local/qwen3-27b`
maps to `qwen3.8-27b-iq3m-92k:latest`. These are not BF16 or 128K-context results;
account for these differences when comparing other evaluations.

| Run | Test date (UTC) | Agent wall time | Agent outcome | Official result | FAIL_TO_PASS | PASS_TO_PASS |
| --- | --- | ---: | --- | --- | --- | --- |
| [SymPy unbounded](nexus-qwen-sympy-20590-unbounded-20261024/report.json) | 2026-10-24 | Not recorded | Limit reached | Resolved | 1 passed | 21 passed |
| [Astropy](nexus-qwen-astropy-context-observation-20261024-retry/report.json) | 2026-10-24 | 13m 39s | Limit reached: model output limit | Resolved | 2 passed | 13 passed |
| [Django](nexus-qwen-django-context-observation-20261024/report.json) | 2026-10-24 | 34m 02s | Completed | Resolved | 3 passed | 19 passed |
| [SymPy summary-fix v2](nexus-qwen-sympy-20590-context-summaryfix-v2-20261025/report.json) | 2026-10-25 | 14m 11s | Completed | Resolved | 1 passed | 21 passed |
| Astropy context-retention run (artifacts local) | 2026-10-25 | 13m 13s | Completed | Resolved | Not recorded | Not recorded |

The unbounded SymPy row is the earlier exposed-task pilot. The summary-fix v2
row is another exposed-task replay, not blind benchmark evidence. Its official
tests passed, but the agent run triggered no context reduction, so it does not
validate the live summarization path.

The Astropy context-retention run is also an exposed-task replay, not blind
benchmark evidence. It completed one context reduction; the official SWE-bench
5.0.2 evaluator resolved the task with no infrastructure failures or evaluator
errors. Its generated report and run logs remain under the ignored benchmark
artifacts at
`/Volumes/Nayuki/Development/Rust/nexus-benchmark-work/run-astropy__astropy-12907-20261025T175536Z-b4200e8c/official-evaluation/context-state-v1-20261026/`.
This single result does not show that investigation repetition decreased or
that context-retention changes caused the repair. FAIL_TO_PASS and PASS_TO_PASS
counts were not captured in the aggregate evaluator report.

## Artifact mapping

Each run directory contains:

- `report.json`: the official evaluator's per-instance test report.
- `patch.diff`: the exact agent prediction applied by the official evaluator.

The Astropy and Django directories also contain:

- `result.json`: the original runner record, including agent outcome, duration,
  usage, and configured runner budgets.
- `evaluation-summary.json`: the official aggregate report, copied without
  content changes from its model/run-named report file.

The SymPy summary-fix v2 directory contains the same four artifacts. It used
SWE-bench evaluator 5.0.2; the report records the new `test_immutable` as
FAIL_TO_PASS and all 21 PASS_TO_PASS tests as successful. The archived
`patch.diff` was checked byte-for-byte against the submitted prediction.

Astropy's directory uses the successful evaluator retry's run ID; its
`result.json` describes the single actual agent execution, not a second model
attempt. The first evaluator process was interrupted by the host command timeout
and produced no completed report. The initial agent startup attempt lacked the
container feature and made no model requests.

Astropy and Django used SWE-bench evaluator 5.0.2 and the configured local model
`ollama-local/qwen3-27b`. Their agent runs did not trigger context reduction.
The original predictions include all submitted changes: Astropy's patch retains
temporary reproduction scripts, including an incorrect assertion in `repro2.py`.
An official resolved result does not validate these extra scripts or convert a
limit-reached agent run into normal completion. Do not execute archived patches
as trusted project code.

The summaries report no evaluator infrastructure failures or unstopped
containers. Retained task images are cache entries, not running containers.
Missing provenance fields must not be reconstructed from current repository or
model state as if they were historical measurements.

## Excluded contaminated attempt

The separate SWE-bench Verified attempt on `astropy__astropy-13398` is not part
of the result table and has no valid score. The agent found the upstream fix in
future commits exposed by inherited Git refs; the run was stopped before a patch
or official agent evaluation. Its gold-patch preflight is environment validation
only. Do not reuse this task as a blind sample. The run manifest and logs remain
outside the repository at
`/Volumes/Nayuki/Development/Rust/nexus-benchmark-work/pilot-astropy-13398-unbounded-20261024/`.

The two attempts on `sphinx-doc__sphinx-9461` are also excluded from the result
table: the first failed on provider HTTP 500, and the post-exposure retry ended
with an empty patch, which the official evaluator classified as empty-patch
(not an infrastructure failure). The issue was exposed to the model, so this
task is not a blind sample. Its attempt-2 report and finalized manifest are at
`/Volumes/Nayuki/Development/Rust/nexus-benchmark-work/pilot-sphinx-9461-isolated-20261024/`.

Subsequent exposed Sphinx replays on 2026-10-25 (not blind evaluation runs):

| Run | Agent wall time | Outcome |
| --- | ---: | --- |
| `run-sphinx-...T075030Z` | 20m 17s | Output limit; no patch |
| `run-sphinx-...T082835Z` | 1m 58s | Summary failed; no patch |
| `run-sphinx-...T083345Z` | 5m 43s | Summary unavailable; no patch |
| `run-sphinx-...T085510Z` | 60m 30s | Interrupted; no patch |
| `run-sphinx-...T150703Z` | 1h 04m 50s | Summary request hit output limit; no patch |

Durations are agent-run wall time, not evaluator time; these exposed replays
were not official resolved evaluations. The final replay successfully completed
multiple bounded summary chunks, then stopped when a later summary response hit
the configured 4,096-token summary output cap.

See [benchmark methodology and run history](../agent-benchmarks.md) and the
[reliability follow-up](../../tasks/09-agent-reliability.md).
