//! Coverage hardening for the [`SessionStore`] revision-conflict contract.
//!
//! nexus-core owns the port, not a concrete store, and this crate must not
//! depend on an adapter, so these tests pin the documented conflict policy
//! through a deterministic in-memory reference double assembled from public
//! API only:
//!
//! - a save below the stored logical revision is rejected explicitly and the
//!   newer checkpoint wins;
//! - an equal-revision save with identical content is an idempotent retry;
//! - an equal-revision save with different content is rejected and preserves
//!   the stored checkpoint exactly;
//! - an interrupted write fails before any mutation, so it never replaces a
//!   prior observation or fabricates a partial record.
//!
//! The double is explicitly non-durable: passing these tests is not evidence
//! of crash-safe file persistence. No clock, randomness, or external I/O is
//! involved.

#![forbid(unsafe_code)]

use std::cmp::Ordering;
use std::collections::BTreeMap;

use nexus_core::store::{StoredMessage, check_checkpoint_bounds, check_format_revision};
use nexus_core::{
    AgentError, ApprovedScope, CallId, EffectState, ErrorCategory, Evidence, ExecutionStatus,
    M0_REVISION, NormalizedArgs, RetryGuidance, RunId, STORE_FORMAT_REVISION, SessionCheckpoint,
    SessionId, SessionMetadata, SessionStore, ToolId, ToolIntentRecord, ToolOutcomeRecord,
};

/// Minimal deterministic reference implementation of the public port.
///
/// Contract fixture, not a product adapter: it encodes the documented
/// revision-conflict policy so the boundary can be exercised from an external
/// crate with public API only. `BTreeMap` gives stable identity order, and the
/// single-shot interruption flag models a write that fails before any
/// mutation. It is explicitly non-durable and promises no crash recovery.
struct ConflictStore {
    checkpoints: BTreeMap<String, SessionCheckpoint>,
    intents: Vec<ToolIntentRecord>,
    outcomes: Vec<ToolOutcomeRecord>,
    fail_next_write: bool,
}

impl ConflictStore {
    fn new() -> Self {
        Self {
            checkpoints: BTreeMap::new(),
            intents: Vec::new(),
            outcomes: Vec::new(),
            fail_next_write: false,
        }
    }

    /// Arms the single-shot interrupted-write simulation.
    fn interrupt_next_write(&mut self) {
        self.fail_next_write = true;
    }

    fn take_interruption(&mut self) -> Result<(), AgentError> {
        if self.fail_next_write {
            self.fail_next_write = false;
            return Err(store_error(
                ErrorCategory::StorageFailure,
                "interrupted write",
            ));
        }
        Ok(())
    }
}

impl SessionStore for ConflictStore {
    fn list_sessions(&self, limit: usize) -> Result<Vec<SessionMetadata>, AgentError> {
        Ok(self
            .checkpoints
            .values()
            .take(limit)
            .map(|checkpoint| SessionMetadata {
                session: checkpoint.session().clone(),
                logical_revision: checkpoint.logical_revision(),
            })
            .collect())
    }

    fn load_session(&self, id: &SessionId) -> Result<SessionCheckpoint, AgentError> {
        self.checkpoints
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| store_error(ErrorCategory::StorageFailure, "session not found"))
    }

    fn save_checkpoint(&mut self, checkpoint: &SessionCheckpoint) -> Result<(), AgentError> {
        // The interruption check precedes every mutation: a failed write
        // leaves the previous observation untouched and creates nothing.
        self.take_interruption()?;
        check_format_revision(checkpoint.format_revision())?;
        check_checkpoint_bounds(checkpoint)?;
        if let Some(stored) = self.checkpoints.get(checkpoint.session().as_str()) {
            match checkpoint
                .logical_revision()
                .cmp(&stored.logical_revision())
            {
                Ordering::Less => {
                    return Err(store_error(
                        ErrorCategory::InvalidInput,
                        "stale checkpoint revision",
                    ));
                }
                // Equal revisions are an idempotent retry only when the whole
                // record is identical; a conflicting write preserves the
                // stored checkpoint and fails explicitly.
                Ordering::Equal if checkpoint != stored => {
                    return Err(store_error(
                        ErrorCategory::InvalidInput,
                        "conflicting checkpoint at equal revision",
                    ));
                }
                Ordering::Equal | Ordering::Greater => {}
            }
        }
        self.checkpoints
            .insert(checkpoint.session().as_str().to_owned(), checkpoint.clone());
        Ok(())
    }

    fn record_intent(&mut self, intent: &ToolIntentRecord) -> Result<(), AgentError> {
        self.take_interruption()?;
        self.intents.push(intent.clone());
        Ok(())
    }

    fn record_outcome(&mut self, outcome: &ToolOutcomeRecord) -> Result<(), AgentError> {
        self.take_interruption()?;
        self.outcomes.push(outcome.clone());
        Ok(())
    }
}

fn store_error(category: ErrorCategory, message: &'static str) -> AgentError {
    AgentError::new(category, message, RetryGuidance::DoNotRetry)
        .expect("static safe store-double message builds")
}

fn session(id: &str) -> SessionId {
    SessionId::new(id).expect("valid session id")
}

fn message(source: &str, text: &str, complete: bool) -> StoredMessage {
    StoredMessage {
        source: source.to_owned(),
        text: text.to_owned(),
        complete,
    }
}

fn checkpoint_with_message(
    id: &str,
    logical_revision: u64,
    message: StoredMessage,
    profile: &str,
) -> SessionCheckpoint {
    SessionCheckpoint::new(
        session(id),
        STORE_FORMAT_REVISION,
        logical_revision,
        vec![message],
        profile,
    )
    .expect("valid checkpoint builds")
}

fn checkpoint(id: &str, logical_revision: u64, text: &str, profile: &str) -> SessionCheckpoint {
    checkpoint_with_message(id, logical_revision, message("user", text, true), profile)
}

fn intent() -> ToolIntentRecord {
    ToolIntentRecord {
        run: RunId::new("run-1").expect("valid"),
        call: CallId::new("call-1").expect("valid"),
        tool: ToolId::new("host_write", M0_REVISION).expect("valid"),
        args: NormalizedArgs::new(r#"{"path":"dst"}"#).expect("valid"),
        scope: ApprovedScope::new("project-write").expect("valid"),
    }
}

fn outcome() -> ToolOutcomeRecord {
    ToolOutcomeRecord {
        run: RunId::new("run-1").expect("valid"),
        call: CallId::new("call-1").expect("valid"),
        status: ExecutionStatus::Succeeded,
        effect: EffectState::KnownApplied,
        evidence: Evidence::HostObserved,
        truncated: false,
    }
}

/// A save below the stored logical revision is rejected explicitly and the
/// newer stored checkpoint stays intact.
#[test]
fn stale_save_is_rejected_and_newer_revision_wins() {
    let mut store = ConflictStore::new();
    let newer = checkpoint("sess-stale", 2, "newer", "profile-newer");
    store
        .save_checkpoint(&newer)
        .expect("initial save accepted");

    let stale = checkpoint("sess-stale", 1, "older", "profile-older");
    let error = store
        .save_checkpoint(&stale)
        .expect_err("stale save must be rejected");
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);

    let loaded = store
        .load_session(&session("sess-stale"))
        .expect("stored checkpoint loads");
    assert_eq!(loaded, newer, "the newer checkpoint is preserved exactly");
    assert_eq!(loaded.logical_revision(), 2);

    let newest = checkpoint("sess-stale", 3, "newest", "profile-newest");
    store
        .save_checkpoint(&newest)
        .expect("strictly newer save is accepted");
    assert_eq!(
        store.load_session(&session("sess-stale")).expect("loads"),
        newest
    );
}

/// Re-saving the same revision with identical content is an idempotent retry,
/// not a conflict, and never duplicates the session.
#[test]
fn identical_equal_revision_save_is_an_idempotent_retry() {
    let mut store = ConflictStore::new();
    let original = checkpoint("sess-retry", 4, "same", "profile-same");
    store
        .save_checkpoint(&original)
        .expect("first save accepted");

    let retry = original.clone();
    store
        .save_checkpoint(&retry)
        .expect("identical equal-revision retry is accepted");

    assert_eq!(
        store.load_session(&session("sess-retry")).expect("loads"),
        original
    );
    assert_eq!(
        store.checkpoints.len(),
        1,
        "retry never duplicates a session"
    );
}

/// An equal-revision save with different content is rejected and the stored
/// checkpoint is preserved exactly; only a strictly newer revision replaces
/// it.
#[test]
fn conflicting_equal_revision_save_is_rejected_and_preserves_stored() {
    let mut store = ConflictStore::new();
    let original = checkpoint("sess-conflict", 5, "original", "profile-original");
    store
        .save_checkpoint(&original)
        .expect("initial save accepted");

    let conflicting = checkpoint("sess-conflict", 5, "changed", "profile-changed");
    let error = store
        .save_checkpoint(&conflicting)
        .expect_err("conflicting equal-revision save must be rejected");
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(
        store
            .load_session(&session("sess-conflict"))
            .expect("stored checkpoint is preserved"),
        original
    );

    // Equality covers the whole record: a difference in a single carried
    // message field at the same revision is still a conflict.
    let same_revision_incomplete = checkpoint_with_message(
        "sess-conflict",
        5,
        message("user", "original", false),
        "profile-original",
    );
    let error = store
        .save_checkpoint(&same_revision_incomplete)
        .expect_err("whole-record conflict must be rejected");
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert_eq!(
        store
            .load_session(&session("sess-conflict"))
            .expect("stored checkpoint is still preserved"),
        original
    );

    let replacement = checkpoint("sess-conflict", 6, "changed", "profile-changed");
    store
        .save_checkpoint(&replacement)
        .expect("strictly newer save replaces");
    let loaded = store
        .load_session(&session("sess-conflict"))
        .expect("newer checkpoint loads");
    assert_eq!(loaded, replacement);
    assert_eq!(loaded.messages()[0].text, "changed");
    assert_eq!(loaded.profile(), "profile-changed");
}

/// An interrupted write fails before any mutation: the prior checkpoint and
/// the prior record sets stay exactly as observed, no session is fabricated,
/// and the single-shot flag disarms.
#[test]
fn interrupted_write_never_replaces_or_fabricates_state() {
    let mut store = ConflictStore::new();
    let original = checkpoint("sess-interrupted", 1, "original", "profile-original");
    store
        .save_checkpoint(&original)
        .expect("initial save accepted");

    store.interrupt_next_write();
    let replacement = checkpoint("sess-interrupted", 2, "replacement", "profile-replacement");
    let error = store
        .save_checkpoint(&replacement)
        .expect_err("interrupted save must fail");
    assert_eq!(error.category(), ErrorCategory::StorageFailure);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(
        store
            .load_session(&session("sess-interrupted"))
            .expect("prior checkpoint survives"),
        original
    );

    // An interrupted first save creates nothing.
    store.interrupt_next_write();
    assert!(
        store
            .save_checkpoint(&checkpoint("sess-new", 1, "first", "profile-first"))
            .is_err()
    );
    assert!(
        store.load_session(&session("sess-new")).is_err(),
        "a failed first save must not fabricate a session"
    );

    // Interrupted intent and outcome writes never fabricate partial records.
    store.interrupt_next_write();
    assert!(store.record_intent(&intent()).is_err());
    store.interrupt_next_write();
    assert!(store.record_outcome(&outcome()).is_err());
    assert!(store.intents.is_empty());
    assert!(store.outcomes.is_empty());

    // The interruption flag is single-shot: the same save succeeds on retry.
    store
        .save_checkpoint(&replacement)
        .expect("retry after interruption succeeds");
    assert_eq!(
        store
            .load_session(&session("sess-interrupted"))
            .expect("loads"),
        replacement
    );
}

/// The conflict policy is per session: equal revisions in distinct sessions
/// are independent, and listing reports each session's newest metadata.
#[test]
fn equal_revisions_in_distinct_sessions_are_independent() {
    let mut store = ConflictStore::new();
    let first = checkpoint("sess-a", 7, "first", "profile-a");
    let second = checkpoint("sess-b", 7, "second", "profile-b");
    store.save_checkpoint(&first).expect("session a accepted");
    store
        .save_checkpoint(&second)
        .expect("session b accepted at the same revision");

    assert_eq!(
        store.load_session(&session("sess-a")).expect("a loads"),
        first
    );
    assert_eq!(
        store.load_session(&session("sess-b")).expect("b loads"),
        second
    );

    let listed = store.list_sessions(usize::MAX).expect("list works");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].session, session("sess-a"));
    assert_eq!(listed[0].logical_revision, 7);
    assert_eq!(listed[1].session, session("sess-b"));
    assert_eq!(listed[1].logical_revision, 7);
}
