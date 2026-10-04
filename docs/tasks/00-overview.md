# Phase 1 Tasks Overview

Status: Draft task plan for M0 only. It organizes accepted direction and draft contracts into executable work; it claims no implementation.

## Scope

Phase 1 builds the M0 baseline from [implementation readiness](../design/09-implementation-readiness.md):

- Minimum Cargo workspace, typed core/runtime, fake provider and fake tool.
- One authoritative run lifecycle with explicit completion, failure, cancellation, and limits.
- Scoped automatic reads; confirmation for mutation/command-like fake calls; denial without an approval handler in headless mode.
- Bounded events and output with responsive cancellation and stale-event rejection.
- Offline startup, terminal restoration, clean headless output separation.
- Linux and macOS checks plus startup, size, memory, idle CPU, and dispatch baselines.

Out of scope for Phase 1: real model adapters, relay execution, external plugin processes, persistent session durability, browser login, Windows support, and untrusted-plugin isolation.

## Document Map

| Document | Purpose |
| --- | --- |
| [01 - Pipeline](01-pipeline.md) | Stage order, dependencies, and parallel lanes. |
| [02 - Workflow](02-workflow.md) | Orchestration steps, handoffs, and verification loops. |
| [03 - Subagents](03-subagents.md) | Logical roles mapped to available `explore` and `general` subagents. |
| [04 - M0 gates](04-m0-gates.md) | Entry/exit criteria and completion evidence. |

## Operating Rules

- The main agent owns scope, sequencing, integration, and gates. Subagents own bounded slices and return evidence; they do not change contracts or expand scope.
- One slice owns one file set at a time. No two active subagents write overlapping paths.
- Deterministic local doubles first. No credentials, paid APIs, network discovery, or plugin processes in M0.
- Keep normal paths direct and failure paths short. Validation lives at owning boundaries; mutable authorization, cancellation, and deadlines are rechecked at dispatch.
- Record decisions, measurements, and limitations in docs. Do not present draft interfaces as stable.

## Related Documents

- [Implementation readiness](../design/09-implementation-readiness.md)
- [First-release defaults](../design/decisions/01-first-release-defaults.md)
- [Common contract](../contracts/00-common.md)
- [Engineering guidelines](../../AGENTS.md)
