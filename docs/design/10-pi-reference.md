# Pi Architecture Reference

Status: Draft reference note. It records architectural patterns observed in Pi that confirm or constrain our design. It introduces no new product requirements and claims no implementation.

## Subject

Pi by Earendil (`earendil-works/pi`, [pi.dev](https://pi.dev)) is a minimal TypeScript agent harness organized as npm packages: agent core, unified LLM API (`pi-ai`), coding agent, terminal UI, and web UI. Observations below were reviewed in October 2026 against the public repository and documentation; upstream evolves, so recheck before relying on any detail.

Relevant sources: [how Pi works](https://github.com/earendil-works/pi/tree/main/packages/coding-agent/docs/how-pi-works.md), [extensions](https://github.com/earendil-works/pi/tree/main/packages/coding-agent/docs/extensions.md), [sessions](https://github.com/earendil-works/pi/tree/main/packages/coding-agent/docs/sessions.md), [RPC](https://github.com/earendil-works/pi/tree/main/packages/coding-agent/docs/rpc.md), and [SDK](https://github.com/earendil-works/pi/tree/main/packages/coding-agent/docs/sdk.md).

## Patterns That Confirm Our Design

1. **Zero-persistence core with a subscribed session layer.** Pi's core `Agent` owns transcript, tools, streaming, and event lifecycle with no storage references; `AgentSession` subscribes to agent events and layers persistence, compaction, and branching on top. This matches our domain core / runtime / adapter split: the core stays portable and persistence stays in an adapter behind a port.
2. **Narrow hooks instead of a framework.** Pi's effective extension points are small and named: `beforeToolCall` / `afterToolCall`, `transformContext`, `shouldStopAfterTurn`, and `convertToLlm`. Our workflow and context policies should stay as independently testable modules with the same shape, not grow into arbitrary lifecycle hooks.
3. **Context shaping separated from protocol conversion.** Pi's `transformContext` prunes or compacts messages before `convertToLlm` maps them to provider types. Our context-selection policy and provider adapters keep the same boundary: reduction logic never silently drops tool-call associations or continuation state.
4. **Multiple entries over one runtime.** Pi ships interactive, print/JSON, RPC over stdin/stdout, and SDK modes against the same agent. This supports our TUI plus headless decision, including Pi's RPC practice of streaming events with backpressure handling before reading the next command.
5. **Unified provider layer with dynamic credentials.** Pi's `pi-ai` normalizes providers behind one API, resolves keys dynamically via `getApiKey`, and carries a session identifier for caching. This supports our protocol-adapter approach with external credential references and no network work at startup.
6. **In-process extensions alongside MCP.** Pi keeps fast TypeScript-module extensions and MCP integrations side by side. This matches our built-in Rust tools versus MCP stdio split: built-in calls stay direct and never serialize through the external protocol.

## Explicitly Not Adopted

- **Broad extension API.** Pi extensions can reach tools, commands, shortcuts, events, and the full TUI. We keep narrow ports; extensions must not skip authorization, inject terminal outcomes, or mutate another run's state.
- **No permission popups by default.** Pi documents running in a container instead of prompting. This conflicts with our accepted default: scoped reads are automatic, while file mutations and command execution require confirmation.
- **Tree-structured sessions.** Pi stores branches, labels, and compaction entries in a single session file with in-place branching. M0 stays with a linear checkpoint plus a bounded journal; tree history is a possible later extension, not a baseline.
- **Baked-in subagents, plan mode, todos, and background shells.** Pi deliberately leaves these to extensions or external tools like tmux. We already defer them for the same reason: keep the core small and let measured needs drive additions.

## Implications

No architecture change follows from this note. When a proposal cites Pi as precedent, check it against this list: adopt the six patterns above in principle, reject the four exclusions, and keep the Rust, startup, and permission constraints from the accepted decisions.

## Related Documents

- [Components](01-components.md)
- [Execution](02-execution.md)
- [Model integration](03-model-integration.md)
- [Tool plugins](04-tool-plugins.md)
- [TUI and headless frontends](07-tui.md)
- [First-release defaults](decisions/01-first-release-defaults.md)
