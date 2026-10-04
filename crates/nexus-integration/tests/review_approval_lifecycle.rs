#![forbid(unsafe_code)]

//! Review regressions: approval-wait lifecycle and snapshot truth.
//!
//! Findings under test:
//! - cancellation while awaiting an approval must finalize promptly without
//!   dispatching the undecided call and without consuming the next turn;
//! - approval expiry must resolve to an explicit denial outcome, never a
//!   stuck run;
//! - an approval already resolved by `Approve` must not appear in the
//!   pending set of a snapshot taken immediately afterwards.
//!
//! Determinism: every wait is bounded by the shared drain backstop; the
//! immediately-decided test relies on the current-thread runtime not
//! scheduling the run driver between two ready awaits.

mod common;

use std::time::Duration;

use nexus_core::{
    ApproveCommand, CancelCommand, CommandReply, EventPayload, ExecutionStatus, GetSnapshotCommand,
    RequestId, RunLifecycle, RunOutcome,
};
use nexus_fakes::{stop_turn, tool_turn};
use nexus_runtime::{Policy, RuntimeConfig};

#[test]
fn cancel_while_awaiting_approval_finalizes_without_dispatch_or_next_turn() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![common::candidate("host_write", r#"{"path":"src"}"#)]),
            stop_turn("next turn must never be requested after cancellation"),
        ];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("approval-cancel"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let approvals = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        common::find_approval(&approvals).expect("approval requested before cancellation");

        let cancel = bed
            .runtime
            .cancel(CancelCommand {
                request: RequestId::new("req-approval-cancel").expect("valid"),
                run: run.clone(),
            })
            .await;
        assert_eq!(cancel.reply(), CommandReply::Accepted);

        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(approvals);

        assert_eq!(
            finished.outcome(),
            RunOutcome::Cancelled,
            "cancellation while awaiting approval ends the run as cancelled"
        );
        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "the undecided mutation never dispatches"
        );
        assert_eq!(
            bed.provider.call_count(),
            1,
            "cancellation stops the loop before another model turn"
        );
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::ToolStarted(_)
            )),
            0,
            "no call started"
        );
        common::assert_single_terminal(&data, &control);

        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-approval-cancel-snap").expect("valid"),
                run,
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("finalized run keeps a snapshot");
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Cancelled)
        );
        assert!(
            snapshot.pending_approvals().is_empty(),
            "abandoned approvals leave the pending set: {:?}",
            snapshot.pending_approvals()
        );
    });
}

#[test]
fn expired_approval_denies_without_dispatch_and_never_deadlocks() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = nexus_core::Limits::m0_test();
        limits.approval_expiry = Duration::from_millis(40);
        let config = RuntimeConfig {
            limits,
            policy: Policy::m0_test(),
            has_approval_handler: true,
        };
        let script = vec![
            tool_turn(vec![common::candidate("host_write", r#"{"path":"src"}"#)]),
            stop_turn("after expiry"),
        ];
        let mut bed = common::make_bed(script, config);
        let response = bed
            .runtime
            .submit(common::submit_cmd("approval-expiry"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let approvals = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let (approval, _) = common::find_approval(&approvals).expect("approval requested");

        // No decision is sent: the expiry path alone must resolve the wait.
        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(approvals);

        assert_eq!(
            finished.outcome(),
            RunOutcome::Completed,
            "expiry denies the call and the run continues honestly"
        );
        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "expired call never runs"
        );
        assert_eq!(bed.provider.call_count(), 2, "the loop continued one turn");
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::ToolStarted(_)
            )),
            0
        );
        let denied: Vec<_> = control
            .iter()
            .filter_map(|event| match event.payload() {
                EventPayload::ToolFinished(info) => Some(info.outcome.status()),
                _ => None,
            })
            .collect();
        assert_eq!(
            denied,
            vec![ExecutionStatus::Denied],
            "expiry records exactly one denial"
        );
        common::assert_single_terminal(&data, &control);

        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-approval-expiry-snap").expect("valid"),
                run,
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("finalized snapshot");
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed)
        );
        assert!(
            !snapshot.pending_approvals().contains(&approval),
            "an expired approval is no longer pending: {:?}",
            snapshot.pending_approvals()
        );
    });
}

#[test]
fn decided_approval_is_absent_from_snapshot_pending_immediately() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![common::candidate("host_write", r#"{"path":"src"}"#)]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("approval-decided"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let approvals = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let (approval, call) =
            common::find_approval(&approvals).expect("approval requested before decision");

        let approve = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-decided").expect("valid"),
                approval: approval.clone(),
                run: run.clone(),
                call,
            })
            .await;
        assert_eq!(approve.reply(), CommandReply::Accepted);

        // The run driver is parked in its approval wait; on the
        // current-thread runtime it cannot be scheduled between ready
        // awaits, so this snapshot observes the state immediately after
        // resolution without any test-side yield.
        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-decided-snap").expect("valid"),
                run: run.clone(),
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("live run has a snapshot");
        assert!(
            !snapshot.pending_approvals().contains(&approval),
            "an approval resolved by Approve is no longer pending; snapshot still lists {:?}",
            snapshot.pending_approvals()
        );

        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(approvals);
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(
            bed.write_tool.execution_count(),
            1,
            "approved call runs once"
        );
        common::assert_single_terminal(&data, &control);

        let (_, final_snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-decided-final").expect("valid"),
                run,
            })
            .await;
        let final_snapshot = final_snapshot.expect("finalized snapshot");
        assert!(
            final_snapshot.pending_approvals().is_empty(),
            "finalized run keeps no pending approvals"
        );
    });
}
