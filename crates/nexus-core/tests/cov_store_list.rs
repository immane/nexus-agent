#![forbid(unsafe_code)]

//! Public-boundary coverage for the session-store listing and load contract.
//!
//! nexus-core owns the [`SessionStore`] port, the versioned checkpoint record,
//! and the finite caps; concrete stores live in adapters. These tests pin the
//! documented listing/load semantics from outside the crate using a minimal
//! deterministic reference store assembled only from public API:
//!
//! - a missing load fails explicitly with a bounded diagnostic and leaves the
//!   store untouched;
//! - listing is finite, stably ordered by session identity, prefix-consistent
//!   across limits, and reports newest metadata without loading history;
//! - format revisions use exact equality: only [`STORE_FORMAT_REVISION`] is
//!   accepted, and mismatches fail before any checkpoint can exist.

use std::collections::BTreeMap;

use nexus_core::error::MAX_MESSAGE_LEN;
use nexus_core::store::{
    MAX_INTENT_RECORDS, MAX_OUTCOME_RECORDS, StoredMessage, check_checkpoint_bounds,
    check_format_revision,
};
use nexus_core::{
    AgentError, ErrorCategory, M0_REVISION, RetryGuidance, STORE_FORMAT_REVISION,
    SessionCheckpoint, SessionId, SessionMetadata, SessionStore, ToolIntentRecord,
    ToolOutcomeRecord,
};

/// Minimal deterministic reference implementation of the public port.
///
/// This is a contract fixture, not a product adapter: it exists so the
/// listing/load boundary can be exercised from an external crate with public
/// API only. `BTreeMap` iteration gives identity order, and `take(limit)`
/// bounds the materialized listing.
struct ReferenceStore {
    checkpoints: BTreeMap<String, SessionCheckpoint>,
    intents: Vec<ToolIntentRecord>,
    outcomes: Vec<ToolOutcomeRecord>,
}

impl ReferenceStore {
    fn new() -> Self {
        Self {
            checkpoints: BTreeMap::new(),
            intents: Vec::new(),
            outcomes: Vec::new(),
        }
    }
}

impl SessionStore for ReferenceStore {
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
        self.checkpoints.get(id.as_str()).cloned().ok_or_else(|| {
            AgentError::new(
                ErrorCategory::StorageFailure,
                "session not found",
                RetryGuidance::DoNotRetry,
            )
            .expect("static safe message builds")
        })
    }

    fn save_checkpoint(&mut self, checkpoint: &SessionCheckpoint) -> Result<(), AgentError> {
        check_format_revision(checkpoint.format_revision())?;
        check_checkpoint_bounds(checkpoint)?;
        self.checkpoints
            .insert(checkpoint.session().as_str().to_owned(), checkpoint.clone());
        Ok(())
    }

    fn record_intent(&mut self, intent: &ToolIntentRecord) -> Result<(), AgentError> {
        if self.intents.len() >= MAX_INTENT_RECORDS {
            return Err(capacity_error("intent capacity exhausted"));
        }
        self.intents.push(intent.clone());
        Ok(())
    }

    fn record_outcome(&mut self, outcome: &ToolOutcomeRecord) -> Result<(), AgentError> {
        if self.outcomes.len() >= MAX_OUTCOME_RECORDS {
            return Err(capacity_error("outcome capacity exhausted"));
        }
        self.outcomes.push(outcome.clone());
        Ok(())
    }
}

fn capacity_error(message: &'static str) -> AgentError {
    AgentError::new(
        ErrorCategory::ResourceLimit,
        message,
        RetryGuidance::DoNotRetry,
    )
    .expect("static safe message builds")
}

fn session(id: &str) -> SessionId {
    SessionId::new(id).expect("valid session id")
}

fn checkpoint(id: &str, logical_revision: u64) -> SessionCheckpoint {
    SessionCheckpoint::new(
        session(id),
        STORE_FORMAT_REVISION,
        logical_revision,
        vec![StoredMessage {
            source: "user".to_owned(),
            text: format!("message-{id}-{logical_revision}"),
            complete: true,
        }],
        "test-profile",
    )
    .expect("valid checkpoint builds")
}

#[test]
fn missing_load_fails_explicitly_and_leaves_store_intact() {
    let mut store = ReferenceStore::new();
    store
        .save_checkpoint(&checkpoint("sess-present", 1))
        .expect("save works");

    for missing in [
        "sess-missing",
        "sess-presen",
        "sess-present-extra",
        "sess-0",
    ] {
        let error = store
            .load_session(&session(missing))
            .expect_err("missing session load fails");
        assert_eq!(error.category(), ErrorCategory::StorageFailure);
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        assert!(!error.message().is_empty());
        assert!(error.message().len() <= MAX_MESSAGE_LEN);
        assert!(error.correlation().is_empty());
    }

    // Repeated failure is deterministic, and failed loads never fabricate or
    // drop stored state.
    assert!(store.load_session(&session("sess-missing")).is_err());
    let listed = store.list_sessions(usize::MAX).expect("list works");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].session.as_str(), "sess-present");
    let loaded = store
        .load_session(&session("sess-present"))
        .expect("present session still loads");
    assert_eq!(loaded.logical_revision(), 1);
    assert_eq!(loaded.messages().len(), 1);
    assert_eq!(loaded.profile(), "test-profile");
}

#[test]
fn listing_is_finite_ordered_and_prefix_consistent() {
    let mut store = ReferenceStore::new();
    // Scrambled insertion order: listing order follows session identity, not
    // write order.
    for id in ["sess-c", "sess-a", "sess-e", "sess-b", "sess-d"] {
        store
            .save_checkpoint(&checkpoint(id, 1))
            .expect("save works");
    }

    assert!(
        store
            .list_sessions(0)
            .expect("zero limit lists nothing")
            .is_empty(),
        "limit zero never fabricates entries"
    );

    let full = store
        .list_sessions(usize::MAX)
        .expect("an over-large limit is clamped by the stored count");
    let ids: Vec<&str> = full.iter().map(|meta| meta.session.as_str()).collect();
    assert_eq!(ids, ["sess-a", "sess-b", "sess-c", "sess-d", "sess-e"]);

    for limit in 0..=full.len() {
        let listed = store.list_sessions(limit).expect("bounded list works");
        assert_eq!(listed.len(), limit, "limit {limit} bounds the result");
        assert_eq!(
            listed,
            full[..limit],
            "limit {limit} returns a stable prefix of the full order"
        );
    }

    let repeated = store.list_sessions(3).expect("list works");
    assert_eq!(repeated, store.list_sessions(3).expect("list works"));
    assert_eq!(repeated.len(), 3);
}

#[test]
fn listing_reports_newest_logical_revision_as_metadata_only() {
    let mut store = ReferenceStore::new();
    store
        .save_checkpoint(&checkpoint("sess-a", 1))
        .expect("save works");
    store
        .save_checkpoint(&checkpoint("sess-b", 7))
        .expect("save works");
    store
        .save_checkpoint(&checkpoint("sess-a", 4))
        .expect("newer save works");

    let listed = store.list_sessions(usize::MAX).expect("list works");
    assert_eq!(
        listed,
        vec![
            SessionMetadata {
                session: session("sess-a"),
                logical_revision: 4,
            },
            SessionMetadata {
                session: session("sess-b"),
                logical_revision: 7,
            },
        ],
        "listing carries newest identity metadata only"
    );
}

#[test]
fn incompatible_format_revision_fails_exact_equality() {
    assert_eq!(
        STORE_FORMAT_REVISION, M0_REVISION,
        "store format tracks the M0 lock revision"
    );
    assert!(check_format_revision(STORE_FORMAT_REVISION).is_ok());

    let mut mismatches = vec![
        0,
        1,
        u32::MAX,
        STORE_FORMAT_REVISION.wrapping_add(1),
        STORE_FORMAT_REVISION.wrapping_sub(1),
        STORE_FORMAT_REVISION ^ 1,
    ];
    mismatches.sort_unstable();
    mismatches.dedup();
    mismatches.retain(|revision| *revision != STORE_FORMAT_REVISION);
    assert!(
        !mismatches.is_empty(),
        "at least one revision always differs from the accepted one"
    );

    for revision in mismatches {
        let error = check_format_revision(revision).expect_err("mismatch is rejected");
        assert_eq!(error.category(), ErrorCategory::InvalidInput);
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        assert!(!error.message().is_empty());
        assert!(error.message().len() <= MAX_MESSAGE_LEN);

        // No migration and no guessing: the same mismatch fails before a
        // checkpoint can exist.
        let build_error = SessionCheckpoint::new(session("sess-1"), revision, 1, vec![], "profile")
            .expect_err("revision mismatch must not construct a checkpoint");
        assert_eq!(build_error.category(), ErrorCategory::InvalidInput);
        assert_eq!(build_error.retry(), RetryGuidance::DoNotRetry);
    }

    let accepted = SessionCheckpoint::new(
        session("sess-1"),
        STORE_FORMAT_REVISION,
        1,
        vec![],
        "profile",
    )
    .expect("accepted revision builds");
    assert_eq!(accepted.format_revision(), STORE_FORMAT_REVISION);
    check_checkpoint_bounds(&accepted).expect("constructed checkpoint passes the boundary recheck");
}

#[test]
fn session_store_port_is_object_safe_for_listing_and_loading() {
    let mut store = ReferenceStore::new();
    store
        .save_checkpoint(&checkpoint("sess-b", 2))
        .expect("save works");
    store
        .save_checkpoint(&checkpoint("sess-a", 1))
        .expect("save works");

    let port: &dyn SessionStore = &store;
    let listed = port.list_sessions(1).expect("trait-object listing works");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].session.as_str(), "sess-a");
    assert!(port.load_session(&session("sess-a")).is_ok());
    assert!(port.load_session(&session("sess-missing")).is_err());
}
