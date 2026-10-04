# Session Store Contract

Status: Draft contract, revision `draft-0`. No storage implementation or on-disk format exists. Read the [common contract](00-common.md) first.

## Scope and Operations

The store persists accepted conversation and operation records. It does not decide authorization, schedule tools, or treat stored prompts as executable instructions.

Semantic operations include loading a selected session, saving a versioned checkpoint, recording tool intent, and recording tool outcome. Actual method signatures and physical layout remain open.

Prefer a small file-backed implementation first. Versioned snapshots and a compact operation journal are candidates; a database or full event-sourcing system requires a concrete need. Do not persist every token merely because runtime events exist.

## Records

| Record | Required information |
| --- | --- |
| Session/checkpoint | Session identity, format revision, logical revision, accepted messages, selected profile metadata |
| Continuation data | Adapter identity/version, compatibility scope, bounded original data |
| Tool intent | Run/call/tool identity, implementation revision, validated arguments or safe references, relevant approved scope |
| Tool outcome | Original execution/effect/evidence state, bounded result or content reference, relationship to the intent |
| Interrupted run | Prior run identity and unresolved operations without fabricated completion |

Keep credentials out of records. Tool arguments, results, and continuation data may still be sensitive; apply retention/redaction policy without making recovery records misleading or unsafe to interpret.

## Modes and Durability

Persistent mode is the proposed normal coding-session mode. Ephemeral mode MAY be selected explicitly for use cases or measurements; it must disclose that crash recovery and durable history are unavailable.

Every write acknowledgement MUST identify the promised durability level. Buffered acceptance is not crash-recoverable acknowledgement. A claim of crash recoverability requires an implemented and tested filesystem/database protocol, including relevant flush and atomicity behavior.

In persistent mode, side-effecting tool intent MUST reach the required durability level before dispatch. If that cannot be established, do not execute the operation. Side-effecting or uncertain external calls must be classified conservatively by host policy.

After execution, store the actual outcome without rewriting the original intent or erasing uncertainty. If outcome recording fails after effects occurred, surface storage failure and preserve the in-memory observation; do not retry the tool to repair a write.

## Crash Recovery

Recover a valid checkpoint and valid operation records without guessing away corrupted required data. A recoverable incomplete trailing record may be excluded only under the documented storage format, not arbitrary string repair.

Unmatched intents are uncertain after a crash, even if execution might never have started. Completed outcomes must not be replayed. Loaded unfinished runs are interrupted, not live runs, and require explicit reconciliation before any operation is repeated.

History loading MUST NOT execute tools, reconnect plugins, or reinstate expired approvals. Stored authorization history is evidence of an earlier decision, not a new grant.

## Consistency and Limits

Use an explicit single-writer or revision-conflict policy. A stale save MUST NOT overwrite a newer session silently. File identifiers and references must be validated through the storage/platform boundary rather than treated as trusted paths.

Load only the selected session on demand. Bound records, materialized history, and referenced output. Retention or compaction must preserve required call/result associations and provider continuation semantics; unsupported compaction fails explicitly.

Changing provider or adapter revision requires compatibility validation. Invalid or incompatible opaque state cannot be silently dropped while claiming lossless continuation.

## Required Tests

Cover interrupted writes, stale revisions, invalid formats, limits, intent durability failure, crash between intent and result, result-write failure after effects, recovery without replay, expired approvals, and incompatible continuation data.

Filesystem durability and recovery behavior require tests on both Linux and macOS. Tests of an in-memory double alone are not evidence of crash-safe file persistence.

## Related Documents

- [Execution design](../design/02-execution.md)
- [Security design](../design/06-security.md)
- [Tool](02-tool.md)
- [Configuration](05-configuration.md)
