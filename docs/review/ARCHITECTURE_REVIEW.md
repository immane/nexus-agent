# Nexus Agent Architecture Review

**Perspective:** Principal Rust software architecture review  
**Scope:** Whole-workspace architecture and the provider, runtime, policy, tool, sandbox, TUI, server, and headless execution paths.  
**Status:** Read-only code review; no source code changes or Cargo tests were performed as part of the review.

## Executive assessment

Nexus Agent has a sound architectural direction and should **not be rewritten**. The core is independent of third-party dependencies, runtime owns execution and policy, adapters implement narrow provider/tool ports, and frontends do not directly execute tools. Strict argument validation, exact approval binding, bounded public event channels, honest effect states, worker quarantine, terminal restoration, and output sanitization are valuable foundations to preserve.

The principal risks are not the number of crates or file lengths by themselves. They are:

1. Contract and execution mismatches, including provider selection and usage attribution.
2. Implicit policy contracts encoded by tool names, JSON field names, and scope strings.
3. Application/configuration responsibilities duplicated across frontends.
4. Resource bounds that do not cover every queue and lifecycle path.
5. Legacy or parallel abstractions that are not part of the real execution path.

The recommended approach is to keep the existing crate graph and ports, fix concrete behavior and lifecycle defects first, and only extract ordinary modules/functions or small private types where real duplication or ownership problems exist. Do not add an architectural layer merely to give every responsibility a new name.

This is a static review, not an exhaustive audit, a reproduced exploit, or a security certification. In particular, no Linux sandbox or live TLS behavior was exercised here.

## Findings and severity

Severity levels:

- **P1 — Priority:** fix before expanding the affected behavior; involves execution correctness, security boundaries, or resource lifecycle.
- **P2 — Consolidation:** resolve before significant feature growth; represents hidden coupling, duplicated ownership, or material architectural debt.
- **P3 — Lower priority:** documentation and contract drift that can mislead maintainers but does not alone establish a runtime defect.

### P1: behavior, security, and lifecycle

#### P1.1 — Server submit selection may not identify the provider actually used

**References:** `crates/nexus-server/src/server.rs:272–342`, `587–623`, `648–719`.

Provider adapters are selected and attached when a session is created. At submit time, the request's optional provider/model selection is validated and used for usage attribution, but it does not change the adapter already held by that session's runtime. A request can therefore select model B while the session still invokes model A, then record the run against B. A demo session likewise is not converted into a live-provider session merely by specifying a model at submit.

**Recommendation:** establish one explicit contract. The minimal compatible choice is a session bound to its provider/model, with conflicting submit selections rejected. If per-run selection is later required, resolve it before accepting the run and bind that actual selection to the run; do not add a generic provider router preemptively.

#### P1.2 — Streaming is not end-to-end bounded

**References:** `crates/nexus-runtime/src/runtime.rs:959–985`, `1047–1085`; `crates/nexus-openai/src/provider.rs:423–441`; `crates/nexus-runtime/src/runtime.rs:2471–2478`.

The runtime's provisional provider path uses an unbounded channel. The TLS stdout pump uses an unbounded `std::sync::mpsc::channel`. Provisional identity sets have no explicit cardinality cap; usage updates can generate control events; and the control outbox's finite-growth rationale assumes a finite event count that provisional input does not enforce. Full provider-batch validation happens after streaming, so it cannot constrain memory already queued.

Public bounded event channels do not make the whole pipeline bounded.

**Recommendation:** apply explicit event-count and byte budgets at each provisional boundary; bound the TLS pump; constrain or coalesce usage updates; and give the sink a way to observe closure, cancellation, and budget exhaustion. Preserve capacity/semantics for required terminal control records. Do not simply replace the queue with a blocking bounded send, which can deadlock when the consumer exits or cancellation occurs.

#### P1.3 — Closed control channel can trigger repeated flusher scheduling

**References:** `crates/nexus-runtime/src/runtime.rs:2495–2535`.

When `reserve().await` fails, the flusher puts the event back into the outbox and marks the control channel closed. At function exit, a nonempty outbox causes another flusher to be scheduled. A consumer that closes while committed events remain may therefore produce repeated tasks that take an event, fail on the closed channel, restore it, and reschedule.

**Recommendation:** make transport shutdown terminal, using a clear local lifecycle such as `Open`, `Draining`, and `Closed`. Once closed, do not reschedule; retain honest state indicating whether the terminal event was delivered.

#### P1.4 — Exec child and pipe ownership is incomplete on some exit paths

**References:** `crates/nexus-tools/src/exec.rs:205–235`, `354–359`, `405–414`.

After the direct child exits, the executor joins stdout/stderr reader threads. A descendant retaining a pipe can keep those threads blocked after the main child has exited and after deadline polling has stopped. `try_wait()` error handling does not share a clear cleanup path. Stopping reads once the output cap is reached can also change child behavior instead of draining and discarding excess output. Process-group termination is not proof that every descendant terminated. The startup sandbox probe uses synchronous `.status()` without an explicit timeout.

**Recommendation:** use a small private exec resource guard that owns the child and pipe-reader lifecycle, applies bounded cleanup/wait behavior on every exit path, and drains excess output while discarding it. Keep provider helper cleanup separate unless genuinely identical lifecycle code emerges. Do not build a general process/task framework.

#### P1.5 — Canonical path checks are not race-free filesystem capabilities

**References:** `crates/nexus-permissions/src/lib.rs:105–138`; `crates/nexus-tools/src/development.rs:75–116`; `crates/nexus-tools/src/fs.rs:95–120`, `737–757`, `936–967`.

The implementation documents the check/use race correctly. Repeating canonicalization does not eliminate the gap between checking a path and opening or mutating it. Writes and patches also truncate existing files in place; interruption or write failure may leave partial content, and patch read/modify/write can overwrite intervening changes.

**Recommendation:** treat stable binding of the checked resource to the actual operation as a separate platform/file-access problem, using narrow platform code or a mature capability API where appropriate. Separately define update consistency semantics, including whether version checks and temporary-file replacement are appropriate. Atomic replacement changes symlink, hardlink, permission, and file-identity behavior, so it must not be introduced as an undocumented drop-in change.

No race attack was reproduced in this review; this is a code-path and documented-boundary finding, not a claim of a demonstrated exploit.

#### P1.6 — Loopback server trust boundary is weak when real tools are enabled

**References:** `crates/nexus-server/src/main.rs:3–7`, `187–193`; `crates/nexus-server/src/server.rs:157–174`, `284–285`; `crates/nexus-server/src/http.rs:77–80`.

The server is explicitly test-only, unauthenticated, and loopback-bound; any local process can submit, approve, and cancel. When real development tools are enabled, this is an actual tool-execution entry point. Each connection creates a thread without a concurrency cap; sessions have no cap or removal lifecycle; SSE and socket write lifecycle limits are incomplete. Loopback binding reduces network exposure but is not caller authorization.

**Recommendation:** before product use with real tools, define caller and approver authentication, bound connections/sessions/runs, add idle lifecycle and appropriate read/write bounds, and handle browser-origin/host access deliberately. Do not treat sequential session IDs as authorization tokens.

### P2: architectural consistency and extension pressure

#### P2.1 — Tool names and JSON fields implicitly define policy

**References:** `crates/nexus-runtime/src/policy.rs:41`, `186–228`; `crates/nexus-runtime/src/runtime.rs:1415–1420`.

Runtime policy recognizes hard-coded names such as `host_read`, `host_write`, and `host_exec`, and interprets fields such as `path` and `write_dir`. Adding a tool requires editing central runtime logic. Tool descriptions or plugin declarations must not be allowed to grant their own trust, but tool-name inference is also an unnecessarily implicit host contract.

**Recommendation:** introduce only a small host-owned registration classification for current needs (for example read, mutation, process, unknown), with unknown tools requiring approval. Do not build a generic resource resolver framework or permission DSL now. Classification is not proof that a tool implementation is safe; execution adapters still enforce actual resource boundaries.

#### P2.2 — Resource scope is a string protocol shared across layers

**References:** `crates/nexus-core/src/approval.rs:55–73`; `crates/nexus-tools/src/development.rs:83`; `crates/nexus-tools/src/exec.rs:177`.

Values such as `path:...`, `exec-directory:...`, and `exec:workspace` are constructed and interpreted by different components. This mixes authorization data with presentation labels and makes correctness depend on exact string agreement.

**Recommendation:** centralize construction and validation for the current file and exec scope forms, keeping display text separate. The approval can continue to carry a bounded opaque scope where that is sufficient. Avoid a general resource tree, capability algebra, or policy language.

#### P2.3 — TUI configuration has multiple mutable session copies

**References:** `crates/nexus-tui/src/main.rs:395–399`, `441–446`, `966–980`, `1300–1318`.

Each session slot clones the startup configuration and may save it to the same path. Concurrent/background sessions can overwrite one another's recent-model updates.

**Recommendation:** give user configuration one application-level owner; let each session retain only session-local selection/state. This does not require a new application crate or a service layer.

#### P2.4 — TUI model usage can be attributed to the selection at run completion

**References:** `crates/nexus-tui/src/main.rs:1105`, `1300–1302`, `1366`.

The active model can change while a run is active, while recent usage is recorded from the current `active_model` when the terminal event is applied. That can attribute a completed run to a model other than the one selected by its provider adapter.

**Recommendation:** capture the actual model selection when the run is accepted and use that value for the run's usage record. A small run-to-model association is sufficient; a full execution snapshot abstraction is not.

#### P2.5 — Limit fields and effective budgets do not consistently line up

**References:** `crates/nexus-core/src/limits.rs:38–41`; `crates/nexus-runtime/src/runtime.rs:382–412`; `crates/nexus-openai/src/provider.rs:23`; `crates/nexus-runtime/src/protocol.rs:153–157`.

Event-capacity fields are present in `Limits`, while channel construction uses constants. Model response text/reasoning budgets reuse tool-output concepts; the OpenAI adapter allows an 8 MiB response while default runtime validation caps text/reasoning at 256 KiB. This makes it hard to know which values are hard ceilings, configurable budgets, or effective budgets.

**Recommendation:** document and separate hard adapter limits, configurable budgets, and runtime-effective limits. Wire fields that are meant to configure behavior; otherwise remove them from active configuration only when compatibility permits. Keep finite bounds and explicit exhaustion.

#### P2.6 — Streaming has a provisional representation and an authoritative batch

**References:** `crates/nexus-core/src/provider.rs:690–718`; `crates/nexus-runtime/src/runtime.rs:168–177`, `1047–1209`.

The provider emits provisional events and later returns a complete batch. Runtime tracks published item keys to avoid replay while separately validating and ingesting the authoritative batch. This can be valid, but the contract must define prefix/batch consistency and failure behavior; bookkeeping currently adds conceptual complexity.

**Recommendation:** retain this shape while streaming is valuable, first add contract tests for incomplete/mismatched prefixes and terminal races. Do not switch all ports to async or introduce a general stream-processing framework without demonstrated need.

#### P2.7 — Event ordering and merging responsibilities differ by frontend

**References:** `crates/nexus-tui/src/main.rs:823–948`; `crates/nexus-headless/src/lib.rs:464–468`; `crates/nexus-server/src/server.rs:879–985`.

The TUI reorders two channels; headless retains then sorts; server forwards selected events and drains predecessors before terminal, but does not use the same reorder policy. This can produce frontend-specific sequence behavior.

**Recommendation:** if shared code is warranted, extract one small bounded ordered event receiver/merger. Keep JSON formatting, terminal presentation, and report retention frontend-specific. Do not create a broad event framework.

#### P2.8 — Development file wrappers rebuild and redispatch existing tools

**References:** `crates/nexus-tools/src/development.rs:26–38`, `75–116`.

The development constructor creates tools to obtain descriptors, rewrites descriptions, then the wrapper parses arguments, resolves scope, selects an implementation by tool name, reconstructs that tool, rewrites JSON arguments, and calls it. This makes reuse indirect and binds behavior to names.

**Recommendation:** share the underlying filesystem operation functions and explicit path/scope checks directly. Preserve one runtime authorization path, but do not use `ToolPort` as an internal proxy to another `ToolPort` just to reuse implementation.

#### P2.9 — Provider configuration distinguishes routing but not protocol families

**References:** `crates/nexus-config/src/model.rs:95–100`; `crates/nexus-openai/src/provider.rs:221–231`; `crates/nexus-tui/src/main.rs:540–542`.

`AdapterKind::Direct/Relay` describes routing, not whether a provider speaks Chat Completions, Responses, Anthropic Messages, or another protocol. TUI's dynamic provider wrapper advertises capabilities from a fake provider rather than from the adapter selected for the run.

**Recommendation:** when adding another protocol family, represent protocol selection explicitly and use the selected adapter's capabilities for run admission. Do not build a provider registry/plugin platform before a second concrete protocol requires it.

#### P2.10 — Core contains parallel models not used by the real runtime path

**References:** `crates/nexus-core/src/content.rs:299–393`; `crates/nexus-core/src/provider.rs:135–188`; `crates/nexus-core/src/execution.rs:113–147`.

`AssistantTurn`/`CompletedTurn` exist alongside the production `ModelContextItem` plus `validate_batch` flow. `CompletedTurn` documentation says dispatch/admission takes it, but runtime does not. Core also carries duplicate credential-reference concepts and legacy elapsed/bool context paths used by fixtures.

**Recommendation:** choose the real execution contract as the source of truth. After caller and compatibility checks, remove or narrow unused parallel models and fixture-only compatibility API. Do not add conversion layers just to make legacy types appear integrated.

#### P1 validation gap — Sandbox evidence is asymmetric across platforms

**References:** `crates/nexus-tools/tests/exec_sandbox.rs:55–87`, `89–226`; `.github/workflows/ci.yml:51–74`.

On Linux, the basic exec test can pass by asserting a fail-closed denial when sandbox tooling is absent. Network, outside-root, and credential-isolation coverage in this test file is predominantly macOS-specific. Linux CI success therefore does not establish Linux sandbox isolation.

**Recommendation:** run dedicated Linux tests in an environment where the intended sandbox is available, and distinguish “backend unavailable, correctly denied” from “isolation behavior verified.” Preserve macOS and Linux-specific claims separately.

### P3: contract and documentation drift

Some descriptions no longer match implementation: `CompletedTurn` says it is used for dispatch while it is not; event-channel overflow descriptions do not fully capture flusher shutdown; and manifests/comments still say HTTPS is unsupported despite the OpenSSL bridge. These discrepancies make later reviews harder.

**Recommendation:** update comments and architecture/contract documents as corresponding behavior is deliberately settled. Do not make documentation edits imply unverified guarantees.

## Target architecture: minimal version

Keep the current crate graph and primary dependency direction:

```text
TUI / headless / HTTP entry points
  ├─ user input, presentation, transport
  └─ concrete startup configuration and adapter/tool wiring
                 │
                 ▼
              Runtime
  run lifecycle, policy, approvals, budgets, dispatch, history, events
                 │
                 ▼
          Core contracts

Provider and tool adapters ──> Core contracts
Runtime ──> Core + Validation + Permissions
```

This is a responsibility description, not a proposal to add an `ApplicationService` layer. Entrypoints remain composition roots. Shared application logic should become ordinary functions or private modules where duplication is real.

### Responsibility boundaries

| Area | Owns | Does not own |
|---|---|---|
| Core | IDs, the selected conversation/call/outcome model, narrow ports and contracts | Vendor fields, terminal APIs, HTTP clients, OS sandbox implementation |
| Runtime | Run state, scheduling, authorization, approval lifecycle, effective budgets, worker ownership and event lifecycle | Concrete provider construction, configuration file persistence, rendering, tool business logic |
| Entrypoints/frontends | Input, presentation/transport, startup composition | Alternate execution or authorization paths |
| Config | Validated user config, selection/reference resolution, persistence | A second copy of runtime policy |
| Adapters/platform code | Protocol and OS I/O, resource cleanup, tool-specific validation | Approval grants or mutation of runtime state |

Three contracts need to be explicit without introducing broad abstractions:

1. **Run identity:** a run uses the provider/model and mode selected when accepted. Later UI changes affect the next run. Record usage against that accepted identity.
2. **Tool trust:** policy classification is host-owned; unknown tools require approval. A descriptor or plugin cannot grant its own trust. Actual file/process boundaries remain the adapter/sandbox's job.
3. **Event consumption:** shared sequence/terminal ordering may use one small bounded merger, while each frontend keeps its own output format and presentation retention.

## Critical review of the proposed architecture

The initial architecture proposal risked introducing a layer for every responsibility. Its unnecessary or premature parts should be avoided:

- **A new Application crate or service layer:** not needed while entrypoints can call a few shared functions. It risks becoming a pass-through between frontend and runtime.
- **A comprehensive immutable execution snapshot:** the principle of fixed run identity is necessary, but copying all runtime configuration into a new abstraction is not. Capture only the actual provider/model identity needed to fix behavior.
- **Generic resource resolvers and prepared-call typestate:** these could create a chain of validated/prepared/authorized/executable wrappers without eliminating filesystem races. Start with a small host-owned tool classification and existing execution contracts.
- **A universal ResourceScope enum/tree or permission DSL:** current scope agreement should be centralized, but a general resource language is not justified. Keep current file/exec cases concrete and separate display labels from authorization values.
- **Shared worker/process manager:** provider and exec process lifecycles differ. Add a private exec guard for child/pipe cleanup; keep provider cleanup local unless reuse is proven.
- **A wholesale runtime state-machine rewrite:** reducing booleans is not a goal by itself. Change only locally inconsistent state; preserve the existing run lifecycle and quarantine guarantees.
- **Wiring `CompletedTurn` into production just to satisfy its comments:** prefer one authoritative model and retire unused parallel types after compatibility review.
- **A generic provider registry or plugin system:** wait for a second concrete protocol or plugin implementation.
- **Broad async conversion:** synchronous ports with bounded `spawn_blocking` adaptation remain reasonable for current sequential execution. Fix queue bounds and lifecycle first.
- **A general event framework:** a focused ordered receiver may be useful; frontend formatting and retention should remain local.

The governing rule is: **one owner for each important state, one authoritative entry point for each security decision, and as few new public abstractions as possible.** Repeated checks of immutable validated facts can be removed; mutable authorization, cancellation, deadlines, and filesystem boundaries must still be checked where required.

## Keep, simplify, remove, redesign

### Keep

- Current core/runtime/adapter/frontend dependency direction.
- `ProviderPort` and `ToolPort` as real replacement boundaries.
- Strict JSON parsing, duplicate-key rejection, schema compilation, and finite limits.
- Exact approval binding, dispatch-time checks, cancellation/deadlines, and quarantine.
- Explicit outcome/effect/evidence distinctions and terminal honesty.
- Fail-closed sandbox selection, terminal restoration, sanitization, and presentation bounds.
- Fakes and cross-frontend integration tests.

### Simplify

- TUI model selection, session configuration ownership, and tool/provider startup wiring.
- Duplicated tool-set construction and path/scope handling.
- Event ordering logic, if a small shared implementation can preserve each frontend's behavior.
- Runtime internals only where a module split makes ownership easier to understand; do not create a manager/trait for every subsystem.

### Remove or narrow after caller/compatibility review

- Unused parallel turn-completion models and duplicate credential representations.
- Fixture-only compatibility context/constructors from production API where feasible.
- Public test-only keymap surface if it is not part of supported behavior.

Do not delete meaningful safety tests to enable cleanup; move tests to the retained contract.

### Redesign selectively

- Provider selection and usage attribution at run acceptance.
- Actual boundedness and close behavior across streaming and control delivery.
- Exec child/pipe cleanup.
- Tool authorization classification and file-access race boundary.
- Configuration ownership across TUI sessions.
- Server trust and resource limits before any product-facing real-tool use.

Do not pursue by default: generic hooks, a crate per tool, workflow frameworks, DI containers, speculative plugin APIs, or broad infrastructure rewrites. A mature HTTP/TLS client may reduce hand-written protocol/lifecycle code, but decide separately using dependency cost, startup/performance goals, and protocol regression evidence.

## Incremental plan, prioritized by impact

### Phase 1 — Fix concrete behavior and lifecycle defects

1. Reject server submit selections that conflict with the session's bound model/provider.
2. Stop control flusher rescheduling after closure while retaining honest undelivered-terminal state.
3. Bound provisional provider queues, TLS pump, usage updates, and outbox growth in both events and bytes.
4. Give exec child/pipe cleanup a bounded, all-exit-path owner; drain/discard output after the report budget is reached.
5. Bind TUI usage attribution to the model actually selected for the accepted run.
6. Before product exposure, cap server connections/sessions and define local caller/approver trust.

Keep fixes separate and add targeted regression tests; do not mix them with mass file moves.

**Acceptance:** actual provider matches the accepted selection; closed channels do not spawn retry loops; producer/consumer imbalance has a measured finite bound; descendant-held pipes cannot extend waits without bound; model switching does not alter attribution; server trust and capacity assumptions are explicit.

### Phase 2 — Tighten concrete permission/platform boundaries

1. Replace scattered tool-name policy branches with a small host-owned access classification; default unknown tools to approval.
2. Centralize current file/exec scope construction and keep presentation text separate.
3. State and test the difference between path checks and race-free capabilities; improve resource binding where supported.
4. Define file update/interruption semantics and add concurrency/version checks or atomic replacement only where compatible.
5. Run Linux sandbox isolation tests with the backend available and keep platform evidence distinct.

**Acceptance:** adding a tool does not require unrelated name-based policy branches; unknown tools fail closed; existing strict/development/read-only/grant behavior remains; mutable policy/deadline/cancellation checks stay at dispatch.

### Phase 3 — Reduce duplicated configuration and assembly ownership

1. Extract ordinary shared functions for tool construction and configuration selection where TUI/server actually duplicate behavior.
2. Give TUI user configuration one owner; session slots hold local model choice, not independently writable global config copies.
3. Associate a run with its accepted provider/model for use recording.
4. Keep entrypoints as composition roots; add no application service/crate unless multiple real consumers justify it.

**Acceptance:** equivalent frontend selections resolve to equivalent execution wiring, and one session cannot overwrite another session's unrelated configuration update.

### Phase 4 — Selective internal consolidation

1. Share one bounded event-ordering helper only if it preserves server, TUI, and headless lifecycle semantics.
2. Simplify development file wrappers by sharing filesystem functions directly.
3. Split large runtime/frontend files into private modules only where ownership is clearer.
4. Remove unused parallel contracts after callers and compatibility are checked.

Do not rewrite the entire state machine or add generic manager traits. Preserve terminal, cancellation, approval, and quarantine invariants.

### Phase 5 — Revisit extension contracts only when a concrete extension arrives

When adding a second provider protocol, make protocol family selection explicit and use the selected adapter's capabilities. When adding external tool plugins, define trust and lifecycle boundaries for that concrete transport. Reassess HTTP/TLS reuse independently. Do not construct provider/plugin registries, hooks, or public extension APIs ahead of demonstrated consumers.

## Final recommendation

The next changes should prioritize **run identity, end-to-end boundedness, and resource lifecycle**, not broad module restructuring. The key design challenges are whether tool names should encode authorization (they should not), whether frontends should independently resolve providers (they should not), whether parallel turn models provide real guarantees (currently unclear), and whether hand-written HTTP/TLS remains simpler than a maintained dependency (reassess with evidence).

Preserve quarantine and other safety mechanisms even where they add code. Simplify by removing duplicated ownership and implicit contracts, not by weakening checks or adding a framework around them.

Validation commands were not run for this documentation-only review artifact. The workspace's `docs/testing.md` requires sequential Cargo validation for code changes; that is not applicable to this document-only change.
