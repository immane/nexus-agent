//! Scripted deterministic provider double.
//!
//! The port carries normalized events, so wire fragmentation is modeled the
//! way a correct adapter must surface it: byte chunks are reassembled (see
//! [`reassemble_bytes`]) before valid UTF-8 [`ProviderEvent::TextDelta`]
//! fragments are emitted, and split argument JSON appears as
//! [`ProviderEvent::ToolCallDelta`] progress followed by one complete
//! [`CallCandidate`](nexus_core::CallCandidate). Conflict, malformed, and
//! truncated scripts always end in a terminal failure or incomplete outcome,
//! never in a dispatchable success.

use std::collections::VecDeque;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

use nexus_core::{
    AgentError, CallCandidate, ContinuationData, ErrorCategory, FinishReason, ModelRequest,
    ProviderCapabilities, ProviderContext, ProviderEvent, ProviderPort, RetryGuidance,
    TurnFinished, Usage, UsageFinality,
};

/// Text served by [`FakeProvider::fragmented_text_then_two_calls`].
pub const FRAGMENTED_TEXT: &str = "héllo 🌍";

/// Reassembles raw byte chunks split at arbitrary boundaries (possibly
/// mid-codepoint) into text. Models the adapter step before emitting valid
/// `TextDelta` strings; a chunk sequence that is not valid UTF-8 as a whole
/// is a protocol failure, never silent replacement.
pub fn reassemble_bytes(chunks: &[&[u8]]) -> Result<String, AgentError> {
    let total: usize = chunks.iter().map(|chunk| chunk.len()).sum();
    let mut joined = Vec::with_capacity(total);
    for chunk in chunks {
        joined.extend_from_slice(chunk);
    }
    String::from_utf8(joined).map_err(|_| {
        AgentError::new(
            ErrorCategory::Protocol,
            "split fragments are not valid UTF-8",
            RetryGuidance::DoNotRetry,
        )
        .expect("static safe fake message builds")
    })
}

/// Builds a validated call candidate for custom scripts.
pub fn candidate(
    item_key: &str,
    provider_ref: &str,
    tool_name: &str,
    arguments_json: &str,
) -> CallCandidate {
    CallCandidate::new(item_key, provider_ref, tool_name, arguments_json)
        .expect("fake candidate builds")
}

/// A single stop turn carrying `text` with unknown (never zero) usage.
pub fn stop_turn(text: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::TextDelta {
            item_key: "item-0".to_owned(),
            text: text.to_owned(),
        },
        ProviderEvent::TurnFinished(TurnFinished::new(
            FinishReason::Stop,
            Usage::new(None, None, UsageFinality::Final),
            None,
        )),
    ]
}

/// A tool-call turn for the given candidates with unknown usage.
pub fn tool_turn(candidates: Vec<CallCandidate>) -> Vec<ProviderEvent> {
    let mut events: Vec<ProviderEvent> = candidates
        .into_iter()
        .map(ProviderEvent::ToolCallReady)
        .collect();
    events.push(ProviderEvent::TurnFinished(TurnFinished::new(
        FinishReason::ToolCalls,
        Usage::new(None, None, UsageFinality::Final),
        None,
    )));
    events
}

fn protocol_failure(message: &'static str) -> ProviderEvent {
    ProviderEvent::Failed(
        AgentError::new(ErrorCategory::Protocol, message, RetryGuidance::DoNotRetry)
            .expect("static safe fake message builds"),
    )
}

fn cancelled_failure() -> ProviderEvent {
    ProviderEvent::Failed(
        AgentError::new(
            ErrorCategory::Cancelled,
            "provider invocation cancelled",
            RetryGuidance::DoNotRetry,
        )
        .expect("static safe fake message builds"),
    )
}

fn continuation(bytes: Vec<u8>) -> ContinuationData {
    ContinuationData::new("fake-adapter", "fake-model", bytes).expect("fake continuation builds")
}

/// Deterministic scripted provider. Each `stream` call pops the next queued
/// turn; a cancelled context always yields a single terminal `Failed`
/// without consuming the script. Unknown limits stay `None`.
pub struct FakeProvider {
    calls: AtomicUsize,
    script: Mutex<VecDeque<Vec<ProviderEvent>>>,
}

impl FakeProvider {
    /// Serves a custom turn script, one entry per `stream` call.
    pub fn new(script: Vec<Vec<ProviderEvent>>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            script: Mutex::new(script.into()),
        }
    }

    /// Returns the number of `stream` calls observed, including cancelled ones.
    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Split multibyte text (`h` | `éllo ` | `🌍`; the `é` spans the first
    /// byte boundary) plus split argument JSON for two calls in one turn.
    pub fn fragmented_text_then_two_calls() -> Self {
        let args_a = r#"{"path":"src"}"#;
        let args_b = r#"{"path":"other"}"#;
        Self::new(vec![vec![
            ProviderEvent::TextDelta {
                item_key: "item-0".to_owned(),
                text: "h".to_owned(),
            },
            ProviderEvent::TextDelta {
                item_key: "item-0".to_owned(),
                text: "éllo ".to_owned(),
            },
            ProviderEvent::TextDelta {
                item_key: "item-0".to_owned(),
                text: "🌍".to_owned(),
            },
            ProviderEvent::ToolCallDelta {
                item_key: "item-1".to_owned(),
                assembled_bytes: args_a.len() / 2,
            },
            ProviderEvent::ToolCallDelta {
                item_key: "item-1".to_owned(),
                assembled_bytes: args_a.len(),
            },
            ProviderEvent::ToolCallReady(candidate("item-1", "prov-ref-1", "host_read", args_a)),
            ProviderEvent::ToolCallDelta {
                item_key: "item-2".to_owned(),
                assembled_bytes: args_b.len(),
            },
            ProviderEvent::ToolCallReady(candidate("item-2", "prov-ref-2", "host_read", args_b)),
            ProviderEvent::TurnFinished(TurnFinished::new(
                FinishReason::ToolCalls,
                Usage::new(None, None, UsageFinality::Final),
                None,
            )),
        ]])
    }

    /// Interleaved text and argument progress across three items, ending in
    /// two calls in declared order.
    pub fn interleaved_items() -> Self {
        Self::new(vec![vec![
            ProviderEvent::TextDelta {
                item_key: "item-0".to_owned(),
                text: "first ".to_owned(),
            },
            ProviderEvent::ToolCallDelta {
                item_key: "item-1".to_owned(),
                assembled_bytes: 4,
            },
            ProviderEvent::TextDelta {
                item_key: "item-0".to_owned(),
                text: "second".to_owned(),
            },
            ProviderEvent::ToolCallDelta {
                item_key: "item-2".to_owned(),
                assembled_bytes: 4,
            },
            ProviderEvent::ToolCallReady(candidate(
                "item-1",
                "prov-ref-1",
                "host_read",
                r#"{"path":"a"}"#,
            )),
            ProviderEvent::ToolCallReady(candidate(
                "item-2",
                "prov-ref-2",
                "host_write",
                r#"{"path":"b"}"#,
            )),
            ProviderEvent::TextDelta {
                item_key: "item-0".to_owned(),
                text: " end".to_owned(),
            },
            ProviderEvent::TurnFinished(TurnFinished::new(
                FinishReason::ToolCalls,
                Usage::new(None, None, UsageFinality::Final),
                None,
            )),
        ]])
    }

    /// The same provider reference proposed twice, then a terminal failure.
    /// A runtime must discard the candidates with the failed invocation and
    /// never dispatch them.
    pub fn duplicate_reference_failure() -> Self {
        Self::new(vec![vec![
            ProviderEvent::ToolCallReady(candidate(
                "item-1",
                "prov-dup",
                "host_read",
                r#"{"path":"a"}"#,
            )),
            ProviderEvent::ToolCallReady(candidate(
                "item-2",
                "prov-dup",
                "host_read",
                r#"{"path":"a"}"#,
            )),
            protocol_failure("duplicate provider reference"),
        ]])
    }

    /// One provider reference bound to two different calls, then failure.
    pub fn conflicting_reference_failure() -> Self {
        Self::new(vec![vec![
            ProviderEvent::ToolCallReady(candidate(
                "item-1",
                "prov-dup",
                "host_read",
                r#"{"path":"a"}"#,
            )),
            ProviderEvent::ToolCallReady(candidate(
                "item-2",
                "prov-dup",
                "host_write",
                r#"{"path":"b"}"#,
            )),
            protocol_failure("conflicting provider reference"),
        ]])
    }

    /// Argument fragments that never parse: progress only, then failure. No
    /// candidate is emitted because there is nothing valid to dispatch.
    pub fn malformed_json_failure() -> Self {
        Self::new(vec![vec![
            ProviderEvent::ToolCallDelta {
                item_key: "item-1".to_owned(),
                assembled_bytes: 8,
            },
            protocol_failure("malformed tool arguments"),
        ]])
    }

    /// Partial text with an incomplete (never stop-success) terminal.
    pub fn truncated_stream() -> Self {
        Self::new(vec![vec![
            ProviderEvent::TextDelta {
                item_key: "item-0".to_owned(),
                text: "partial answer".to_owned(),
            },
            ProviderEvent::TurnFinished(TurnFinished::new(
                FinishReason::Incomplete,
                Usage::new(None, None, UsageFinality::Final),
                None,
            )),
        ]])
    }

    /// Refusal with unknown usage.
    pub fn refusal() -> Self {
        Self::new(vec![vec![ProviderEvent::TurnFinished(TurnFinished::new(
            FinishReason::Refusal,
            Usage::new(None, None, UsageFinality::Final),
            None,
        ))]])
    }

    /// Provisional usage, then text, then final usage that matches the
    /// terminal turn record.
    pub fn usage_provisional_then_final() -> Self {
        let provisional = Usage::new(Some(10), Some(5), UsageFinality::Provisional);
        let committed = Usage::new(Some(10), Some(8), UsageFinality::Final);
        Self::new(vec![vec![
            ProviderEvent::Usage(provisional),
            ProviderEvent::TextDelta {
                item_key: "item-0".to_owned(),
                text: "answer".to_owned(),
            },
            ProviderEvent::Usage(committed),
            ProviderEvent::TurnFinished(TurnFinished::new(FinishReason::Stop, committed, None)),
        ]])
    }

    /// Two-turn script: turn one proposes a call and carries opaque
    /// continuation bytes; turn two completes. The caller echoes the turn-one
    /// continuation into the second request.
    pub fn continuation_round_trip() -> Self {
        let first = vec![
            ProviderEvent::ToolCallReady(candidate(
                "item-7",
                "prov-ref-7",
                "host_read",
                r#"{"path":"src"}"#,
            )),
            ProviderEvent::TurnFinished(TurnFinished::new(
                FinishReason::ToolCalls,
                Usage::new(None, None, UsageFinality::Final),
                Some(continuation(vec![7, 7, 1])),
            )),
        ];
        Self::new(vec![first, stop_turn("done")])
    }
}

impl ProviderPort for FakeProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            text: true,
            streaming: true,
            tool_calls: true,
            structured_output: false,
            usage_reporting: true,
            max_context_items: None,
            max_output_bytes: None,
        }
    }

    fn stream(&self, _request: &ModelRequest, context: &ProviderContext) -> Vec<ProviderEvent> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if context.is_cancelled() {
            return vec![cancelled_failure()];
        }
        self.script
            .lock()
            .expect("fake script readable")
            .pop_front()
            .unwrap_or_else(|| stop_turn("idle"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::{RunId, TurnId};
    use std::time::Duration;

    fn request(continuation: Option<ContinuationData>) -> ModelRequest {
        ModelRequest::new(
            RunId::new("run-1").expect("valid"),
            TurnId::new("turn-1").expect("valid"),
            "fake-profile",
            vec![],
            continuation,
            1024,
        )
        .expect("valid request builds")
    }

    fn live_context() -> ProviderContext {
        ProviderContext::new(Duration::from_secs(60), false, None)
    }

    fn cancelled_context() -> ProviderContext {
        ProviderContext::new(Duration::from_secs(60), true, None)
    }

    fn terminal(events: &[ProviderEvent]) -> &ProviderEvent {
        assert!(!events.is_empty());
        let terminal: Vec<&ProviderEvent> =
            events.iter().filter(|event| event.is_terminal()).collect();
        assert_eq!(terminal.len(), 1, "exactly one terminal event");
        assert!(
            events.iter().position(|event| event.is_terminal()).unwrap() == events.len() - 1,
            "terminal event is last"
        );
        terminal[0]
    }

    #[test]
    fn split_multibyte_bytes_reassemble() {
        let raw = FRAGMENTED_TEXT.as_bytes();
        for split in 1..raw.len() {
            let text =
                reassemble_bytes(&[&raw[..split], &raw[split..]]).expect("every split reassembles");
            assert_eq!(text, FRAGMENTED_TEXT, "split at byte {split}");
        }
        let singles: Vec<&[u8]> = raw.iter().map(std::slice::from_ref).collect();
        assert_eq!(
            reassemble_bytes(&singles).expect("single-byte chunks reassemble"),
            FRAGMENTED_TEXT
        );
        assert!(reassemble_bytes(&[&raw[..raw.len() - 1]]).is_err());
    }

    #[test]
    fn fragmented_text_and_two_calls_reassemble() {
        let provider = FakeProvider::fragmented_text_then_two_calls();
        let events = provider.stream(&request(None), &live_context());
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::TextDelta { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, FRAGMENTED_TEXT);
        let ready: Vec<&CallCandidate> = events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::ToolCallReady(candidate) => Some(candidate),
                _ => None,
            })
            .collect();
        assert_eq!(ready.len(), 2);
        assert_ne!(ready[0].provider_ref(), ready[1].provider_ref());
        assert!(matches!(
            terminal(&events),
            ProviderEvent::TurnFinished(finished)
                if finished.reason() == FinishReason::ToolCalls
        ));
        assert_eq!(provider.call_count(), 1);
    }

    #[test]
    fn interleaved_items_keep_two_calls_in_order() {
        let provider = FakeProvider::interleaved_items();
        let events = provider.stream(&request(None), &live_context());
        let refs: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::ToolCallReady(candidate) => Some(candidate.provider_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(refs, vec!["prov-ref-1", "prov-ref-2"]);
        assert!(matches!(
            terminal(&events),
            ProviderEvent::TurnFinished(finished)
                if finished.reason() == FinishReason::ToolCalls
        ));
    }

    #[test]
    fn duplicate_reference_ends_in_failure() {
        let provider = FakeProvider::duplicate_reference_failure();
        let events = provider.stream(&request(None), &live_context());
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderEvent::ToolCallReady(_))),
            "candidates are present so the runtime must discard them"
        );
        assert!(
            matches!(terminal(&events), ProviderEvent::Failed(error) if error.category() == ErrorCategory::Protocol)
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ProviderEvent::TurnFinished(_))),
            "no successful turn may follow a duplicate reference"
        );
    }

    #[test]
    fn conflicting_reference_ends_in_failure() {
        let provider = FakeProvider::conflicting_reference_failure();
        let events = provider.stream(&request(None), &live_context());
        assert!(matches!(
            terminal(&events),
            ProviderEvent::Failed(error) if error.category() == ErrorCategory::Protocol
        ));
    }

    #[test]
    fn malformed_json_emits_no_candidate() {
        let provider = FakeProvider::malformed_json_failure();
        let events = provider.stream(&request(None), &live_context());
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ProviderEvent::ToolCallReady(_))),
            "unparseable arguments must never become a candidate"
        );
        assert!(matches!(
            terminal(&events),
            ProviderEvent::Failed(error) if error.category() == ErrorCategory::Protocol
        ));
    }

    #[test]
    fn truncated_stream_is_incomplete_never_stop() {
        let provider = FakeProvider::truncated_stream();
        let events = provider.stream(&request(None), &live_context());
        match terminal(&events) {
            ProviderEvent::TurnFinished(finished) => {
                assert_eq!(finished.reason(), FinishReason::Incomplete);
                assert_ne!(finished.reason(), FinishReason::Stop);
            }
            ProviderEvent::Failed(_) => {}
            other => panic!("unexpected terminal {other:?}"),
        }
    }

    #[test]
    fn refusal_carries_unknown_usage() {
        let provider = FakeProvider::refusal();
        let events = provider.stream(&request(None), &live_context());
        match terminal(&events) {
            ProviderEvent::TurnFinished(finished) => {
                assert_eq!(finished.reason(), FinishReason::Refusal);
                assert_eq!(finished.usage().input_tokens(), None);
                assert_eq!(finished.usage().output_tokens(), None);
            }
            other => panic!("unexpected terminal {other:?}"),
        }
    }

    #[test]
    fn usage_moves_provisional_then_final() {
        let provider = FakeProvider::usage_provisional_then_final();
        let events = provider.stream(&request(None), &live_context());
        let usages: Vec<Usage> = events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::Usage(usage) => Some(*usage),
                _ => None,
            })
            .collect();
        assert_eq!(usages.len(), 2);
        assert_eq!(usages[0].finality(), UsageFinality::Provisional);
        assert_eq!(usages[1].finality(), UsageFinality::Final);
        match terminal(&events) {
            ProviderEvent::TurnFinished(finished) => assert_eq!(finished.usage(), usages[1]),
            other => panic!("unexpected terminal {other:?}"),
        }
    }

    #[test]
    fn cancellation_overrides_any_script() {
        let provider = FakeProvider::usage_provisional_then_final();
        let events = provider.stream(&request(None), &cancelled_context());
        assert_eq!(events.len(), 1);
        assert!(
            matches!(events[0], ProviderEvent::Failed(ref error) if error.category() == ErrorCategory::Cancelled)
        );
        assert_eq!(provider.call_count(), 1);
    }

    #[test]
    fn continuation_round_trip_preserves_refs_and_bytes() {
        let provider = FakeProvider::continuation_round_trip();
        let first = provider.stream(&request(None), &live_context());
        let (provider_ref, carried) = match terminal(&first) {
            ProviderEvent::TurnFinished(finished) => {
                let ready = first
                    .iter()
                    .find_map(|event| match event {
                        ProviderEvent::ToolCallReady(candidate) => Some(candidate),
                        _ => None,
                    })
                    .expect("first turn proposes a call");
                (
                    ready.provider_ref().to_owned(),
                    finished
                        .continuation()
                        .expect("first turn carries continuation")
                        .clone(),
                )
            }
            other => panic!("unexpected terminal {other:?}"),
        };
        assert_eq!(provider_ref, "prov-ref-7");
        assert_eq!(carried.bytes(), &[7, 7, 1]);

        let second_request = request(Some(carried.clone()));
        assert_eq!(
            second_request.continuation().expect("request carries it"),
            &carried
        );
        let second = provider.stream(&second_request, &live_context());
        assert!(matches!(
            terminal(&second),
            ProviderEvent::TurnFinished(finished) if finished.reason() == FinishReason::Stop
        ));
        assert_eq!(provider.call_count(), 2);
    }

    #[test]
    fn capabilities_claim_no_unknown_limits() {
        let capabilities = FakeProvider::new(vec![]).capabilities();
        assert!(capabilities.text && capabilities.streaming && capabilities.tool_calls);
        assert_eq!(capabilities.max_context_items, None);
        assert_eq!(capabilities.max_output_bytes, None);
    }
}
