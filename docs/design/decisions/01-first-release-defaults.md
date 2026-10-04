# Decision 01: First-Release Defaults

Status: Accepted product requirements. Detailed contracts remain drafts; no implementation or performance result is claimed.

Source: choices explicitly confirmed by the project owner after the [project foundation decision](00-project-foundation.md).

## Decisions

| Area | Accepted requirement |
| --- | --- |
| Tool permissions | Approved project-scoped reads and searches run automatically; file mutations and command execution require confirmation. |
| TUI | Use the official Grok Build full-screen interaction style as a reference, with an independently implemented lightweight Rust frontend. |
| Sessions | Automatically save locally outside the project; start a new conversation by default and restore history only on explicit selection. |
| Entry points | Ship both the TUI and a headless entry point using the same runtime and policy boundary. |
| Authentication | Prioritize API configuration with external credential references; browser login/OAuth is not first-release scope. |
| API relay | Treat relay integration as a provider plugin. First support existing relay services; retain a boundary for optional local relay plugins. |
| Code simplicity | Keep normal paths direct, avoid redundant checks and speculative branches, and preserve necessary safety checks at explicit boundaries. |

## Consequences

Automatic reads remain subject to scope and host restrictions. Commands require confirmation even when they appear read-only; a working directory is not an OS permission boundary. Headless calls requiring confirmation are denied when no explicit approval handler is configured.

Automatic session writes are host-managed persistence under the configured storage policy, not model-directed permission to modify arbitrary files. Restoring history does not replay operations, restart interrupted work, or reinstate old call approvals.

Reference Grok Build's conversation scrollback, fixed composer, compact status, and expandable tool output. Do not copy its full implementation, dependency closure, or always-approve controls as mandatory features.

Relay plugins receive only credentials explicitly assigned to their profiles and preserve the provider contract. Existing relay services require no local proxy process. Local relay support must remain optional, on demand, and outside the core's startup path.

Code simplicity must not remove input validation, authorization, deadlines, cancellation, resource limits, or uncertainty handling. Prefer validated types and explicit state transitions over repeatedly checking the same immutable facts.

## Alternatives Not Selected

- Automatic file edits or command execution as the default permission policy.
- Inline-only terminal interaction instead of the selected full-screen reference.
- Automatic restoration of the latest session or default ephemeral conversations.
- A TUI-only first release or mandatory account login before interaction.
- A mandatory local relay server or treating relay routing as a model-invoked tool.

## Still Open

Concrete trait signatures, wire formats, CLI flags, keyboard bindings, configuration/storage formats, dependency versions, and numerical resource defaults require implementation-time review. MCP for external tools does not define a cross-language provider-plugin protocol.

## Related Documents

- [TUI design](../07-tui.md)
- [API relay design](../08-api-relay.md)
- [Implementation readiness](../09-implementation-readiness.md)
- [Tool contract](../../contracts/02-tool.md)
- [Session store contract](../../contracts/04-session-store.md)
- [Configuration contract](../../contracts/05-configuration.md)
