# Phase 1 Subagents

Status: Draft roster. Logical roles map to the available `explore` and `general` subagent types.

## Roster

| Logical role | Subagent type | Owns in M0 | Must not do |
| --- | --- | --- | --- |
| Contract reviewer | `explore` | Consistency checks across designs, contracts, and tasks; conflict lists | Change contracts or approve scope. |
| Workspace bootstrapper | `general` | Toolchain pin, workspace layout, fmt/clippy/test setup | Add model, TUI, HTTP, or plugin dependencies. |
| Core builder | `general` | Domain identifiers, messages, errors, limits, policy types | Import terminal, HTTP, SDK, storage, or OS APIs. |
| Runtime builder | `general` | Single-run lifecycle, validation, approvals, cancellation, ordered events | Render UI, parse vendor protocols, or execute concrete tools. |
| Doubles builder | `general` | Fake provider/tool, fragmented and malformed fixtures, limit cases | Call networks, use credentials, or create fallback success paths. |
| Headless builder | `general` | Composition root and headless consumer over the runtime port | Add a daemon, service, second policy path, or terminal init. |
| TUI builder | `general` | Bounded viewport, composer, approval view, refresh control, cleanup | Call providers/tools directly or bypass runtime authorization. |
| Integration tester | `general` | Backpressure, slow-consumer, disconnect, stale-run, and approval tests | Weaken checks or delete valid failures to pass. |
| Perf recorder | `general` | Deterministic harness and Linux/macOS measurements | Claim cross-platform results from one host or hide methodology. |

The main agent retains orchestration, merges, contract edits, scope changes, commits, and gate decisions.

## Dispatch Mapping

| Pipeline stage | Subagent use |
| --- | --- |
| P0 Contract review | `explore` for consistency; main agent locks the M0 subset. |
| P1 Bootstrap | One `general` slice; `explore` only to verify file placement if needed. |
| P2 Core | One `general` core slice. |
| P3 Runtime | One `general` runtime slice after P2 passes. |
| P4 Doubles | One `general` doubles slice; may reuse core/runtime evidence. |
| P5A Headless | One `general` headless slice. |
| P5B TUI | One `general` TUI slice in parallel with P5A on disjoint paths. |
| P6 Integration | One `general` testing slice. |
| P7 Baselines | One `general` perf slice; main agent plus `explore` for final doc verification. |

Run at most two implementation slices concurrently, and only P5A/P5B are approved for parallel execution. All other work is sequential to keep ownership and review simple.

## Brief Template

Give each subagent:

- Objective and explicit non-goals.
- Allowed files and forbidden paths.
- Contracts and designs to follow with revision or section.
- Tests and evidence required.
- Performance and simplicity constraints: direct happy path, short failures, bounded buffers, no speculative branches.

Require the subagent to stop and report when its brief conflicts with a contract, needs a new dependency, or requires touching another slice's files.

## Related Documents

- [Pipeline](01-pipeline.md)
- [Workflow](02-workflow.md)
- [M0 gates](04-m0-gates.md)
- [Components](../design/01-components.md)
