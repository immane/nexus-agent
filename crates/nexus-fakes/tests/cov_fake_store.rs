#![forbid(unsafe_code)]

//! Public-boundary coverage hardening for [`EphemeralStore`].
//!
//! `nexus-fakes` ships the explicitly non-durable in-memory [`SessionStore`]
//! used by deterministic tests. These integration tests exercise it only
//! through its public API and pin the documented contract from outside the
//! crate:
//!
//! - save/load round-trips preserve every stored field and sessions stay
//!   independent;
//! - stale logical revisions are rejected and the newest revision wins;
//! - an equal-revision save is accepted only when identical; a conflicting
//!   equal-revision save fails explicitly and preserves the stored record;
//! - the single-shot interrupted-write hook fails the next write explicitly
//!   and never replaces a prior observation;
//! - finite session/intent/outcome capacities fail with explicit
//!   `ResourceLimit` errors and never evict;
//! - listing is bounded, identity-ordered, and prefix-consistent;
//! - the store self-identifies as non-durable (`Ephemeral`), never `Saved`.
//!
//! No clock, randomness, threads, or external I/O: every run is deterministic.

use nexus_core::store::{MAX_INTENT_RECORDS, MAX_OUTCOME_RECORDS, MAX_SESSIONS, StoredMessage};
use nexus_core::{
    ApprovedScope, CallId, EffectState, ErrorCategory, Evidence, ExecutionStatus, M0_REVISION,
    NormalizedArgs, PersistenceState, RetryGuidance, RunId, STORE_FORMAT_REVISION,
    SessionCheckpoint, SessionId, SessionMetadata, SessionStore, ToolId, ToolIntentRecord,
    ToolOutcomeRecord,
};
use nexus_fakes::EphemeralStore;

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

fn checkpoint_with(
    id: &str,
    logical_revision: u64,
    messages: Vec<StoredMessage>,
    profile: &str,
) -> SessionCheckpoint {
    SessionCheckpoint::new(
        session(id),
        STORE_FORMAT_REVISION,
        logical_revision,
        messages,
        profile,
    )
    .expect("valid checkpoint builds")
}

fn checkpoint(id: &str, logical_revision: u64) -> SessionCheckpoint {
    checkpoint_with(
        id,
        logical_revision,
        vec![message("user", "hello", true)],
        "fake-profile",
    )
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
fn self_identifies_as_non_durable_test_only_store() {
    let mut store = EphemeralStore::new();
    assert!(!store.is_durable());
    assert_eq!(store.durability(), PersistenceState::Ephemeral);
    assert_ne!(store.durability(), PersistenceState::Saved);
    assert_ne!(store.durability(), PersistenceState::SaveFailed);
    assert_eq!(store.format_revision(), STORE_FORMAT_REVISION);
    assert_eq!(store.format_revision(), M0_REVISION);

    // Writes never upgrade the durability claim.
    store
        .save_checkpoint(&checkpoint("sess-durable", 1))
        .expect("save works");
    store
        .record_intent(&intent("call-1"))
        .expect("intent works");
    store
        .record_outcome(&outcome("call-1"))
        .expect("outcome works");
    assert!(!store.is_durable());
    assert_eq!(store.durability(), PersistenceState::Ephemeral);

    // Default constructs the same empty, non-durable store.
    let defaulted = EphemeralStore::default();
    assert!(!defaulted.is_durable());
    assert_eq!(defaulted.durability(), PersistenceState::Ephemeral);
    assert!(
        defaulted
            .list_sessions(usize::MAX)
            .expect("list works")
            .is_empty()
    );
}

#[test]
fn save_load_round_trip_preserves_every_stored_field() {
    let mut store = EphemeralStore::new();
    let original = checkpoint_with(
        "sess-round-trip",
        7,
        vec![
            message("user", "first", true),
            message("assistant", "partial", false),
        ],
        "profile-round-trip",
    );
    store.save_checkpoint(&original).expect("save works");

    let loaded = store
        .load_session(&session("sess-round-trip"))
        .expect("load works");
    assert_eq!(loaded, original, "load returns the exact saved checkpoint");
    assert_eq!(loaded.session().as_str(), "sess-round-trip");
    assert_eq!(loaded.format_revision(), STORE_FORMAT_REVISION);
    assert_eq!(loaded.logical_revision(), 7);
    assert_eq!(loaded.profile(), "profile-round-trip");
    assert_eq!(loaded.messages(), original.messages());
    assert!(loaded.messages()[0].complete);
    assert!(!loaded.messages()[1].complete);

    // Sessions are independent; one session never aliases another.
    store
        .save_checkpoint(&checkpoint("sess-other", 1))
        .expect("save works");
    assert_eq!(
        store
            .load_session(&session("sess-round-trip"))
            .expect("load works"),
        original
    );
    assert_ne!(
        store
            .load_session(&session("sess-other"))
            .expect("load works"),
        original
    );

    // Advancing the same session replaces the stored record with the newest.
    let newer = checkpoint_with(
        "sess-round-trip",
        8,
        vec![message("user", "second", true)],
        "profile-round-trip",
    );
    store.save_checkpoint(&newer).expect("newer save works");
    assert_eq!(
        store
            .load_session(&session("sess-round-trip"))
            .expect("load works"),
        newer
    );

    // A missing session fails explicitly and fabricates nothing.
    let error = store
        .load_session(&session("sess-missing"))
        .expect_err("missing session load fails");
    assert_eq!(error.category(), ErrorCategory::StorageFailure);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert!(!error.message().is_empty());
    assert!(error.correlation().is_empty());
}

#[test]
fn stale_save_is_rejected_and_newest_revision_wins() {
    let mut store = EphemeralStore::new();
    let current = checkpoint_with(
        "sess-stale",
        5,
        vec![message("user", "revision-5", true)],
        "profile-5",
    );
    store.save_checkpoint(&current).expect("save works");

    for stale_revision in [0, 1, 4] {
        let stale = checkpoint_with(
            "sess-stale",
            stale_revision,
            vec![message("user", "stale", true)],
            "stale-profile",
        );
        let error = store
            .save_checkpoint(&stale)
            .expect_err("stale revision is rejected");
        assert_eq!(error.category(), ErrorCategory::InvalidInput);
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        assert_eq!(
            store
                .load_session(&session("sess-stale"))
                .expect("load works"),
            current,
            "stale save {stale_revision} never replaces the stored checkpoint"
        );
    }

    // A strictly newer revision replaces the stored checkpoint.
    let newest = checkpoint_with(
        "sess-stale",
        6,
        vec![message("user", "revision-6", true)],
        "profile-6",
    );
    store.save_checkpoint(&newest).expect("newer save works");
    assert_eq!(
        store
            .load_session(&session("sess-stale"))
            .expect("load works"),
        newest
    );

    // An identical equal-revision retry is idempotent, not a conflict.
    store
        .save_checkpoint(&newest)
        .expect("identical equal-revision retry is accepted");
    assert_eq!(
        store
            .load_session(&session("sess-stale"))
            .expect("load works"),
        newest
    );
}

#[test]
fn equal_revision_conflict_is_rejected_and_preserves_stored() {
    let mut store = EphemeralStore::new();
    let stored = checkpoint_with(
        "sess-conflict",
        3,
        vec![message("user", "stored", true)],
        "stored-profile",
    );
    store.save_checkpoint(&stored).expect("save works");

    let conflicts = [
        checkpoint_with(
            "sess-conflict",
            3,
            vec![message("user", "changed", true)],
            "stored-profile",
        ),
        checkpoint_with(
            "sess-conflict",
            3,
            vec![message("user", "stored", false)],
            "stored-profile",
        ),
        checkpoint_with(
            "sess-conflict",
            3,
            vec![message("assistant", "stored", true)],
            "stored-profile",
        ),
        checkpoint_with("sess-conflict", 3, vec![], "stored-profile"),
        checkpoint_with(
            "sess-conflict",
            3,
            vec![message("user", "stored", true)],
            "other-profile",
        ),
    ];
    for conflict in &conflicts {
        assert_ne!(conflict, &stored, "fixture must actually conflict");
        let error = store
            .save_checkpoint(conflict)
            .expect_err("conflicting equal revision fails");
        assert_eq!(error.category(), ErrorCategory::InvalidInput);
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        assert_eq!(
            store
                .load_session(&session("sess-conflict"))
                .expect("load works"),
            stored,
            "stored checkpoint survives every equal-revision conflict"
        );
    }

    // A strictly newer revision is allowed to replace the record.
    let newer = checkpoint_with(
        "sess-conflict",
        4,
        vec![message("user", "changed", true)],
        "other-profile",
    );
    store
        .save_checkpoint(&newer)
        .expect("newer revision replaces");
    assert_eq!(
        store
            .load_session(&session("sess-conflict"))
            .expect("load works"),
        newer
    );
}

#[test]
fn interrupted_write_never_replaces_prior_observation() {
    let mut store = EphemeralStore::new();
    let prior = checkpoint_with(
        "sess-interrupted",
        2,
        vec![message("user", "prior", true)],
        "prior-profile",
    );
    store.save_checkpoint(&prior).expect("save works");
    store
        .record_intent(&intent("call-1"))
        .expect("intent works");
    store
        .record_outcome(&outcome("call-1"))
        .expect("outcome works");

    assert!(!store.fail_next_write(), "the hook starts disarmed");
    store.set_fail_next_write(true);
    assert!(store.fail_next_write(), "the hook reports its armed state");

    let replacement = checkpoint_with(
        "sess-interrupted",
        3,
        vec![message("user", "replacement", true)],
        "prior-profile",
    );
    let error = store
        .save_checkpoint(&replacement)
        .expect_err("interrupted save fails");
    assert_eq!(error.category(), ErrorCategory::StorageFailure);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert!(
        !store.fail_next_write(),
        "the interruption is single-shot and disarms itself"
    );
    assert_eq!(
        store
            .load_session(&session("sess-interrupted"))
            .expect("load works"),
        prior,
        "the prior checkpoint survives the interrupted write"
    );

    // Disarmed, the same replacement succeeds.
    store
        .save_checkpoint(&replacement)
        .expect("retry after interruption succeeds");
    assert_eq!(
        store
            .load_session(&session("sess-interrupted"))
            .expect("load works"),
        replacement
    );

    // Interrupted intent and outcome writes append nothing.
    store.set_fail_next_write(true);
    let error = store
        .record_intent(&intent("call-2"))
        .expect_err("interrupted intent fails");
    assert_eq!(error.category(), ErrorCategory::StorageFailure);
    store.set_fail_next_write(true);
    let error = store
        .record_outcome(&outcome("call-2"))
        .expect_err("interrupted outcome fails");
    assert_eq!(error.category(), ErrorCategory::StorageFailure);
    assert_eq!(store.intents().len(), 1);
    assert_eq!(store.intents()[0].call.as_str(), "call-1");
    assert_eq!(store.outcomes().len(), 1);
    assert_eq!(store.outcomes()[0].call.as_str(), "call-1");

    // The disarmed retry appends exactly once.
    store
        .record_intent(&intent("call-2"))
        .expect("intent works");
    store
        .record_outcome(&outcome("call-2"))
        .expect("outcome works");
    assert_eq!(store.intents().len(), 2);
    assert_eq!(store.intents()[1].call.as_str(), "call-2");
    assert_eq!(store.outcomes().len(), 2);
    assert_eq!(store.outcomes()[1].call.as_str(), "call-2");
}

#[test]
fn finite_capacities_fail_explicitly_without_eviction() {
    let mut store = EphemeralStore::new();

    // Session capacity: exactly MAX_SESSIONS distinct sessions fit.
    for index in 0..MAX_SESSIONS {
        store
            .save_checkpoint(&checkpoint(&format!("sess-{index:03}"), 1))
            .expect("within session capacity");
    }
    let error = store
        .save_checkpoint(&checkpoint("sess-overflow", 1))
        .expect_err("session capacity is enforced");
    assert_eq!(error.category(), ErrorCategory::ResourceLimit);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    let listed = store.list_sessions(usize::MAX).expect("list works");
    assert_eq!(listed.len(), MAX_SESSIONS, "no silent eviction at capacity");
    assert!(
        !listed
            .iter()
            .any(|meta| meta.session.as_str() == "sess-overflow"),
        "the rejected session never lands"
    );

    // Existing sessions stay writable at capacity.
    store
        .save_checkpoint(&checkpoint("sess-000", 2))
        .expect("existing session stays writable at capacity");
    let listed = store.list_sessions(1).expect("list works");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].session.as_str(), "sess-000");
    assert_eq!(listed[0].logical_revision, 2);

    // Intent capacity: the cap is a hard bound, not an eviction trigger.
    for index in 0..MAX_INTENT_RECORDS {
        store
            .record_intent(&intent(&format!("intent-{index:03}")))
            .expect("within intent capacity");
    }
    let error = store
        .record_intent(&intent("intent-overflow"))
        .expect_err("intent capacity is enforced");
    assert_eq!(error.category(), ErrorCategory::ResourceLimit);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(store.intents().len(), MAX_INTENT_RECORDS);
    assert_eq!(store.intents()[0].call.as_str(), "intent-000");
    assert_eq!(
        store.intents()[MAX_INTENT_RECORDS - 1].call.as_str(),
        format!("intent-{:03}", MAX_INTENT_RECORDS - 1)
    );

    // Outcome capacity follows the same explicit-failure rule.
    for index in 0..MAX_OUTCOME_RECORDS {
        store
            .record_outcome(&outcome(&format!("outcome-{index:03}")))
            .expect("within outcome capacity");
    }
    let error = store
        .record_outcome(&outcome("outcome-overflow"))
        .expect_err("outcome capacity is enforced");
    assert_eq!(error.category(), ErrorCategory::ResourceLimit);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(store.outcomes().len(), MAX_OUTCOME_RECORDS);
}

#[test]
fn listing_is_bounded_ordered_and_prefix_consistent() {
    let mut store = EphemeralStore::new();
    // Scrambled insertion order: listing order follows identity, not writes.
    for id in ["sess-e", "sess-a", "sess-d", "sess-b", "sess-c"] {
        store
            .save_checkpoint(&checkpoint(id, 1))
            .expect("save works");
    }
    store
        .save_checkpoint(&checkpoint("sess-c", 9))
        .expect("newer save works");

    let empty = store.list_sessions(0).expect("zero bound works");
    assert!(empty.is_empty(), "limit zero never fabricates entries");

    let full = store
        .list_sessions(usize::MAX)
        .expect("an over-large limit is clamped by the stored count");
    let ids: Vec<&str> = full.iter().map(|meta| meta.session.as_str()).collect();
    assert_eq!(ids, ["sess-a", "sess-b", "sess-c", "sess-d", "sess-e"]);
    assert_eq!(
        full[2].logical_revision, 9,
        "listing reports newest metadata"
    );

    for limit in 0..=full.len() {
        let listed = store.list_sessions(limit).expect("bounded list works");
        assert_eq!(listed.len(), limit, "limit {limit} bounds the result");
        assert_eq!(
            listed,
            full[..limit],
            "limit {limit} returns a stable prefix of the full order"
        );
    }

    // Listing carries metadata only; no history load is required.
    assert_eq!(
        store.list_sessions(3).expect("list works"),
        vec![
            SessionMetadata {
                session: session("sess-a"),
                logical_revision: 1,
            },
            SessionMetadata {
                session: session("sess-b"),
                logical_revision: 1,
            },
            SessionMetadata {
                session: session("sess-c"),
                logical_revision: 9,
            },
        ]
    );

    // Repeated listing is deterministic.
    assert_eq!(
        store.list_sessions(4).expect("list works"),
        store.list_sessions(4).expect("list works")
    );
}

#[test]
fn store_serves_the_session_store_port_object_safely() {
    let mut store = EphemeralStore::new();
    {
        let port: &mut dyn SessionStore = &mut store;
        port.save_checkpoint(&checkpoint("sess-port", 1))
            .expect("save works");
        port.record_intent(&intent("call-1")).expect("intent works");
        port.record_outcome(&outcome("call-1"))
            .expect("outcome works");
    }

    let port: &dyn SessionStore = &store;
    assert_eq!(port.list_sessions(1).expect("list works").len(), 1);
    assert_eq!(
        port.load_session(&session("sess-port"))
            .expect("load works")
            .logical_revision(),
        1
    );
    assert!(port.load_session(&session("sess-missing")).is_err());
}
