#![forbid(unsafe_code)]

//! Public-boundary coverage for `FakeProvider` observed-request recording.
//!
//! Everything here drives `nexus_fakes`' public API plus the public
//! `nexus_core` request types: every `stream` invocation is recorded in call
//! order (including cancelled and exhausted-script calls), each observed
//! request exposes the effective per-turn output budget, conversation items
//! accumulate across turns without rewriting earlier snapshots, and a gated
//! fake makes its in-flight request inspectable before the worker is
//! released. No adapter, runtime, clock, or randomness is involved; the only
//! concurrency is the bounded entered/release gate, so every assertion is
//! deterministic.

use std::time::Duration;

use nexus_core::{
    CallId, ContinuationData, EffectState, ErrorCategory, Evidence, ExecutionStatus, FinishReason,
    Limits, M0_REVISION, ModelContextItem, ModelRequest, NormalizedArgs, ProviderContext,
    ProviderEvent, ProviderPort, RetryGuidance, RunId, ToolCall, ToolId, ToolOutcome, TurnId,
};
use nexus_fakes::{FakeProvider, candidate, stop_turn, tool_turn};

const BUDGET_SMALL: usize = 1024;
const BUDGET_LARGE: usize = 4096;
const BUDGET_MAX: usize = Limits::M0_TEST_TOOL_OUTPUT_BYTES;

fn run_id() -> RunId {
    RunId::new("run-1").expect("valid run id")
}

fn turn_id(raw: &str) -> TurnId {
    TurnId::new(raw).expect("valid turn id")
}

fn tool_id() -> ToolId {
    ToolId::new("host_read", M0_REVISION).expect("valid tool id")
}

fn request(
    turn: &str,
    profile: &str,
    budget: usize,
    continuation: Option<ContinuationData>,
) -> ModelRequest {
    ModelRequest::new(
        run_id(),
        turn_id(turn),
        profile,
        vec![tool_id()],
        continuation,
        budget,
    )
    .expect("valid request builds")
}

fn live_context() -> ProviderContext {
    ProviderContext::new(Duration::from_secs(60), false, None)
}

fn cancelled_context() -> ProviderContext {
    ProviderContext::new(Duration::from_secs(60), true, None)
}

/// Asserts the single-terminal contract and returns that terminal event.
fn terminal(events: &[ProviderEvent]) -> &ProviderEvent {
    let mut terminals = events.iter().filter(|event| event.is_terminal());
    let terminal = terminals
        .next()
        .expect("invocation ends in a terminal event");
    assert!(terminals.next().is_none(), "exactly one terminal event");
    assert_eq!(events.last(), Some(terminal), "terminal event is last");
    terminal
}

/// Concatenates every text fragment in emission order.
fn text(events: &[ProviderEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_call(turn: &str) -> ToolCall {
    ToolCall::new(
        run_id(),
        turn_id(turn),
        CallId::new("call-1").expect("valid call id"),
        tool_id(),
        NormalizedArgs::new(r#"{"path":"src"}"#).expect("valid arguments"),
    )
}

fn outcome(content: &str) -> ToolOutcome {
    ToolOutcome::new(
        ExecutionStatus::Succeeded,
        EffectState::KnownApplied,
        Evidence::HostObserved,
        content,
        false,
    )
    .expect("bounded outcome builds")
}

fn conversation_after_first_turn() -> Vec<ModelContextItem> {
    vec![ModelContextItem::user_text("read src").expect("user text builds")]
}

fn conversation_after_second_turn() -> Vec<ModelContextItem> {
    let mut items = conversation_after_first_turn();
    items.push(
        ModelContextItem::assistant_call("item-1", "prov-ref-1", tool_call("turn-1"))
            .expect("assistant call builds"),
    );
    items.push(
        ModelContextItem::tool_result(
            CallId::new("call-1").expect("valid call id"),
            "item-1",
            "prov-ref-1",
            tool_id(),
            outcome("listed src"),
        )
        .expect("tool result builds"),
    );
    items
}

fn conversation_after_third_turn() -> Vec<ModelContextItem> {
    let mut items = conversation_after_second_turn();
    items.push(ModelContextItem::assistant_text("item-2", "done").expect("assistant text builds"));
    items
}

#[test]
fn every_invocation_is_recorded_in_call_order() {
    let provider = FakeProvider::new(vec![stop_turn("one"), stop_turn("two")]);
    assert!(
        provider.requests().is_empty(),
        "nothing is observed before the first call"
    );
    assert_eq!(provider.call_count(), 0);

    // Each request varies a different field so a log that reuses or swaps
    // entries cannot pass whole-struct equality by accident.
    let first = request("turn-1", "profile-a", BUDGET_SMALL, None);
    let cancelled = request("turn-2", "profile-a", BUDGET_LARGE, None);
    let third = request(
        "turn-3",
        "profile-b",
        BUDGET_MAX,
        Some(
            ContinuationData::new("fake-adapter", "fake-model", vec![1, 2, 3])
                .expect("continuation builds"),
        ),
    );
    let exhausted = request("turn-4", "profile-b", BUDGET_SMALL, None);

    assert_eq!(text(&provider.stream(&first, &live_context())), "one");
    assert!(matches!(
        terminal(&provider.stream(&cancelled, &cancelled_context())),
        ProviderEvent::Failed(error) if error.category() == ErrorCategory::Cancelled
    ));
    assert_eq!(
        text(&provider.stream(&third, &live_context())),
        "two",
        "the cancelled invocation did not consume the script"
    );
    assert!(matches!(
        terminal(&provider.stream(&exhausted, &live_context())),
        ProviderEvent::Failed(error) if error.category() == ErrorCategory::Protocol
    ));

    assert_eq!(
        provider.call_count(),
        4,
        "cancelled and exhausted invocations are counted"
    );
    let observed = provider.requests();
    assert_eq!(
        observed,
        vec![
            first.clone(),
            cancelled.clone(),
            third.clone(),
            exhausted.clone()
        ],
        "every invocation is observed with its exact request"
    );
    let turns: Vec<&str> = observed
        .iter()
        .map(|request| request.turn().as_str())
        .collect();
    assert_eq!(
        turns,
        vec!["turn-1", "turn-2", "turn-3", "turn-4"],
        "the log follows invocation order"
    );
    assert_eq!(
        observed[2]
            .continuation()
            .expect("third request carries continuation")
            .bytes(),
        &[1, 2, 3],
        "the log preserves opaque continuation bytes"
    );

    // `requests()` hands out owned snapshots: mutating one never rewrites the
    // provider's own log.
    let mut snapshot = provider.requests();
    snapshot.clear();
    snapshot.push(first.clone());
    assert_eq!(
        provider.requests().len(),
        4,
        "returned snapshots are independent clones"
    );
    assert_eq!(provider.call_count(), 4);
}

#[test]
fn effective_budget_is_visible_per_turn() {
    let provider = FakeProvider::new(vec![stop_turn("one"), stop_turn("two"), stop_turn("three")]);
    let budgets = [BUDGET_SMALL, BUDGET_LARGE, BUDGET_MAX];
    let turn_names = ["turn-1", "turn-2", "turn-3"];
    for (turn, budget) in turn_names.iter().zip(budgets) {
        let sent = request(turn, "fake-profile", budget, None);
        assert!(matches!(
            terminal(&provider.stream(&sent, &live_context())),
            ProviderEvent::TurnFinished(_)
        ));
    }

    let observed = provider.requests();
    assert_eq!(observed.len(), budgets.len());
    for (index, budget) in budgets.iter().enumerate() {
        assert_eq!(
            observed[index].output_budget_bytes(),
            *budget,
            "{} exposes its effective budget",
            turn_names[index]
        );
    }
    assert_ne!(
        observed[0].output_budget_bytes(),
        observed[1].output_budget_bytes(),
        "the effective budget is not reused across turns"
    );
    assert_ne!(
        observed[1].output_budget_bytes(),
        observed[2].output_budget_bytes()
    );

    // A cancelled invocation is still recorded with the budget it was
    // invoked with, even though it consumed no script turn.
    let cancelled = request("turn-cancelled", "fake-profile", BUDGET_LARGE, None);
    assert!(matches!(
        terminal(&provider.stream(&cancelled, &cancelled_context())),
        ProviderEvent::Failed(error) if error.category() == ErrorCategory::Cancelled
    ));
    assert_eq!(
        provider.requests()[3].output_budget_bytes(),
        BUDGET_LARGE,
        "cancelled requests keep their effective budget observable"
    );

    // The effective budget is always finite and bounded: zero and one past
    // the global cap are rejected before any invocation exists.
    for invalid in [0, BUDGET_MAX + 1] {
        let error = ModelRequest::new(
            run_id(),
            turn_id("turn-bad-budget"),
            "fake-profile",
            vec![tool_id()],
            None,
            invalid,
        )
        .expect_err("invalid output budget is rejected");
        assert_eq!(error.category(), ErrorCategory::InvalidInput);
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    }
}

#[test]
fn conversation_is_additive_across_turns() {
    let provider = FakeProvider::new(vec![
        tool_turn(vec![candidate(
            "item-1",
            "prov-ref-1",
            "host_read",
            r#"{"path":"src"}"#,
        )]),
        stop_turn("done"),
        stop_turn("again"),
    ]);
    let sent = [
        request("turn-1", "fake-profile", BUDGET_SMALL, None)
            .with_conversation(conversation_after_first_turn())
            .expect("turn-one conversation builds"),
        request("turn-2", "fake-profile", BUDGET_SMALL, None)
            .with_conversation(conversation_after_second_turn())
            .expect("turn-two conversation builds"),
        request("turn-3", "fake-profile", BUDGET_SMALL, None)
            .with_conversation(conversation_after_third_turn())
            .expect("turn-three conversation builds"),
    ];
    for sent_request in &sent {
        assert!(matches!(
            terminal(&provider.stream(sent_request, &live_context())),
            ProviderEvent::TurnFinished(_)
        ));
    }

    let observed = provider.requests();
    assert_eq!(observed, sent, "each observed request is the exact input");
    let lengths: Vec<usize> = observed
        .iter()
        .map(|request| request.conversation().len())
        .collect();
    assert_eq!(
        lengths,
        vec![1, 3, 4],
        "the conversation grows turn over turn"
    );

    // Every later turn keeps the previous turn's items as an exact, ordered
    // prefix: additivity is append-only, never a rewrite or reordering.
    for later in 1..observed.len() {
        let earlier = observed[later - 1].conversation();
        let grown = observed[later].conversation();
        assert!(grown.len() > earlier.len(), "turn {later} is additive");
        assert_eq!(
            &grown[..earlier.len()],
            earlier,
            "turn {later} retains the earlier items in order"
        );
    }
    let payloads: Vec<usize> = observed
        .iter()
        .map(|request| {
            request
                .conversation()
                .iter()
                .map(ModelContextItem::payload_bytes)
                .sum()
        })
        .collect();
    assert!(
        payloads[0] < payloads[1] && payloads[1] < payloads[2],
        "additive items increase the counted payload"
    );

    // The new items land at the end and keep their correlated identities.
    match &observed[2].conversation()[3] {
        ModelContextItem::AssistantText { item_key, text } => {
            assert_eq!(item_key.as_str(), "item-2");
            assert_eq!(text.as_str(), "done");
        }
        other => panic!("unexpected last item {other:?}"),
    }
    // Later turns never rewrite the earlier observed snapshots.
    assert_eq!(observed[0].conversation().len(), 1);
    assert!(
        matches!(&observed[0].conversation()[0], ModelContextItem::UserText(text) if text.as_str() == "read src")
    );
}

#[test]
fn gated_provider_exposes_the_request_before_release() {
    let provider = FakeProvider::gated(vec![stop_turn("released")]);
    let gate = provider.gate().expect("gated provider exposes its gate");
    assert!(!gate.is_entered());
    assert!(!gate.is_released());
    assert!(provider.requests().is_empty());

    let blocked = request("turn-1", "fake-profile", BUDGET_LARGE, None);
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| provider.stream(&blocked, &live_context()));
        assert!(
            gate.wait_entered(Duration::from_secs(5)),
            "gated worker reached the gate"
        );
        assert_eq!(provider.call_count(), 1);
        let observed = provider.requests();
        assert_eq!(
            observed.len(),
            1,
            "the request is recorded before the gate wait finishes"
        );
        assert_eq!(
            observed[0], blocked,
            "the blocked invocation's exact request is observable"
        );
        assert_eq!(observed[0].output_budget_bytes(), BUDGET_LARGE);
        assert!(!gate.is_released(), "the worker is still blocked");
        gate.release();
        let events = worker.join().expect("gated worker joins");
        assert!(matches!(
            terminal(&events),
            ProviderEvent::TurnFinished(finished) if finished.reason() == FinishReason::Stop
        ));
        assert_eq!(text(&events), "released");
    });
    assert!(gate.is_released());
    assert_eq!(provider.call_count(), 1);
    assert_eq!(
        provider.requests(),
        vec![blocked.clone()],
        "release must not duplicate the record"
    );

    // Release is sticky: a later invocation records second and never blocks.
    let after = request("turn-2", "fake-profile", BUDGET_SMALL, None);
    let events = provider.stream(&after, &live_context());
    assert!(
        matches!(
            terminal(&events),
            ProviderEvent::Failed(error) if error.category() == ErrorCategory::Protocol
        ),
        "the single scripted turn was already consumed"
    );
    assert_eq!(provider.requests(), vec![blocked, after.clone()]);
    assert_eq!(provider.call_count(), 2);

    // Non-gated fakes expose no gate handle.
    assert!(FakeProvider::new(vec![]).gate().is_none());
}

#[test]
fn gated_provider_records_pre_cancelled_request_without_entering_the_gate() {
    let provider = FakeProvider::gated(vec![stop_turn("preserved")]);
    let gate = provider.gate().expect("gated provider exposes its gate");
    let cancelled = request("turn-1", "fake-profile", BUDGET_LARGE, None);

    let events = provider.stream(&cancelled, &cancelled_context());
    assert!(matches!(
        terminal(&events),
        ProviderEvent::Failed(error) if error.category() == ErrorCategory::Cancelled
    ));
    assert!(
        !gate.is_entered(),
        "a cancelled invocation never reaches the gate"
    );
    assert!(!gate.is_released());
    assert_eq!(
        provider.requests(),
        vec![cancelled.clone()],
        "cancelled requests are recorded observably"
    );

    gate.release();
    let live = request("turn-2", "fake-profile", BUDGET_SMALL, None);
    let events = provider.stream(&live, &live_context());
    assert_eq!(
        text(&events),
        "preserved",
        "cancellation did not consume the scripted turn"
    );
    assert_eq!(provider.requests(), vec![cancelled, live]);
    assert_eq!(provider.call_count(), 2);
}
