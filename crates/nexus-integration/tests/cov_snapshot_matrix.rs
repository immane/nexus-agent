#![forbid(unsafe_code)]

//! Bounded-snapshot truth matrix against the real runtime and real fakes.
//!
//! Findings under test:
//! - an active run's snapshot reports the approval it is actually waiting on
//!   together with the tool outcomes already recorded, and names the sequence
//!   of the last event published before the wait;
//! - a finalized run keeps a bounded snapshot whose lifecycle carries the
//!   terminal outcome and whose `last_sequence` names the terminal sequence
//!   itself, with the run's full outcome record and no leftover approvals;
//! - a grant decided by `Approve`/`Deny` leaves the pending set immediately
//!   and never regrows, including while a later approval is outstanding and
//!   after the run finalizes;
//! - a snapshot taken while a call is in flight shows the in-flight call as
//!   last published state and reports no known outcome for it, so an
//!   unrecorded execution is never presented as finished work.
//!
//! Determinism: every wait is bounded by the shared drain backstop; the
//! in-flight case uses the gated tool double so the worker is confirmed
//! blocked at its entry hand-off before the snapshot is read, and the
//! immediately-decided case relies on the current-thread runtime not
//! scheduling the run driver between two ready awaits.

mod common;

use std::time::Duration;

use nexus_core::{
    ApproveCommand, CallId, CommandReply, DenyCommand, EffectState, EventPayload, Evidence,
    ExecutionStatus, GetSnapshotCommand, RequestId, RunEvent, RunId, RunLifecycle, RunOutcome,
    Snapshot,
};
use nexus_fakes::{FakeTool, stop_turn, tool_turn};

/// Longest a gated tool worker is given to reach its entry hand-off before the
/// in-flight snapshot is read. Generous failure backstop only: the worker
/// announces entry before it blocks.
const GATE_ENTRY_TIMEOUT: Duration = Duration::from_secs(5);

/// A read-then-write turn: the automatic read records an outcome, then the
/// mutating call parks the run awaiting an approval grant.
fn read_then_write_turn() -> Vec<nexus_core::ProviderEvent> {
    tool_turn(vec![
        common::candidate_with("item-0", "prov-ref-0", "host_read", r#"{"path":"src"}"#),
        common::candidate_with("item-1", "prov-ref-1", "host_write", r#"{"path":"dst"}"#),
    ])
}

/// Builds a snapshot request bound to `run`.
fn snapshot_command(tag: &str, run: &RunId) -> GetSnapshotCommand {
    GetSnapshotCommand {
        request: RequestId::new(format!("req-{tag}")).expect("valid"),
        run: run.clone(),
    }
}

/// Returns the sequence of the single event in `events` whose payload
/// satisfies `matches`, panicking with the observed events otherwise.
fn seq_of(events: &[RunEvent], matches: impl Fn(&EventPayload) -> bool) -> u64 {
    let mut found = events
        .iter()
        .filter(|event| matches(event.payload()))
        .map(RunEvent::seq);
    let seq = found
        .next()
        .unwrap_or_else(|| panic!("expected event was published: {events:?}"));
    assert!(
        found.next().is_none(),
        "expected exactly one such event: {events:?}"
    );
    seq
}

/// Collects the calls admitted by `ToolStarted`, in publication order.
fn started_calls(events: &[RunEvent]) -> Vec<CallId> {
    events
        .iter()
        .filter_map(|event| match event.payload() {
            EventPayload::ToolStarted(info) => Some(info.call.clone()),
            _ => None,
        })
        .collect()
}

/// Collects `(call, status, effect, evidence)` for every known outcome, so a
/// snapshot's recorded work is asserted as a whole rather than field by field.
fn outcome_record(snapshot: &Snapshot) -> Vec<(String, ExecutionStatus, EffectState, Evidence)> {
    snapshot
        .known_outcomes()
        .iter()
        .map(|summary| {
            (
                summary.call.as_str().to_owned(),
                summary.status,
                summary.effect,
                summary.evidence,
            )
        })
        .collect()
}

/// An active run's snapshot names the outstanding approval, the outcome already
/// recorded for the earlier call, and the last published sequence. The
/// snapshot must not hide the wait behind a bare "active" marker: a consumer
/// that reconnects mid-run has to learn what it is waiting on and what already
/// happened.
#[test]
fn active_snapshot_reports_pending_approval_and_recorded_outcome() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![read_then_write_turn(), stop_turn("done")];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("snapshot-active"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        // The read executes first and the mutating call then parks the run in
        // its approval wait; both facts must be visible in one snapshot.
        let control = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let (approval, write_call) =
            common::find_approval(&control).expect("mutating call awaits approval");
        let calls = started_calls(&control);
        assert_eq!(
            calls.len(),
            1,
            "only the automatic read started before the approval wait: {calls:?}"
        );
        let read_call = calls[0].clone();

        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(snapshot_command("active", &run))
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        assert_eq!(reply.run(), Some(&run), "the snapshot names its own run");
        let snapshot = snapshot.expect("a live run keeps a snapshot");
        assert_eq!(snapshot.run(), &run);
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Active,
            "a run waiting on an approval has not finalized"
        );
        assert_eq!(
            snapshot.pending_approvals(),
            std::slice::from_ref(&approval),
            "the snapshot names the approval the run is actually waiting on"
        );
        assert_eq!(
            outcome_record(&snapshot),
            vec![(
                read_call.as_str().to_owned(),
                ExecutionStatus::Succeeded,
                EffectState::KnownNotApplied,
                Evidence::HostObserved,
            )],
            "the read outcome is already known; the unapproved write has none"
        );
        assert_eq!(
            snapshot.last_sequence(),
            Some(seq_of(&control, |payload| matches!(
                payload,
                EventPayload::ApprovalRequired(_)
            ))),
            "the snapshot names the last event published before the wait"
        );
        assert!(
            !snapshot.is_content_truncated(),
            "a drained-channel run reports no truncation"
        );

        // Release the wait so the runtime finalizes and the run slot frees.
        let approve = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-snap-active-grant").expect("valid"),
                approval,
                run,
                call: write_call,
            })
            .await;
        assert_eq!(approve.reply(), CommandReply::Accepted);
        let (_, _, finished) = common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
    });
}

/// A finalized run keeps a bounded snapshot: the lifecycle carries the
/// terminal outcome, `last_sequence` is the terminal sequence itself (never a
/// stale earlier event), the whole run's outcome record survives, and no
/// approval outlives the run.
#[test]
fn finalized_snapshot_names_terminal_sequence_and_keeps_outcomes() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![read_then_write_turn(), stop_turn("done")];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("snapshot-final"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let mut control = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let (approval, write_call) =
            common::find_approval(&control).expect("mutating call awaits approval");
        let approve = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-snap-final-grant").expect("valid"),
                approval,
                run: run.clone(),
                call: write_call.clone(),
            })
            .await;
        assert_eq!(approve.reply(), CommandReply::Accepted);

        let (data, mut rest, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.append(&mut rest);
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        common::assert_single_terminal(&data, &control);
        common::assert_contiguous(&data, &control);

        let calls = started_calls(&control);
        assert_eq!(
            calls.len(),
            2,
            "the approved mutation dispatched after the read: {calls:?}"
        );
        let terminal = control
            .iter()
            .find(|event| event.is_terminal())
            .expect("terminal observed");

        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(snapshot_command("final", &run))
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("a finalized run keeps a snapshot");
        assert_eq!(snapshot.run(), &run);
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed),
            "the finalized lifecycle carries the terminal outcome"
        );
        assert_eq!(
            snapshot.last_sequence(),
            Some(terminal.seq()),
            "the finalized snapshot names the terminal sequence"
        );
        let highest = data
            .iter()
            .chain(control.iter())
            .map(RunEvent::seq)
            .max()
            .expect("the run published events");
        assert_eq!(
            snapshot.last_sequence(),
            Some(highest),
            "the terminal sequence is the highest published one"
        );
        assert!(
            snapshot.pending_approvals().is_empty(),
            "a finalized run keeps no pending approvals: {:?}",
            snapshot.pending_approvals()
        );
        assert_eq!(
            outcome_record(&snapshot),
            vec![
                (
                    calls[0].as_str().to_owned(),
                    ExecutionStatus::Succeeded,
                    EffectState::KnownNotApplied,
                    Evidence::HostObserved,
                ),
                (
                    write_call.as_str().to_owned(),
                    ExecutionStatus::Succeeded,
                    EffectState::KnownApplied,
                    Evidence::HostObserved,
                ),
            ],
            "the finalized snapshot keeps every recorded outcome in call order"
        );
        assert!(
            !snapshot.is_content_truncated(),
            "an untruncated run does not claim truncation"
        );

        // A repeat read of the retired run is stable, never a live view again.
        let (_, again) = bed
            .runtime
            .get_snapshot(snapshot_command("final-again", &run))
            .await;
        let again = again.expect("snapshot persists");
        assert_eq!(again.lifecycle(), snapshot.lifecycle());
        assert_eq!(again.last_sequence(), snapshot.last_sequence());
        assert_eq!(
            outcome_record(&again),
            outcome_record(&snapshot),
            "a repeat read never gains or loses recorded outcomes"
        );
    });
}

/// A decided grant leaves the pending set immediately and never regrows: the
/// resolved approval is absent from the very next snapshot, it stays absent
/// while a later approval is outstanding, and it is still absent after the run
/// finalizes. Re-deciding the same grant is rejected instead of re-entering.
#[test]
fn decided_grants_leave_the_pending_set_immediately_and_never_regrow() {
    let rt = common::test_rt();
    rt.block_on(async {
        // Two mutating calls: the first is approved, the second refused.
        let script = vec![
            tool_turn(vec![
                common::candidate_with("item-0", "prov-ref-0", "host_write", r#"{"path":"a"}"#),
                common::candidate_with("item-1", "prov-ref-1", "host_write", r#"{"path":"b"}"#),
            ]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("snapshot-decided"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let first_events = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let (first_approval, first_call) =
            common::find_approval(&first_events).expect("first approval requested");
        assert_eq!(
            first_events
                .iter()
                .filter(|event| matches!(event.payload(), EventPayload::ApprovalRequired(_)))
                .count(),
            1,
            "one queued mutation is presented for approval at a time"
        );

        let approve = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-snap-decided-grant").expect("valid"),
                approval: first_approval.clone(),
                run: run.clone(),
                call: first_call.clone(),
            })
            .await;
        assert_eq!(approve.reply(), CommandReply::Accepted);

        // The run driver is parked in its approval wait, so on the
        // current-thread runtime it cannot be scheduled between two ready
        // awaits: this snapshot observes the state immediately after the
        // decision, with no test-side yield.
        let (reply, decided) = bed
            .runtime
            .get_snapshot(snapshot_command("decided", &run))
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let decided = decided.expect("live run has a snapshot");
        assert_eq!(decided.lifecycle(), RunLifecycle::Active);
        assert!(
            decided.pending_approvals().is_empty(),
            "a decided grant leaves the pending set immediately; snapshot still lists {:?}",
            decided.pending_approvals()
        );
        assert!(
            !outcome_record(&decided)
                .iter()
                .any(|(call, ..)| call == first_call.as_str()),
            "a granted-but-unexecuted call has no known outcome yet"
        );

        // A repeat decision for the same grant is refused, never re-queued.
        let repeat = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-snap-decided-repeat").expect("valid"),
                approval: first_approval.clone(),
                run: run.clone(),
                call: first_call.clone(),
            })
            .await;
        assert_eq!(
            repeat.reply(),
            CommandReply::StaleOrUnknownTarget,
            "a decided grant cannot be decided twice"
        );

        // The next queued mutation is presented on its own; the resolved
        // approval must not reappear beside it.
        let second_events = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let (second_approval, second_call) =
            common::find_approval(&second_events).expect("second approval requested");
        assert_ne!(second_approval, first_approval, "distinct queued calls");
        assert_ne!(second_call, first_call, "distinct queued calls");
        let (_, later) = bed
            .runtime
            .get_snapshot(snapshot_command("decided-later", &run))
            .await;
        let later = later.expect("live run has a snapshot");
        assert_eq!(
            later.pending_approvals(),
            std::slice::from_ref(&second_approval),
            "only the outstanding approval is pending: {:?}",
            later.pending_approvals()
        );
        assert!(
            !later.pending_approvals().contains(&first_approval),
            "the resolved approval never regrows while another is pending"
        );
        assert_eq!(
            outcome_record(&later),
            vec![(
                first_call.as_str().to_owned(),
                ExecutionStatus::Succeeded,
                EffectState::KnownApplied,
                Evidence::HostObserved,
            )],
            "the granted call is recorded once it executes"
        );

        let deny = bed
            .runtime
            .deny(DenyCommand {
                request: RequestId::new("req-snap-decided-deny").expect("valid"),
                approval: second_approval.clone(),
                run: run.clone(),
                call: second_call.clone(),
            })
            .await;
        assert_eq!(deny.reply(), CommandReply::Accepted);

        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(first_events);
        control.extend(second_events);
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(bed.write_tool.execution_count(), 1, "only the grant runs");
        common::assert_single_terminal(&data, &control);

        let (_, settled) = bed
            .runtime
            .get_snapshot(snapshot_command("decided-final", &run))
            .await;
        let settled = settled.expect("finalized run keeps a snapshot");
        assert_eq!(
            settled.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed)
        );
        assert!(
            settled.pending_approvals().is_empty(),
            "neither decided grant survives into the finalized snapshot: {:?}",
            settled.pending_approvals()
        );
        assert_eq!(
            outcome_record(&settled),
            vec![
                (
                    first_call.as_str().to_owned(),
                    ExecutionStatus::Succeeded,
                    EffectState::KnownApplied,
                    Evidence::HostObserved,
                ),
                (
                    second_call.as_str().to_owned(),
                    ExecutionStatus::Denied,
                    EffectState::NotStarted,
                    Evidence::HostObserved,
                ),
            ],
            "the refusal is recorded as an explicit denial without effects"
        );
    });
}

/// A snapshot taken while a call is in flight names that call as the last
/// published state and reports no outcome for it. An unrecorded execution must
/// never be presented as finished work, and the in-flight identity must be
/// observable so a reconnecting consumer can tell "running" from "done".
#[test]
fn snapshot_during_tool_execution_shows_in_flight_call_without_outcome() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![common::candidate("host_read", r#"{"path":"src"}"#)]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed_with_tools(
            script,
            common::quick_config(),
            // The worker announces entry and then blocks at its gate, so the
            // snapshot is read while the execution provably owns the tool.
            FakeTool::gated("host_read"),
            FakeTool::mutation(),
        );
        let response = bed
            .runtime
            .submit(common::submit_cmd("snapshot-inflight"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let prelude = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ToolStarted(_))
        })
        .await;
        let inflight = started_calls(&prelude);
        assert_eq!(
            inflight.len(),
            1,
            "the automatic read entered execution: {inflight:?}"
        );
        let inflight_call = inflight[0].clone();

        // `ToolStarted` is published before the worker is spawned, so wait
        // until the worker is confirmed blocked inside the execution.
        let gate = bed.read_tool.gate().expect("gated tool exposes its gate");
        tokio::time::timeout(GATE_ENTRY_TIMEOUT, async {
            while !gate.is_entered() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("the blocked worker reached its execution");
        let log = bed.read_tool.log();
        assert_eq!(log.len(), 1, "the worker entered before the snapshot");
        assert_eq!(
            log[0].call, inflight_call,
            "the in-flight call is the one the host executed"
        );

        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(snapshot_command("inflight", &run))
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("a running run keeps a snapshot");
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Active,
            "an executing call is not a finalized run"
        );
        assert_eq!(
            snapshot.last_sequence(),
            Some(seq_of(&prelude, |payload| matches!(
                payload,
                EventPayload::ToolStarted(_)
            ))),
            "the snapshot names the in-flight call's start as last published state"
        );
        assert!(
            snapshot.pending_approvals().is_empty(),
            "an automatic call needs no approval, so none is pending: {:?}",
            snapshot.pending_approvals()
        );
        assert!(
            snapshot.known_outcomes().is_empty(),
            "an in-flight call has no recorded outcome yet: {:?}",
            outcome_record(&snapshot)
        );
        assert!(
            !snapshot.is_content_truncated(),
            "a drained-channel run reports no truncation"
        );

        // Releasing the worker completes the call and the same identity then
        // appears in the outcome record.
        gate.release();
        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(prelude);
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(bed.read_tool.execution_count(), 1, "executed once");
        common::assert_single_terminal(&data, &control);

        let (_, settled) = bed
            .runtime
            .get_snapshot(snapshot_command("inflight-final", &run))
            .await;
        let settled = settled.expect("finalized run keeps a snapshot");
        assert_eq!(
            settled.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed)
        );
        assert_eq!(
            outcome_record(&settled),
            vec![(
                inflight_call.as_str().to_owned(),
                ExecutionStatus::Succeeded,
                EffectState::KnownNotApplied,
                Evidence::HostObserved,
            )],
            "the in-flight identity is recorded once, after the worker returned"
        );
    });
}
