#![forbid(unsafe_code)]

//! Coverage hardening for the command/event boundary through the public API:
//! sequence contiguity helpers, run-event envelope ownership, usage
//! provisional/final labels, and snapshot pending/outcome bounds.
//!
//! Deterministic: no clock reads, randomness, runtime, or external services.

use std::time::Duration;

use nexus_core::commands::{EventSequence, MAX_TEXT_FRAGMENT_BYTES, checked_next_sequence};
use nexus_core::{
    ApprovalId, ApprovalNotice, AssistantText, CallId, EffectState, ErrorCategory, EventPayload,
    Evidence, ExecutionStatus, Limits, OutcomeSummary, PersistenceState, RequestId, RetryGuidance,
    RunEvent, RunFinished, RunId, RunLifecycle, RunOutcome, SessionId, Snapshot, ToolFinishedInfo,
    ToolOutcome, ToolProgress, ToolStartedInfo, TurnId, Usage, UsageFinality,
};

fn session() -> SessionId {
    SessionId::new("sess-cov").expect("valid session id")
}

fn run() -> RunId {
    RunId::new("run-cov").expect("valid run id")
}

fn request(raw: &str) -> RequestId {
    RequestId::new(raw).expect("valid request id")
}

fn approval(index: usize) -> ApprovalId {
    ApprovalId::new(format!("appr-{index}")).expect("valid approval id")
}

fn outcome_summary(index: usize) -> OutcomeSummary {
    OutcomeSummary {
        call: CallId::new(format!("call-{index}")).expect("valid call id"),
        status: ExecutionStatus::Succeeded,
        effect: EffectState::KnownApplied,
        evidence: Evidence::HostObserved,
    }
}

fn started_event(seq: EventSequence) -> RunEvent {
    RunEvent::new(
        session(),
        run(),
        seq,
        EventPayload::RunStarted {
            request: request("req-seq"),
        },
    )
}

/// Walks an event stream in order and reports whether sequences start at zero
/// and advance by exactly one through [`checked_next_sequence`]. The maximum
/// sequence has no successor, so any event after it is a gap.
fn sequences_are_contiguous(events: &[RunEvent]) -> bool {
    let mut expected = Some(0);
    for event in events {
        match expected {
            Some(want) if event.seq() == want => {}
            _ => return false,
        }
        expected = checked_next_sequence(event.seq());
    }
    true
}

#[test]
fn checked_next_sequence_advances_without_wrapping() {
    assert_eq!(checked_next_sequence(0), Some(1));
    assert_eq!(checked_next_sequence(41), Some(42));
    assert_eq!(
        checked_next_sequence(EventSequence::MAX - 1),
        Some(EventSequence::MAX)
    );
    assert_eq!(
        checked_next_sequence(EventSequence::MAX),
        None,
        "overflow is reported, never wrapped"
    );
    assert_ne!(checked_next_sequence(EventSequence::MAX), Some(0));
}

#[test]
fn contiguous_event_streams_walk_with_the_public_helper() {
    let events: Vec<RunEvent> = (0..5).map(started_event).collect();
    assert!(sequences_are_contiguous(&events));
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event.seq(), index as EventSequence);
        assert_eq!(event.session(), &session());
        assert_eq!(event.run(), &run());
    }

    let mut gapped = events.clone();
    gapped.remove(2);
    assert!(
        !sequences_are_contiguous(&gapped),
        "a missing sequence is a gap"
    );

    let mut duplicated = events.clone();
    duplicated[2] = started_event(1);
    assert!(
        !sequences_are_contiguous(&duplicated),
        "a repeated sequence is a gap"
    );

    let shifted: Vec<RunEvent> = (1..5).map(started_event).collect();
    assert!(
        !sequences_are_contiguous(&shifted),
        "a stream that starts at one is not contiguous"
    );

    let empty: [RunEvent; 0] = [];
    assert!(
        sequences_are_contiguous(&empty),
        "an empty stream is vacuously contiguous"
    );
}

#[test]
fn run_event_envelope_preserves_owner_sequence_and_payload() {
    let session = SessionId::new("sess-owner").expect("valid session id");
    let run = RunId::new("run-owner").expect("valid run id");
    let payload = EventPayload::RunStarted {
        request: request("req-owner"),
    };
    let event = RunEvent::new(session.clone(), run.clone(), 7, payload.clone());
    assert_eq!(event.session(), &session);
    assert_eq!(event.run(), &run);
    assert_eq!(event.seq(), 7);
    assert_eq!(event.payload(), &payload);
    assert!(!event.is_terminal());
    assert_eq!(event.clone(), event, "envelope equality includes owners");
    event
        .validate()
        .expect("RunStarted carries no payload bounds to violate");

    let other_session = RunEvent::new(
        SessionId::new("sess-other").expect("valid session id"),
        run.clone(),
        7,
        payload.clone(),
    );
    assert_ne!(
        event, other_session,
        "session ownership is part of identity"
    );

    let other_run = RunEvent::new(
        session.clone(),
        RunId::new("run-other").expect("valid run id"),
        7,
        payload.clone(),
    );
    assert_ne!(event, other_run, "run ownership is part of identity");

    let other_seq = RunEvent::new(session.clone(), run.clone(), 8, payload.clone());
    assert_ne!(event, other_seq, "sequence is part of identity");

    let other_payload = RunEvent::new(
        session,
        run,
        7,
        EventPayload::RunStarted {
            request: request("req-other"),
        },
    );
    assert_ne!(event, other_payload, "payload is part of identity");
}

#[test]
fn only_run_finished_payloads_are_terminal() {
    let call = CallId::new("call-term").expect("valid call id");
    let notice = ApprovalNotice::new(
        approval(0),
        call.clone(),
        "delete directory",
        "project scope",
        Duration::from_secs(120),
    )
    .expect("safe notice builds");
    let outcome = ToolOutcome::new(
        ExecutionStatus::Succeeded,
        EffectState::KnownApplied,
        Evidence::HostObserved,
        "ok",
        false,
    )
    .expect("bounded outcome builds");
    let non_terminal: [EventPayload; 8] = [
        EventPayload::RunStarted {
            request: request("req-term"),
        },
        EventPayload::AssistantTextDelta(
            AssistantText::new(
                TurnId::new("turn-term").expect("valid turn id"),
                "item-0",
                "hi",
            )
            .expect("valid fragment builds"),
        ),
        EventPayload::ToolCallPreview {
            item_key: "item-0".to_owned(),
        },
        EventPayload::ApprovalRequired(notice),
        EventPayload::ToolStarted(ToolStartedInfo {
            call: call.clone(),
            tool: nexus_core::ToolId::new("host_read", nexus_core::M0_REVISION).unwrap(),
            args_preview: None,
        }),
        EventPayload::ToolOutput(
            ToolProgress::new(call.clone(), "progress", false).expect("valid progress builds"),
        ),
        EventPayload::ToolFinished(ToolFinishedInfo { call, outcome }),
        EventPayload::UsageUpdated(Usage::new(Some(1), None, UsageFinality::Provisional)),
    ];
    for (index, payload) in non_terminal.into_iter().enumerate() {
        let event = RunEvent::new(session(), run(), index as EventSequence, payload);
        assert!(!event.is_terminal(), "payload {index} is not terminal");
        event
            .validate()
            .expect("bounded payload validates through the envelope");
    }

    let finished = RunEvent::new(
        session(),
        run(),
        8,
        EventPayload::RunFinished(
            RunFinished::new(RunOutcome::Completed, PersistenceState::Ephemeral, None)
                .expect("terminal record builds"),
        ),
    );
    assert!(finished.is_terminal());
    finished
        .validate()
        .expect("terminal payload validates through the envelope");
}

#[test]
fn usage_events_keep_provisional_and_final_labels_distinct() {
    let provisional = Usage::new(Some(120), None, UsageFinality::Provisional);
    assert_eq!(provisional.finality(), UsageFinality::Provisional);
    assert_eq!(provisional.input_tokens(), Some(120));
    assert_eq!(
        provisional.output_tokens(),
        None,
        "unknown counters stay unknown"
    );

    let provisional_event =
        RunEvent::new(session(), run(), 3, EventPayload::UsageUpdated(provisional));
    assert!(!provisional_event.is_terminal());
    provisional_event
        .validate()
        .expect("usage payload has no payload bounds");
    let EventPayload::UsageUpdated(carried) = provisional_event.payload() else {
        panic!("expected a UsageUpdated payload");
    };
    assert_eq!(carried.finality(), UsageFinality::Provisional);
    assert_eq!(carried.input_tokens(), Some(120));
    assert_eq!(carried.output_tokens(), None);

    let final_usage = Usage::new(Some(120), Some(80), UsageFinality::Final);
    assert_eq!(final_usage.finality(), UsageFinality::Final);
    assert_ne!(
        provisional, final_usage,
        "the finality label is observable state"
    );
    let final_event = RunEvent::new(session(), run(), 3, EventPayload::UsageUpdated(final_usage));
    assert_ne!(
        provisional_event, final_event,
        "the same counters under a different label are a different event"
    );
    assert_eq!(
        final_event.payload(),
        &EventPayload::UsageUpdated(final_usage),
        "the envelope preserves the exact usage record"
    );
}

#[test]
fn usage_unknown_counters_are_never_fabricated_as_zero() {
    let unknown = Usage::new(None, None, UsageFinality::Provisional);
    assert_eq!(unknown.input_tokens(), None);
    assert_eq!(unknown.output_tokens(), None);
    assert_ne!(unknown.input_tokens(), Some(0));
    assert_ne!(unknown.output_tokens(), Some(0));
    assert_eq!(unknown, Usage::new(None, None, UsageFinality::Provisional));
    assert_ne!(
        unknown,
        Usage::new(None, Some(0), UsageFinality::Provisional)
    );

    let event = RunEvent::new(session(), run(), 4, EventPayload::UsageUpdated(unknown));
    let EventPayload::UsageUpdated(carried) = event.payload() else {
        panic!("expected a UsageUpdated payload");
    };
    assert_eq!(
        carried, &unknown,
        "unknown counters survive the envelope unchanged"
    );
    event
        .validate()
        .expect("all-unknown usage is a valid payload");
}

#[test]
fn snapshot_accepts_exact_pending_and_outcome_bounds() {
    let pending: Vec<ApprovalId> = (0..Limits::M0_TEST_MAX_CONCURRENT_OPS)
        .map(approval)
        .collect();
    let outcomes: Vec<OutcomeSummary> = (0..Limits::M0_TEST_TOOL_CALLS_PER_RUN as usize)
        .map(outcome_summary)
        .collect();
    let snapshot = Snapshot::new(
        session(),
        run(),
        Some(11),
        RunLifecycle::Active,
        pending.clone(),
        outcomes.clone(),
        false,
    )
    .expect("exact bounds build");
    assert_eq!(snapshot.session(), &session());
    assert_eq!(snapshot.run(), &run());
    assert_eq!(snapshot.last_sequence(), Some(11));
    assert_eq!(snapshot.lifecycle(), RunLifecycle::Active);
    assert_eq!(snapshot.pending_approvals(), pending.as_slice());
    assert_eq!(snapshot.known_outcomes(), outcomes.as_slice());
    assert!(!snapshot.is_content_truncated());
    assert_eq!(snapshot.clone(), snapshot);
}

#[test]
fn snapshot_rejects_pending_approvals_over_the_concurrency_budget() {
    let over: Vec<ApprovalId> = (0..=Limits::M0_TEST_MAX_CONCURRENT_OPS)
        .map(approval)
        .collect();
    let error = Snapshot::new(
        session(),
        run(),
        None,
        RunLifecycle::Active,
        over,
        Vec::new(),
        false,
    )
    .expect_err("one past the pending bound is rejected");
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert!(
        !error.message().contains("appr-"),
        "snapshot diagnostics are static and never interpolate identifiers"
    );
}

#[test]
fn snapshot_rejects_known_outcomes_over_the_per_run_call_budget() {
    let over: Vec<OutcomeSummary> = (0..=Limits::M0_TEST_TOOL_CALLS_PER_RUN as usize)
        .map(outcome_summary)
        .collect();
    let error = Snapshot::new(
        session(),
        run(),
        Some(0),
        RunLifecycle::Finalized(RunOutcome::Completed),
        Vec::new(),
        over,
        false,
    )
    .expect_err("one past the known-outcome bound is rejected");
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert!(
        !error.message().contains("call-"),
        "snapshot diagnostics are static and never interpolate identifiers"
    );
}

#[test]
fn snapshot_roundtrips_lifecycle_sequence_and_truncation_flags() {
    for outcome in [
        RunOutcome::Completed,
        RunOutcome::Refused,
        RunOutcome::Failed,
        RunOutcome::Cancelled,
        RunOutcome::LimitReached,
    ] {
        let snapshot = Snapshot::new(
            session(),
            run(),
            Some(EventSequence::MAX),
            RunLifecycle::Finalized(outcome),
            Vec::new(),
            Vec::new(),
            true,
        )
        .expect("bounded finalized snapshot builds");
        assert_eq!(snapshot.lifecycle(), RunLifecycle::Finalized(outcome));
        assert_eq!(
            snapshot.last_sequence(),
            Some(EventSequence::MAX),
            "the sequence maximum round-trips without snapshot arithmetic"
        );
        assert!(snapshot.is_content_truncated());
    }

    let active = Snapshot::new(
        session(),
        run(),
        None,
        RunLifecycle::Active,
        Vec::new(),
        Vec::new(),
        false,
    )
    .expect("bounded active snapshot builds");
    assert_eq!(active.lifecycle(), RunLifecycle::Active);
    assert_eq!(active.last_sequence(), None);
    assert!(!active.is_content_truncated());
    assert!(active.pending_approvals().is_empty());
    assert!(active.known_outcomes().is_empty());
}

#[test]
fn snapshot_known_outcomes_preserve_status_effect_and_evidence() {
    let summaries: Vec<OutcomeSummary> = [
        OutcomeSummary {
            call: CallId::new("call-a").expect("valid call id"),
            status: ExecutionStatus::Succeeded,
            effect: EffectState::KnownApplied,
            evidence: Evidence::HostObserved,
        },
        OutcomeSummary {
            call: CallId::new("call-b").expect("valid call id"),
            status: ExecutionStatus::Failed,
            effect: EffectState::KnownNotApplied,
            evidence: Evidence::PluginReported,
        },
        OutcomeSummary {
            call: CallId::new("call-c").expect("valid call id"),
            status: ExecutionStatus::Denied,
            effect: EffectState::NotStarted,
            evidence: Evidence::HostObserved,
        },
        OutcomeSummary {
            call: CallId::new("call-d").expect("valid call id"),
            status: ExecutionStatus::TimedOut,
            effect: EffectState::Unknown,
            evidence: Evidence::Uncertain,
        },
        OutcomeSummary {
            call: CallId::new("call-e").expect("valid call id"),
            status: ExecutionStatus::Cancelled,
            effect: EffectState::Unknown,
            evidence: Evidence::Uncertain,
        },
    ]
    .to_vec();
    let snapshot = Snapshot::new(
        session(),
        run(),
        Some(2),
        RunLifecycle::Active,
        Vec::new(),
        summaries.clone(),
        false,
    )
    .expect("bounded snapshot builds");
    assert_eq!(snapshot.known_outcomes(), summaries.as_slice());
    let calls: Vec<&str> = snapshot
        .known_outcomes()
        .iter()
        .map(|summary| summary.call.as_str())
        .collect();
    assert_eq!(calls, ["call-a", "call-b", "call-c", "call-d", "call-e"]);
}

#[test]
fn run_event_validation_rechecks_payload_bounds_but_not_sequence_contiguity() {
    let mut fragment = AssistantText::new(
        TurnId::new("turn-cov").expect("valid turn id"),
        "item-0",
        "ok",
    )
    .expect("valid fragment builds");
    fragment.text = "x".repeat(MAX_TEXT_FRAGMENT_BYTES + 1);
    let oversized = RunEvent::new(
        session(),
        run(),
        0,
        EventPayload::AssistantTextDelta(fragment),
    );
    assert!(
        oversized.validate().is_err(),
        "payload bounds are revalidated through the envelope"
    );

    let empty_key = RunEvent::new(
        session(),
        run(),
        1,
        EventPayload::ToolCallPreview {
            item_key: String::new(),
        },
    );
    assert!(
        empty_key.validate().is_err(),
        "empty preview keys are rejected"
    );

    let maximum = RunEvent::new(
        session(),
        run(),
        EventSequence::MAX,
        EventPayload::RunStarted {
            request: request("req-max"),
        },
    );
    assert!(
        maximum.validate().is_ok(),
        "the envelope never claims sequence contiguity; the runtime owns it"
    );
    assert_eq!(checked_next_sequence(maximum.seq()), None);
}
