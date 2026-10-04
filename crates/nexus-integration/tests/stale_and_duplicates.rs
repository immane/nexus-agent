#![forbid(unsafe_code)]

//! Stale-identity rejection on both the runtime and frontend-consumer sides,
//! plus duplicate approval/cancellation safety, against real components.

mod common;

use std::time::Duration;

use nexus_core::{
    ApprovalId, ApproveCommand, CallId, CancelCommand, Command, CommandReply, DenyCommand,
    EventPayload, RequestId, RunId, RunOutcome, SessionId,
};
use nexus_fakes::{FakeTool, stop_turn, tool_turn};
use nexus_runtime::event_is_live;
use nexus_tui::{AppState, decisions};

/// Unknown runs are stale on the runtime path; finalized runs report their
/// terminal state instead of accepting more work.
#[test]
fn runtime_rejects_stale_and_finalized_targets() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut bed = common::make_bed(vec![stop_turn("done")], common::quick_config());
        let response = bed.runtime.submit(common::submit_cmd("stale")).await;
        let run = response.run().cloned().expect("run issued");
        let (_, _, finished) = common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);

        let unknown = RunId::new("run-999").expect("valid");
        let cancel_unknown = bed
            .runtime
            .cancel(CancelCommand {
                request: RequestId::new("req-stale-cancel").expect("valid"),
                run: unknown.clone(),
            })
            .await;
        assert_eq!(cancel_unknown.reply(), CommandReply::StaleOrUnknownTarget);
        let approve_unknown = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-stale-approve").expect("valid"),
                approval: ApprovalId::new("a9-9").expect("valid"),
                run: unknown,
                call: CallId::new("c9-9").expect("valid"),
            })
            .await;
        assert_eq!(approve_unknown.reply(), CommandReply::StaleOrUnknownTarget);

        let approve_finalized = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-late-approve").expect("valid"),
                approval: ApprovalId::new("a1-0").expect("valid"),
                run: run.clone(),
                call: CallId::new("c1-0").expect("valid"),
            })
            .await;
        assert_eq!(approve_finalized.reply(), CommandReply::AlreadyFinalized);
        let cancel_finalized = bed
            .runtime
            .cancel(CancelCommand {
                request: RequestId::new("req-late-cancel").expect("valid"),
                run: run.clone(),
            })
            .await;
        assert_eq!(cancel_finalized.reply(), CommandReply::AlreadyFinalized);
        let deny_finalized = bed
            .runtime
            .deny(DenyCommand {
                request: RequestId::new("req-late-deny").expect("valid"),
                approval: ApprovalId::new("a1-0").expect("valid"),
                run,
                call: CallId::new("c1-0").expect("valid"),
            })
            .await;
        assert_eq!(deny_finalized.reply(), CommandReply::AlreadyFinalized);
    });
}

/// An approval for the wrong call identity is rejected without dispatch; the
/// live approval still works exactly once afterwards.
#[test]
fn runtime_rejects_wrong_call_without_dispatch() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![common::candidate("host_write", r#"{"path":"src"}"#)]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed.runtime.submit(common::submit_cmd("wrong-call")).await;
        let run = response.run().cloned().expect("run issued");
        let approvals = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let (approval, call) = common::find_approval(&approvals).expect("approval requested");

        let wrong = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-wrong").expect("valid"),
                approval: approval.clone(),
                run: run.clone(),
                call: CallId::new("c9-9").expect("valid"),
            })
            .await;
        assert_eq!(wrong.reply(), CommandReply::StaleOrUnknownTarget);
        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "no dispatch on mismatch"
        );

        let good = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-good").expect("valid"),
                approval,
                run,
                call,
            })
            .await;
        assert_eq!(good.reply(), CommandReply::Accepted);
        let (_, _, finished) = common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(bed.write_tool.execution_count(), 1);
    });
}

/// The TUI consumer rejects stale-run updates instead of applying them to the
/// live run, while live events still apply.
#[test]
fn frontend_rejects_stale_run_updates() {
    use nexus_core::{AssistantText, RunEvent, TurnId, Usage, UsageFinality};

    let session = SessionId::new("sess-1").expect("valid");
    let live = RunId::new("run-1").expect("valid");
    let stale = RunId::new("run-9").expect("valid");
    let request = RequestId::new("req-1").expect("valid");

    let mut state = AppState::new();
    let started = RunEvent::new(
        session.clone(),
        live.clone(),
        0,
        EventPayload::RunStarted {
            request: request.clone(),
        },
    );
    assert!(state.apply_event(&started));
    assert!(event_is_live(&started, Some(&live)));

    let stale_event = RunEvent::new(
        session.clone(),
        stale.clone(),
        1,
        EventPayload::UsageUpdated(Usage::new(None, None, UsageFinality::Provisional)),
    );
    assert!(!event_is_live(&stale_event, Some(&live)));
    assert!(!event_is_live(&stale_event, None));
    assert!(!state.apply_event(&stale_event), "stale update rejected");
    assert_eq!(state.stale_rejected, 1);
    assert_eq!(state.last_seq(), Some(0), "stale event moves no cursor");

    let live_text = RunEvent::new(
        session,
        live.clone(),
        1,
        EventPayload::AssistantTextDelta(
            AssistantText::new(TurnId::new("t1-0").expect("valid"), "item-0", "live")
                .expect("fragment builds"),
        ),
    );
    assert!(event_is_live(&live_text, Some(&live)));
    assert!(state.apply_event(&live_text));
    assert_eq!(state.last_seq(), Some(1));
    assert_eq!(state.stale_rejected, 1, "live events are not counted stale");
    let _ = request;
}

/// TUI decision constructors preserve the exact live approval identity (no
/// edited arguments, no second policy path); the submit path validates at
/// the boundary and the runtime accepts it.
#[test]
fn tui_decisions_preserve_exact_approval_identity() {
    let rt = common::test_rt();
    rt.block_on(async {
        let card = nexus_tui::PendingApprovalCard {
            approval: ApprovalId::new("a1-0").expect("valid"),
            call: CallId::new("c1-0").expect("valid"),
            summary: "run tool host_write".to_owned(),
            scope_summary: "project scope".to_owned(),
            expires_at_elapsed: Duration::from_secs(120),
        };
        let run = RunId::new("run-7").expect("valid");

        let approve =
            decisions::approve_command(RequestId::new("req-a").expect("valid"), &run, &card);
        let Command::Approve(body) = &approve else {
            panic!("approve must emit Command::Approve");
        };
        assert_eq!(body.approval.as_str(), "a1-0");
        assert_eq!(body.call.as_str(), "c1-0");
        assert_eq!(body.run, run);

        let deny = decisions::deny_command(RequestId::new("req-d").expect("valid"), &run, &card);
        let Command::Deny(body) = &deny else {
            panic!("deny must emit Command::Deny");
        };
        assert_eq!(body.approval.as_str(), "a1-0");
        assert_eq!(body.call.as_str(), "c1-0");

        let cancel = decisions::cancel_command(RequestId::new("req-c").expect("valid"), &run);
        let Command::Cancel(body) = &cancel else {
            panic!("cancel must emit Command::Cancel");
        };
        assert_eq!(body.run, run);

        let session = SessionId::new("sess-1").expect("valid");
        let submit = decisions::submit_command(
            RequestId::new("req-s").expect("valid"),
            session,
            "do work",
            "m0-test",
        )
        .expect("valid submit builds");
        assert!(
            decisions::submit_command(
                RequestId::new("req-e").expect("valid"),
                SessionId::new("sess-1").expect("valid"),
                "",
                "m0-test",
            )
            .is_err()
        );

        let bed = common::make_bed(vec![stop_turn("done")], common::quick_config());
        let (response, _) = bed.runtime.handle(submit).await;
        assert_eq!(response.reply(), CommandReply::Accepted);
    });
}

/// A repeated approval for the same grant is stale and never dispatches a
/// second execution.
#[test]
fn duplicate_approve_never_redispatches() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![common::candidate("host_write", r#"{"path":"src"}"#)]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed.runtime.submit(common::submit_cmd("dup-approve")).await;
        let run = response.run().cloned().expect("run issued");
        let approvals = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let (approval, call) = common::find_approval(&approvals).expect("approval requested");

        let first = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-first").expect("valid"),
                approval: approval.clone(),
                run: run.clone(),
                call: call.clone(),
            })
            .await;
        assert_eq!(first.reply(), CommandReply::Accepted);
        let second = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-second").expect("valid"),
                approval,
                run,
                call,
            })
            .await;
        assert_eq!(second.reply(), CommandReply::StaleOrUnknownTarget);

        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(approvals);
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(bed.write_tool.execution_count(), 1, "single dispatch only");
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::ToolStarted(_)
            )),
            1
        );
    });
}

/// Repeated cancellations never double-dispatch: both are accepted while the
/// run is live, the tool runs at most once, and the run finalizes once as
/// cancelled (completion is not rewritten into rollback).
#[test]
fn duplicate_cancel_never_double_dispatches() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![common::candidate("host_read", r#"{"path":"src"}"#)]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed_with_tools(
            script,
            common::auto_config(),
            FakeTool::delayed("host_read", Duration::from_millis(200)),
            FakeTool::mutation(),
        );
        let response = bed.runtime.submit(common::submit_cmd("dup-cancel")).await;
        let run = response.run().cloned().expect("run issued");
        let prelude = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ToolStarted(_))
        })
        .await;

        let first = bed
            .runtime
            .cancel(CancelCommand {
                request: RequestId::new("req-cancel-1").expect("valid"),
                run: run.clone(),
            })
            .await;
        assert_eq!(first.reply(), CommandReply::Accepted);
        let second = bed
            .runtime
            .cancel(CancelCommand {
                request: RequestId::new("req-cancel-2").expect("valid"),
                run: run.clone(),
            })
            .await;
        assert_eq!(second.reply(), CommandReply::Accepted);

        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(prelude);
        assert_eq!(finished.outcome(), RunOutcome::Cancelled);
        assert_eq!(bed.read_tool.execution_count(), 1, "single dispatch only");
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::ToolStarted(_)
            )),
            1
        );
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::RunFinished(_)
            )),
            1,
            "single terminal outcome"
        );

        let late = bed
            .runtime
            .cancel(CancelCommand {
                request: RequestId::new("req-cancel-3").expect("valid"),
                run,
            })
            .await;
        assert_eq!(late.reply(), CommandReply::AlreadyFinalized);
    });
}
