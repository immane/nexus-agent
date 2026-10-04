# Overview

Status: Draft design. Confirmed requirements are recorded in the [project foundation decision](decisions/00-project-foundation.md). Nothing in this document claims an implemented feature.

## Confirmed Goals

- Build a coding agent with a Rust core and Rust TUI.
- Make startup as fast as practical; use an interactive startup time of at most 100 ms as the first-release target, then continue reducing it.
- Minimize binary size, memory, idle CPU usage, and local execution overhead using measurements rather than unsupported numerical claims.
- Support Linux and macOS in the first release. Windows is deferred.
- Integrate models broadly and allow extensions at explicit boundaries.
- Support built-in Rust tool modules and cross-language external plugins.
- Design isolation for untrusted plugins separately; do not equate subprocesses with a sandbox.

## First-Release Scope

The proposed baseline is one active agent run in the host process, streaming model output, bounded tool execution, explicit authorization, cancellation, and recoverable session history. The same runtime must be usable without a terminal UI.

The initial coding distribution should register a small set of tools for reading files, searching, applying patches, and running commands. These are tool implementations, not special cases embedded in the core loop.

Provider integration should cover mainstream protocol families. A configurable endpoint or model name is not proof of compatibility: each adapter must identify supported features and pass relevant tests.

## Non-Goals for the Initial Core

- A general-purpose multi-agent framework or arbitrary lifecycle-hook system.
- A vector database, always-running indexing service, or internal HTTP server without a concrete requirement.
- Native dynamic-library loading or an embedded WASM runtime.
- Guaranteed safety for untrusted plugins without an implemented isolation backend.
- Identical support for every model feature or all third-party compatible services.
- Guaranteed startup time on arbitrary hardware, filesystems, or terminal environments.

These are scope boundaries, not permanent prohibitions. Add capabilities through an explicit decision and preserve the small default path.

## Architectural Strategy

Use a modular host process with domain contracts, runtime orchestration, concrete adapters, and presentation. External plugins may run as child processes. Do not introduce IPC between built-in components solely for architectural separation.

Keep the default path short. Built-in tools use direct calls; external tools use a transport adapter; provider-specific details stay in model adapters. Optional capabilities should not initialize or start background work until needed.

Extensibility means replaceable components with documented ownership and invariants, not unrestricted access to mutable runtime internals. Workflow and context-policy customization must preserve authorization, resource limits, cancellation, and truthful outcomes.

## Acceptance Boundaries

A useful first release must complete a model/tool/model cycle, handle interruption without blindly replaying effects, restore terminal state, and run on both supported platforms. Performance acceptance uses the [measurement design](05-performance.md); documentation alone does not satisfy these checks.

## Related Documents

- [Components](01-components.md)
- [Execution](02-execution.md)
- [Security](06-security.md)
- [Common contract](../contracts/00-common.md)
