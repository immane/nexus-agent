#![forbid(unsafe_code)]

//! Cancellation matrix against the real runtime and the real fakes.
//!
//! Cancellation is cooperative, so the interesting question is not *whether*
//! a cancel lands but *which phase* it lands in and what that phase then
//! records. The rows, one per test:
//! - **during a provider turn**: the worker owns `stream`; the run finalizes
//!   as cancelled without ingesting the late batch, no call dispatches, and
//!   the still-blocked worker keeps ownership of the run slot until it
//!   actually terminates;
//! - **during tool execution**: the worker owns `execute`; the run finalizes
//!   promptly, the call dispatches exactly once, and the published outcome is
//!   the honest `Cancelled`/`Unknown`/`Uncertain` triple, never a rewritten
//!   success and never a rollback claim;
//! - **while awaiting approval**: the driver is parked in its approval wait and
//!   no worker owns anything; the undecided call never dispatches, no second
//!   turn is consumed, the abandoned approval leaves the pending set, and the
//!   call still records one honest outcome rather than a denial nobody decided;
//! - **duplicate cancel**: repeated cancels against a live run are all
//!   accepted and never double-dispatch, double-record, or double-publish a
//!   terminal;
//! - **after the terminal**: `AlreadyFinalized` (naming the run), which is a
//!   different answer from the `StaleOrUnknownTarget` an unknown run gets;
//! - **honest outcomes**: an interrupted call is never reported with a claimed
//!   effect, and late evidence from a terminated worker refines only the
//!   retained record without republishing an event.
//!
//! Determinism: no test races a fixed delay. The provider and tool phases are
//! held by gated doubles (`nexus_fakes::FakeProvider::gated` /
//! `FakeTool::gated`, and the test-local `gates::GatedTool`), which announce
//! entry before the worker blocks, so a cancel provably lands while the named
//! phase still owns the runtime. Every wait is a bounded backstop over a
//! recorded state, never an assertion about elapsed time.

mod common;

#[path = "review_gates/mod.rs"]
mod gates;

use std::sync::Arc;
use std::time::Duration;

use gates::{GatedTool, review_bed};
use nexus_core::{
    ApproveCommand, CallId, CancelCommand, CommandReply, CommandResponse, EffectState,
    EventPayload, Evidence, ExecutionStatus, GetSnapshotCommand, OutcomeSummary, ProviderEvent,
    RequestId, RunEvent, RunId, RunLifecycle, RunOutcome, Snapshot, ToolOutcome, ToolPort,
};
use nexus_fakes::{FakeGate, FakeProvider, FakeTool, stop_turn, tool_turn};
use nexus_runtime::Runtime;

/// Bounded backstop for every wait below. Passing runs settle in
/// milliseconds; a genuine hang fails here instead of stalling the suite.
const WAIT_BUDGET: Duration = Duration::from_secs(10);

/// Builds a correlated cancel for `run` with its own request identity.
fn cancel_cmd(tag: &str, run: &RunId) -> CancelCommand {
    CancelCommand {
        request: RequestId::new(format!("req-{tag}")).expect("request builds"),
        run: run.clone(),
    }
}

/// Cancels a live run and asserts the accepted reply names that run.
async fn cancel_live(runtime: &Runtime, tag: &str, run: &RunId) {
    let response = runtime.cancel(cancel_cmd(tag, run)).await;
    assert_eq!(
        response.reply(),
        CommandReply::Accepted,
        "{tag}: a cancel against a live run is accepted"
    );
    assert_eq!(
        response.run(),
        Some(run),
        "{tag}: the accepted cancel names the run it stopped"
    );
}

/// A bed over the shared gated provider double, plus that provider's gate.
///
/// `common::make_bed` builds its own ungated provider, so this wires the
/// gated one directly while keeping the shared registration order
/// (`host_read`, `host_write`) and headless denial semantics.
fn gated_provider_bed(script: Vec<Vec<ProviderEvent>>) -> (common::Bed, FakeGate) {
    let provider = Arc::new(FakeProvider::gated(script));
    let gate = provider.gate().expect("gated provider exposes its gate");
    let read_tool = Arc::new(FakeTool::read_only());
    let write_tool = Arc::new(FakeTool::mutation());
    let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![read_tool.clone(), write_tool.clone()];
    let (runtime, streams) = Runtime::new(common::auto_config(), provider.clone(), tools);
    (
        common::Bed {
            runtime,
            data: streams.data,
            control: streams.control,
            provider,
            read_tool,
            write_tool,
        },
        gate,
    )
}

/// A gate-tool bed over the shared scripted provider, plus the tool's entry
/// channel. The gate tool answers with its own observed outcome regardless of
/// the live token, which is how the late-evidence row is exercised.
fn gated_mutation_bed(
    provider_script: Vec<Vec<ProviderEvent>>,
) -> (
    gates::ReviewBed,
    Arc<GatedTool>,
    tokio::sync::mpsc::UnboundedReceiver<gates::ToolEntry>,
) {
    let (tool, entries) = GatedTool::new("host_write", "write ok", true, true);
    let tool = Arc::new(tool);
    let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![tool.clone()];
    let provider = Arc::new(FakeProvider::new(provider_script));
    (
        review_bed(common::quick_config(), provider, tools),
        tool,
        entries,
    )
}

/// Waits (bounded) until `ready` holds.
async fn wait_until(label: &str, mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(WAIT_BUDGET, async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{label} within the wait budget"));
}

/// Submits until the runtime accepts, proving the quarantine cleared and no
/// worker still owns the run slot.
async fn submit_when_free(runtime: &Runtime, tag: &str) -> CommandResponse {
    tokio::time::timeout(WAIT_BUDGET, async {
        loop {
            let response = runtime.submit(common::submit_cmd(tag)).await;
            if response.reply() == CommandReply::Accepted {
                return response;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the run slot frees once every quarantined worker terminates")
}

/// Reads a known run's snapshot; the runtime must still know it.
async fn snapshot_of(runtime: &Runtime, run: &RunId, tag: &str) -> Snapshot {
    let (reply, snapshot) = runtime
        .get_snapshot(GetSnapshotCommand {
            request: RequestId::new(format!("req-{tag}")).expect("request builds"),
            run: run.clone(),
        })
        .await;
    assert_eq!(
        reply.reply(),
        CommandReply::Accepted,
        "{tag}: a known run keeps a snapshot"
    );
    snapshot.expect("a known run keeps a snapshot")
}

/// Polls a run's snapshot until `ready` accepts its known outcomes.
async fn snapshot_when(
    runtime: &Runtime,
    run: &RunId,
    mut ready: impl FnMut(&[OutcomeSummary]) -> bool,
) -> Snapshot {
    tokio::time::timeout(WAIT_BUDGET, async {
        loop {
            let (reply, snapshot) = runtime
                .get_snapshot(GetSnapshotCommand {
                    request: RequestId::new(format!("req-snap-{}", run.as_str()))
                        .expect("request builds"),
                    run: run.clone(),
                })
                .await;
            assert_eq!(reply.reply(), CommandReply::Accepted, "known run");
            let snapshot = snapshot.expect("a known run keeps a snapshot");
            if ready(snapshot.known_outcomes()) {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the retained snapshot reaches the expected record")
}

/// Collects `ToolStarted` call identities in arrival order.
fn started_calls(events: &[RunEvent]) -> Vec<CallId> {
    events
        .iter()
        .filter_map(|event| match event.payload() {
            EventPayload::ToolStarted(info) => Some(info.call.clone()),
            _ => None,
        })
        .collect()
}

/// Collects `ToolFinished` records in arrival order across both channels.
fn finished_calls(data: &[RunEvent], control: &[RunEvent]) -> Vec<(CallId, ToolOutcome)> {
    data.iter()
        .chain(control.iter())
        .filter_map(|event| match event.payload() {
            EventPayload::ToolFinished(info) => Some((info.call.clone(), info.outcome.clone())),
            _ => None,
        })
        .collect()
}

/// Asserts the honest cancelled triple. A cancelled attempt has an unknown
/// effect and uncertain evidence: never a claimed rollback, never a fabricated
/// success, never host-observed certainty.
fn assert_honest_cancelled(outcome: &ToolOutcome) {
    assert_eq!(outcome.status(), ExecutionStatus::Cancelled);
    assert_eq!(
        outcome.effect(),
        EffectState::Unknown,
        "a cancelled attempt has an unknown effect, never a known one"
    );
    assert_eq!(
        outcome.evidence(),
        Evidence::Uncertain,
        "a cancelled attempt has uncertain evidence, never host-observed"
    );
}

/// Asserts no call records two published outcomes. This holds for every
/// cancellation phase, including ones where a call never started: an admitted
/// call may record a pre-dispatch outcome (an abandoned approval records a
/// cancelled one), but never a second one.
fn assert_no_duplicate_outcomes(data: &[RunEvent], control: &[RunEvent]) {
    let mut recorded: Vec<CallId> = finished_calls(data, control)
        .into_iter()
        .map(|(call, _)| call)
        .collect();
    recorded.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let unique = {
        let mut deduped = recorded.clone();
        deduped.dedup();
        deduped.len()
    };
    assert_eq!(
        unique,
        recorded.len(),
        "no call records two outcomes: {recorded:?}"
    );
}

/// Row: cancel during a provider turn.
///
/// The provider worker owns `stream` when the cancel lands. The run finalizes
/// as cancelled, the scripted batch is never ingested (no call dispatches),
/// the worker keeps ownership of the run slot until it terminates, and the
/// released worker observes cancellation on its live token instead of serving
/// its scripted stop turn.
#[test]
fn cancel_during_provider_turn_never_ingests_the_late_batch() {
    let rt = common::test_rt();
    rt.block_on(async {
        let (mut bed, gate) = gated_provider_bed(vec![stop_turn("late"), stop_turn("next")]);
        let response = bed
            .runtime
            .submit(common::submit_cmd("provider-cancel"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        wait_until("provider worker enters stream", || gate.is_entered()).await;
        cancel_live(&bed.runtime, "provider-cancel", &run).await;

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Cancelled,
            "cancelling a live provider turn ends the run as cancelled"
        );
        assert!(
            started_calls(&control).is_empty(),
            "a cancelled provider turn dispatches nothing: {:?}",
            started_calls(&control)
        );
        assert!(
            finished_calls(&data, &control).is_empty(),
            "no call was admitted, so no tool outcome is recorded"
        );
        assert_eq!(bed.read_tool.execution_count(), 0);
        assert_eq!(bed.write_tool.execution_count(), 0);
        common::assert_single_terminal(&data, &control);
        common::assert_contiguous(&data, &control);

        // The run is already retired, even though its worker is still live.
        let late = bed.runtime.cancel(cancel_cmd("provider-late", &run)).await;
        assert_eq!(
            late.reply(),
            CommandReply::AlreadyFinalized,
            "a cancel after the provider-turn terminal is finalized"
        );
        assert_eq!(late.run(), Some(&run), "the finalized reply names the run");

        // The blocked worker still owns the run slot.
        let blocked = bed
            .runtime
            .submit(common::submit_cmd("provider-quarantine"))
            .await;
        assert_eq!(
            blocked.reply(),
            CommandReply::Busy,
            "a blocked provider worker retains ownership of the run slot"
        );
        assert_eq!(
            blocked.run(),
            Some(&run),
            "busy names the retired run the worker still owns"
        );

        // Releasing the worker lets it observe the live cancellation and
        // terminate; only then does the quarantine clear.
        gate.release();
        let next = submit_when_free(&bed.runtime, "provider-after-release").await;
        let next_run = next.run().cloned().expect("next run issued");
        let (data, control, next_finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            next_finished.outcome(),
            RunOutcome::Completed,
            "the run after the released worker completes normally"
        );
        assert!(
            data.iter().chain(control.iter()).any(|event| {
                event.run() == &next_run
                    && matches!(
                        event.payload(),
                        EventPayload::RunFinished(_) | EventPayload::UsageUpdated(_)
                    )
            }),
            "the follow-up run publishes its own events under its own identity"
        );
        assert_eq!(
            bed.provider.call_count(),
            2,
            "the cancelled invocation served no scripted turn"
        );
    });
}

/// Row: cancel during tool execution.
///
/// The tool worker owns `execute` when the cancel lands. The run finalizes
/// promptly as cancelled without waiting for the worker, the call dispatches
/// exactly once and records exactly one honest cancelled outcome, and the
/// retained snapshot carries that inconclusive record rather than a claim
/// about effects.
#[test]
fn cancel_during_tool_execution_records_one_honest_cancelled_outcome() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![common::candidate("host_read", r#"{"path":"src"}"#)]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed_with_tools(
            script,
            common::auto_config(),
            FakeTool::gated("host_read"),
            FakeTool::mutation(),
        );
        let response = bed.runtime.submit(common::submit_cmd("tool-cancel")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let gate = bed.read_tool.gate().expect("gated tool exposes its gate");
        wait_until("tool worker enters execute", || gate.is_entered()).await;
        assert_eq!(
            bed.read_tool.execution_count(),
            1,
            "the worker owns exactly one execution when the cancel lands"
        );

        cancel_live(&bed.runtime, "tool-cancel", &run).await;

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Cancelled,
            "cancelling a live tool execution ends the run as cancelled"
        );
        let recorded = finished_calls(&data, &control);
        let started = started_calls(&control);
        assert_eq!(
            started.len(),
            1,
            "the gated call started exactly once: {started:?}"
        );
        assert_eq!(recorded.len(), 1, "one start, one outcome");
        assert_eq!(recorded[0].0, started[0], "outcomes correlate to starts");
        assert_honest_cancelled(&recorded[0].1);
        assert_no_duplicate_outcomes(&data, &control);
        common::assert_single_terminal(&data, &control);
        common::assert_contiguous(&data, &control);

        // While the worker's termination is unconfirmed, the snapshot must not
        // upgrade the inconclusive record into a known effect.
        let snapshot = snapshot_of(&bed.runtime, &run, "tool-cancel-snap").await;
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Cancelled)
        );
        assert_eq!(
            snapshot.known_outcomes().len(),
            1,
            "exactly the interrupted call is known: {:?}",
            snapshot.known_outcomes()
        );
        let summary = &snapshot.known_outcomes()[0];
        assert_eq!(summary.call, started[0]);
        assert_eq!(summary.status, ExecutionStatus::Cancelled);
        assert_eq!(summary.effect, EffectState::Unknown);
        assert_eq!(summary.evidence, Evidence::Uncertain);

        // The interrupted worker is quarantined until it terminates.
        let blocked = bed
            .runtime
            .submit(common::submit_cmd("tool-quarantine"))
            .await;
        assert_eq!(
            blocked.reply(),
            CommandReply::Busy,
            "an interrupted tool worker retains ownership"
        );
        gate.release();
        let _ = submit_when_free(&bed.runtime, "tool-after-release").await;
        assert_eq!(
            bed.read_tool.execution_count(),
            1,
            "releasing the interrupted worker never re-dispatches it"
        );
        let (_, _, next_finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(next_finished.outcome(), RunOutcome::Completed);
    });
}

/// Row: cancel while awaiting approval.
///
/// The driver is parked in its approval wait, so no worker owns anything. The
/// undecided call never dispatches, the loop does not consume the next model
/// turn, the abandoned approval leaves the pending set, the call still records
/// exactly one outcome, and that outcome is the honest cancelled triple rather
/// than a denial nobody decided.
#[test]
fn cancel_while_awaiting_approval_never_dispatches_the_undecided_call() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![common::candidate("host_write", r#"{"path":"src"}"#)]),
            stop_turn("the next turn must never be requested"),
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
        let (approval, call) = common::find_approval(&approvals).expect("approval requested");
        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "the call is still undecided when the cancel lands"
        );

        cancel_live(&bed.runtime, "approval-cancel", &run).await;

        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(approvals);
        assert_eq!(
            finished.outcome(),
            RunOutcome::Cancelled,
            "cancelling during the approval wait ends the run as cancelled"
        );
        assert!(
            started_calls(&control).is_empty(),
            "an undecided call never starts: {:?}",
            started_calls(&control)
        );
        assert_eq!(bed.write_tool.execution_count(), 0);
        assert_eq!(
            bed.provider.call_count(),
            1,
            "cancellation stops the loop before the next model turn"
        );
        let recorded = finished_calls(&data, &control);
        assert_eq!(recorded.len(), 1, "the abandoned call records one outcome");
        assert_eq!(recorded[0].0, call, "the outcome correlates to the call");
        assert_honest_cancelled(&recorded[0].1);
        assert_no_duplicate_outcomes(&data, &control);
        common::assert_single_terminal(&data, &control);

        let snapshot = snapshot_of(&bed.runtime, &run, "approval-cancel-snap").await;
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Cancelled)
        );
        assert!(
            !snapshot.pending_approvals().contains(&approval),
            "the abandoned approval is no longer pending: {:?}",
            snapshot.pending_approvals()
        );
        assert!(
            snapshot.pending_approvals().is_empty(),
            "the abandoned approval leaves the pending set entirely"
        );

        // A grant arriving after the cancelled run cannot resurrect it.
        let late = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-approval-late").expect("valid"),
                approval,
                run: run.clone(),
                call,
            })
            .await;
        assert_eq!(
            late.reply(),
            CommandReply::AlreadyFinalized,
            "a grant against a cancelled run is finalized, never dispatching"
        );
        assert_eq!(bed.write_tool.execution_count(), 0);

        // No worker owned anything, so nothing is quarantined.
        let next = bed
            .runtime
            .submit(common::submit_cmd("approval-after-cancel"))
            .await;
        assert_eq!(
            next.reply(),
            CommandReply::Accepted,
            "an approval-wait cancel retains no worker ownership"
        );
        let (_, _, next_finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(next_finished.outcome(), RunOutcome::Completed);
    });
}

/// Row: duplicate cancel.
///
/// Five cancels against one live run are all accepted, while the interrupted
/// call dispatches once, records one outcome, and the run publishes exactly
/// one terminal. The turn proposed two calls: the second never starts, so the
/// duplicates cannot smuggle a second dispatch past the first.
#[test]
fn duplicate_cancel_is_accepted_repeatedly_but_dispatches_once() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![
                common::candidate_with("item-0", "prov-ref-0", "host_read", r#"{"path":"src"}"#),
                common::candidate_with("item-1", "prov-ref-1", "host_read", r#"{"path":"dst"}"#),
            ]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed_with_tools(
            script,
            common::auto_config(),
            FakeTool::gated("host_read"),
            FakeTool::mutation(),
        );
        let response = bed
            .runtime
            .submit(common::submit_cmd("duplicate-cancel"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let gate = bed.read_tool.gate().expect("gated tool exposes its gate");
        wait_until("tool worker enters execute", || gate.is_entered()).await;

        for attempt in 0..5 {
            cancel_live(&bed.runtime, &format!("dup-cancel-{attempt}"), &run).await;
        }

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Cancelled,
            "duplicate cancellation still finalizes as cancelled"
        );
        assert_eq!(
            bed.read_tool.execution_count(),
            1,
            "five cancels never dispatch the interrupted call twice"
        );
        let started = started_calls(&control);
        assert_eq!(
            started.len(),
            1,
            "the second queued call never starts under cancellation: {started:?}"
        );
        let recorded = finished_calls(&data, &control);
        assert_eq!(recorded.len(), 1, "one outcome, not one per cancel");
        assert_honest_cancelled(&recorded[0].1);
        assert_no_duplicate_outcomes(&data, &control);
        common::assert_single_terminal(&data, &control);

        let blocked = bed
            .runtime
            .submit(common::submit_cmd("dup-during-quarantine"))
            .await;
        assert_eq!(
            blocked.reply(),
            CommandReply::Busy,
            "duplicate cancellation retains worker ownership"
        );
        gate.release();
        let _ = submit_when_free(&bed.runtime, "dup-after-release").await;
        assert_eq!(
            bed.read_tool.execution_count(),
            1,
            "releasing the worker never re-dispatches the interrupted call"
        );
        let (_, _, next_finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(next_finished.outcome(), RunOutcome::Completed);
    });
}

/// Row: cancel after the terminal.
///
/// A retired run reports `AlreadyFinalized` and names the run; a run the
/// runtime never issued reports `StaleOrUnknownTarget` and names nothing. That
/// distinction is what lets a frontend tell "too late" from "never existed",
/// and it stays stable across repeats and while a later run is live.
#[test]
fn cancel_after_terminal_is_already_finalized_and_unknown_run_is_stale() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut bed = common::make_bed(
            vec![stop_turn("first"), stop_turn("second")],
            common::quick_config(),
        );
        let response = bed.runtime.submit(common::submit_cmd("finalized")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");
        let (_, _, finished) = common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Completed,
            "the baseline run completes, so this is not a cancellation row"
        );

        let late = bed.runtime.cancel(cancel_cmd("after-terminal", &run)).await;
        assert_eq!(
            late.reply(),
            CommandReply::AlreadyFinalized,
            "a cancel after a completed terminal reports finalized"
        );
        assert_eq!(
            late.run(),
            Some(&run),
            "the finalized reply names the retired run"
        );

        for attempt in 0..3 {
            let again = bed
                .runtime
                .cancel(cancel_cmd(&format!("after-terminal-{attempt}"), &run))
                .await;
            assert_eq!(
                again.reply(),
                CommandReply::AlreadyFinalized,
                "repeat late cancels keep reporting finalized"
            );
        }

        let unknown = RunId::new("run-never-issued").expect("valid");
        let stale = bed
            .runtime
            .cancel(cancel_cmd("unknown-run", &unknown))
            .await;
        assert_eq!(
            stale.reply(),
            CommandReply::StaleOrUnknownTarget,
            "a cancel against an unknown run is stale, not finalized"
        );
        assert_eq!(stale.run(), None, "a stale reply names no run");

        let next = bed
            .runtime
            .submit(common::submit_cmd("after-finalized"))
            .await;
        assert_eq!(
            next.reply(),
            CommandReply::Accepted,
            "the finalized run frees the slot"
        );
        let next_run = next.run().cloned().expect("second run issued");
        assert_ne!(next_run, run, "run identities never alias");

        // The retired run stays known while the new run is live: the runtime
        // retains the last finalized run, so the answer is still finalized
        // rather than stale.
        let still_late = bed.runtime.cancel(cancel_cmd("still-late", &run)).await;
        assert_eq!(
            still_late.reply(),
            CommandReply::AlreadyFinalized,
            "a retired run stays known as finalized while a new run is live"
        );
        assert_eq!(
            bed.runtime
                .cancel(cancel_cmd("live-cancel", &next_run))
                .await
                .reply(),
            CommandReply::Accepted,
            "the live run is still cancellable while the retired one is finalized"
        );
        let (_, _, next_finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            next_finished.outcome(),
            RunOutcome::Cancelled,
            "the live run stops as cancelled"
        );
    });
}

/// Row: late evidence refines only the retained record.
///
/// The gate tool answers with its own observed applied outcome regardless of
/// the live token. The published event stays the honest cancelled triple and
/// the run finalizes as cancelled, but once the worker terminates the retained
/// snapshot is refined to the worker's observed effect, without republishing a
/// `ToolFinished` or a second terminal.
#[test]
fn late_evidence_refines_the_retained_record_without_republishing() {
    let rt = common::test_rt();
    rt.block_on(async {
        let (mut bed, tool, mut entries) = gated_mutation_bed(vec![
            tool_turn(vec![common::candidate("host_write", r#"{"path":"src"}"#)]),
            stop_turn("done"),
        ]);
        let response = bed
            .runtime
            .submit(common::submit_cmd("observed-effect"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let approvals = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let (approval, call) = common::find_approval(&approvals).expect("approval requested");

        let approve = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-observed-approve").expect("valid"),
                approval,
                run: run.clone(),
                call: call.clone(),
            })
            .await;
        assert_eq!(approve.reply(), CommandReply::Accepted);

        // Entry is announced before the worker blocks, so the cancel below
        // lands while the tool phase owns the runtime.
        let entry = gates::next_entry(&mut entries).await;
        assert_eq!(
            entry.call.call(),
            &call,
            "the granted call is the one executing"
        );
        assert!(
            !entry.context.scope().as_str().is_empty(),
            "the dispatched context carries an approved scope"
        );

        cancel_live(&bed.runtime, "observed-effect", &run).await;
        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(approvals);
        assert_eq!(
            finished.outcome(),
            RunOutcome::Cancelled,
            "the run still ends as cancelled"
        );
        let recorded = finished_calls(&data, &control);
        assert_eq!(recorded.len(), 1, "exactly one published outcome");
        assert_eq!(recorded[0].0, call, "correlates to the granted call");
        assert_honest_cancelled(&recorded[0].1);
        assert_no_duplicate_outcomes(&data, &control);
        common::assert_single_terminal(&data, &control);

        let blocked = bed
            .runtime
            .submit(common::submit_cmd("observed-quarantine"))
            .await;
        assert_eq!(
            blocked.reply(),
            CommandReply::Busy,
            "the applied-but-unterminated worker still owns the run slot"
        );

        // Releasing the worker lets it report what it observed. The retained
        // record is refined; the published event stream is not.
        entry.release();
        let snapshot = snapshot_when(&bed.runtime, &run, |known| {
            known
                .iter()
                .any(|summary| summary.effect == EffectState::KnownApplied)
        })
        .await;
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Cancelled),
            "refinement never changes the lifecycle outcome"
        );
        let summary = snapshot
            .known_outcomes()
            .iter()
            .find(|summary| summary.call == call)
            .expect("the interrupted call stays in the retained record");
        assert_eq!(
            summary.status,
            ExecutionStatus::Succeeded,
            "the observed status replaces the inconclusive one"
        );
        assert_eq!(summary.effect, EffectState::KnownApplied);
        assert_eq!(summary.evidence, Evidence::HostObserved);
        assert_eq!(
            finished_calls(&data, &control)
                .iter()
                .filter(|(recorded, _)| recorded == &call)
                .count(),
            1,
            "refinement never republishes the outcome"
        );
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::RunFinished(_)
            )),
            1,
            "refinement never republishes a terminal"
        );
        assert_eq!(tool.execution_count(), 1, "one execution, one refinement");

        let next = bed
            .runtime
            .submit(common::submit_cmd("observed-after-release"))
            .await;
        assert_eq!(
            next.reply(),
            CommandReply::Accepted,
            "the slot frees once the worker terminates"
        );
        let (_, _, next_finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(next_finished.outcome(), RunOutcome::Completed);
    });
}

/// Row: a cancel never overwrites a non-cancelled terminal.
///
/// The run ends as failed on a provider protocol failure, and a cancel issued
/// afterwards is `AlreadyFinalized`: neither the reply nor the retained
/// snapshot turns the failure into a cancellation.
#[test]
fn cancel_never_overwrites_a_failed_terminal_outcome() {
    let rt = common::test_rt();
    rt.block_on(async {
        // A turn whose argument progress never yields a complete candidate:
        // the invocation fails, so the run ends failed rather than cancelled.
        let script = vec![vec![
            ProviderEvent::ToolCallDelta {
                item_key: "item-1".to_owned(),
                assembled_bytes: 8,
            },
            ProviderEvent::Failed(
                nexus_core::AgentError::new(
                    nexus_core::ErrorCategory::Protocol,
                    "malformed tool arguments",
                    nexus_core::RetryGuidance::DoNotRetry,
                )
                .expect("static safe fake message builds"),
            ),
        ]];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("failed-then-cancel"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Failed,
            "the provider protocol failure fails the run"
        );
        common::assert_single_terminal(&data, &control);
        assert!(
            started_calls(&control).is_empty(),
            "a failed provider turn dispatches nothing"
        );

        let late = bed.runtime.cancel(cancel_cmd("after-failed", &run)).await;
        assert_eq!(
            late.reply(),
            CommandReply::AlreadyFinalized,
            "a cancel after a failed terminal is finalized"
        );
        let snapshot = snapshot_of(&bed.runtime, &run, "failed-snap").await;
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Failed),
            "the retained snapshot keeps the failed outcome"
        );
    });
}
