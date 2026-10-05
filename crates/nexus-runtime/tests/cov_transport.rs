//! Coverage hardening for the event transport through the public boundary.
//!
//! Pins the M0 lock-table channel capacities independently of the exported
//! constants, exercises the consume-path staleness check across every
//! [`nexus_core::EventPayload`] variant and both sequence extremes, and proves
//! the declared capacities are the effective bounds of the bounded channels
//! carried by [`EventStreams`].
//!
//! Deterministic: no clock, no randomness, no I/O, no async runtime; the
//! channel-bound checks use only non-blocking `try_send`/`try_recv`.

#![forbid(unsafe_code)]

use std::time::Duration;

use nexus_core::{
    ApprovalId, ApprovalNotice, AssistantText, CallId, EffectState, EventPayload, Evidence,
    ExecutionStatus, Limits, PersistenceState, RequestId, RunEvent, RunFinished, RunId, RunOutcome,
    SessionId, ToolFinishedInfo, ToolOutcome, ToolProgress, ToolStartedInfo, TurnId, Usage,
    UsageFinality,
};
use nexus_runtime::{CONTROL_CAPACITY, DATA_CAPACITY, EventStreams, event_is_live};
use tokio::sync::mpsc;

fn session() -> SessionId {
    SessionId::new("sess-transport").expect("valid session id")
}

fn run(raw: &str) -> RunId {
    RunId::new(raw).expect("valid run id")
}

fn call(raw: &str) -> CallId {
    CallId::new(raw).expect("valid call id")
}

fn event(run: &RunId, seq: u64, payload: EventPayload) -> RunEvent {
    RunEvent::new(session(), run.clone(), seq, payload)
}

/// One valid payload per `EventPayload` variant, in declaration order. The
/// private classification test keeps the same fixture set; a new variant must
/// be added in both places, where an exhaustive match fails compilation.
fn payloads() -> Vec<(&'static str, EventPayload)> {
    let request = RequestId::new("req-transport").expect("valid request id");
    let turn = TurnId::new("turn-transport").expect("valid turn id");
    let notice = ApprovalNotice::new(
        ApprovalId::new("approval-transport").expect("valid approval id"),
        call("call-transport"),
        "run tool host_write",
        "project scope",
        Duration::from_secs(120),
    )
    .expect("safe approval notice builds");
    let outcome = ToolOutcome::new(
        ExecutionStatus::Succeeded,
        EffectState::KnownApplied,
        Evidence::HostObserved,
        "ok",
        false,
    )
    .expect("bounded tool outcome builds");
    vec![
        ("RunStarted", EventPayload::RunStarted { request }),
        (
            "AssistantTextDelta",
            EventPayload::AssistantTextDelta(
                AssistantText::new(turn, "item-0", "delta").expect("valid fragment builds"),
            ),
        ),
        (
            "ToolCallPreview",
            EventPayload::ToolCallPreview {
                item_key: "item-0".to_owned(),
            },
        ),
        ("ApprovalRequired", EventPayload::ApprovalRequired(notice)),
        (
            "ToolStarted",
            EventPayload::ToolStarted(ToolStartedInfo {
                call: call("call-transport"),
            }),
        ),
        (
            "ToolOutput",
            EventPayload::ToolOutput(
                ToolProgress::new(call("call-transport"), "progress", false)
                    .expect("valid progress builds"),
            ),
        ),
        (
            "ToolFinished",
            EventPayload::ToolFinished(ToolFinishedInfo {
                call: call("call-transport"),
                outcome,
            }),
        ),
        (
            "UsageUpdated",
            EventPayload::UsageUpdated(Usage::new(Some(1), Some(2), UsageFinality::Provisional)),
        ),
        (
            "RunFinished",
            EventPayload::RunFinished(
                RunFinished::new(RunOutcome::Completed, PersistenceState::Ephemeral, None)
                    .expect("terminal record builds"),
            ),
        ),
    ]
}

#[test]
fn locked_capacities_match_the_m0_lock_table() {
    // Transcribed from docs/tasks/06-m0-lock.md section 1. The duplication is
    // deliberate: editing a constant without the lock table must fail here
    // instead of passing by comparing the constant with itself.
    assert_eq!(DATA_CAPACITY, 1_024);
    assert_eq!(CONTROL_CAPACITY, 128);

    // The exported constants stay the lock-table budget fields used by the
    // M0-test profile, and that profile still validates.
    assert_eq!(DATA_CAPACITY, Limits::M0_TEST_EVENT_DATA_CAPACITY);
    assert_eq!(CONTROL_CAPACITY, Limits::M0_TEST_EVENT_CONTROL_CAPACITY);
    let limits = Limits::m0_test();
    assert_eq!(limits.event_data_capacity, DATA_CAPACITY);
    assert_eq!(limits.event_control_capacity, CONTROL_CAPACITY);
    limits.validate().expect("M0-test budgets are valid");
}

#[test]
fn event_is_live_matrix_over_every_payload_variant() {
    let live = run("run-live");
    let other = run("run-other");

    for (name, payload) in payloads() {
        // Sequence extremes never change liveness; only the owning run does.
        for seq in [0, u64::MAX] {
            assert!(
                event_is_live(&event(&live, seq, payload.clone()), Some(&live)),
                "{name}: the active run's own event is live"
            );
            assert!(
                !event_is_live(&event(&other, seq, payload.clone()), Some(&live)),
                "{name}: another run's event is stale"
            );
            assert!(
                !event_is_live(&event(&live, seq, payload.clone()), Some(&other)),
                "{name}: the live run is stale against another active run"
            );
            assert!(
                !event_is_live(&event(&live, seq, payload.clone()), None),
                "{name}: no active run rejects every event"
            );
        }
    }
}

#[test]
fn stale_run_updates_are_rejected_regardless_of_sequence_or_payload() {
    let live = run("run-2");
    let stale = run("run-1");

    // A stale run cannot win with a higher sequence, and its control events
    // (approval, terminal) are rejected exactly like presentation traffic.
    let approval = ApprovalNotice::new(
        ApprovalId::new("approval-stale").expect("valid approval id"),
        call("call-stale"),
        "run tool host_write",
        "project scope",
        Duration::from_secs(120),
    )
    .expect("safe approval notice builds");
    let terminal = RunFinished::new(RunOutcome::Completed, PersistenceState::Ephemeral, None)
        .expect("terminal record builds");
    for payload in [
        EventPayload::ApprovalRequired(approval),
        EventPayload::RunFinished(terminal),
    ] {
        assert!(!event_is_live(
            &event(&stale, u64::MAX, payload),
            Some(&live)
        ));
    }

    // The check is value equality on the run identity, not instance identity.
    let same_run = run("run-2");
    let started = EventPayload::RunStarted {
        request: RequestId::new("req-same-run").expect("valid request id"),
    };
    assert!(event_is_live(&event(&same_run, 0, started), Some(&live)));
}

#[test]
fn declared_capacities_are_the_effective_channel_bounds() {
    let (data_tx, data_rx) = mpsc::channel::<RunEvent>(DATA_CAPACITY);
    let (control_tx, control_rx) = mpsc::channel::<RunEvent>(CONTROL_CAPACITY);
    let mut streams = EventStreams {
        data: data_rx,
        control: control_rx,
    };

    // The runtime's receivers carry the declared bounds; the channels are
    // bounded, never unbounded.
    assert_eq!(streams.data.max_capacity(), DATA_CAPACITY);
    assert_eq!(streams.control.max_capacity(), CONTROL_CAPACITY);
    assert_eq!(data_tx.max_capacity(), DATA_CAPACITY);
    assert_eq!(control_tx.max_capacity(), CONTROL_CAPACITY);

    let run = run("run-bounds");
    let started = || EventPayload::RunStarted {
        request: RequestId::new("req-bounds").expect("valid request id"),
    };

    // Exactly the declared capacity is accepted on each channel.
    for seq in 0..DATA_CAPACITY as u64 {
        data_tx
            .try_send(event(&run, seq, started()))
            .expect("data event within the declared capacity");
    }
    for seq in 0..CONTROL_CAPACITY as u64 {
        control_tx
            .try_send(event(&run, seq, started()))
            .expect("control event within the declared capacity");
    }
    assert_eq!(data_tx.capacity(), 0);
    assert_eq!(control_tx.capacity(), 0);

    // One over the bound is rejected explicitly as `Full`, never silently
    // accepted, coalesced, or dropped.
    assert!(matches!(
        data_tx.try_send(event(&run, DATA_CAPACITY as u64, started())),
        Err(mpsc::error::TrySendError::Full(_))
    ));
    assert!(matches!(
        control_tx.try_send(event(&run, CONTROL_CAPACITY as u64, started())),
        Err(mpsc::error::TrySendError::Full(_))
    ));

    // Consuming one event frees exactly one slot on each channel.
    let first = streams.data.try_recv().expect("a buffered data event");
    assert_eq!(first.seq(), 0);
    let first = streams
        .control
        .try_recv()
        .expect("a buffered control event");
    assert_eq!(first.seq(), 0);
    assert_eq!(data_tx.capacity(), 1);
    assert_eq!(control_tx.capacity(), 1);
    data_tx
        .try_send(event(&run, 0, started()))
        .expect("the freed data slot accepts one event");
    control_tx
        .try_send(event(&run, 0, started()))
        .expect("the freed control slot accepts one event");
    assert!(streams.data.try_recv().is_ok());
}
