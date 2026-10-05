#![forbid(unsafe_code)]

//! Concurrent submit and single-admission regressions against the real
//! runtime and real fakes.
//!
//! Findings under test:
//! - a submit issued while the only run waits on an approval decision is
//!   `Busy`, names that run, and leaves the pending grant and the undecided
//!   call untouched, so the decided grant still dispatches exactly once;
//! - a submit issued while a cancelled run's worker termination is
//!   unconfirmed is also `Busy`, but from a different state: the run is
//!   already `Finalized`, so the rejection comes from retained quarantine
//!   ownership and no second run may dispatch into the worker the runtime
//!   still owns;
//! - `Submit` acceptance reconciles with the `RunStarted` event (host-issued
//!   run plus originating request) exactly once, at sequence zero, with every
//!   later event belonging to that run;
//! - an invocation whose batch carries a duplicate terminal, and one that
//!   never reaches a terminal event, are refused whole: no candidate
//!   dispatches, no replacement invocation is issued, and the refused run
//!   still releases the single-run slot.
//!
//! Determinism: the shared `collect_control_until`/`drain_until_finished`
//! backstops are the only failure guards. Quarantine is observed through a
//! gated tool fake's entered flag (a blocking wait on a condition variable),
//! never a wall-clock race, and the "slot clears" poll accepts only
//! `Accepted`, because `Busy` is precisely the state under test.

mod common;

use std::time::Duration;

use nexus_core::{
    AgentError, ApproveCommand, CancelCommand, CommandReply, CommandResponse, ErrorCategory,
    EventPayload, FinishReason, GetSnapshotCommand, ProviderEvent, RequestId, RetryGuidance,
    RunLifecycle, RunOutcome, SessionId, TurnFinished, Usage, UsageFinality,
};
use nexus_fakes::{FakeTool, stop_turn, tool_turn};
use nexus_runtime::Runtime;

/// Bounded backstop for every loop below. Deliberately shorter than
/// [`nexus_fakes::FakeGate::RELEASE_TIMEOUT`], so a gated worker cannot
/// expire its own gate and end the test's quarantine window early.
const BACKSTOP: Duration = Duration::from_secs(5);

/// Terminal turn record with unknown (never zero) usage.
fn finished(reason: FinishReason) -> TurnFinished {
    TurnFinished::new(reason, Usage::new(None, None, UsageFinality::Final), None)
}

/// Asserts a rejected submit published nothing at all.
///
/// A `Busy` reply must be inert: no second `RunStarted`, and no event on
/// either channel, so a host draining the streams can never mistake the
/// rejection for the start of a run. Both channels were drained before this is
/// called, so a single extra event is a defect rather than a race.
fn assert_rejected_submit_published_nothing(bed: &mut common::Bed) {
    assert!(
        bed.data.try_recv().is_err(),
        "a rejected submit publishes no data event"
    );
    assert!(
        bed.control.try_recv().is_err(),
        "a rejected submit publishes no control event"
    );
}

/// Polls `submit` until the single active-run slot is admitted again.
///
/// The wait ends only on `Accepted`: a `Busy` answer is exactly the retained
/// ownership under test, so accepting it as success would hide the
/// regression. Bounded, so a slot that never clears fails the test instead of
/// hanging it.
async fn submit_until_admitted(runtime: &Runtime, tag: &str) -> CommandResponse {
    tokio::time::timeout(BACKSTOP, async {
        loop {
            let response = runtime.submit(common::submit_cmd(tag)).await;
            if response.reply() == CommandReply::Accepted {
                break response;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the retained single-run slot clears")
}

/// Drives one adversarial provider batch that must never dispatch.
///
/// `expected_message` is the batch-level diagnostic the run must record.
/// Asserting it (not just "nothing dispatched") is what separates a real
/// refusal from a defect that skips dispatch only because it honored some
/// other part of the batch.
///
/// Each scenario owns its runtime and its own bed, so one failing scenario
/// cannot leave a held gate or a live run behind for the next one.
fn assert_rejected_invocation(tag: &str, batch: Vec<ProviderEvent>, expected_message: &str) {
    let rt = common::test_rt();
    rt.block_on(async {
        // The script keeps a healthy follow-up turn: a refused first turn must
        // not be retried inside the run, and must not wedge the run either.
        let script = vec![batch, stop_turn("follow-up turn")];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed.runtime.submit(common::submit_cmd(tag)).await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response
            .run()
            .cloned()
            .expect("accepted submit issues a run");

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Failed,
            "the invocation is refused whole, never partially honored"
        );
        let error = finished.error().expect("a failed run keeps its diagnostic");
        assert_eq!(error.category(), ErrorCategory::Protocol);
        assert_eq!(
            error.message(),
            expected_message,
            "the batch-level diagnostic is recorded verbatim"
        );
        assert_eq!(
            bed.read_tool.execution_count(),
            0,
            "a refused invocation never dispatches"
        );
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::ToolStarted(_)
            )),
            0,
            "no call started"
        );
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::ApprovalRequired(_)
            )),
            0,
            "a refused candidate is never queued for a grant"
        );
        assert_eq!(
            bed.provider.call_count(),
            1,
            "the refused turn is not replaced by another invocation in the same run"
        );
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::RunStarted { .. }
            )),
            1,
            "one run, one opening event"
        );
        common::assert_single_terminal(&data, &control);
        common::assert_contiguous(&data, &control);
        for event in data.iter().chain(control.iter()) {
            assert_eq!(
                event.run(),
                &run,
                "every published event belongs to the refused run"
            );
        }

        // The refused run released the slot instead of wedging it: the next
        // submit is admitted, and the refused candidate still never runs.
        let next = bed
            .runtime
            .submit(common::submit_cmd(&format!("{tag}-next")))
            .await;
        assert_eq!(
            next.reply(),
            CommandReply::Accepted,
            "a failed run releases the single-run slot"
        );
        assert_ne!(
            next.run(),
            Some(&run),
            "the follow-up run has its own identity"
        );
        let (next_data, next_control, next_finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(next_finished.outcome(), RunOutcome::Completed);
        common::assert_single_terminal(&next_data, &next_control);
        assert_eq!(
            bed.read_tool.execution_count(),
            0,
            "the refused candidate never dispatches later either"
        );
    });
}

/// A submit while the single run is parked on an approval decision is `Busy`
/// and names that run; the rejection consumes no grant, drops no pending
/// approval, and dispatches nothing, and the run still completes with exactly
/// one dispatch for the one decision sent.
#[test]
fn submit_during_approval_wait_is_busy_and_leaves_the_grant_pending() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![common::candidate("host_write", r#"{"path":"dst"}"#)]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, common::quick_config());
        let first = bed
            .runtime
            .submit(common::submit_cmd("approval-busy"))
            .await;
        assert_eq!(first.reply(), CommandReply::Accepted);
        let run = first.run().cloned().expect("run issued");

        // Only the live approval wait makes the concurrent submit meaningful:
        // the run is parked between its request and its decision.
        let approvals = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let (approval, call) = common::find_approval(&approvals).expect("approval requested");

        for tag in ["approval-busy-second", "approval-busy-third"] {
            let busy = bed.runtime.submit(common::submit_cmd(tag)).await;
            assert_eq!(busy.reply(), CommandReply::Busy);
            assert_eq!(busy.run(), Some(&run), "busy names the waiting run");
        }
        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "a rejected submit never dispatches the undecided call"
        );
        assert_rejected_submit_published_nothing(&mut bed);

        // The rejection is inert state: the run is still active, the grant is
        // still pending, and nothing has been recorded as an outcome yet.
        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-approval-busy-snap").expect("valid"),
                run: run.clone(),
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("live run has a snapshot");
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Active,
            "the run waits for a decision, it is not finalized"
        );
        assert!(
            snapshot.pending_approvals().contains(&approval),
            "the pending grant survives a rejected submit: {:?}",
            snapshot.pending_approvals()
        );
        assert!(
            snapshot.known_outcomes().is_empty(),
            "nothing has been recorded yet: {:?}",
            snapshot.known_outcomes()
        );

        let granted = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-approval-busy-grant").expect("valid"),
                approval,
                run: run.clone(),
                call,
            })
            .await;
        assert_eq!(granted.reply(), CommandReply::Accepted);

        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(approvals);
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(
            bed.write_tool.execution_count(),
            1,
            "the one decided grant dispatches exactly once"
        );
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::ToolStarted(_)
            )),
            1,
            "one started call for one decision"
        );
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::RunStarted { .. }
            )),
            1,
            "the rejected submits never opened a second run"
        );
        assert_eq!(
            bed.provider.call_count(),
            2,
            "the run still followed its scripted two turns"
        );
        common::assert_single_terminal(&data, &control);
        for event in data.iter().chain(control.iter()) {
            assert_eq!(event.run(), &run, "every event belongs to the waiting run");
        }
    });
}

/// A submit while a cancelled run's worker termination is unconfirmed is
/// `Busy` even though the run already reached its terminal: the rejection
/// comes from retained quarantine ownership, so no second run may dispatch
/// into the worker the runtime still owns. Ownership ends only when the
/// worker actually terminates.
#[test]
fn submit_during_quarantine_is_busy_and_dispatches_nothing_extra() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![common::candidate("host_read", r#"{"path":"src"}"#)]),
            stop_turn("after quarantine"),
        ];
        // No approval handler: the read dispatches directly, so the gated
        // worker owns a real execution instead of waiting on a grant.
        let mut bed = common::make_bed_with_tools(
            script,
            common::auto_config(),
            FakeTool::gated("host_read"),
            FakeTool::mutation(),
        );
        let first = bed
            .runtime
            .submit(common::submit_cmd("quarantine-busy"))
            .await;
        assert_eq!(first.reply(), CommandReply::Accepted);
        let run = first.run().cloned().expect("run issued");

        let prelude = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ToolStarted(_))
        })
        .await;

        // `ToolStarted` is published before the worker is spawned, so wait for
        // the worker to confirm it entered the tool: only then does cancelling
        // interrupt an execution that owns the tool.
        let gate = bed.read_tool.gate().expect("gated tool exposes its gate");
        assert!(
            gate.wait_entered(BACKSTOP),
            "the blocked worker reached its gate before cancellation"
        );

        let cancel = bed
            .runtime
            .cancel(CancelCommand {
                request: RequestId::new("req-quarantine-cancel").expect("valid"),
                run: run.clone(),
            })
            .await;
        assert_eq!(cancel.reply(), CommandReply::Accepted);

        // Cancellation finalizes promptly while the worker stays owned.
        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(prelude);
        assert_eq!(finished.outcome(), RunOutcome::Cancelled);
        common::assert_single_terminal(&data, &control);
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::ToolStarted(_)
            )),
            1,
            "one dispatched execution before the cancel"
        );

        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-quarantine-snap").expect("valid"),
                run: run.clone(),
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        assert_eq!(
            snapshot
                .expect("finalized run keeps a snapshot")
                .lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Cancelled),
            "the busy window is a finalized run, not an active one"
        );

        for tag in ["quarantine-busy-second", "quarantine-busy-third"] {
            let busy = bed.runtime.submit(common::submit_cmd(tag)).await;
            assert_eq!(busy.reply(), CommandReply::Busy);
            assert_eq!(
                busy.run(),
                Some(&run),
                "busy names the quarantined run, so the host can name it to the user"
            );
        }
        assert_eq!(
            bed.read_tool.execution_count(),
            1,
            "no second dispatch while the runtime still owns the worker"
        );
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::RunStarted { .. }
            )),
            1,
            "no run was opened during quarantine"
        );
        assert_rejected_submit_published_nothing(&mut bed);

        // Releasing the worker lets it observe the live cancellation and
        // terminate; only then does retained ownership end.
        gate.release();
        let next = submit_until_admitted(&bed.runtime, "quarantine-after").await;
        let next_run = next.run().cloned().expect("run issued");
        assert_ne!(next_run, run, "the admitted run has its own identity");
        let (_, _, next_finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(next_finished.outcome(), RunOutcome::Completed);
        assert_eq!(
            bed.read_tool.execution_count(),
            1,
            "the released worker never runs a second time"
        );
    });
}

/// `Submit` acceptance reconciles with the `RunStarted` event: same
/// host-issued run, same originating request, sequence zero, exactly once.
/// A submit racing that window cannot open a second run, so the reconciled run
/// stays the only run in the stream, and the terminal plus the snapshot agree
/// on its final sequence.
#[test]
fn submit_reply_reconciles_with_run_started_and_stays_unique() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut bed = common::make_bed(vec![stop_turn("done")], common::quick_config());
        let command = common::submit_cmd("reconcile");
        let request = command.request.clone();
        let response = bed.runtime.submit(command).await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        assert_eq!(
            response.request(),
            &request,
            "the reply echoes the originating request"
        );
        let run = response
            .run()
            .cloned()
            .expect("accepted submit issues a run");

        let opened = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::RunStarted { .. })
        })
        .await;
        let started = opened.last().expect("run-started arrives");
        assert_eq!(
            started.run(),
            &run,
            "the event reconciles with the reply run"
        );
        assert_eq!(
            started.session(),
            &SessionId::new("sess-1").expect("valid"),
            "the event carries the submitted session"
        );
        assert_eq!(started.seq(), 0, "the opening event is sequence zero");
        match started.payload() {
            EventPayload::RunStarted { request: seen } => {
                assert_eq!(seen, &request, "the event carries the originating request");
            }
            other => panic!("expected run-started, got {other:?}"),
        }

        // A submit racing the open run cannot start a second one, so the
        // reconciled identity stays the single active run.
        let busy = bed
            .runtime
            .submit(common::submit_cmd("reconcile-busy"))
            .await;
        assert_eq!(busy.reply(), CommandReply::Busy);
        assert_eq!(busy.run(), Some(&run));

        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(opened);
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);

        let mut all: Vec<&nexus_core::RunEvent> = data.iter().chain(control.iter()).collect();
        all.sort_by_key(|event| event.seq());
        assert!(
            matches!(
                all.first().expect("events").payload(),
                EventPayload::RunStarted { .. }
            ),
            "the reconciled run opens with run-started"
        );
        assert!(
            all.last().expect("events").is_terminal(),
            "the run closes with its terminal event"
        );
        assert_eq!(
            all.iter()
                .filter(|event| matches!(event.payload(), EventPayload::RunStarted { .. }))
                .count(),
            1,
            "exactly one run-started for the reconciled run"
        );
        for event in &all {
            assert_eq!(
                event.run(),
                &run,
                "no event from the rejected submit leaked into the stream"
            );
        }

        // The snapshot reconciles with the same terminal sequence.
        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-reconcile-snap").expect("valid"),
                run,
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("finalized run keeps a snapshot");
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed)
        );
        assert_eq!(
            snapshot.last_sequence(),
            Some(all.last().expect("events").seq()),
            "the snapshot names the terminal sequence"
        );
    });
}

/// A batch carrying a duplicate terminal is refused whole: the structural
/// check runs before admission, so the complete candidate in the batch never
/// dispatches and the run fails with the batch-level diagnostic instead of
/// completing on whichever terminal it kept. A batch whose first terminal is
/// an honest `Failed` is not laundered by a trailing tool-calls terminal, and
/// the provider's typed error stays primary.
#[test]
fn duplicate_terminal_invocation_never_dispatches() {
    let duplicate_terminal = vec![
        ProviderEvent::ToolCallReady(common::candidate("host_read", r#"{"path":"src"}"#)),
        ProviderEvent::TurnFinished(finished(FinishReason::ToolCalls)),
        // A trailing clean stop tries to launder the turn into a completion
        // that never reports the extra terminal.
        ProviderEvent::TurnFinished(finished(FinishReason::Stop)),
    ];
    assert_rejected_invocation(
        "duplicate-terminal",
        duplicate_terminal,
        "provider batch contains more than one terminal event",
    );

    let failed_then_tool_calls = vec![
        ProviderEvent::Failed(
            AgentError::new(
                ErrorCategory::Protocol,
                "provider stream ended mid turn",
                RetryGuidance::DoNotRetry,
            )
            .expect("static safe diagnostic builds"),
        ),
        ProviderEvent::ToolCallReady(common::candidate("host_read", r#"{"path":"src"}"#)),
        ProviderEvent::TurnFinished(finished(FinishReason::ToolCalls)),
    ];
    assert_rejected_invocation(
        "duplicate-terminal-after-failure",
        failed_then_tool_calls,
        "provider stream ended mid turn",
    );
}

/// An invocation that never reaches a terminal event, and one whose argument
/// progress never resolves to a candidate while the terminal claims an
/// ordinary completion, are both refused whole: neither dispatches, neither is
/// treated as a clean stop, and neither consumes another model turn.
#[test]
fn unfinished_invocation_never_dispatches() {
    let never_finished = vec![
        ProviderEvent::ToolCallReady(common::candidate("host_read", r#"{"path":"src"}"#)),
        ProviderEvent::ToolCallDelta {
            item_key: "item-0".to_owned(),
            assembled_bytes: 8,
        },
        // No terminal event: the invocation is still in flight when the
        // runtime is handed the batch.
    ];
    assert_rejected_invocation(
        "unfinished-invocation",
        never_finished,
        "provider batch does not end with a terminal event",
    );

    // Mirrors the adversarial `FakeProvider::unfinished_success` shape: real
    // argument progress, no complete candidate, yet an ordinary stop.
    let unresolved_progress = vec![
        ProviderEvent::ToolCallDelta {
            item_key: "item-1".to_owned(),
            assembled_bytes: 8,
        },
        ProviderEvent::TurnFinished(finished(FinishReason::Stop)),
    ];
    assert_rejected_invocation(
        "unfinished-progress-stop",
        unresolved_progress,
        "stop finish reason leaves tool argument progress unresolved",
    );
}
