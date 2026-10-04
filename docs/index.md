# Nexus Agent Documentation

Nexus Agent is a Rust coding agent designed around very fast startup, low resource usage, and replaceable integrations. General-purpose capabilities can be added through plugins without turning the core into a large framework.

## Implementation Status

The repository currently contains engineering guidance, the [MIT license](../LICENSE), and this documentation foundation. No Rust workspace, agent runtime, provider adapter, tool executor, or TUI has been implemented. There are no measured performance results or verified provider/platform compatibility claims yet.

## Document Status and Authority

- **Accepted project direction** records requirements confirmed by the project owner, not implemented behavior.
- **Draft design** proposes how to satisfy those requirements.
- **Draft contract** proposes interfaces and invariants to review before implementation. Type names describe semantics, not an existing Rust API, ABI, or wire format.
- **Implemented behavior** must be identified with its implementation and tested scope when it exists.

Follow explicit user requirements and applicable repository guidance. Accepted decisions and contracts take precedence over draft proposals. A newer draft does not silently supersede an accepted decision. Update related documents together when an authorized change affects their assumptions.

## Design

| Document | Purpose | Status |
| --- | --- | --- |
| [00 - Overview](design/00-overview.md) | Goals, scope, and first-release boundaries | Draft design; confirmed requirements identified |
| [01 - Components](design/01-components.md) | Responsibilities, dependency direction, and extension boundaries | Draft design |
| [02 - Execution](design/02-execution.md) | Startup, agent loop, state ownership, and shutdown | Draft design |
| [03 - Model integration](design/03-model-integration.md) | Broad provider coverage without vendor SDK proliferation | Draft design |
| [04 - Tool plugins](design/04-tool-plugins.md) | Built-in Rust tools and cross-language external plugins | Draft design |
| [05 - Performance](design/05-performance.md) | Startup acceptance, resource measurement, and optimization | Draft design; startup goal confirmed |
| [06 - Security](design/06-security.md) | Trust, authorization, side effects, and separate isolation work | Draft design |

## Contracts

| Document | Purpose | Status |
| --- | --- | --- |
| [00 - Common](contracts/00-common.md) | Shared identifiers, data ownership, errors, limits, and compatibility | Draft contract |
| [01 - Provider](contracts/01-provider.md) | Requests, normalized streaming events, and continuation correctness | Draft contract |
| [02 - Tool](contracts/02-tool.md) | Tool registration, execution, authorization, and outcomes | Draft contract |
| [03 - Commands and events](contracts/03-command-events.md) | Frontend/runtime interaction, ordering, and backpressure | Draft contract |
| [04 - Session store](contracts/04-session-store.md) | Persistence, durability, interrupted operations, and recovery | Draft contract |
| [05 - Configuration](contracts/05-configuration.md) | Configuration snapshots, credentials, permissions, and bounded defaults | Draft contract |

## Decisions

- [00 - Project foundation](design/decisions/00-project-foundation.md): accepted project direction.

## Reading Order

1. Read the overview and project foundation for confirmed scope.
2. Read component and execution designs for boundaries and lifecycle.
3. Read common contracts before the relevant component contract.
4. Read security and performance requirements before introducing integrations or dependencies.

## Before Implementation

Review and accept the relevant draft contracts. Select the Rust toolchain, dependency features, schema-validation support, and external protocol versions explicitly. Establish Linux and macOS reference environments for performance measurements. Do not add placeholder crates, runbooks, or validators just because a document mentions a future component.
