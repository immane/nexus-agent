# Session inbox: steer, queue, and side questions

Status: ordinary user queue, steering and standalone `/btw` implemented;
Chat Completions also supports an explicit child `delivery: "steer"` at the
originating run's next safe model boundary. Explicit `delegate_await` races
for the same result. Inbox Queue delivery, idle wake and the planned default
Steer behavior are not implemented yet. Omission remains await-only. Other
protocols reject typed replay rather than coercing it into a user message
or unmatched tool response.
The intended inbox handles ordinary user submissions and delivers
[child completion results](12-background-delegation.md). Users will be able
to steer or queue without enabling subagents or session persistence.

### Implemented slice

`POST /sessions/{id}/runs` accepts the opt-in fields `delivery: "queue"`
and `submission_id`. Idle requests can start immediately; busy requests
return `202` and enter the session FIFO. The same submission ID can recover
its acknowledgment while retained. `GET /sessions/{id}/inbox` lists pending
inputs, `DELETE /sessions/{id}/inbox/{submission_id}` removes a pending
item, and `POST /sessions/{id}/inbox/{submission_id}/start` explicitly
starts the head once the session can accept a run. Web Send uses queue
delivery and shows pending items, removal and Resume. Successful runtime
completion drives the next execution after the SSE subscriber releases its
claim. `/btw` invokes `POST /sessions/{id}/side-question` in a live session
bound to a configured model; it opens a separate answer and usage display.
`delivery: "steer"` additionally requires the target `run` ID, and
`user-steered` identifies text accepted at a safe model boundary. Opaque
provider continuation defers the input to the next run rather than being
rewritten. No synthetic child inbox item is exposed in this slice.

## OpenCode reference and Nexus controls

The [OpenCode V2 API](https://opencode.ai/docs/openapi.json)
(`Session.Inbox.Delivery`) defines `steer` as delivery before the next model
call, even mid-work, and `queue` as delivery one item at a time after the
current work finishes. The [TUI guide](https://opencode.ai/docs/cli/tui/)
separately describes `/btw` as a transient side question. Its
[subagent tool](https://opencode.ai/docs/tools/#subagent) returns immediately
and notifies the parent when a child goes idle. Adopt those timing boundaries
and the distinction between user and synthetic input. The child policy
below is a Nexus choice, not a claim about OpenCode's child delivery default.

| Web/desktop action | Effect |
| --- | --- |
| **Send / Enter** | Start a normal run when idle and nothing is pending; otherwise queue the prompt. This preserves the requested Nexus default, unlike OpenCode TUI's default steer. |
| **Steer current task** | Explicit composer action: deliver the message to the active run before its next safe model request. No magic text prefix and no forced interruption. |
| **Queue** | Add the prompt to the end of the session FIFO. A pending prompt can be removed before delivery. |
| **`/btw <question>`** | Ask about the accepted conversation so far using the current model. Show an ephemeral answer separately. This is neither steering nor queueing. |

`Shift+Enter` remains a newline. A pending item isn't added to the normal
transcript or sent to a model until delivered.

## Admission and delivery timing

- Use one in-memory inbox per live server session with typed origin
  (`user` or `child_completion`) and delivery (`steer` or `queue`).
  Keep one active ordinary run per session. Reuse the existing executor,
  session admission, snapshots and SSE; no new crate, daemon, database,
  durable inbox, or scheduler. TUI and headless stay unchanged in this slice.
- The Web client opts in to explicit delivery at the authenticated HTTP
  boundary; callers using the existing submit behavior still get `Busy`
  when appropriate. Decide start vs. queue atomically. Return a run ID for
  a started request and an inbox ID for a pending one. An idle submit must
  not jump ahead of the queued head. Scope submission IDs to the session,
  deduplicate retries with unchanged input and mode, and report an uncertain
  outcome rather than resending under a new ID. Bound ack retention.
- **Steer:** bind user input to the exact active run. Drain accepted steer
  inputs in order before the next model request, after the current model
  stream and admitted tool batch have settled and tool-call/result pairs
  are complete. Don't interrupt generation, execution or approval; don't
  insert into an in-flight request or bypass unfinished approvals. At the
  successful terminal boundary, undelivered user steers become next-run
  inputs in the existing FIFO, ordered by admission time; show the change
  instead of dropping them. If the run fails or is cancelled, keep undelivered
  user steers visible as paused next-run inputs for explicit resume or removal.
  Don't inject stale steer input into a different run.
- **Queue:** wait for a successful terminal run, released stream ownership,
  and no unconfirmed worker blocking admission. Start one head item as a
  fresh run in the same session, then repeat on successful completion.
  For example, B steers before A's next model call; C and D are queued and
  start in that order after A finishes. Use host lifecycle state, not receipt
  of `run-finished` by a browser or a timer. Cancel, timeout, failure, and
  rejected or uncertain starts pause automatic draining. Preserve waiting
  user items in order; **Resume** or remove a rejected head after the issue
  clears. Reconcile uncertain admission before resume; never silently retry.
- Use the existing input limit and an initial per-session allowance of
  16 pending user inputs or 128 KiB of waiting user text, whichever comes
  first, across both delivery modes. Refuse overflow without clearing the
  draft. This is a **user-input memory bound**, not a child-count or
  child-concurrency limit. Revalidate the live session binding and budget
  at delivery; don't offer model or project changes through inbox items.

## `/btw` and child completion

**Side question.** Snapshot accepted context at invocation using existing
context budgeting; exclude queued messages and provisional output. Use the
bound model for one bounded text request on the existing provider path,
without tools, delegation, or main-run continuation state. Don't mutate the
main transcript, provider continuation, approvals, or queue; report usage
separately where available. Reuse the existing request cancellation and UI
primitives. A side question's failure does not cancel the active task.

**Child completion.** A child inherits the delivery policy selected at
`delegate_start` (`steer` by default; `queue` to defer). The host publishes
one synthetic item with job ID, originating parent run, and bounded outcome.
Register summaries by reference to the job record; don't duplicate payloads
in the inbox.

- If the originating parent run is active, `steer` makes its result available
  at the same next-model boundary as user steering. `queue` waits for the
  current work to finish and uses the normal FIFO handoff.
- If the parent finishes successfully before the result is delivered, let
  the inbox wake an idle session for a bounded follow-up, using the same
  ordinary admission and pause rules. If a newer run is already active,
  defer the late result to FIFO rather than steering the wrong task. Show
  that the item is a child result, not a user instruction. A child-only
  follow-up does not launch more children; it cannot become an automatic
  delegation loop. No results from cancelled, failed, or timed-out parents
  may wake the session or be replayed after resume or restart.
- `delegate_await(job_id)` remains an explicit wait. Normal tool-result
  handoff and inbox delivery compete for one atomic claim: if `await`
  consumes the result first, withdraw the inbox item; if already delivered,
  `await` reports that state. A local waiter must be able to consume a
  completed result without waiting for the parent to reach a model boundary.
- Synthetic input is untrusted data, not user/system instructions, a grant,
  or a fabricated tool result. Introduce only the minimal typed context
  boundary needed to preserve that provenance in the transcript and use
  valid provider message shapes. Verify supported adapter fixtures before
  enabling automatic child delivery; fail explicitly if provenance or
  message pairing cannot be preserved. Awaited results retain ordinary
  tool pairing. Visible completion notifications alone don't trigger an
  extra inference outside inbox admission.

## UI, recovery, and delivery slices

1. **Inbox and steering:** keep the composer usable while running and list
   pending user items outside the transcript. Allow removal before delivery.
   Test next-call timing during streaming/tool batches/approvals, the
   terminal race, duplicate admission, and cancelled runs with fake providers.
2. **FIFO and reconnect:** expose pause reason and Resume; reuse a session
   snapshot or small authenticated state query for the current run ID and
   inbox state. Test two clients with a single SSE subscriber, old streams,
   unconfirmed workers, removal races, and consecutive runs. Refresh can
   recover pending items from the same server; session deletion, expiry,
   shutdown and restart discard pending work. Don't persist pending inputs
   or resubmit browser drafts.
3. **Side question:** show a separate answer without editing the transcript.
   Verify no extra tool call, no history mutation, unchanged model binding,
   cancellation and failure independence, and explicit unknown usage.
4. **Child delivery:** use the same timing gates and show the child origin.
   Exercise both modes, out-of-order completions, delivery vs. `await`,
   successful idle wake, old-run isolation, failed-parent suppression, and
   the no-recursion rule. No new model request may race an active run.

Use deterministic fixtures, not live inference. Document accepted HTTP and
Web behavior when implemented. Follow [development testing](../testing.md)
for sequential server checks and Web Node tests.
