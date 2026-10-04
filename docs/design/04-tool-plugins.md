# Tool Plugins

Status: Draft design. A plugin is an independently registered capability, not necessarily a dynamically loaded library.

## Two Execution Paths

| Path | Registration and execution | Tradeoff |
| --- | --- | --- |
| Built-in Rust tool | Compiled into the host and registered through the tool port | Direct calls and lowest transport overhead; changing code requires rebuilding |
| External tool plugin | Separately launched program connected through a transport adapter | Cross-language extensibility; additional startup, process, and serialization costs |

Both paths implement the [tool contract](../contracts/02-tool.md). Built-in tools must not serialize their calls through an external protocol just to look like plugins. Pi keeps fast in-process extensions alongside MCP for the same reason; only the transport cost differs, not the policy boundary. See the [Pi reference](10-pi-reference.md).

A Rust trait is an in-process implementation interface, not a cross-language ABI. Python, Go, JavaScript, Rust executables, or other programs can participate if they implement a supported language-neutral protocol and their runtime is available.

This document covers tool plugins. Provider plugins, including API relay integrations, use the separate [provider contract](../contracts/01-provider.md) and [relay design](08-api-relay.md). Do not expose relay routing as an executable tool or force provider streams through MCP tool calls.

## External Transport

MCP over stdio is the preferred initial interoperability candidate. Keep it in an optional adapter, outside the domain contract. Select and document supported MCP versions and capabilities before implementation; the project currently claims no MCP compliance.

MCP stdio uses newline-delimited JSON-RPC messages on stdin/stdout, with logs on stderr. Implement the lifecycle and version/capability rules required by the selected protocol revision rather than assuming all versions share the same handshake.

Bound message sizes, pending requests, timeouts, and stderr retention. Drain pipes without unbounded allocation. Malformed frames, unexpected process exits, and unsupported capabilities need explicit outcomes.

Protocol support does not require every optional MCP capability. Do not silently enable sampling, resource fetching, remote transports, or other host actions merely because a plugin asks for them.

## Registration and Discovery

Use explicit user configuration to enable plugins. Do not scan and execute arbitrary repository files, download dependencies, or start every known plugin during startup.

A registration identifies the implementation, namespaced tool names, schemas, required host permissions, and resource limits. Cached descriptions are hints until the active implementation has been checked. Changed schemas or identities invalidate affected approvals.

Tool descriptions and annotations are untrusted metadata, not authorization or proof of read-only behavior. The host controls grants, available tools, and model-visible definitions.

## Process Lifecycle

Start a plugin on demand. Reuse an active process when useful instead of restarting an interpreter for every call, but bound retained processes and evict idle instances according to explicit policy.

Use a direct executable and argument list for plugin launch rather than shell interpolation. Pass only intended environment variables and credentials. The host owns child handles and handles cancellation, timeout, disconnect, and shutdown.

Restarting a plugin must not automatically replay an uncertain tool call. Recovery of a process and recovery of an operation are separate decisions.

## Initial Coding Tools

The proposed coding distribution registers tools for scoped reading, search, patch application, and command execution. They live outside the core loop and use the same policy and outcome contracts as external tools.

Command execution is a high-impact capability, not a safe escape hatch around missing tool permissions. Large command output must remain bounded and visibly report truncation.

Under the accepted default policy, approved project reads/searches are automatic; mutations and command execution require exact-call confirmation. This applies to both native and external executors. Plugin annotations cannot grant exemptions. A headless consumer without an explicit approval handler receives a denial instead of automatic execution.

## Deferred Mechanisms

Native dynamic-library loading, WASM execution, arbitrary runtime hooks, and automatic plugin installation are deferred. Add them only for a concrete need and after evaluating footprint, compatibility, lifecycle, and trust.

Untrusted plugin isolation is a separate design effort. A subprocess still normally has the user's ambient OS privileges.

## References and Related Documents

- [MCP specification](https://modelcontextprotocol.io/specification)
- [MCP stdio transport](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/stdio)
- [Tool contract](../contracts/02-tool.md)
- [Configuration](../contracts/05-configuration.md)
- [Security](06-security.md)
- [First-release defaults](decisions/01-first-release-defaults.md)
