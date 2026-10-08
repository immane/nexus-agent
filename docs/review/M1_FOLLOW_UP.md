# M1 Follow-up Review

**Date:** 2026-10-08
**Status:** M1.3 findings resolved; M1.4 remains incomplete and not verified.

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

**Status: Partially resolved; not verified.** The implementation improves bounded pipe draining, interrupted-read responsiveness, and ownership retention, but the following lifecycle issue remains open.

### M1.4-F1 — Nonblocking setup failure cannot prove descendant termination

**Severity:** P1. **Path:** `crates/nexus-tools/src/exec.rs`, unreadable-pipe cleanup and runtime quarantine.

If setting nonblocking mode fails for either pipe, cleanup signals the original process group, closes both read ends, and waits for the direct child to be reaped. A descendant that escaped the process group may remain alive after the direct child exits; closing the read ends does not establish that this descendant terminated. The tool worker can then return and release runtime quarantine while descendant termination remains unconfirmed.

Read and wait errors on the normal nonblocking cleanup path are no longer treated as EOF or proof of completion. This preserves ownership, but a persistent error or a descendant that holds a pipe open can keep the worker quarantined indefinitely. The behavior is conservative, but no bounded recovery/operator policy is defined.

**Required follow-up:** Preserve ownership when termination cannot be confirmed, including nonblocking-setup failure, without performing a potentially blocking read. Define how permanently unconfirmed workers are surfaced and recovered; do not report cleanup complete based only on direct-child exit. Add injected tests for one-sided nonblocking failure, persistent read failure, and wait failure.

### Additional M1.4 verification gaps

- The cleanup predicate test checks boolean combinations but does not exercise the actual cleanup loop under injected failures.
- The unreadable-pipe test exercises direct-child cleanup, not an escaped descendant retaining a pipe.
- Startup probe reapers are capped at four owners and fail closed at saturation, but remain detached; wait-error saturation is untested.
- Linux sandbox execution and injected wait/read/probe failure paths remain unverified. The descendant-held-pipe integration test is macOS-only.

**Validation recorded:** `cargo nextest run -p nexus-tools --locked --offline` (46 passed), including bounded-drain, unreadable-pipe cleanup, and cleanup-completion tests. This does not close the outstanding lifecycle issue or the platform/failure-injection gaps above.
