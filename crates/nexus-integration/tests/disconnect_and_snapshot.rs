#![forbid(unsafe_code)]

//! Disconnect handling and bounded snapshots against the real runtime.

mod common;

use std::time::Duration;

use nexus_core::{
    ApprovalId, ApproveCommand, CallId, CommandReply, GetSnapshotCommand, RequestId, RunLifecycle,
    SubmitCommand,
};
use nexus_fakes::{FakeTool, stop_turn, tool_turn};

/// Dropping the consumer while a tool is active still finalizes the run
/// exactly once: a bounded snapshot remains available, the retired run
/// rejects further grants as finalized (not stale), and a new submission is
/// accepted, proving the runtime was not left with stuck active work.
#[test]
fn disconnect_while_tool_active_finalizes_once_with_bounded_snapshot() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![common::candidate("host_read", r#"{"path":"src"}"#)]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed_with_tools(
            script,
            common::auto_config(),
            FakeTool::delayed("host_read", Duration::from_millis(100)),
            FakeTool::mutation(),
        );
        let response = bed.runtime.submit(common::submit_cmd("disconnect")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let started = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), nexus_core::EventPayload::ToolStarted(_))
        })
        .await;
        assert!(
            matches!(
                started.last().expect("control events").payload(),
                nexus_core::EventPayload::ToolStarted(_)
            ),
            "tool entered execution while the consumer was attached"
        );

        // Disconnect: no consumer remains on either channel.
        drop(bed.data);
        drop(bed.control);

        let snapshot = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let (reply, snapshot) = bed
                    .runtime
                    .get_snapshot(GetSnapshotCommand {
                        request: RequestId::new("req-disc-snap").expect("valid"),
                        run: run.clone(),
                    })
                    .await;
                assert_eq!(reply.reply(), CommandReply::Accepted);
                let snapshot = snapshot.expect("known run keeps a snapshot");
                if matches!(snapshot.lifecycle(), RunLifecycle::Finalized(_)) {
                    break snapshot;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("run finalizes promptly after disconnect");
        assert!(
            snapshot.pending_approvals().len() <= nexus_core::Limits::M0_TEST_MAX_CONCURRENT_OPS,
            "pending approvals stay bounded"
        );
        assert!(
            snapshot.known_outcomes().len()
                <= nexus_core::Limits::M0_TEST_TOOL_CALLS_PER_RUN as usize,
            "known outcomes stay bounded"
        );

        // The terminal record is stable: a repeat read agrees, and grants
        // against the retired run are finalized, never silently stale.
        let (_, again) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-disc-snap2").expect("valid"),
                run: run.clone(),
            })
            .await;
        let again = again.expect("snapshot persists");
        assert_eq!(again.lifecycle(), snapshot.lifecycle());
        assert_eq!(again.last_sequence(), snapshot.last_sequence());

        let late_grant = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-disc-grant").expect("valid"),
                approval: ApprovalId::new("a1-0").expect("valid"),
                run: run.clone(),
                call: CallId::new("c1-0").expect("valid"),
            })
            .await;
        assert_eq!(late_grant.reply(), CommandReply::AlreadyFinalized);

        let next = bed
            .runtime
            .submit(
                SubmitCommand::new(
                    RequestId::new("req-disc-next").expect("valid"),
                    nexus_core::SessionId::new("sess-1").expect("valid"),
                    "do work",
                    "m0-test",
                )
                .expect("submit builds"),
            )
            .await;
        assert_eq!(
            next.reply(),
            CommandReply::Accepted,
            "retired run frees the single-run slot"
        );
        let _ = run;
    });
}
