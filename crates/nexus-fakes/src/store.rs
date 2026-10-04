//! Test-only ephemeral session store.
//!
//! [`EphemeralStore`] is an in-memory [`SessionStore`](nexus_core::SessionStore)
//! that is explicitly **non-durable**: [`EphemeralStore::is_durable`] always
//! returns `false` and [`EphemeralStore::durability`] reports
//! [`PersistenceState::Ephemeral`](nexus_core::PersistenceState), so crash
//! recovery and durable history are unavailable. It must never be mistaken
//! for evidence of crash-safe file persistence. Format revisions use exact
//! equality against [`STORE_FORMAT_REVISION`](nexus_core::STORE_FORMAT_REVISION)
//! (M0 revision `0`); a stale checkpoint (older logical revision than the
//! stored one) is rejected explicitly. The plain-bool `fail_next_write` flag
//! simulates an interrupted write for P6 without any framework.

use std::collections::HashMap;

use nexus_core::store::check_format_revision;
use nexus_core::{
    AgentError, ErrorCategory, PersistenceState, RetryGuidance, STORE_FORMAT_REVISION,
    SessionCheckpoint, SessionId, SessionMetadata, SessionStore, ToolIntentRecord,
    ToolOutcomeRecord,
};

/// Non-durable test-only session store. See the module docs.
pub struct EphemeralStore {
    checkpoints: HashMap<String, SessionCheckpoint>,
    intents: Vec<ToolIntentRecord>,
    outcomes: Vec<ToolOutcomeRecord>,
    fail_next_write: bool,
}

impl EphemeralStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self {
            checkpoints: HashMap::new(),
            intents: Vec::new(),
            outcomes: Vec::new(),
            fail_next_write: false,
        }
    }

    /// Always `false`: this store promises no crash recoverability.
    pub fn is_durable(&self) -> bool {
        false
    }

    /// Always [`PersistenceState::Ephemeral`](nexus_core::PersistenceState).
    pub fn durability(&self) -> PersistenceState {
        PersistenceState::Ephemeral
    }

    /// The exact stored-format revision this store accepts (M0 revision `0`).
    pub fn format_revision(&self) -> u32 {
        STORE_FORMAT_REVISION
    }

    /// Arms (or clears) the single-shot interrupted-write simulation: the
    /// next checkpoint save or intent/outcome record fails with a storage
    /// error and disarms the flag.
    pub fn set_fail_next_write(&mut self, fail: bool) {
        self.fail_next_write = fail;
    }

    /// Returns whether the interrupted-write flag is currently armed.
    pub fn fail_next_write(&self) -> bool {
        self.fail_next_write
    }

    /// Returns recorded intents in insertion order.
    pub fn intents(&self) -> &[ToolIntentRecord] {
        &self.intents
    }

    /// Returns recorded outcomes in insertion order.
    pub fn outcomes(&self) -> &[ToolOutcomeRecord] {
        &self.outcomes
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

impl Default for EphemeralStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore for EphemeralStore {
    fn list_sessions(&self, limit: usize) -> Result<Vec<SessionMetadata>, AgentError> {
        let mut entries: Vec<SessionMetadata> = self
            .checkpoints
            .values()
            .map(|checkpoint| SessionMetadata {
                session: checkpoint.session().clone(),
                logical_revision: checkpoint.logical_revision(),
            })
            .collect();
        entries.sort_by(|left, right| left.session.as_str().cmp(right.session.as_str()));
        entries.truncate(limit);
        Ok(entries)
    }

    fn load_session(&self, id: &SessionId) -> Result<SessionCheckpoint, AgentError> {
        self.checkpoints
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| store_error(ErrorCategory::StorageFailure, "session not found"))
    }

    fn save_checkpoint(&mut self, checkpoint: &SessionCheckpoint) -> Result<(), AgentError> {
        self.take_interruption()?;
        check_format_revision(checkpoint.format_revision())?;
        if let Some(stored) = self.checkpoints.get(checkpoint.session().as_str())
            && checkpoint.logical_revision() < stored.logical_revision()
        {
            return Err(store_error(
                ErrorCategory::InvalidInput,
                "stale checkpoint revision",
            ));
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
        .expect("static safe fake message builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::store::StoredMessage;
    use nexus_core::{
        ApprovedScope, CallId, EffectState, Evidence, ExecutionStatus, M0_REVISION, NormalizedArgs,
        RunId, ToolId,
    };

    fn checkpoint(session: &str, logical_revision: u64) -> SessionCheckpoint {
        SessionCheckpoint::new(
            SessionId::new(session).expect("valid"),
            STORE_FORMAT_REVISION,
            logical_revision,
            vec![StoredMessage {
                source: "user".to_owned(),
                text: "hello".to_owned(),
                complete: true,
            }],
            "fake-profile",
        )
        .expect("valid checkpoint builds")
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

    #[test]
    fn self_identifies_as_non_durable_test_only() {
        let store = EphemeralStore::new();
        assert!(!store.is_durable());
        assert_eq!(store.durability(), PersistenceState::Ephemeral);
        assert_eq!(store.format_revision(), 0);
        assert_eq!(store.format_revision(), M0_REVISION);
    }

    #[test]
    fn save_and_load_round_trip() {
        let mut store = EphemeralStore::new();
        store
            .save_checkpoint(&checkpoint("sess-1", 3))
            .expect("save works");
        let loaded = store
            .load_session(&SessionId::new("sess-1").expect("valid"))
            .expect("load works");
        assert_eq!(loaded.logical_revision(), 3);
        assert_eq!(loaded.messages().len(), 1);
    }

    #[test]
    fn stale_save_is_rejected_newer_wins() {
        let mut store = EphemeralStore::new();
        store
            .save_checkpoint(&checkpoint("sess-1", 3))
            .expect("save works");
        let stale = store.save_checkpoint(&checkpoint("sess-1", 2));
        assert!(stale.is_err());
        let loaded = store
            .load_session(&SessionId::new("sess-1").expect("valid"))
            .expect("newer revision kept");
        assert_eq!(loaded.logical_revision(), 3);
        store
            .save_checkpoint(&checkpoint("sess-1", 4))
            .expect("newer saves");
        store
            .save_checkpoint(&checkpoint("sess-1", 4))
            .expect("same revision is an idempotent overwrite");
    }

    #[test]
    fn interrupted_write_hook_fails_once_then_disarms() {
        let mut store = EphemeralStore::new();
        store.set_fail_next_write(true);
        assert!(store.fail_next_write());
        let error = store.save_checkpoint(&checkpoint("sess-1", 1)).unwrap_err();
        assert_eq!(error.category(), ErrorCategory::StorageFailure);
        assert!(!store.fail_next_write());
        store
            .save_checkpoint(&checkpoint("sess-1", 1))
            .expect("retry works");
        store.set_fail_next_write(true);
        assert!(store.record_intent(&intent()).is_err());
        store.set_fail_next_write(true);
        assert!(store.record_outcome(&outcome()).is_err());
        assert!(store.intents().is_empty());
        store.record_intent(&intent()).expect("disarmed");
        store.record_outcome(&outcome()).expect("disarmed");
        assert_eq!(store.intents().len(), 1);
        assert_eq!(store.outcomes().len(), 1);
    }

    #[test]
    fn listing_is_bounded_and_missing_load_fails() {
        let mut store = EphemeralStore::new();
        for session in ["sess-b", "sess-a", "sess-c"] {
            store
                .save_checkpoint(&checkpoint(session, 1))
                .expect("save works");
        }
        let listed = store.list_sessions(2).expect("list works");
        assert_eq!(listed.len(), 2);
        let all = store.list_sessions(16).expect("list works");
        assert_eq!(all.len(), 3);
        assert!(
            all.windows(2)
                .all(|pair| pair[0].session.as_str() <= pair[1].session.as_str()),
            "deterministic session order"
        );
        assert!(
            store
                .load_session(&SessionId::new("sess-missing").expect("valid"))
                .is_err()
        );
    }
}
