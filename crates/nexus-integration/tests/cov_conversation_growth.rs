#![forbid(unsafe_code)]

//! Conversation-accumulation coverage against the real runtime and the real
//! fakes.
//!
//! Invariants under test:
//! - within the budgets, the retained conversation is additive: the submitted
//!   user text, every admitted assistant call, and every recorded tool result
//!   accumulate in order, and each model request carries a strictly longer
//!   conversation than the one before it;
//! - the provider observes the accumulation: every turn's conversation is a
//!   prefix of the next turn's conversation (nothing is rewritten, reordered,
//!   or dropped), the calls and results it carries correlate to the observed
//!   `ToolStarted`/`ToolFinished` events, and each recorded result states the
//!   outcome that was actually observed;
//! - one model turn issues exactly one inspected request: the recorded request
//!   count equals the provider invocation count, every request owns a distinct
//!   turn identity, and an earlier request's snapshot is never retroactively
//!   grown by later turns;
//! - budget pressure evicts oldest complete exchanges, keeping the task and
//!   newest exchange intact; an uncompactable newest exchange maps to an
//!   explicit `LimitReached` terminal with numeric accounting, while every
//!   already-recorded outcome stays intact.
//!
//! Determinism: scripts are fixed turn lists, timeouts come from the shared
//! helpers as generous failure backstops, and every assertion reads recorded
//! invariants (item kinds, payload bytes, call identities, terminal outcomes),
//! never wall-clock timing.

mod common;

use nexus_core::{
    CallCandidate, CallId, CommandReply, EffectState, ErrorCategory, EventPayload, Evidence,
    ExecutionStatus, FinishReason, Limits, MAX_CONVERSATION_BYTES, MAX_CONVERSATION_ITEMS,
    ModelContextItem, ModelRequest, ProviderEvent, RetryGuidance, RunEvent, RunFinished, RunId,
    RunOutcome, ToolOutcome, TurnFinished, Usage, UsageFinality,
};
use nexus_fakes::{FakeTool, stop_turn, tool_turn};
use nexus_runtime::{Policy, RuntimeConfig};

/// M0 policy with caller-chosen budgets and no approval handler, so every
/// automatic read dispatches and each observation is about the conversation
/// rather than about a pending decision.
fn config_with(limits: Limits) -> RuntimeConfig {
    RuntimeConfig {
        limits,
        policy: Policy::m0_test(),
        has_approval_handler: false,
    }
}

/// A tool turn that publishes assistant text under `item-t` and then proposes
/// `candidates`. The text key never collides with a call key, which the
/// protocol rejects.
fn text_tool_turn(text: &str, candidates: Vec<CallCandidate>) -> Vec<ProviderEvent> {
    let mut turn = vec![ProviderEvent::TextDelta {
        item_key: "item-t".to_owned(),
        text: text.to_owned(),
    }];
    turn.extend(candidates.into_iter().map(ProviderEvent::ToolCallReady));
    turn.push(ProviderEvent::TurnFinished(TurnFinished::new(
        FinishReason::ToolCalls,
        Usage::new(None, None, UsageFinality::Final),
        None,
    )));
    turn
}

/// One read candidate with an explicit turn-local identity.
fn read(item: &str, provider_ref: &str, path: &str) -> CallCandidate {
    common::candidate_with(
        item,
        provider_ref,
        "host_read",
        &format!(r#"{{"path":"{path}"}}"#),
    )
}

/// Item variant label, so a conversation can be asserted as an ordered kind
/// sequence instead of a length alone.
fn kind(item: &ModelContextItem) -> &'static str {
    match item {
        ModelContextItem::UserText(_) => "user",
        ModelContextItem::AssistantText { .. } => "text",
        ModelContextItem::AssistantCall { .. } => "call",
        ModelContextItem::AssistantDeniedCall { .. } => "denied-call",
        ModelContextItem::AssistantReasoning { .. } => "reasoning",
        ModelContextItem::ToolResult { .. } => "result",
    }
}

/// Ordered variant labels of one request's conversation.
fn kinds(request: &ModelRequest) -> Vec<&'static str> {
    request.conversation().iter().map(kind).collect()
}

/// Aggregate owned payload bytes of one request's conversation, counted the
/// same way the request bound counts them.
fn payload_bytes(request: &ModelRequest) -> usize {
    request
        .conversation()
        .iter()
        .fold(0usize, |total, item| total + item.payload_bytes())
}

/// The submitted user text carried by a request.
fn user_texts(request: &ModelRequest) -> Vec<String> {
    request
        .conversation()
        .iter()
        .filter_map(|item| match item {
            ModelContextItem::UserText(text) => Some(text.as_str().to_owned()),
            _ => None,
        })
        .collect()
}

/// `(provider_ref, item_key, tool)` per admitted call, in request order.
fn carried_calls(request: &ModelRequest) -> Vec<(String, String, String)> {
    request
        .conversation()
        .iter()
        .filter_map(|item| match item {
            ModelContextItem::AssistantCall {
                item_key,
                provider_ref,
                call,
                ..
            } => Some((
                provider_ref.as_str().to_owned(),
                item_key.as_str().to_owned(),
                call.tool().name().to_owned(),
            )),
            _ => None,
        })
        .collect()
}

/// `(call, item_key, provider_ref, tool)` per recorded result, in request
/// order, so a result can be matched to the call that produced it.
fn carried_results(request: &ModelRequest) -> Vec<(CallId, String, String, String)> {
    request
        .conversation()
        .iter()
        .filter_map(|item| match item {
            ModelContextItem::ToolResult {
                call,
                item_key,
                provider_ref,
                tool,
                ..
            } => Some((
                call.clone(),
                item_key.as_str().to_owned(),
                provider_ref.as_str().to_owned(),
                tool.name().to_owned(),
            )),
            _ => None,
        })
        .collect()
}

/// Call identities per `ModelContextItem::ToolResult`, in request order.
fn result_calls(request: &ModelRequest) -> Vec<CallId> {
    carried_results(request)
        .into_iter()
        .map(|(call, ..)| call)
        .collect()
}

/// Call identities per `ModelContextItem::AssistantCall`, in request order.
fn call_calls(request: &ModelRequest) -> Vec<CallId> {
    request
        .conversation()
        .iter()
        .filter_map(|item| match item {
            ModelContextItem::AssistantCall { call, .. } => Some(call.call().clone()),
            _ => None,
        })
        .collect()
}

/// Recorded outcomes paired with their call, in sequence order across both
/// channels. An outcome rides the control channel, so it is the only proof
/// that a dispatched call actually ran.
fn recorded_outcomes(data: &[RunEvent], control: &[RunEvent]) -> Vec<(CallId, ToolOutcome)> {
    let mut all: Vec<&RunEvent> = data.iter().chain(control.iter()).collect();
    all.sort_by_key(|event| event.seq());
    all.iter()
        .filter_map(|event| match event.payload() {
            EventPayload::ToolFinished(info) => Some((info.call.clone(), info.outcome.clone())),
            _ => None,
        })
        .collect()
}

/// Calls that entered execution, in sequence order across both channels.
fn started_calls(data: &[RunEvent], control: &[RunEvent]) -> Vec<CallId> {
    let mut all: Vec<&RunEvent> = data.iter().chain(control.iter()).collect();
    all.sort_by_key(|event| event.seq());
    all.iter()
        .filter_map(|event| match event.payload() {
            EventPayload::ToolStarted(info) => Some(info.call.clone()),
            _ => None,
        })
        .collect()
}

/// Asserts an exhausted byte-budget terminal with numeric-only accounting.
fn assert_byte_limit_terminal(finished: &RunFinished, items: usize) {
    assert_eq!(finished.outcome(), RunOutcome::LimitReached);
    let error = finished
        .error()
        .expect("an exhausted conversation budget records its cause");
    assert_eq!(error.category(), ErrorCategory::ResourceLimit);
    let prefix = format!(
        "context budget exhausted: items={items} max_items={} bytes=",
        Limits::M0_TEST_RETAINED_CONTEXT_ITEMS
    );
    let suffix = format!(" max_bytes={MAX_CONVERSATION_BYTES} continuation=0");
    let bytes = error
        .message()
        .strip_prefix(&prefix)
        .and_then(|message| message.strip_suffix(&suffix))
        .expect("diagnostic contains only numeric context accounting")
        .parse::<usize>()
        .expect("payload byte count is numeric");
    assert!(bytes > MAX_CONVERSATION_BYTES);
    assert_eq!(
        error.retry(),
        RetryGuidance::DoNotRetry,
        "an exhausted conversation budget is never retried into the same bound"
    );
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

/// The submitted user text, every admitted call, and every recorded result
/// accumulate in order, and each request carries strictly more than the last.
///
/// The kind sequence is asserted exactly, so a reordering, a dropped result, a
/// duplicated user item, or a call recorded without its result all fail here
/// rather than passing on a length check alone.
#[test]
fn user_text_calls_and_results_accumulate_in_order_across_turns() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            text_tool_turn(
                "read two ",
                vec![read("item-0", "prov-0", "a"), read("item-1", "prov-1", "b")],
            ),
            text_tool_turn("read one ", vec![read("item-2", "prov-2", "c")]),
            text_tool_turn(
                "read two ",
                vec![read("item-3", "prov-3", "d"), read("item-4", "prov-4", "e")],
            ),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, config_with(Limits::m0_test()));
        let response = bed
            .runtime
            .submit(common::submit_cmd("conversation-growth"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Completed,
            "the four-turn cycle completes on the stop turn"
        );
        assert_owned_by_run(&run, &data, &control);
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);
        assert_eq!(
            bed.read_tool.execution_count(),
            5,
            "one read per admitted call"
        );

        let requests = bed.provider.requests();
        assert_eq!(requests.len(), 4, "one request per model turn");
        assert_eq!(
            kinds(&requests[0]),
            vec!["user"],
            "the first turn carries only the submitted task"
        );
        assert_eq!(
            kinds(&requests[1]),
            vec!["user", "text", "call", "call", "result", "result"],
            "turn two adds turn one's text, both admitted calls, and both results"
        );
        assert_eq!(
            kinds(&requests[2]),
            vec![
                "user", "text", "call", "call", "result", "result", "text", "call", "result"
            ],
            "turn three appends without rewriting turn two"
        );
        assert_eq!(
            kinds(&requests[3]),
            vec![
                "user", "text", "call", "call", "result", "result", "text", "call", "result",
                "text", "call", "call", "result", "result"
            ],
            "turn four appends the final text, two calls, and two results"
        );

        // The submitted task is carried exactly once, unchanged, on every
        // turn: accumulation never re-adds or edits the user's input.
        for (index, request) in requests.iter().enumerate() {
            assert_eq!(
                user_texts(request),
                vec!["do work".to_owned()],
                "turn {} carries the submitted task exactly once",
                index + 1
            );
            let conversation = request.conversation();
            assert!(
                matches!(conversation.first(), Some(ModelContextItem::UserText(_))),
                "the task stays the first item: {conversation:?}"
            );
        }

        // Each call is recorded once, and each recorded result is the call's
        // own: five calls, five results, one-to-one, in the same order.
        let last = &requests[3];
        assert_eq!(
            call_calls(last),
            started_calls(&data, &control),
            "every dispatched call is carried once, in dispatch order"
        );
        let results = carried_results(last);
        assert_eq!(
            result_calls(last),
            started_calls(&data, &control),
            "every dispatched call has exactly one carried result, in order"
        );
        assert_eq!(
            carried_calls(last)
                .iter()
                .map(|(_, item_key, _)| item_key.as_str())
                .collect::<Vec<_>>(),
            vec!["item-0", "item-1", "item-2", "item-3", "item-4"],
            "each call keeps the item key its own turn proposed"
        );
        assert_eq!(
            carried_calls(last)
                .iter()
                .map(|(provider_ref, _, _)| provider_ref.as_str())
                .collect::<Vec<_>>(),
            vec!["prov-0", "prov-1", "prov-2", "prov-3", "prov-4"],
            "each call keeps the reference its own turn proposed"
        );
        for (index, (call, item_key, provider_ref, tool)) in results.iter().enumerate() {
            let (expected_ref, expected_key, expected_tool) = &carried_calls(last)[index];
            assert_eq!(
                provider_ref, expected_ref,
                "result {index} keeps its reference"
            );
            assert_eq!(item_key, expected_key, "result {index} keeps its item key");
            assert_eq!(
                tool, expected_tool,
                "result {index} keeps its resolved tool"
            );
            assert_eq!(call, &started_calls(&data, &control)[index]);
        }

        // A carried result states the outcome actually observed on the event
        // stream: same status, effect, evidence, and content, never a
        // rewritten success.
        for (call, outcome) in recorded_outcomes(&data, &control) {
            let carried = last
                .conversation()
                .iter()
                .find_map(|item| match item {
                    ModelContextItem::ToolResult {
                        call: recorded,
                        outcome,
                        ..
                    } if recorded == &call => Some(outcome),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("the recorded outcome for {call} is carried"));
            assert_eq!(carried.status(), outcome.status());
            assert_eq!(carried.effect(), outcome.effect());
            assert_eq!(carried.evidence(), outcome.evidence());
            assert_eq!(carried.content(), outcome.content());
            assert_eq!(carried.is_truncated(), outcome.is_truncated());
            assert_eq!(carried.status(), ExecutionStatus::Succeeded);
            assert_eq!(carried.effect(), EffectState::KnownNotApplied);
            assert_eq!(carried.evidence(), Evidence::HostObserved);
        }
    });
}

/// The provider observes an additive conversation: each turn's conversation is
/// a prefix of the next, every turn carries more than the last, and each
/// observed result matches the recorded outcome for the same call.
#[test]
fn provider_observes_a_growing_prefix_conversation_each_turn() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![read("item-0", "prov-0", "a")]),
            tool_turn(vec![read("item-1", "prov-1", "b")]),
            tool_turn(vec![read("item-2", "prov-2", "c")]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, config_with(Limits::m0_test()));
        let response = bed
            .runtime
            .submit(common::submit_cmd("conversation-prefix"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(
            bed.provider.call_count(),
            4,
            "one invocation per model turn"
        );

        let requests = bed.provider.requests();
        assert_eq!(
            requests.len(),
            bed.provider.call_count(),
            "every invocation is inspected exactly once"
        );
        let lengths: Vec<usize> = requests
            .iter()
            .map(|request| request.conversation().len())
            .collect();
        assert_eq!(
            lengths,
            vec![1, 3, 5, 7],
            "the conversation grows turn over turn"
        );
        for (index, request) in requests.iter().enumerate() {
            assert_eq!(
                request.run(),
                &run,
                "request {} owns the accepted run",
                index + 1
            );
            assert!(
                request.conversation().len() <= MAX_CONVERSATION_ITEMS,
                "every observed request stays inside the item bound"
            );
            assert!(
                payload_bytes(request) <= MAX_CONVERSATION_BYTES,
                "every observed request stays inside the aggregate byte bound"
            );
        }
        for later in 1..requests.len() {
            let (earlier, grown) = (&requests[later - 1], &requests[later]);
            assert_eq!(
                &grown.conversation()[..earlier.conversation().len()],
                earlier.conversation(),
                "turn {}'s conversation extends turn {}'s without rewriting it",
                later + 1,
                later
            );
            assert!(
                grown.conversation().len() > earlier.conversation().len(),
                "turn {} adds new items rather than re-sending the same conversation",
                later + 1
            );
        }

        let started = started_calls(&data, &control);
        let recorded: Vec<(CallId, ToolOutcome)> = recorded_outcomes(&data, &control);
        assert_eq!(started.len(), 3, "three reads dispatched");
        assert_eq!(recorded.len(), 3, "three outcomes recorded");
        for (index, request) in requests.iter().enumerate() {
            let carried = result_calls(request);
            assert_eq!(
                carried,
                started[..index].to_vec(),
                "turn {} carries exactly the {} results recorded before it",
                index + 1,
                index
            );
            for call in &carried {
                let (_, outcome) = recorded
                    .iter()
                    .find(|(recorded_call, _)| recorded_call == call)
                    .unwrap_or_else(|| panic!("{call} has a recorded outcome"));
                let item = request
                    .conversation()
                    .iter()
                    .find_map(|item| match item {
                        ModelContextItem::ToolResult {
                            call: carried_call,
                            outcome,
                            ..
                        } if carried_call == call => Some(outcome),
                        _ => None,
                    })
                    .expect("the carried result for the call");
                assert_eq!(item, outcome, "the carried result is the recorded outcome");
            }
        }
    });
}

/// One model turn issues exactly one inspected request.
///
/// Each turn owns a distinct request identity, the inspected count equals the
/// invocation count, and the earliest request still carries only the submitted
/// task after the run ends: a later turn never retroactively grows an
/// already-inspected snapshot.
#[test]
fn each_turn_inspects_exactly_one_request_with_its_own_identity() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            text_tool_turn("one ", vec![read("item-0", "prov-0", "a")]),
            text_tool_turn("two ", vec![read("item-1", "prov-1", "b")]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, config_with(Limits::m0_test()));
        let response = bed
            .runtime
            .submit(common::submit_cmd("conversation-inspection"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);

        let requests = bed.provider.requests();
        assert_eq!(
            requests.len(),
            3,
            "three turns produce three inspected requests"
        );
        assert_eq!(
            requests.len(),
            bed.provider.call_count(),
            "inspection count equals invocation count: no re-ask, no skipped request"
        );
        let mut turns: Vec<String> = Vec::new();
        for (index, request) in requests.iter().enumerate() {
            assert_eq!(request.run(), &run, "request {} owns the run", index + 1);
            assert_eq!(
                request.profile(),
                "m0-test",
                "request {} keeps its profile",
                index + 1
            );
            assert!(
                !turns.contains(&request.turn().as_str().to_owned()),
                "request {} mints a distinct turn identity",
                index + 1
            );
            turns.push(request.turn().as_str().to_owned());
        }

        let earliest = requests[0].conversation().len();
        assert_eq!(earliest, 1, "the first request carries only the task");
        assert!(
            requests.last().expect("requests").conversation().len() > earliest,
            "later turns really did add items"
        );
        assert_eq!(
            bed.provider.requests()[0].conversation().len(),
            earliest,
            "the recorded first request is not retroactively grown"
        );
        assert_eq!(
            user_texts(&requests[0]),
            vec!["do work".to_owned()],
            "the first request never gained a second task item"
        );
        common::assert_single_terminal(&data, &control);
    });
}

/// A failed call accumulates its recorded failure, never a rewritten success.
///
/// A recorded result states what happened. The failure enters the next turn's
/// conversation with its honest status, unknown effects, and uncertain
/// evidence; it authorizes no retry, so the host port is entered exactly once
/// and the run still completes normally on the stop turn.
#[test]
fn failed_result_accumulates_as_observed_and_authorizes_no_redispatch() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![read("item-0", "prov-0", "a")]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed_with_tools(
            script,
            config_with(Limits::m0_test()),
            FakeTool::failing("host_read"),
            FakeTool::mutation(),
        );
        let response = bed
            .runtime
            .submit(common::submit_cmd("conversation-failed-result"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Completed,
            "a failed tool call does not end the run"
        );
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);

        let started = started_calls(&data, &control);
        let recorded = recorded_outcomes(&data, &control);
        assert_eq!(started.len(), 1, "the call dispatched once");
        assert_eq!(recorded.len(), 1, "the failure is recorded once");
        assert_eq!(
            recorded[0].0, started[0],
            "the outcome belongs to the started call"
        );
        assert_eq!(recorded[0].1.status(), ExecutionStatus::Failed);
        assert_eq!(recorded[0].1.effect(), EffectState::Unknown);
        assert_eq!(recorded[0].1.evidence(), Evidence::Uncertain);
        assert!(
            !recorded[0].1.is_truncated(),
            "the recorded failure is whole, not a cut success"
        );

        let requests = bed.provider.requests();
        assert_eq!(requests.len(), 2, "one request per model turn");
        let carried = requests[1]
            .conversation()
            .iter()
            .find_map(|item| match item {
                ModelContextItem::ToolResult { call, outcome, .. } if call == &started[0] => {
                    Some(outcome)
                }
                _ => None,
            })
            .expect("the second request carries the recorded failure");
        assert_eq!(
            carried, &recorded[0].1,
            "the conversation states the failure exactly as it was recorded"
        );
        assert_eq!(carried.status(), ExecutionStatus::Failed);
        assert_ne!(
            carried.status(),
            ExecutionStatus::Succeeded,
            "a recorded failure is never rewritten as a success"
        );
        assert_eq!(
            bed.read_tool.execution_count(),
            1,
            "a recorded result authorizes no retry inside the same run"
        );
        assert_eq!(bed.write_tool.execution_count(), 0);
    });
}

/// Retained-item pressure evicts the oldest complete exchange, not its
/// recorded outcome, and the newest call/result pair reaches the provider.
#[test]
fn retained_item_pressure_evicts_the_oldest_complete_exchange() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![read("item-0", "prov-0", "a")]),
            tool_turn(vec![read("item-1", "prov-1", "b")]),
            stop_turn("done"),
        ];
        let mut limits = Limits::m0_test();
        // One user item plus, per dispatched call, one assistant-call item and
        // one result item: three items after the first call, five after the
        // second. A bound of four requires evicting the first complete pair.
        limits.retained_context_items = 4;
        let mut bed = common::make_bed(script, config_with(limits));
        let response = bed
            .runtime
            .submit(common::submit_cmd("conversation-item-overflow"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert!(finished.error().is_none());
        assert_eq!(
            bed.provider.call_count(),
            3,
            "compaction makes room for the final provider invocation"
        );

        // Evicting context never rewrites recorded tool outcomes.
        assert_eq!(bed.read_tool.execution_count(), 2);
        let recorded = recorded_outcomes(&data, &control);
        assert_eq!(recorded.len(), 2, "both admitted calls recorded an outcome");
        assert!(
            recorded
                .iter()
                .all(|(_, outcome)| outcome.status() == ExecutionStatus::Succeeded),
            "recorded outcomes stay successful: {recorded:?}"
        );
        let requests = bed.provider.requests();
        assert_eq!(requests.len(), 3, "every request was inspected");
        assert_eq!(
            result_calls(&requests[1]),
            vec![started_calls(&data, &control)[0].clone()],
            "the earlier snapshot is unchanged by later compaction"
        );
        assert_eq!(kinds(&requests[2]), vec!["user", "call", "result"]);
        assert!(
            requests[2]
                .context_note()
                .contains("Earlier completed exchanges were omitted")
        );
        assert!(data.iter().any(|event| matches!(
            event.payload(),
            EventPayload::ContextReduced {
                removed_items: 2,
                ..
            }
        )));
        assert_eq!(user_texts(&requests[2]), vec!["do work".to_owned()]);
        assert_eq!(call_calls(&requests[2]), vec![recorded[1].0.clone()]);
        assert_eq!(result_calls(&requests[2]), vec![recorded[1].0.clone()]);
        assert_eq!(
            carried_calls(&requests[2]),
            vec![(
                "prov-1".to_owned(),
                "item-1".to_owned(),
                "host_read".to_owned()
            )]
        );
        assert!(matches!(
            &requests[2].conversation()[2],
            ModelContextItem::ToolResult { outcome, .. } if outcome == &recorded[1].1
        ));
        assert!(
            requests
                .iter()
                .all(|request| request.conversation().len() <= 4),
            "no inspected request exceeded the retained bound: {:?}",
            requests
                .iter()
                .map(|request| request.conversation().len())
                .collect::<Vec<_>>()
        );
    });
}

#[test]
fn omitted_inherited_history_is_noted_in_context_and_published() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = Limits::m0_test();
        limits.retained_context_items = 4;
        let script = vec![
            tool_turn(vec![read("item-0", "prov-0", "a")]),
            stop_turn("first answer"),
            stop_turn("second answer"),
        ];
        let mut bed = common::make_bed(script, config_with(limits));

        assert_eq!(
            bed.runtime
                .submit(common::submit_cmd("first"))
                .await
                .reply(),
            CommandReply::Accepted
        );
        let (_, _, first) = common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(first.outcome(), RunOutcome::Completed);

        assert_eq!(
            bed.runtime
                .submit(common::submit_cmd("second"))
                .await
                .reply(),
            CommandReply::Accepted
        );
        let (data, _, second) = common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(second.outcome(), RunOutcome::Completed);
        assert!(
            data.iter()
                .any(|event| matches!(event.payload(), EventPayload::ContextReduced { .. }))
        );

        let requests = bed.provider.requests();
        let request = &requests[2];
        assert_eq!(user_texts(request), vec!["do work".to_owned()]);
        assert!(
            request
                .context_note()
                .contains("Earlier completed exchanges were omitted")
        );
    });
}

#[test]
fn cache_eviction_notice_survives_until_the_next_run() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = Limits::m0_test();
        limits.retained_context_items = 4;
        let script = vec![
            tool_turn(vec![read("item-0", "prov-0", "a")]),
            stop_turn("first answer"),
            tool_turn(vec![read("item-1", "prov-1", "b")]),
            stop_turn("second answer"),
            stop_turn("third answer"),
        ];
        let mut bed = common::make_bed(script, config_with(limits));

        for tag in ["cache-one", "cache-two"] {
            assert_eq!(
                bed.runtime.submit(common::submit_cmd(tag)).await.reply(),
                CommandReply::Accepted
            );
            let (_, _, finished) =
                common::drain_until_finished(&mut bed.data, &mut bed.control).await;
            assert_eq!(finished.outcome(), RunOutcome::Completed);
        }

        assert_eq!(
            bed.runtime
                .submit(common::submit_cmd("cache-three"))
                .await
                .reply(),
            CommandReply::Accepted
        );
        let (data, _, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert!(data.iter().any(|event| matches!(
            event.payload(),
            EventPayload::ContextReduced { removed_items, .. } if *removed_items >= 4
        )));

        let requests = bed.provider.requests();
        let last = requests.last().expect("third run request");
        assert!(
            last.context_note()
                .contains("Earlier completed exchanges were omitted")
        );
    });
}

#[test]
fn cache_omission_notice_survives_a_failed_submit_and_reaches_the_next_submit() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = Limits::m0_test();
        limits.retained_context_items = 4;
        let script = vec![
            tool_turn(vec![read("item-0", "prov-0", "a")]),
            stop_turn("first answer"),
            tool_turn(vec![read("item-1", "prov-1", "b")]),
            stop_turn("second answer"),
            vec![ProviderEvent::Failed(
                nexus_core::AgentError::new(
                    ErrorCategory::Internal,
                    "scripted provider failure",
                    RetryGuidance::DoNotRetry,
                )
                .expect("error"),
            )],
            stop_turn("recovered answer"),
        ];
        let mut bed = common::make_bed(script, config_with(limits));
        for (tag, expected) in [
            ("notice-first", RunOutcome::Completed),
            ("notice-second", RunOutcome::Completed),
            ("notice-failed", RunOutcome::Failed),
            ("notice-recovered", RunOutcome::Completed),
        ] {
            assert_eq!(
                bed.runtime.submit(common::submit_cmd(tag)).await.reply(),
                CommandReply::Accepted
            );
            let (data, _, finished) =
                common::drain_until_finished(&mut bed.data, &mut bed.control).await;
            assert_eq!(finished.outcome(), expected);
            if tag == "notice-failed" || tag == "notice-recovered" {
                let requests = bed.provider.requests();
                let request = requests.last().expect("request from submitted run");
                assert_eq!(
                    usize::from(
                        request
                            .context_note()
                            .contains("Earlier completed exchanges were omitted")
                    ),
                    1,
                    "both failed and recovered submissions receive exactly one notice"
                );
                assert!(data.iter().any(|event| matches!(
                    event.payload(),
                    EventPayload::ContextReduced { removed_items, retained_items, .. }
                        if *removed_items >= 4
                            && *retained_items == request.conversation().len()
                )));
            }
        }
        assert_eq!(bed.provider.call_count(), 6);
        assert_eq!(bed.read_tool.execution_count(), 2, "no tool was replayed");
    });
}

/// Aggregate byte pressure evicts whole oldest exchanges and completes.
///
/// Each recorded tool result carries the full effective output budget, so the
/// accumulated conversation passes the one-MiB aggregate bound while the item
/// bound is nowhere near. The byte bound triggers compaction, and no inspected
/// request ever exceeds either bound.
#[test]
fn aggregate_byte_pressure_evicts_oldest_exchanges_and_completes() {
    let rt = common::test_rt();
    rt.block_on(async {
        let read_tool = FakeTool::oversized("host_read", Limits::M0_TEST_TOOL_OUTPUT_BYTES);
        // Each turn adds one full-budget result, so four turns pass the one-MiB
        // aggregate bound while the item bound (128) is nowhere near.
        let script = (0..6)
            .map(|index| {
                tool_turn(vec![read(
                    &format!("item-{index}"),
                    &format!("prov-{index}"),
                    "a",
                )])
            })
            .chain(std::iter::once(stop_turn("done")))
            .collect::<Vec<Vec<ProviderEvent>>>();
        let mut bed = common::make_bed_with_tools(
            script,
            config_with(Limits::m0_test()),
            read_tool,
            FakeTool::mutation(),
        );
        let response = bed
            .runtime
            .submit(common::submit_cmd("conversation-byte-overflow"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert!(finished.error().is_none());

        let requests = bed.provider.requests();
        assert_eq!(
            requests.len(),
            7,
            "all six tool turns and the stop turn run"
        );
        assert_eq!(
            requests.len(),
            bed.provider.call_count(),
            "every invocation was still inspected exactly once"
        );
        for (index, request) in requests.iter().enumerate() {
            assert!(
                payload_bytes(request) <= MAX_CONVERSATION_BYTES,
                "inspected request {} stays inside the aggregate byte bound: {} bytes",
                index + 1,
                payload_bytes(request)
            );
            assert!(
                request.conversation().len() < MAX_CONVERSATION_ITEMS,
                "the item bound is untouched"
            );
        }
        let last = requests.last().expect("at least one request");
        assert!(
            payload_bytes(last) > MAX_CONVERSATION_BYTES / 2,
            "the run really did accumulate a large conversation: {} bytes",
            payload_bytes(last)
        );
        assert_eq!(
            kinds(last),
            vec!["user", "call", "result", "call", "result", "call", "result"]
        );
        assert!(
            last.context_note()
                .contains("Earlier completed exchanges were omitted")
        );
        assert!(
            data.iter()
                .any(|event| matches!(event.payload(), EventPayload::ContextReduced { .. }))
        );
        assert_eq!(user_texts(last), vec!["do work".to_owned()]);
        assert_eq!(
            carried_calls(last)
                .iter()
                .map(|(reference, _, _)| reference.as_str())
                .collect::<Vec<_>>(),
            vec!["prov-3", "prov-4", "prov-5"],
            "only the oldest complete exchanges are evicted"
        );
        assert_eq!(
            result_calls(&requests[1]).len(),
            1,
            "earlier snapshots remain intact"
        );

        // Compaction does not roll back or truncate observed effects.
        let recorded = recorded_outcomes(&data, &control);
        assert_eq!(
            recorded.len(),
            6,
            "every tool turn records an outcome, including evicted exchanges"
        );
        assert_eq!(
            recorded.len(),
            bed.read_tool.execution_count(),
            "one recorded outcome per dispatched call"
        );
        assert!(
            recorded.iter().all(
                |(_, outcome)| outcome.status() == ExecutionStatus::Succeeded
                    && outcome.content().len() == Limits::M0_TEST_TOOL_OUTPUT_BYTES
            ),
            "recorded outcomes keep their full bounded content: {:?}",
            recorded
                .iter()
                .map(|(call, outcome)| (call.as_str(), outcome.content().len()))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            call_calls(last),
            recorded[3..]
                .iter()
                .map(|(call, _)| call.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(result_calls(last), call_calls(last));
        for item in last.conversation() {
            if let ModelContextItem::ToolResult { call, outcome, .. } = item {
                assert_eq!(
                    outcome,
                    &recorded
                        .iter()
                        .find(|(recorded_call, _)| recorded_call == call)
                        .expect("recorded result")
                        .1
                );
            }
        }
    });
}

/// A parallel exchange cannot be split to fit the byte budget, even when each
/// individual tool result fits its own output budget. All observed results
/// remain recorded, but the provider never receives an oversized request.
#[test]
fn newest_exchange_exceeding_byte_budget_ends_as_limit_reached() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(
                (0..4)
                    .map(|index| read(&format!("item-{index}"), &format!("prov-{index}"), "a"))
                    .collect(),
            ),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed_with_tools(
            script,
            config_with(Limits::m0_test()),
            FakeTool::oversized("host_read", Limits::M0_TEST_TOOL_OUTPUT_BYTES),
            FakeTool::mutation(),
        );
        let response = bed
            .runtime
            .submit(common::submit_cmd("newest-exchange-byte-overflow"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);
        assert_byte_limit_terminal(&finished, 9);
        assert_eq!(bed.provider.call_count(), 1);
        let requests = bed.provider.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(kinds(&requests[0]), vec!["user"]);
        assert_eq!(bed.read_tool.execution_count(), 4);
        let recorded = recorded_outcomes(&data, &control);
        assert_eq!(recorded.len(), 4);
        assert_eq!(
            recorded
                .iter()
                .map(|(call, _)| call.clone())
                .collect::<Vec<_>>(),
            started_calls(&data, &control)
        );
        assert!(recorded.iter().all(|(_, outcome)| {
            outcome.status() == ExecutionStatus::Succeeded
                && outcome.content().len() == Limits::M0_TEST_TOOL_OUTPUT_BYTES
                && !outcome.is_truncated()
        }));
    });
}
