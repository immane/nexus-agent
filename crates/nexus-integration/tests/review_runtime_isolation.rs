#![forbid(unsafe_code)]

//! Review regressions: identity scoping across two runtime instances.
//!
//! Findings under test:
//! - identifiers are scoped to their runtime incarnation: a fresh runtime
//!   must not alias an earlier instance's `RunId`/`CallId`/`ApprovalId`
//!   values, and an identifier from one instance must be unknown to the
//!   other, never mistaken for its own finalized or live run;
//! - distinct identifiers across two live runtimes must not cross-dispatch:
//!   approving on one instance never executes the other's pending call, and
//!   each instance finalizes exactly once.

mod common;

use nexus_core::{
    ApproveCommand, CancelCommand, CommandReply, EventPayload, GetSnapshotCommand, RequestId,
    RunLifecycle, RunOutcome,
};
use nexus_fakes::{stop_turn, tool_turn};
use nexus_runtime::event_is_live;

fn mutation_script() -> Vec<Vec<nexus_core::ProviderEvent>> {
    vec![
        tool_turn(vec![common::candidate("host_write", r#"{"path":"src"}"#)]),
        stop_turn("done"),
    ]
}

#[test]
fn ids_from_one_runtime_are_unknown_to_another_instance() {
    let rt = common::test_rt();
    rt.block_on(async {
        // Runtime A completes its first run.
        let mut bed_a = common::make_bed(vec![stop_turn("a")], common::quick_config());
        let response_a = bed_a.runtime.submit(common::submit_cmd("iso-a")).await;
        let run_a = response_a.run().cloned().expect("run issued");
        assert_ne!(
            run_a.as_str(),
            "run-1",
            "run identifiers are incarnation-scoped, never a bare per-instance counter"
        );
        let (events_a, _, finished_a) =
            common::drain_until_finished(&mut bed_a.data, &mut bed_a.control).await;
        assert_eq!(finished_a.outcome(), RunOutcome::Completed);
        assert!(
            events_a
                .iter()
                .all(|event| event_is_live(event, Some(&run_a))),
            "A's events are live for A's run"
        );

        // Fresh runtime B has never seen A's run.
        let mut bed_b = common::make_bed(vec![stop_turn("b")], common::quick_config());
        let unknown_cancel = bed_b
            .runtime
            .cancel(CancelCommand {
                request: RequestId::new("req-iso-unknown-cancel").expect("valid"),
                run: run_a.clone(),
            })
            .await;
        assert_eq!(
            unknown_cancel.reply(),
            CommandReply::StaleOrUnknownTarget,
            "another instance's finalized run is unknown, not already finalized"
        );
        let (unknown_snapshot, snapshot) = bed_b
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-iso-unknown-snap").expect("valid"),
                run: run_a.clone(),
            })
            .await;
        assert_eq!(unknown_snapshot.reply(), CommandReply::StaleOrUnknownTarget);
        assert!(snapshot.is_none(), "no snapshot leaks across instances");
        let unknown_approve = bed_b
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-iso-unknown-approve").expect("valid"),
                approval: nexus_core::ApprovalId::new("a1-0").expect("valid"),
                run: run_a.clone(),
                call: nexus_core::CallId::new("c1-0").expect("valid"),
            })
            .await;
        assert_eq!(
            unknown_approve.reply(),
            CommandReply::StaleOrUnknownTarget,
            "another instance's grant identity is unknown and dispatches nothing"
        );
        assert_eq!(
            bed_b.write_tool.execution_count(),
            0,
            "cross-instance commands never dispatch"
        );

        // B's own run carries a distinct incarnation-scoped identity.
        let response_b = bed_b.runtime.submit(common::submit_cmd("iso-b")).await;
        assert_eq!(response_b.reply(), CommandReply::Accepted);
        let run_b = response_b.run().cloned().expect("run issued");
        assert_ne!(
            run_a.as_str(),
            run_b.as_str(),
            "fresh runtimes must not alias run identifiers"
        );
        assert_ne!(run_b.as_str(), "run-1");
        assert!(
            events_a
                .iter()
                .all(|event| !event_is_live(event, Some(&run_b))),
            "A's events are stale for B's run"
        );
        let (_, _, finished_b) =
            common::drain_until_finished(&mut bed_b.data, &mut bed_b.control).await;
        assert_eq!(finished_b.outcome(), RunOutcome::Completed);

        // A's late command still resolves against A, not B.
        let late = bed_a
            .runtime
            .cancel(CancelCommand {
                request: RequestId::new("req-iso-late").expect("valid"),
                run: run_a,
            })
            .await;
        assert_eq!(late.reply(), CommandReply::AlreadyFinalized);
    });
}

#[test]
fn distinct_instance_ids_do_not_cross_dispatch() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut bed_a = common::make_bed(mutation_script(), common::quick_config());
        let mut bed_b = common::make_bed(mutation_script(), common::quick_config());

        let response_a = bed_a.runtime.submit(common::submit_cmd("live-a")).await;
        let response_b = bed_b.runtime.submit(common::submit_cmd("live-b")).await;
        assert_eq!(response_a.reply(), CommandReply::Accepted);
        assert_eq!(response_b.reply(), CommandReply::Accepted);
        let run_a = response_a.run().cloned().expect("run issued");
        let run_b = response_b.run().cloned().expect("run issued");
        assert_ne!(run_a.as_str(), run_b.as_str(), "distinct run identities");

        let approvals_a = common::collect_control_until(&mut bed_a.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let approvals_b = common::collect_control_until(&mut bed_b.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let (approval_a, call_a) =
            common::find_approval(&approvals_a).expect("A requested approval");
        let (approval_b, call_b) =
            common::find_approval(&approvals_b).expect("B requested approval");
        assert_ne!(
            approval_a.as_str(),
            approval_b.as_str(),
            "approval identifiers are incarnation-scoped"
        );

        // Approving on A must dispatch only A's call.
        let approve_a = bed_a
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-live-a").expect("valid"),
                approval: approval_a,
                run: run_a,
                call: call_a,
            })
            .await;
        assert_eq!(approve_a.reply(), CommandReply::Accepted);
        let (data_a, mut control_a, finished_a) =
            common::drain_until_finished(&mut bed_a.data, &mut bed_a.control).await;
        control_a.extend(approvals_a);
        assert_eq!(finished_a.outcome(), RunOutcome::Completed);
        assert_eq!(bed_a.write_tool.execution_count(), 1, "A dispatched once");
        assert_eq!(
            bed_b.write_tool.execution_count(),
            0,
            "approving on A never dispatches B's pending call"
        );
        common::assert_single_terminal(&data_a, &control_a);

        // B is still awaiting its own decision.
        let (reply, snapshot) = bed_b
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-live-b-snap").expect("valid"),
                run: run_b.clone(),
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("B keeps a live snapshot");
        assert_eq!(snapshot.lifecycle(), RunLifecycle::Active);
        assert!(
            snapshot.pending_approvals().contains(&approval_b),
            "B's own approval is still pending: {:?}",
            snapshot.pending_approvals()
        );

        let approve_b = bed_b
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-live-b").expect("valid"),
                approval: approval_b,
                run: run_b,
                call: call_b,
            })
            .await;
        assert_eq!(approve_b.reply(), CommandReply::Accepted);
        let (data_b, mut control_b, finished_b) =
            common::drain_until_finished(&mut bed_b.data, &mut bed_b.control).await;
        control_b.extend(approvals_b);
        assert_eq!(finished_b.outcome(), RunOutcome::Completed);
        assert_eq!(bed_b.write_tool.execution_count(), 1, "B dispatched once");
        common::assert_single_terminal(&data_b, &control_b);
    });
}
