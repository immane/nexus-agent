# Configuration Contract

Status: Implemented subset, revision `1` for operational settings. Provider/model configuration remains a separate revisioned document.

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

Operational settings resolve as profile built-ins, optional `/etc/nexus-agent/base.jsonc` (or `NEXUS_BASE_CONFIG`), then ordered repeatable `--config-overlay` files. `--base-config` replaces the base path. Shared profiles are only `tui` and `server`. A file such as `benchmark.jsonc` is an ordinary settings file with the same schema and can be passed through the existing base/overlay options; there is no benchmark-specific path, option, or environment variable. These settings are separate from the provider/model document and are never written back by the TUI. UserConfig remains strict JSON; its migration/save behavior is unchanged.

Revision-1 operational JSONC is a JSON object with `"revision": 1` and optional `defaults`, `profiles`, and `tui` objects. `defaults` and each `profiles.tui` / `profiles.server` object may contain `runtime`, `provider`, `server`, `tools`, `launch`, and `tui` objects. Each layer is partial and omitted fields inherit preceding values, including nested frontend/theme fields. The same shape applies to every file name, including the checked-in [`config/benchmark.jsonc`](../../config/benchmark.jsonc) example; filename extensions do not select a parser. Plain operational JSON remains valid. Existing operational TOML must be converted to JSONC; operational settings have no TOML dependency.

JSONC accepts only JSON plus `//` line comments and `/* */` block comments. Comment recognition is string-safe: comment markers inside JSON strings remain literal content, including escaped quotes and backslashes. Trailing commas and other JSON5 extensions are rejected. For example, a partial layer is:

```jsonc
{
  "revision": 1,
  "defaults": {
    "runtime": {
      "max_tool_output_bytes": 131072
    }
  },
  "profiles": {
    "tui": {
      "runtime": {
        // Override only the TUI turn budget.
        "max_model_turns_per_run": 96
      }
    }
  }
}
```

Top-level `tui` is also supported for shared presentation settings. Within a layer, defaults apply first, then top-level TUI settings, then the selected profile. `tui.theme`, `tui.display`, `tui.input`, and `tui.render` objects are deeply merged with preceding settings before typed deserialization and validation. The checked-in [`config/base.jsonc`](../../config/base.jsonc) lists all default presentation fields.

Every `Limits` field can be overridden: `max_model_turns_per_run`, `max_tool_calls_per_run`, `max_tool_calls_per_turn`, `run_timeout_seconds`, `tool_timeout_seconds`, `retained_context_items`, `max_arg_assembly_bytes`, `max_tool_output_bytes`, `event_data_capacity`, `event_control_capacity`, `max_concurrent_ops`, and `approval_expiry_seconds`. Provider settings include `connect_timeout_seconds` (10), `context_discovery_timeout_seconds` (2), `fallback_call_timeout_seconds` (600), and `retry_backoff_seconds` ([1, 2, 5, 10, 30, 60]). All provider deadlines must be between 1 and 86,400 seconds, matching adapter validation. Retry arrays contain 1–8 nondecreasing delays, each between 1 and 3,600 seconds.

Server lifecycle settings use `defaults.server` or `profiles.server.server`: `max_connections` defaults to 64 (range 1–64), `max_sessions` to 128 (1–128), `session_idle_seconds` to 1800, and `session_cleanup_interval_seconds` to 60 (durations 1–86,400 seconds). Admission caps may be reduced but cannot exceed the existing hard ceilings. Pending session reservations count toward admission. Settings are installed before session creation and are not hot-reloaded. Idle expiry is rechecked during removal; active work, streams, external owners, and concurrent activity still prevent eviction. Eligible sessions are removed on a subsequent periodic cleanup pass, not exactly at the expiry instant. Bind address, authentication, and tool policy are unchanged.

Tool budgets use `defaults.tools` or `profiles.tui.tools` / `profiles.server.tools`. `list_entry_limit` defaults to 10,000 (range 1–10,000); `search_file_bytes` defaults to 1,048,576 bytes per file (range 1–1,048,576). These are startup snapshots, not model-call arguments. Strict, development, and container file tools apply the same reductions without changing scope, protected paths, or approval policy. Listing beyond its entry budget reports truncation; enumeration precedes sorting, so no sorted-prefix guarantee is made. Search beyond its file budget reports truncation and omits an incomplete trailing UTF-8 character while retaining the valid prefix. Worker protocol version 2 carries mandatory validated budgets; stale workload images must be rebuilt and fail explicitly instead of ignoring reductions. See [container compatibility](../../containers/README.md#worker-protocol-compatibility). Other tool and container settings remain outside this schema.

Launch defaults use `defaults.launch` or the selected profile’s `launch`: `execution` (`local`/`container`), `strict_tools`, `tools_root`, `server_port`, `server_tools` (`fake`/`real`), `session_store`, and `auth_token_env` (environment-variable name only). Explicit CLI options take precedence. A missing field or JSON `null` is treated as omitted and inherits the preceding layer; `null` does not clear an inherited value. `server_port` is a `u16` (0–65535); `server_tools`, `server_port`, `session_store`, and `auth_token_env` affect only the server, while `execution`, `strict_tools`, and `tools_root` are consumed by the frontends that support them. Relative `tools_root` and `session_store` paths are interpreted from the process working directory. Path strings must be nonempty, at most 4096 bytes, and contain no NUL; environment-variable names must be 1–128 ASCII letters, digits, or underscores. Container image references remain in the existing user configuration or CLI and retain their existing startup validation. Launch values are startup defaults; strict mode can only be tightened by configuration, and authorization/workspace validation remains in the existing execution paths. Configuration stores no bearer-token value.

Both frontends accept `--check-config` and `--print-effective-config`. Effective output is plain JSON with `runtime`, `provider`, `server`, `tools`, `launch`, and `tui` objects, without comments. This diagnostic output is the resolved operational-configuration snapshot, not the final startup arguments after CLI overrides or user-config image selection. Unknown keys, duplicate keys, unsupported revisions, malformed files, and invalid values (including inactive profiles) fail explicitly. Files are limited to 65,536 bytes, enforced during bounded reads before parsing. An absent conventional system base is allowed; an explicitly configured base or overlay that cannot be read is an error.

Validate revisions, selected profiles, numeric bounds, references, and incompatible settings before dispatch. Unknown mandatory settings fail explicitly. Deferred secrets or integrations may report not-ready status, but startup must not pretend unsafe or invalid configuration is ready for execution.

Bound configuration size and processing. Do not perform network discovery, package installation, repository-wide scanning, or plugin launch during ordinary configuration loading.

## Credentials

Use secret references such as named environment variables or approved secret-store entries. Resolve credentials only for the selected integration when needed. Never serialize secret values into session history, debug dumps, or public errors.

External plugins receive only explicitly allowed environment and credentials. A host provider secret is not implicitly available to every tool. A missing credential should produce a safe actionable failure, not silent fallback to another service.

A relay receives only the credential reference assigned to its profile. Do not embed secret values in stored endpoints or forward another provider's key automatically. Endpoint/routing changes require destination, compatibility, and credential-scope validation. No network authentication, relay probing, or local-relay launch belongs in ordinary startup configuration loading.

## Limits

The implemented operational settings cover all runtime `Limits` fields listed above, provider deadlines/retry backoff, server admission/lifecycle settings, tool list/search budgets, and typed frontend presentation settings. Omitted values retain their code-defined profile defaults.

User overrides remain subject to host validation and policy. Zero, negative, overflowing, or incompatible values require defined semantics; they cannot accidentally disable a safety limit. Large legitimate workloads may use explicitly increased budgets without changing global defaults.

## Snapshots and Changes

Capture a consistent effective configuration for each run and approval. Treat tool identity/schema and relevant policy revisions as part of approval validity.

Configuration changes MUST NOT silently alter an active call's arguments or capabilities. Revocation takes effect before subsequent dispatch, and affected pending approvals are invalidated. Replacing a plugin implementation or widening scope requires explicit revalidation/authorization.

A frontend or plugin cannot change host policy by emitting content. Loading an old session does not restore old grants automatically.

### Current user-config save behavior

The current TUI owns one mutable user-config document per process and serializes its recent-model updates and saves through that owner. Saves use atomic replacement, but this is not a cross-process merge or lock: if multiple processes update the same file, the last successful rename wins and an earlier process's changes may be lost. This policy does not promise reconciliation with edits made by another process while the TUI is running.

The shared revision-2 user config may also contain `container_images`, an object
with optional `gateway` and `workload` local Docker image references. References
are validated as bounded nonempty strings, not as immutable digests, because
local tags are resolved to Docker image IDs at container startup. CLI image
flags override their respective config fields. Startup only inspects local
images; it never pulls or builds them.

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
