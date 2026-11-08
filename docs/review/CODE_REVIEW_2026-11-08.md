# Nexus Agent: Whole-Workspace Code Review and Optimization Recommendations

**Review date:** 2026-11-08

**Code baseline:** `257a999c475bdb55646c962806899370a9a257c9`, version `0.2.5-alpha`, including the working tree available during review.

**Deliverable:** Review and recommendations only. No implementation, configuration, dependency, permission, or deployment changes were made.

**Language:** English, following the repository's documentation convention.

## 1. Assessment

The existing architecture is worth preserving. A dependency-free domain core, a shared runtime policy boundary, direct built-in tool calls, separate context computation, explicit effect uncertainty, bounded event transport, and replaceable frontend/provider implementations support the project's small-host direction. A rewrite, a new orchestration framework, or a database migration is not justified by this review.

The most urgent issue is **private replacement content temporarily receiving broader permissions on Linux**. Other important findings concern loss of uncertain-effect context when reusing a runtime across sessions, conservative tool-classification enforcement, discovery cancellation, malformed provider input, and resource bounds that do not cover some intermediate work.

For optimization, the strongest opportunities are visible in existing algorithms: avoid materializing discarded scrollback, repeated full Markdown parsing, quadratic streaming text copies, repeated session-snapshot decoding, and an OS thread per completion watcher. These are concrete sources of work; the size of any end-to-end improvement still needs measurement.

The review also distinguishes current behavior from accepted limitations. External plugins, production container isolation, arbitrary detached-descendant termination, bare-metal embedded execution, and universal provider compatibility are not implemented guarantees. Their absence is not reported as a newly discovered regression.

### Scope and coverage

| Area | Reviewed implementation and regression surfaces |
| --- | --- |
| Domain and validation | All `nexus-core` and `nexus-validation` modules: identities, approvals, capabilities, content, outcomes, limits, strict JSON parsing, schema compilation, canonical arguments |
| Execution and context | `nexus-runtime`, `nexus-context`: admission, dispatch, cancellation, quarantine, ordered events, compaction, summaries, retained evidence, restoration |
| Persistence | `nexus-session`: file/directory stores, recovery, transaction publication, locking, capacity, interrupted writes |
| Models and configuration | `nexus-openai`, `nexus-config`: three protocol families, HTTP/TLS/SSE, retries, credentials, model settings, layered JSONC, save behavior |
| Tools and platforms | `nexus-tools`, `nexus-permissions`: descriptor-bound paths, read/list/search/write/patch, metadata, sandboxed processes, cleanup |
| Containers | `nexus-container`, worker/keeper binaries, image recipes, gateway policy, ownership records, framing, lifecycle and transport tests |
| Frontends | `nexus-tui`, `nexus-server`, `nexus-headless`: composition, input, rendering, clipboard, sessions, HTTP authentication, SSE, machine output |
| Test infrastructure | `nexus-fakes`, `nexus-integration`, package test registration, nextest configuration, GitHub Actions |
| Supporting tools | Installation/configuration/release scripts; benchmark preparation, launch, evaluation and metrics; performance harnesses and their tests |
| Design consistency | Current README, accepted decisions, contracts, migration records, and the previous architecture/M1 reviews |

The working tree already contained README and benchmark-related edits. They were preserved and were subsequently included in commit `7c54a917694ad70ff01cbcab5edc6869f0922d03` during the review. The final comparison with the starting baseline showed changes only to README and two benchmark files; production sources were unchanged. Production-source references below refer to this review baseline; later edits may move line anchors. Historical review findings were checked against current implementations rather than copied forward.

### Severity and evidence

- **P1:** Address promptly because ordinary supported behavior can expose private data or undermine a critical boundary.
- **P2:** Address before expanding the affected integration or device claims; material correctness, lifecycle, portability, or resource issue.
- **P3:** Lower-impact hardening or maintenance issue.
- **Reproduced:** A deterministic current-code probe exercised the behavior.
- **Source-confirmed:** The relevant branch/order is directly present, but the complete platform scenario was not executed.
- **Optimization/risk:** An identified cost or missing experiment, without an unsupported speedup or exploit claim.

## 2. Findings at a glance

| ID | Priority | Finding | Evidence / scope |
| --- | --- | --- | --- |
| F01 | P1 | Linux replacement staging can expose a private file's content | Source-confirmed Linux branch; isolated POSIX permission-order analogue |
| F02 | P2 | Restoring one session can discard another session's uncertain active evidence without requiring its restoration | Reproduced through the public Runtime API; standard frontends normally isolate runtimes |
| F03 | P2 | Recognized tool names can override the conservative default `Unknown` classification in development mode | Reproduced; default/unclassified registration, not the correctly classified shipped composition |
| F04 | P2 | Context discovery delays terminal cancellation/deadline handling until its worker returns | Reproduced responsiveness limitation; current test explicitly permits this ordering |
| F05 | P2 | User configuration is fully read before the size limit applies | Source-confirmed bounded-input gap |
| F06 | P2 | SSE parsing silently replaces malformed UTF-8 | Reproduced with the current parser |
| F07 | P2 | Complete JSON fallback responses still wait for EOF | Reproduced with a deterministic chunk source |
| F08 | P2 | TLS request-pipe upload is outside the configured setup timeout | Reproduced with a local non-reading helper |
| F09 | P2 | Clipboard helpers block the UI path and skip reaping after a failed write | Source-confirmed; process-level clipboard probe inconclusive |
| F10 | P2 | Recursive search has no frontier/aggregate traversal budget | Source-confirmed intermediate allocation gap |
| F11 | P2 | Container capacity is reserved after gateway creation | Source-confirmed admission ordering |
| F12 | P2 | The provided workload image does not install the advertised Bash interpreter | Source-confirmed adapter/image mismatch |
| F13 | P2 | Cooperative filesystem timeouts are recorded as generic failures | Source-confirmed outcome mapping |
| F14 | P3 | Provider response headers can exceed their cap by the chunk containing the delimiter | Source-confirmed finite overshoot |

## 3. Detailed correctness and lifecycle findings

### F01 — P1: Create replacement stages privately before writing content

**References:** [fs_access.rs:233](../../crates/nexus-tools/src/fs_access.rs#L233), [fs_access.rs:282](../../crates/nexus-tools/src/fs_access.rs#L282), [fs_access.rs:353](../../crates/nexus-tools/src/fs_access.rs#L353), [fs_access.rs:464](../../crates/nexus-tools/src/fs_access.rs#L464).

**Trigger and impact:** Replace or patch a `0600` file in a directory another local user can traverse, under a typical `022` umask. The non-macOS replacement branch calls `create_temporary`, which requests mode `0666`. `replace_existing` writes the complete replacement at line 237 and restores source metadata afterward. The populated intermediate file can therefore be `0644`. An observer who opens it during that interval can retain access after chmod or rename. Patch replacement can include original private content as well as the modification.

**Evidence:** The Linux branch and operation order are explicit in source. A temporary POSIX syscall-order analogue observed `source=0600`, `populated stage=0644`, then `stage=0600` after metadata restoration. This was not a native Linux Rust execution or a cross-UID exploit test. The macOS clone-based branch has different metadata behavior and is not assigned this finding.

**Recommendation:** Give replacement staging a private `0600` creation mode and preserve safe ACL restrictions while writing. Restore intended final metadata before publication. Keep new-file final-mode semantics separate; do not simply change every creation's final permissions. Preserve exclusive/no-follow opens, stamps, metadata failure handling, atomic rename, and cleanup.

**Verification:** Add deterministic checks at the populated-stage boundary for private sources, patching, cancellation, and metadata-copy failure. Cover Linux default ACLs. On an appropriate unprivileged Linux reference host, verify that another UID cannot read the stage. This issue is distinct from the already documented final compare/rename race.

### F02 — P2: Invalidate displaced session admission state after restoration

**References:** [runtime.rs:876](../../crates/nexus-runtime/src/runtime.rs#L876), [runtime.rs:899](../../crates/nexus-runtime/src/runtime.rs#L899), [runtime.rs:3647](../../crates/nexus-runtime/src/runtime.rs#L3647), [runtime.rs:3684](../../crates/nexus-runtime/src/runtime.rs#L3684).

**Trigger:** Reuse one Runtime for session A and session B. Complete an A run containing an `Unknown/Uncertain` tool result, explicitly restore B, then submit A again without restoring A.

**Evidence:** The probe recorded uncertain evidence in A's original follow-up request. Restoring B returned `Accepted`; returning to A also returned `Accepted`, but its model request contained only the new user item and a generic omission notice. `unknown_effect_retained=false`. `restore_session_inner` clears all history, including other sessions, while their entries can remain in `durable_sessions`. The persistence admission check then treats A as already initialized.

**Impact and scope:** The durable archive is not deleted. The defect is that new decisions can proceed without previously pinned uncertainty evidence. Normal TUI slots and server sessions usually construct separate runtimes, reducing current application exposure; embedders using the public multi-session API can trigger it.

**Recommendation:** Invalidate the loaded/admission status of displaced sessions, or preserve their safety-critical context. Returning to a discarded durable session must require explicit restoration before a new model/tool action. A generic omission notice is insufficient for unresolved effects.

**Verification:** A deterministic A-unknown → restore-B → submit-A regression must reject or defer A until its source evidence is restored. Also cover known-effect history, empty sessions, cancellation, and store errors; do not replay operations or restore grants.

### F03 — P2: Preserve the conservative registration category in development policy

**References:** [tool.rs:25](../../crates/nexus-core/src/tool.rs#L25), [policy.rs:192](../../crates/nexus-runtime/src/policy.rs#L192), [policy.rs:205](../../crates/nexus-runtime/src/policy.rs#L205), [runtime.rs:2784](../../crates/nexus-runtime/src/runtime.rs#L2784).

**Trigger:** Register a fixture through `ToolRegistration::new`, leaving its host classification `Unknown`, but give its descriptor a recognized built-in identity such as `host_write` and a project/temp target.

**Evidence:** The core documents `Unknown` as confirmation-required. Development `call_requires_approval` instead auto-authorizes a recognized automatic path without considering that category. The later validation phase overwrites the initial approval decision. A no-I/O runtime fixture named `host_write` executed once with `has_approval_handler=false`, emitted zero approval events, and completed.

**Scope:** Standard built-in constructors supply explicit classifications and are not shown to be misclassified. This is a conservative-default/API contract defect, not proof that arbitrary model text can register a tool or bypass a correctly classified shipped tool.

**Recommendation:** Reconcile the policy's recognized-target behavior with the documented conservative default. Keep `Unknown` confirmation-required before automatic target grants, and validate expected category/name combinations where appropriate. Preserve intended automation for explicitly classified project mutations/processes. Container policy already has a category/name compatibility check that provides a local pattern.

**Verification:** Test recognized names across `Read/Mutation/Process/Unknown`, project/temp/external targets, strict/development modes, read-only runs, and missing approval handlers. Existing strict-classification and custom-name tests miss the recognized-name/Unknown combination.

### F04 — P2: Bound discovery's terminal-response wait without dropping its worker

**References:** [runtime.rs:1377](../../crates/nexus-runtime/src/runtime.rs#L1377), [main.rs:8191](../../crates/nexus-tui/src/main.rs#L8191).

**Observed behavior:** The runtime directly awaits `spawn_blocking(discovery)`. A fixture with a 25 ms run budget still had an active run and no terminal result after 100 ms, although `Cancel` returned `Accepted`. Releasing discovery then produced `Cancelled` with zero model turns.

**Interpretation:** Command handling remains responsive and ownership is retained. The current test explicitly expects completion after discovery returns. This is a confirmed responsiveness/deadline limitation requiring an explicit behavior decision, rather than false cleanup or an unowned-worker claim. A blocking resolver or custom discovery closure can make the terminal wait much longer than the nominal discovery setting.

**Recommendation:** Define cancellation/deadline behavior for discovery consistently with other blocking workers: publish an honest terminal state when appropriate, retain/quarantine the worker, prevent conflicting admission, and discard late discovery results. Do not detach and forget the closure or pretend a blocking syscall was killed.

**Verification:** Hold discovery behind a deterministic gate, cancel or expire the run, and observe the agreed terminal bound before gate release. Assert that ownership remains visible, no model/tool is invoked, and releasing the late worker cannot modify a later run.

### F05 — P2: Enforce configuration bounds while reading

**References:** [store.rs:150](../../crates/nexus-config/src/store.rs#L150), [settings.rs:353](../../crates/nexus-config/src/settings.rs#L353), [config.sh:187](../../scripts/config.sh#L187).

`nexus_config::load` calls unbounded `read_to_string` before the 65,536-byte parser limit. The interactive helper likewise reads the complete selected config before checking its encoded size. A mistakenly selected large/growing file can therefore exhaust startup memory before rejection. Rejecting special files or bounding blocking reads is an additional compatibility decision; a byte cap alone does not provide a time bound.

**Recommendation:** Use bounded limit-plus-one reads, following the already bounded operational-settings/credential paths. Preserve missing-file versus malformed-file behavior and static diagnostics. A metadata-length check alone is insufficient for files that grow during the read.

**Verification:** Use an injected counting reader for the exact limit and limit-plus-one case; test invalid UTF-8, duplicate keys, a changing file, and ordinary missing-file defaults. Decide and document whether non-regular config sources are supported.

### F06 — P2: Reject malformed SSE UTF-8 instead of repairing it

**References:** [provider.rs:2135](../../crates/nexus-openai/src/provider.rs#L2135), [provider.rs:2548](../../crates/nexus-openai/src/provider.rs#L2548), [provider.rs:3345](../../crates/nexus-openai/src/provider.rs#L3345).

The SSE paths convert a completed line with `String::from_utf8_lossy` before JSON parsing. A current-source probe containing byte `0xff` inside a JSON string returned text containing U+FFFD and a successful `Stop`. The conversion also affects tool names, identities, and argument strings, because it precedes field extraction. Plain JSON parsing is stricter.

**Impact:** Malformed wire data can become successful altered content or an altered proposal. Host argument validation and authorization still apply; no separate authorization bypass was demonstrated.

**Recommendation:** Decode complete records strictly with `from_utf8`, returning a static `Protocol` failure on invalid encoding. Continue accumulating bytes across transport chunks before decoding so valid split multibyte characters remain supported.

**Verification:** Exercise all three protocol families with invalid text and tool-argument bytes, and valid UTF-8 split at each byte boundary. Invalid records must not yield dispatchable candidates or completed success.

### F07 — P2: Finish non-SSE responses at their framing boundary

**References:** [provider.rs:1770](../../crates/nexus-openai/src/provider.rs#L1770), [provider.rs:1787](../../crates/nexus-openai/src/provider.rs#L1787), [provider.rs:2739](../../crates/nexus-openai/src/provider.rs#L2739).

The JSON fallback reads until EOF before inspecting `Content-Length`, and the eventual length check only rejects short bodies. Chunked JSON likewise waits for EOF. A deterministic source delivered a complete valid fixed-length answer and then reported an idle timeout; the adapter returned `Failed(Timeout)` instead of the available answer.

**Recommendation:** Parse framing explicitly and complete fixed-length/chunked bodies at their declared terminal boundary. Reject invalid/conflicting framing, retain independent wire/content caps, and make invalid chunk decoding an explicit failure instead of falling back to treating framed bytes as a raw body. Requesting `Connection: close` does not eliminate the value of correct response framing.

**Verification:** Complete fixed-length and chunked responses must succeed without asking a source for another chunk. Cover truncation, fragmented delimiters, conflicting lengths, invalid terminal chunks, cancellation, and oversized initial body data.

### F08 — P2: Include TLS request-pipe setup in the configured timeout

**References:** [provider.rs:1460](../../crates/nexus-openai/src/provider.rs#L1460), [provider.rs:1518](../../crates/nexus-openai/src/provider.rs#L1518), [provider.rs:1657](../../crates/nexus-openai/src/provider.rs#L1657).

`spawn_tls` waits for its stdin writer under the overall deadline. The connection timer starts only after that write completes. A local fake helper that did not consume a legal 2 MiB request, with a one-second connection setting and three-second overall deadline, returned at approximately **3,003 ms** in the repeated probe. This measurement starts inside the probe, excluding executable launch delay.

**Recommendation:** Establish the setup deadline before helper launch and apply it to blocked pre-response writes. Clarify whether this setting means connection/setup or first-response time; keep any upload deadline explicit. Preserve cancellation polling and ownership of incomplete helper/pump cleanup.

**Verification:** Test non-reading, slowly consuming, and successful helpers; cancellation during upload; setup expiry shorter than the overall run; and bounded cleanup with uncertain termination. No external TLS service is required for these regressions.

### F09 — P2: Keep clipboard process I/O off the terminal event loop

**References:** [main.rs:1313](../../crates/nexus-tui/src/main.rs#L1313), [main.rs:2507](../../crates/nexus-tui/src/main.rs#L2507), [main.rs:2725](../../crates/nexus-tui/src/main.rs#L2725), [main.rs:3830](../../crates/nexus-tui/src/main.rs#L3830).

Mouse release and keyboard copy invoke the clipboard writer inline on the current-thread runtime. `stdin.write_all` and `child.wait` have no deadline. A helper stalled on its input/display connection can therefore stop redraw, event draining, approvals, and cancellation handling. Also, `wrote && child.wait()` skips waiting when a pipe write fails; dropping a Rust Child does not establish reaping.

**Evidence boundary:** These control paths are source-confirmed. A temporary process-level clipboard probe did not establish helper startup in this environment and is not counted as a successful reproduction or a measured zombie observation.

**Recommendation:** Use a single bounded, owned copy job off the UI path, with a deadline covering write and wait, bounded payload/admission, and completion delivered back to the UI. Always reap or explicitly retain the child on failure. Keep OSC 52 output on the terminal owner and preserve explicit backend/fallback semantics. Clipboard helpers may intentionally daemonize; do not indiscriminately kill successful clipboard-owner descendants.

**Verification:** Inject helpers that close stdin, never read a pipe-filling payload, and consume input but never exit. Assert continued input/output/cancellation progress and eventual ownership reconciliation, rather than testing only backend selection.

### F10 — P2: Bound the search frontier and total traversal

**References:** [fs.rs:576](../../crates/nexus-tools/src/fs.rs#L576), [fs.rs:612](../../crates/nexus-tools/src/fs.rs#L612).

Recursive search pushes every discovered sibling directory into a `Vec<PathBuf>` without a count/byte frontier cap. A wide no-match tree can accumulate substantial path memory without producing output. Per-file bytes and output limits do not constrain that intermediate queue; a deadline does not itself bound allocations within its interval.

**Recommendation:** Add explicit queued-path count/bytes and aggregate visited-entry budgets. Surface exhaustion as truncation or the chosen explicit limit outcome. Preserve cancellation and existing path restrictions. Do not silently exclude directories merely to improve measurements unless the search contract permits those exclusions.

**Verification:** Use wide, deep, and long-path no-match fixtures with small injected limits; verify bounded frontier growth, deterministic exhaustion reporting, and responsive cancellation. Keep existing per-file and output-limit tests.

### F11 — P2: Reserve a container lifecycle slot before creating external resources

**References:** [workload.rs:803](../../crates/nexus-container/src/workload.rs#L803), [workload.rs:819](../../crates/nexus-container/src/workload.rs#L819), [workload.rs:881](../../crates/nexus-container/src/workload.rs#L881).

The public composition path creates/starts a gateway and its ownership record before `ContainerWorkloadSession::prepare` reserves one of the two lifecycle slots. A full-capacity attempt can therefore mutate Docker state before rejection. Concurrent preparations can exceed the intended cap transiently; uncertain cleanup of a rejected gateway has no reserved slot to retain.

**Recommendation:** Acquire admission at the outer preparation boundary before the first external mutation. Carry the slot through gateway/workload preparation and retain it if cleanup is uncertain. Keep exact ownership verification and fail-closed stale-state handling.

**Verification:** With capacity exhausted, the fake runner must observe zero create/start calls. Inject failed cleanup during preparation and verify that both capacity and ownership evidence remain retained. No claim of general container isolation is needed to test this resource invariant.

### F12 — P2: Align the provided image with the Bash execution contract

**References:** [exec.rs:117](../../crates/nexus-container/src/exec.rs#L117), [Dockerfile:18](../../containers/workload/Dockerfile#L18).

The container adapter advertises `shell_command` and dispatches `/bin/bash -c`. The supplied minimal Alpine runtime installs CA certificates and checks `/bin/sh`, but does not install Bash. Its health check can therefore pass while the advertised shell form is unavailable. Custom task images containing Bash are a separate case.

**Recommendation:** Either package/pin Bash and update image provenance, or explicitly review and revise the shell contract. Do not silently substitute POSIX sh for Bash. A bounded capability/admission check can detect missing required executables before exposing tools.

**Verification:** Add missing-shell capability fixtures and an opt-in smoke against the built default image that actually executes a Bash command. The current fake-transport argv assertion cannot verify image contents. This review did not build or run Docker images.

### F13 — P2: Preserve filesystem deadline causes in tool outcomes

**References:** [fs.rs:279](../../crates/nexus-tools/src/fs.rs#L279), [fs.rs:1385](../../crates/nexus-tools/src/fs.rs#L1385), [runtime.rs:3427](../../crates/nexus-runtime/src/runtime.rs#L3427).

The filesystem failure mappers recognize `Cancelled`, but map `ErrorCategory::Timeout` to generic `Failed` and I/O prose. If an operation cooperatively observes expiry and returns within the runtime's cleanup grace, the runtime preserves that result rather than replacing it with a timeout. The model and source store then lose the actual deadline cause. Pre-access development-wrapper handling does not cover expiry inside the operation.

**Recommendation:** Map timeouts explicitly to `TimedOut` with a static deadline diagnostic. Preserve conservative effect/evidence states. Use known-not-applied only when staging/publication state proves it; a timeout alone is not rollback.

**Verification:** Cover expired admission and gated mid-read/mid-staging expiry for all five filesystem tools, then preserve the result across worker encoding and runtime recording.

### F14 — P3: Check the discovered response-head boundary against the cap

**References:** [provider.rs:1729](../../crates/nexus-openai/src/provider.rs#L1729).

The head cap is checked before reading a chunk. If the next chunk contains a delimiter beyond the 16 KiB boundary, the loop exits without checking that offset. Current transport chunk sizes make the excess finite, but an oversized header can still be accepted.

**Recommendation and verification:** Validate the discovered head-end offset and enforce the cap after an append without a delimiter. Test exact-boundary and one-byte-over heads with delimiters split across chunks and a body immediately following the head.

## 4. Optimization recommendations

The following items preserve current responsibilities. Complexity/allocation observations are source-based; they are not measured percentage improvements.

### O01 — Render the requested viewport, not the rows discarded below it

**References:** [state.rs:1907](../../crates/nexus-tui/src/state.rs#L1907), [state.rs:1940](../../crates/nexus-tui/src/state.rs#L1940).

`visible_lines` sets `need = height + scrollback`, materializes up to that many rows, and then truncates `scrollback` rows. At the top of a long transcript it can materialize almost the entire retained transcript to return one screen. Raw retention is bounded, but transient text/style/title allocations scale with discarded rows, contradicting the viewport-only comment.

Compute the visible end as `total.saturating_sub(scrollback)`, derive its start from height, and stop materializing at that end. Keep height counting/caching for coordinates; a new indexing system is unnecessary for this fix. Verify exact lines, styles, titles, and `hidden_above` at top/middle/tail, CJK/narrow widths, folds, and an oversized entry. Instrument materialized-row counts or allocation bytes; checking final `lines.len()` alone misses the cost.

### O02 — Parse assistant Markdown once per relevant revision/frame

**References:** [state.rs:365](../../crates/nexus-tui/src/state.rs#L365), [state.rs:451](../../crates/nexus-tui/src/state.rs#L451), [markdown.rs](../../crates/nexus-tui/src/markdown.rs).

Height calculation and range rendering separately parse the complete assistant body. Streaming invalidates height repeatedly, and visible entries can be reparsed on subsequent frames even when only a small slice is shown.

Start by sharing a parsed result within one frame/entry revision. Then measure whether bounded caching of completed visible entries is worthwhile. Charge cached representation bytes to an explicit budget; caching cannot silently evade the current raw-entry/aggregate memory limits. Preserve Markdown block semantics across streamed fragments and resize. Benchmark long code blocks, tables, many short deltas, and idle caret redraws before attempting a custom incremental parser.

### O03 — Remove avoidable repeated provider work

**References:** [provider.rs:2328](../../crates/nexus-openai/src/provider.rs#L2328), [provider.rs:2567](../../crates/nexus-openai/src/provider.rs#L2567), [provider.rs:2715](../../crates/nexus-openai/src/provider.rs#L2715).

- Anthropic text deltas rebuild the full accumulated block with `format!("{existing}{text}")`, while also appending to `response_text`. Repeated tiny deltas cause quadratic copying. Mutate the existing string with `push_str`; replace duplicate complete-text retention with counters where final encoding permits it.
- Chat SSE parses each JSON record to inspect tool identities, then parses it again in `apply_sse_data`. Pass the already parsed value while preserving pre-mutation identity budgets.
- `take_line` repeatedly scans an incomplete tail and drains the buffer front. Use scan/consume offsets and compact after a batch; preserve record/wire caps.

Compare identical 1-, 16-, and 256-byte-fragment payloads. Record copied/allocated bytes, CPU and peak memory alongside wall time. Preserve provisional/final identity agreement, content order, cancellation, and hard-limit outcomes.

### O04 — Load a selected session once during restoration

**References:** [session lib.rs:113](../../crates/nexus-session/src/lib.rs#L113), [session lib.rs:338](../../crates/nexus-session/src/lib.rs#L338), [session lib.rs:763](../../crates/nexus-session/src/lib.rs#L763).

Recovery requests 256-record pages. `DirectorySessionStore::load_source_records` reloads and decodes the entire selected snapshot for each page. At the 16,384-record bound, this can involve one checkpoint load plus 65 page requests, including the final empty page. Paging therefore bounds returned records without making the underlying reads incremental.

Use one immutable selected-session recovery snapshot or narrowly scoped cache under the existing single-owner boundary. Preserve sequence validation, uncertain-write rejection, format checks, and metadata-only startup. Measure decode count, bytes read, wall time and peak RSS across increasing histories; do not replace this with eager loading of every saved session.

### O05 — Reduce snapshot write amplification while retaining write-ahead intent

**References:** [session lib.rs:813](../../crates/nexus-session/src/lib.rs#L813), [session lib.rs:832](../../crates/nexus-session/src/lib.rs#L832), [session lib.rs:777](../../crates/nexus-session/src/lib.rs#L777).

Intent, outcome, and source updates can clone records, encode/decode the full snapshot, and rewrite/fsync it repeatedly. The directory store also reloads selected bodies around updates. Total work grows with both history length and operation count.

First reuse a validated loaded selected session and remove provably redundant encode/decode work. Batch only records whose existing durability ordering permits batching. An operation's intent must still be durable before dispatch; observed uncertainty must survive interruption. A journal or database is a later format/design change justified only by measurements and crash-recovery tests, not the first response to this cost.

### O06 — Use the existing executor for completion observation

**References:** [server.rs:1193](../../crates/nexus-server/src/server.rs#L1193).

The server starts a named OS watcher thread per bound-model run, polling every 100 ms, with a finite 128-watcher cap. This is bounded and independently settles usage without an SSE consumer, but is a substantial thread/wakeup surface for a small host.

Use an existing-runtime task or one bounded observer owner. Preserve registration before completion, exact run/model attribution, cleanup and no-subscriber accounting. Measure thread count, idle wakeups, RSS and settlement delay at 1/16/64/128 sessions; do not infer resident stack memory from virtual stack reservations.

### O07 — Apply HTTP read timeouts at buffer refill boundaries

**References:** [http.rs:89](../../crates/nexus-server/src/http.rs#L89), [http.rs:205](../../crates/nexus-server/src/http.rs#L205).

Each header byte triggers `set_read_deadline` even when `BufReader` already holds the bytes. This can repeatedly configure the socket without another network read. Scan buffered slices and update timeouts before actual refills, retaining the absolute request deadline and head/body caps. Verify slow split headers, partial bodies, malformed authentication headers, and cancellation/closure behavior. Count read/timeout-setting calls before claiming a speedup.

### O08 — Offer measured local-only and container-enabled distribution choices

**References:** [install.sh:17](../../scripts/install.sh#L17), [package-release.py:17](../../scripts/package-release.py#L17), [Cargo.toml](../../Cargo.toml).

Source installation and packaging always enable container features even though ordinary local execution is the default. Introduce an explicit local-only distribution choice if demand warrants it, preserving today's feature-complete option and documenting the capability difference.

Measure default/local-only versus container-enabled builds at the same source, compiler and lockfile. Separately evaluate stripping, thin LTO, codegen units and size optimization profiles. Record executable, archive, dependency footprint, startup and full-session behavior. Do not switch to `panic=abort` merely for size: panic supervision, uncertain-effect reporting and terminal restoration need an explicit compatibility review. Existing old archive measurements must not be relabeled as current binaries.

### O09 — Validate a constrained-device profile before expanding device claims

**References:** [base.jsonc](../../config/base.jsonc), [settings.rs](../../crates/nexus-config/src/settings.rs), [workload.rs:539](../../crates/nexus-container/src/workload.rs#L539), [workload.rs:1124](../../crates/nexus-container/src/workload.rs#L1124).

Existing settings already cover connection/session counts, queues, tool bytes, deadlines and context budgets. Start with lower validated settings on a small Linux host or appropriately constrained VM; avoid a separate embedded architecture.

Record the Agent, OpenSSL helper, tool children and model service separately and as a complete process tree. Test cold/warm launch, first HTTPS use, slow storage, long histories and cancellation under load. Cross-compilation alone is not runtime support.

Container UID/GID `1000` is a documented prototype choice. The resolver bind file is host-owner-only `0600`; native Linux users with a different UID may also encounter resolver readability and ordinary workspace-write problems. Prefer actionable admission diagnostics or a reviewed UID mapping design over broad chmod/chown. Fixed container 4 GiB/2-CPU limits are ceilings, not eager reservations or established minimum device requirements. Bare-metal/RTOS/no_std support is not implied by these optimizations.

### O10 — Size and reuse Linux metadata buffers

**References:** [fs_access.rs:419](../../crates/nexus-tools/src/fs_access.rs#L419), [fs_access.rs:452](../../crates/nexus-tools/src/fs_access.rs#L452).

Metadata preservation allocates a 1 MiB names buffer and another 1 MiB-capacity value buffer per attribute. Query bounded required sizes and reuse a value buffer where the APIs permit it. Preserve attribute-size ceilings, unsupported-filesystem policy, ownership/mode/ACL handling, and fail-closed behavior. Evaluate this after F01; lower allocation must not weaken metadata or staging privacy.

### O11 — Keep benchmark replay memory proportional to summaries, not all raw events

**References:** [context_metrics.py:11](../../tools/benchmarks/context_metrics.py#L11), [context_metrics.py:29](../../tools/benchmarks/context_metrics.py#L29).

`load_events` materializes an entire JSONL run before `derive_metrics` iterates it. Long diagnostic traces can make the Python analysis process consume much more memory than the Agent being characterized. Add an iterator-based internal replay path and a bounded line/record policy; retain compatibility where callers need a list. Report malformed/truncated capture honestly rather than silently dropping records to fit a budget. This is tooling optimization, not a claim that the runtime itself retains these JSONL objects.

### O12 — Close low-cost CI and documentation gaps

**References:** [ci.yml](../../.github/workflows/ci.yml), [test-release.py](../../scripts/test-release.py), [test-config.py](../../scripts/test-config.py), [docs index](../index.md), [Quickstart](../../QUICKSTART.md), [CONTRIBUTING](../../CONTRIBUTING.md), [SECURITY](../../SECURITY.md).

CI exercises Cargo, benchmark and performance tests, but the release/configuration helper regression scripts and pure container-policy tests are not included in the current workflow. Add these existing offline suites to an appropriate job, with the built `nexus-server` available for helper validation. Avoid another redundant full workspace run.

Some introductions and manifest comments still describe ephemeral-only sessions, incomplete M0 acceptance, HTTP-only providers, or absent real tools after those descriptions became historical. Align current summaries with implemented scope while retaining dated evidence and production-readiness limitations. Keep the old reviews clearly historical; this report does not automatically supersede accepted decisions.

As small cleanup, the headless entry point duplicates task joining/empty validation around mode reporting. Remove that duplication only when touching the entry point; it is not a reason to redesign the headless runner or imply live headless support.

## 5. Risks and follow-up experiments, not confirmed new defects

1. **Blocking system DNS:** Provider connect/discovery paths use synchronous `to_socket_addrs` before later deadline checks. Inject a delayed resolver to characterize cancellation and ownership. Avoid one unbounded detached resolver thread per retry.
2. **Gateway helper lifecycle:** Some gateway commands still use reader threads and direct waits, unlike the bounded nonblocking workload/worker runner. Test read/wait failures and held pipes before reusing a small private runner where contracts match. Deliberately hostile detached descendants remain outside the current guarantee.
3. **Anthropic content-block overhead:** Wire and record counts are finite, but distinct content blocks do not have a smaller dedicated cardinality cap. Measure many small blocks and define supported/unknown-block retention before adding a cap that could break continuation fidelity.
4. **Real TLS trust-store behavior:** Verification flags are present; this review did not establish all system OpenSSL/LibreSSL combinations. Add local CA, wrong-hostname, untrusted-certificate and cancellation fixtures without modifying the user's trust store or calling cloud APIs.
5. **Cross-process config saves:** Last-successful-rename-wins is explicitly documented in the configuration contract. Merge/locking semantics are an optional product change, not an undisclosed defect. Preserve the existing single TUI configuration owner.
6. **Retry behavior:** Retrying recoverable remote access failures until success/cancellation/deadline is accepted behavior. Do not add an arbitrary attempt cap as a supposed bug fix without a requirements decision.
7. **First-launch sensitivity in validation:** Five container transport tests failed in the unrestricted full workspace run and then their complete ten-test module passed serially. Parallel load or first-execution effects are possibilities, not proven causes. Preserve the failures, collect pre-main/helper-start timing, and establish a reproducible cause before changing timeouts or platform-security settings.

## 6. Recommended delivery order

| Order | Small, reviewable work package | Completion criteria |
| --- | --- | --- |
| 1 | F01 private staging | Restrictive intermediate permissions, Linux ACL coverage, unchanged replacement/metadata guarantees |
| 2 | F02/F03 runtime safety boundaries | Multi-session uncertainty restoration and conservative-category regressions; standard composition unchanged |
| 3 | F06/F07/F14 provider framing/integrity | Strict UTF-8, explicit body completion, exact head limits across protocol fixtures |
| 4 | F04/F08/F09 lifecycle responsiveness | Agreed terminal/setup bounds, live cancellation, retained worker/child ownership, responsive UI |
| 5 | F05/F10 bounded local work | Limit-plus-one reads, bounded search frontier, honest truncation |
| 6 | F11/F12/F13 container and outcome consistency | Admission precedes resource creation; image capability matches tools; deadlines retain their cause |
| 7 | O01/O03/O04 measured allocation reductions | Correctness-equivalent views/events/recovery; lower measured allocation/work on fixed fixtures |
| 8 | O02/O05/O06/O07 long-session efficiency | Bounded caches, preserved durability/accounting/deadlines, before/after process-tree and I/O data |
| 9 | O08–O12 distribution/device/tooling closure | Reproducible capability-labeled artifacts, tested device profile, existing suites in CI, current summaries |

Each package should be a separate focused change with its relevant tests. Do not combine a transport rewrite, session format migration, permission redesign and embedded port into one optimization effort.

## 7. Validation performed and its limits

**Host:** macOS/Darwin arm64, rustc/cargo 1.99.0, Python 3.13.5. Cargo checks were run sequentially against the shared target directory. No production service, cloud model, paid API, Docker daemon operation, Linux VM, or user credential was used for this review.

| Command / check | Result |
| --- | --- |
| `cargo nextest run --workspace --locked --offline --no-fail-fast` inside the restricted execution sandbox | 2,503 passed, 159 failed, 20 skipped; many failures explicitly involved forbidden loopback binds, process inspection, or nested OS sandbox operations. Not classified as 159 product defects. |
| Same default-feature workspace command outside that sandbox | 2,657 passed, 5 failed, 20 skipped across 122 binaries; all five failures were container `worker_transport` tests, primarily at the five-second context probe. The run is not reported as clean. |
| `cargo nextest run -p nexus-container --locked --offline -j 1 -E 'test(worker_transport::tests::validates_context_and_runs_fixed_command_with_clean_environment_and_eof)'` outside the sandbox | 1 passed; 83 filtered/skipped. |
| `cargo nextest run -p nexus-container --locked --offline -j 1 -E 'test(worker_transport::tests::)'` outside the sandbox | All 10 passed; 74 filtered/skipped. This does not erase the workspace-run failures. |
| `cargo fmt --all -- --check` | Passed. |
| `cargo clippy --workspace --all-targets --all-features --locked --offline -- -D warnings` | Passed with warnings denied. |
| `python3 -B -m unittest discover -s tools/benchmarks -p 'test_*.py'` | 123 passed. Includes the local benchmark edits present during review. |
| `python3 -B scripts/test-release.py` | 9 passed; no native artifact rebuild. |
| `PATH="$PWD/target/debug:$PATH" python3 -B scripts/test-config.py` outside the sandbox | 18 passed, using temporary configurations and local fake services. |
| `python3 -B -m unittest -v containers/test_policy.py` | 10 passed; pure policy tests, not Docker isolation evidence. |
| `python3 -B -m unittest discover -s tools/perf -p 'test_*.py'` inside the sandbox | 282 run: 2 failures, 2 errors, 1 skip; unavailable process/RSS observations included explicit `/bin/ps` permission errors. |
| Same performance-tool tests outside the sandbox | Passed: 282 run, 1 platform skip. These validate harness behavior, not new application performance numbers. |
| Temporary current-runtime probes | Reproduced F02, F03 and F04 using fixture tools/providers; no real mutation or model request. |
| Temporary current-source provider probes | Reproduced F06, F07 and F08 using deterministic bytes/chunk sources and a local fake OpenSSL helper; no network. |
| Temporary staging-permission analogue | Observed `0600 → populated 0644 stage → 0600`; not a native Linux Rust/cross-UID test. |
| Temporary clipboard subprocess probe | Inconclusive helper-start observation; F09 remains source-confirmed. |

The provider probe included the current production provider source verbatim, with same-module probes appended, and linked the compiled workspace dependencies. Runtime probes used the current compiled runtime/core/store interfaces. Their diagnostic results were inspected again by the primary reviewer. Temporary probe files/logs under `/tmp/nexus-review-*` are local review artifacts, not committed regression tests or a required permanent reproduction format.

**Not run:** the complete all-feature/ignored execution suite, Rust doc tests, a fresh release-size/startup benchmark, actual container/image execution, Linux cross-UID/ACL regressions, other architectures/devices, live model protocols, or live certificate compatibility. All-feature Clippy checks compilation/lints for optional paths; it does not establish their runtime behavior.

### Report validation

All 76 local Markdown links resolve to existing files, and all numeric source-line anchors are within their target files. Formatting and all-feature Clippy passed. The final patch was checked for whitespace errors and unrelated modifications. This review adds only this document; no finding is marked fixed by writing it.
