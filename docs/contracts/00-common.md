# Common Contract

Status: Draft contract, revision `draft-0`. Requirements below are proposed invariants for review before implementation; they are not claims about existing code.

## Contract Vocabulary

`MUST` identifies a correctness or safety requirement. `SHOULD` identifies the default behavior, with deviations requiring a documented reason. `MAY` identifies an optional capability. These terms apply to this proposed contract once accepted.

Type names describe semantic data structures. Concrete Rust signatures, ownership representations, serialization formats, and protocol versions remain to be selected. Internal interfaces do not establish a stable Rust ABI or a public JSON API.

## Identifiers

| Identifier | Scope and owner |
| --- | --- |
| `SessionId` | Host-issued conversation identity, unique in the configured store |
| `RunId` | Runtime-issued execution identity, distinct across runs and persisted records |
| `TurnId` | Runtime-issued model invocation identity within a run |
| `CallId` | Runtime-issued identity for an admitted tool call |
| `ToolId` | Namespaced registered tool identity, associated with an implementation revision |
| `RequestId` | Frontend correlation identity; not an automatic idempotency guarantee |
| `ApprovalId` | Runtime-issued grant request bound to one specific call |

Provider item keys and provider call references are separate from host identifiers. The runtime maps complete call candidates to `CallId` values while preserving the original reference for adapter round trips.

Identifiers MUST be bounded and validated. Never use them directly as filesystem paths or infer permissions from their names. Events and grants MUST identify their owning run; stale identities cannot target a newer run.

## Domain Data

Use explicit message, content, tool-call, tool-result, capability, and outcome types. Relevant content includes text, tool calls/results, and adapter-scoped continuation data. The core MUST NOT depend on provider SDK objects, terminal widgets, concrete database types, or platform handles.

Dynamic JSON is allowed at validated tool-argument/schema boundaries. Opaque bytes are allowed for bounded provider continuation state. Neither becomes an unrestricted map for the entire domain model.

Content records SHOULD preserve source identity and whether the content is complete. Partial streaming output MUST NOT be silently stored as a completed assistant turn. External content does not gain instruction authority by being placed in a domain record.

## Errors and Outcomes

`AgentError` carries a typed category, a safe message, optional bounded correlation data, and retry guidance. Relevant categories include invalid input, unsupported capability, authentication, permission denial, rate limiting, protocol failure, timeout, cancellation, resource limit, tool failure, storage failure, uncertain outcome, and internal failure.

Diagnostics containing secrets or unrestricted payloads MUST NOT cross the public error boundary. Retry guidance is advisory; host policy and known effect state still govern retries.

Keep these concepts separate:

- Execution status: what finished, failed, or was interrupted.
- Effect state: what changes are known to have happened.
- Evidence: whether an outcome is host-observed, plugin-reported, or uncertain.
- User outcome: whether the intended task has actually been established as successful.

An accepted request or zero exit code is not universal evidence of task success.

## Limits and Time

The effective policy MUST bound model turns, tool calls, run duration, context, stream assembly, tool output, event buffers, pending work, and plugin processes where applicable. Limit exhaustion produces an explicit outcome, not unlimited fallback.

Numeric defaults are pending measurement and MUST be selected before implementation ships. An omitted configuration field cannot imply infinity. Use a monotonic clock for elapsed-time decisions; wall-clock timestamps are for records, not timeout arithmetic.

## Compatibility

Accepted contracts, stored formats, and external protocols MUST have explicit revision policies. Reject unsupported major formats rather than guessing. Tolerating unknown fields is acceptable only when mandatory semantics and validation are preserved.

Breaking semantic changes require coordinated updates to contracts, implementations, fixtures, and relevant design documents. A draft sketch must not be advertised as a stable plugin API.

## Validation Obligations

Test identity scoping, input bounds, complete versus partial records, error sanitization, limit exhaustion, and incompatible revisions. Use deterministic local doubles; do not require credentials to validate domain behavior.

## Related Documents

- [Provider](01-provider.md)
- [Tool](02-tool.md)
- [Commands and events](03-command-events.md)
- [Session store](04-session-store.md)
- [Configuration](05-configuration.md)
