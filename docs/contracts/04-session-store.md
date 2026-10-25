# Session Store Contract

Status: Draft contract, revision `draft-1`. Automatic local saves and manual restoration are accepted in [Decision 01](../design/decisions/01-first-release-defaults.md). `nexus-session` has a bounded JSON snapshot, and the runtime's opt-in store mode persists accepted source items and operation records. The interactive TUI now opens the private default local store, lists saved metadata, and exposes explicit restore by session ID. Server and headless application wiring remain ephemeral. Read the [common contract](00-common.md) first.

## Scope and Operations

The store persists accepted conversation and operation records. It does not decide authorization, schedule tools, or treat stored prompts as executable instructions.

`nexus-session` is the session-state and recovery boundary. Its intended scope
includes original history, active-view checkpoints, record validation, and
recovery assembly. It now exposes an explicit, bounded-page recovery helper
that checks the selected checkpoint and contiguous source sequence. Runtime owns
model calls, tool dispatch, approvals, cancellation, and run lifecycle, including
when persistence is required. `nexus-context` retains context-selection and
projection algorithms. Moving session state into this crate must not move
execution control or make restoration execute stored actions.

The crate rename does not change the caller-supplied data directory. The TUI
directory store uses `session-index.json` plus independent session files;
the original `session-store.json` remains readable through explicit import.
Snapshot revision 3 adds an explicit interrupted execution status;
revision-2 snapshots remain readable and are rewritten as revision 3 on their
next successful write. Runtime
commands invoke bounded metadata listing and selected-session recovery only
when a store was explicitly injected.

Semantic operations include bounded/paged session metadata listing, loading a selected session, saving a versioned checkpoint, recording tool intent, and recording tool outcome. Actual method signatures and physical layout remain open.

`nexus-core` defines a `SessionStore` port and bounded checkpoint/operation
types. `nexus-session` provides a file-backed implementation, including
append-only, paged source records for complete original model-context items.
Runtime exposes an opt-in constructor that initializes a checkpoint, persists
accepted user input, complete assistant context items, and tool results, and
writes tool intents before dispatch. Source items are committed before the run
advances to dispatch/another model turn; write failure terminates the run.
TUI storage uses the platform default user data directory outside the project.
Its bounded metadata index is opened at startup; each session's history and
operation journal live in an independent file and are decoded only when that
session is selected or updated. An invalid old session body therefore does not
prevent listing or creating another session. It is not application-encrypted.
Restore only populates the runtime's
bounded history cache; unfinished calls are represented as unknown effects and
are never replayed. A completed run reports saved only after its source and
operation records have passed the per-session file's sync-and-rename protocol. See the
[context-management design](../design/12-context-management.md) for the current
gap and the source-history/active-view boundary.

Prefer a small file-backed implementation first. Versioned snapshots and a compact operation journal are candidates; a database or full event-sourcing system requires a concrete need. Do not persist every token merely because runtime events exist.

The initial `nexus-session` snapshot uses the standard library, existing
`serde_json`, and existing strict JSON validation; it adds no third-party
dependency. It caps the encoded file at 16 MiB and applies existing per-record
and record-count limits. Its current format is revision 3 and rejects duplicate
keys, unknown fields, and unsupported revisions. This format is an initial
implementation detail, not yet the complete session format promised by this
contract.

The TUI directory store caps the metadata index at `MAX_SESSIONS` entries.
Tool intents, outcomes, and source records are bounded per session rather than
globally. No automatic age-based or silent deletion is performed. When the
session limit is reached, the user can remove an explicitly selected saved
session with `:session delete <id>`; sessions currently open in the TUI cannot
be deleted from underneath a live runtime. Deletion is permanent and frees the
index slot and per-session record capacity. Per-session record-limit errors are
explicit; the user can restart in a fresh session, then delete the old saved
session if it is no longer needed.

Import and deletion run in one background maintenance worker per TUI. The UI
keeps handling input and rendering, and reports completion to the originating
conversation even after a switch. While maintenance is outstanding, additional
storage commands are refused and new submissions preserve their drafts rather
than waiting for the same storage lock. Quitting does not cancel or roll back a
blocking filesystem operation; existing bounded process shutdown still applies.
Failures retain safe diagnostic details and advise reopening to reconcile any
partially published work before retrying. A disconnected worker is an uncertain
failure, never a success acknowledgement.

### Legacy import

When `session-store.json` exists, the TUI displays an existence-only notice.
`:session import` explicitly reads and validates that legacy snapshot and copies
its sessions to the indexed layout. It does not restore a conversation or execute
tools. The original file remains an unchanged backup; deleting an imported
session does not delete this backup, so another explicit import can restore it.
Run ownership for tool records must be unambiguous in the legacy source records;
unknown ownership, conflicting existing data, and insufficient capacity reject
import before copying any sessions. Interrupted imports may have copied complete
sessions; retry skips identical sessions and never overwrites conflicting data.

### Indexed publication and recovery

Each changed session is first staged as a complete private snapshot. A synced
`session-pending.json` metadata journal then names the target session and revision
(or explicit deletion). Publication renames the staged body, syncs its directory,
atomically updates the index, and removes and syncs the journal. Deletion records
the same durable intent before removing the index entry and body. Any failure
after durable intent blocks further access until reopening; reopening finishes
the pending operation idempotently without decoding unrelated histories. A
staging failure before durable intent cannot change the published session.
Unpublished staging files are removed and the directory synced on reopening
after pending recovery, so deletion does not leave abandoned staging copies.
Body sync uncertainty also blocks the outer store until reopen.
An interrupted operation may have completed and must not cause tool replay.
Index revisions are updated in memory only after successful index publication.
Session filenames encode ID bytes as lowercase hexadecimal to avoid collisions
between case-distinct IDs on case-insensitive filesystems.

The adapter is Unix-only until a private-directory and file-ACL implementation
is available for other platforms. The caller supplies a
dedicated directory; the adapter creates it with mode `0700` or rejects an
existing directory with group/other permissions. It creates snapshot and
temporary files with mode `0600`, flushes the temporary file, atomically renames
it, and syncs the directory. An OS file lock rejects a second writer for the
same directory and is released when the store handle/process closes. A failure
after rename can mean publication occurred but
durability could not be confirmed; callers must not blindly repeat external
effects to repair such a storage error.
After a post-rename directory-sync failure, the adapter rejects further reads
and writes until reopened, so stale in-memory state cannot overwrite the
published snapshot. Reopening reconciles the visible snapshot, but does not
retroactively confirm durability of the failed write. Re-recording an identical
intent or outcome for the same run/call is idempotent, including at capacity;
conflicting records and inconsistent denied outcomes are rejected before writing.

Before publication, encoded snapshots are checked against the same complete
validation and parser limits used on reopen, including the JSON node budget.
The runtime bounds store-worker waits by live cancellation and the run deadline.
Interrupted workers remain owned until termination; further store-backed runs
are refused until the host reopens and reconciles the store, even if the worker
later succeeds. Input admission reserves the run slot without holding the runtime
state lock during storage I/O. A cancelled input save returns a rejected submit;
it may still have published data and must not be blindly retried.

## Records

| Record | Required information |
| --- | --- |
| Session/checkpoint | Session identity, format revision, logical revision, accepted messages, selected profile metadata |
| Continuation data | Adapter identity/version, compatibility scope, bounded original data |
| Tool intent | Run/call/tool identity, implementation revision, validated arguments or safe references, relevant approved scope |
| Tool outcome | Original execution/effect/evidence state, bounded result or content reference, relationship to the intent |
| Interrupted run | Prior run identity and unresolved operations without fabricated completion |

Keep credentials out of records. Tool arguments, results, and continuation data may still be sensitive; apply retention/redaction policy without making recovery records misleading or unsafe to interpret.

## Accepted Save and Restore Defaults

Automatically save accepted input/turns and relevant operation records in configured local application storage outside the project. Do not place conversation history in the repository, upload it as model context by default, or persist each text delta.

Start a new conversation. Metadata listing and restoration occur only on explicit request and remain bounded; ordinary startup MUST NOT scan/materialize all history or restore the most recent session automatically.

Restoration loads history, not execution. It MUST NOT replay tools, restart interrupted tasks, or restore old per-call grants. It uses the current explicit profile/policy and validates compatibility before continuation. A later run receives a fresh identity.

Host-managed persistence under the selected storage policy does not require a model-tool approval for each checkpoint, and grants no arbitrary filesystem access. The physical data directory and format remain implementation choices.

On 2026-10-25, the project owner confirmed local persistent history for the next
implementation increment, rather than a runtime-only archive. Initial protection
is current-user-only filesystem access, without application-level encryption:
private storage directories and Unix files created with mode `0600`. Equivalent
protection on other platforms must be specified and verified separately. Do not
claim encryption or persist configured credentials. User/tool content may still
contain sensitive data; document that limitation and the retention policy.

Compaction changes the model-facing view, not original retained records. A
storage failure must prevent publishing a reduction that depends on recovering
unarchived sources. Persisting conversation, rebuilding model context, and
reconciling interrupted execution are separate operations; restoring a session
does not imply filesystem rollback or exactly-once execution.

## Modes and Durability

Persistent automatic-saving mode is the accepted default. Ephemeral mode MAY be selected explicitly for use cases or measurements; it must disclose that crash recovery and durable history are unavailable. Storage failure MUST NOT silently switch a normal session into ephemeral mode.

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

Cover automatic saves outside the project, default new-session startup, bounded explicit listing/restoration, no silent ephemeral fallback, interrupted writes, stale revisions, invalid formats, limits, intent durability failure, crash between intent and result, result-write failure after effects, recovery without replay, expired approvals, and incompatible continuation data.

Filesystem durability and recovery behavior require tests on both Linux and macOS. Tests of an in-memory double alone are not evidence of crash-safe file persistence.

## Related Documents

- [Execution design](../design/02-execution.md)
- [Security design](../design/06-security.md)
- [Tool](02-tool.md)
- [Configuration](05-configuration.md)
