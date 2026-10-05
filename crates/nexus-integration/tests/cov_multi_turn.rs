#![forbid(unsafe_code)]

//! Multi-turn run coverage against the real runtime and the real fakes.
//!
//! Invariants under test:
//! - the model/tool/model cycle: an automatic read needs no grant, the loop
//!   re-enters the model with the recorded result, and the stop turn
//!   completes the run;
//! - a model-directed mutation never starts on the model's word alone: the run
//!   parks on an exact approval request, only the grant dispatches that call,
//!   and the cycle still completes afterwards;
//! - without an approval handler the automatic read still proceeds while the
//!   mutation is denied explicitly, so a turn never blocks on a decision that
//!   cannot arrive;
//! - every model turn of a cycle owns a distinct turn identity, on the request
//!   the provider observed and on the presentation fragments that turn
//!   published, and each follow-up turn carries the earlier results once;
//! - a whole cycle publishes exactly one terminal event, published last, with
//!   contiguous sequences and one outcome per started call.
//!
//! Determinism: only the shared bounded drains wait for runtime progress, and
//! each is a failure backstop rather than a scheduling assumption. Every
//! assertion reads recorded invariants (call and turn identities, counts,
//! sequence order, terminal outcomes), never elapsed wall-clock time.

mod common;

use std::collections::HashSet;

use nexus_core::{
    ApproveCommand, CallCandidate, CallId, CommandReply, EffectState, EventPayload,
    ExecutionStatus, FinishReason, GetSnapshotCommand, ModelContextItem, ModelRequest,
    ProviderEvent, RequestId, RunEvent, RunId, RunLifecycle, RunOutcome, TurnFinished, TurnId,
    Usage, UsageFinality,
};
use nexus_fakes::{stop_turn, tool_turn};

/// One tool-call turn that also publishes assistant text, so the turn
/// identity is observable on the presentation channel.
///
/// The candidate must carry its own turn-local item key: reusing the text
/// key would be a text/call collision the protocol rejects.
fn text_tool_turn(text: &str, candidate: CallCandidate) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::TextDelta {
            item_key: "item-0".to_owned(),
            text: text.to_owned(),
        },
        ProviderEvent::ToolCallReady(candidate),
        ProviderEvent::TurnFinished(TurnFinished::new(
            FinishReason::ToolCalls,
            Usage::new(None, None, UsageFinality::Final),
            None,
        )),
    ]
}

/// Host calls that entered execution, in publication order.
fn started_calls(control: &[RunEvent]) -> Vec<CallId> {
    control
        .iter()
        .filter_map(|event| match event.payload() {
            EventPayload::ToolStarted(info) => Some(info.call.clone()),
            _ => None,
        })
        .collect()
}

/// Recorded call outcomes as (call, status, effect) in publication order.
fn recorded_outcomes(control: &[RunEvent]) -> Vec<(CallId, ExecutionStatus, EffectState)> {
    control
        .iter()
        .filter_map(|event| match event.payload() {
            EventPayload::ToolFinished(info) => Some((
                info.call.clone(),
                info.outcome.status(),
                info.outcome.effect(),
            )),
            _ => None,
        })
        .collect()
}

/// The call identities of the results one model request carries.
fn recorded_calls(outcomes: &[(CallId, ExecutionStatus, EffectState)]) -> Vec<CallId> {
    outcomes.iter().map(|(call, _, _)| call.clone()).collect()
}

/// Turn identities of the observed model requests, in invocation order.
fn request_turns(requests: &[ModelRequest]) -> Vec<TurnId> {
    requests
        .iter()
        .map(|request| request.turn().clone())
        .collect()
}

/// Assistant calls one model request carries, in request order.
fn carried_calls(request: &ModelRequest) -> Vec<CallId> {
    request
        .conversation()
        .iter()
        .filter_map(|item| match item {
            ModelContextItem::AssistantCall { call, .. } => Some(call.call().clone()),
            _ => None,
        })
        .collect()
}

/// Recorded tool results one model request carries, in request order.
fn carried_results(request: &ModelRequest) -> Vec<(CallId, ExecutionStatus)> {
    request
        .conversation()
        .iter()
        .filter_map(|item| match item {
            ModelContextItem::ToolResult { call, outcome, .. } => {
                Some((call.clone(), outcome.status()))
            }
            _ => None,
        })
        .collect()
}

/// Presentation text per observed turn, ordered by sequence with each turn's
/// fragments merged.
///
/// Only adjacent same-turn fragments merge, so a turn whose fragments were
/// separated by another turn's would appear twice instead of hiding the
/// interleaving.
fn text_by_turn(data: &[RunEvent]) -> Vec<(TurnId, String)> {
    let mut ordered: Vec<&RunEvent> = data.iter().collect();
    ordered.sort_by_key(|event| event.seq());
    let mut merged: Vec<(TurnId, String)> = Vec::new();
    for event in ordered {
        let EventPayload::AssistantTextDelta(fragment) = event.payload() else {
            continue;
        };
        match merged.last_mut() {
            Some((turn, text)) if turn == &fragment.turn => text.push_str(&fragment.text),
            _ => merged.push((fragment.turn.clone(), fragment.text.clone())),
        }
    }
    merged
}

/// Sequence number of the earliest matching event, for ordering assertions
/// that must hold across both channels.
fn first_seq(events: &[RunEvent], matches: impl Fn(&EventPayload) -> bool) -> Option<u64> {
    events
        .iter()
        .filter(|event| matches(event.payload()))
        .map(|event| event.seq())
        .min()
}

/// Sequence number of the one terminal event, across both channels.
fn terminal_seq(data: &[RunEvent], control: &[RunEvent]) -> u64 {
    let mut seqs: Vec<u64> = data
        .iter()
        .chain(control.iter())
        .filter(|event| event.is_terminal())
        .map(|event| event.seq())
        .collect();
    assert_eq!(seqs.len(), 1, "exactly one terminal event: {seqs:?}");
    seqs.remove(0)
}

/// Highest sequence published across both channels.
fn highest_seq(data: &[RunEvent], control: &[RunEvent]) -> u64 {
    data.iter()
        .chain(control.iter())
        .map(|event| event.seq())
        .max()
        .expect("a run publishes events")
}

/// Every published event belongs to the accepted run.
fn assert_owned_by_run(run: &RunId, data: &[RunEvent], control: &[RunEvent]) {
    for event in data.iter().chain(control.iter()) {
        assert_eq!(
            event.run(),
            run,
            "every published event owns the accepted run"
        );
    }
}

/// The model/tool/model cycle: an automatic read dispatches without any grant,
/// the loop re-enters the model once, and the stop turn completes the run with
/// one terminal and contiguous sequences.
#[test]
fn model_tool_model_cycle_completes_with_automatic_read() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![common::candidate("host_read", r#"{"path":"src"}"#)]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("multi-turn-read"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response
            .run()
            .cloned()
            .expect("accepted submit issues a run");

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Completed,
            "the cycle completes on the stop turn"
        );
        assert_eq!(
            bed.provider.call_count(),
            2,
            "exactly one provider invocation per model turn"
        );
        assert_eq!(
            bed.read_tool.execution_count(),
            1,
            "the automatic read dispatches exactly once"
        );
        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "no mutation was proposed"
        );
        assert!(
            common::all_approvals(&control).is_empty(),
            "a policy-automatic read never asks for a grant"
        );
        assert_owned_by_run(&run, &data, &control);
        common::assert_contiguous(&data, &control);

        let started = started_calls(&control);
        assert_eq!(started.len(), 1, "one dispatched call in the cycle");
        let recorded = recorded_outcomes(&control);
        assert_eq!(
            recorded_calls(&recorded),
            started,
            "every started call has exactly one recorded outcome"
        );
        assert!(
            recorded
                .iter()
                .all(|(_, status, _)| *status == ExecutionStatus::Succeeded),
            "the automatic read is observed as a success: {recorded:?}"
        );
        common::assert_single_terminal(&data, &control);
    });
}

/// A mutation is never dispatched on the model's word alone: the run parks on
/// an exact approval request, the grant starts exactly that call, and the
/// cycle still completes afterwards with one terminal.
#[test]
fn mutation_requires_grant_before_dispatch_then_cycle_completes() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![common::candidate("host_write", r#"{"path":"src"}"#)]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("multi-turn-grant"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response
            .run()
            .cloned()
            .expect("accepted submit issues a run");

        let approvals = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let (approval, call) =
            common::find_approval(&approvals).expect("the mutation is offered for a grant");
        // The run driver is parked in its approval wait; on the
        // current-thread runtime it cannot be scheduled between ready awaits,
        // so neither the undecided call nor another model turn has started.
        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "the undecided mutation never dispatches"
        );
        assert_eq!(
            bed.provider.call_count(),
            1,
            "no model turn is requested while a call is undecided"
        );

        let approve = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-multi-turn-grant").expect("valid"),
                approval,
                run: run.clone(),
                call: call.clone(),
            })
            .await;
        assert_eq!(approve.reply(), CommandReply::Accepted);

        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(approvals);

        assert_eq!(
            finished.outcome(),
            RunOutcome::Completed,
            "the granted cycle completes on the stop turn"
        );
        assert_eq!(
            bed.write_tool.execution_count(),
            1,
            "the grant dispatches the mutation exactly once"
        );
        assert_eq!(
            bed.read_tool.execution_count(),
            0,
            "no automatic call was proposed"
        );
        assert_eq!(
            bed.provider.call_count(),
            2,
            "the grant resumes the loop with exactly one more model turn"
        );
        assert_eq!(
            common::all_approvals(&control).len(),
            1,
            "the cycle asks for a grant exactly once"
        );
        assert_owned_by_run(&run, &data, &control);
        common::assert_contiguous(&data, &control);

        let requested = first_seq(&control, |payload| {
            matches!(payload, EventPayload::ApprovalRequired(_))
        })
        .expect("approval request published");
        let started = first_seq(&control, |payload| {
            matches!(payload, EventPayload::ToolStarted(_))
        })
        .expect("dispatch published");
        assert!(
            requested < started,
            "the grant request precedes dispatch: request {requested}, dispatch {started}"
        );

        assert_eq!(
            started_calls(&control),
            vec![call.clone()],
            "the approved call is the only dispatch"
        );
        let recorded = recorded_outcomes(&control);
        assert_eq!(
            recorded_calls(&recorded),
            vec![call.clone()],
            "the recorded outcome correlates to the approved call"
        );
        assert_eq!(
            recorded,
            vec![(call, ExecutionStatus::Succeeded, EffectState::KnownApplied)],
            "the granted mutation is recorded as applied"
        );
        common::assert_single_terminal(&data, &control);
    });
}

/// With no approval handler a turn must not block on a decision that cannot
/// arrive: the automatic read still proceeds, the mutation is denied
/// explicitly at admission (never started), and the run completes honestly.
#[test]
fn automatic_read_proceeds_while_mutation_is_denied_without_a_handler() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![
                common::candidate_with("item-0", "prov-ref-0", "host_read", r#"{"path":"src"}"#),
                common::candidate_with("item-1", "prov-ref-1", "host_write", r#"{"path":"src"}"#),
            ]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, common::auto_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("multi-turn-headless"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response
            .run()
            .cloned()
            .expect("accepted submit issues a run");

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Completed,
            "an undecidable turn is denied, not fatal to the run"
        );
        assert_eq!(
            bed.read_tool.execution_count(),
            1,
            "the automatic read proceeds beside the denied mutation"
        );
        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "the mutation never dispatches"
        );
        assert_eq!(
            bed.provider.call_count(),
            2,
            "the denial costs no extra model turn"
        );
        let approvals = common::all_approvals(&control);
        assert!(
            approvals.is_empty(),
            "no approval is requested when no handler exists: {approvals:?}"
        );
        assert_owned_by_run(&run, &data, &control);
        common::assert_contiguous(&data, &control);

        let started = started_calls(&control);
        assert_eq!(started.len(), 1, "only the automatic read started");
        let recorded = recorded_outcomes(&control);
        assert_eq!(recorded.len(), 2, "both candidates have an outcome");
        assert_eq!(
            recorded[0],
            (
                started[0].clone(),
                ExecutionStatus::Succeeded,
                EffectState::KnownNotApplied
            ),
            "the automatic read is recorded as a success"
        );
        assert_ne!(
            recorded[1].0, started[0],
            "the denied mutation is a different call than the read"
        );
        assert_eq!(
            recorded[1].1,
            ExecutionStatus::Denied,
            "the mutation is denied explicitly"
        );
        assert_eq!(
            recorded[1].2,
            EffectState::NotStarted,
            "a denial claims no effect"
        );
        let requested = first_seq(&control, |payload| {
            matches!(payload, EventPayload::ApprovalRequired(_))
        });
        assert!(requested.is_none(), "no approval is ever requested");
        common::assert_single_terminal(&data, &control);
    });
}

/// Every model turn of a cycle owns a distinct turn identity: the provider
/// observes one per invocation, the presentation fragments carry the identity
/// of the turn that published them, and each follow-up turn carries the
/// earlier calls and results exactly once.
#[test]
fn multi_turn_cycle_mints_distinct_turn_identities() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            text_tool_turn(
                "read first ",
                common::candidate_with("item-1", "prov-ref-0", "host_read", r#"{"path":"src"}"#),
            ),
            text_tool_turn(
                "read second ",
                common::candidate_with("item-1", "prov-ref-1", "host_read", r#"{"path":"other"}"#),
            ),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("multi-turn-identity"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response
            .run()
            .cloned()
            .expect("accepted submit issues a run");

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Completed,
            "the three-turn cycle completes on the stop turn"
        );
        assert_owned_by_run(&run, &data, &control);
        common::assert_contiguous(&data, &control);
        assert_eq!(
            bed.read_tool.execution_count(),
            2,
            "one automatic read per tool turn"
        );

        let requests = bed.provider.requests();
        let turns = request_turns(&requests);
        assert_eq!(turns.len(), 3, "one request per model turn");
        for (index, request) in requests.iter().enumerate() {
            assert_eq!(request.run(), &run, "request {index} owns the accepted run");
        }
        for (index, turn) in turns.iter().enumerate() {
            for other in &turns[index + 1..] {
                assert_ne!(turn, other, "each model turn has its own identity");
            }
        }

        let published = text_by_turn(&data);
        assert_eq!(
            published.len(),
            turns.len(),
            "each turn publishes its own presentation text: {published:?}"
        );
        let expected_text = ["read first ", "read second ", "done"];
        for (index, ((turn, text), expected)) in
            published.iter().zip(expected_text.iter()).enumerate()
        {
            assert_eq!(turn, &turns[index], "text {index} belongs to its turn");
            assert_eq!(text, expected, "turn {index} keeps its own text");
        }

        // Each follow-up turn carries the earlier calls and their recorded
        // results exactly once, correlated to the observed host calls.
        let started = started_calls(&control);
        assert_eq!(started.len(), 2, "both reads dispatched");
        assert!(started[0] != started[1], "each read is its own call");
        assert!(
            carried_calls(&requests[0]).is_empty(),
            "the first turn carries no prior call: {:?}",
            requests[0].conversation()
        );
        assert!(
            carried_results(&requests[0]).is_empty(),
            "no result exists before the first turn: {:?}",
            requests[0].conversation()
        );
        assert_eq!(
            carried_calls(&requests[1]),
            vec![started[0].clone()],
            "the second turn carries the first call once"
        );
        assert_eq!(
            carried_results(&requests[1]),
            vec![(started[0].clone(), ExecutionStatus::Succeeded)],
            "the second turn carries the first result once"
        );
        assert_eq!(
            carried_calls(&requests[2]),
            vec![started[0].clone(), started[1].clone()],
            "the third turn carries both calls, in order"
        );
        assert_eq!(
            carried_results(&requests[2]),
            vec![
                (started[0].clone(), ExecutionStatus::Succeeded),
                (started[1].clone(), ExecutionStatus::Succeeded)
            ],
            "the third turn carries both results, in order"
        );
        common::assert_single_terminal(&data, &control);
    });
}

/// A longer cycle still publishes exactly one terminal: it is the last
/// sequence, every started call has one outcome, and the snapshot agrees that
/// the run finalized with that terminal.
#[test]
fn multi_turn_cycle_publishes_exactly_one_terminal_last() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![common::candidate_with(
                "item-0",
                "prov-ref-0",
                "host_read",
                r#"{"path":"a"}"#,
            )]),
            tool_turn(vec![common::candidate_with(
                "item-1",
                "prov-ref-1",
                "host_read",
                r#"{"path":"b"}"#,
            )]),
            tool_turn(vec![common::candidate_with(
                "item-2",
                "prov-ref-2",
                "host_read",
                r#"{"path":"c"}"#,
            )]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("multi-turn-terminal"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response
            .run()
            .cloned()
            .expect("accepted submit issues a run");

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Completed,
            "the four-turn cycle completes on the stop turn"
        );
        assert_eq!(bed.provider.call_count(), 4, "one invocation per turn");
        assert_eq!(
            bed.read_tool.execution_count(),
            3,
            "each tool turn dispatched its read"
        );
        assert_owned_by_run(&run, &data, &control);
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);

        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::RunStarted { .. }
            )),
            1,
            "the cycle opens with exactly one run-started"
        );
        let terminal = terminal_seq(&data, &control);
        assert_eq!(
            highest_seq(&data, &control),
            terminal,
            "the terminal is the last published sequence"
        );

        let started = started_calls(&control);
        assert_eq!(started.len(), 3, "one start per dispatched call");
        let distinct: HashSet<&CallId> = started.iter().collect();
        assert_eq!(
            distinct.len(),
            started.len(),
            "every dispatched call has its own identity: {started:?}"
        );
        let recorded = recorded_outcomes(&control);
        assert_eq!(
            recorded_calls(&recorded),
            started,
            "every started call has exactly one recorded outcome"
        );
        assert!(
            recorded
                .iter()
                .all(|(_, status, _)| *status == ExecutionStatus::Succeeded),
            "every read is observed as a success: {recorded:?}"
        );

        let (_, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-multi-turn-snap").expect("valid"),
                run: run.clone(),
            })
            .await;
        let snapshot = snapshot.expect("a finalized run keeps a snapshot");
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed),
            "the snapshot agrees with the terminal outcome"
        );
        assert_eq!(
            snapshot.last_sequence(),
            Some(terminal),
            "the snapshot names the terminal as the last sequence"
        );
        assert_eq!(
            snapshot.known_outcomes().len(),
            3,
            "the snapshot keeps one summary per dispatched call"
        );
        assert!(
            snapshot.pending_approvals().is_empty(),
            "the finalized run keeps no pending approvals: {:?}",
            snapshot.pending_approvals()
        );
    });
}
