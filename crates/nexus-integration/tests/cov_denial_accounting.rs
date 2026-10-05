#![forbid(unsafe_code)]

//! Denial-budget accounting across every refusal path.
//!
//! Findings under test:
//! - an unknown-tool candidate, an invalid-argument candidate, a call refused
//!   for want of an approval handler, and a call refused by an explicit `Deny`
//!   each consume exactly one slot of the per-run tool-call budget, so an
//!   adversarial turn cannot buy unbounded denials for free;
//! - once the budget is spent, further admission is refused: the runtime
//!   finalizes as [`RunOutcome::LimitReached`] carrying a
//!   [`ErrorCategory::ResourceLimit`] diagnostic, and the not-yet-admitted
//!   candidate never reaches a tool, an approval notice, or a dispatch;
//! - every refusal is recorded as `Denied` / [`EffectState::NotStarted`] /
//!   [`Evidence::HostObserved`]: nothing started, and the host observed the
//!   refusal directly. Denials are visible in the run's snapshot as well as on
//!   the control channel.
//!
//! Determinism: every run is bounded by the shared drain backstop and asserts
//! on recorded invariants (denial count and ordering, tool execution counts,
//! provider turn count, terminal outcome), never on wall-clock timing.

mod common;

use std::collections::HashSet;

use nexus_core::{
    CallId, CommandReply, DenyCommand, EffectState, ErrorCategory, EventPayload, Evidence,
    ExecutionStatus, GetSnapshotCommand, Limits, OutcomeSummary, RequestId, RunEvent, RunFinished,
    RunLifecycle, RunOutcome,
};
use nexus_fakes::{stop_turn, tool_turn};
use nexus_runtime::{Policy, RuntimeConfig};

/// One recorded refusal: the call identity plus its honest outcome triple.
type Denial = (CallId, ExecutionStatus, EffectState, Evidence);

fn config_with(limits: Limits, has_approval_handler: bool) -> RuntimeConfig {
    RuntimeConfig {
        limits,
        policy: Policy::m0_test(),
        has_approval_handler,
    }
}

/// Merges both channels into true per-run order.
///
/// Tests that answer an approval request before draining the remainder hold
/// those earlier events in a side vector; concatenating the two buffers would
/// otherwise report a plausible but wrong event order. Sorting by the
/// per-run sequence number restores the runtime's own ordering without
/// depending on channel interleaving.
fn ordered(data: &[RunEvent], control: &[RunEvent]) -> Vec<RunEvent> {
    let mut all: Vec<RunEvent> = data.iter().chain(control.iter()).cloned().collect();
    all.sort_by_key(RunEvent::seq);
    all
}

/// Every recorded `ToolFinished` outcome, in per-run order. Both channels are
/// inspected because a denial is a control event while any presentation
/// traffic sits on the data channel.
fn outcomes(events: &[RunEvent]) -> Vec<Denial> {
    events
        .iter()
        .filter_map(|event| match event.payload() {
            EventPayload::ToolFinished(info) => Some((
                info.call.clone(),
                info.outcome.status(),
                info.outcome.effect(),
                info.outcome.evidence(),
            )),
            _ => None,
        })
        .collect()
}

/// Denial contents in per-run order, so each refusal path is identifiable.
fn denial_contents(events: &[RunEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event.payload() {
            EventPayload::ToolFinished(info)
                if info.outcome.status() == ExecutionStatus::Denied =>
            {
                Some(info.outcome.content().to_owned())
            }
            _ => None,
        })
        .collect()
}

fn approval_count(data: &[RunEvent], control: &[RunEvent]) -> usize {
    common::count_payload(data, control, |payload| {
        matches!(payload, EventPayload::ApprovalRequired(_))
    })
}

fn started_count(data: &[RunEvent], control: &[RunEvent]) -> usize {
    common::count_payload(data, control, |payload| {
        matches!(payload, EventPayload::ToolStarted(_))
    })
}

/// Every denial claims nothing started and was observed by the host.
fn assert_denial_invariants(denials: &[Denial]) {
    for (call, status, effect, evidence) in denials {
        assert_eq!(*status, ExecutionStatus::Denied, "call {call} is denied");
        assert_eq!(
            *effect,
            EffectState::NotStarted,
            "a denial must not claim applied effects for call {call}"
        );
        assert_eq!(
            *evidence,
            Evidence::HostObserved,
            "the host observed the refusal for call {call} directly"
        );
    }
}

fn assert_call_ids_distinct(denials: &[Denial]) {
    // `CallId` is hashable but deliberately unordered, so distinctness is
    // checked through a set rather than by sorting.
    let mut calls: HashSet<&CallId> = HashSet::with_capacity(denials.len());
    for (call, ..) in denials {
        assert!(
            calls.insert(call),
            "call {call} is recorded more than once: {denials:?}"
        );
    }
}

/// Asserts the refusal that ended admission is an explicit, typed budget
/// refusal rather than a silent or fabricated success.
fn assert_limit_terminal(finished: &RunFinished, expected_message: &'static str) {
    assert_eq!(finished.outcome(), RunOutcome::LimitReached);
    let error = finished
        .error()
        .expect("a limit outcome carries its typed diagnostic");
    assert_eq!(error.category(), ErrorCategory::ResourceLimit);
    assert_eq!(error.message(), expected_message);
}

/// An unknown-tool candidate consumes the single admitted slot, so the next
/// turn's valid candidate is refused admission instead of dispatching.
#[test]
fn unknown_tool_denial_consumes_the_run_budget_and_refuses_admission() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = Limits::m0_test();
        limits.max_tool_calls_per_run = 1;
        let script = vec![
            tool_turn(vec![common::candidate_with(
                "item-0",
                "prov-ref-0",
                "host_missing",
                r#"{"path":"a"}"#,
            )]),
            tool_turn(vec![common::candidate_with(
                "item-1",
                "prov-ref-1",
                "host_read",
                r#"{"path":"b"}"#,
            )]),
        ];
        let mut bed = common::make_bed(script, config_with(limits, true));
        let response = bed
            .runtime
            .submit(common::submit_cmd("denied-unknown"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_limit_terminal(&finished, "tool call budget for run exhausted");
        assert_eq!(
            denial_contents(&ordered(&data, &control)),
            vec!["unknown tool".to_owned()],
            "exactly the unknown tool is refused, with its static diagnostic"
        );
        let denials = outcomes(&ordered(&data, &control));
        assert_denial_invariants(&denials);
        assert_call_ids_distinct(&denials);
        assert_eq!(started_count(&data, &control), 0, "no call started");
        assert_eq!(
            bed.read_tool.execution_count(),
            0,
            "the refused candidate never reaches its tool"
        );
        assert_eq!(
            bed.provider.call_count(),
            2,
            "the second turn is requested, then refused at admission"
        );
        common::assert_single_terminal(&data, &control);
        common::assert_contiguous(&data, &control);
    });
}

/// Both invalid-argument shapes consume budget slots, and the run budget is
/// checked per candidate: the valid candidate trailing them in the same turn
/// is refused before any dispatch.
#[test]
fn invalid_argument_denials_each_consume_a_run_budget_slot() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = Limits::m0_test();
        limits.max_tool_calls_per_run = 2;
        let script = vec![tool_turn(vec![
            // Non-object root: strict parsing refuses it.
            common::candidate_with("item-0", "prov-ref-0", "host_read", "not-an-object"),
            // Duplicate keys are refused even though the text parses.
            common::candidate_with(
                "item-1",
                "prov-ref-1",
                "host_read",
                r#"{"path":"a","path":"b"}"#,
            ),
            common::candidate_with("item-2", "prov-ref-2", "host_read", r#"{"path":"c"}"#),
        ])];
        let mut bed = common::make_bed(script, config_with(limits, true));
        let response = bed.runtime.submit(common::submit_cmd("denied-args")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_limit_terminal(&finished, "tool call budget for run exhausted");
        assert_eq!(
            denial_contents(&ordered(&data, &control)),
            vec!["invalid tool arguments".to_owned(); 2],
            "both malformed candidates are refused with the same static reason"
        );
        let denials = outcomes(&ordered(&data, &control));
        assert_denial_invariants(&denials);
        assert_call_ids_distinct(&denials);
        assert_eq!(started_count(&data, &control), 0);
        assert_eq!(
            bed.read_tool.execution_count(),
            0,
            "budget exhaustion inside one turn dispatches nothing"
        );
        assert_eq!(
            bed.provider.call_count(),
            1,
            "the refusal ends the run before another model turn"
        );
        common::assert_single_terminal(&data, &control);
    });
}

/// With no approval handler, a call needing confirmation is denied at
/// validation; the denial still consumes the admitted slot, so the budget
/// refuses the next turn's candidate.
#[test]
fn missing_approval_handler_denial_consumes_the_run_budget() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = Limits::m0_test();
        limits.max_tool_calls_per_run = 2;
        let script = vec![
            tool_turn(vec![
                common::candidate_with("item-0", "prov-ref-0", "host_write", r#"{"path":"a"}"#),
                common::candidate_with("item-1", "prov-ref-1", "host_write", r#"{"path":"b"}"#),
            ]),
            tool_turn(vec![common::candidate_with(
                "item-2",
                "prov-ref-2",
                "host_write",
                r#"{"path":"c"}"#,
            )]),
        ];
        let mut bed = common::make_bed(script, config_with(limits, false));
        let response = bed
            .runtime
            .submit(common::submit_cmd("denied-no-handler"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_limit_terminal(&finished, "tool call budget for run exhausted");
        assert_eq!(
            denial_contents(&ordered(&data, &control)),
            vec!["confirmation required and no approval handler exists".to_owned(); 2],
            "both confirmation-required calls are denied headlessly"
        );
        assert_eq!(
            approval_count(&data, &control),
            0,
            "headless denial never publishes an approval request"
        );
        let denials = outcomes(&ordered(&data, &control));
        assert_denial_invariants(&denials);
        assert_call_ids_distinct(&denials);
        assert_eq!(started_count(&data, &control), 0);
        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "an unconfirmed mutation never dispatches"
        );
        common::assert_single_terminal(&data, &control);
    });
}

/// An explicit `Deny` is an ordinary denial: it consumes the admitted slot, so
/// a run whose budget equals its denial count cannot admit another candidate.
#[test]
fn explicit_deny_consumes_the_run_budget() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = Limits::m0_test();
        limits.max_tool_calls_per_run = 2;
        let script = vec![
            tool_turn(vec![
                common::candidate_with("item-0", "prov-ref-0", "host_write", r#"{"path":"a"}"#),
                common::candidate_with("item-1", "prov-ref-1", "host_write", r#"{"path":"b"}"#),
            ]),
            tool_turn(vec![common::candidate_with(
                "item-2",
                "prov-ref-2",
                "host_read",
                r#"{"path":"c"}"#,
            )]),
        ];
        let mut bed = common::make_bed(script, config_with(limits, true));
        let response = bed
            .runtime
            .submit(common::submit_cmd("denied-explicit"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let mut control_events = Vec::new();
        for tag in ["first", "second"] {
            let mut seen = common::collect_control_until(&mut bed.control, |event| {
                matches!(event.payload(), EventPayload::ApprovalRequired(_))
            })
            .await;
            let (approval, call) = common::find_approval(&seen).expect("approval requested");
            control_events.append(&mut seen);
            let deny = bed
                .runtime
                .deny(DenyCommand {
                    request: RequestId::new(format!("req-deny-{tag}")).expect("valid"),
                    approval,
                    run: run.clone(),
                    call,
                })
                .await;
            assert_eq!(
                deny.reply(),
                CommandReply::Accepted,
                "the {tag} grant is refused"
            );
        }

        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.append(&mut control_events);
        assert_limit_terminal(&finished, "tool call budget for run exhausted");
        assert_eq!(
            denial_contents(&ordered(&data, &control)),
            vec!["approval refused".to_owned(); 2],
            "each explicit refusal is recorded once"
        );
        assert_eq!(
            approval_count(&data, &control),
            2,
            "both queued mutations asked for confirmation before being refused"
        );
        let denials = outcomes(&ordered(&data, &control));
        assert_denial_invariants(&denials);
        assert_call_ids_distinct(&denials);
        assert_eq!(started_count(&data, &control), 0);
        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "a refused mutation never dispatches"
        );
        assert_eq!(
            bed.read_tool.execution_count(),
            0,
            "the trailing automatic candidate is refused, not dispatched"
        );
        assert_eq!(bed.provider.call_count(), 2);
        common::assert_single_terminal(&data, &control);
    });
}

/// Every refusal kind in one run shares a single budget and one honest
/// accounting: distinct calls, `Denied`/`NotStarted`/`HostObserved`, no
/// dispatch, and a snapshot that keeps the denials and no pending approval.
#[test]
fn mixed_denial_kinds_share_one_budget_and_one_accounting() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = Limits::m0_test();
        limits.max_tool_calls_per_run = 4;
        let script = vec![
            tool_turn(vec![
                common::candidate_with("item-0", "prov-ref-0", "host_missing", r#"{"path":"a"}"#),
                common::candidate_with("item-1", "prov-ref-1", "host_read", "not-an-object"),
            ]),
            tool_turn(vec![common::candidate_with(
                "item-2",
                "prov-ref-2",
                "host_write",
                r#"{"path":"b"}"#,
            )]),
            tool_turn(vec![common::candidate_with(
                "item-3",
                "prov-ref-3",
                "host_write",
                r#"{"path":"c"}"#,
            )]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, config_with(limits, true));
        let response = bed.runtime.submit(common::submit_cmd("denied-mixed")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let mut control_events = Vec::new();
        for tag in ["mixed-a", "mixed-b"] {
            let mut seen = common::collect_control_until(&mut bed.control, |event| {
                matches!(event.payload(), EventPayload::ApprovalRequired(_))
            })
            .await;
            let (approval, call) = common::find_approval(&seen).expect("approval requested");
            control_events.append(&mut seen);
            let deny = bed
                .runtime
                .deny(DenyCommand {
                    request: RequestId::new(format!("req-{tag}")).expect("valid"),
                    approval,
                    run: run.clone(),
                    call,
                })
                .await;
            assert_eq!(deny.reply(), CommandReply::Accepted);
        }

        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.append(&mut control_events);
        assert_eq!(
            finished.outcome(),
            RunOutcome::Completed,
            "a run of pure denials still ends honestly: {:?}",
            finished.outcome()
        );
        assert_eq!(
            denial_contents(&ordered(&data, &control)),
            vec![
                "unknown tool".to_owned(),
                "invalid tool arguments".to_owned(),
                "approval refused".to_owned(),
                "approval refused".to_owned(),
            ],
            "each refusal kind is distinguishable in recorded order"
        );
        let denials = outcomes(&ordered(&data, &control));
        assert_denial_invariants(&denials);
        assert_call_ids_distinct(&denials);
        assert_eq!(started_count(&data, &control), 0, "no denial dispatched");
        assert_eq!(bed.read_tool.execution_count(), 0);
        assert_eq!(bed.write_tool.execution_count(), 0);
        assert_eq!(bed.provider.call_count(), 4, "every scripted turn ran");
        common::assert_single_terminal(&data, &control);

        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-mixed-snap").expect("valid"),
                run,
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("finalized run keeps a snapshot");
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed)
        );
        let known: Vec<&OutcomeSummary> = snapshot.known_outcomes().iter().collect();
        assert_eq!(known.len(), 4, "every denial is a known outcome: {known:?}");
        for summary in known {
            assert_eq!(summary.status, ExecutionStatus::Denied);
            assert_eq!(summary.effect, EffectState::NotStarted);
            assert_eq!(summary.evidence, Evidence::HostObserved);
        }
        assert!(
            snapshot.pending_approvals().is_empty(),
            "a finalized run keeps no pending approval: {:?}",
            snapshot.pending_approvals()
        );
    });
}

/// A denial consumes exactly one slot: a budget one larger than the denial
/// count still admits and dispatches the following candidate, which pins the
/// accounting to a per-candidate charge rather than a blanket refusal.
#[test]
fn denial_charges_exactly_one_slot_and_leaves_the_remainder_usable() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = Limits::m0_test();
        limits.max_tool_calls_per_run = 2;
        let script = vec![
            tool_turn(vec![common::candidate_with(
                "item-0",
                "prov-ref-0",
                "host_missing",
                r#"{"path":"a"}"#,
            )]),
            tool_turn(vec![common::candidate_with(
                "item-1",
                "prov-ref-1",
                "host_read",
                r#"{"path":"b"}"#,
            )]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, config_with(limits, true));
        let response = bed
            .runtime
            .submit(common::submit_cmd("denied-exact-charge"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Completed,
            "one denial leaves the remaining slot usable: {:?}",
            finished.outcome()
        );
        assert_eq!(
            denial_contents(&ordered(&data, &control)),
            vec!["unknown tool".to_owned()]
        );
        assert_eq!(
            started_count(&data, &control),
            1,
            "the surviving slot dispatches the valid candidate"
        );
        assert_eq!(
            bed.read_tool.execution_count(),
            1,
            "the admitted candidate ran exactly once"
        );
        assert_eq!(bed.provider.call_count(), 3);
        common::assert_single_terminal(&data, &control);

        let logged = bed.read_tool.log();
        assert_eq!(logged.len(), 1);
        assert_eq!(
            logged[0].args, r#"{"path":"b"}"#,
            "the dispatched call carries its own arguments, not the denied one"
        );
    });
}

/// A second run on a fresh runtime keeps its own budget: denials charged
/// against one run never reduce the next run's admission capacity.
#[test]
fn denial_budget_is_per_run_not_process_wide() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = Limits::m0_test();
        limits.max_tool_calls_per_run = 1;
        // Two turns so the first run ends on an explicit budget refusal rather
        // than on an exhausted provider script.
        let denying = vec![
            tool_turn(vec![common::candidate_with(
                "item-0",
                "prov-ref-0",
                "host_missing",
                r#"{"path":"a"}"#,
            )]),
            tool_turn(vec![common::candidate_with(
                "item-1",
                "prov-ref-1",
                "host_read",
                r#"{"path":"b"}"#,
            )]),
        ];
        let serving = vec![
            tool_turn(vec![common::candidate_with(
                "item-0",
                "prov-ref-0",
                "host_read",
                r#"{"path":"a"}"#,
            )]),
            stop_turn("done"),
        ];

        let mut first = common::make_bed(denying.clone(), config_with(limits, true));
        assert_eq!(
            first
                .runtime
                .submit(common::submit_cmd("per-run-deny"))
                .await
                .reply(),
            CommandReply::Accepted
        );
        let (first_data, first_control, first_finished) =
            common::drain_until_finished(&mut first.data, &mut first.control).await;
        assert_limit_terminal(&first_finished, "tool call budget for run exhausted");

        let mut second = common::make_bed(serving, config_with(limits, true));
        assert_eq!(
            second
                .runtime
                .submit(common::submit_cmd("per-run-serve"))
                .await
                .reply(),
            CommandReply::Accepted
        );
        let (second_data, second_control, second_finished) =
            common::drain_until_finished(&mut second.data, &mut second.control).await;

        assert_eq!(
            second_finished.outcome(),
            RunOutcome::Completed,
            "the denying run spent only its own budget"
        );
        assert_eq!(
            denial_contents(&ordered(&second_data, &second_control)),
            Vec::<String>::new(),
            "the second run records no denial"
        );
        assert_eq!(
            second.read_tool.execution_count(),
            1,
            "the fresh run dispatches within its own budget"
        );
        assert_eq!(outcomes(&ordered(&first_data, &first_control)).len(), 1);
    });
}
