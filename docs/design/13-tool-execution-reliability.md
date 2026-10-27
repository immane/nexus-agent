# Tool Execution Reliability and Observability

Status: **Draft design proposal, not accepted interfaces or implemented global
behavior.** Written on 2026-10-27 following the Sphinx benchmark investigation.
Implementation requires separate review and authorization for each compatible
slice. This document does not authorize interrupting an active run, rebuilding
images, changing permissions, deleting resources, or making hosted model calls.

## Purpose and authority

Make long-running tool execution observable and its results attributable without
weakening cancellation, resource bounds, or uncertain-effect quarantine. Apply
the execution capabilities to ordinary TUI/server operation as well as benchmarks;
keep framework-specific test strategy in agent guidance and benchmark tooling.

Accepted product decisions remain authoritative, especially
[first-release defaults](decisions/01-first-release-defaults.md). This proposal
does not change development-mode permissions, strict-mode approvals, or backend
selection. The [tool](../contracts/02-tool.md) and
[command/event](../contracts/03-command-events.md) contracts are drafts; their
detailed APIs are not existing implementation guarantees. In particular, the
tool contract's strict-default permission wording must not override Decision 01.

Related designs are [execution](02-execution.md),
[container execution](11-container-execution.md), and
[context management](12-context-management.md). The
[agent reliability follow-up](../tasks/09-agent-reliability.md) describes adjacent
verification and experimental-evidence concerns; this document specifies the
execution boundaries rather than introducing a new scheduler or memory system.

## Evidence and limits of the investigation

Two distinct failures must not be conflated:

| Observation | Supported conclusion | Unsupported conclusion |
| --- | --- | --- |
| A cached worker accepted `host_read` with only `path`, but exited 2 when the same operation included `offset` and `limit`; the rebuilt worker passed the paged-read probe | The server and embedded worker had incompatible argument capabilities despite both using protocol version 1 | The model failed to solve Sphinx, or Docker was generally too slow |
| A full-suite command ran under `timeout 3000 ... \| tail -20`, stopped after approximately 3,000 seconds, and the enclosing shell returned success | The pipeline hid the timeout wrapper's status behind the successful output filter | Tests passed because the tool reported `Succeeded` |
| Recent temporary directories involved link-check tests; pytest had waiting threads, low CPU, no swap, and no OOM events | A wait in the test suite was plausible; memory exhaustion was not supported by the measurements | A particular link-check test, network restriction, or Rosetta was proven to be the root cause |
| After the timeout, the model issued another full-suite command with better logging but the same long deadline | Better host guidance and progress visibility were warranted | A runtime may safely rewrite arbitrary commands or retry uncertain work |

Agent completion, tool completion, test completion, and official benchmark
resolution remain separate outcomes. Neither this document nor diagnostic test
counts establish official resolution for the Sphinx attempt.

## Current implementation boundary

This is a source snapshot, not a released-version compatibility claim. Some
benchmark changes were present in the working tree when this document was
written; their existence is not evidence that they were committed or deployed.

| Area | Current behavior | Global gap |
| --- | --- | --- |
| `nexus-core/src/outcomes.rs` | `ToolOutcome` separates execution status, effect, evidence, bounded content, and truncation | No structured process exit observation |
| `nexus-core/src/tool.rs` | Synchronous `ToolPort::execute`; context carries approved scope, output budget, cancellation, and deadline | No executor-to-runtime progress sink |
| `nexus-core/src/commands.rs` | `ToolProgress` and `ToolOutput` exist, carrying call identity, preview, and truncation | Existing process execution does not feed live stdout/stderr into them |
| `nexus-tools/src/exec.rs` | Local sandbox captures output, observes process status, and handles termination | Numeric exit code/signal are discarded from the returned domain outcome |
| `nexus-container/src/exec.rs`, `worker_transport.rs` | Container transport observes Docker CLI status; nonzero status is represented in text, with a `-1` fallback for no numeric code | Observer identity and absence of a code are not structured; output is collected until completion |
| `nexus-runtime/src/runtime.rs` | Admits calls, records intent, clamps deadlines, publishes lifecycle events, and quarantines uncertain blocking workers | No bounded progress bridge during execution; late worker activity needs a progress publication fence |
| `nexus-server/src/json.rs` | Manually encodes outcomes and progress; SSE sends idle connection pings | No process metadata or actual executor output stream |
| `nexus-session/src/lib.rs` | Separate operation outcome records and source-history tool results; file revision 3, revision 2 readable | Both persistence representations need explicit metadata compatibility handling |
| TUI/server CLI | Ten-minute tools within one-hour runs; benchmark-pilot uses one-hour tools within 24-hour runs | Longer budgets alone cannot diagnose hangs or repair worker incompatibility |
| Scripted headless | Uses M0 test limits and scripted execution | Do not advertise this as a live-provider execution frontend |

Paths in this table are relative to `crates/`. Keep runtime dependencies directed
toward domain types, not Docker or frontend implementations.

### Benchmark-only improvements already present

The current benchmark launcher has a read-only paged worker preflight and handles
an already-removed workload before exact-identity gateway cleanup. The runner
shows rate-limited SSE liveness reports, reports available output-event metadata,
and checks configured budgets during heartbeat-only intervals. Its task prompt
requests preserved test status, staged verification, progress logs, supported
stack diagnostics, and a changed diagnostic approach after timeout.

These are **not** global worker admission, global streaming stdout, enforced
shell pipeline semantics, or per-test runtime timeouts. See the
[benchmark documentation](../../tools/benchmarks/README.md) for their actual scope.

## Non-negotiable boundaries

Execution status is not proof of task success. Effects and evidence remain
independent of process exit. Unknown effects do not become not-applied merely
because the CLI exited or the frontend disconnected. Cancellation is not rollback.

Only the runtime publishes authoritative lifecycle ordering. Tools cannot grant
permissions, alter conversation state, inject model instructions, or publish
post-terminal events. Each started call has at most one live `ToolFinished`;
late reconciliation must not create a second finish or replay the operation.

Output, progress queues, retained history, and optional disk logs are bounded.
Slow consumers cannot prevent cancellation or cause unbounded buffering. Raw
output is untrusted: terminal control sequences, secrets, binary data, and split
UTF-8 must be handled at the appropriate presentation and transport boundaries.

Silence is not a failed test. A heartbeat proves transport liveness only. Neither
silence nor a previously failed command authorizes automatic retry, resource
replacement, network access, or broader permissions.

## Proposed global interfaces and semantics

### Process exit observations

Extend the existing outcome with optional process metadata rather than inventing
a second execution result hierarchy. Illustrative names below describe semantics;
they are not finalized Rust signatures or a new wire contract:

```text
ToolOutcome
  existing status / effect / evidence / content / truncated
  process: optional ProcessObservation

ProcessObservation
  observer: LocalProcess | DockerExecCli
  termination: Exited(code) | Signaled(signal) | Unknown
```

The existing `ExecutionStatus` continues to express admission/execution outcome,
including timeout, cancellation, and interruption. A termination request and an
observed exit are different facts; preserve both when they race. If a further
stop-request reason is necessary, add a bounded enum explicitly rather than
embedding diagnostic strings into the exit code.

| Situation | Required representation |
| --- | --- |
| Normal zero/nonzero local exit | Actual code with `LocalProcess`; derive succeeded/failed without claiming known effects |
| Observed local Unix signal | Signal when available; do not fabricate a portable numeric exit code |
| Completed Docker CLI | Actual CLI code with `DockerExecCli`; do not claim independent observation of the workload's signal or termination |
| Interrupted Docker transport | Unknown remote effects and existing quarantine; any local CLI observation remains explicitly local |
| Rejected or never-dispatched operation | No fabricated exit code; preserve denied/not-started semantics |
| File tool without a subprocess | Process metadata absent |
| Historical result without metadata | Unknown/not recorded, never an assumed zero |

Remove the semantic need for sentinel `-1`: absence or unknown termination is a
typed state. Keep textual diagnostics useful for older clients, but new clients
must not parse prose such as `exit status 1` as the authoritative data source.
Do not infer that every exit 124 is a timeout: a program can intentionally return
124. A shell pipeline exposes only its selected shell status; per-stage status
requires cooperation from that shell or an explicitly different execution API.

Existing constructors may default metadata to absent to minimize caller churn.
Add a validated process-specific construction path, update adapters, and retain
existing status/effect/evidence checks. No external plugin may turn self-reported
metadata into host-observed evidence.

### Worker admission and capabilities

Introduce a bounded, side-effect-free capability query to the worker executable,
before publishing its tool registrations for use. A possible invocation is
`container-worker --capabilities`, with stdin closed and a small fixed response
limit and deadline. It must not evaluate user code, write probe files, expose
environment variables, or execute a model-supplied command.

The response identifies a capability-envelope revision, supported file-protocol
versions, operations, and concrete argument features such as paged reads. Host
admission compares required capabilities derived from its actual registrations;
matching protocol version alone is insufficient. An optional source fingerprint
is provenance evidence, not a requirement that compatible binaries share an
identical source tree.

An old worker may ignore a new CLI argument and wait for stdin. Closed stdin,
bounded execution, and explicit rejection of missing/malformed capabilities are
therefore mandatory. Unsupported workers produce an actionable rebuild diagnostic
and fail admission before model inference, rather than poisoning an admitted
session on its first file operation. Health checks and isolation checks remain
separate: a keeper being healthy does not prove file-worker compatibility.

Cache the verified result only within the immutable image ID and live environment
generation. Recreation or changed images invalidates that evidence. Preserve the
benchmark paged-read probe until global admission demonstrates equivalent or
stronger compatibility checks; capability declarations alone do not exercise an
actual request/response round trip. The generic round-trip probe must be read-only
and not depend on Git or a particular project layout.

### Bounded executor progress

Keep synchronous tool execution and the existing runtime ownership model. Add an
optional narrow progress reporter to `ToolContext`, backed by a bounded per-call
channel. The runtime constructs it after admission; callers without a reporter
retain existing behavior. Executors offer stream-tagged byte chunks to that
reporter; they cannot choose run identity, call identity, event sequence, or
publication authority.

The runtime drains accepted chunks while waiting on the blocking worker and uses
the existing `ToolOutput` presentation path. Preserve stdout/stderr identity where
known, but promise only host observation order, not an exact total ordering across
two OS pipes. Use bounded incremental UTF-8 decoding with explicit handling of
invalid bytes and trailing partial characters.

Initial bounded defaults can be reviewed as constants rather than introducing a
new public configuration surface. One candidate is at most 16 queued chunks of
4 KiB per active call, additionally limited by that call's byte allowance. Queue
capacity is not permission to retain more than the effective output budget.
Under pressure, drop/coalesce optional previews with an explicit loss indication;
continue draining process pipes so a full pipe does not deadlock the child.

The effective output budget `B` must bound **progress plus final content**, not
permit each to consume `B` independently. A simple candidate reserves up to
`min(B / 4, 64 KiB)` for streamed previews and the remainder for final output.
Count accepted preview bytes even if a downstream optional presentation event is
later dropped. Duplicate bytes repeated in final output are charged again; this
keeps transmission and retention accounting conservative and explicit. With
streaming disabled, the existing final-result allowance remains `B`. Small budgets
must leave useful final diagnostics or disable previews rather than underflow.

Final content remains the canonical model-visible tool result. Streaming
presentation must not also append independent tool-result messages to model
history, double-count token usage, or cause a summarizer to receive the same
output repeatedly. Output loss, final-content truncation, and dropped progress
are distinguishable facts; preserve existing per-run drop/truncation reporting.

Do not add unlimited temporary output files as a fallback. Optional file-backed
logs require a separate approved path, retention/byte limits, and access checks.

### Cancellation, completion, and late output

The runtime owns a per-call publication gate in addition to existing cancellation
and quarantine state. On normal completion, bounded pending progress can be
drained before `ToolFinished`. On timeout or cancellation, close the publication
gate before recording the terminal call outcome; late worker writes are rejected
without blocking termination. No `ToolOutput` follows that call's finish or the
run's terminal event.

The current context uses the run's cancellation token. Do not silently change
run-wide cancellation into per-call cancellation as part of streaming. A closed
progress sink stops publication, not necessarily the remote command. If the
blocking worker remains live, retain quarantine and its resource ownership until
existing reconciliation establishes a safe state.

Transport interruption must keep bounded diagnostic categories, such as invalid
worker response versus CLI failure. Do not print arbitrary request bodies,
arguments, generic third-party error strings, or stderr directly into the TUI.
Route metadata through the existing diagnostics boundary; payload logging remains
separately controlled and cannot guarantee arbitrary-secret detection.

### Frontend liveness and long-call presentation

TUI/server clients display elapsed call duration, last observed output age,
output-loss/truncation state, and termination/uncertainty status. They can derive
elapsed presentation from `ToolStarted` and their local monotonic clocks, without
adding high-frequency authoritative heartbeat events. A reconnecting client needs
a bounded active-call snapshot; do not imply historical precision absent recorded
start evidence.

Keep SSE pings transport-only and out of runtime sequence accounting. Distinguish
connection alive, process alive, output observed, and useful task progress. An idle
indicator may say `quiet` or `no recent output`, never assert `deadlocked` solely
from a timer. No-output notices do not reset deadlines or trigger cancellation.
Actual configured run/tool budgets remain enforceable during silence.

Render bounded progress incrementally without repeatedly rebuilding the entire
transcript. Slow TUI rendering, a disconnected SSE client, or a saturated output
queue must not starve approvals, cancellation, or required finish delivery.

### Ownership and cleanup reconciliation

Use the existing ownership store and lifecycle owners; do not add a second
resource manager. Preserve stable nonce, exact container IDs/names, owner labels,
and workspace association as audit evidence. A future record format can represent
each resource as present, removal-requested, removed-and-verified, or uncertain,
rather than making an absent ID the only explanation of removal.

The runtime/session owner performs normal shutdown. A launcher or explicit
operator reconciliation acts only after establishing that the former owner is
not active; PID disappearance alone is not universal proof across PID reuse or
hosts. Existing stale-lock admission remains fail-closed until deliberate
reconciliation; this proposal does not authorize automatic lock deletion.

Deletion requires fresh exact ID, label, and relevant workspace verification. If
an ID was cleared, verify exact-name absence before treating the resource as
already removed; a surviving named container without sufficient identity remains
unresolved. An engine inspection error is not evidence of absence. Remove the
workload before the gateway, and preserve ownership evidence if either cleanup or
record persistence is uncertain. Cancellation never implies successful cleanup.

Persist transitions atomically. A crash after removal but before recording its
completion is reconciled by inspection without blind mutation replay. New record
fields require their own ownership-format migration; they are not the same schema
as conversation/session persistence.

## Compatibility and distribution

| Boundary | Compatibility treatment |
| --- | --- |
| Rust outcome/context types | Preserve existing construction paths where practical; private additive metadata with validated accessors; no dependency on Docker in core |
| Server JSON | Document optional bounded process metadata and stream identity; old data means unknown; test consumers that reject unknown fields before selecting additive rollout or a wire revision |
| Session persistence | Update operation outcome records and source-history tool results together. Existing strict revision-3 readers cannot silently accept changed records; propose a revision-4 writer only after migration review, with revision-2/3 read fixtures and explicit rejection by unsupported readers |
| Worker executable/protocol | Capability discovery is separate from file-protocol versioning; validate required argument features, response framing, and immutable image identity |
| Ownership records | Version independently from session files; keep old records readable for safe inspection, never infer authority from an unrecognized record |
| Model-visible content | Preserve bounded final content and honest status wording; optional metadata must survive serialization without granting execution authority |

Package server/worker artifacts with matching capability evidence. Rebuilding a
host binary does not update cached worker images. Record immutable image IDs and
source/build evidence, while distinguishing compatibility from reproducibility
and from isolation validation. Verify local-only builds remain independent of
Docker and container features.

Rolling back a binary is not equivalent to rolling back data. Do not launch an
older reader against newly written session/ownership revisions. Preserve original
records and use an explicit migration/recovery process; disabling optional
streaming cannot erase already emitted events or undo applied effects.

## Review boundaries and dependency order

The following are independently reviewable design slices, not a single release
commit or an authorization to implement everything together.

| Slice | Dependencies and bounded scope | Evidence required before broad enablement |
| --- | --- | --- |
| Worker admission | Can precede all other changes; container startup, worker capability response, image verification | Old incompatible image rejected before inference; compatible worker round trip; failure cleanup verified |
| Process observations | Core types, local/container adapters, server encoding, session compatibility together | Exit/signal/unknown matrix; old persisted data readable; no changed command semantics |
| Progress transport | Final-output budget semantics reviewed first; core reporter, executor drains, runtime publication gate | Bounded memory under flood; cancellation and required events responsive; no post-finish output |
| Frontend presentation | Uses existing start/finish immediately; consumes optional streaming/exit metadata when available | Safe rendering, stale-run rejection, clear quiet/uncertain states, responsive slow-consumer behavior |
| Cleanup state representation | Can proceed independently; lifecycle and ownership-format compatibility reviewed together | Partial removal, crash windows, stale owners, failed inspection, and foreign resources remain safe |
| Benchmark integration | Uses generic capabilities as they become available; retains framework-specific guidance | Separate attempt evidence; no silent retries or hidden conversion of infrastructure failures into model scores |

The smallest useful global change is worker admission. Process observations are
a medium cross-representation change. Streaming is the largest slice because it
crosses output accounting, concurrency, and delivery semantics. Avoid bundling
provider changes, dependency upgrades, sandbox/network policy changes, or a new
task scheduler into any of these slices.

Before each slice, review the actual source and accepted contracts again. Update
affected contracts and operator documentation with that slice; this draft is not
permission to mechanically implement its illustrative names or numeric defaults.

## Verification and acceptance evidence

| Area | Deterministic regression coverage |
| --- | --- |
| Exit metadata | Exit 0, nonzero, signal/no code, pre-dispatch denial, timeout/cancel races, Docker CLI versus remote uncertainty; wrapper exit 124 not automatically reclassified |
| Shell semantics | Explicit `false \| tail` can legitimately return 0; explicit Bash `pipefail` exposes failure; direct argv remains unchanged; no implicit command rewriting |
| Worker admission | Missing executable; old argument schema; malformed/oversized/duplicate-key response; deadline; EOF behavior; required capability absent; optional extra capability; changed environment generation |
| Progress correctness | Silent child; split UTF-8; binary bytes; separate stderr/stdout; rapid output; queue saturation; terminal escape data; final output/truncation budget boundaries |
| Lifecycle ordering | Tool start before output; finish exactly once; no late output after timeout/quarantine or run termination; no stale data adopted by a subsequent run |
| Cancellation/backpressure | Slow/dropped consumer, closed progress channel, child holding pipes open, output flood; control messages and cleanup remain bounded |
| Persistence | Old session revisions, absent metadata, new round trip, strict unknown schema rejection, disk failure, interrupted writes; operation record and source history agree |
| Cleanup | Workload already removed, gateway remains, missing ID with surviving name, foreign label/mount, owner still active, PID reuse ambiguity, daemon unavailable, partial record update |
| Frontends | Quiet versus liveness wording, bounded transcript, sanitization across chunks, reconnect gaps, terminal restoration and no uncontrolled stderr writes |
| Benchmark reporting | Heartbeats do not fabricate event sequences or progress; budgets checked during silence; model completion and official evaluation remain separate |

Use [the workspace testing guidance](../testing.md). Select affected packages per
slice and run Cargo commands sequentially against the shared target directory.
For example, a core/result/persistence change may require `nexus-core`,
`nexus-session`, `nexus-runtime`, `nexus-tools`, `nexus-container`, `nexus-server`,
`nexus-tui`, and affected fake consumers; do not assume only the edited crate is
affected. Enable worker/keeper or frontend container features when their paths
change. Register new integration modules in the corresponding `tests/behavior.rs`.

Formatting, affected-package nextest, and Clippy are the normal Rust checks.
Benchmark changes use `python3 -B -m unittest discover -s tools/benchmarks -p
'test_*.py'`. Routine validation uses fake transports, clocks, subprocesses, and
legacy format fixtures; it must not silently invoke Docker, hosted inference, or
external evaluators.

Real Docker validation is a separate, explicitly chosen smoke test with fresh
identities, read-only probes where possible, and exact-identity cleanup. Preserve
each result. Docker Desktop/Rosetta evidence does not establish native Linux or
other architecture behavior. Hosted benchmark reruns and official scoring require
their own explicit opt-in and remain separate from development tests.

Measure time to first output, bounded retained bytes, peak queue occupancy,
cancellation latency under output flood, and terminal-result delivery under a slow
consumer. Record environment and before/after workloads; do not claim speedups from
language choice or infer that larger deadlines improved convergence.

## Deliberate exclusions

No global automatic `pipefail`, shell rewriting, pytest dependency, per-test
framework parser, network opening, silent retry, or timeout escalation based only
on silence. No remote-process termination claim inferred from a killed Docker
CLI. No unbounded logging, new infrastructure service, automatic stale-lock
takeover, or broad Docker prune. Existing finite timeout defaults remain unchanged
unless a separate behavior change is reviewed.

Framework-specific case logs, stack dumps, and staged verification remain agent
guidance or explicit project configuration. Their effectiveness must be measured;
a prompt change is not runtime enforcement. Actual per-test timeouts, debugger
attachment, or thread-stack collection can have side effects and need deliberate
framework/platform-specific support outside this generic execution proposal.
