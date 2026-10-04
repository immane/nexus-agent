# Implementation Readiness

Status: Draft engineering gates based on accepted [project direction](decisions/00-project-foundation.md) and [first-release defaults](decisions/01-first-release-defaults.md). No workspace, implementation, CI workflow, or benchmark exists yet.

## What Is Settled

The first release targets a small Rust coding core, a Grok Build-style Rust TUI and headless entry point, Linux/macOS support, and a 100 ms interactive-startup goal. Reads/searches are scoped and automatic; mutations/commands require confirmation. Sessions save locally outside the project and restore only on request. API configuration and existing relay services come before browser login or local relay execution.

Normal paths must be direct, with short explicit failure handling. Optional integrations must not expand the minimum startup path or introduce speculative frameworks.

## Engineering Choices Before Relevant Code

| Choice | Gate |
| --- | --- |
| Toolchain | Select and pin a stable Rust toolchain and document the minimum supported version strategy. |
| Packaging | Use the smallest meaningful module/crate split; keep core dependencies independent of terminal and concrete integrations. |
| Ports and ownership | Review concrete common, provider, tool, and command/event types; record which draft semantics are accepted before relying on them. |
| Async execution | Choose one shared runtime and narrow features; measure thread/dispatch choices instead of assuming they are free. |
| Resource policy | Give implemented queues, output, deadlines, and counters finite documented defaults and boundary tests. |
| Reference environments | Identify Linux and macOS hardware/terminal/build settings; report results separately. |

Config/storage serialization and numerical budgets should be selected when their implementation becomes necessary. External-tool MCP versions and local-relay process protocols are gates for those integrations, not reasons to delay a fake-provider core loop. No draft here promises a stable plugin ABI.

## M0: Small, Testable Baseline

Build only the minimum workspace, typed core/runtime, fake provider and fake tool, a basic full-screen frontend, and a headless consumer. Exercise one model/tool/model cycle with synthetic output and approval decisions.

M0 must demonstrate:

- One authoritative run lifecycle with truthful completion, failure, cancellation, and limits.
- No execution from partial arguments; validation and policy decisions have explicit owners.
- Read-like fake calls proceed under the scoped policy; mutation/command-like fake calls wait for approval or are denied when no handler exists.
- Bounded event/output handling with responsive cancellation and safe stale-event handling.
- Offline startup without real credentials, plugin processes, repository indexing, or network calls.
- Local startup, size, memory, idle CPU, and dispatch baselines, plus applicable formatting/lint/test checks on Linux and macOS.
- Terminal restoration and clean separation of headless data from diagnostics.

Fake integrations are test implementations, not production fallback. Any simulation entry point must identify itself; missing real configuration must never silently produce a fabricated successful task.

An explicit test-only ephemeral or in-memory store is acceptable for M0. It does not implement the accepted automatic-persistence default or establish crash recovery. Document that limitation instead of claiming production session support.

## Subsequent Implementation Gates

Before real side effects, implement persistent session intent/outcome semantics, scoped file operations, exact-call approvals, and recovery without blind replay. Confirm file durability behavior on both supported platforms.

Before model coverage claims, implement and test each mainstream protocol's streaming/tool/continuation behavior. Existing relay profiles follow the same provider gate, not a special bypass.

Before external-tool interoperability, choose supported protocol versions/capabilities and enforce lifecycle, output, environment, and credential limits. Leave local relay execution, browser login, Windows, and untrusted-plugin isolation outside the initial mandatory path.

## Code Review Checklist

- Can the happy path be read without following many wrappers or nested conditionals?
- Are immutable facts validated at boundaries and carried in explicit types?
- Are mutable permissions, cancellation, and deadlines rechecked where execution requires them?
- Does every abstraction correspond to a real integration or ownership boundary?
- Are fallback branches justified, tested, and safe with existing effects?
- Does the change add measurable cost or an unnecessary dependency to the minimal build?
- Are implementation claims, contract status, and measured evidence consistent?

Do not enforce arbitrary line-count limits or remove safety checks merely to make code look short. Prefer Rust enums, clear matches, `?`, and early returns where they simplify the actual control flow.

## Completion Evidence

For each implemented milestone, record exact checks, tested platforms/features, performance methodology, and remaining limitations in the relevant documentation. Use synthetic local fixtures by default; live API/relay tests require authorization and may incur cost.

No additional general-purpose plugin framework, daemon, indexing service, or public transport schema should be built just to make the preparation look complete.

## Related Documents

- [Common contract](../contracts/00-common.md)
- [Execution](02-execution.md)
- [TUI and headless design](07-tui.md)
- [Performance](05-performance.md)
- [Security](06-security.md)
