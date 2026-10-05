//! Test-only ephemeral session store.
//!
//! [`EphemeralStore`] is an in-memory [`SessionStore`]
//! that is explicitly **non-durable**: [`EphemeralStore::is_durable`] always
//! returns `false` and [`EphemeralStore::durability`] reports
//! [`PersistenceState::Ephemeral`](nexus_core::PersistenceState), so crash
//! recovery and durable history are unavailable. It must never be mistaken
//! for evidence of crash-safe file persistence. Format revisions use exact
//! equality against [`STORE_FORMAT_REVISION`]
//! (M0 revision `0`); a stale checkpoint (older logical revision than the
//! stored one) is rejected explicitly, and an equal-revision save is accepted
//! only when the checkpoint is identical (idempotent retry) — a conflicting
//! equal-revision save fails explicitly and preserves the stored checkpoint.
//! Retained sessions, intent records, and outcome records are bounded by the
//! finite M0-test caps in [`nexus_core::store`]; listing is bounded by the
//! caller's limit over a `BTreeMap` without materializing all sessions. The
//! plain-bool `fail_next_write` flag simulates an interrupted write for P6
//! without any framework and never replaces a prior observation.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use nexus_core::store::{
    MAX_INTENT_RECORDS, MAX_OUTCOME_RECORDS, MAX_SESSIONS, check_checkpoint_bounds,
    check_format_revision,
};
use nexus_core::{
    AgentError, ErrorCategory, PersistenceState, RetryGuidance, STORE_FORMAT_REVISION,
    SessionCheckpoint, SessionId, SessionMetadata, SessionStore, ToolIntentRecord,
    ToolOutcomeRecord,
};

/// Non-durable test-only session store. See the module docs.
pub struct EphemeralStore {
    checkpoints: BTreeMap<String, SessionCheckpoint>,
    intents: Vec<ToolIntentRecord>,
    outcomes: Vec<ToolOutcomeRecord>,
    fail_next_write: bool,
}

impl EphemeralStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self {
            checkpoints: BTreeMap::new(),
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
        // `BTreeMap` iteration is ordered by session id, and `take` stops at
        // the requested bound instead of materializing every session.
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
                // Equal revisions are an idempotent retry only when the
                // checkpoint is identical; a conflicting write preserves the
                // stored checkpoint and fails explicitly.
                Ordering::Equal if checkpoint != stored => {
                    return Err(store_error(
                        ErrorCategory::InvalidInput,
                        "conflicting checkpoint at equal revision",
                    ));
                }
                Ordering::Equal | Ordering::Greater => {}
            }
        } else if self.checkpoints.len() >= MAX_SESSIONS {
            return Err(store_error(
                ErrorCategory::ResourceLimit,
                "session capacity exhausted",
            ));
        }
        self.checkpoints
            .insert(checkpoint.session().as_str().to_owned(), checkpoint.clone());
        Ok(())
    }

    fn record_intent(&mut self, intent: &ToolIntentRecord) -> Result<(), AgentError> {
        self.take_interruption()?;
        if self.intents.len() >= MAX_INTENT_RECORDS {
            return Err(store_error(
                ErrorCategory::ResourceLimit,
                "intent capacity exhausted",
            ));
        }
        self.intents.push(intent.clone());
        Ok(())
    }

    fn record_outcome(&mut self, outcome: &ToolOutcomeRecord) -> Result<(), AgentError> {
        self.take_interruption()?;
        if self.outcomes.len() >= MAX_OUTCOME_RECORDS {
            return Err(store_error(
                ErrorCategory::ResourceLimit,
                "outcome capacity exhausted",
            ));
        }
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

    fn checkpoint_with(
        session: &str,
        logical_revision: u64,
        text: &str,
        profile: &str,
    ) -> SessionCheckpoint {
        SessionCheckpoint::new(
            SessionId::new(session).expect("valid"),
            STORE_FORMAT_REVISION,
            logical_revision,
            vec![StoredMessage {
                source: "user".to_owned(),
                text: text.to_owned(),
                complete: true,
            }],
            profile,
        )
        .expect("valid checkpoint builds")
    }

    fn checkpoint(session: &str, logical_revision: u64) -> SessionCheckpoint {
        checkpoint_with(session, logical_revision, "hello", "fake-profile")
    }

    fn intent_for(call: usize) -> ToolIntentRecord {
        ToolIntentRecord {
            run: RunId::new("run-1").expect("valid"),
            call: CallId::new(format!("call-{call}")).expect("valid"),
            tool: ToolId::new("host_write", M0_REVISION).expect("valid"),
            args: NormalizedArgs::new(r#"{"path":"dst"}"#).expect("valid"),
            scope: ApprovedScope::new("project-write").expect("valid"),
        }
    }

    fn intent() -> ToolIntentRecord {
        intent_for(1)
    }

    fn outcome_for(call: usize) -> ToolOutcomeRecord {
        ToolOutcomeRecord {
            run: RunId::new("run-1").expect("valid"),
            call: CallId::new(format!("call-{call}")).expect("valid"),
            status: ExecutionStatus::Succeeded,
            effect: EffectState::KnownApplied,
            evidence: Evidence::HostObserved,
            truncated: false,
        }
    }

    fn outcome() -> ToolOutcomeRecord {
        outcome_for(1)
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
            .expect("identical equal-revision retry is accepted");
    }

    #[test]
    fn equal_revision_conflict_is_rejected_and_preserves_stored() {
        let mut store = EphemeralStore::new();
        let original = checkpoint_with("sess-1", 3, "hello", "fake-profile");
        store
            .save_checkpoint(&original)
            .expect("initial save works");
        store
            .save_checkpoint(&original)
            .expect("identical retry is idempotent");

        let conflicting = checkpoint_with("sess-1", 3, "changed", "other-profile");
        let error = store.save_checkpoint(&conflicting).unwrap_err();
        assert_eq!(error.category(), ErrorCategory::InvalidInput);
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        let loaded = store
            .load_session(&SessionId::new("sess-1").expect("valid"))
            .expect("stored checkpoint is preserved");
        assert_eq!(loaded.logical_revision(), 3);
        assert_eq!(loaded.messages()[0].text, "hello");
        assert_eq!(loaded.profile(), "fake-profile");

        store
            .save_checkpoint(&checkpoint_with("sess-1", 4, "changed", "other-profile"))
            .expect("strictly newer revision replaces");
        let reloaded = store
            .load_session(&SessionId::new("sess-1").expect("valid"))
            .expect("newer checkpoint loads");
        assert_eq!(reloaded.logical_revision(), 4);
        assert_eq!(reloaded.messages()[0].text, "changed");
    }

    #[test]
    fn interrupted_write_never_replaces_prior_observation() {
        let mut store = EphemeralStore::new();
        store
            .save_checkpoint(&checkpoint_with("sess-1", 1, "original", "fake-profile"))
            .expect("save works");
        store.set_fail_next_write(true);
        assert!(
            store
                .save_checkpoint(&checkpoint_with("sess-1", 2, "replacement", "fake-profile"))
                .is_err()
        );
        let loaded = store
            .load_session(&SessionId::new("sess-1").expect("valid"))
            .expect("prior checkpoint survives the interrupted write");
        assert_eq!(loaded.logical_revision(), 1);
        assert_eq!(loaded.messages()[0].text, "original");

        store.set_fail_next_write(true);
        assert!(store.record_intent(&intent()).is_err());
        store.set_fail_next_write(true);
        assert!(store.record_outcome(&outcome()).is_err());
        assert!(store.intents().is_empty());
        assert!(store.outcomes().is_empty());
    }

    #[test]
    fn finite_capacities_fail_explicitly_without_eviction() {
        let mut store = EphemeralStore::new();
        for index in 0..MAX_SESSIONS {
            store
                .save_checkpoint(&checkpoint(&format!("sess-{index:03}"), 1))
                .expect("within session capacity");
        }
        let error = store
            .save_checkpoint(&checkpoint("sess-over", 1))
            .unwrap_err();
        assert_eq!(error.category(), ErrorCategory::ResourceLimit);
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        store
            .save_checkpoint(&checkpoint("sess-000", 2))
            .expect("existing session stays writable at capacity");

        for index in 0..MAX_INTENT_RECORDS {
            store
                .record_intent(&intent_for(index))
                .expect("within intent capacity");
        }
        let error = store.record_intent(&intent_for(0)).unwrap_err();
        assert_eq!(error.category(), ErrorCategory::ResourceLimit);
        assert_eq!(store.intents().len(), MAX_INTENT_RECORDS);

        for index in 0..MAX_OUTCOME_RECORDS {
            store
                .record_outcome(&outcome_for(index))
                .expect("within outcome capacity");
        }
        let error = store.record_outcome(&outcome_for(0)).unwrap_err();
        assert_eq!(error.category(), ErrorCategory::ResourceLimit);
        assert_eq!(store.outcomes().len(), MAX_OUTCOME_RECORDS);
    }

    #[test]
    fn listing_is_bounded_and_missing_load_fails() {
        let mut store = EphemeralStore::new();
        for session in ["sess-b", "sess-a", "sess-c"] {
            store
                .save_checkpoint(&checkpoint(session, 1))
                .expect("save works");
        }
        assert!(store.list_sessions(0).expect("zero bound works").is_empty());
        let listed = store.list_sessions(2).expect("list works");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].session.as_str(), "sess-a");
        assert_eq!(listed[1].session.as_str(), "sess-b");
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
}

#[cfg(test)]
mod cov_store_private {
    //! Unit coverage for the private internals of [`EphemeralStore`]:
    //! explicit error construction, single-shot interruption precedence,
    //! capacity gates that only apply to new sessions, construction-bypass
    //! rechecks, and the guarantee that failed writes leave every private
    //! field untouched. Deterministic; no clock, randomness, or I/O.

    use super::*;
    use nexus_core::store::{MAX_PROFILE_LEN, MAX_STORED_MESSAGES, StoredMessage};
    use nexus_core::{
        ApprovedScope, CallId, EffectState, Evidence, ExecutionStatus, M0_REVISION, NormalizedArgs,
        RunId, ToolId,
    };

    fn session(id: &str) -> SessionId {
        SessionId::new(id).expect("valid session id")
    }

    fn message(text: &str) -> StoredMessage {
        StoredMessage {
            source: "user".to_owned(),
            text: text.to_owned(),
            complete: true,
        }
    }

    fn checkpoint(id: &str, logical_revision: u64) -> SessionCheckpoint {
        SessionCheckpoint::new(
            session(id),
            STORE_FORMAT_REVISION,
            logical_revision,
            vec![message("hello")],
            "private-profile",
        )
        .expect("valid checkpoint builds")
    }

    fn intent(call: &str) -> ToolIntentRecord {
        ToolIntentRecord {
            run: RunId::new("run-1").expect("valid run id"),
            call: CallId::new(call).expect("valid call id"),
            tool: ToolId::new("host_write", M0_REVISION).expect("valid tool id"),
            args: NormalizedArgs::new(r#"{"path":"dst"}"#).expect("valid args"),
            scope: ApprovedScope::new("project-write").expect("valid scope"),
        }
    }

    fn outcome(call: &str) -> ToolOutcomeRecord {
        ToolOutcomeRecord {
            run: RunId::new("run-1").expect("valid run id"),
            call: CallId::new(call).expect("valid call id"),
            status: ExecutionStatus::Succeeded,
            effect: EffectState::KnownApplied,
            evidence: Evidence::HostObserved,
            truncated: false,
        }
    }

    #[test]
    fn private_store_error_is_explicit_do_not_retry_for_every_probe() {
        let probes = [
            (ErrorCategory::StorageFailure, "session not found"),
            (ErrorCategory::StorageFailure, "interrupted write"),
            (ErrorCategory::InvalidInput, "stale checkpoint revision"),
            (
                ErrorCategory::InvalidInput,
                "conflicting checkpoint at equal revision",
            ),
            (ErrorCategory::ResourceLimit, "session capacity exhausted"),
            (ErrorCategory::ResourceLimit, "intent capacity exhausted"),
            (ErrorCategory::ResourceLimit, "outcome capacity exhausted"),
        ];
        for (category, message) in probes {
            let error = store_error(category, message);
            assert_eq!(error.category(), category);
            assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
            assert_eq!(error.message(), message);
            assert!(error.correlation().is_empty());
        }
    }

    #[test]
    fn private_take_interruption_is_single_shot() {
        let mut store = EphemeralStore::new();
        assert!(
            store.take_interruption().is_ok(),
            "disarmed take is a no-op"
        );
        store.set_fail_next_write(true);
        let error = store
            .take_interruption()
            .expect_err("armed take reports the interruption");
        assert_eq!(error.category(), ErrorCategory::StorageFailure);
        assert_eq!(error.message(), "interrupted write");
        assert!(
            !store.fail_next_write(),
            "taking the interruption disarms it"
        );
        assert!(store.take_interruption().is_ok(), "second take is a no-op");
    }

    #[test]
    fn private_interruption_precedes_validation_and_capacity() {
        let mut store = EphemeralStore::new();
        let current = checkpoint("sess-order", 5);
        store.save_checkpoint(&current).expect("save works");

        // Interruption is taken before the stale-revision check: the armed
        // save reports StorageFailure even though the checkpoint is also
        // stale, consumes the flag, and preserves the stored record.
        let stale = checkpoint("sess-order", 1);
        store.set_fail_next_write(true);
        let error = store
            .save_checkpoint(&stale)
            .expect_err("armed save fails as interrupted");
        assert_eq!(error.category(), ErrorCategory::StorageFailure);
        assert!(!store.fail_next_write());
        assert_eq!(store.checkpoints.get("sess-order"), Some(&current));
        let error = store
            .save_checkpoint(&stale)
            .expect_err("retry now reports staleness");
        assert_eq!(error.category(), ErrorCategory::InvalidInput);
        assert_eq!(error.message(), "stale checkpoint revision");

        // Interruption is taken before the capacity checks too: seed both
        // record vectors at their caps directly and observe StorageFailure,
        // not ResourceLimit, with the vectors untouched.
        store.intents = (0..MAX_INTENT_RECORDS).map(|_| intent("call-1")).collect();
        store.outcomes = (0..MAX_OUTCOME_RECORDS)
            .map(|_| outcome("call-1"))
            .collect();

        store.set_fail_next_write(true);
        let error = store
            .record_intent(&intent("call-2"))
            .expect_err("armed intent fails as interrupted");
        assert_eq!(error.category(), ErrorCategory::StorageFailure);
        assert_eq!(store.intents.len(), MAX_INTENT_RECORDS);
        let error = store
            .record_intent(&intent("call-2"))
            .expect_err("retry now reports the cap");
        assert_eq!(error.category(), ErrorCategory::ResourceLimit);
        assert_eq!(error.message(), "intent capacity exhausted");

        store.set_fail_next_write(true);
        let error = store
            .record_outcome(&outcome("call-2"))
            .expect_err("armed outcome fails as interrupted");
        assert_eq!(error.category(), ErrorCategory::StorageFailure);
        assert_eq!(store.outcomes.len(), MAX_OUTCOME_RECORDS);
        let error = store
            .record_outcome(&outcome("call-2"))
            .expect_err("retry now reports the cap");
        assert_eq!(error.category(), ErrorCategory::ResourceLimit);
        assert_eq!(error.message(), "outcome capacity exhausted");
    }

    #[test]
    fn private_invalid_checkpoints_never_reach_the_store() {
        let store = EphemeralStore::new();

        // `SessionCheckpoint::new` is the only public constructor, and it
        // validates the exact format revision and the stored-part bounds.
        // Every invalid shape therefore fails before the store sees a value,
        // so the store's internal rechecks are unreachable from this crate;
        // they are covered by `nexus_core`'s own tests.
        let wrong_format = SessionCheckpoint::new(
            session("sess-format"),
            STORE_FORMAT_REVISION + 1,
            1,
            vec![message("hello")],
            "p",
        )
        .expect_err("wrong format revision cannot construct");
        assert_eq!(wrong_format.category(), ErrorCategory::InvalidInput);

        let over_messages = SessionCheckpoint::new(
            session("sess-messages"),
            STORE_FORMAT_REVISION,
            1,
            (0..=MAX_STORED_MESSAGES).map(|_| message("t")).collect(),
            "p",
        )
        .expect_err("over-count messages cannot construct");
        assert_eq!(over_messages.category(), ErrorCategory::InvalidInput);

        let over_profile = SessionCheckpoint::new(
            session("sess-profile"),
            STORE_FORMAT_REVISION,
            1,
            vec![message("hello")],
            "p".repeat(MAX_PROFILE_LEN + 1),
        )
        .expect_err("over-long profile cannot construct");
        assert_eq!(over_profile.category(), ErrorCategory::InvalidInput);

        assert!(store.checkpoints.is_empty());
        assert!(
            store
                .list_sessions(usize::MAX)
                .expect("list works")
                .is_empty()
        );
    }

    #[test]
    fn private_failed_writes_leave_every_field_untouched() {
        let mut store = EphemeralStore::new();
        let stored = checkpoint("sess-state", 4);
        store.save_checkpoint(&stored).expect("save works");
        store
            .record_intent(&intent("call-1"))
            .expect("intent works");
        store
            .record_outcome(&outcome("call-1"))
            .expect("outcome works");

        let checkpoints = store.checkpoints.clone();
        let intents = store.intents.clone();
        let outcomes = store.outcomes.clone();
        assert!(!store.fail_next_write);

        store
            .save_checkpoint(&checkpoint("sess-state", 3))
            .expect_err("stale save fails");
        let conflict = SessionCheckpoint::new(
            session("sess-state"),
            STORE_FORMAT_REVISION,
            4,
            vec![message("changed")],
            "private-profile",
        )
        .expect("conflicting checkpoint is constructor-valid");
        store
            .save_checkpoint(&conflict)
            .expect_err("conflicting save fails");
        store.set_fail_next_write(true);
        store
            .save_checkpoint(&checkpoint("sess-state", 5))
            .expect_err("interrupted save fails");

        assert_eq!(store.checkpoints, checkpoints);
        assert_eq!(store.intents, intents);
        assert_eq!(store.outcomes, outcomes);
        assert!(
            !store.fail_next_write,
            "the failed interrupted save consumed and disarmed the flag"
        );
    }

    #[test]
    fn private_session_capacity_gates_only_new_sessions() {
        let mut store = EphemeralStore::new();
        for index in 0..MAX_SESSIONS {
            store
                .save_checkpoint(&checkpoint(&format!("sess-{index:03}"), 1))
                .expect("within session capacity");
        }
        assert_eq!(store.checkpoints.len(), MAX_SESSIONS);

        let error = store
            .save_checkpoint(&checkpoint("sess-new", 1))
            .expect_err("new session over the cap fails");
        assert_eq!(error.category(), ErrorCategory::ResourceLimit);
        assert_eq!(error.message(), "session capacity exhausted");
        assert_eq!(store.checkpoints.len(), MAX_SESSIONS);
        assert!(!store.checkpoints.contains_key("sess-new"));

        // An existing session stays writable at the cap and updates in place.
        let updated = checkpoint("sess-000", 2);
        store
            .save_checkpoint(&updated)
            .expect("existing session stays writable at the cap");
        assert_eq!(store.checkpoints.len(), MAX_SESSIONS);
        assert_eq!(store.checkpoints.get("sess-000"), Some(&updated));
    }

    #[test]
    fn private_identical_equal_revision_retry_is_state_preserving() {
        let mut store = EphemeralStore::new();
        let original = checkpoint("sess-idem", 3);
        store.save_checkpoint(&original).expect("save works");
        let before = store.checkpoints.clone();

        store
            .save_checkpoint(&original)
            .expect("identical retry is accepted");
        assert_eq!(store.checkpoints, before);
        assert_eq!(store.checkpoints.len(), 1);
        assert_eq!(store.checkpoints.get("sess-idem"), Some(&original));
    }

    #[test]
    fn private_listing_takes_exactly_the_bound_in_identity_order() {
        let mut store = EphemeralStore::new();
        for id in ["sess-c", "sess-a", "sess-b"] {
            store
                .save_checkpoint(&checkpoint(id, 1))
                .expect("save works");
        }
        assert!(store.list_sessions(0).expect("zero bound works").is_empty());
        let listed = store.list_sessions(2).expect("bounded list works");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].session.as_str(), "sess-a");
        assert_eq!(listed[1].session.as_str(), "sess-b");
        let all = store
            .list_sessions(usize::MAX)
            .expect("over-large bound works");
        assert_eq!(all.len(), 3);
        assert_eq!(all[2].session.as_str(), "sess-c");
    }

    #[test]
    fn private_default_is_an_empty_non_durable_store() {
        let defaulted = EphemeralStore::default();
        assert!(defaulted.checkpoints.is_empty());
        assert!(defaulted.intents.is_empty());
        assert!(defaulted.outcomes.is_empty());
        assert!(!defaulted.fail_next_write);
        assert!(!defaulted.is_durable());
        assert_eq!(defaulted.durability(), PersistenceState::Ephemeral);
        assert_eq!(defaulted.format_revision(), STORE_FORMAT_REVISION);
    }

    #[test]
    fn private_records_are_owned_copies_not_aliases() {
        let mut store = EphemeralStore::new();
        let mut intent_record = intent("call-alias");
        let intent_before = intent_record.clone();
        store.record_intent(&intent_record).expect("intent records");
        intent_record.call = CallId::new("call-mutated").expect("valid call id");
        assert_eq!(store.intents[0], intent_before);

        let mut outcome_record = outcome("call-alias");
        let outcome_before = outcome_record.clone();
        store
            .record_outcome(&outcome_record)
            .expect("outcome records");
        outcome_record.truncated = true;
        assert_eq!(store.outcomes[0], outcome_before);
    }
}
