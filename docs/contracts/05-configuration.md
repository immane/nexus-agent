# Configuration Contract

Status: Draft contract, revision `draft-1`. Product defaults are accepted in [Decision 01](../design/decisions/01-first-release-defaults.md). No configuration parser, schema, environment-variable naming convention, or file format has been selected.

## Effective Configuration

Represent configuration with explicit typed profiles, not an unrestricted runtime map. Relevant groups are:

- Provider profiles: direct or relay implementation, protocol adapter, endpoint, model/mapping, assigned secret reference, known capabilities, conservative overrides.
- Tool registrations: enabled identities/revisions, approved resource scope, effect policy, and limits.
- Plugin launch profiles: executable/arguments, permitted environment, protocol selection, process limits.
- Runtime policy: turn/call/concurrency limits, deadlines, queue/output budgets, approval handling.
- Session policy: automatic local persistence outside the project, explicit restoration, optional explicitly selected ephemeral mode, durability, retention.
- Frontend policy: presentation bounds, refresh behavior, and output sanitization.

The implementation MUST choose one documented representation and revision policy rather than introducing multiple parsers for hypothetical needs.

## Accepted Defaults

- Permit scoped project reads/searches subject to host restrictions; require confirmation for model-directed mutations and all command execution.
- Deny confirmation-required headless calls when no explicit approval handler is configured.
- Save sessions automatically outside the project; begin a new conversation and restore history only on request.
- Provide the full-screen TUI and headless entry point over one runtime.
- Use API profiles and external credential references; browser account login/OAuth is deferred.
- Register existing relay services as provider plugins; local relay execution remains an optional later boundary and never mandatory startup work.

These are product defaults, not names of implemented enum variants, config keys, or CLI switches. Loading history must not reapply old grants or replace the current credential/profile policy implicitly.

## Resolution and Validation

The proposed precedence is built-in bounded defaults, selected configuration file, documented environment overrides, then explicit command-line options. Resolve this locally and deterministically, and distinguish absent values from invalid values.

Validate revisions, selected profiles, numeric bounds, references, and incompatible settings before dispatch. Unknown mandatory settings fail explicitly. Deferred secrets or integrations may report not-ready status, but startup must not pretend unsafe or invalid configuration is ready for execution.

Bound configuration size and processing. Do not perform network discovery, package installation, repository-wide scanning, or plugin launch during ordinary configuration loading.

## Credentials

Use secret references such as named environment variables or approved secret-store entries. Resolve credentials only for the selected integration when needed. Never serialize secret values into session history, debug dumps, or public errors.

External plugins receive only explicitly allowed environment and credentials. A host provider secret is not implicitly available to every tool. A missing credential should produce a safe actionable failure, not silent fallback to another service.

A relay receives only the credential reference assigned to its profile. Do not embed secret values in stored endpoints or forward another provider's key automatically. Endpoint/routing changes require destination, compatibility, and credential-scope validation. No network authentication, relay probing, or local-relay launch belongs in ordinary startup configuration loading.

## Limits

Before the first implementation ships, document finite defaults for model turns, tool calls, active operations, run duration, context/stream assembly, output, queues, pending requests, retained presentation/history, and plugin processes.

User overrides remain subject to host validation and policy. Zero, negative, overflowing, or incompatible values require defined semantics; they cannot accidentally disable a safety limit. Large legitimate workloads may use explicitly increased budgets without changing global defaults.

## Snapshots and Changes

Capture a consistent effective configuration for each run and approval. Treat tool identity/schema and relevant policy revisions as part of approval validity.

Configuration changes MUST NOT silently alter an active call's arguments or capabilities. Revocation takes effect before subsequent dispatch, and affected pending approvals are invalidated. Replacing a plugin implementation or widening scope requires explicit revalidation/authorization.

A frontend or plugin cannot change host policy by emitting content. Loading an old session does not restore old grants automatically.

## Compatibility and Privacy

Version stored configuration independently from provider and external-plugin protocols. Explain migrations and reject unsupported required formats instead of inferring them.

Safe configuration summaries MUST redact secrets and sensitive launch arguments. Do not send local configuration files or diagnostics to a provider merely to explain a configuration error.

## Required Tests

Cover precedence, invalid bounds, oversized input, missing/deferred secrets, incompatible profiles, revocation, changed tool identities, secret redaction, controlled plugin environments, and offline startup without plugin execution.

Verify accepted defaults, isolated relay credentials, manual-only history restoration, and denial of headless confirmation-required calls without a handler. Concise implementation must retain validation of mutable policy at dispatch rather than spreading repeated immutable checks across adapters.

## Related Documents

- [Execution design](../design/02-execution.md)
- [Tool plugin design](../design/04-tool-plugins.md)
- [Performance design](../design/05-performance.md)
- [Security design](../design/06-security.md)
- [API relay design](../design/08-api-relay.md)
- [TUI and headless design](../design/07-tui.md)
