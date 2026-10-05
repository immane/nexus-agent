#![forbid(unsafe_code)]

//! Approval-expiry race hardening against the real runtime and the real
//! fakes.
//!
//! Findings under test:
//! - an approval nobody answers resolves itself: one denial per approval, no
//!   dispatch, and no stuck run (the shared bounded drain is the deadlock
//!   witness, so a parked approval wait fails the test instead of hanging);
//! - the effective expiry is the minimum of the approval lifetime and the run
//!   budget: a shorter lifetime decides the wait as a denial and the run keeps
//!   going, while a lifetime beyond the run budget is clamped to the run
//!   budget and the run deadline decides with an explicit limit outcome;
//! - a late grant for an already-expired approval is refused and never
//!   dispatches: `AlreadyFinalized` for the run that expired it, and
//!   `StaleOrUnknownTarget` for an identity the live run never issued;
//! - the pending approval set is empty in the snapshot of an expired or
//!   finalized run, and a rejected grant leaves a live pending set untouched.
//!
//! Determinism: no assertion reads wall-clock time. Every wait is bounded by
//! a generous backstop, and every passing assertion is on a recorded
//! invariant: outcome `(status, effect, evidence)` triples, tool dispatch
//! counters, provider turn count, command reply variants, and snapshot state.
//! Approval lifetimes and run budgets are short so a genuine failure is fast.

mod common;

use std::time::Duration;

use nexus_core::{
    ApprovalId, ApproveCommand, CallId, CommandReply, DenyCommand, EffectState, EventPayload,
    Evidence, ExecutionStatus, GetSnapshotCommand, Limits, RequestId, RunEvent, RunLifecycle,
    RunOutcome,
};
use nexus_fakes::{stop_turn, tool_turn};
use nexus_runtime::{Policy, RuntimeConfig};

/// Approval lifetime for the expiry-driven cases. Short enough to keep a
/// failure fast, long enough that the model turn and command round-trips
/// around it are not racing the expiry they are setting up.
const SHORT_EXPIRY: Duration = Duration::from_millis(40);

/// Approval lifetime longer than the run budget in the clamped case, so the
/// run deadline is the only thing that can decide the wait.
const BEYOND_RUN_EXPIRY: Duration = Duration::from_secs(30);

/// Run budget for the clamped case. Deliberately far larger than the work the
/// fake provider needs and far smaller than [`BEYOND_RUN_EXPIRY`].
const SHORT_RUN_BUDGET: Duration = Duration::from_millis(1500);

/// Approval lifetime wide enough that the two command round-trips and the
/// snapshot of the cross-run stale case finish inside the window they
/// observe, without depending on how fast the machine is.
const WIDE_EXPIRY: Duration = Duration::from_millis(150);

/// Outcome triple a caller observes for one finished call.
type Outcome = (ExecutionStatus, EffectState, Evidence);

/// The only triple reachable when a registered tool is gated by an unanswered
/// approval: the expiry branch denies with host-observed, not-started effects.
/// A dispatched call would report success or failure instead, so this triple
/// also proves nothing ran.
const EXPIRED_DENIAL: Outcome = (
    ExecutionStatus::Denied,
    EffectState::NotStarted,
    Evidence::HostObserved,
);

/// The deadline branch: the call never started, so its effects stay unknown
/// and the run ends on its budget rather than on the approval lifetime.
const AWAIT_DEADLINE: Outcome = (
    ExecutionStatus::TimedOut,
    EffectState::Unknown,
    Evidence::Uncertain,
);

/// M0-test wiring with an approval handler and the given budgets.
fn config_with(limits: Limits) -> RuntimeConfig {
    RuntimeConfig {
        limits,
        policy: Policy::m0_test(),
        has_approval_handler: true,
    }
}

/// M0-test budgets with only the approval lifetime changed.
fn expiring_limits(expiry: Duration) -> Limits {
    let mut limits = Limits::m0_test();
    limits.approval_expiry = expiry;
    limits
}

/// Collects `(status, effect, evidence)` for every `ToolFinished` in order.
fn finished_outcomes(data: &[RunEvent], control: &[RunEvent]) -> Vec<Outcome> {
    data.iter()
        .chain(control.iter())
        .filter_map(|event| match event.payload() {
            EventPayload::ToolFinished(info) => Some((
                info.outcome.status(),
                info.outcome.effect(),
                info.outcome.evidence(),
            )),
            _ => None,
        })
        .collect()
}

/// The monotonic expiry reading published with the first approval notice.
fn expiry_reading(events: &[RunEvent]) -> Duration {
    events
        .iter()
        .find_map(|event| match event.payload() {
            EventPayload::ApprovalRequired(notice) => Some(notice.expires_at_elapsed),
            _ => None,
        })
        .expect("an approval notice carries a monotonic expiry reading")
}

/// Asserts the control channel publishes nothing further inside a bounded
/// window. The runtime keeps its sender for the lifetime of the bed, so a
/// finalized run simply never publishes again; this is a bounded quiescence
/// check on top of the recorded invariants, not a claim about all time.
async fn assert_control_quiet(control: &mut tokio::sync::mpsc::Receiver<RunEvent>) {
    match tokio::time::timeout(Duration::from_millis(50), control.recv()).await {
        Ok(Some(event)) => panic!("a finalized run published another control event: {event:?}"),
        Ok(None) => panic!("the control channel closed instead of staying quiet"),
        Err(_elapsed) => {}
    }
}

/// A denial-free dispatch check shared by the expiry cases.
fn assert_nothing_dispatched(bed: &common::Bed, data: &[RunEvent], control: &[RunEvent]) {
    assert_eq!(
        common::count_payload(data, control, |payload| matches!(
            payload,
            EventPayload::ToolStarted(_)
        )),
        0,
        "an expired approval never starts a call"
    );
    assert_eq!(
        bed.write_tool.execution_count(),
        0,
        "no mutation dispatched"
    );
    assert_eq!(bed.read_tool.execution_count(), 0, "no tool dispatched");
}

/// Two unanswered approvals in one turn both resolve themselves: each call
/// gets its own approval identity and its own denial, the mutation never
/// dispatches, the run finishes on the next model turn, and the finalized
/// snapshot keeps no pending approval. A parked approval wait cannot pass:
/// the drain is bounded.
#[test]
fn unanswered_approvals_expire_into_denials_without_dispatch_or_deadlock() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![
                common::candidate_with("item-0", "prov-ref-0", "host_write", r#"{"path":"a"}"#),
                common::candidate_with("item-1", "prov-ref-1", "host_write", r#"{"path":"b"}"#),
            ]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, config_with(expiring_limits(SHORT_EXPIRY)));
        let response = bed.runtime.submit(common::submit_cmd("expiry-many")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        // No decision is ever sent: the expiry path alone must resolve every
        // approval of this turn.
        let notices = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        common::find_approval(&notices).expect("first approval requested");
        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(notices);

        let approvals = common::all_approvals(&control);
        assert_eq!(
            approvals.len(),
            2,
            "both queued calls asked for approval: {control:?}"
        );
        assert_ne!(
            approvals[0].0, approvals[1].0,
            "each call carries its own approval identity"
        );
        assert_ne!(
            approvals[0].1, approvals[1].1,
            "each approval binds its own call"
        );

        assert_eq!(
            finished.outcome(),
            RunOutcome::Completed,
            "both expiry denials let the run finish honestly"
        );
        assert_eq!(
            finished_outcomes(&data, &control),
            vec![EXPIRED_DENIAL, EXPIRED_DENIAL],
            "each expired approval records exactly one denial"
        );
        assert_nothing_dispatched(&bed, &data, &control);
        assert_eq!(
            bed.provider.call_count(),
            2,
            "the loop continued exactly one turn after both denials"
        );
        common::assert_single_terminal(&data, &control);

        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-expiry-many-snap").expect("valid"),
                run,
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("finalized run keeps a snapshot");
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed)
        );
        assert!(
            snapshot.pending_approvals().is_empty(),
            "expired approvals leave the pending set: {:?}",
            snapshot.pending_approvals()
        );
    });
}

/// With an approval lifetime shorter than the run budget the approval
/// deadline decides the wait: the notice keeps the uncapped lifetime, the
/// call is denied, the run is not truncated by its own budget, and the
/// finalized snapshot keeps no pending approval.
#[test]
fn expiry_shorter_than_run_budget_denies_and_lets_the_run_complete() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = expiring_limits(SHORT_EXPIRY);
        // Comfortably longer than the approval lifetime, so only the
        // approval deadline can explain the denial.
        limits.run_duration = Duration::from_secs(5);
        let script = vec![
            tool_turn(vec![common::candidate("host_write", r#"{"path":"src"}"#)]),
            stop_turn("after expiry"),
        ];
        let mut bed = common::make_bed(script, config_with(limits));
        let response = bed.runtime.submit(common::submit_cmd("expiry-short")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let notices = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let expiry = expiry_reading(&notices);
        assert!(
            expiry >= SHORT_EXPIRY,
            "the notice reading is the lifetime added to the request instant: {expiry:?}"
        );
        assert!(
            expiry < limits.run_duration,
            "a shorter approval lifetime is not clamped by the run budget: {expiry:?}"
        );

        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(notices);

        assert_eq!(
            finished.outcome(),
            RunOutcome::Completed,
            "the approval deadline denied the call; the run budget never expired"
        );
        assert_eq!(
            finished_outcomes(&data, &control),
            vec![EXPIRED_DENIAL],
            "the expiry branch records exactly one denial"
        );
        assert_nothing_dispatched(&bed, &data, &control);
        assert_eq!(
            bed.provider.call_count(),
            2,
            "the loop continued one turn past the denial"
        );
        common::assert_single_terminal(&data, &control);

        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-expiry-short-snap").expect("valid"),
                run,
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("finalized run keeps a snapshot");
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed)
        );
        assert!(
            snapshot.pending_approvals().is_empty(),
            "the expired approval left the pending set: {:?}",
            snapshot.pending_approvals()
        );
    });
}

/// The mirror image: an approval lifetime longer than the run budget is
/// clamped to the run budget in the published notice, and the run deadline
/// decides the wait with an explicit limit outcome instead of a denial. The
/// call still never dispatches, the run never asks for another turn, the
/// pending set is empty, and a late grant for the abandoned approval is
/// refused.
#[test]
fn expiry_beyond_run_budget_is_clamped_and_the_run_deadline_decides() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = expiring_limits(BEYOND_RUN_EXPIRY);
        limits.run_duration = SHORT_RUN_BUDGET;
        let script = vec![
            tool_turn(vec![common::candidate("host_write", r#"{"path":"src"}"#)]),
            stop_turn("must never be requested"),
        ];
        let mut bed = common::make_bed(script, config_with(limits));
        let response = bed
            .runtime
            .submit(common::submit_cmd("expiry-clamped"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let notices = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let (approval, call) = common::find_approval(&notices).expect("approval requested");
        assert_eq!(
            expiry_reading(&notices),
            limits.run_duration,
            "a lifetime beyond the run budget is clamped to the run budget"
        );

        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(notices);

        assert_eq!(
            finished.outcome(),
            RunOutcome::LimitReached,
            "the run deadline, not the approval lifetime, decided this wait"
        );
        assert_eq!(
            finished_outcomes(&data, &control),
            vec![AWAIT_DEADLINE],
            "the deadline branch records one uncertain, never-started outcome"
        );
        assert_nothing_dispatched(&bed, &data, &control);
        assert_eq!(
            bed.provider.call_count(),
            1,
            "the run ends at its deadline without another model turn"
        );
        common::assert_single_terminal(&data, &control);

        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-expiry-clamped-snap").expect("valid"),
                run: run.clone(),
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("finalized run keeps a snapshot");
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::LimitReached)
        );
        assert!(
            snapshot.pending_approvals().is_empty(),
            "the abandoned approval left the pending set: {:?}",
            snapshot.pending_approvals()
        );

        let late = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-expiry-clamped-late").expect("valid"),
                approval,
                run: run.clone(),
                call,
            })
            .await;
        assert_eq!(
            late.reply(),
            CommandReply::AlreadyFinalized,
            "a grant for the abandoned approval cannot revive a finalized run"
        );
        assert_eq!(late.run(), Some(&run));
        assert_eq!(bed.write_tool.execution_count(), 0);
        assert_control_quiet(&mut bed.control).await;
    });
}

/// A grant that arrives after its approval expired can never resurrect the
/// call: both the approve and the deny are refused as already finalized, the
/// mutation count stays at zero, and nothing else is published afterwards.
#[test]
fn late_grant_after_expiry_is_already_finalized_and_never_dispatches() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![common::candidate("host_write", r#"{"path":"src"}"#)]),
            stop_turn("after expiry"),
        ];
        let mut bed = common::make_bed(script, config_with(expiring_limits(SHORT_EXPIRY)));
        let response = bed
            .runtime
            .submit(common::submit_cmd("expiry-late-grant"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let notices = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let (approval, call) = common::find_approval(&notices).expect("approval requested");

        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(notices);
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(finished_outcomes(&data, &control), vec![EXPIRED_DENIAL]);
        assert_nothing_dispatched(&bed, &data, &control);
        common::assert_single_terminal(&data, &control);

        // The run is finalized by the time the terminal event is observable,
        // so these late decisions can only be refused, never accepted.
        let late_approve = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-late-approve").expect("valid"),
                approval: approval.clone(),
                run: run.clone(),
                call: call.clone(),
            })
            .await;
        assert_eq!(late_approve.reply(), CommandReply::AlreadyFinalized);
        assert_eq!(late_approve.run(), Some(&run));

        let late_deny = bed
            .runtime
            .deny(DenyCommand {
                request: RequestId::new("req-late-deny").expect("valid"),
                approval,
                run: run.clone(),
                call,
            })
            .await;
        assert_eq!(
            late_deny.reply(),
            CommandReply::AlreadyFinalized,
            "a late refusal of an expired approval is equally stale"
        );
        assert_eq!(late_deny.run(), Some(&run));

        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "a refused late grant never dispatches"
        );
        assert_control_quiet(&mut bed.control).await;
    });
}

/// An approval that expired in a finished run stays unusable in the next run:
/// replaying it while the new run awaits its own approval is refused, an
/// identity the live run never issued is stale rather than finalized, and
/// neither attempt disturbs the live pending set or dispatches anything. The
/// live approval then resolves on its own expiry like any other.
#[test]
fn expired_approval_from_a_finished_run_is_stale_in_the_next_run() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![common::candidate("host_write", r#"{"path":"first"}"#)]),
            stop_turn("first run done"),
            tool_turn(vec![common::candidate(
                "host_write",
                r#"{"path":"second"}"#,
            )]),
            stop_turn("second run done"),
        ];
        let mut bed = common::make_bed(script, config_with(expiring_limits(WIDE_EXPIRY)));
        let first = bed
            .runtime
            .submit(common::submit_cmd("expiry-stale-first"))
            .await;
        assert_eq!(first.reply(), CommandReply::Accepted);
        let first_run = first.run().cloned().expect("first run issued");

        let first_notices = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let (expired_approval, expired_call) =
            common::find_approval(&first_notices).expect("first run asked for approval");
        let (first_data, mut first_control, first_finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        first_control.extend(first_notices);
        assert_eq!(first_finished.outcome(), RunOutcome::Completed);
        assert_eq!(
            finished_outcomes(&first_data, &first_control),
            vec![EXPIRED_DENIAL]
        );
        assert_nothing_dispatched(&bed, &first_data, &first_control);
        common::assert_single_terminal(&first_data, &first_control);

        // The finished run released its slot, so a second run is admissible.
        let second = bed
            .runtime
            .submit(common::submit_cmd("expiry-stale-second"))
            .await;
        assert_eq!(second.reply(), CommandReply::Accepted);
        let second_run = second.run().cloned().expect("second run issued");
        assert_ne!(
            second_run, first_run,
            "each submit issues its own run identity"
        );

        let second_notices = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let (live_approval, live_call) =
            common::find_approval(&second_notices).expect("second run asked for approval");
        assert_ne!(
            live_approval, expired_approval,
            "the live run issues a fresh approval identity"
        );
        assert_ne!(live_call, expired_call);

        // Replaying the expired grant from the finished run: rejected, and no
        // dispatch reaches the mutation tool.
        let replay = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-replay-expired").expect("valid"),
                approval: expired_approval,
                run: first_run.clone(),
                call: expired_call,
            })
            .await;
        assert_eq!(
            replay.reply(),
            CommandReply::AlreadyFinalized,
            "the run that expired this approval is finished"
        );
        // An identity the live run never issued is stale, not finalized: the
        // two rejection reasons stay distinguishable.
        let never_issued = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-never-issued").expect("valid"),
                approval: ApprovalId::new("a0-never-issued").expect("valid"),
                run: second_run.clone(),
                call: CallId::new("c0-never-issued").expect("valid"),
            })
            .await;
        assert_eq!(never_issued.reply(), CommandReply::StaleOrUnknownTarget);
        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "no rejected grant dispatches"
        );

        let (reply, live_snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-live-snap").expect("valid"),
                run: second_run.clone(),
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let live_snapshot = live_snapshot.expect("live run has a snapshot");
        assert_eq!(live_snapshot.lifecycle(), RunLifecycle::Active);
        assert_eq!(
            live_snapshot.pending_approvals(),
            std::slice::from_ref(&live_approval),
            "the rejected grants left the live pending set untouched"
        );

        let (second_data, mut second_control, second_finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        second_control.extend(second_notices);
        assert_eq!(
            second_finished.outcome(),
            RunOutcome::Completed,
            "the live approval resolves on its own expiry like any other"
        );
        assert_eq!(
            finished_outcomes(&second_data, &second_control),
            vec![EXPIRED_DENIAL]
        );
        assert_nothing_dispatched(&bed, &second_data, &second_control);
        assert_eq!(
            bed.provider.call_count(),
            4,
            "each run took exactly two model turns"
        );
        common::assert_single_terminal(&second_data, &second_control);

        let (reply, final_snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-second-final-snap").expect("valid"),
                run: second_run,
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let final_snapshot = final_snapshot.expect("finalized run keeps a snapshot");
        assert_eq!(
            final_snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed)
        );
        assert!(
            final_snapshot.pending_approvals().is_empty(),
            "the expired approval left the pending set: {:?}",
            final_snapshot.pending_approvals()
        );
    });
}
