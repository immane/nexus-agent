#![forbid(unsafe_code)]

//! Sequential-run coverage against the real runtime and the real fakes: one
//! runtime instance, several runs in sequence on the same channels.
//!
//! Invariants under test:
//! - a finalized run frees the single execution slot, so the next submit is
//!   `Accepted` with its own `RunId`, and each run restarts its own per-run
//!   sequence space;
//! - the submission counter carried inside `RunId` grows strictly and never
//!   reuses an issued identity, including across a submit the runtime
//!   rejected while another run was active;
//! - a finalized run's terminal never reopens: no later cancel is accepted,
//!   its snapshot is never `Active` again, and no second terminal event or
//!   earlier-run envelope appears on either channel;
//! - a `Failed` run and a `Cancelled` run both leave the runtime able to
//!   accept the next submit, and neither dispatches work it never decided.
//!
//! Deterministic: every wait is a bounded failure backstop around recorded
//! invariants (terminal outcomes, event identity, sequence contiguity), never
//! a wall-clock expectation. The busy slot and the cancellation point are
//! both reached through a control-channel approval request, which a fast
//! provider cannot skip.

mod common;

use nexus_core::{
    AgentError, CancelCommand, CommandReply, EffectState, ErrorCategory, EventPayload,
    ExecutionStatus, GetSnapshotCommand, ProviderEvent, RequestId, RetryGuidance, RunEvent, RunId,
    RunLifecycle, RunOutcome, SessionId,
};
use nexus_fakes::{stop_turn, tool_turn};
use nexus_runtime::event_is_live;

/// Session shared by every `submit_cmd` helper call.
fn session() -> SessionId {
    SessionId::new("sess-1").expect("valid session id")
}

/// Splits an issued `RunId` into its incarnation prefix and its decimal
/// submission counter. The runtime formats run ids as
/// `r<incarnation>-<counter>` with the incarnation in hex, so the trailing
/// segment is unambiguously the counter.
#[track_caller]
fn run_identity(run: &RunId) -> (&str, u64) {
    let (incarnation, counter) = run
        .as_str()
        .rsplit_once('-')
        .unwrap_or_else(|| panic!("run id carries a counter segment: {}", run.as_str()));
    let parsed = counter
        .parse()
        .unwrap_or_else(|_| panic!("counter segment is decimal: {counter}"));
    (incarnation, parsed)
}

/// Asserts one drained run published only its own envelopes: a single owning
/// run and session, a per-run sequence space that starts again at zero, one
/// terminal event, and no gaps.
#[track_caller]
fn assert_published_only_this_run(
    data: &[RunEvent],
    control: &[RunEvent],
    run: &RunId,
    label: &str,
) {
    for event in data.iter().chain(control.iter()) {
        assert_eq!(event.run(), run, "{label}: envelope belongs to this run");
        assert_eq!(
            event.session(),
            &session(),
            "{label}: envelope keeps the submitted session"
        );
    }
    common::assert_single_terminal(data, control);
    common::assert_contiguous(data, control);
    let mut ordered: Vec<&RunEvent> = data.iter().chain(control.iter()).collect();
    ordered.sort_by_key(|event| event.seq());
    assert_eq!(
        ordered.first().expect("published events").seq(),
        0,
        "{label}: the per-run sequence space starts again at zero"
    );
    assert!(
        ordered.last().expect("published events").is_terminal(),
        "{label}: the terminal event is published last"
    );
}

/// Asserts a finalized run cannot be reopened by a later command.
///
/// Finished-run retention is deliberately bounded, so a late command may
/// degrade from `AlreadyFinalized` to `StaleOrUnknownTarget` once another run
/// has been finalized. Both are non-actionable classifications: the point of
/// this helper is the invariant they share, that the target never becomes
/// cancellable again and never reports itself active.
///
/// No `#[track_caller]`: an async function cannot propagate its caller's
/// location, so every assertion carries `label` instead.
async fn assert_terminal_cannot_reopen(bed: &common::Bed, run: &RunId, label: &str) {
    let cancel = bed
        .runtime
        .cancel(CancelCommand {
            request: RequestId::new(format!("req-{label}-reopen")).expect("valid request id"),
            run: run.clone(),
        })
        .await;
    assert_ne!(
        cancel.reply(),
        CommandReply::Accepted,
        "{label}: a finalized run is never cancelled again"
    );
    let (reply, snapshot) = bed
        .runtime
        .get_snapshot(GetSnapshotCommand {
            request: RequestId::new(format!("req-{label}-reopen-snap")).expect("valid request id"),
            run: run.clone(),
        })
        .await;
    match snapshot {
        Some(snapshot) => {
            assert_eq!(
                snapshot.run(),
                run,
                "{label}: the snapshot is the target run"
            );
            assert_ne!(
                snapshot.lifecycle(),
                RunLifecycle::Active,
                "{label}: a finalized run never reports itself active again"
            );
        }
        None => assert_eq!(
            reply.reply(),
            CommandReply::StaleOrUnknownTarget,
            "{label}: an evicted run carries no snapshot rather than a fabricated one"
        ),
    }
}

/// Payload predicates for the tool-lifecycle fixtures used below.
fn is_tool_started(payload: &EventPayload) -> bool {
    matches!(payload, EventPayload::ToolStarted(_))
}

fn is_tool_finished(payload: &EventPayload) -> bool {
    matches!(payload, EventPayload::ToolFinished(_))
}

fn is_approval_required(payload: &EventPayload) -> bool {
    matches!(payload, EventPayload::ApprovalRequired(_))
}

/// A completed run releases the slot: the next submit is accepted with its own
/// identity, and neither run leaks an envelope, a terminal, or conversation
/// state into the other.
#[test]
fn completed_run_frees_the_slot_for_the_next_sequential_run() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut bed = common::make_bed(
            vec![stop_turn("first answer"), stop_turn("second answer")],
            common::quick_config(),
        );

        let first = bed.runtime.submit(common::submit_cmd("seq-first")).await;
        assert_eq!(first.reply(), CommandReply::Accepted);
        let run_one = first.run().cloned().expect("accepted submit issues a run");
        let (data_one, control_one, finished_one) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished_one.outcome(), RunOutcome::Completed);
        assert!(
            finished_one.error().is_none(),
            "a completed run carries no execution error: {:?}",
            finished_one.error()
        );
        assert_published_only_this_run(&data_one, &control_one, &run_one, "first run");

        // While it is still the most recent run it answers commands explicitly.
        let (snapshot_reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-seq-one-snap").expect("valid request id"),
                run: run_one.clone(),
            })
            .await;
        assert_eq!(snapshot_reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("the finalized run keeps a snapshot");
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed)
        );
        let late = bed
            .runtime
            .cancel(CancelCommand {
                request: RequestId::new("req-seq-one-cancel").expect("valid request id"),
                run: run_one.clone(),
            })
            .await;
        assert_eq!(
            late.reply(),
            CommandReply::AlreadyFinalized,
            "a completed run is already finalized, never re-cancellable"
        );

        // The slot is free: the next submit is accepted with a fresh identity.
        let second = bed.runtime.submit(common::submit_cmd("seq-second")).await;
        assert_eq!(
            second.reply(),
            CommandReply::Accepted,
            "a finalized run frees the execution slot"
        );
        let run_two = second.run().cloned().expect("accepted submit issues a run");
        assert_ne!(
            run_two, run_one,
            "each sequential run carries its own identity"
        );
        let (incarnation_one, counter_one) = run_identity(&run_one);
        let (incarnation_two, counter_two) = run_identity(&run_two);
        assert_eq!(
            incarnation_one, incarnation_two,
            "one runtime incarnation issued both runs"
        );
        assert!(
            counter_two > counter_one,
            "the submission counter grows between sequential runs: {counter_one} -> {counter_two}"
        );

        let (data_two, control_two, finished_two) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished_two.outcome(), RunOutcome::Completed);
        assert_published_only_this_run(&data_two, &control_two, &run_two, "second run");

        // Each run keeps exactly one terminal of its own, and the consume-side
        // staleness check rejects the earlier run's events for the later run.
        assert_eq!(common::terminal_count(&data_one, &control_one), 1);
        assert_eq!(common::terminal_count(&data_two, &control_two), 1);
        for event in data_one.iter().chain(control_one.iter()) {
            assert!(
                !event_is_live(event, Some(&run_two)),
                "first-run events are stale once the second run is live"
            );
            assert!(
                !event_is_live(event, None),
                "first-run events are never live without a current run"
            );
        }

        // Every run builds its own model context: no conversation, turn, or
        // continuation state carries from one run into the next.
        let requests = bed.provider.requests();
        assert_eq!(requests.len(), 2, "exactly one model invocation per run");
        assert_eq!(requests[0].run(), &run_one);
        assert_eq!(requests[1].run(), &run_two);
        assert_ne!(
            requests[0].turn(),
            requests[1].turn(),
            "turn identity is issued per run, never reused"
        );
        for (index, request) in requests.iter().enumerate() {
            assert_eq!(
                request.conversation().len(),
                1,
                "run {} starts from its own input only, never an earlier run's context",
                index + 1
            );
            assert!(
                request.continuation().is_none(),
                "run {} inherits no continuation state",
                index + 1
            );
        }

        assert_terminal_cannot_reopen(&bed, &run_one, "first-run").await;
    });
}

/// The submission counter is an issuance counter, not an active-run index: it
/// grows strictly across accepted runs and never reuses a value, even across a
/// submit the runtime rejected while another run held the slot.
#[test]
fn run_counter_grows_across_sequential_and_rejected_submits() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut bed = common::make_bed(
            vec![
                stop_turn("first answer"),
                tool_turn(vec![common::candidate("host_write", r#"{"path":"dst"}"#)]),
                stop_turn("third answer"),
            ],
            common::quick_config(),
        );

        let first = bed
            .runtime
            .submit(common::submit_cmd("counter-first"))
            .await;
        assert_eq!(first.reply(), CommandReply::Accepted);
        let run_one = first.run().cloned().expect("accepted submit issues a run");
        let (_, _, finished_one) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished_one.outcome(), RunOutcome::Completed);
        let (incarnation_one, counter_one) = run_identity(&run_one);

        // Second run parks on an approval request, which is a deterministic
        // hold: the run cannot finish before this test decides its fate.
        let second = bed
            .runtime
            .submit(common::submit_cmd("counter-second"))
            .await;
        assert_eq!(second.reply(), CommandReply::Accepted);
        let run_two = second.run().cloned().expect("accepted submit issues a run");
        let (incarnation_two, counter_two) = run_identity(&run_two);
        let approvals = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        common::find_approval(&approvals).expect("second run requested approval");

        // A submit rejected while the slot is held issues no run identity, but
        // it must not rewind or reuse the counter either.
        let rejected = bed
            .runtime
            .submit(common::submit_cmd("counter-rejected"))
            .await;
        assert_eq!(rejected.reply(), CommandReply::Busy);
        assert_eq!(
            rejected.run(),
            Some(&run_two),
            "a rejected submit names the active run instead of issuing a new one"
        );

        let cancel = bed
            .runtime
            .cancel(CancelCommand {
                request: RequestId::new("req-counter-cancel").expect("valid request id"),
                run: run_two.clone(),
            })
            .await;
        assert_eq!(cancel.reply(), CommandReply::Accepted);
        let (data_two, mut control_two, finished_two) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control_two.extend(approvals);
        assert_eq!(finished_two.outcome(), RunOutcome::Cancelled);
        assert_published_only_this_run(&data_two, &control_two, &run_two, "second run");

        let third = bed
            .runtime
            .submit(common::submit_cmd("counter-third"))
            .await;
        assert_eq!(
            third.reply(),
            CommandReply::Accepted,
            "the finalized slot is reusable after a cancellation"
        );
        let run_three = third.run().cloned().expect("accepted submit issues a run");
        let (incarnation_three, counter_three) = run_identity(&run_three);
        assert_eq!(incarnation_two, incarnation_three);
        assert_eq!(
            incarnation_one, incarnation_three,
            "one runtime incarnation issued every run"
        );
        assert!(
            counter_two > counter_one,
            "the counter grows between the first and second run"
        );
        assert!(
            counter_three > counter_two,
            "the counter grows after a run that ended in cancellation: {counter_two} -> {counter_three}"
        );
        assert_ne!(run_three, run_one, "an earlier run identity is never reissued");
        assert_ne!(
            run_three, run_two,
            "the identity of a cancelled run is never reissued"
        );

        let (data_three, control_three, finished_three) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished_three.outcome(), RunOutcome::Completed);
        assert_published_only_this_run(&data_three, &control_three, &run_three, "third run");
    });
}

/// A failed run is terminal but not sticky: its candidates are discarded
/// without dispatch, the terminal carries the typed provider failure, and the
/// next submit is accepted and completes.
#[test]
fn failed_run_is_followed_by_an_accepted_successful_run() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut bed = common::make_bed(
            vec![failed_turn(), stop_turn("recovered answer")],
            common::quick_config(),
        );

        let first = bed.runtime.submit(common::submit_cmd("failed-first")).await;
        assert_eq!(first.reply(), CommandReply::Accepted);
        let run_one = first.run().cloned().expect("accepted submit issues a run");
        let (data_one, control_one, finished_one) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished_one.outcome(), RunOutcome::Failed);
        assert_published_only_this_run(&data_one, &control_one, &run_one, "failed run");

        // The terminal keeps the provider's typed failure, not a bare status.
        let error = finished_one
            .error()
            .expect("a failed run retains its typed execution error");
        assert_eq!(error.category(), ErrorCategory::Protocol);
        assert_eq!(error.message(), "provider invocation failed mid turn");
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);

        // The candidate in the failed turn never became a call: nothing was
        // offered for approval, dispatched, or recorded as a tool outcome.
        assert_eq!(
            common::count_payload(&data_one, &control_one, is_approval_required),
            0,
            "a failed invocation requests no approval"
        );
        assert_eq!(
            common::count_payload(&data_one, &control_one, is_tool_started),
            0,
            "a failed invocation dispatches nothing"
        );
        assert_eq!(
            common::count_payload(&data_one, &control_one, is_tool_finished),
            0,
            "a discarded candidate produces no outcome record"
        );
        assert_eq!(bed.write_tool.execution_count(), 0);

        // The failure is a real terminal, not a hole in the record.
        let (snapshot_reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-failed-snap").expect("valid request id"),
                run: run_one.clone(),
            })
            .await;
        assert_eq!(snapshot_reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("the failed run keeps a snapshot");
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Failed)
        );
        assert!(
            snapshot.pending_approvals().is_empty(),
            "no approval survives the failed run"
        );
        assert!(snapshot.known_outcomes().is_empty());

        // The next submit is accepted with a fresh identity and succeeds.
        let second = bed
            .runtime
            .submit(common::submit_cmd("failed-second"))
            .await;
        assert_eq!(
            second.reply(),
            CommandReply::Accepted,
            "a failed run frees the execution slot"
        );
        let run_two = second.run().cloned().expect("accepted submit issues a run");
        assert_ne!(run_two, run_one);
        let (_, counter_one) = run_identity(&run_one);
        let (_, counter_two) = run_identity(&run_two);
        assert!(
            counter_two > counter_one,
            "the submission counter keeps growing after a failure: {counter_one} -> {counter_two}"
        );

        let (data_two, control_two, finished_two) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished_two.outcome(), RunOutcome::Completed);
        assert!(
            finished_two.error().is_none(),
            "the recovering run completes without an execution error: {:?}",
            finished_two.error()
        );
        assert_published_only_this_run(&data_two, &control_two, &run_two, "second run");
        assert_eq!(common::terminal_count(&data_one, &control_one), 1);
        assert_eq!(common::terminal_count(&data_two, &control_two), 1);
        for event in data_one.iter().chain(control_one.iter()) {
            assert!(
                !event_is_live(event, Some(&run_two)),
                "the failed run's events are stale for the recovering run"
            );
        }

        // The failed invocation's discarded candidate never reaches the
        // provider context of the recovering run.
        let requests = bed.provider.requests();
        assert_eq!(requests.len(), 2, "one model invocation per run");
        assert_eq!(requests[0].run(), &run_one);
        assert_eq!(requests[1].run(), &run_two);
        assert_eq!(
            requests[1].conversation().len(),
            1,
            "the recovering run starts from its own input only"
        );
        assert_eq!(bed.write_tool.execution_count(), 0);

        assert_terminal_cannot_reopen(&bed, &run_one, "failed-run").await;
    });
}

/// A run cancelled while awaiting approval records honest unknown effects,
/// never executes the undecided call, and still frees the slot for the next
/// accepted run.
#[test]
fn cancelled_run_is_followed_by_a_new_accepted_run() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut bed = common::make_bed(
            vec![
                tool_turn(vec![common::candidate("host_write", r#"{"path":"dst"}"#)]),
                stop_turn("answer after cancellation"),
            ],
            common::quick_config(),
        );

        let first = bed.runtime.submit(common::submit_cmd("cancel-first")).await;
        assert_eq!(first.reply(), CommandReply::Accepted);
        let run_one = first.run().cloned().expect("accepted submit issues a run");

        // Waiting for the approval request is the deterministic cancellation
        // point: the run cannot reach a terminal before a decision arrives.
        let approvals = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let (approval, call) = common::find_approval(&approvals).expect("approval requested");
        let cancel = bed
            .runtime
            .cancel(CancelCommand {
                request: RequestId::new("req-cancel-first").expect("valid request id"),
                run: run_one.clone(),
            })
            .await;
        assert_eq!(cancel.reply(), CommandReply::Accepted);

        // The approval is deliberately NOT answered mid-flight: a grant racing
        // the cancellation is still abandoned by the run, and asserting on that
        // race would be wall-clock dependent. The post-terminal grant below is
        // the deterministic form of the same claim.
        let (data_one, mut control_one, finished_one) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control_one.extend(approvals);
        assert_eq!(finished_one.outcome(), RunOutcome::Cancelled);
        assert_published_only_this_run(&data_one, &control_one, &run_one, "cancelled run");

        // The undecided call is recorded, not executed: no dispatch, and the
        // outcome keeps unknown effects instead of claiming a rollback.
        assert_eq!(
            common::count_payload(&data_one, &control_one, is_tool_started),
            0,
            "a call cancelled before dispatch never starts"
        );
        assert_eq!(
            common::count_payload(&data_one, &control_one, is_tool_finished),
            1,
            "the abandoned call still gets exactly one outcome record"
        );
        let statuses: Vec<ExecutionStatus> = control_one
            .iter()
            .filter_map(|event| match event.payload() {
                EventPayload::ToolFinished(info) => Some(info.outcome.status()),
                _ => None,
            })
            .collect();
        assert_eq!(
            statuses,
            vec![ExecutionStatus::Cancelled],
            "the interrupted call is recorded as cancelled"
        );
        let effects: Vec<EffectState> = control_one
            .iter()
            .filter_map(|event| match event.payload() {
                EventPayload::ToolFinished(info) => Some(info.outcome.effect()),
                _ => None,
            })
            .collect();
        assert_eq!(
            effects,
            vec![EffectState::Unknown],
            "an undecided call keeps unknown effects, never a fabricated rollback"
        );
        assert_eq!(bed.write_tool.execution_count(), 0);

        let (snapshot_reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-cancel-snap").expect("valid request id"),
                run: run_one.clone(),
            })
            .await;
        assert_eq!(snapshot_reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("the cancelled run keeps a snapshot");
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Cancelled)
        );
        assert!(snapshot.pending_approvals().is_empty());

        // The abandoned grant is dead once the terminal is published: it is
        // classified against the finalized run and never dispatches the call
        // the cancellation withheld.
        let late_approval = bed
            .runtime
            .approve(nexus_core::ApproveCommand {
                request: RequestId::new("req-cancel-late-approve").expect("valid request id"),
                approval,
                run: run_one.clone(),
                call,
            })
            .await;
        assert_eq!(
            late_approval.reply(),
            CommandReply::AlreadyFinalized,
            "the cancelled run's abandoned approval cannot be granted afterwards"
        );
        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "the withheld call never executes, whatever the grant answers"
        );

        // Cancellation is not sticky: the next submit is accepted immediately.
        let second = bed
            .runtime
            .submit(common::submit_cmd("cancel-second"))
            .await;
        assert_eq!(
            second.reply(),
            CommandReply::Accepted,
            "a cancelled run frees the execution slot for a new run"
        );
        let run_two = second.run().cloned().expect("accepted submit issues a run");
        assert_ne!(
            run_two, run_one,
            "the cancelled run's identity is not reissued"
        );
        let (_, counter_one) = run_identity(&run_one);
        let (_, counter_two) = run_identity(&run_two);
        assert!(
            counter_two > counter_one,
            "the submission counter grows after a cancellation: {counter_one} -> {counter_two}"
        );
        assert_terminal_cannot_reopen(&bed, &run_one, "cancelled-run").await;

        let (data_two, control_two, finished_two) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished_two.outcome(), RunOutcome::Completed);
        assert!(finished_two.error().is_none());
        assert_published_only_this_run(&data_two, &control_two, &run_two, "second run");
        assert_eq!(common::terminal_count(&data_one, &control_one), 1);
        assert_eq!(common::terminal_count(&data_two, &control_two), 1);
        for event in data_one.iter().chain(control_one.iter()) {
            assert!(
                !event_is_live(event, Some(&run_two)),
                "the cancelled run's events are stale for the next run"
            );
        }

        // The new run inherits nothing from the abandoned one, and the write
        // double stayed untouched across both runs.
        let requests = bed.provider.requests();
        assert_eq!(requests.len(), 2, "one model invocation per run");
        assert_eq!(requests[0].run(), &run_one);
        assert_eq!(requests[1].run(), &run_two);
        assert_eq!(
            requests[1].conversation().len(),
            1,
            "the cancelled run's admitted call never leaks into the next context"
        );
        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "neither run dispatched the undecided mutation"
        );

        assert_terminal_cannot_reopen(&bed, &run_one, "cancelled-run-final").await;
    });
}

/// A turn that proposes a call and then fails terminally.
fn failed_turn() -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ToolCallReady(common::candidate("host_write", r#"{"path":"dst"}"#)),
        ProviderEvent::Failed(
            AgentError::new(
                ErrorCategory::Protocol,
                "provider invocation failed mid turn",
                RetryGuidance::DoNotRetry,
            )
            .expect("static safe error builds"),
        ),
    ]
}
