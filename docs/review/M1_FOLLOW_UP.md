# M1 Follow-up Review

**Date:** 2026-10-08
**Status as of 2026-11-02:** M1.3 findings resolved. M1.4 is complete within the explicitly documented process-group support boundary; deliberately detached descendants remain a non-goal, not a verified cleanup capability.

This document consolidates the remaining M1 review follow-up from [the architecture review](ARCHITECTURE_REVIEW.md). It records implementation status and validation limits; it is not a security certification.

## M1.3 — Streaming and control pipeline

**Status: Resolved in the current worktree.** Regression coverage is in `crates/nexus-runtime/tests/cov_incremental_stream.rs` and runtime control-outbox tests.

- **Usage replay (P2):** The runtime tracks provisional usage emitted by the incremental sink, validates the prefix in successful authoritative batches, and skips matching replayed usage for both successful and failed batches while preserving authoritative final counters and provider failures. Successful and failed replay paths have deterministic regression tests.
- **Outbox close/finalization race (P2):** On permanent receiver closure, the flusher retires the in-flight and queued undeliverable events, reconciles retained counters, and records that the terminal was not delivered and the run was truncated. A deterministic close-during-finalization test covers the race.
- **Remaining coverage improvement:** Add direct deterministic coverage for the sink publishing final usage before batch ingestion. The current replay counter is intended to protect this ordering, but this exact interleaving is not independently exercised.

## M1.5 — TUI run-bound provider selection

**Status: Fix present; lifecycle coverage remains indirect.** Selection snapshots are bound to a run, stale snapshots are rejected by the provider, and terminal handling clears an unconsumed snapshot. Before submitting again, the TUI also checks whether the snapshot's owning run has already finalized.

- The terminal-before-first-provider-call test uses a synthetic accepted response and terminal event; it does not drive cancellation through the runtime before provider invocation.
- The stale-run test exercises the selection helper directly rather than accepting two sequential runs through the runtime/provider boundary.
- `clear_finalized_pending_selection()` has no direct test for the terminal-not-yet-consumed submission race.

These are validation gaps, not evidence of a remaining behavior defect. Add an integration-style regression if this path is changed further or before treating the race as fully covered.

## M1.4 — Exec cleanup and worker ownership

**Status: Verified on macOS and the recorded Linux reference VM within the documented support boundary.** The implementation uses bounded nonblocking pipe draining and retains ownership when read/wait completion is uncertain. Deliberately detached descendants and sandbox escapes are explicitly not covered; effects remain uncertain and the implementation does not claim their termination.

### M1.4-F1 — Nonblocking setup failure cannot prove descendant termination

**Severity:** P1. **Path:** `crates/nexus-tools/src/exec.rs`, unreadable-pipe cleanup and runtime quarantine.

If setting nonblocking mode fails for either pipe, cleanup signals the original process group, closes both read ends without blocking, and waits for the direct child to be reaped. This is complete only for the supported direct-child/original-process-group contract; detached descendants are outside the contract and are never represented as confirmed terminated. The tool outcome remains `Unknown/Uncertain` where effects may have occurred.

Read and wait errors on the normal nonblocking cleanup path are not treated as EOF or proof of completion. A persistent error or a descendant that holds a pipe open can keep the worker quarantined indefinitely. This is the fail-closed recovery policy: no automatic retry/release occurs while completion is unconfirmed; the runtime exposes an uncertain terminal outcome. There is no in-process force-release because that would discard ownership evidence.

**Verification:** Added injected tests for one-sided nonblocking setup failure, persistent read failure, and wait failure. Each remains non-completion rather than EOF/reaping. The actual cleanup retains direct-child ownership until reaped; no potentially blocking read is performed on the setup-failure path.

### Additional M1.4 verification gaps

- The cleanup predicate test checks boolean combinations but does not exercise the actual cleanup loop under injected failures.
- The unreadable-pipe test exercises direct-child cleanup. Detached descendants retaining a pipe are outside the supported contract and are not claimed terminated.
- Startup probe reapers are capped at four owners and fail closed at saturation, but remain detached; wait-error saturation is untested.
- Linux sandbox execution, exec failure-injection tests, and a descendant-held-pipe integration test now pass on the recorded Ubuntu reference VM; the equivalent supported-path integration test passes on macOS. Results do not generalize to every Linux distribution/filesystem.

**Current validation:** macOS `cargo test -p nexus-tools --locked --offline -- --test-threads=1` (74 passed, 1 ignored across unit/behavior/doc tests), `cargo clippy -p nexus-tools --all-targets --locked --offline -- -D warnings`, and the descendant-held-pipe test passed. On Ubuntu 26.04.1/ext4, 11 exec unit tests and the explicitly enabled descendant-held-pipe test passed. Linux validation used the isolated reference-VM checkout; no live provider was used.
