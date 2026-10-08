# Architecture Migration Plan

Status: Proposed implementation plan based on the [architecture review](../review/ARCHITECTURE_REVIEW.md). No task is implemented or validated by this document. This is consolidation of the working agent, not a rewrite, a new M0 lock, or evidence of production security readiness.

## Goal and scope

Fix execution-contract and lifecycle defects first, then reduce duplicated ownership and implicit contracts without adding a framework. Preserve the existing crate graph, synchronous provider/tool ports where practical, one authoritative runtime execution path, and the working frontend behavior.

The five migration phases below are named **M1–M5** to avoid confusion with the historical M0 implementation stages in [Pipeline](01-pipeline.md). M1–M4 are the current consolidation work. M5 is conditional follow-up, activated only by concrete extension requirements; completing consolidation does not require implementing new providers or plugins.

The source review is static evidence, not a reproduced vulnerability or a complete audit. Before each task, re-read its implementation, callers, and tests in the current checkout. References in the review identify the reviewed version; line numbers may change. Confirm or refine findings with regression tests before changing behavior. If a finding no longer applies, record the evidence rather than implementing a speculative fix.

## Non-goals and invariants

- No agent rewrite, new application/service layer, general worker manager, permission DSL, generic provider registry, lifecycle hooks, or speculative plugin APIs.
- Do not create a crate per responsibility or convert all ports to async. Prefer existing modules, ordinary functions, and small private resource owners.
- Preserve exact approval binding to run/call/tool revision/arguments/scope/expiry/policy revision. Model output and plugin descriptions never grant permission.
- Keep dispatch-time checks for mutable cancellation, deadlines, approval validity, and resource state. Remove repeated immutable checks only when their validated contract is carried forward.
- Preserve strict and development behavior, read-only denial, denial without an approval handler, session-only directory grants, and no persisted/restored grants. [First-release defaults](../design/decisions/01-first-release-defaults.md) remain authoritative for accepted product choices.
- Preserve sandbox fail-closed behavior. Do not add unsandboxed fallbacks or claim that canonical paths, process groups, or loopback binding provide guarantees they do not.
- Preserve honest effect/evidence states, unknown usage, stale-run rejection, required terminal records, and worker quarantine. Cancellation is not rollback; do not replay uncertain side effects.
- Preserve frontend-specific output formats, terminal restoration, sanitization, approval focus, and bounded presentation retention unless an explicit compatibility decision changes them.
- No dependency upgrades, paid/live-provider calls, deployment, public service exposure, or machine security changes as part of routine migration. New dependencies or platform privileges need separate justification and authorization.
- Do not replace local persistence/configuration formats, implement session durability, or remove meaningful safety tests merely to simplify the architecture.

## Phase order and dependencies

| Phase | Focus | Entry condition | Exit condition |
| --- | --- | --- | --- |
| M1 | Correctness, bounded streaming, process and server lifecycle | Task-specific current-code review and regression fixture | Selected provider and usage agree; close/overflow/cleanup paths are bounded and honest |
| M2 | Tool policy and concrete filesystem/platform boundaries | M1 lifecycle foundations; platform/compatibility decisions accepted | Policy classification is host-owned, scopes agree, file guarantees and platform evidence are explicit |
| M3 | Configuration ownership and composition reuse | M1 run-selection fix and M2 tool-registration contract | One TUI config owner; shared wiring functions do not duplicate execution policy |
| M4 | Selective internal consolidation | Relevant M1–M3 contracts and tests stable | Less duplication and fewer unused contracts, with equivalent observable behavior |
| M5 | Extension contracts on demand | A real second protocol, concrete plugin, or transport replacement requirement | The requested extension fits existing boundaries and passes its own compatibility/security gate |

Recommended first slices: **M1.1 → M1.2 → M1.3 → M1.4**. M1.5–M1.7 can be separate scoped slices after their own prerequisites; do not postpone them until a large frontend refactor. M2/M3 independent preparation may overlap, but changes to shared contracts or files must be integrated sequentially. All Cargo commands against the shared target directory run sequentially.

Task dependencies are listed below. Stage gates are the default integration order, not a reason to bundle tasks into one large change. A blocked platform/security task remains an explicit blocker for its affected capability, not a fabricated passing gate.

## M1 — Fix concrete behavior and lifecycle defects

### M1.1 — Bind server submissions to the session's actual selection

**Findings:** P1.1. **Paths:** `crates/nexus-server/src/server.rs`, `json.rs`, server tests; affected API/setup documentation. **Dependencies:** none.

- Retain session-bound provider/model wiring. Store enough validated selection identity to compare a submit request against that binding; do not add a provider router or rebuild the runtime on each submit.
- Define omitted fields as using the session binding. Define equivalent provider-default/model selections explicitly; reject conflicting selections before accepting a run or recording usage.
- A demo session must not accept an explicit live selection while continuing to run fakes. Return an explicit mismatch error; changing the session's binding is not part of this task.
- Attribute usage to the actual bound model, not an unchecked submit label. Document request compatibility and error responses before changing the API.

**Acceptance/tests:** session A plus submit A succeeds; omitted selection uses A; submit B is rejected without minting a run or altering history; demo/live mismatch is rejected; unknown selection and unavailable credentials remain explicit failures. Assertions verify the adapter actually invoked and the model recorded, not just HTTP status.

### M1.2 — Make closed control transport terminal

**Findings:** P1.3; relevant P3 comments. **Paths:** `crates/nexus-runtime/src/runtime.rs`, `transport.rs`, runtime tests. **Dependencies:** none.

- Stop flusher scheduling after permanent channel closure. Keep saturation distinct from disconnection and define ownership of an event already removed from the outbox but not delivered.
- Preserve undelivered-terminal information in the existing outcome/snapshot contract. Do not silently mark a buffered terminal delivered or drop worker ownership.
- Use the smallest local state change that proves the invariant; a new transport state machine is not mandatory if existing state can express it clearly.

**Acceptance/tests:** fill the control channel, buffer events, then close its receiver before and after logical run completion. Verify no repeated flusher work, no false delivery claim, no duplicate terminal, and a stable observable outcome. Exercise normal drain and cancellation races with deterministic synchronization.

### M1.3 — Bound the provisional streaming and control pipeline

**Findings:** P1.2, P2.5, P2.6. **Paths:** runtime `runtime.rs`, `protocol.rs`, `transport.rs`; OpenAI `provider.rs`; core provider/limit contracts only if necessary; associated tests. **Dependencies:** M1.2.

- Inventory every queue, retained identity set, usage update, and aggregate batch. Establish explicit event-count and byte ceilings, including the TLS pump, provisional stream, and control outbox.
- Enforce provisional identity and cumulative payload budgets before retaining/publishing input. Constrain usage update frequency without losing the authoritative final counters.
- Define overflow, receiver closure, cancellation, and producer termination behavior. If sink feedback requires a port change, update all implementations and callers in the same slice; otherwise use a clearly bounded local bridge. Do not add parallel public streaming APIs for compatibility without an actual caller need.
- Keep cancellation responsive while a producer encounters pressure. Do not replace unbounded sends with indefinite blocking sends.
- Reserve finite capacity for mandatory lifecycle/terminal records. Specify what happens when the reserve is exhausted; preserve explicit limit outcomes rather than silent control drops.
- Distinguish adapter hard ceilings, request budgets, and effective runtime budgets. Wire channel-capacity fields if configurable, or explicitly deprecate inactive fields. Do not silently raise the current output limits to accommodate the adapter's larger ceiling.
- Keep provisional presentation non-executable. Define its relationship to the authoritative batch, including mismatched/partial prefixes, invalid final batches, and completion races.

**Acceptance/tests:** fast producer/slow consumer, many tiny fragments, oversized fragments, many identities, changing usage, closed receiver, cancel under pressure, and join-versus-drain races. Record queue/retention high-water counts and bytes against declared ceilings using local fixtures. A bad/incomplete batch dispatches no tool; accepted text is not duplicated; final usage remains accurate. Show timeout/cancellation progress without wall-clock-dependent correctness assertions.

### M1.4 — Own exec cleanup on every exit path

**Implementation status: In progress.** Unix exec pipes are polled nonblocking with a per-pipe drain quantum of 64 KiB; retained output remains capped at 64 KiB per stream. Cleanup grace is 250 ms and the startup probe timeout is 2 s. If a descendant keeps a pipe open beyond cleanup, the exec worker retains ownership rather than returning; runtime timeout/cancellation quarantines that worker. The original process group is signalled, but escaped descendants that close inherited pipes cannot be proven terminated by this mechanism. Linux-specific sandbox execution and injected wait/read/probe failures remain unverified.

**Findings:** P1.4. **Paths:** `crates/nexus-tools/src/exec.rs`, tool exec tests. **Dependencies:** none; validate together with M1.3 when both are integrated.

- Introduce only a private exec resource guard or equivalent local ownership. Cover child spawn, wait failure, pipe-reader failure, deadline, cancellation, and normal completion.
- Continue draining/discarding output after the retained-output budget is reached. Bound retention independently from process output volume.
- Keep deadline/cancellation checks active during pipe cleanup and sandbox probing. Define a finite cleanup grace period and report unconfirmed termination honestly.
- Do not assume direct-child exit or process-group signaling proves every descendant has stopped. Where bounded pipe-reader cleanup needs platform I/O support, select a concrete supported implementation rather than pretending a timed thread join cancels a blocking read.
- Keep the runtime quarantine behavior; no success or known-no-effect claim while relevant work remains unconfirmed.

**Acceptance/tests:** direct child exits with a descendant holding stdout/stderr; output exceeds the cap; cancellation during output; startup probe stalls; launch/wait/read failures. Use injected boundaries where actual OS failures cannot be reproduced reliably. Confirm bounded waiting, truncation, cleanup ownership, and honest effect states on supported platforms.

### M1.5 — Record TUI usage against the accepted run's model

**Implementation status: Verified.** TUI submission now snapshots the live provider selection before runtime dispatch; the provider consumes that snapshot for its first call for the run. Accepted run IDs retain their selected model for terminal usage recording, independent of subsequent picker changes. Busy/rejected submissions clear the pending snapshot and do not create usage attribution. TUI configuration save failures remain transcript notices and do not alter run outcomes.

**Findings:** P2.4. **Paths:** `crates/nexus-tui/src/main.rs`, TUI tests. **Dependencies:** none.

- Capture the actual selection consistently with run acceptance and the per-run provider binding, not merely the display label when the terminal event arrives.
- Resolve the acceptance-to-first-provider-call race: later selection changes must not alter an accepted run. Use the smallest explicit binding mechanism; any temporary restriction on model switching is a user-visible compatibility decision, not a hidden workaround.
- Preserve next-run selection changes and associate terminal use recording with the accepted identity. Do not introduce a full execution snapshot.

**Acceptance/tests:** accept A, select B before the first provider invocation and during streaming, complete A, then submit B. Verify actual invocations and recent attribution. Rejected/busy submits create no attribution; terminal duplicates do not record twice; failed config saves remain visible without changing the run outcome.

**Validation evidence:** `cargo fmt --all --check`; `cargo nextest run -p nexus-tui --locked --offline` (497 passed, 11 skipped); `cargo clippy -p nexus-tui --all-targets --locked --offline -- -D warnings` (passed). Regression coverage verifies the pre-provider selection snapshot and run-bound model attribution after the picker changes; no live-provider call was made.

### M1.6 — Bound server connections, sessions, and SSE lifecycle

**Implementation status: Verified.** The binary admits at most 64
connection threads and rejects additional sockets by closing them; the server
admits at most 128 sessions and rejects excess creation with `503` rather than
evicting session-owned runtime state, so at most 128 runtime runs can be active
(one per session). HTTP request reads have a 60-second wall-clock deadline
(30-second maximum per read), and socket writes have a
30-second timeout. SSE subscriber ownership is released through an RAII guard
on normal return, disconnect, and unwinding. Usage associations are capped at
128, and registration is serialized with terminal attribution to close the
completion-before-registration race; bound-model submissions are refused with
`503` at capacity. SSE rejects unknown/evicted run ids before taking receiver
ownership. Cross-run pending retention is capped at 4,096 events and 1 MiB per
session, plus one overflow event reserve capped at 2 MiB; once saturated, the
SSE stream closes with a backpressure comment and submissions are refused with
`409` until retained events are drained. Deferred terminal records are capped
at 128 per session and stop new run admission at the cap. Completion watchers
are capped at 128 per server; if the watcher cap is full, the bounded
association is reconciled on the next run or safe idle sweep. One minute
housekeeping performs a 30-minute idle sweep; it reclaims only sessions with
no in-flight request or subscriber and a finalized (or never-started) runtime.
A bounded-per-session completion watcher observes finalized runs; usage is also settled before a
newer run replaces its snapshot and during safe idle reclamation. It therefore
does not require a client to consume the terminal SSE frame. Unknown or active
runtime state is retained conservatively.

Regression coverage includes connection/session saturation, retained-event
count/byte ceilings, unknown-run subscriptions, expired/active session cleanup,
SSE timeout installation, and a completed bound-model run with no SSE reader.

**Findings:** P1.6. **Paths:** server `main.rs`, `http.rs`, `server.rs`, server tests. **Dependencies:** M1.2 for closed-stream integration; M1.1 for selection identity.

- Choose explicit finite limits for connections, sessions, active runs, pending events, and unconsumed usage associations. Reject excess admission rather than silently dropping an active session.
- Add write and total-request/idle bounds where necessary; a per-read timeout alone does not establish a total slow-client bound. Release subscriber ownership on all SSE exit paths.
- Define session cleanup so grants/history/subscriptions are session-owned. Never evict an active or quarantined runtime by merely discarding its handle. Disconnection must not fabricate cancellation or rollback.
- Do not make authoritative completion accounting depend indefinitely on a client reading the terminal SSE event. Define bounded attribution cleanup and handle completion-before-registration races.

**Acceptance/tests:** saturation and slow clients, client stops reading, invalid/unknown-run subscription, reconnect/closure, idle expiry, session cleanup while work remains active, and a run never streamed to completion. Verify limits and absence of orphaned subscribers or unbounded retained maps.

### M1.7 — Set the real-tool server trust contract

**Findings:** P1.6. **Paths:** server entry/HTTP/route code and tests; security/setup documentation. **Dependencies:** explicit decision D2 below; M1.6 for integration.

- Keep loopback binding. Decide a minimal local caller/approver authorization mechanism before product-facing real-tool use; session identifiers are correlation identities, not credentials.
- Define unauthenticated access, authenticated submission/approval, browser Origin/Host handling, credential provisioning, and errors. Do not broaden network exposure or add OAuth/account infrastructure.
- Preserve the explicitly test-only demo profile only if its limitations and real-tool gating are clear. Any startup/auth compatibility change requires documented migration instructions.

**Acceptance/tests:** unauthorized submit/approve/cancel cannot act on real-tool runs; authorization never follows from a session ID; allowed/denied browser-origin and host cases are tested; credentials do not appear in URLs, logs, configuration examples, or responses. Pending design or tests block product security readiness, not silently pass it.

### M1 gate

All tasks above are resolved or have explicit evidence that the finding no longer applies. M1.7 may await an approved trust decision, but real-tool server product acceptance remains blocked while it is unresolved. Run affected-package and cross-frontend lifecycle tests; record finite bounds, confirmed cleanup capabilities, and platform limitations. No broad module moves belong in this phase.

## M2 — Tighten permissions and concrete platform boundaries

### M2.1 — Make tool access classification host-owned

**Findings:** P2.1. **Paths:** runtime policy/registration, core tool contracts only where needed, concrete tool/fake registration and tests. **Dependencies:** M1; keep existing automatic behavior covered by tests.

- Introduce a minimal host-registration access category for current built-ins, including conservative unknown handling. Do not trust model-visible descriptions or arbitrary plugin declarations.
- Remove scattered name-based read-only classification; mutation/process calls remain denied in read-only mode even when development grants would otherwise allow them.
- Keep concrete built-in argument interpretation local and explicit. Eliminate duplicated name/field conventions only where a real shared owner exists; no generic per-tool resolver or prepared-call pipeline is required.

**Acceptance/tests:** tool classification is independent of display/name conventions; unknown tools cannot self-authorize; custom read/mutation fixtures exercise strict/development/read-only/no-handler paths; registration alone never grants scope. Automatic access still requires a valid resource authorization.

### M2.2 — Centralize file and exec scope agreement

**Findings:** P2.2. **Paths:** permissions, runtime policy, tool development/exec modules, approval tests. **Dependencies:** M2.1.

- Use shared construction/checking for existing file and exec scope forms. Authorization data is never reconstructed from display text.
- Preserve bounded opaque approval scope and exact binding unless a concrete local typed representation proves simpler. Keep serialized compatibility where possible; a scope representation change invalidates incompatible pending grants explicitly.
- Recheck changed targets and session directory boundaries immediately before use. Do not broaden directory grants or persist them.

**Acceptance/tests:** malformed or mismatched scope, renamed/symlink-changed target, external write directory, protected path, expiry, stale run, and once/session approval cases remain fail-closed. UI sanitization cannot alter the authorized resource.

### M2.3 — Bind file access and define update consistency

**Findings:** P1.5. **Paths:** permissions, `crates/nexus-tools/src/fs.rs`, `development.rs`, filesystem tests; platform/security docs. **Dependencies:** M2.2 and accepted D3.

- Inventory read/list/search/write/patch check/use gaps. Choose narrow platform file-access code or a mature capability dependency for supported Linux/macOS guarantees; do not add repeated canonicalization as a claimed race fix.
- Define symlink, parent traversal, new-file, hardlink, metadata, permission, and file-identity behavior. Preserve current documented behavior or approve and document intentional differences.
- Separate access containment from update integrity. Assess version checks and temporary-file replacement for write/patch, with ownership/cleanup of temporary files and explicit partial-failure outcomes.
- Do not claim that compare-then-rename alone prevents all concurrent external writers. Test and document the supported conflict-detection guarantee; blocking filesystem I/O may still have platform limits.

**Acceptance/tests:** deterministically synchronized path/parent swaps, protected targets, special files, existing/new files, interruption/write failure, concurrent patch changes, permission preservation, and symlink/hardlink semantics. An unimplemented platform guarantee remains a declared blocker for that guarantee, not a success claim.

### M2.4 — Verify sandbox capability on both supported platforms

**Findings:** P1 validation gap. **Paths:** `crates/nexus-tools/tests/exec_sandbox.rs`, `tests/behavior.rs`, CI workflow if authorized, platform documentation. **Dependencies:** M1.4; repeat relevant tests after M2.3.

- Maintain separate coverage for backend-unavailable denial and actual sandbox isolation. Add a Linux test lane/environment where the intended backend must be available.
- Verify allowed project writes, denied outside writes, denied network, protected credential paths, declared external write scope, and cancellation/descendant cleanup. State deliberate platform differences.
- Provision test environments explicitly; never change the operator's machine isolation/security settings to make tests pass. A missing required backend makes isolation validation blocked or failed, not passed via a denial test.

**Acceptance:** Linux and macOS evidence separately identifies backend/version/profile and tests actually executed. Fail-closed tests remain valid but are not counted as containment evidence. No paid endpoints or real secrets are used.

### M2 gate

Strict/development/read-only and exact approval invariants hold after policy changes. File-access and update guarantees have an approved compatibility description and regression coverage. Record both-platform sandbox results or explicitly block the corresponding capability. No general permission framework has been introduced.

## M3 — Reduce configuration and composition duplication

### M3.1 — Give TUI user configuration one owner

**Findings:** P2.3. **Paths:** TUI session/frontend state in `main.rs`, config APIs only where necessary, TUI/config tests. **Dependencies:** M1.5.

- Own mutable user config once in the TUI process; slots retain local selected model/mode and presentation state. New sessions use the current shared config rather than a stale startup copy.
- Serialize recent/favourite updates against that owner. Preserve best-effort save behavior and surface errors without changing successful run outcomes.
- Do not imply atomic file replacement merges edits from other processes. State the existing cross-process save policy; add locking/merge semantics only if explicitly required.

**Acceptance/tests:** two sessions finish in either order and retain both updates; creating/switching sessions does not overwrite unrelated config; failed saves preserve in-memory changes; per-run attribution and next-run selection remain consistent.

### M3.2 — Share concrete wiring functions, not an application layer

**Findings:** P2.8 composition duplication; review target-boundary recommendations. **Paths:** tools constructors, config selection helpers, TUI/server entrypoints and tests. **Dependencies:** M1.1, M2.1–M2.2; M3.1 for TUI integration.

- Extract shared construction of current strict/development tool sets into `nexus-tools`. Keep runtime config and concrete provider assembly at the entrypoints.
- Put pure configured-provider/model reference resolution in config only where both callers genuinely need it. Config must not import a concrete provider or runtime policy implementation.
- Preserve frontend-specific credentials/readiness errors and approval-handler defaults. No fake fallback for an invalid live selection, and no new execution loop.

**Acceptance/tests:** equivalent supported configurations yield matching tool identities and permission behavior through both composition roots; custom modes and unknown references fail explicitly; fake/headless limitations remain documented. The runtime does not gain dependencies on concrete adapters or frontends.

### M3.3 — Use truthful provider identity/capabilities for current runs

**Findings:** P2.9 (existing dynamic capability claim), P2.4. **Paths:** TUI provider binding, runtime registration only if needed, provider tests. **Dependencies:** M1.5, M3.2.

- Stop using fake capabilities as a blanket claim for every real selection. Validate the current adapter's advertised capabilities at the actual binding boundary, with clear rejection if it cannot satisfy enabled tools/budgets.
- Retain the current single protocol implementation and fixed per-run selection. Do not add protocol/plugin registries or rebuild history as a workaround.
- If current adapter capabilities are intentionally identical, make the invariant explicit and tested rather than deriving production claims from a fake.

**Acceptance/tests:** a limited-capability adapter fixture cannot bypass admission through the wrapper; model changes affect only future runs; demo identity is explicit; existing conversation/history remains valid.

### M3 gate

TUI config has one mutable owner and accepted runs have one actual selection. Shared functions reduce existing duplication without introducing an application service/crate. Equivalent supported configuration is exercised across frontends; intentional differences (including fake-only headless behavior) remain explicit.

## M4 — Selective internal consolidation

### M4.1 — Unify ordered event consumption where it is actually shared

**Findings:** P2.7. **Paths:** runtime `transport.rs`, TUI merger, headless consumer, server SSE and related tests. **Dependencies:** M1.2–M1.3, M1.6; frontend contracts stable after M3.

- Define one bounded ordering helper for run/sequence rejection, cross-channel merge, gaps, terminal handling, and closure. Prefer the existing transport module; introduce no event framework or mandatory background service.
- Preserve each frontend's formatting and retention. Specify pre-terminal draining and outbox interactions so no predecessor is silently stranded and no nonexistent event is fabricated.
- Do not change public wire revision or event naming merely for internal consistency.

**Acceptance/tests:** reordered channels, duplicates, stale/post-terminal events, missing sequences, saturation, closure, and reconnect policies. Server output sequence is monotonic; TUI output is not duplicated; headless encoding/exit codes remain compatible. Cross-frontend tests exercise the same lifecycle fixtures.

### M4.2 — Simplify development file-tool reuse

**Findings:** P2.8. **Paths:** tools `development.rs`, `fs.rs`, tool tests. **Dependencies:** M2.2–M2.3, M3.2.

- Replace descriptor reconstruction, per-call tool rebuilding, description rewriting, and JSON/call redispatch with direct reuse of concrete filesystem operation functions.
- Keep runtime authorization and adapter resource rechecks. Do not expose operations that let frontends bypass dispatch policy.
- Preserve tool IDs/schemas, outcomes, protected path handling, and output budgets. Changes to descriptions must remain truthful, not silently imply different permissions.

**Acceptance/tests:** strict and development paths perform equivalent allowed file operations, reject wrong scope/protected paths, preserve write/patch semantics, and observe cancellation/deadlines. No second session grant cache appears in tools.

### M4.3 — Remove unused contracts and split only cohesive modules

**Findings:** P2.10, P3. **Paths:** core and its callers/tests; runtime/TUI private modules only where justified; affected docs. **Dependencies:** preceding behavioral contracts stable; accepted D4 for public API removals.

- Inventory real consumers of parallel completion types, credential references, legacy bool/elapsed contexts, panic constructors, and public test-only keymap APIs. Keep required compatibility where consumers exist.
- Prefer the working `ProviderEvent → validate_batch → admission → dispatch` contract; do not wire unused completion wrappers into it merely to match comments.
- Move useful regression coverage to retained contracts before retiring obsolete API-specific fixtures. Preserve low-value/ignored test policy; removal of a public fixture API requires an explicit compatibility decision, not deletion just to get green tests.
- Split files only around clear ownership, such as conversation retention, delivery, or exec cleanup. Do not rewrite the whole state machine or add a manager/trait per module.
- Update stale comments/manifests and architecture/contract docs in the same slice that settles their behavior.

**Acceptance:** all in-workspace callers compile; supported APIs and outputs are preserved or changes are approved/documented; safety tests remain; dependencies still point inward. New modules reduce duplicated state or clarify ownership, not just shorten files.

### M4 gate

Complete M1–M4 regression validation and diff review. Every finding has a resolution, a reproducible no-longer-applicable determination, or an explicit unresolved limitation. Unresolved P1 boundaries prevent claiming the affected capability hardened. Consolidation may be declared complete without implementing M5, but not by hiding an active correctness or security blocker.

## M5 — Extension contracts only on concrete demand

These tasks are **deferred**, not prerequisites for current migration completion.

| Task | Trigger and finding | Required work and gate |
| --- | --- | --- |
| M5.1 — Second provider protocol | A requested second concrete protocol; remaining P2.9 | Distinguish protocol family from direct/relay routing. Declare actual capabilities and continuation compatibility; add transcript/tool-call/error tests. Change config revisions only with a documented migration. No registry/framework by default. |
| M5.2 — External tool transport | A requested concrete plugin/transport | Define host trust, credentials, resource/approval scope, bounded process/transport lifecycle, cancellation, and failure outcomes. All dispatch stays under runtime policy. A subprocess is not a sandbox; no compliance or untrusted-code containment claim without tests. |
| M5.3 — HTTP/TLS replacement evaluation | A concrete maintenance/correctness need in current transport | Compare a mature implementation against hand-written HTTP/chunked/SSE/OpenSSL code using dependency size, build/startup cost, cancellation, verified TLS, bounded buffering, and protocol regressions. Keep this separate from permission refactoring; select a dependency only with justification. |

## Decisions required before behavior-changing implementation

| Decision | Proposed minimum | Must be resolved before |
| --- | --- | --- |
| D1 — Selection/error compatibility | Server sessions remain bound; conflicting submit fields are rejected; omitted selection uses the binding. TUI accepted runs keep their actual model despite later UI changes. | M1.1/M1.5 behavior changes; specify default-model equivalence and attribution rules |
| D2 — Real-tool server trust | Retain loopback exposure and establish minimal local caller/approver authorization; no OAuth/accounts or public listener. | M1.7; document token provisioning, browser access, demo exceptions, and startup compatibility |
| D3 — File operation guarantees | A concrete supported platform access boundary plus separately specified update consistency; no universal capability framework. | M2.3; choose symlink/hardlink/permissions/identity semantics and residual concurrency limits |
| D4 — API retirement | Remove/narrow a public legacy contract only after caller inventory and compatibility review; migrate meaningful tests. | M4.3 public API removal |
| D5 — Effective budgets | Finite explicit event/byte/cleanup/server bounds; distinguish configurable limits from hard ceilings and keep terminal reserve. | M1.3/M1.4/M1.6; record chosen values and evidence, no silent widening |

These are implementation review checkpoints, not permission to silently change accepted product defaults. Routine internal details can be resolved locally; material compatibility/security choices need confirmation or an accepted decision before coding them.

## Finding coverage

| Review finding | Migration tasks |
| --- | --- |
| P1.1 — Server selection | M1.1, M3.2 |
| P1.2 — End-to-end bounded streaming | M1.3 |
| P1.3 — Closed flusher | M1.2, M4.1 |
| P1.4 — Exec lifecycle | M1.4, M2.4 |
| P1.5 — File access/update races | M2.2–M2.3 |
| P1.6 — Server trust/resources | M1.6–M1.7 |
| P1 validation gap — Platform sandbox evidence | M2.4 |
| P2.1 — Name-based tool policy | M2.1 |
| P2.2 — Scope string protocol | M2.2 |
| P2.3 — Config copies | M3.1 |
| P2.4 — TUI usage attribution | M1.5, M3.1/M3.3; no second independent fix |
| P2.5 — Effective limits | M1.3 and D5 |
| P2.6 — Provisional/batch contract | M1.3 |
| P2.7 — Event ordering | M4.1 |
| P2.8 — Development wrapper/assembly | M3.2, M4.2 |
| P2.9 — Protocol/capability identity | M3.3 for current capabilities; M5.1 only for a requested second protocol |
| P2.10 — Parallel/legacy core models | M4.3 |
| P3 — Documentation drift | Each behavior-changing slice plus M4.3 |

## Validation and delivery workflow

For each task:

1. Review current code, applicable guidance, accepted requirements, callers, and tests; identify any compatibility decision. Do not treat draft proposals as implemented guarantees.
2. Add a targeted regression or characterization fixture before the fix where feasible. Use barriers/channels or injected I/O failures instead of timing-dependent correctness tests; any watchdog is a failure bound, not synchronization.
3. Make one focused implementation slice. Keep fixes separate from moves, renames, formatting churn, dependency changes, or unrelated frontend work.
4. Follow [Development tests](../testing.md). Register new tools/server/TUI integration modules in their `tests/behavior.rs` entrypoints. Run default-feature tests for affected packages and callers, then clippy, sequentially against the shared target directory.
5. For shared contract changes, include concrete adapters, fakes, and `nexus-integration` as applicable. Run Linux/macOS platform tests for affected OS behavior; report unavailable platforms as unverified.
6. Update affected contract/setup/security documentation and record results in the task status or existing evidence location. No extra report scaffolding is required.
7. Inspect the diff for accidental changes and sensitive data. Advance the gate only on evidence, not on implementation intent.

Typical affected-package commands (replace the package list for the slice; these are planned commands, not commands executed for this document):

```sh
cargo fmt --all --check
cargo nextest run -p nexus-runtime -p nexus-tools -p nexus-server -p nexus-tui --locked --offline
cargo clippy -p nexus-runtime -p nexus-tools -p nexus-server -p nexus-tui --all-targets --locked --offline -- -D warnings
```

Do not run redundant `cargo check` immediately before clippy. Optional/ignored tests follow `docs/testing.md`; the complete suite is appropriate for explicit final full validation or when changing its subject:

```sh
cargo nextest run --workspace --all-features --locked --offline --run-ignored all
```

For contract/core API changes, include applicable doctests as well; nextest does not replace them. For this plan-only edit, check document references, finding coverage, and the final diff instead of running application tests.

Every delivery records changed paths, regression names, commands/results, effective bounds where relevant, compatibility decisions, platform evidence, and unresolved limitations. Use status values **Not started**, **In progress**, **Blocked**, **Verified**, or **Deferred**. No tasks are marked verified in this plan.

## Migration safety and stop conditions

- Keep slices narrowly reversible and dependency-ordered; create commits only when requested. A rollback must not discard unrelated user work or weaken a verified security boundary to restore an older implementation.
- No session/config data migration is planned for M1–M4. If an implementation requires one, pause, define compatibility and recovery, and obtain approval rather than changing formats incidentally.
- Approval bindings from incompatible scope/policy changes must be rejected; they are never silently migrated or reused. Do not replay interrupted or uncertain operations during restart or rollback.
- Stop a slice when it requires a new public abstraction without concrete consumers, changes accepted permission defaults, introduces an unbounded fallback, cannot preserve required terminal/worker ownership, or depends on unapproved privileges/external cost.
- When local platform cleanup or race-free access cannot be guaranteed, report the limitation and block the corresponding guarantee. Do not infer support from passing fake or unavailable-backend tests.

## Related documents

- [Architecture review](../review/ARCHITECTURE_REVIEW.md)
- [Development tests](../testing.md)
- [Components and dependency boundaries](../design/01-components.md)
- [First-release defaults](../design/decisions/01-first-release-defaults.md)
- [M0 subset lock](06-m0-lock.md)
- [TUI stability backlog](07-tui-stability.md)
- [Engineering guidelines](../../AGENTS.md)
