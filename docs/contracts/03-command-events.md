# Commands and Events Contract

Status: Draft contract, revision `draft-1`. This describes an in-process frontend/runtime boundary, not a public transport protocol. TUI/headless scope and manual history restoration are accepted in [Decision 01](../design/decisions/01-first-release-defaults.md).

## Ownership

The runtime owns execution state. Frontends submit typed commands and maintain their own presentation state from typed events. Neither a TUI nor a headless frontend may directly invoke providers/tools or bypass policy.

The baseline uses one active run and one authoritative event consumer. Additional subscribers require an explicit delivery policy; do not replace required delivery with a lossy broadcast by default.

Ship a Grok Build-style full-screen TUI and headless entry point over the same port. Headless mode MUST NOT initialize terminal state, introduce a mandatory service, or auto-approve confirmation-required calls. When no explicit approval handler exists, deny such calls and expose the permission outcome.

Machine-readable result/event output needs a documented wire revision before release. Keep diagnostics separate from that data; do not mix terminal escapes or banners into structured output. These requirements do not select CLI flags or a serialization format yet.

## Commands and Replies

| Command | Required semantics |
| --- | --- |
| `Submit` | Correlated request containing session, input, and selected execution profile; accepts a new run or rejects explicitly |
| `Cancel` | Targets an existing run; stops future dispatch and requests cancellation of active work |
| `Approve` | Targets a live approval and its run/call identity; cannot change the approved arguments |
| `Deny` | Refuses a live approval without executing its call |
| `GetSnapshot` | Requests a bounded consistent view of a known run and its last event sequence |
| `ListSessions` | Explicit bounded/paged history metadata lookup; does not load all conversation contents or execute work |
| `RestoreSession` | Loads selected bounded history into an idle session without replay or old call grants; rejects conflicting active work |

Replies distinguish accepted, rejected, already finalized, stale/unknown target, and busy outcomes as applicable. Every processed command has a correlated response; disconnected clients cannot assume they received it.

`Submit` returns a host-issued `RunId`. `RequestId` is correlation, not a promise of exactly-once processing. A frontend MUST resolve uncertain submission status instead of blindly resubmitting. Repeated approval/cancellation commands cannot cause duplicate tool dispatch.

A second run is rejected as busy in the baseline. Changing this behavior requires a bounded scheduling contract, not an implicit submission queue.

Start a new conversation by default. History listing/restoration are explicit session-level operations with correlated replies, not synthetic agent runs. A restore reply identifies the selected session/revision and bounded presentation state; any incomplete retained view is labeled. A subsequent submission creates a new `RunId`, not a revival of the interrupted one.

## Event Envelope

The current server's `tool-started` JSON detail includes `call`, `tool` (name),
`revision` (tool revision), and `args_preview` (safe preview or null). The Rust
`ToolStartedInfo` requires a tool identity and an optional preview, validated at
publication. These display fields convey no approval or dispatch authority.

Each run event contains `SessionId`, `RunId`, a runtime-assigned increasing sequence number, and its typed payload. Turn/call/approval identities are included when relevant. Assign sequence numbers after text batching so published events remain contiguous.

The runtime is the sole publisher of authoritative ordering; adapters report through it. Replies and events may arrive through different channels, so frontends MUST NOT assume cross-channel arrival order. `RunStarted` includes the originating request identity for reconciliation.

## Event Payloads

| Event | Meaning |
| --- | --- |
| `RunStarted` | Accepted run identity and effective profile summary |
| `AssistantTextDelta` | Ordered presentation fragment for an identified turn/item |
| `ToolCallPreview` | Optional, non-executable progress for a proposed call |
| `ApprovalRequired` | Exact approval identity, safe action summary, and expiry |
| `ToolStarted` | An authorized admitted call has entered execution; carries call/tool identity and an optional bounded, policy-redacted argument preview |
| `ToolOutput` | Optional bounded progress, with explicit truncation state |
| `ToolFinished` | Actual outcome and effect/evidence summary |
| `UsageUpdated` | Available usage information, labeled provisional or final |
| `RunFinished` | Terminal outcome and persistence state |

Terminal run outcomes distinguish completed, refused, failed, cancelled, and limit reached. Completion describes the runtime lifecycle, not proof of every requested business outcome. Required persistence failure must be reported without losing already observed tool effects.

## Ordering and Lifecycle

Within a live runtime, an accepted run has one `RunStarted` and one terminal `RunFinished`. No new run events follow its terminal event. Every `ToolStarted` must have an eventual outcome while the owner remains alive, including unknown effects when termination cannot be established.

Do not fabricate these events after an abrupt process death; recovery reports interruption separately. Frontends MUST reject stale-run updates rather than applying them to the current run.

Approval requests precede their grant/denial handling and cannot be reused after resolution. Tool previews are not `ToolStarted` and never imply execution.

## Backpressure and Disconnection

Bound event traffic by count and memory. Adjacent text deltas for the same item may be combined without losing content or crossing lifecycle boundaries. Required approvals, outcomes, and terminal events must not be silently dropped.

Cancellation and approval responses need a bounded control path that remains responsive when output is saturated. Event delivery must not block control handling indefinitely.

Define bounded stall/disconnect handling: stop new dispatch, cancel or reconcile active work according to policy, and retain a bounded snapshot. A required event cannot be guaranteed delivered to a disconnected consumer. Headless operation must explicitly install a consumer or deliberate sink, not accumulate unconsumed events.

Snapshots identify their last published sequence, pending approvals, lifecycle, known tool outcomes, and whether retained presentation content is truncated. They are not full replay logs or automatic permission to resume an interrupted operation.

## Required Tests

Cover causal reconciliation of replies/events, stale runs, duplicate approvals, busy submission, finalization, slow consumers, bounded buffers, text batching, prompt cancellation under load, and disconnect while a tool is active. Also cover bounded manual history restoration without replay/grants, rejection during conflicting work, structured-output separation, and headless approval denial.

## Related Documents

- [Execution design](../design/02-execution.md)
- [Performance design](../design/05-performance.md)
- [Tool](02-tool.md)
- [Session store](04-session-store.md)
- [TUI and headless design](../design/07-tui.md)
