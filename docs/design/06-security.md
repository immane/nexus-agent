# Security and Trust Boundaries

Status: Draft design. No authorization implementation, sandbox, or isolation guarantee currently exists.

## Trust Model

The first implementation may run trusted built-in code and external plugins explicitly enabled by the user. Built-in tools run inside the host trust boundary. External plugins normally retain the OS privileges of the launching user, even if they communicate through a narrow protocol.

Repository content, model output, tool results, plugin descriptions, and retrieved data are not authority to grant permissions. They must not change the task, approved resource scope, or host policy through embedded instructions.

## Host Authorization

The runtime owns policy enforcement for every frontend and executor. Check the tool identity, validated arguments, resource scope, effective policy, and approval state before dispatch.

Approvals bind to a run, call, implementation/schema identity, exact arguments, and relevant scope. Denial, expiry, cancellation, configuration revocation, or a changed implementation invalidates the grant. An approval must not silently become permission for a different call.

Classify file mutation, command execution, network access, and other high-impact operations conservatively. Tool annotations are hints; they cannot prove that arbitrary plugin code or shell commands are read-only or idempotent.

## Accepted Default Policy

Automatic access is limited to host-permitted project reads and searches. Model-directed file mutations and command execution require confirmation, including apparently read-only shell commands. Existing deny/sensitive-resource restrictions still apply; a tool cannot label itself safe to bypass them.

The TUI presents allow-once or deny for the exact call. Headless mode without an explicitly configured approval handler denies confirmation-required calls rather than waiting indefinitely or switching to automatic approval. Restored history never reinstates old call grants.

Automatic session persistence is a separately configured host operation in local application storage outside the project. It is not blanket permission for a model or plugin to write files. An existing relay profile authorizes only the selected provider destination and its assigned credentials, not new tool network access or silent forwarding elsewhere.

Keep security checks at their actual enforcement boundary. Avoid repeating immutable validation, but recheck grant validity, cancellation, deadlines, and revoked policy where they can change before dispatch. Simplicity does not justify a fail-open path.

## Filesystem and Process Safety

Do not validate filesystem scope using string prefixes alone. Account for traversal, symlinks, nonexistent write targets, and check/use races in the relevant platform adapter. Where a guarantee cannot be enforced, report the limitation and require appropriate authorization.

Launching plugins should use explicit executables and argument arrays with a controlled environment. Do not leak provider credentials into every child process. Shell tools require their own explicit authorization and remain capable of broad effects within the granted trust model.

Cancellation and process-group termination are not guaranteed containment of detached or hostile descendants. Preserve uncertain outcomes instead of declaring rollback or exactly-once execution.

## Privacy and Output Handling

Send only authorized context to the configured provider. Enabling a provider does not authorize reading or uploading every file. Credentials are external secret references, not persisted conversation fields.

The same privacy boundary applies to relay services. Do not silently substitute a relay for another provider or pass unrelated vendor keys to it. Local relay processes, when eventually supported, receive only explicit environment/credential grants.

Redact sensitive data from errors, operational logs, fixtures, and performance artifacts. Apply retention rules to conversation history, tool output, and opaque provider state; these may contain private information even when not human-readable.

Treat terminal control sequences in external output as untrusted. Render or escape content without allowing arbitrary terminal commands, clipboard operations, or control-sequence injection. Restore terminal state on shutdown and failure paths.

Do not automatically dereference tool-returned URLs or resource references. Fetching a resource is a separate authorized operation with its own size and network limits.

## Separate Isolation Work

Before claiming support for untrusted plugins, define and implement an isolation boundary for both Linux and macOS. The separate design must address filesystem, network, process creation, environment/credentials, CPU, memory, descriptors, timeout, and cleanup.

Specify how policies are enforced by the OS, how unsupported restrictions fail closed, and which escapes or platform limitations remain. Do not select a particular sandbox technology merely to fill this document.

Isolation must fit behind executor boundaries rather than adding sandbox APIs to domain messages. Until implemented and tested, the supported trust model remains explicitly trusted code.

## Required Safety Tests

Test denied and stale approvals, changed tool definitions, malformed calls, output limits, control-sequence handling, scoped file operations, cancellation after effects, and recovery of uncertain operations. Add platform-specific adversarial tests before making isolation claims.

## Related Documents

- [Tool contract](../contracts/02-tool.md)
- [Commands and events](../contracts/03-command-events.md)
- [Session store](../contracts/04-session-store.md)
- [Configuration](../contracts/05-configuration.md)
- [First-release defaults](decisions/01-first-release-defaults.md)
- [API relay](08-api-relay.md)
