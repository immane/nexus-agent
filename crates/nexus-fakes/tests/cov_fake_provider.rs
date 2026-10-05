#![forbid(unsafe_code)]

//! Coverage hardening: `FakeProvider` scripted turns through its public API.
//!
//! Pinned from the public boundary only:
//! - scripts are served verbatim, one queued turn per `stream` call, FIFO and
//!   per provider; an exhausted script ends in the explicit `Protocol` failure
//!   instead of a fabricated idle success;
//! - the fragmented fixture's raw-byte helper reassembles every two-way split,
//!   including splits inside multibyte codepoints, and rejects invalid UTF-8
//!   as a protocol failure rather than silently replacing bytes;
//! - stop, incomplete, refusal, and adversarial unfinished shapes stay
//!   distinct, and every named fixture ends in exactly one terminal event
//!   whose finished turn carries final usage;
//! - usage moves from a provisional update to a final one, and unknown
//!   counters stay `None`, never zero;
//! - the continuation round-trip carries adapter/scope labels matching the
//!   provider's declared identity and echoes into the observed second request;
//! - capabilities claim only fixture-backed features and leave unknown limits
//!   unestimated;
//! - cancellation is recorded and never consumes a scripted turn.
//!
//! Deterministic: no sleeps, threads, I/O, network, or wall-clock reads.

use std::time::Duration;

use nexus_core::{
    AgentError, CallCandidate, ContinuationData, ErrorCategory, FinishReason, Limits, ModelRequest,
    ProviderContext, ProviderEvent, ProviderPort, RetryGuidance, RunId, TurnFinished, TurnId,
    Usage, UsageFinality,
};
use nexus_fakes::{
    FRAGMENTED_TEXT, FakeGate, FakeProvider, candidate, reassemble_bytes, stop_turn, tool_turn,
};

const OUTPUT_BUDGET: usize = 1024;

fn request(profile: &str, continuation: Option<ContinuationData>) -> ModelRequest {
    ModelRequest::new(
        RunId::new("run-1").expect("valid run id"),
        TurnId::new("turn-1").expect("valid turn id"),
        profile,
        vec![],
        continuation,
        OUTPUT_BUDGET,
    )
    .expect("valid request builds")
}

fn live_context() -> ProviderContext {
    ProviderContext::new(Duration::from_secs(60), false, None)
}

fn cancelled_context() -> ProviderContext {
    ProviderContext::new(Duration::from_secs(60), true, None)
}

/// Asserts the single-terminal framing invariant and returns the terminal.
fn terminal(events: &[ProviderEvent]) -> &ProviderEvent {
    assert!(!events.is_empty(), "every invocation emits events");
    let terminals: Vec<&ProviderEvent> =
        events.iter().filter(|event| event.is_terminal()).collect();
    assert_eq!(terminals.len(), 1, "exactly one terminal event");
    assert_eq!(
        events.iter().position(ProviderEvent::is_terminal),
        Some(events.len() - 1),
        "the terminal event is last"
    );
    terminals[0]
}

fn finished(events: &[ProviderEvent]) -> &TurnFinished {
    match terminal(events) {
        ProviderEvent::TurnFinished(finished) => finished,
        other => panic!("expected a finished turn, got {other:?}"),
    }
}

fn failed(events: &[ProviderEvent]) -> &AgentError {
    match terminal(events) {
        ProviderEvent::Failed(error) => error,
        other => panic!("expected a terminal failure, got {other:?}"),
    }
}

fn text_deltas(events: &[ProviderEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn concatenated_text(events: &[ProviderEvent]) -> String {
    text_deltas(events).concat()
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

#[test]
fn scripted_turns_are_served_verbatim_in_fifo_order() {
    let provider = FakeProvider::new(vec![stop_turn("one"), stop_turn("two")]);
    assert_eq!(provider.call_count(), 0);
    assert!(provider.requests().is_empty());

    let first = provider.stream(&request("profile-a", None), &live_context());
    assert_eq!(first, stop_turn("one"), "first turn is served verbatim");
    let second = provider.stream(&request("profile-a", None), &live_context());
    assert_eq!(second, stop_turn("two"), "second turn is served verbatim");

    assert_eq!(provider.call_count(), 2);
    assert_eq!(provider.requests().len(), 2);
}

#[test]
fn named_turn_builders_emit_exact_event_shapes() {
    let stop = stop_turn("answer");
    assert_eq!(stop.len(), 2);
    match (&stop[0], &stop[1]) {
        (ProviderEvent::TextDelta { item_key, text }, ProviderEvent::TurnFinished(finished)) => {
            assert_eq!(item_key.as_str(), "item-0");
            assert_eq!(text.as_str(), "answer");
            assert_eq!(finished.reason(), FinishReason::Stop);
            assert_eq!(finished.usage().input_tokens(), None);
            assert_eq!(finished.usage().output_tokens(), None);
            assert_eq!(finished.usage().finality(), UsageFinality::Final);
            assert!(finished.continuation().is_none());
            finished
                .validate()
                .expect("stop terminal carries final usage");
        }
        other => panic!("unexpected stop turn shape {other:?}"),
    }

    let calls = tool_turn(vec![
        candidate("item-1", "prov-ref-1", "host_read", r#"{"path":"a"}"#),
        candidate("item-2", "prov-ref-2", "host_write", r#"{"path":"b"}"#),
    ]);
    assert_eq!(calls.len(), 3);
    let refs: Vec<&str> = ready_candidates(&calls)
        .iter()
        .map(|candidate| candidate.provider_ref())
        .collect();
    assert_eq!(refs, vec!["prov-ref-1", "prov-ref-2"], "declared order");
    let calls_terminal = finished(&calls);
    assert_eq!(calls_terminal.reason(), FinishReason::ToolCalls);
    assert_eq!(calls_terminal.usage().finality(), UsageFinality::Final);
    calls_terminal
        .validate()
        .expect("tool terminal carries final usage");

    let empty = tool_turn(vec![]);
    assert_eq!(empty.len(), 1, "no candidates means only the terminal");
    assert_eq!(finished(&empty).reason(), FinishReason::ToolCalls);
    assert!(ready_candidates(&empty).is_empty());
}

#[test]
fn candidate_builder_preserves_identity_and_arguments() {
    let args = r#"{"path":"src","depth":2}"#;
    let built = candidate("item-9", "prov-ref-9", "host_read", args);
    assert_eq!(built.item_key(), "item-9");
    assert_eq!(built.provider_ref(), "prov-ref-9");
    assert_eq!(built.tool_name(), "host_read");
    assert_eq!(built.arguments_json(), args);
}

#[test]
fn providers_do_not_share_scripts_and_exhaustion_is_explicit() {
    let first = FakeProvider::new(vec![stop_turn("first")]);
    let second = FakeProvider::new(vec![stop_turn("second")]);
    assert_eq!(
        first.stream(&request("profile-a", None), &live_context()),
        stop_turn("first")
    );
    assert_eq!(
        second.stream(&request("profile-b", None), &live_context()),
        stop_turn("second"),
        "provider instances own independent scripts"
    );

    let exhausted = first.stream(&request("profile-a", None), &live_context());
    assert_eq!(exhausted.len(), 1);
    let error = failed(&exhausted);
    assert_eq!(error.category(), ErrorCategory::Protocol);
    assert_eq!(error.message(), "fake provider script exhausted");
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert!(
        !exhausted
            .iter()
            .any(|event| matches!(event, ProviderEvent::TurnFinished(_))),
        "exhaustion never fabricates an idle success"
    );
    assert_eq!(first.call_count(), 2);
    assert_eq!(first.requests().len(), 2);
}

#[test]
fn cancelled_invocation_is_recorded_and_preserves_the_script() {
    let provider = FakeProvider::new(vec![stop_turn("kept")]);
    let cancelled = provider.stream(&request("profile-a", None), &cancelled_context());
    assert_eq!(cancelled.len(), 1);
    let error = failed(&cancelled);
    assert_eq!(error.category(), ErrorCategory::Cancelled);
    assert_eq!(error.message(), "provider invocation cancelled");
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(provider.call_count(), 1);
    assert_eq!(
        provider.requests().len(),
        1,
        "cancelled invocations are still observed"
    );

    let live = provider.stream(&request("profile-a", None), &live_context());
    assert_eq!(
        live,
        stop_turn("kept"),
        "cancellation did not consume the scripted turn"
    );
    assert_eq!(provider.call_count(), 2);
}

#[test]
fn every_named_fixture_has_one_final_terminal_and_final_usage() {
    let fixtures: Vec<(&str, FakeProvider)> = vec![
        (
            "fragmented_text_then_two_calls",
            FakeProvider::fragmented_text_then_two_calls(),
        ),
        ("interleaved_items", FakeProvider::interleaved_items()),
        (
            "duplicate_reference_failure",
            FakeProvider::duplicate_reference_failure(),
        ),
        (
            "duplicate_reference_success",
            FakeProvider::duplicate_reference_success(),
        ),
        (
            "conflicting_reference_failure",
            FakeProvider::conflicting_reference_failure(),
        ),
        (
            "conflicting_reference_success",
            FakeProvider::conflicting_reference_success(),
        ),
        (
            "malformed_json_failure",
            FakeProvider::malformed_json_failure(),
        ),
        (
            "malformed_json_success",
            FakeProvider::malformed_json_success(),
        ),
        ("truncated_stream", FakeProvider::truncated_stream()),
        ("unfinished_success", FakeProvider::unfinished_success()),
        (
            "oversized_progress_success",
            FakeProvider::oversized_progress_success(),
        ),
        ("refusal", FakeProvider::refusal()),
        (
            "usage_provisional_then_final",
            FakeProvider::usage_provisional_then_final(),
        ),
        (
            "continuation_round_trip",
            FakeProvider::continuation_round_trip(),
        ),
    ];

    for (name, provider) in fixtures {
        let events = provider.stream(&request("profile-a", None), &live_context());
        if let ProviderEvent::TurnFinished(finished) = terminal(&events) {
            finished
                .validate()
                .unwrap_or_else(|error| panic!("{name} terminal usage must be final: {error}"));
        }
        assert_eq!(provider.call_count(), 1, "{name}");
        assert_eq!(provider.requests().len(), 1, "{name}");
    }
}

#[test]
fn fragmented_fixture_concatenates_to_the_documented_text() {
    assert_eq!(FRAGMENTED_TEXT, "héllo 🌍");
    assert_eq!(
        FRAGMENTED_TEXT.len(),
        11,
        "byte length includes multibyte chars"
    );
    assert_eq!(FRAGMENTED_TEXT.chars().count(), 7);

    let provider = FakeProvider::fragmented_text_then_two_calls();
    let events = provider.stream(&request("profile-a", None), &live_context());
    let deltas = text_deltas(&events);
    assert_eq!(deltas, vec!["h", "éllo ", "🌍"], "documented split pieces");
    assert_eq!(deltas.concat(), FRAGMENTED_TEXT);
    assert_eq!(
        deltas.iter().map(|fragment| fragment.len()).sum::<usize>(),
        FRAGMENTED_TEXT.len(),
        "fragment byte lengths add up to the whole text"
    );
}

#[test]
fn reassemble_bytes_recovers_every_two_way_byte_split() {
    let raw = FRAGMENTED_TEXT.as_bytes();
    for split in 0..=raw.len() {
        let text = reassemble_bytes(&[&raw[..split], &raw[split..]])
            .unwrap_or_else(|error| panic!("split at byte {split} must reassemble: {error}"));
        assert_eq!(text, FRAGMENTED_TEXT, "split at byte {split}");
    }

    // The `é` (bytes 1-2) and `🌍` (bytes 7-10) interiors are the multibyte
    // splits the helper exists for: each split point is mid-codepoint, so no
    // fragment is valid UTF-8 alone.
    for split in [2, 8, 9, 10] {
        assert!(
            !FRAGMENTED_TEXT.is_char_boundary(split),
            "byte {split} is inside a multibyte character"
        );
    }

    let singles: Vec<&[u8]> = raw.iter().map(std::slice::from_ref).collect();
    assert_eq!(
        reassemble_bytes(&singles).expect("byte-by-byte chunks reassemble"),
        FRAGMENTED_TEXT
    );
}

#[test]
fn reassemble_bytes_rejects_invalid_utf8_and_handles_empty_input() {
    assert_eq!(reassemble_bytes(&[]).expect("no chunks is empty text"), "");
    assert_eq!(
        reassemble_bytes(&[&[], &[]]).expect("empty chunks are empty text"),
        ""
    );

    let raw = FRAGMENTED_TEXT.as_bytes();
    // The first byte of `é`, the second byte of `é`, the first byte of `🌍`,
    // and a lone continuation byte are each invalid UTF-8 alone.
    for invalid in [&raw[1..2], &raw[2..3], &raw[7..8], &[0x80u8][..]] {
        let error = reassemble_bytes(&[invalid]).expect_err("invalid UTF-8 fails explicitly");
        assert_eq!(error.category(), ErrorCategory::Protocol);
        assert_eq!(error.message(), "split fragments are not valid UTF-8");
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    }
}

#[test]
fn fragmented_fixture_progress_and_candidates_match_argument_bytes() {
    let provider = FakeProvider::fragmented_text_then_two_calls();
    let events = provider.stream(&request("profile-a", None), &live_context());
    let progress: Vec<(&str, usize)> = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ToolCallDelta {
                item_key,
                assembled_bytes,
            } => Some((item_key.as_str(), *assembled_bytes)),
            _ => None,
        })
        .collect();
    let args_a = r#"{"path":"src"}"#;
    let args_b = r#"{"path":"other"}"#;
    assert_eq!(
        progress,
        vec![
            ("item-1", args_a.len() / 2),
            ("item-1", args_a.len()),
            ("item-2", args_b.len()),
        ],
        "split argument progress advances monotonically to the full length"
    );

    let ready = ready_candidates(&events);
    assert_eq!(ready.len(), 2);
    assert_eq!(ready[0].item_key(), "item-1");
    assert_eq!(ready[1].item_key(), "item-2");
    assert_eq!(ready[0].arguments_json(), args_a);
    assert_eq!(ready[1].arguments_json(), args_b);
    assert_eq!(
        ready[0].arguments_json().len(),
        progress[1].1,
        "progress reaches the full argument length"
    );
    assert_eq!(ready[1].arguments_json().len(), progress[2].1);

    let limits = Limits::m0_test();
    for candidate in &ready {
        limits
            .check_arg_assembly_bytes(candidate.arguments_json().len())
            .expect("fixture progress stays within the assembly budget");
        nexus_core::NormalizedArgs::new(candidate.arguments_json())
            .expect("fixture arguments are valid object-root JSON");
    }
    assert_eq!(finished(&events).reason(), FinishReason::ToolCalls);
}

#[test]
fn interleaved_fixture_keeps_item_order_and_text_reassembly() {
    let provider = FakeProvider::interleaved_items();
    let events = provider.stream(&request("profile-a", None), &live_context());
    assert_eq!(concatenated_text(&events), "first second end");

    let ready = ready_candidates(&events);
    let refs: Vec<&str> = ready
        .iter()
        .map(|candidate| candidate.provider_ref())
        .collect();
    let tools: Vec<&str> = ready
        .iter()
        .map(|candidate| candidate.tool_name())
        .collect();
    assert_eq!(refs, vec!["prov-ref-1", "prov-ref-2"]);
    assert_eq!(tools, vec!["host_read", "host_write"]);

    let delta_keys: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ToolCallDelta { item_key, .. } => Some(item_key.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(delta_keys, vec!["item-1", "item-2"]);
    assert_eq!(finished(&events).reason(), FinishReason::ToolCalls);
}

#[test]
fn stop_shape_is_complete_stop_with_unknown_final_usage() {
    let provider = FakeProvider::new(vec![stop_turn("done")]);
    let events = provider.stream(&request("profile-a", None), &live_context());
    assert_eq!(concatenated_text(&events), "done");
    let finished = finished(&events);
    assert_eq!(finished.reason(), FinishReason::Stop);
    assert_eq!(
        finished.usage(),
        Usage::new(None, None, UsageFinality::Final)
    );
    assert_eq!(finished.usage().input_tokens(), None);
    assert_eq!(finished.usage().output_tokens(), None);
    assert!(finished.continuation().is_none());
    finished
        .validate()
        .expect("stop terminal carries final usage");
}

#[test]
fn incomplete_shape_never_claims_stop() {
    let provider = FakeProvider::truncated_stream();
    let events = provider.stream(&request("profile-a", None), &live_context());
    assert_eq!(concatenated_text(&events), "partial answer");
    let finished = finished(&events);
    assert_eq!(finished.reason(), FinishReason::Incomplete);
    assert_ne!(finished.reason(), FinishReason::Stop);
    assert_eq!(finished.usage().finality(), UsageFinality::Final);
    assert_eq!(finished.usage().input_tokens(), None);
    assert_eq!(finished.usage().output_tokens(), None);
    finished
        .validate()
        .expect("incomplete terminal still carries final usage");
}

#[test]
fn refusal_shape_carries_no_text_and_unknown_final_usage() {
    let provider = FakeProvider::refusal();
    let events = provider.stream(&request("profile-a", None), &live_context());
    assert!(
        text_deltas(&events).is_empty(),
        "a refusal fabricates no text"
    );
    assert!(ready_candidates(&events).is_empty());
    let finished = finished(&events);
    assert_eq!(finished.reason(), FinishReason::Refusal);
    assert_eq!(finished.usage().input_tokens(), None);
    assert_eq!(finished.usage().output_tokens(), None);
    assert!(finished.continuation().is_none());
    finished
        .validate()
        .expect("refusal terminal carries final usage");
}

#[test]
fn unfinished_success_is_an_adversarial_clean_stop_claim() {
    let provider = FakeProvider::unfinished_success();
    let events = provider.stream(&request("profile-a", None), &live_context());
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ProviderEvent::ToolCallDelta { .. })),
        "declared argument progress is present"
    );
    assert!(
        ready_candidates(&events).is_empty(),
        "no item ever reaches a complete candidate"
    );
    assert_eq!(
        finished(&events).reason(),
        FinishReason::Stop,
        "the adversarial fixture claims a clean stop for host rejection"
    );
}

#[test]
fn usage_moves_from_provisional_to_final_without_regression() {
    let provider = FakeProvider::usage_provisional_then_final();
    let events = provider.stream(&request("profile-a", None), &live_context());
    let usages: Vec<Usage> = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::Usage(usage) => Some(*usage),
            _ => None,
        })
        .collect();
    assert_eq!(usages.len(), 2, "one provisional and one final update");
    let provisional = usages[0];
    let committed = usages[1];
    assert_eq!(provisional.finality(), UsageFinality::Provisional);
    assert_eq!(committed.finality(), UsageFinality::Final);
    assert_eq!(provisional.input_tokens(), Some(10));
    assert_eq!(provisional.output_tokens(), Some(5));
    assert_eq!(committed.input_tokens(), Some(10));
    assert_eq!(committed.output_tokens(), Some(8));
    assert!(
        committed.output_tokens() >= provisional.output_tokens(),
        "final counters never regress below provisional"
    );
    assert_eq!(
        committed.input_tokens(),
        provisional.input_tokens(),
        "the input count stays stable"
    );

    assert_eq!(concatenated_text(&events), "answer");
    let usage_positions: Vec<usize> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            ProviderEvent::Usage(_) => Some(index),
            _ => None,
        })
        .collect();
    assert_eq!(usage_positions[0], 0, "provisional usage precedes text");
    assert_eq!(
        usage_positions[1],
        events.len() - 2,
        "final usage immediately precedes the terminal"
    );

    let finished = finished(&events);
    assert_eq!(
        finished.usage(),
        committed,
        "terminal usage is the final update"
    );
    finished.validate().expect("terminal usage is final");
}

#[test]
fn unknown_usage_is_never_fabricated_as_zero() {
    let zero = Usage::new(Some(0), Some(0), UsageFinality::Final);
    for provider in [
        FakeProvider::new(vec![stop_turn("text")]),
        FakeProvider::refusal(),
        FakeProvider::truncated_stream(),
    ] {
        let events = provider.stream(&request("profile-a", None), &live_context());
        let usage = finished(&events).usage();
        assert_eq!(usage.input_tokens(), None);
        assert_eq!(usage.output_tokens(), None);
        assert_ne!(usage, zero, "unknown counters are not zero");
        assert_eq!(usage.finality(), UsageFinality::Final);
    }
}

#[test]
fn continuation_carries_matching_adapter_scope_and_bytes() {
    let provider = FakeProvider::continuation_round_trip();
    let first_request = request("fake-profile", None);
    let events = provider.stream(&first_request, &live_context());
    let finished = finished(&events);
    assert_eq!(finished.reason(), FinishReason::ToolCalls);
    let carried = finished
        .continuation()
        .expect("first turn carries continuation");
    assert_eq!(carried.bytes(), &[7, 7, 1]);
    assert_eq!(carried.adapter(), provider.adapter_identity());
    assert_eq!(carried.adapter(), "fake-adapter");
    assert_eq!(
        carried.scope(),
        provider.continuation_scope(first_request.profile())
    );
    assert_eq!(carried.scope(), "fake-model");
    assert_ne!(carried.adapter(), nexus_core::DEFAULT_ADAPTER_IDENTITY);
    assert_ne!(
        carried.scope(),
        first_request.profile(),
        "the scope is not the bare profile"
    );
    assert!(carried.is_compatible_with(
        provider.adapter_identity(),
        &provider.continuation_scope(first_request.profile())
    ));
    assert!(!carried.is_compatible_with(
        "other-adapter",
        &provider.continuation_scope(first_request.profile())
    ));
    assert!(!carried.is_compatible_with(provider.adapter_identity(), "other-scope"));

    let ready = ready_candidates(&events);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].provider_ref(), "prov-ref-7");
}

#[test]
fn continuation_round_trip_echoes_into_the_second_request() {
    let provider = FakeProvider::continuation_round_trip();
    let first_request = request("fake-profile", None);
    let first = provider.stream(&first_request, &live_context());
    let carried = finished(&first)
        .continuation()
        .expect("first turn carries continuation")
        .clone();

    let second_request = request("fake-profile", Some(carried.clone()));
    let second = provider.stream(&second_request, &live_context());
    assert_eq!(concatenated_text(&second), "done");
    assert_eq!(finished(&second).reason(), FinishReason::Stop);
    assert!(
        finished(&second).continuation().is_none(),
        "the final turn carries no continuation"
    );
    assert_eq!(provider.call_count(), 2);

    let observed = provider.requests();
    assert_eq!(observed.len(), 2);
    assert_eq!(observed[0], first_request);
    assert_eq!(observed[1], second_request);
    assert_eq!(
        observed[1].continuation(),
        Some(&carried),
        "the observed second request echoes the issued continuation"
    );
    assert_eq!(
        observed[1]
            .continuation()
            .expect("echoed continuation")
            .bytes(),
        &[7, 7, 1]
    );
    assert_eq!(
        provider.continuation_scope("profile-a"),
        provider.continuation_scope("profile-b"),
        "the scripted scope is stable across profiles"
    );
}

#[test]
fn capabilities_claim_only_fixture_backed_features() {
    let capabilities = FakeProvider::new(vec![]).capabilities();
    assert!(capabilities.text && capabilities.streaming && capabilities.tool_calls);
    assert!(capabilities.usage_reporting);
    assert!(
        !capabilities.structured_output,
        "no fixture emits structured output"
    );

    let text = FakeProvider::fragmented_text_then_two_calls()
        .stream(&request("profile-a", None), &live_context());
    assert!(
        text.iter()
            .any(|event| matches!(event, ProviderEvent::TextDelta { .. })),
        "the text claim is backed by text deltas"
    );
    assert!(
        text_deltas(&text).len() > 1,
        "the streaming claim is backed by more than one delta"
    );
    assert!(
        text.iter()
            .any(|event| matches!(event, ProviderEvent::ToolCallReady(_))),
        "the tool_calls claim is backed by a candidate"
    );
    let usage = FakeProvider::usage_provisional_then_final()
        .stream(&request("profile-a", None), &live_context());
    assert!(
        usage
            .iter()
            .any(|event| matches!(event, ProviderEvent::Usage(_))),
        "the usage_reporting claim is backed by usage events"
    );

    assert_eq!(
        FakeProvider::new(vec![]).capabilities(),
        capabilities,
        "capabilities are stable across instances and scripts"
    );
    assert_eq!(
        FakeProvider::gated(vec![]).capabilities(),
        capabilities,
        "gating does not change declared capabilities"
    );
}

#[test]
fn capabilities_leave_unknown_limits_unestimated() {
    let capabilities = FakeProvider::new(vec![stop_turn("x")]).capabilities();
    assert_eq!(
        capabilities.max_context_items, None,
        "an unknown context bound is never estimated"
    );
    assert_eq!(
        capabilities.max_output_bytes, None,
        "an unknown output bound is never estimated"
    );
    let limits = Limits::m0_test();
    assert!(
        limits.max_arg_assembly_bytes > 0 && limits.max_tool_output_bytes > 0,
        "concrete M0 limits exist but are not claimed by the fake"
    );
}

#[test]
fn gate_state_is_public_only_for_gated_providers() {
    let plain = FakeProvider::new(vec![]);
    assert!(plain.gate().is_none(), "an ungated fake exposes no gate");

    let gated = FakeProvider::gated(vec![stop_turn("gated")]);
    let gate = gated.gate().expect("a gated fake exposes its handle");
    assert!(!gate.is_entered() && !gate.is_released());
    assert!(
        !gate.wait_entered(Duration::ZERO),
        "an unentered gate never reports entry"
    );

    gate.release();
    assert!(gate.is_released());
    gate.release();
    assert!(gate.is_released(), "release is idempotent");
    assert!(!gate.is_entered(), "release alone does not fake entry");
    assert!(
        FakeGate::RELEASE_TIMEOUT > Duration::ZERO,
        "the bounded wait is explicit"
    );
}

#[test]
fn observed_requests_are_snapshots_in_call_order() {
    let provider = FakeProvider::new(vec![stop_turn("one"), stop_turn("two")]);
    let first = request("profile-a", None);
    let second = request(
        "profile-b",
        Some(ContinuationData::new("fake-adapter", "fake-model", vec![1]).expect("builds")),
    );
    let _ = provider.stream(&first, &live_context());
    let _ = provider.stream(&second, &cancelled_context());

    let mut snapshot = provider.requests();
    assert_eq!(snapshot.len(), 2, "cancelled invocations are recorded");
    assert_eq!(snapshot[0], first);
    assert_eq!(snapshot[1], second);
    snapshot.clear();
    assert_eq!(
        provider.requests().len(),
        2,
        "the returned log is a snapshot, not shared state"
    );
    assert_eq!(provider.call_count(), 2);
}
