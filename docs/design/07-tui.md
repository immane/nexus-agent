# TUI and Headless Frontends

Status: Draft design implementing the accepted [first-release defaults](decisions/01-first-release-defaults.md). No frontend currently exists.

## Reference and Scope

Use the official [Grok Build](https://github.com/xai-org/grok-build) full-screen interaction style as a reference, not the older community Grok CLI. Its [user guide](https://github.com/xai-org/grok-build/blob/main/crates/codegen/xai-grok-pager/docs/user-guide/01-getting-started.md) describes conversation scrollback, a bottom prompt, focus navigation, and expandable entries.

Borrow layout and interaction concepts, not source code, branding, the full dependency closure, or its complete feature set. Multi-agent panels, marketplaces, rich theming, and complex viewers are not prerequisites for a useful first frontend.

## Proposed Layout

| Area | Responsibility |
| --- | --- |
| Compact header | Project/path and available Git context; defer expensive metadata work. |
| Conversation viewport | User messages, streamed responses, tool status, and bounded output with expandable entries. |
| Fixed composer | Multiline input that stays visible while history scrolls. |
| Compact footer | Selected model, execution state, and relevant keyboard hints. |
| On-demand view | Approval details, file content, diffs, and explicitly selected session history. |

Start with a single primary conversation view rather than permanent side panels. Show accurate unknown/not-ready states instead of waiting for network discovery to populate the interface.

Keyboard operation must cover submission, multiline editing, focus, scrolling, folding, approval, and cancellation. Concrete bindings require terminal compatibility tests on Linux and macOS; do not blindly copy contradictory or version-dependent shortcuts from another product. Basic mouse support may complement keyboard operation without becoming a core dependency.

## Approvals

Project-scoped reads/searches are automatic only when host policy permits them. Every model-directed file mutation and command execution requires confirmation under the default policy.

Show the exact operation's safe summary, affected scope, and a diff or command preview where applicable. Offer allow-once and deny without silently enabling global automatic approval. Send the decision through the runtime command interface; the frontend never executes the operation itself.

Cancellation must remain available while output streams or an approval is pending. No shortcut should accidentally approve an operation or change the permission baseline.

## History and Rendering

Start a new conversation by default. Automatic saves use the configured local store outside the project. Listing or selecting previous sessions is explicit and bounded; restoration never reruns old tools or restores their grants.

Batch stream updates, cache layout where useful, and process visible entries rather than reparsing all history. Bound retained text, output, and caches; report presentation truncation separately from accepted conversation records. Render untrusted content without allowing terminal control-sequence injection.

## Headless Entry Point

Use the same runtime, providers, tools, storage policy, and default permissions without initializing the terminal. Accept a task and expose results or machine-readable events; concrete CLI flags and wire representations remain to be specified before release.

Structured output must keep diagnostics separate from event/result data, with no terminal escapes or mixed progress banners. Headless mode is not an automatic approval mode: without an explicitly configured approval handler, a confirmation-required call is denied rather than hanging or silently executing.

Do not add a daemon or internal HTTP service just to expose the runtime. A future remote frontend needs its own explicit transport and authentication design.

## Acceptance Checks

Check offline interactive startup, fixed input visibility, bounded scrollback work, folding, safe previews, stale approval rejection, cancellation under output load, terminal cleanup, and equivalent policy enforcement in headless mode. Restoring history must not initialize plugins or replay operations.

## Related Documents

- [Commands and events](../contracts/03-command-events.md)
- [Execution](02-execution.md)
- [Performance](05-performance.md)
- [Security](06-security.md)
