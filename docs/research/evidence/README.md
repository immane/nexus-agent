# Reviewed benchmark evidence

Selected original artifacts from the 2026-10-24 local SWE-bench Lite pilots.
These are small, selected experiments, not a leaderboard score. The original
runner/server logs and generated evaluator output remain local under ignored
paths; this directory contains deliberately reviewed evidence only.

| Run | Agent outcome | Official result | FAIL_TO_PASS | PASS_TO_PASS |
| --- | --- | --- | --- | --- |
| [SymPy unbounded](nexus-qwen-sympy-20590-unbounded-20261024/report.json) | Limit reached | Resolved | 1 passed | 21 passed |
| [Astropy](nexus-qwen-astropy-context-observation-20261024-retry/report.json) | Limit reached: model output limit | Resolved | 2 passed | 13 passed |
| [Django](nexus-qwen-django-context-observation-20261024/report.json) | Completed | Resolved | 3 passed | 19 passed |

The archived SymPy run is the earlier unbounded pilot, not the later context-note
rerun summarized in the README. Its two previously tracked artifacts were moved
here without changing their contents.

## Artifact mapping

Each run directory contains:

- `report.json`: the official evaluator's per-instance test report.
- `patch.diff`: the exact agent prediction applied by the official evaluator.

The Astropy and Django directories also contain:

- `result.json`: the original runner record, including agent outcome, duration,
  usage, and configured runner budgets.
- `evaluation-summary.json`: the official aggregate report, copied without
  content changes from its model/run-named report file.

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

See [benchmark methodology and run history](../agent-benchmarks.md) and the
[reliability follow-up](../../tasks/09-agent-reliability.md).
