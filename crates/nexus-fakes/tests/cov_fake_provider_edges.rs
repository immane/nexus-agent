//! Public-boundary adversarial hardening for the `FakeProvider` scripted
//! double.
//!
//! Every assertion goes through the `nexus-fakes` and `nexus-core` public
//! APIs; no private module is touched and no runtime is involved. The file
//! pins the adversarial edges the provider contract relies on:
//!
//! - duplicate and conflicting provider references never present a
//!   dispatchable success: the failure fixtures end in a `Protocol` failure,
//!   and the adversarial success fixtures only *claim* success while carrying
//!   an ambiguity a host must reject;
//! - malformed argument JSON either emits no candidate at all or a candidate
//!   that the public admission carrier (`NormalizedArgs`) rejects;
//! - an exhausted script yields an explicit `Protocol` failure on every call,
//!   never an empty idle success, and a failed turn still consumes its
//!   scripted entry;
//! - cancellation overrides any script, for the legacy snapshot and for live
//!   tokens, and never consumes a scripted turn;
//! - advertised argument progress one byte over the M0 assembly budget is
//!   rejected by `Limits` while the following candidate stays within bounds,
//!   so a host that trusts progress must reject the invocation;
//! - the adjacent unfinished-item fixture claims a clean stop with no
//!   completed item, which no host may admit as a finished turn.
//!
//! Determinism: no sleeps, randomness, wall-clock reads, I/O, or network. The
//! one threaded test coordinates exclusively through `FakeGate` entry/release
//! and uses bounded waits only as hang guards.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use nexus_core::{
    AgentError, CallCandidate, CancellationToken, ErrorCategory, FinishReason, Limits,
    ModelRequest, NormalizedArgs, ProviderContext, ProviderEvent, ProviderPort, RetryGuidance,
    RunId, TurnId,
};
use nexus_fakes::{FakeProvider, stop_turn};

const REQUEST_BUDGET_BYTES: usize = 1024;

fn request() -> ModelRequest {
    ModelRequest::new(
        RunId::new("run-1").expect("valid run id"),
        TurnId::new("turn-1").expect("valid turn id"),
        "fake-profile",
        vec![],
        None,
        REQUEST_BUDGET_BYTES,
    )
    .expect("valid request")
}

fn live_context() -> ProviderContext {
    ProviderContext::new(Duration::from_secs(60), false, None)
}

fn cancelled_snapshot() -> ProviderContext {
    ProviderContext::new(Duration::from_secs(60), true, None)
}

fn live_controlled(token: &CancellationToken) -> ProviderContext {
    ProviderContext::new(Duration::from_secs(60), false, None)
        .with_control(token.clone(), Instant::now() + Duration::from_secs(60))
}

/// Asserts an invocation carries exactly one terminal event and that it is the
/// last event, then returns it. An empty event list can never be an idle
/// success.
fn terminal(events: &[ProviderEvent]) -> &ProviderEvent {
    assert!(
        !events.is_empty(),
        "an invocation must never return no events"
    );
    let terminal_indices: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.is_terminal())
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        terminal_indices.len(),
        1,
        "an invocation must carry exactly one terminal event"
    );
    assert_eq!(
        terminal_indices[0],
        events.len() - 1,
        "the terminal event must be last"
    );
    &events[events.len() - 1]
}

/// Asserts the invocation ends in a non-retryable failure of `category`.
fn failed_terminal(events: &[ProviderEvent], category: ErrorCategory) -> &AgentError {
    match terminal(events) {
        ProviderEvent::Failed(error) => {
            assert_eq!(error.category(), category, "unexpected failure category");
            assert_eq!(
                error.retry(),
                RetryGuidance::DoNotRetry,
                "scripted adversarial failures are never retryable"
            );
            error
        }
        other => panic!("expected a terminal failure, got {other:?}"),
    }
}

fn assert_no_turn_finished(events: &[ProviderEvent]) {
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ProviderEvent::TurnFinished(_))),
        "a failed invocation must never carry a successful turn"
    );
}

fn ready_candidates(events: &[ProviderEvent]) -> Vec<&CallCandidate> {
    events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ToolCallReady(candidate) => Some(candidate),
            _ => None,
        })
        .collect()
}

fn collected_text(events: &[ProviderEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Duplicate and conflicting provider references
// ---------------------------------------------------------------------------

#[test]
fn duplicate_reference_failure_ends_in_protocol_failure() {
    let provider = FakeProvider::duplicate_reference_failure();
    let events = provider.stream(&request(), &live_context());

    assert_eq!(events.len(), 3, "two proposals then the explicit failure");
    let ready = ready_candidates(&events);
    assert_eq!(ready.len(), 2, "both proposals stay visible for rejection");
    assert_eq!(ready[0].provider_ref(), ready[1].provider_ref());
    assert_eq!(ready[0].provider_ref(), "prov-dup");
    assert_ne!(
        ready[0].item_key(),
        ready[1].item_key(),
        "two items propose the same reference"
    );
    assert_eq!(
        (ready[0].tool_name(), ready[0].arguments_json()),
        (ready[1].tool_name(), ready[1].arguments_json()),
        "the duplicate binds the same call twice"
    );

    let error = failed_terminal(&events, ErrorCategory::Protocol);
    assert!(
        error.message().contains("duplicate"),
        "unexpected failure message: {error}"
    );
    assert_no_turn_finished(&events);
    assert_eq!(provider.call_count(), 1);
    assert_eq!(provider.requests().len(), 1);
}

#[test]
fn conflicting_reference_failure_ends_in_protocol_failure() {
    let provider = FakeProvider::conflicting_reference_failure();
    let events = provider.stream(&request(), &live_context());

    assert_eq!(events.len(), 3, "two proposals then the explicit failure");
    let ready = ready_candidates(&events);
    assert_eq!(ready.len(), 2);
    assert_eq!(ready[0].provider_ref(), ready[1].provider_ref());
    assert_ne!(
        ready[0].tool_name(),
        ready[1].tool_name(),
        "the same reference is bound to two different calls"
    );
    assert_ne!(ready[0].arguments_json(), ready[1].arguments_json());

    let error = failed_terminal(&events, ErrorCategory::Protocol);
    assert!(
        error.message().contains("conflicting"),
        "unexpected failure message: {error}"
    );
    assert_no_turn_finished(&events);
    assert_eq!(provider.call_count(), 1);
}

#[test]
fn duplicate_reference_success_claims_success_but_stays_ambiguous() {
    let provider = FakeProvider::duplicate_reference_success();
    let events = provider.stream(&request(), &live_context());

    let ready = ready_candidates(&events);
    assert_eq!(ready.len(), 2);
    assert_eq!(ready[0].provider_ref(), ready[1].provider_ref());
    assert!(
        ready
            .windows(2)
            .any(|pair| pair[0].provider_ref() == pair[1].provider_ref()),
        "an ambiguity scan over the public candidates must detect the duplicate"
    );

    match terminal(&events) {
        ProviderEvent::TurnFinished(finished) => {
            assert_eq!(finished.reason(), FinishReason::ToolCalls);
            assert!(
                finished.validate().is_ok(),
                "the adversarial terminal is still contract-shaped"
            );
        }
        other => panic!("expected the adversarial success terminal, got {other:?}"),
    }
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ProviderEvent::Failed(_))),
        "the fake itself does not flag the ambiguity; host rejection is required"
    );
}

#[test]
fn conflicting_reference_success_claims_success_but_stays_ambiguous() {
    let provider = FakeProvider::conflicting_reference_success();
    let events = provider.stream(&request(), &live_context());

    let ready = ready_candidates(&events);
    assert_eq!(ready.len(), 2);
    assert_eq!(ready[0].provider_ref(), ready[1].provider_ref());
    assert_ne!(
        ready[0].tool_name(),
        ready[1].tool_name(),
        "one reference cannot be dispatched as two different tools"
    );

    match terminal(&events) {
        ProviderEvent::TurnFinished(finished) => {
            assert_eq!(finished.reason(), FinishReason::ToolCalls);
            assert!(finished.validate().is_ok());
        }
        other => panic!("expected the adversarial success terminal, got {other:?}"),
    }
}

#[test]
fn adversarial_reference_failure_consumes_the_scripted_turn() {
    let provider = FakeProvider::duplicate_reference_failure();
    let first = provider.stream(&request(), &live_context());
    failed_terminal(&first, ErrorCategory::Protocol);

    let second = provider.stream(&request(), &live_context());
    let error = failed_terminal(&second, ErrorCategory::Protocol);
    assert!(
        error.message().contains("exhausted"),
        "the failed turn was consumed, so the follow-up is an exhaustion failure: {error}"
    );
    assert_no_turn_finished(&second);
    assert_eq!(provider.call_count(), 2);
}

// ---------------------------------------------------------------------------
// Malformed argument JSON
// ---------------------------------------------------------------------------

#[test]
fn malformed_json_failure_emits_progress_but_no_candidate() {
    let provider = FakeProvider::malformed_json_failure();
    let events = provider.stream(&request(), &live_context());

    assert_eq!(events.len(), 2, "progress then the explicit failure");
    assert!(
        ready_candidates(&events).is_empty(),
        "unparseable arguments must never become a candidate"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            ProviderEvent::ToolCallDelta { assembled_bytes, .. } if *assembled_bytes > 0
        )),
        "progress is present even though nothing completed"
    );

    let error = failed_terminal(&events, ErrorCategory::Protocol);
    assert!(
        error.message().contains("malformed"),
        "unexpected failure message: {error}"
    );
    assert_no_turn_finished(&events);
    assert_eq!(provider.call_count(), 1);
}

#[test]
fn malformed_json_success_candidate_is_rejectable_at_admission() {
    let provider = FakeProvider::malformed_json_success();
    let events = provider.stream(&request(), &live_context());

    let ready = ready_candidates(&events);
    assert_eq!(
        ready.len(),
        1,
        "the adversarial fixture presents exactly one candidate"
    );
    let rejection = NormalizedArgs::new(ready[0].arguments_json())
        .expect_err("non-object-root argument text must be rejectable");
    assert_eq!(rejection.category(), ErrorCategory::InvalidInput);
    assert_eq!(rejection.retry(), RetryGuidance::DoNotRetry);
    assert!(
        rejection.message().contains("object-root"),
        "the public carrier rejects on shape: {rejection}"
    );

    // Control: the same admission path accepts an object-root value, so the
    // rejection above is about the malformed payload, not a blanket refusal.
    NormalizedArgs::new(r#"{"path":"src"}"#).expect("object-root arguments are admitted");

    match terminal(&events) {
        ProviderEvent::TurnFinished(finished) => {
            assert_eq!(finished.reason(), FinishReason::ToolCalls);
        }
        other => panic!("expected the adversarial success terminal, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Exhausted script
// ---------------------------------------------------------------------------

#[test]
fn empty_script_first_call_fails_explicitly_never_idle_success() {
    let provider = FakeProvider::new(Vec::new());
    let events = provider.stream(&request(), &live_context());

    assert_eq!(
        events.len(),
        1,
        "exhaustion must not fabricate stream content"
    );
    let error = failed_terminal(&events, ErrorCategory::Protocol);
    assert!(
        error.message().contains("exhausted"),
        "unexpected failure message: {error}"
    );
    assert_no_turn_finished(&events);
    assert_eq!(provider.call_count(), 1);
    assert_eq!(provider.requests().len(), 1);
}

#[test]
fn scripted_turns_drain_in_order_then_every_follow_up_fails() {
    let provider = FakeProvider::new(vec![stop_turn("one"), stop_turn("two")]);

    for expected in ["one", "two"] {
        let events = provider.stream(&request(), &live_context());
        assert_eq!(
            collected_text(&events),
            expected,
            "scripted turns drain in order"
        );
        match terminal(&events) {
            ProviderEvent::TurnFinished(finished) => {
                assert_eq!(finished.reason(), FinishReason::Stop);
            }
            other => panic!("expected a scripted stop turn, got {other:?}"),
        }
    }

    for _ in 0..2 {
        let events = provider.stream(&request(), &live_context());
        assert_eq!(
            events.len(),
            1,
            "an exhausted call returns only the explicit failure"
        );
        let error = failed_terminal(&events, ErrorCategory::Protocol);
        assert!(
            error.message().contains("exhausted"),
            "unexpected failure message: {error}"
        );
        assert_no_turn_finished(&events);
    }
    assert_eq!(provider.call_count(), 4);
    assert_eq!(
        provider.requests().len(),
        4,
        "every call, successful or not, is observed"
    );
}

// ---------------------------------------------------------------------------
// Cancellation overrides any script
// ---------------------------------------------------------------------------

#[test]
fn cancellation_overrides_every_adversarial_script() {
    let adversarial = [
        FakeProvider::duplicate_reference_success(),
        FakeProvider::conflicting_reference_success(),
        FakeProvider::malformed_json_success(),
        FakeProvider::oversized_progress_success(),
        FakeProvider::fragmented_text_then_two_calls(),
    ];
    for provider in &adversarial {
        let sent = request();
        let events = provider.stream(&sent, &cancelled_snapshot());

        assert_eq!(
            events.len(),
            1,
            "cancellation collapses the whole script to one terminal"
        );
        let error = failed_terminal(&events, ErrorCategory::Cancelled);
        assert!(
            error.message().contains("cancelled"),
            "unexpected failure message: {error}"
        );
        assert_no_turn_finished(&events);
        assert!(
            ready_candidates(&events).is_empty(),
            "cancellation emits no candidates"
        );
        assert_eq!(provider.call_count(), 1);
        assert_eq!(
            provider.requests(),
            vec![sent],
            "the request is recorded before the cancellation check"
        );
    }
}

#[test]
fn cancellation_precedes_script_exhaustion_for_snapshot_and_live_tokens() {
    let provider = FakeProvider::new(Vec::new());

    let snapshot = provider.stream(&request(), &cancelled_snapshot());
    failed_terminal(&snapshot, ErrorCategory::Cancelled);

    let token = CancellationToken::new();
    token.cancel();
    let live = provider.stream(&request(), &live_controlled(&token));
    failed_terminal(&live, ErrorCategory::Cancelled);

    // Control: an uncancelled live-control context still reads the script, so
    // the `Cancelled` outcomes above come from cancellation, not from
    // `with_control` itself.
    let uncancelled = CancellationToken::new();
    let exhausted = provider.stream(&request(), &live_controlled(&uncancelled));
    let error = failed_terminal(&exhausted, ErrorCategory::Protocol);
    assert!(error.message().contains("exhausted"));
    assert_eq!(provider.call_count(), 3);
}

#[test]
fn cancelled_invocations_never_consume_the_script() {
    let provider = FakeProvider::new(vec![stop_turn("survivor")]);

    let cancelled = provider.stream(&request(), &cancelled_snapshot());
    failed_terminal(&cancelled, ErrorCategory::Cancelled);

    let live = provider.stream(&request(), &live_context());
    assert_eq!(
        collected_text(&live),
        "survivor",
        "the scripted turn survived cancellation"
    );
    match terminal(&live) {
        ProviderEvent::TurnFinished(finished) => {
            assert_eq!(finished.reason(), FinishReason::Stop);
        }
        other => panic!("expected the preserved scripted turn, got {other:?}"),
    }
    assert_eq!(provider.call_count(), 2);
}

#[test]
fn cancellation_while_gated_overrides_the_script_and_preserves_it() {
    let provider = FakeProvider::gated(vec![stop_turn("survivor")]);
    let gate = provider
        .gate()
        .expect("gated provider exposes a gate handle");
    let token = CancellationToken::new();
    let context = live_controlled(&token);

    std::thread::scope(|scope| {
        let worker = scope.spawn(|| provider.stream(&request(), &context));
        assert!(
            gate.wait_entered(Duration::from_secs(5)),
            "the worker reached the gate before cancellation"
        );
        assert_eq!(
            provider.requests().len(),
            1,
            "the request is observed while the worker is blocked"
        );
        assert!(!gate.is_released());

        token.cancel();
        gate.release();

        let events = worker.join().expect("gated worker joins");
        assert_eq!(
            events.len(),
            1,
            "live cancellation collapses the scripted turn"
        );
        failed_terminal(&events, ErrorCategory::Cancelled);
        assert_no_turn_finished(&events);
    });

    let live = provider.stream(&request(), &live_context());
    assert_eq!(collected_text(&live), "survivor");
    assert_eq!(
        provider.call_count(),
        2,
        "the cancelled invocation still counts"
    );
}

#[test]
fn pre_cancelled_gated_provider_never_blocks_and_preserves_the_script() {
    let provider = FakeProvider::gated(vec![stop_turn("kept")]);
    let gate = provider
        .gate()
        .expect("gated provider exposes a gate handle");
    let token = CancellationToken::new();
    token.cancel();

    let events = provider.stream(&request(), &live_controlled(&token));
    failed_terminal(&events, ErrorCategory::Cancelled);
    assert!(
        !gate.is_entered(),
        "a pre-cancelled call never blocks a worker"
    );

    gate.release();
    let live = provider.stream(&request(), &live_context());
    assert_eq!(collected_text(&live), "kept");
    assert_eq!(provider.call_count(), 2);
}

// ---------------------------------------------------------------------------
// Oversized argument progress
// ---------------------------------------------------------------------------

#[test]
fn oversized_progress_exceeds_the_effective_assembly_budget() {
    let provider = FakeProvider::oversized_progress_success();
    let events = provider.stream(&request(), &live_context());

    let progress = events
        .iter()
        .find_map(|event| match event {
            ProviderEvent::ToolCallDelta {
                assembled_bytes, ..
            } => Some(*assembled_bytes),
            _ => None,
        })
        .expect("progress is present");
    assert_eq!(progress, Limits::M0_TEST_ARG_ASSEMBLY_BYTES + 1);

    let limits = Limits::m0_test();
    assert!(
        limits
            .check_arg_assembly_bytes(Limits::M0_TEST_ARG_ASSEMBLY_BYTES)
            .is_ok(),
        "the exact budget is inclusive"
    );
    let rejection = limits
        .check_arg_assembly_bytes(progress)
        .expect_err("one byte over the M0 budget must be rejected");
    assert_eq!(rejection.category(), ErrorCategory::ResourceLimit);
    assert_eq!(rejection.retry(), RetryGuidance::DoNotRetry);
    assert!(
        rejection.message().contains("assembly"),
        "unexpected message: {rejection}"
    );

    // The candidate itself stays inside the budget, so the violation is the
    // advertised progress a host must check before dispatch.
    let ready = ready_candidates(&events);
    assert_eq!(
        ready.len(),
        1,
        "a candidate follows the over-budget progress"
    );
    assert!(
        ready[0].arguments_json().len() <= Limits::M0_TEST_ARG_ASSEMBLY_BYTES,
        "the candidate payload alone would fit the budget"
    );
    NormalizedArgs::new(ready[0].arguments_json())
        .expect("candidate arguments pass the carrier check");

    match terminal(&events) {
        ProviderEvent::TurnFinished(finished) => {
            assert_eq!(finished.reason(), FinishReason::ToolCalls);
        }
        other => panic!("expected the adversarial success terminal, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Adjacent false-success edge
// ---------------------------------------------------------------------------

#[test]
fn unfinished_success_claims_stop_without_a_completed_item() {
    let provider = FakeProvider::unfinished_success();
    let events = provider.stream(&request(), &live_context());

    assert!(
        events
            .iter()
            .any(|event| matches!(event, ProviderEvent::ToolCallDelta { .. })),
        "progress is present"
    );
    assert!(
        ready_candidates(&events).is_empty(),
        "no item ever completes assembly"
    );
    match terminal(&events) {
        ProviderEvent::TurnFinished(finished) => {
            assert_eq!(finished.reason(), FinishReason::Stop);
        }
        other => panic!("expected the adversarial stop terminal, got {other:?}"),
    }
}
