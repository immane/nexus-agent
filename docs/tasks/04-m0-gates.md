# M0 Gates and Evidence

Status: Draft acceptance gates. No gate has been executed; there are no measured results yet.

## Per-Stage Exit Criteria

- P0: accepted M0 semantic subset recorded; deferred items listed; no code reviewed against unaccepted semantics.
- P1: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` pass on Linux and macOS.
- P2: core unit tests cover identity scoping, bounds, complete versus partial records, error sanitization, and limit exhaustion.
- P3: fake-driven run tests prove one terminal outcome per run, no execution from partial calls, exact-call approval binding, and headless denial without a handler.
- P4: fragmented UTF-8/JSON, malformed frames, duplicate references, truncation, refusals, and continuation identity all have fixtures and tests.
- P5A: headless task runs offline with structured output separated from diagnostics.
- P5B: TUI starts offline, keeps the composer fixed, bounds viewport work, folds entries, and restores the terminal on normal and error exits.
- P6: cancellation under output load, slow consumers, disconnect handling, stale-run rejection, and duplicate-approval safety pass deterministically.
- P7: startup, binary size, memory, idle CPU, streaming, buffer high-water marks, and dispatch overhead recorded for both platforms with methodology.

## M0 Acceptance Checklist

- [ ] One model/tool/model cycle completes with fake integrations.
- [ ] Read-like fake calls proceed under scoped policy; mutation/command-like calls require approval or headless denial.
- [ ] Cancellation stops dispatch, reports uncertain effects honestly, and never claims rollback.
- [ ] Bounded queues, output, context, and event buffers have finite defaults and visible truncation.
- [ ] Startup performs bounded local work only; no network, credentials, plugin processes, or history scan.
- [ ] TUI and headless paths share one runtime and policy boundary.
- [ ] Linux and macOS validation recorded separately.
- [ ] Limitations documented, including ephemeral-only storage and no real provider/plugin support.

## Evidence Format

For each gate, record:

- Exact commands and results.
- Changed paths and test names.
- Platforms, hardware, terminal/PTY setup, build profile, and enabled features.
- Performance sample counts with p50, p95, and maximum where applicable.
- Remaining blockers and deferred work.

Use synthetic fixtures by default. Live-model checks are out of scope for M0 and require separate authorization if ever added.

## Failure Handling

A failed gate returns to its owning slice with the failing command, output, and reproduction. Do not bypass the gate with extra fallback logic, relaxed lints, deleted tests, or changed expectations. If the gate itself is wrong, the main agent updates the relevant task or contract doc first, then reruns the gate.

## Related Documents

- [Overview](00-overview.md)
- [Pipeline](01-pipeline.md)
- [Workflow](02-workflow.md)
- [Implementation readiness](../design/09-implementation-readiness.md)
- [Performance](../design/05-performance.md)
