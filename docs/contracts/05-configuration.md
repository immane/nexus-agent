# Configuration Contract

Status: Draft contract, revision `draft-0`. No configuration parser, schema, environment-variable naming convention, or file format has been selected.

## Effective Configuration

Represent configuration with explicit typed profiles, not an unrestricted runtime map. Relevant groups are:

- Provider profiles: protocol adapter, endpoint, model, secret reference, known capabilities, conservative overrides.
- Tool registrations: enabled identities/revisions, approved resource scope, effect policy, and limits.
- Plugin launch profiles: executable/arguments, permitted environment, protocol selection, process limits.
- Runtime policy: turn/call/concurrency limits, deadlines, queue/output budgets, approval handling.
- Session policy: persistent or explicit ephemeral mode, storage location, durability, retention.
- Frontend policy: presentation bounds, refresh behavior, and output sanitization.

The implementation MUST choose one documented representation and revision policy rather than introducing multiple parsers for hypothetical needs.

## Resolution and Validation

The proposed precedence is built-in bounded defaults, selected configuration file, documented environment overrides, then explicit command-line options. Resolve this locally and deterministically, and distinguish absent values from invalid values.

Validate revisions, selected profiles, numeric bounds, references, and incompatible settings before dispatch. Unknown mandatory settings fail explicitly. Deferred secrets or integrations may report not-ready status, but startup must not pretend unsafe or invalid configuration is ready for execution.

Bound configuration size and processing. Do not perform network discovery, package installation, repository-wide scanning, or plugin launch during ordinary configuration loading.

## Credentials

Use secret references such as named environment variables or approved secret-store entries. Resolve credentials only for the selected integration when needed. Never serialize secret values into session history, debug dumps, or public errors.

External plugins receive only explicitly allowed environment and credentials. A host provider secret is not implicitly available to every tool. A missing credential should produce a safe actionable failure, not silent fallback to another service.

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

## Related Documents

- [Execution design](../design/02-execution.md)
- [Tool plugin design](../design/04-tool-plugins.md)
- [Performance design](../design/05-performance.md)
- [Security design](../design/06-security.md)
