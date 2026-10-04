# Components and Dependency Boundaries

Status: Draft design. The components below are logical responsibilities, not fixed crate names or an implemented workspace layout.

## Responsibilities

| Component | Owns | Must not own |
| --- | --- | --- |
| Domain core | Messages, identifiers, state transitions, semantic ports, limit and outcome types | Terminal types, HTTP clients, provider SDK objects, concrete storage, OS APIs |
| Runtime | Agent loop, task lifecycle, authoritative run state, policy enforcement, cancellation, command/event delivery | Rendering, vendor wire formats, concrete tool business logic |
| Adapters | Provider protocols, tool implementations, plugin transports, session I/O, platform operations | Agent scheduling decisions or frontend state |
| Frontend | Input, presentation state, rendering, user-facing approval requests | Direct provider calls, tool execution, permission bypasses |
| Composition root | Local configuration, implementation selection, dependency assembly, startup/shutdown | Business logic or duplicated execution loops |

## Dependency Direction

The arrows below mean code dependencies, not message flow:

```text
frontend --------> runtime public handle ------> domain contracts
runtime --------------------------------------> domain contracts
adapters -------------------------------------> domain contracts
composition root -> frontend, runtime, adapters
```

The runtime receives implementations through ports. It must not import a concrete provider, plugin transport, database, or terminal library. Adapters must not import runtime internals to mutate a run.

A headless frontend and the TUI use the same runtime interface. Replacing a frontend must not create a second authorization or tool-execution path.

Both are first-release entry points. The TUI follows the [Grok Build-style frontend design](07-tui.md); headless mode uses no terminal or additional service and denies confirmation-required calls without an explicit approval handler.

## Ownership

- The runtime owns authoritative run state and accepted conversation state.
- The frontend owns its bounded presentation model, input buffer, scrolling, and layout caches.
- Adapters own connections, subprocess handles, protocol assembly, and their cleanup obligations.
- The session store owns persistence mechanics, not whether an operation may execute.
- Configuration and policy are captured as explicit snapshots for operations; a plugin cannot replace them through output text.

Avoid an application-wide mutable object shared by every component. Prefer messages and narrow handles. Shared immutable data or connection handles are acceptable when ownership and lifetime are clear.

## Extension Boundaries

Provider, tool, and frontend command/event contracts are the first public boundaries. Session storage is a separate port. Workflow and context-selection policies should remain independently testable modules; introduce public replacement interfaces when a concrete extension needs them.

All replacement paths must obey common runtime invariants. Do not expose arbitrary hooks that let extensions skip authorization, inject terminal outcomes, or change another run's state.

API relay belongs to the provider port, not the tool port. A registered provider plugin may reuse a protocol adapter against an existing relay endpoint; future local relay executors stay behind that boundary. MCP tool interoperability does not define the provider-plugin ABI or process protocol.

## Concise Implementation

Keep the normal agent path direct. Parse and validate external data at its boundary, carry explicit validated types, and use clear state transitions, matches, and early returns instead of repeating immutable checks in every layer.

Recheck mutable policy, approvals, cancellation, and deadlines where execution requires it. Do not remove those checks for brevity or hide simple logic behind wrappers, generic pipelines, or macros. Introduce helpers for meaningful ownership or actual reuse, not merely to shorten a function's apparent length.

## Packaging and Dependencies

Begin with cohesive modules. Split crates where it provides meaningful dependency isolation, independent testing, or reuse; do not create a crate per tool or event type.

Optional plugin transports and heavy integrations should be independently selectable in the eventual build. Disabling them must not pull their runtime machinery into the minimal distribution. Shared HTTP and serialization infrastructure should be reused across provider families.

An async runtime, HTTP client, and terminal library will need explicit selection. No dependency versions, thread count, feature graph, or minimum supported Rust version have been accepted yet.

## Review Checks

Check that the core can be tested with fake providers and tools, the runtime can operate without a TUI, and both built-in and external tools pass through the same policy boundary. Review dependency direction as part of code review once a workspace exists.

## Related Documents

- [Provider contract](../contracts/01-provider.md)
- [Tool contract](../contracts/02-tool.md)
- [Commands and events](../contracts/03-command-events.md)
- [Session store](../contracts/04-session-store.md)
- [API relay](08-api-relay.md)
- [Implementation readiness](09-implementation-readiness.md)
