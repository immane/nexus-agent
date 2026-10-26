# Agent reliability follow-up

## Goal and constraints

Improve convergence efficiency, delivery quality, and traceability rather than
repeatedly demonstrating the same repair. This is a proposed work backlog, not
implemented behavior or a claim of measured improvement.

- Do not add task-specific prompts or rules for SymPy, Astropy, or Django.
- Passing task tests does not excuse abnormal termination, incorrect temporary
  scripts, or uncertain tool effects.
- Preserve unknown usage, failure causes, and execution outcomes as unknown.
- Change one primary experimental factor at a time; collect evidence before
  deciding on product changes.
- Do not introduce dependencies or complex memory infrastructure without evidence
  that they are needed.

Related context: [benchmark observations](../research/agent-benchmarks.md) and the
[task-context compiler research proposal](../research/task-context-compiler.md).

## P0: Establish a reliable experimental baseline

### 1. Reconcile existing run records

- [ ] Inventory the SymPy attempts, including reported successful repairs that
  have not been archived.
- [ ] Archive Astropy and Django outcomes, original predictions, and official
  evaluator reports.
- [ ] Separate verifiable records from user observations. Do not fabricate
  missing metrics or retrospectively treat observations as evaluator results.
- [ ] Correct inaccurate test counts in existing summaries where applicable.
- [ ] Record the following for every attempt:
  - Nexus commit, model identity, configuration, and image identities.
  - Dataset revision, instance ID, and base commit.
  - Agent terminal outcome, error category, and original reason.
  - Whether a patch was saved and whether official evaluation passed.
  - Usage, duration, tool calls, and whether context reduction occurred.
  - Infrastructure failures and declared retry reasons.

Acceptance: each conclusion is traceable to a specific run and original
artifacts. Agent outcome and patch outcome are separate fields.

### 2. Analyze actual tool trajectories

- [ ] Determine whether existing logs contain tool arguments, results, and
  ordering; model narration alone is insufficient.
- [ ] Locate when the final correct core patch first appeared, where evidence
  permits.
- [ ] Classify subsequent turns as verification, repair of verification scripts,
  environment investigation, or repeated operations.
- [ ] Examine Astropy stash operations for incomplete restoration or state
  confusion.
- [ ] Identify the exact limit and trigger path behind `model output limit
  reached`; distinguish per-response limits from aggregate output usage.
- [ ] If trajectories are incomplete, state the analysis boundary and design
  minimal opt-in recording with appropriate redaction.

Acceptance: distinguish confirmed problems, hypotheses, and missing evidence.
Repeated-looking activity alone is not evidence of forgetting.

## P1: Improve verification and completion

### 3. Define generic completion criteria

- [ ] Define the facts to establish before completion:
  1. Requested changes are persisted.
  2. Appropriate verification was performed, or its blocker is explicit.
  3. The final diff was reviewed.
  4. Temporary artifacts were handled or their retention was justified.
  5. No uncertain tool effects remain unexplained.
- [ ] Define when to stop widening verification or reproving the same fact.
- [ ] Allow honest delivery with blocked verification instead of endless
  environment investigation.
- [ ] Separate model behavior guidance from runtime-enforced constraints.
- [ ] If experimenting with host instructions, keep them short, generic,
  optional, and disabled by default during initial evaluation.

Acceptance: completion is not a superficial response template. Existing
protections for uncertain effects, cancellation, and timeout remain intact.

### 4. Prioritize reliable verification

- [ ] Prefer existing tests directly related to the change.
- [ ] Use minimal reproductions or add regression tests when suitable existing
  tests are unavailable.
- [ ] Widen test scope only for an explicit benefit; do not default to the whole
  project suite.
- [ ] Distinguish implementation failures, verification-script errors, and
  environment failures.
- [ ] Stop repeating environment checks once sufficient evidence exists, and
  record the remaining unverified scope.
- [ ] Avoid repeated worktree edits, stash operations, or restoration merely to
  establish an environment problem.

Acceptance: a correct repair has a reliable path to completion without weakening
verification requirements to inflate completion rates.

### 5. Review final delivery artifacts

- [ ] Inspect tracked and untracked files and the final patch.
- [ ] Identify temporary reproduction scripts created by the agent.
- [ ] Validate assertions and choose an appropriate location for scripts retained
  as deliverables.
- [ ] Never delete pre-existing user files or files of uncertain ownership.
- [ ] Distinguish changed behavior, verified behavior, blocked verification, and
  remaining risks in the final response.

Acceptance: incorrect temporary assertions are not silently delivered. Official
evaluation uses the original submitted prediction, not a manually improved patch.

### 6. Improve tool discovery and evidence-based research

- [ ] Provide a runtime-generated, bounded capability summary at task start:
  available executables and relevant versions, tool path scopes, writable
  locations, and supported network/search interfaces. Do not rely on stale
  image documentation or repeated command probes.
- [ ] Make tool errors distinguish unsupported commands/options, invalid or
  out-of-scope paths, worker startup failures, and execution failures, while
  keeping diagnostics bounded and free of sensitive payloads.
- [ ] Evaluate a dedicated, permission-bounded web search and page-reading
  integration that returns limited titles, snippets, URLs, and source identity;
  prefer it to ad hoc shell scripts and raw HTML when available.
- [ ] For research tasks, use a staged retrieval strategy: identify the relevant
  project facts, formulate comparison criteria, search a small candidate set,
  and open only sources needed to verify claims. Bound page and result output.
- [ ] Require final factual recommendations to retain source links and separate
  verified facts from similarity judgments and unverified candidates. Exclude
  claims whose author, title, or cited detail could not be verified.
- [ ] Stop and change strategy after repeated low-yield searches or environment
  probes. Record the blocker or uncertainty rather than inventing an explanation
  for search behavior or filling gaps from uncertain memory.
- [ ] Track useful progress versus repeated planning, probes, and failed
  retrievals; assess context use alongside tool-call count and completion time.

Acceptance: a generic multi-step research task can discover its actual tool
capabilities, gather bounded and attributable evidence, and produce calibrated
recommendations without repeated environment probing or unsupported factual
claims. Improvements must be evaluated on tasks not used to design the behavior.

## P1: Make experiments and resource lifecycle reliable

### 7. Add startup preflight checks

- [ ] Check that the server supports container execution.
- [ ] Check port availability, model endpoint, credential references, images,
  and checkout.
- [ ] Check that the workspace is an independent Git checkout at the required
  base commit.
- [ ] Check ownership state without automatically bypassing unresolved records.
- [ ] Stop immediately on preflight failure rather than invoking the runner.
- [ ] Capture the actual configuration and executable identity so an intervening
  build cannot silently change the experiment.

Acceptance: missing features and premature server exits are identified before a
task starts.

### 8. Improve shutdown and cleanup

- [ ] Determine why workload and gateway containers remain after task completion.
- [ ] Design and verify graceful server shutdown with bounded cleanup.
- [ ] Handle evaluator host exit while container-side evaluation is still active.
- [ ] Reconcile exact ownership identities; never use broad resource deletion.
- [ ] Preserve records and actionable reconciliation details when cleanup fails.
- [ ] Cover completion, startup failure, cancellation, timeout, and interruption.

Acceptance: normal operation leaves no run-owned resources. Exceptional paths do
not delete unrelated resources or falsely report successful cleanup.

### 9. Preserve partial and final results

- [ ] Save the patch, known usage, and errors where possible after abnormal agent
  termination.
- [ ] Mark whether usage is complete.
- [ ] Use separate logs for every attempt; never overwrite failure evidence.
- [ ] Record agent, runner, and evaluator outcomes separately.
- [ ] Exercise disk-write failures and interrupted partial-result handling.

Acceptance: repairs produced before abnormal termination are not lost from the
record.

## P2: Validate context reduction independently

### 10. Start with deterministic long-history replay

- [ ] Construct cross-turn histories unrelated to any specific benchmark answer.
- [ ] Trigger item and byte pressure independently.
- [ ] Check retention of the current task, newest exchange, pending calls, and
  uncertain effects.
- [ ] Check complete call/result pairing, provenance boundaries, and the separate
  runtime context note.
- [ ] Check that instructions and note bytes are charged to request budgets.
- [ ] Cover cross-submit history, failure recovery, cancellation, and contexts
  that cannot safely be reduced.
- [ ] Preserve before/after snapshots and evidence mapping for removed items.

Acceptance: explain when reduction occurs, what it removes, and why removal is
allowed. Context notes never confer tool authority.

### 11. Run live comparisons only when warranted

- [ ] After deterministic validation, select tasks that naturally create long
  histories.
- [ ] Fix model, parameters, tools, environment, and budgets.
- [ ] Compare one main change at a time.
- [ ] Treat runs without reduction as agent-behavior observations only.
- [ ] Report artificially reduced budgets as stress experiments, separately from
  default-configuration runs.

Acceptance: aggregate token usage and task duration are never substitutes for
evidence that context reduction actually occurred.

## P2: Evaluate generalization on a small fixed sample

### 12. Preselect the next task batch

- [ ] Start with four to six tasks across repositories and problem types.
- [ ] Do not replace tasks based on known solution difficulty or interim results.
- [ ] Include logic defects, boundary conditions, cross-file changes, and varied
  test environments.
- [ ] Declare budgets, retry eligibility, and stopping conditions before runs.
- [ ] Retain failed and infrastructure-invalid attempts, not only resolved tasks.
- [ ] Finish batch analysis before tuning prompts and retrying failed tasks.

### 13. Compare outcomes and efficiency

- [ ] Report official task-test results and normal agent completion separately.
- [ ] Measure additional turns and time after the first correct repair where
  evidence supports that measurement.
- [ ] Compare tool calls, input/output usage, temporary files, and unrelated edits.
- [ ] Evaluate behavior when verification is blocked.
- [ ] Record reduction events and evidence of repetition or forgetting.

Acceptance: do not optimize resolved rate alone. Efficiency, delivery quality,
and safety must not materially regress. Small samples support observations, not
claims of stable improvement.

## P3: Let evidence determine memory research investment

- [ ] If completion remains the bottleneck, prioritize completion and verification
  behavior.
- [ ] Only trial a minimal task IR when evidence identifies loss of important
  context as a cause; include provenance, applicability, and evidence references.
- [ ] Distinguish facts, inferences, unverified hypotheses, and uncertain tool
  execution outcomes in any IR.
- [ ] Defer Graph, Vector DB, and LoRA until experiments establish a concrete need.
- [ ] State the costs, benefits, failure paths, and disablement strategy for each
  new mechanism.

## Suggested execution order

1. Reconcile records and analyze trajectories.
2. Add preflight checks and reliable result persistence.
3. Improve resource cleanup.
4. Experiment with completion and verification behavior.
5. Validate reduction through offline replay.
6. Run controlled comparisons on new tasks.

Begin with P0 and startup preflight: these reduce the risk of optimizing against
incomplete evidence or unreliable experiments. Implement and validate in focused
batches rather than treating this backlog as one large change.
