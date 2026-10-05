#![forbid(unsafe_code)]

//! Consumer-visible approval-decision matrix over the shared integration bed.
//!
//! One cell per decision shape, each driven through the real
//! [`nexus_runtime::Runtime`] with the shared fakes and helpers in
//! `tests/common/mod.rs`, so every cell is pinned against the same recorded
//! evidence a frontend sees: both event channels, the tool ports' own
//! execution logs, the model requests the provider actually observed, and the
//! run snapshot.
//!
//! Cells:
//! - **grant** — the exact accepted `(run, approval, call)` tuple dispatches
//!   the bound call once, under the scope the notice advertised, with the
//!   bound arguments, and the success reaches both the record and the next
//!   model turn;
//! - **deny** — the refusal records exactly one `Denied`/`NotStarted` outcome
//!   for the bound call and no port is ever invoked;
//! - **wrong call** — a mismatched tuple is stale, dispatches nothing, and
//!   leaves the live grant pending so the exact tuple still works;
//! - **second grant** — a repeated grant for a decided call is stale in either
//!   polarity and never dispatches or rewrites a second execution;
//! - **late grant** — a grant replayed after the terminal event replies
//!   `AlreadyFinalized`, dispatches nothing, and leaves the recorded outcome
//!   untouched;
//! - **continuation** — a denial is carried into the next model turn, so the
//!   run keeps making progress instead of stalling on the refusal.
//!
//! Scope split: `nexus-runtime/tests/cov_approval_dispatch.rs` pins the same
//! decision shapes from inside that crate with in-file port doubles. This file
//! adds the cross-boundary facts only the shared bed can observe: the shared
//! fakes' execution logs and request log, the two-channel event record with
//! contiguity and single-terminal checks, and the snapshot truth.
//!
//! Determinism: every wait goes through the shared bounded drain helpers and
//! every assertion is on recorded counts, identities, and terminal state — no
//! wall-clock assertion and no fixed sleep. The stale/duplicate cells send
//! their decisions back to back: on the current-thread runtime an uncontended
//! `resolve_grant` completes without yielding, so the parked run driver cannot
//! finalize between two consecutive decisions, which is what makes the
//! `StaleOrUnknownTarget` replies (rather than `AlreadyFinalized`) stable.

mod common;

use nexus_core::{
    ApprovalId, ApprovalNotice, ApproveCommand, CallId, CommandReply, CommandResponse, DenyCommand,
    EffectState, EventPayload, Evidence, ExecutionStatus, GetSnapshotCommand, ModelContextItem,
    ModelRequest, ProviderEvent, RequestId, RunEvent, RunId, RunLifecycle, RunOutcome, Snapshot,
    ToolOutcome, TurnId,
};
use nexus_fakes::{stop_turn, tool_turn};

/// Arguments of the single mutating candidate. `host_write` always requires a
/// grant under the M0 policy, so every cell in the matrix reaches the same
/// approval wait.
const WRITE_ARGS: &str = r#"{"path":"src"}"#;
/// Scope the policy resolves for [`WRITE_ARGS`].
const WRITE_SCOPE: &str = "path:src";

/// Two-turn script: one mutating candidate, then a plain stop. A third turn is
/// never scripted, so any extra turn surfaces as an explicit failure instead of
/// a fabricated idle success.
fn mutation_script() -> Vec<Vec<ProviderEvent>> {
    vec![
        tool_turn(vec![common::candidate("host_write", WRITE_ARGS)]),
        stop_turn("done"),
    ]
}

/// One recorded tool outcome, flattened to the fields the matrix pins.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Recorded {
    call: CallId,
    status: ExecutionStatus,
    effect: EffectState,
    evidence: Evidence,
}

impl Recorded {
    /// Convenience for the single-outcome cells.
    fn of(call: &CallId, status: ExecutionStatus, effect: EffectState) -> Vec<Self> {
        vec![Self {
            call: call.clone(),
            status,
            effect,
            evidence: Evidence::HostObserved,
        }]
    }
}

/// Submits one run and returns its host-issued identity.
async fn submit_run(bed: &common::Bed, tag: &str) -> RunId {
    let response = bed.runtime.submit(common::submit_cmd(tag)).await;
    assert_eq!(
        response.reply(),
        CommandReply::Accepted,
        "submit is accepted"
    );
    response
        .run()
        .cloned()
        .expect("an accepted submit issues a run")
}

/// Drives the run to its first approval request. Returns the consumed control
/// prelude (to be merged back into the final control record) and the exact
/// notice, so every later assertion binds to the grant the run really
/// published.
async fn approval_request(bed: &mut common::Bed) -> (Vec<RunEvent>, ApprovalNotice) {
    let requested = common::collect_control_until(&mut bed.control, |event| {
        matches!(event.payload(), EventPayload::ApprovalRequired(_))
    })
    .await;
    let notice = requested
        .iter()
        .find_map(|event| match event.payload() {
            EventPayload::ApprovalRequired(notice) => Some(notice.clone()),
            _ => None,
        })
        .expect("the mutating candidate requests a grant before dispatch");
    assert_eq!(
        common::find_approval(&requested),
        Some((notice.approval.clone(), notice.call.clone())),
        "the shared helper and the notice agree on the live grant"
    );
    (requested, notice)
}

/// Every call that entered execution, across both channels, in publication
/// order. An extra entry is always a duplicate dispatch.
fn started_calls(data: &[RunEvent], control: &[RunEvent]) -> Vec<CallId> {
    data.iter()
        .chain(control.iter())
        .filter_map(|event| match event.payload() {
            EventPayload::ToolStarted(info) => Some(info.call.clone()),
            _ => None,
        })
        .collect()
}

/// Every recorded tool outcome, across both channels, in publication order.
fn recorded_outcomes(data: &[RunEvent], control: &[RunEvent]) -> Vec<Recorded> {
    data.iter()
        .chain(control.iter())
        .filter_map(|event| match event.payload() {
            EventPayload::ToolFinished(info) => Some(Recorded {
                call: info.call.clone(),
                status: info.outcome.status(),
                effect: info.outcome.effect(),
                evidence: info.outcome.evidence(),
            }),
            _ => None,
        })
        .collect()
}

/// The recorded outcome bound to `call`, for content-level assertions.
fn outcome_for<'a>(
    data: &'a [RunEvent],
    control: &'a [RunEvent],
    call: &CallId,
) -> &'a ToolOutcome {
    data.iter()
        .chain(control.iter())
        .find_map(|event| match event.payload() {
            EventPayload::ToolFinished(info) if &info.call == call => Some(&info.outcome),
            _ => None,
        })
        .expect("the bound call records exactly one outcome")
}

/// Tool results the provider actually received in model request `index`.
fn tool_results_in(requests: &[ModelRequest], index: usize) -> Vec<Recorded> {
    requests
        .get(index)
        .expect("the run requested this model turn")
        .conversation()
        .iter()
        .filter_map(|item| match item {
            ModelContextItem::ToolResult { call, outcome, .. } => Some(Recorded {
                call: call.clone(),
                status: outcome.status(),
                effect: outcome.effect(),
                evidence: outcome.evidence(),
            }),
            _ => None,
        })
        .collect()
}

/// Turn identity of the recorded model request at `index`.
fn turn_of(requests: &[ModelRequest], index: usize) -> TurnId {
    requests
        .get(index)
        .expect("the run requested this model turn")
        .turn()
        .clone()
}

/// Snapshot of a known run, accepted by construction.
async fn snapshot_of(bed: &common::Bed, run: &RunId, tag: &str) -> Snapshot {
    let (reply, snapshot) = bed
        .runtime
        .get_snapshot(GetSnapshotCommand {
            request: RequestId::new(format!("req-{tag}")).expect("request builds"),
            run: run.clone(),
        })
        .await;
    assert_eq!(
        reply.reply(),
        CommandReply::Accepted,
        "a known run keeps a reachable snapshot"
    );
    snapshot.expect("a known run always has snapshot truth")
}

/// Grants the bound call for an exact `(run, approval, call)` tuple.
async fn approve(
    bed: &common::Bed,
    run: &RunId,
    approval: &ApprovalId,
    call: &CallId,
    tag: &str,
) -> CommandResponse {
    bed.runtime
        .approve(ApproveCommand {
            request: RequestId::new(format!("req-{tag}")).expect("request builds"),
            approval: approval.clone(),
            run: run.clone(),
            call: call.clone(),
        })
        .await
}

/// Cell **grant**: one accepted tuple, one dispatch, exact binding. The
/// advertised scope, the bound arguments, the executor log, the recorded
/// outcome, the next model turn, and the snapshot must all agree.
#[test]
fn granted_call_executes_exactly_once_with_the_advertised_scope() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut bed = common::make_bed(mutation_script(), common::quick_config());
        let run = submit_run(&bed, "grant-once").await;
        let (requested, notice) = approval_request(&mut bed).await;
        let (approval, call) = (notice.approval.clone(), notice.call.clone());
        assert_eq!(
            notice.scope_summary, WRITE_SCOPE,
            "the notice advertises the resolved resource scope"
        );
        let preview = format!("host_write {WRITE_ARGS}");
        assert_eq!(
            notice.args_preview(),
            Some(preview.as_str()),
            "the notice previews the exact arguments that would execute"
        );
        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "an undecided call never reaches a tool port"
        );

        let granted = approve(&bed, &run, &approval, &call, "grant-once").await;
        assert_eq!(granted.reply(), CommandReply::Accepted);
        assert_eq!(granted.run(), Some(&run), "the grant reply names the run");

        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(requested);

        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(
            started_calls(&data, &control),
            vec![call.clone()],
            "the bound call enters execution exactly once"
        );
        assert_eq!(
            recorded_outcomes(&data, &control),
            Recorded::of(&call, ExecutionStatus::Succeeded, EffectState::KnownApplied),
            "exactly one honest success is recorded for the bound call"
        );
        let log = bed.write_tool.log();
        assert_eq!(log.len(), 1, "the mutation port ran exactly once");
        assert_eq!(log[0].call, call, "the executor ran the granted call");
        assert_eq!(
            log[0].args, WRITE_ARGS,
            "the executor received the bound arguments, unedited"
        );
        assert_eq!(
            log[0].scope.as_str(),
            notice.scope_summary,
            "the executor ran under the scope the notice advertised"
        );
        assert_eq!(
            bed.read_tool.execution_count(),
            0,
            "no unadmitted port is touched"
        );
        assert_eq!(
            common::all_approvals(&control),
            vec![(approval.clone(), call.clone())],
            "the run asks for exactly one grant"
        );
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);

        let requests = bed.provider.requests();
        assert_eq!(
            requests.len(),
            2,
            "the loop continues after the recorded result"
        );
        assert_eq!(
            tool_results_in(&requests, 1),
            Recorded::of(&call, ExecutionStatus::Succeeded, EffectState::KnownApplied),
            "the approved outcome reaches the model, not only the record"
        );
        let snapshot = snapshot_of(&bed, &run, "grant-once").await;
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed)
        );
        assert!(
            snapshot.pending_approvals().is_empty(),
            "a decided grant leaves the pending set: {:?}",
            snapshot.pending_approvals()
        );
        let known = snapshot.known_outcomes();
        assert_eq!(known.len(), 1, "one known outcome: {known:?}");
        assert_eq!(known[0].call, call);
        assert_eq!(known[0].status, ExecutionStatus::Succeeded);
        assert_eq!(known[0].effect, EffectState::KnownApplied);
    });
}

/// Cell **deny**: the refusal is recorded honestly for the bound call and no
/// port is invoked.
#[test]
fn denied_call_records_an_outcome_without_executing() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut bed = common::make_bed(mutation_script(), common::quick_config());
        let run = submit_run(&bed, "deny-once").await;
        let (requested, notice) = approval_request(&mut bed).await;
        let (approval, call) = (notice.approval.clone(), notice.call.clone());

        let denied = bed
            .runtime
            .deny(DenyCommand {
                request: RequestId::new("req-deny-once").expect("valid"),
                approval: approval.clone(),
                run: run.clone(),
                call: call.clone(),
            })
            .await;
        assert_eq!(denied.reply(), CommandReply::Accepted);
        assert_eq!(denied.run(), Some(&run), "the refusal reply names the run");

        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(requested);

        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::ToolStarted(_)
            )),
            0,
            "a refused call never enters execution"
        );
        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "the mutating port is never invoked for a refusal"
        );
        assert_eq!(bed.read_tool.execution_count(), 0);
        assert_eq!(
            recorded_outcomes(&data, &control),
            Recorded::of(&call, ExecutionStatus::Denied, EffectState::NotStarted),
            "the refusal records exactly one denial bound to the call"
        );
        let outcome = outcome_for(&data, &control, &call);
        assert!(
            !outcome.content().is_empty(),
            "the denial keeps its reason for the consumer: {:?}",
            outcome.content()
        );
        assert!(!outcome.is_truncated());
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);

        let snapshot = snapshot_of(&bed, &run, "deny-once").await;
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed)
        );
        assert!(
            snapshot.pending_approvals().is_empty(),
            "a refused grant leaves the pending set: {:?}",
            snapshot.pending_approvals()
        );
        let known = snapshot.known_outcomes();
        assert_eq!(known.len(), 1, "one known outcome: {known:?}");
        assert_eq!(known[0].call, call);
        assert_eq!(known[0].status, ExecutionStatus::Denied);
        assert_eq!(
            known[0].effect,
            EffectState::NotStarted,
            "a refusal claims no effect"
        );
    });
}

/// Cell **wrong call**: a mismatched tuple is stale, dispatches nothing, and
/// leaves the live grant pending so the exact tuple still resolves it once.
#[test]
fn wrong_call_grant_is_stale_and_leaves_the_live_grant_undecided() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut bed = common::make_bed(mutation_script(), common::quick_config());
        let run = submit_run(&bed, "wrong-call").await;
        let (requested, notice) = approval_request(&mut bed).await;
        let (approval, call) = (notice.approval.clone(), notice.call.clone());

        let mismatched = approve(
            &bed,
            &run,
            &approval,
            &CallId::new("cff-0-0-9").expect("valid"),
            "wrong-call",
        )
        .await;
        assert_eq!(
            mismatched.reply(),
            CommandReply::StaleOrUnknownTarget,
            "a grant bound to another call is stale"
        );
        assert_eq!(
            mismatched.run(),
            Some(&run),
            "the stale reply still correlates the owning run"
        );
        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "a mismatched tuple never dispatches"
        );
        assert_eq!(bed.read_tool.execution_count(), 0);

        // The driver is parked in its approval wait, so this snapshot observes
        // the state immediately after the rejection without a test-side yield.
        let live = snapshot_of(&bed, &run, "wrong-call-live").await;
        assert_eq!(live.lifecycle(), RunLifecycle::Active);
        assert!(
            live.pending_approvals().contains(&approval),
            "the rejected tuple never consumes the live grant: {:?}",
            live.pending_approvals()
        );
        assert!(
            live.known_outcomes().is_empty(),
            "an undecided call has no recorded outcome: {:?}",
            live.known_outcomes()
        );

        let granted = approve(&bed, &run, &approval, &call, "wrong-call-exact").await;
        assert_eq!(granted.reply(), CommandReply::Accepted);
        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(requested);

        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(
            started_calls(&data, &control),
            vec![call.clone()],
            "the exact tuple dispatches once after the rejection"
        );
        assert_eq!(
            recorded_outcomes(&data, &control),
            Recorded::of(&call, ExecutionStatus::Succeeded, EffectState::KnownApplied),
            "the single execution records one honest success"
        );
        assert_eq!(bed.write_tool.execution_count(), 1);
        let log = bed.write_tool.log();
        assert_eq!(log[0].call, call);
        assert_eq!(log[0].scope.as_str(), WRITE_SCOPE);
        assert_eq!(
            common::all_approvals(&control),
            vec![(approval.clone(), call.clone())],
            "the rejected tuple requested no second grant"
        );
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);
    });
}

/// Cell **second grant**: a decided grant never decides twice. Both polarities
/// of the repeat are stale, and neither dispatches again nor rewrites the
/// recorded success of the accepted grant.
#[test]
fn second_grant_for_the_same_call_is_stale_and_never_redispatches() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut bed = common::make_bed(mutation_script(), common::quick_config());
        let run = submit_run(&bed, "second-grant").await;
        let (requested, notice) = approval_request(&mut bed).await;
        let (approval, call) = (notice.approval.clone(), notice.call.clone());

        let first = approve(&bed, &run, &approval, &call, "second-grant-1").await;
        assert_eq!(first.reply(), CommandReply::Accepted);

        // Sent back to back: an uncontended `resolve_grant` completes without
        // yielding, so the run is still live for both repeats.
        let second = approve(&bed, &run, &approval, &call, "second-grant-2").await;
        assert_eq!(
            second.reply(),
            CommandReply::StaleOrUnknownTarget,
            "a grant never decides twice"
        );
        assert_eq!(second.run(), Some(&run));
        let late_refusal = bed
            .runtime
            .deny(DenyCommand {
                request: RequestId::new("req-second-grant-3").expect("valid"),
                approval: approval.clone(),
                run: run.clone(),
                call: call.clone(),
            })
            .await;
        assert_eq!(
            late_refusal.reply(),
            CommandReply::StaleOrUnknownTarget,
            "a decided grant cannot be reversed by a late refusal"
        );
        assert_eq!(late_refusal.run(), Some(&run));

        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(requested);

        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(
            bed.write_tool.execution_count(),
            1,
            "repeated decisions never redispatch"
        );
        assert_eq!(
            started_calls(&data, &control),
            vec![call.clone()],
            "exactly one execution was announced"
        );
        assert_eq!(
            recorded_outcomes(&data, &control),
            Recorded::of(&call, ExecutionStatus::Succeeded, EffectState::KnownApplied),
            "the accepted grant keeps its recorded success"
        );
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);

        let snapshot = snapshot_of(&bed, &run, "second-grant").await;
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed)
        );
        let known = snapshot.known_outcomes();
        assert_eq!(known.len(), 1, "one known outcome: {known:?}");
        assert_eq!(known[0].call, call);
        assert_eq!(
            known[0].status,
            ExecutionStatus::Succeeded,
            "a late refusal never rewrites the outcome"
        );
    });
}

/// Cell **late grant**: a grant replayed after the terminal event is
/// `AlreadyFinalized`, dispatches nothing, and leaves the retained outcome
/// untouched.
#[test]
fn approve_after_the_run_finished_is_already_finalized() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut bed = common::make_bed(mutation_script(), common::quick_config());
        let run = submit_run(&bed, "late-grant").await;
        let (requested, notice) = approval_request(&mut bed).await;
        let (approval, call) = (notice.approval.clone(), notice.call.clone());

        // Resolve the grant honestly first, so the run reaches a terminal with
        // a retained record and the frontend still holds a grant it can replay.
        let denied = bed
            .runtime
            .deny(DenyCommand {
                request: RequestId::new("req-late-grant-first").expect("valid"),
                approval: approval.clone(),
                run: run.clone(),
                call: call.clone(),
            })
            .await;
        assert_eq!(denied.reply(), CommandReply::Accepted);
        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(requested);
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(
            recorded_outcomes(&data, &control),
            Recorded::of(&call, ExecutionStatus::Denied, EffectState::NotStarted)
        );

        let late = approve(&bed, &run, &approval, &call, "late-grant-replay").await;
        assert_eq!(
            late.reply(),
            CommandReply::AlreadyFinalized,
            "a finalized run reports its terminal state, not a stale target"
        );
        assert_eq!(late.run(), Some(&run));
        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "a finalized run never dispatches a replayed grant"
        );
        assert_eq!(bed.read_tool.execution_count(), 0);
        assert!(
            started_calls(&data, &control).is_empty(),
            "a finalized run announces no execution"
        );
        assert_eq!(
            recorded_outcomes(&data, &control),
            Recorded::of(&call, ExecutionStatus::Denied, EffectState::NotStarted),
            "the retained denial is not rewritten by the replayed grant"
        );
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);

        let snapshot = snapshot_of(&bed, &run, "late-grant").await;
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed)
        );
        assert!(
            snapshot.pending_approvals().is_empty(),
            "a finalized run keeps no pending grant: {:?}",
            snapshot.pending_approvals()
        );
        let known = snapshot.known_outcomes();
        assert_eq!(known.len(), 1, "one known outcome: {known:?}");
        assert_eq!(known[0].call, call);
        assert_eq!(known[0].status, ExecutionStatus::Denied);
        assert_eq!(known[0].effect, EffectState::NotStarted);
    });
}

/// Cell **continuation**: a denial is a recorded result, not a dead end. The
/// run requests the next model turn, hands it the denial, and finalizes
/// honestly instead of stalling on the refusal.
#[test]
fn a_denied_call_still_lets_the_run_continue_to_the_next_turn() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut bed = common::make_bed(mutation_script(), common::quick_config());
        let run = submit_run(&bed, "deny-continues").await;
        let (requested, notice) = approval_request(&mut bed).await;
        let (approval, call) = (notice.approval.clone(), notice.call.clone());

        let denied = bed
            .runtime
            .deny(DenyCommand {
                request: RequestId::new("req-deny-continues").expect("valid"),
                approval: approval.clone(),
                run: run.clone(),
                call: call.clone(),
            })
            .await;
        assert_eq!(denied.reply(), CommandReply::Accepted);
        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(requested);

        assert_eq!(
            finished.outcome(),
            RunOutcome::Completed,
            "a refusal is a normal result, not a run failure"
        );
        let requests = bed.provider.requests();
        assert_eq!(
            requests.len(),
            2,
            "the follow-up turn is requested exactly once"
        );
        assert_ne!(
            turn_of(&requests, 1),
            turn_of(&requests, 0),
            "the follow-up is a distinct model turn"
        );
        assert!(
            tool_results_in(&requests, 0).is_empty(),
            "no result exists before the call was decided"
        );
        assert_eq!(
            tool_results_in(&requests, 1),
            Recorded::of(&call, ExecutionStatus::Denied, EffectState::NotStarted),
            "the denial is carried into the next turn instead of stalling it"
        );
        assert_eq!(
            bed.provider.call_count(),
            2,
            "the scripted follow-up turn is served once"
        );
        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "continuing past a denial never executes the refused call"
        );
        assert_eq!(bed.read_tool.execution_count(), 0);
        assert_eq!(
            recorded_outcomes(&data, &control),
            Recorded::of(&call, ExecutionStatus::Denied, EffectState::NotStarted),
            "the refusal is recorded exactly once"
        );
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);

        let snapshot = snapshot_of(&bed, &run, "deny-continues").await;
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed)
        );
        let known = snapshot.known_outcomes();
        assert_eq!(known.len(), 1, "one known outcome: {known:?}");
        assert_eq!(known[0].call, call);
        assert_eq!(known[0].status, ExecutionStatus::Denied);
    });
}
