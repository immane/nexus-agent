# Tool Contract

Status: Draft contract, revision `draft-1`. Read the [common contract](00-common.md) first. The default permission policy is accepted in [Decision 01](../design/decisions/01-first-release-defaults.md); remaining mechanics are proposed for both built-in and external executors.

## Semantic Operations

- `describe()`: return an immutable registered `ToolSpec` for the current implementation revision.
- `execute(call, context)`: execute one admitted call and return its actual `ToolOutcome`, optionally emitting bounded progress.

Registration is not permission to execute. The runtime controls resolution, validation, approval, dispatch, and outcome recording. A tool MUST NOT directly mutate the conversation, schedule another run, or grant itself capabilities.

## Descriptor and Call

`ToolSpec` identifies a namespaced tool, implementation revision, description, object-root input schema, optional output schema, and declared requirements. The host associates it with authoritative permissions, limits, and conservative effect classifications.

Input schemas use a declared JSON Schema dialect. The initial intended dialect is 2020-12; the validator and supported feature set require review before implementation. Unsupported validation features MUST cause registration failure rather than being silently ignored. Remote reference fetching is disabled; local reference processing must be bounded and explicitly supported.

`ToolCall` identifies the run, turn, host call identity, resolved tool revision, and immutable validated arguments. Stream fragments, unknown tools, invalid arguments, or unresolved schema support MUST NOT reach an executor.

## Execution Context

`ToolContext` supplies effective limits, cancellation/deadline information, approved resource scope, and host-issued access handles where applicable. It MUST NOT include unrestricted access to runtime state or unrelated credentials.

A narrow context does not remove the ambient OS privileges of trusted native code or a normal subprocess. Claims of enforced confinement require the separate isolation design.

## Authorization and Dispatch

Check authorization against the actual tool identity, revision, normalized arguments, current resource scope, and effective policy immediately before dispatch. An approval binds these values plus its run, call, expiry, and relevant policy revision.

The default policy automatically permits only host-authorized project reads/searches. Model-directed file mutations and all command execution require confirmation, even for commands described as read-only. Unknown or externally declared effect classifications do not independently earn automatic permission.

Without an explicitly configured approval handler, a headless confirmation-required call MUST be denied with not-started effects. It cannot wait for nonexistent terminal input, silently enable automatic approval, or execute via another adapter.

Host-managed session writes under the configured storage policy are not model-directed tool mutations. They grant no broader filesystem permission to the model or plugins.

Changed arguments, tool definitions, revocation, expiry, or run cancellation invalidate the approval. An already consumed approval cannot dispatch the call again. External read-only/idempotent annotations are not authoritative security evidence.

Carry validated immutable arguments into the executor without redundant structural checking. After approval or persistence waits, recheck mutable authorization/cancellation/deadline conditions at the actual dispatch boundary. A shorter code path MUST NOT bypass those checks.

In persistent mode, side-effecting dispatch MUST meet the [session-store intent requirements](04-session-store.md). If required validation, authorization, or intent recording fails, do not execute.

## Outcomes and Effects

`ToolOutcome` contains execution status, bounded content, effect state/evidence, relevant execution metadata, and an explicit truncation indication when applicable.

Execution statuses include succeeded, failed, denied, cancelled, and timed out. Effect states distinguish not started, known not applied, known applied, and unknown. Evidence distinguishes host observations from plugin reports.

A successful command exit means the command met its execution convention; it does not prove the user's intended task. An interrupted plugin can have unknown effects even when no result arrived. The runtime MUST preserve the original outcome rather than replacing it with a fabricated success or rollback.

## Output

Text, structured content, and resource references MUST have defined bounds and supported representations. Invalid structured output must be identified rather than silently accepted as matching its schema.

Progress and final output share the operation's resource budget. Truncation must preserve a clear indication that content is incomplete. Optional file-backed output needs separate storage limits and authorized access; do not move unlimited output from memory to disk.

Returned resources are references, not authorization to read a file or fetch a URL. Unsupported content variants fail explicitly or use a documented, deliberate conversion.

## Cancellation and Retry

Cancellation stops new dispatch and requests termination of active work. Executors MUST describe their cancellation limits. Record successful effects that raced with cancellation; do not rewrite them as not applied.

Unknown effects forbid blind retry. A caller timeout does not establish that an external process stopped. Do not reuse affected resources unsafely while work may remain active.

## Initial Coding Tool Requirements

- Reads/searches operate within approved scope and bounded results.
- Patch application validates targets and expected content; mismatch must not trigger an unrelated overwrite fallback.
- Command execution has explicit authorization, environment/scope handling, deadlines, exit metadata, and bounded stdout/stderr.
- Errors and previews do not leak secrets or emit uncontrolled terminal sequences.

## Required Tests

Cover registration/schema failure, incomplete calls, automatic scoped reads/searches, confirmation for mutations and apparently read-only commands, headless denial without a handler, denied/stale/consumed approvals, changed arguments, scope enforcement, output limits, cleanup, timeout with uncertain effects, and prevention of blind replay.

## Related Documents

- [Tool plugin design](../design/04-tool-plugins.md)
- [Security design](../design/06-security.md)
- [Commands and events](03-command-events.md)
