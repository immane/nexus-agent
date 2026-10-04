# Decision 00: Project Foundation

Status: Accepted project direction.

Source: requirements confirmed by the project owner during the initial architecture discussion. This decision records scope and priorities, not an implemented architecture or accepted detailed API.

## Context

The project needs a coding agent that starts extremely quickly, uses few resources, remains aggressively optimizable, and can gain general-purpose capabilities through extensions. A large framework or tightly coupled terminal application would make those goals harder to maintain.

## Decision

1. Use Rust for the agent core and terminal UI.
2. Prioritize very fast interactive startup. Use 100 ms as the first-release target and keep improving it.
3. Minimize executable size and memory using measured baselines; no fixed numerical resource budget is accepted yet.
4. Support Linux and macOS in the first release. Defer Windows support without embedding platform assumptions into semantic contracts.
5. Begin with coding-agent behavior and add broader tasks through plugins.
6. Seek broad model integration while keeping provider details outside the core.
7. Provide explicit replacement boundaries for tools, models, workflow/context policies, storage, and frontends; do not allow arbitrary mutation of runtime internals.
8. Support compiled Rust tools and language-independent external plugins as separate execution paths.
9. Treat initially enabled external plugins as trusted code. Design and validate untrusted-plugin isolation separately.

## Consequences

Startup must not wait for model requests or external plugin readiness. Resource measurements must distinguish the host from enabled external process trees.

The runtime must remain independent of presentation and concrete integrations. Extensibility should not create mandatory heavyweight dependencies or transport overhead for built-in calls.

Linux is an acceptance platform, not a later port inferred from macOS tests. Untrusted-plugin safety cannot be claimed before isolation exists.

## Not Decided Here

- Exact crate layout, dependency versions, thread count, or Rust toolchain.
- Final provider and tool trait signatures or command/event representations.
- Specific MCP versions, optional capabilities, or sandbox technology.
- Configuration and persistence wire formats.
- Numerical memory/size budgets, reference hardware, or a finalized benchmark gate.
- Multi-agent scheduling or arbitrary lifecycle hooks.

These choices require review of the relevant draft design or contract. They do not become accepted merely by appearing in a document.

## Related Documents

- [Overview](../00-overview.md)
- [Components](../01-components.md)
- [Performance](../05-performance.md)
- [Security](../06-security.md)
- [Decision 01: First-release defaults](01-first-release-defaults.md)
