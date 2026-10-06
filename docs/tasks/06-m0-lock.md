# M0 Subset Lock (P0 exit)

Status: Accepted M0 semantics for P2-P5. Test-only; no stability claims and no product defaults. This lock describes required M0 semantics only; it makes no claim about workspace contents or about M0 implementation or acceptance status.

Source: P0 consistency review of all draft contracts, designs, decisions, and task docs. Items the drafts mark pending, proposed, or to-be-selected are resolved below with explicit M0-test stand-ins. Anything not listed here stays deferred and must not be silently invented by P2-P5.

## Locked resolutions

1. **Finite M0-test constants** (not product defaults; exhaustion always yields an explicit outcome, never silent fallback):
   | Budget | M0-test value |
   | --- | --- |
   | Model turns per run | 8 |
   | Tool calls per run | 64 |
   | Tool calls per turn | 16, declared order, sequential |
   | Run duration | 300 s (monotonic clock) |
   | Per-tool timeout | 60 s |
   | Retained context items | 256 |
   | Streamed tool-argument assembly | 65,536 bytes |
   | Tool output (progress + final share one budget) | 262,144 bytes |
   | Event data channel | 1,024 events |
   | Event control channel | 128 events |
   | Concurrent active operations | 8 |
   | Default approval expiry | 120 s |
2. **Configuration:** code-constructed profiles only. No file parsing, no environment overrides, no CLI flags in M0.
3. **Headless output:** unstable test-only revision. Data/events go to stdout, diagnostics to stderr, never mixed; no escape codes in data.
4. **Tool validation subset:** object-root JSON with a closed minimal validator (objects with bounded properties, strings/numbers/booleans/null/arrays with size bounds, `required`; no `$ref`, no remote fetch, no other keywords). Anything outside the subset fails registration explicitly.
5. **Session store:** in-memory ephemeral only, self-identifying as non-durable. The persistent intent-durability-before-dispatch gate is disabled for M0 and returns in the post-M0 gate with file-durability tests.
6. **Revisions:** exact equality, M0 revision `0`. Any mismatch is an explicit failure; no migration, no guessing.
7. **Text batching:** adapters may batch wire fragments; the runtime assigns contiguous sequence numbers after any runtime batching. Batching never crosses turn, item, or lifecycle boundaries.
8. **Stale rejection on both sides:** the runtime never applies stale events/grants/approvals to a newer run, and frontends reject stale-run updates. P6 tests both directions.
9. **Event transport:** two bounded channels. Data channel (text deltas, previews, progress; adjacent text coalescible) and control channel (approvals, outcomes, terminal events; never coalesced, never silently dropped).
10. **Approval binding:** exact tuple of run, call, tool identity plus revision, normalized immutable arguments, approved scope, expiry, and policy revision, checked immediately before dispatch. Only host-authorized scoped reads/searches are automatic; every model-directed mutation and every command execution requires confirmation. Stream fragments and previews never authorize. Mutable authorization, cancellation, and deadlines are rechecked at dispatch.
11. **Identifiers:** opaque string newtypes, at most 64 chars of `[A-Za-z0-9_-]`. Never used as filesystem paths, never infer permissions. Every event and grant carries its owning run; provider item keys and call references stay separate and preserved for round-trips.
12. **Terminal-event axioms:** one `RunStarted` plus one `RunFinished` per run; exactly one `TurnFinished` or `Failed` per fully consumed invocation; every `ToolStarted` gets an outcome while its owner lives; nothing is fabricated after process death. Truncated, refused, or incomplete output is never labeled complete success; missing usage stays unknown, never zero.
13. **Deferred stays deferred:** browser login, relay execution, external plugin processes, MCP versions, sandboxing, Windows, indexing, multi-agent scheduling, arbitrary lifecycle hooks, and all product defaults listed as open in the foundation decision.

## Open to the implementer (reviewed at the stage gate)

Representation details inside the locked shapes: message/content variants, `AgentError` fields, approval and snapshot structs, capability encoding, usage counters, deadline types. Choose the simplest form that satisfies the contracts, document it, and expect review. Inventing config sources, wire formats, or schema dialects beyond this note is a gate failure.

## Related Documents

- [Common contract](../contracts/00-common.md)
- [Implementation readiness](../design/09-implementation-readiness.md)
- [M0 gates](04-m0-gates.md)
