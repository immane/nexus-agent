# Security and Trust Boundaries

Status: Draft design. Host authorization is implemented; `host_exec` currently uses the platform sandbox backend when the real-files tools mode is enabled. This is not a general untrusted-plugin boundary.

## Trust Model

The first implementation may run trusted built-in code and external plugins explicitly enabled by the user. Built-in tools run inside the host trust boundary. External plugins normally retain the OS privileges of the launching user, even if they communicate through a narrow protocol.

Repository content, model output, tool results, plugin descriptions, and retrieved data are not authority to grant permissions. They must not change the task, approved resource scope, or host policy through embedded instructions.

## Host Authorization

The runtime owns policy enforcement for every frontend and executor. Check the tool identity, validated arguments, resource scope, effective policy, and approval state before dispatch.

Approvals bind to a run, call, implementation/schema identity, exact arguments, and relevant scope. Denial, expiry, cancellation, configuration revocation, or a changed implementation invalidates the grant. An approval must not silently become permission for a different call.

Classify file mutation, command execution, network access, and other high-impact operations conservatively. Tool annotations are hints; they cannot prove that arbitrary plugin code or shell commands are read-only or idempotent.

## Accepted Default Policy

Real tools default to **development mode**. Reads, listings, searches, writes, patches, and sandboxed argv execution within the configured project and `/tmp` (including the canonical platform process-temp directory) are automatic. File tools require approval for other paths. Exec reads broadly from non-protected locations; its internal reads do not prompt. Writes remain OS-restricted to the project/temp roots plus a declared, approved external `write_dir`. Merely starting a command in the project never grants arbitrary writes. Network remains disabled.

**Strict mode** (`--strict-tools`) preserves project-jailed automatic reads/listings/searches and per-call confirmation for mutations and exec, with narrower exec reads. Scripted demo/test wiring retains its strict policy. Server real tools (`--tools real`) default to development; the server's default fake-tools wiring is unchanged.

External development approvals offer allow once, allow the displayed directory for this session, or deny. Directory grants include read/write access to descendants and can be reused by exec only when its `write_dir` explicitly names an authorized directory. They are runtime-owned, bound to the live notice, limited to 64 directories per session / 128 sessions per runtime, and never persisted or restored. A grant cannot override protected paths or read-only agent mode. Root-wide session grants are not offered. A read-only run never executes writes, patches, or exec, even where development policy would otherwise auto-approve them.

The loopback server includes `session_directory` in approval JSON (`null` when unavailable). `POST /sessions/{session}/runs/{run}/approve` accepts the existing `approval` and `call` identities plus optional `scope`: `once` (default) or `session-directory`. It accepts no client-selected directory; the runtime resolves the offered directory from the pending call. Stale, consumed, cancelled, or changed targets cannot create a directory grant.

Headless mode without an explicitly configured approval handler denies confirmation-required calls rather than waiting indefinitely or switching to automatic approval. Project/temp calls remain automatic in development mode. Restored history never reinstates old grants.

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

### `host_exec` v1 boundary

Real-files tools register `host_exec`; its mandatory backend must pass a startup probe (`sandbox-exec` on macOS, `bwrap` on Linux), otherwise calls are denied without spawning a command. It never falls back to an unsandboxed child, and other filesystem tools remain available. Calls accept a non-empty argv array and do not automatically invoke a shell. Development mode additionally accepts one existing `write_dir`; external access must receive runtime approval before the adapter accepts its canonical scope. A failed command is never automatically replayed with wider permissions. Strict approvals remain bound to exact normalized arguments. The working directory is the configured project root.

Strict mode retains selected system/toolchain reads and project-only writes. Development mode grants broad reads and project/temp writes, denies network, inherits only PATH from the host, clears other environment variables, sets HOME to a nonexistent location, and supplies TMPDIR and PYTHONDONTWRITEBYTECODE. This supports interpreters located outside standard system paths without inheriting provider credentials.

Built-in protected paths are the launching user's `.ssh`, `.aws`, `.gnupg`, `.azure`, `.config/gcloud`, `.config/gh`, `.kube`, `.netrc`, `.npmrc`, and `.pypirc`, plus `.env`, `.env.local`, `.env.production`, `.env.development`, and `.env.test` directly under home/project. Development startup requires an existing absolute host HOME so this guard is not silently omitted. Home aliases and existing symlink targets are also protected; canonical permission paths must be UTF-8. File tools reject these paths and filter protected entries from listings/search traversal. macOS denies read/write access explicitly; Linux masks existing protected directories/files **after** all writable binds. This is a known-path guard, not exhaustive secret discovery: nested/custom credential files, hard-link aliases, other users' credentials, and files created concurrently outside those known paths are not guaranteed protected. Broad exec output can disclose any readable data to the model. Choose strict mode when that read exposure is unacceptable.

Canonical path checks and dispatch revalidation prevent ordinary traversal/symlink scope changes but are not race-free filesystem capabilities. Host-concurrent rename/mount/symlink changes remain a check/use limitation. Session grants do not remove it.

The current executor bounds captured stdout/stderr, checks cancellation and deadlines while polling, and terminates/waits for the sandbox process group. Linux additionally uses a PID namespace and `--die-with-parent`; effects after cancellation or timeout remain `Unknown`, since termination does not establish rollback. Deliberately detached descendants and sandbox escapes are not claimed to be contained. Backend installation/availability is not proof against every OS sandbox escape. Linux `bwrap` behavior must be adversarially validated on a supported Linux host before claiming Linux isolation; unsupported or missing backends fail closed.

On macOS the system `/usr/bin/sandbox-exec` is located independently of PATH. The profile permits reading the root directory itself with `(literal "/")`, needed for process startup; it does not grant recursive root access with `(subpath "/")`. Startup probing uses the same cleared environment and working directory as execution. Denials distinguish a missing backend program, a program that could not be launched, and an unsuccessful initialization probe, without exposing raw probe output or falling back to ordinary execution.

## Required Safety Tests

Test denied and stale approvals, changed tool definitions, malformed calls, output limits, control-sequence handling, scoped file operations, cancellation after effects, and recovery of uncertain operations. Add platform-specific adversarial tests before making isolation claims.

## Related Documents

- [Tool contract](../contracts/02-tool.md)
- [Commands and events](../contracts/03-command-events.md)
- [Session store](../contracts/04-session-store.md)
- [Configuration](../contracts/05-configuration.md)
- [First-release defaults](decisions/01-first-release-defaults.md)
- [API relay](08-api-relay.md)
