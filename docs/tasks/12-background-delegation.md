# Background delegation

Status: offline runtime foundation implemented; delegation is not exposed to
Web clients or models. The remaining behavior is a proposed implementation
plan. This is product behavior; [03 - Subagents](03-subagents.md) describes how
to divide development work on this repository.

The runtime foundation uses an ephemeral child runtime for each accepted
exploration and a run-owned job registry. It accepts a host-selected provider
and host-supplied project-jailed read registrations, returns a bounded result
at most once, and signals all unfinished children on parent cancellation.
The tools crate supplies a project-local read/list/search set that rejects
external targets and protected paths without consulting parent grants.
There is no configured model resolution, Web tool registration, child inbox
handoff, proposal application, or background recovery yet. The host must
enforce the parent tool-call budget and project scope before exposing this
primitive to a model.

## Goal and first release

Let a parent agent overlap its work with multiple scoped investigations.
Children can find code, explain call paths, identify tests, or draft an edit.
They do not write to the workspace. The parent can continue working and wait
for a specific child only when its answer is needed. Model and reasoning
choices are independent of the parent's and apply to one invocation.

Ship `Explore` enabled by default in eligible local Web/desktop sessions:
the session has a bound local project, a supported model, and the host can
compose the read-only tool set. No experiment switch is required. Add
`Propose` after the read-only and cancellation gates pass. TUI and normal
headless behavior stay unchanged. Use scripted providers and disposable
workspaces for development. Demo, container, and unbound sessions don't
expose delegation in v1. Default availability doesn't dispatch a child on
session creation or make an extra inference request.

Keep the existing [single-run execution model](../design/02-execution.md):
one active run per runtime and sequential dispatch within each run. A child
gets a short-lived runtime with its own context, continuation state, limits, and
event receivers. Do not add a crate, dependency, process, dedicated thread per
job, daemon, database, generalized scheduler, or new SSE protocol. An eligible
session that doesn't delegate incurs no child worker, provider discovery, or
additional model request.

There is **no delegation-specific task-count or concurrency cap**: no
per-parent total, simultaneous-per-parent limit, or server-wide child limit.
Don't add new counters, semaphores, or configuration for those limits, or
implicitly queue launches to throttle them. Multiple start calls in a parent
turn are still dispatched in order; each accepted start returns promptly, so
the children can overlap. The existing parent run/tool budgets, each child's
individual limits, and provider/host constraints still apply. This is not a
promise of unlimited throughput or protection against aggregate cost. If launch fails or a
provider refuses work (including rate limits), report it; don't queue or
retry the delegation behind the caller's back. The
[session inbox](13-session-prompt-queue.md) controls when finished child
results reach the parent. It does not queue child launches.

Not in v1: recursive delegation, autonomous write/exec, shared-write locks,
worktrees, patch merging, resume after restart, child sessions in the sidebar,
cross-session messaging, a batch dispatch or join API, or recursive follow-up
work triggered by child-only input.

## Caller contract

Names and bounds below are proposed; freeze them only after the offline slice
proves the behavior.

| Operation | Inputs | Result |
| --- | --- | --- |
| `delegate_start` | `role` (`explore`, later `propose`), `task`, optional `model` (configured model ID), optional `thinking` (`inherit`, `off`, `low`), optional `delivery` (`steer` by default, or `queue`) | An opaque job ID and `accepted`. This means admitted, not completed. Returns without waiting on the child provider. `delivery` affects only the result, never launch timing. Invalid or over-budget calls do not create jobs. |
| `delegate_await` | `job_id` from this parent run | A bounded typed result. If the job is pending, suspend the parent at this tool call without polling the model. If already terminal, return immediately. The same ID is never executed again by awaiting it. |

`Explore` returns a short conclusion, project-relative file/line references,
and any important uncertainty. `Propose` may additionally suggest one exact
replacement in an existing UTF-8 file: a relative path, `old_text`, and
`new_text`. Validate shape, size, path scope, and any referenced file before
returning a successful structured proposal; malformed output is not a valid
proposal. Use bounded text/JSON through the current provider path, not a new
vendor-specific structured-output contract. A result carries the job ID, state
(`completed`, `failed`, `cancelled`, or `timed-out`), and an explicit
truncation or output-limit indication where applicable. Failed and cancelled
results do not masquerade as successful proposals.

An `await` from another run or session is rejected. Reuse existing tool call
identity checks; duplicate or uncertain start replies must not accidentally
dispatch again. Store accepted jobs by parent-run identity and opaque job ID,
not in a single slot. `await` selects only the specified job even when children
complete out of start order. Either `await` returns the payload through
normal tool pairing, or the inbox delivers it as typed child-completion
data, not both. Claim once across the two paths and release the payload
only after handoff has been accepted. Later `await` calls report the
terminal state and `already collected` instead of repeating the payload
or re-executing the child. The existing
parent tool-call budget applies to both operations. No join-any, manual retry
tool, or implicit retry is added. When other work can overlap, start
independent children before awaiting any of them.

## Model and permission boundaries

- Default enablement authorizes use of the parent's bound model, not every
  configured model. Let the caller select a different enabled model only from
  an operator-approved allowlist of configured model IDs. An unset list
  allows only the parent model. The user can select the model, or the parent
  can choose within that policy. No endpoint, credential, operating mode,
  filesystem root, or tool list is caller-supplied. Keep credential resolution
  on the host; don't pass secrets to the child or include them in its output.
- Omitted `model` uses the parent's actual bound model. If it cannot be
  identified, require an explicit allowed model rather than resolving a
  possibly different provider default. The selected model must support text
  and structured tool calls and pass existing provider checks. Switching
  providers sends the bounded task and selected tool outputs to that
  provider; an explicit allowlist entry is the operator's authorization for
  that transfer, not just a pricing preference. No automatic cross-provider
  fallback.
- Omitted `thinking` and `inherit` resolve the child's configured
  model → provider → global default; they do not copy the parent's per-call
  settings. An explicit `off` or `low` overrides that resolution for this job
  only. Validate the effective value with the selected adapter before any
  inference or network discovery. Reject unsupported values instead of
  silently ignoring them. Current explicit support is limited to recognized
  Ollama Direct / Chat Completions; `low` can mean only “thinking on” for a
  boolean-capability model. Do not introduce extra effort levels. Freeze model,
  credentials reference resolution, effective effort, and limits for the
  admitted job; do not write settings back to the parent or configuration.
- The child sees the bounded task brief and its own tool results, not the
  parent transcript, archive, approval grants, or provider continuation. Bind
  read/list/search tools to the parent's admitted local project, intersecting
  the parent's effective read scope with the project. Reject outside and
  protected paths; never inherit temporary/external scopes or allow approvals
  to expand child permissions. Use actual tool registration and filesystem
  checks, not prompt-only restrictions. No patch, write, exec, external
  network tool, or delegation tool is registered in the child. Model requests
  still use the chosen provider's normal transport.
- `Propose` is data, not an approval, command, or effect. No automatic apply.
  The parent re-reads the target, reviews the replacement, and uses its ordinary
  `host_patch` path and current authorization if it chooses to apply. Its
  exact-text guard rejects missing or ambiguous `old_text`; a changed target
  detected during review requires a new proposal or a fresh parent edit.
  The existing patch guard is not a whole-file revision lock. Don't add
  filesystem locking or claim exact revision identity in v1. No creation,
  deletion, or multi-file transaction in a proposal.

## Lifecycle and background handoff

1. **Admission.** Validate the call, parent run and project identity,
   cancellation, absolute deadline, and model/effort. Use ordinary parent
   admission accounting and derive limits for the new child; don't reserve a
   child concurrency slot. Register child ownership in memory before
   publishing `accepted`; order registration against parent cancellation so
   no child is missed. Report a known launch failure instead of leaving a
   job pending without a worker.
2. **Execution.** Run on the existing executor without an implicit launch queue;
   consume each child's bounded data/control channels even when nobody is
   waiting. Keep only the final result and necessary lifecycle/usage data.
   Never mirror the full child stream, chain of thought, or raw tool logs
   into the parent's context or Web transcript. Maintain existing sequence
   and backpressure guarantees for the parent's stream.
3. **Completion and wake.** Publish exactly one bounded terminal state into the
   corresponding job record and signal that job's waiter. The state is
   authoritative; signaling is only a wake-up hint. Completion before `await`
   and completion during registration of a waiter must produce the same
   outcome. A different child completing doesn't satisfy this wait.
   `delegate_await` delivers it through normal tool pairing; otherwise the
   [inbox timing rules](13-session-prompt-queue.md#btw-and-child-completion)
   deliver it at the next safe model boundary for `steer` or after current
   work completes for `queue`. Don't interrupt in-flight generation or
   approvals, invent a provider tool result without a call, or inject a
   user/system message. The inbox owns wake and admission; presentation is
   never responsible for starting a run.
4. **Parent ends or cancels.** On success, children already running may finish
   within the original parent deadline and their own smaller limits. Pending
   results may be delivered once through the same inbox in a bounded
   follow-up. On parent cancellation, failure, deadline, or session shutdown,
   close child admission, suppress pending child results, and signal **all**
   unfinished children. Don't wait for them serially before reporting parent
   cancellation. Terminal cancellation is not proof that provider workers
   have exited: keep ownership until each exits, and make unconfirmed cleanup
   visible without claiming rollback. Reuse existing supervision where possible.
5. **Presentation and cleanup.** Start/wait appear as ordinary tool cards.
   Reuse the authenticated parent run snapshot or its recovery path for a
   compact summary by parent run (pending and per-terminal-state counts, with
   explicit expiry). Don't add a feed or a poller per child. If needed, one read-only
   check may run every two seconds for the visible parent run after success
   while there is unfinished work. Stop on completion, timeout, navigation,
   or tab hidden; reconcile once when returning if still in retention. No
   model polling, permanent Web poller, second subscriber, or post-terminal
   SSE events. After the parent ends, retain aggregate status for presentation
   and only the per-job records needed for inbox handoff or worker supervision.
   Keep their bounded payloads by reference until delivery, explicit removal,
   or expiry. If a pending result expires, drop it without starting a run and
   report expiry through the live session status. Session deletion, idle
   eviction and shutdown must not orphan workers. Job state is not durable.
   Accepted deliveries may be saved through normal parent history with
   their provenance; after restart, unconsumed results are unavailable and
   no job is replayed.

### Starting budgets

Use these initial per-child limits for offline validation. They are not
limits on the number of children.

| Resource | Initial limit |
| --- | --- |
| Admission | No child-count or child-concurrency cap; existing parent admission rules still apply. No nesting or implicit queue |
| Time | Child gets at most 45 seconds and never more than the parent's remaining time; child tool deadline capped at 5 seconds |
| Work | At most 4 child model turns and 12 child tool calls, also capped by the host's configured limits; count child summaries/auxiliary model calls or disable them for this path |
| Context | Task ≤ 4 KiB, collected result including proposal ≤ 8 KiB, at most 8 file references; child context and tool output have their own bounded byte caps |
| Inference | Establish the individual child envelope at admission. Start with an 8K input envelope per child request and a 1K generation cap where supported; do not claim a hard token or pricing cap from estimates or unsupported controls |
| Status | Keep compact status and unconsumed child results for up to 60 seconds after both parent and children are terminal, or until session cleanup. Expiry is not dispatch; non-exited workers remain supervised independently |

Use existing runtime and adapter controls where they fit. Count child
inference, including retries that actually reach the provider, separately from
parent inference. Reuse transport limits, surface provider rate limits and
resource failures, and don't add delegation-level retry or delayed dispatch.
If the existing adapter retries other transport failures, distinguish its
attempts from child starts. There is no pooled billing balance; per-child
bounds don't cap aggregate spend. If a per-child limit can't be enforced,
narrow the supported path or report the limitation; never present unavailable
usage as zero. No billing integration.

## Implementation order

| Slice | Work and exit condition |
| --- | --- |
| A. Offline jobs and wait | Add a small runtime-owned registry and completion/wait path; demonstrate overlapping jobs with scripted providers. Keep synchronous tool ports unchanged. If an ordinary waiter would hold an operation permit needed by a child or block cancellation, use a narrow async wait at the runtime dispatch boundary; don't add a generalized async tool API. Parent run/approval/sequence behavior and the no-delegation path stay unchanged. |
| B. Default read-only composition | Wire only eligible local Web/desktop sessions. Reuse provider selection and tool/path policies; take the minimum of parent deadline and child limits. Validate model/effort without inference and exercise supported protocol fixtures. Integrate multi-job ownership, cancellation, shutdown and idle cleanup before exposing the tools by default. |
| C. Inbox handoff and proposals | Reuse tool cards and the session inbox timing rules. Verify late results wake only an eligible session and that explicit `await` cannot double-deliver. Add `Propose` as bounded data and test stale or malformed replacements; don't introduce a generic patch engine or approval path for child writes. |
| D. Verify readiness | Run offline correctness, concurrency and lightweight checks for each delivered slice. If the gates fail, fix the issue or hold the release; don't add an undocumented cap or silently turn the feature into opt-in. Compare matched real tasks only if live inference is authorized. |

Place ownership and wake logic beside the current runtime execution loop,
provider/model bindings beside the server's existing session composition, and
any read-only tool composition in the existing tools/permissions boundary.
Use the core for shared value types only when an actual crossing needs them;
keep presentation in the Web client. Update the affected
[execution](../design/02-execution.md), [events](../contracts/03-command-events.md),
and [Web](../design/14-web-client.md) docs when behavior is accepted, not merely
because it appears in this plan.

## Validation and release gates

- **Lifecycle:** use deterministic fakes and gates, not sleeps. Verify parent
  progress during overlapping work, out-of-order child completions, completion
  before and during `await`, no lost wakes or duplicate dispatch, identity
  isolation, provider refusal/panic, and bounded output/backpressure. Verify
  parent cancellation and deadline while waiting, late completion after
  parent success, shutdown, and unconfirmed worker retention. A completion
  notification alone must not run the model, and a failed or cancelled
  parent must not be woken by a child.
- **Concurrency:** stress multiple parents with multiple children, within the
  normal parent budgets. Check that no child-specific cap or queue serializes
  the launches, existing parent limits still apply, and cancel reaches all
  outstanding children during a burst. Record peak RSS, cancellation
  responsiveness, rate-limit reporting, and post-run cleanup; don't claim
  arbitrary capacity from the test.
- **Permissions and settings:** reject external/protected paths, namespace
  escape, unsupported model/effort, direct write/exec, recursive calls, and any
  provider parameters outside the configured choices. Check different parent
  and child model/effort settings without mutation or secret leakage. Verify
  stale proposal rejection and preservation of ordinary parent approvals.
- **Frontend and recovery:** no change in ineligible sessions; no unbounded
  polling or replay; reconnect while status is retained; correct provenance
  for an admitted follow-up; unavailable pending results after restart. Preserve
  single-subscriber SSE behavior and ordinary saved history.
- **Lightweight gate:** before and after with the same build and toolchain,
  record release binary size, warm startup, and idle RSS on available
  reference hosts. Default availability without delegation must not start
  child work or send additional requests. Explain any repeatable regression
  before shipping.
  Follow [development tests](../testing.md): affected-package checks
  sequentially, register new server integration modules in `tests/behavior.rs`,
  and run `node --test crates/nexus-server/tests/web_*.mjs` if the UI changes.
- **Usefulness gate:** compare against a single-agent baseline on the same
  repository snapshots and independent tasks (entry-point search, call-chain
  investigation, regression-test discovery, and simple edit proposals). Record
  task correctness, elapsed time, input/output tokens split by parent and
  child, and cancellations. Report missing usage as unknown and distinguish
  estimates from provider usage. Look for a repeatable win in quality,
  latency, or parent-context pressure without an unacceptable rise in
  total cost; if the trade-off is poor, revise the design before shipping.
  No live provider calls or external costs are authorized by this plan.
