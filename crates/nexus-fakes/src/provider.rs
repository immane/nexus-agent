//! Scripted deterministic provider double.
//!
//! The port carries normalized events, so wire fragmentation is modeled the
//! way a correct adapter must surface it: byte chunks are reassembled (see
//! [`reassemble_bytes`]) before valid UTF-8 [`ProviderEvent::TextDelta`]
//! fragments are emitted, and split argument JSON appears as
//! [`ProviderEvent::ToolCallDelta`] progress followed by one complete
//! [`CallCandidate`]. Conflict, malformed, and
//! truncated scripts never present a dispatchable success: the scripted
//! failure fixtures end in `Failed`, and the adversarial success fixtures
//! end in `TurnFinished` while violating the provider contract, so only the
//! host's rejection can keep them from dispatching.
//!
//! Every invocation records the observed [`ModelRequest`] clone (see
//! [`FakeProvider::requests`]) before any cancellation or gate wait, and an
//! exhausted script yields an explicit terminal `Failed`, never a fabricated
//! idle success. [`FakeGate`] adds entered/release coordination for
//! deterministic cancellation worker ownership tests; it is a test
//! primitive, not a runtime cancellation token.

use std::collections::VecDeque;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use nexus_core::{
    AgentError, CallCandidate, ContinuationData, ErrorCategory, FinishReason, Limits, ModelRequest,
    ProviderCapabilities, ProviderContext, ProviderEvent, ProviderPort, RetryGuidance,
    TurnFinished, Usage, UsageFinality,
};

/// Text served by [`FakeProvider::fragmented_text_then_two_calls`].
pub const FRAGMENTED_TEXT: &str = "héllo 🌍";

/// Reassembles raw byte chunks split at arbitrary boundaries (possibly
/// mid-codepoint) into text. This is a fixture-building helper for tests,
/// **not** a wire parser: it makes no framing or protocol decisions, and
/// adapters own real wire parsing. A chunk sequence that is not valid UTF-8
/// as a whole is a protocol failure, never silent replacement.
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

/// Adapter label carried by the fake's scripted continuation state and
/// returned by [`ProviderPort::adapter_identity`].
const FAKE_ADAPTER: &str = "fake-adapter";

/// Compatibility scope label carried by the fake's scripted continuation
/// state and returned by [`ProviderPort::continuation_scope`].
const FAKE_SCOPE: &str = "fake-model";

fn continuation(bytes: Vec<u8>) -> ContinuationData {
    ContinuationData::new(FAKE_ADAPTER, FAKE_SCOPE, bytes).expect("fake continuation builds")
}

fn exhausted_failure() -> ProviderEvent {
    ProviderEvent::Failed(
        AgentError::new(
            ErrorCategory::Protocol,
            "fake provider script exhausted",
            RetryGuidance::DoNotRetry,
        )
        .expect("static safe fake message builds"),
    )
}

fn gate_failure() -> ProviderEvent {
    ProviderEvent::Failed(
        AgentError::new(
            ErrorCategory::Protocol,
            "fake provider gate was never released",
            RetryGuidance::DoNotRetry,
        )
        .expect("static safe fake message builds"),
    )
}

/// Test coordination gate shared by a gated fake and the test driving it.
///
/// The gate is **not** a runtime cancellation token: a synchronous port call
/// cannot be interrupted from outside, so the gate instead makes worker
/// ownership deterministic. The fake marks `entered` and blocks at its
/// hand-off point; the test waits for entry, performs its action (for
/// example, cancelling the owning run), and calls [`FakeGate::release`] so
/// the blocked worker can return. When the context carries live control
/// (`with_control`), the fakes re-read `is_cancelled` after the gate, so a
/// cancellation that arrived while blocked is observed without changing this
/// gate API.
#[derive(Debug, Clone, Default)]
pub struct FakeGate {
    inner: Arc<GateInner>,
}

#[derive(Debug, Default)]
struct GateInner {
    state: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct GateState {
    entered: bool,
    released: bool,
}

impl FakeGate {
    /// Longest a gated fake waits for [`FakeGate::release`] before failing
    /// explicitly instead of hanging an entire test run.
    pub const RELEASE_TIMEOUT: Duration = Duration::from_secs(10);

    /// Creates an unreleased gate.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns true once the fake reached its blocking point.
    #[must_use]
    pub fn is_entered(&self) -> bool {
        self.inner.state.lock().expect("fake gate readable").entered
    }

    /// Returns true once the fake was released.
    #[must_use]
    pub fn is_released(&self) -> bool {
        self.inner
            .state
            .lock()
            .expect("fake gate readable")
            .released
    }

    /// Waits until the fake reaches its blocking point or `timeout` elapses;
    /// returns true when entry was observed.
    #[must_use]
    pub fn wait_entered(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.inner.state.lock().expect("fake gate readable");
        while !state.entered {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, _) = self
                .inner
                .changed
                .wait_timeout(state, remaining)
                .expect("fake gate waitable");
            state = next;
        }
        true
    }

    /// Releases the fake and stays released; safe before or after entry.
    pub fn release(&self) {
        let mut state = self.inner.state.lock().expect("fake gate readable");
        state.released = true;
        self.inner.changed.notify_all();
    }

    /// Fake side: announces entry, then waits (bounded) for release. Returns
    /// false when the test never released the gate.
    pub(crate) fn enter_and_wait(&self) -> bool {
        let mut state = self.inner.state.lock().expect("fake gate readable");
        state.entered = true;
        self.inner.changed.notify_all();
        let deadline = Instant::now() + Self::RELEASE_TIMEOUT;
        while !state.released {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, _) = self
                .inner
                .changed
                .wait_timeout(state, remaining)
                .expect("fake gate waitable");
            state = next;
        }
        true
    }
}

/// Deterministic scripted provider. Each `stream` call records the observed
/// request and pops the next queued turn; a cancelled context always yields a
/// single terminal `Failed` without consuming the script, and an exhausted
/// script yields an explicit terminal `Failed` instead of a fallback success.
/// [`ProviderPort::adapter_identity`] and [`ProviderPort::continuation_scope`]
/// report the `fake-adapter`/`fake-model` labels carried by the scripted
/// continuation state. Unknown limits stay `None`.
pub struct FakeProvider {
    calls: AtomicUsize,
    script: Mutex<VecDeque<Vec<ProviderEvent>>>,
    requests: Mutex<Vec<ModelRequest>>,
    gate: Option<FakeGate>,
}

impl FakeProvider {
    /// Serves a custom turn script, one entry per `stream` call.
    pub fn new(script: Vec<Vec<ProviderEvent>>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            script: Mutex::new(script.into()),
            requests: Mutex::new(Vec::new()),
            gate: None,
        }
    }

    /// Serves the script after the shared gate is released. The `stream` call
    /// records its request and blocks at the gate, so a test can cancel the
    /// owning run while the worker still owns the invocation.
    pub fn gated(script: Vec<Vec<ProviderEvent>>) -> Self {
        let mut provider = Self::new(script);
        provider.gate = Some(FakeGate::new());
        provider
    }

    /// Returns the gate handle when this provider is gated.
    #[must_use]
    pub fn gate(&self) -> Option<FakeGate> {
        self.gate.clone()
    }

    /// Returns the observed request clones in call order. Requests are
    /// recorded before any cancellation or gate wait, so a blocked worker's
    /// request stays inspectable.
    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests
            .lock()
            .expect("fake request log readable")
            .clone()
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

    /// Two-turn demo script: the interleaved tool turn followed by a stop
    /// turn, so one granted or denied run completes instead of exhausting.
    ///
    /// TEST-ONLY fixture for single-process demos that serve many runs;
    /// real adapters never replay a script.
    pub fn demo_two_turn() -> Self {
        let provider = Self::interleaved_items();
        provider
            .script
            .lock()
            .expect("fake script writable")
            .push_back(stop_turn("done"));
        provider
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

    /// Adversarial: the same provider reference proposed twice, followed by a
    /// **successful** tool-calls terminal. A host that trusts the terminal
    /// would dispatch an ambiguous duplicate, so rejection is the only
    /// compliant outcome. Contrast
    /// [`FakeProvider::duplicate_reference_failure`], which ends in `Failed`.
    pub fn duplicate_reference_success() -> Self {
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
            ProviderEvent::TurnFinished(TurnFinished::new(
                FinishReason::ToolCalls,
                Usage::new(None, None, UsageFinality::Final),
                None,
            )),
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

    /// Adversarial: one provider reference bound to two different calls,
    /// followed by a **successful** tool-calls terminal. The host must reject
    /// the invocation instead of choosing one binding. Contrast
    /// [`FakeProvider::conflicting_reference_failure`].
    pub fn conflicting_reference_success() -> Self {
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
            ProviderEvent::TurnFinished(TurnFinished::new(
                FinishReason::ToolCalls,
                Usage::new(None, None, UsageFinality::Final),
                None,
            )),
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

    /// Adversarial: a complete candidate whose argument text is not valid
    /// JSON, followed by a **successful** tool-calls terminal. The host must
    /// reject the arguments at admission instead of executing them. Contrast
    /// [`FakeProvider::malformed_json_failure`], which emits no candidate.
    pub fn malformed_json_success() -> Self {
        Self::new(vec![vec![
            ProviderEvent::ToolCallReady(candidate(
                "item-1",
                "prov-ref-1",
                "host_read",
                "not-json",
            )),
            ProviderEvent::TurnFinished(TurnFinished::new(
                FinishReason::ToolCalls,
                Usage::new(None, None, UsageFinality::Final),
                None,
            )),
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

    /// Adversarial: argument progress for one item never reaches a
    /// `ToolCallReady`, yet the turn claims an ordinary completed stop. The
    /// host must not accept an unfinished item as a clean success. Contrast
    /// [`FakeProvider::truncated_stream`], which marks itself incomplete.
    pub fn unfinished_success() -> Self {
        Self::new(vec![vec![
            ProviderEvent::ToolCallDelta {
                item_key: "item-1".to_owned(),
                assembled_bytes: 8,
            },
            ProviderEvent::TurnFinished(TurnFinished::new(
                FinishReason::Stop,
                Usage::new(None, None, UsageFinality::Final),
                None,
            )),
        ]])
    }

    /// Adversarial: advertised argument progress exceeds the M0-test assembly
    /// budget while a valid candidate and a **successful** tool-calls
    /// terminal still follow. The host must reject the invocation on the
    /// budget violation rather than dispatch the candidate.
    pub fn oversized_progress_success() -> Self {
        Self::new(vec![vec![
            ProviderEvent::ToolCallDelta {
                item_key: "item-1".to_owned(),
                assembled_bytes: Limits::M0_TEST_ARG_ASSEMBLY_BYTES + 1,
            },
            ProviderEvent::ToolCallReady(candidate(
                "item-1",
                "prov-ref-1",
                "host_read",
                r#"{"path":"src"}"#,
            )),
            ProviderEvent::TurnFinished(TurnFinished::new(
                FinishReason::ToolCalls,
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

    fn stream(&self, request: &ModelRequest, context: &ProviderContext) -> Vec<ProviderEvent> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests
            .lock()
            .expect("fake request log writable")
            .push(request.clone());
        if context.is_cancelled() {
            return vec![cancelled_failure()];
        }
        if let Some(gate) = &self.gate {
            if !gate.enter_and_wait() {
                return vec![gate_failure()];
            }
            // Live control: a `with_control` token is re-read after the wait,
            // so cancellation that arrived while the worker was blocked
            // yields an explicit `Cancelled` terminal instead of the script.
            // Legacy snapshot contexts keep their dispatch-time observation.
            if context.is_cancelled() {
                return vec![cancelled_failure()];
            }
        }
        self.script
            .lock()
            .expect("fake script readable")
            .pop_front()
            .unwrap_or_else(|| vec![exhausted_failure()])
    }

    fn adapter_identity(&self) -> &str {
        // Matches the adapter label carried by `continuation(...)`.
        FAKE_ADAPTER
    }

    fn continuation_scope(&self, _profile: &str) -> String {
        // Matches the scope label carried by `continuation(...)`; the fake's
        // compatibility scope is its model label, not the profile.
        FAKE_SCOPE.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::{CancellationToken, NormalizedArgs, RunId, TurnId};
    use std::time::{Duration, Instant};

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

    /// Exercises the raw-byte fixture helper only. This is explicitly not a
    /// wire parser test: no framing, protocol, or adapter behavior is
    /// involved.
    #[test]
    fn raw_byte_helper_reassembles_splits_without_wire_parsing() {
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
    fn demo_two_turn_serves_a_tool_turn_then_a_stop_turn() {
        let provider = FakeProvider::demo_two_turn();
        let first = provider.stream(&request(None), &live_context());
        assert!(matches!(
            terminal(&first),
            ProviderEvent::TurnFinished(finished)
                if finished.reason() == FinishReason::ToolCalls
        ));
        let second = provider.stream(&request(None), &live_context());
        assert!(matches!(
            terminal(&second),
            ProviderEvent::TurnFinished(finished)
                if finished.reason() == FinishReason::Stop
        ));
        assert_eq!(provider.call_count(), 2);
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
    fn duplicate_reference_success_claims_success_for_host_rejection() {
        let provider = FakeProvider::duplicate_reference_success();
        let events = provider.stream(&request(None), &live_context());
        let refs: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::ToolCallReady(candidate) => Some(candidate.provider_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(refs, vec!["prov-dup", "prov-dup"]);
        assert!(
            matches!(
                terminal(&events),
                ProviderEvent::TurnFinished(finished)
                    if finished.reason() == FinishReason::ToolCalls
            ),
            "adversarial fixture claims success; the host must reject it"
        );
    }

    #[test]
    fn conflicting_reference_success_claims_success_for_host_rejection() {
        let provider = FakeProvider::conflicting_reference_success();
        let events = provider.stream(&request(None), &live_context());
        let ready: Vec<&CallCandidate> = events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::ToolCallReady(candidate) => Some(candidate),
                _ => None,
            })
            .collect();
        assert_eq!(ready.len(), 2);
        assert_eq!(ready[0].provider_ref(), ready[1].provider_ref());
        assert_ne!(ready[0].tool_name(), ready[1].tool_name());
        assert!(matches!(
            terminal(&events),
            ProviderEvent::TurnFinished(finished) if finished.reason() == FinishReason::ToolCalls
        ));
    }

    #[test]
    fn malformed_json_success_emits_rejectable_candidate() {
        let provider = FakeProvider::malformed_json_success();
        let events = provider.stream(&request(None), &live_context());
        let ready = events
            .iter()
            .find_map(|event| match event {
                ProviderEvent::ToolCallReady(candidate) => Some(candidate),
                _ => None,
            })
            .expect("adversarial fixture emits a candidate");
        assert!(
            NormalizedArgs::new(ready.arguments_json()).is_err(),
            "arguments must not validate as object-root JSON"
        );
        assert!(matches!(
            terminal(&events),
            ProviderEvent::TurnFinished(finished) if finished.reason() == FinishReason::ToolCalls
        ));
    }

    #[test]
    fn unfinished_success_claims_a_clean_stop() {
        let provider = FakeProvider::unfinished_success();
        let events = provider.stream(&request(None), &live_context());
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderEvent::ToolCallDelta { .. })),
            "progress is present"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ProviderEvent::ToolCallReady(_))),
            "no item ever completed assembly"
        );
        assert!(
            matches!(
                terminal(&events),
                ProviderEvent::TurnFinished(finished) if finished.reason() == FinishReason::Stop
            ),
            "adversarial fixture claims a clean stop despite the unfinished item"
        );
    }

    #[test]
    fn oversized_progress_success_exceeds_assembly_budget() {
        let provider = FakeProvider::oversized_progress_success();
        let events = provider.stream(&request(None), &live_context());
        let progress = events
            .iter()
            .find_map(|event| match event {
                ProviderEvent::ToolCallDelta {
                    assembled_bytes, ..
                } => Some(*assembled_bytes),
                _ => None,
            })
            .expect("progress is present");
        let limits = Limits::m0_test();
        assert!(
            limits.check_arg_assembly_bytes(progress).is_err(),
            "fixture progress must exceed the effective assembly budget"
        );
        assert_eq!(progress, Limits::M0_TEST_ARG_ASSEMBLY_BYTES + 1);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderEvent::ToolCallReady(_))),
            "a candidate follows so a naive host would dispatch it"
        );
        assert!(matches!(
            terminal(&events),
            ProviderEvent::TurnFinished(finished) if finished.reason() == FinishReason::ToolCalls
        ));
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
    fn observed_requests_record_every_invocation_in_call_order() {
        let provider = FakeProvider::new(vec![stop_turn("one"), stop_turn("two")]);
        let first = request(None);
        let second = request(Some(continuation(vec![1, 2, 3])));
        let _ = provider.stream(&first, &live_context());
        let _ = provider.stream(&second, &cancelled_context());
        let observed = provider.requests();
        assert_eq!(
            observed.len(),
            2,
            "cancelled invocations are still observed"
        );
        assert_eq!(observed[0], first);
        assert_eq!(observed[1], second);
        assert_eq!(provider.call_count(), 2);
    }

    #[test]
    fn exhausted_script_fails_explicitly_without_idle_success() {
        let provider = FakeProvider::new(vec![stop_turn("only")]);
        let first = provider.stream(&request(None), &live_context());
        assert!(matches!(
            terminal(&first),
            ProviderEvent::TurnFinished(finished) if finished.reason() == FinishReason::Stop
        ));
        let second = provider.stream(&request(None), &live_context());
        match terminal(&second) {
            ProviderEvent::Failed(error) => {
                assert_eq!(error.category(), ErrorCategory::Protocol);
                assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
            }
            other => panic!("exhausted script must fail explicitly, got {other:?}"),
        }
        assert_eq!(provider.call_count(), 2);
        assert_eq!(provider.requests().len(), 2);
    }

    #[test]
    fn gated_provider_blocks_until_released_with_request_observable() {
        let provider = FakeProvider::gated(vec![stop_turn("released")]);
        let gate = provider
            .gate()
            .expect("gated provider exposes a gate handle");
        assert!(!gate.is_entered());
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| provider.stream(&request(None), &live_context()));
            assert!(
                gate.wait_entered(Duration::from_secs(5)),
                "provider worker reached the gate"
            );
            assert_eq!(
                provider.requests().len(),
                1,
                "request is recorded while the worker is blocked"
            );
            assert!(!gate.is_released());
            gate.release();
            let events = worker.join().expect("gated provider worker joins");
            assert!(matches!(
                terminal(&events),
                ProviderEvent::TurnFinished(finished)
                    if finished.reason() == FinishReason::Stop
            ));
        });
        assert!(gate.is_released());
        assert_eq!(provider.call_count(), 1);
    }

    #[test]
    fn pre_cancelled_gated_provider_skips_the_gate_and_preserves_script() {
        let provider = FakeProvider::gated(vec![stop_turn("unused")]);
        let gate = provider
            .gate()
            .expect("gated provider exposes a gate handle");
        let cancelled = provider.stream(&request(None), &cancelled_context());
        assert!(matches!(
            terminal(&cancelled),
            ProviderEvent::Failed(error) if error.category() == ErrorCategory::Cancelled
        ));
        assert!(!gate.is_entered(), "a cancelled call never blocks a worker");
        assert_eq!(provider.call_count(), 1);

        gate.release();
        let live = provider.stream(&request(None), &live_context());
        assert!(
            matches!(terminal(&live), ProviderEvent::TurnFinished(_)),
            "cancellation did not consume the scripted turn"
        );
    }

    #[test]
    fn gated_provider_observes_live_cancellation_after_release() {
        let provider = FakeProvider::gated(vec![stop_turn("unused")]);
        let gate = provider
            .gate()
            .expect("gated provider exposes a gate handle");
        let token = CancellationToken::new();
        let context = ProviderContext::new(Duration::from_secs(60), false, None)
            .with_control(token.clone(), Instant::now() + Duration::from_secs(60));
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| provider.stream(&request(None), &context));
            assert!(
                gate.wait_entered(Duration::from_secs(5)),
                "provider worker reached the gate"
            );
            token.cancel();
            gate.release();
            let events = worker.join().expect("gated provider worker joins");
            assert!(
                matches!(
                    terminal(&events),
                    ProviderEvent::Failed(error) if error.category() == ErrorCategory::Cancelled
                ),
                "live cancellation is re-read after the gate"
            );
        });
        assert_eq!(provider.call_count(), 1);

        let live = provider.stream(&request(None), &live_context());
        assert!(
            matches!(terminal(&live), ProviderEvent::TurnFinished(_)),
            "the cancelled invocation did not consume the script"
        );
    }

    #[test]
    fn continuation_round_trip_preserves_refs_and_bytes() {
        let provider = FakeProvider::continuation_round_trip();
        let first_request = request(None);
        let first = provider.stream(&first_request, &live_context());
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
        assert_eq!(
            carried.adapter(),
            provider.adapter_identity(),
            "scripted continuation adapter matches the declared identity"
        );
        assert_eq!(
            carried.scope(),
            provider.continuation_scope(first_request.profile()),
            "scripted continuation scope matches the declared scope"
        );

        let second_request = request(Some(carried.clone()));
        let second = provider.stream(&second_request, &live_context());
        assert!(matches!(
            terminal(&second),
            ProviderEvent::TurnFinished(finished) if finished.reason() == FinishReason::Stop
        ));
        assert_eq!(provider.call_count(), 2);

        // Honesty check: inspect what the provider actually observed rather
        // than re-asserting the locally constructed request. The first
        // observed request is the exact input; the second observed request
        // carries the continuation state the provider itself issued.
        let observed = provider.requests();
        assert_eq!(observed.len(), 2, "both invocations recorded");
        assert_eq!(
            observed[0], first_request,
            "first observed request matches input"
        );
        let echoed = observed[1]
            .continuation()
            .expect("observed second request carries continuation");
        assert_eq!(
            echoed, &carried,
            "observed request echoes issued continuation"
        );
        assert_eq!(echoed.bytes(), &[7, 7, 1]);
    }

    #[test]
    fn adapter_identity_and_scope_match_the_scripted_continuation_labels() {
        let provider = FakeProvider::new(vec![]);
        assert_eq!(provider.adapter_identity(), FAKE_ADAPTER);
        assert_eq!(provider.adapter_identity(), "fake-adapter");
        assert_ne!(
            provider.adapter_identity(),
            nexus_core::DEFAULT_ADAPTER_IDENTITY,
            "the fake overrides the reserved placeholder"
        );
        assert_eq!(provider.continuation_scope("profile-a"), FAKE_SCOPE);
        assert_eq!(provider.continuation_scope("profile-a"), "fake-model");
        assert_eq!(
            provider.continuation_scope("profile-b"),
            provider.continuation_scope("profile-a"),
            "scope stays stable across profiles for the scripted continuation"
        );
    }

    #[test]
    fn capabilities_claim_no_unknown_limits() {
        let capabilities = FakeProvider::new(vec![]).capabilities();
        assert!(capabilities.text && capabilities.streaming && capabilities.tool_calls);
        assert_eq!(capabilities.max_context_items, None);
        assert_eq!(capabilities.max_output_bytes, None);
    }
}

#[cfg(test)]
mod cov_provider_private {
    //! Private-state coverage for the script queue, request log, gate
    //! coordination, and static failure builders. The public boundary is
    //! covered in `tests/cov_fake_provider.rs`; this module pins invariants
    //! reachable only through private fields, deterministically and without
    //! sleeps.

    use super::*;
    use nexus_core::{RunId, TurnId};

    fn request(profile: &str) -> ModelRequest {
        ModelRequest::new(
            RunId::new("run-1").expect("valid"),
            TurnId::new("turn-1").expect("valid"),
            profile,
            vec![],
            None,
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

    #[test]
    fn new_initializes_private_state_exactly() {
        let provider = FakeProvider::new(vec![stop_turn("a"), stop_turn("b")]);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            provider.script.lock().expect("fake script readable").len(),
            2
        );
        assert!(
            provider
                .requests
                .lock()
                .expect("fake request log readable")
                .is_empty()
        );
        assert!(provider.gate.is_none(), "an ungated fake owns no gate");
    }

    #[test]
    fn gated_constructor_installs_a_gate_and_keeps_the_script() {
        let provider = FakeProvider::gated(vec![stop_turn("a")]);
        assert_eq!(
            provider.script.lock().expect("fake script readable").len(),
            1
        );
        assert!(provider.gate.is_some(), "gated provider owns a gate");
        let handle = provider.gate().expect("gate handle");
        assert!(!handle.is_entered() && !handle.is_released());
    }

    #[test]
    fn stream_pops_one_turn_records_request_first_and_increments_calls() {
        let provider = FakeProvider::new(vec![stop_turn("a"), stop_turn("b")]);
        let observed = request("profile-a");
        let events = provider.stream(&observed, &live_context());
        assert_eq!(events, stop_turn("a"));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            provider.script.lock().expect("fake script readable").len(),
            1,
            "exactly one queued turn is consumed"
        );
        let recorded = provider.requests.lock().expect("fake request log readable");
        assert_eq!(
            recorded.as_slice(),
            std::slice::from_ref(&observed),
            "the request is recorded before the turn is served"
        );
    }

    #[test]
    fn exhausted_private_script_returns_the_exhausted_failure_event() {
        let provider = FakeProvider::new(vec![]);
        let events = provider.stream(&request("profile-a"), &live_context());
        assert_eq!(events, vec![exhausted_failure()]);
        assert!(
            provider
                .script
                .lock()
                .expect("fake script readable")
                .is_empty()
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cancelled_stream_records_request_without_consuming_the_private_script() {
        let provider = FakeProvider::new(vec![stop_turn("kept")]);
        let events = provider.stream(&request("profile-a"), &cancelled_context());
        assert_eq!(events, vec![cancelled_failure()]);
        assert_eq!(
            provider.script.lock().expect("fake script readable").len(),
            1,
            "cancellation never consumes a scripted turn"
        );
        assert_eq!(
            provider
                .requests
                .lock()
                .expect("fake request log readable")
                .len(),
            1,
            "cancelled invocations are still recorded"
        );
        assert_eq!(
            provider.stream(&request("profile-a"), &live_context()),
            stop_turn("kept")
        );
    }

    #[test]
    fn failure_builders_pin_exact_category_message_and_retry() {
        let cases: [(&str, ProviderEvent, ErrorCategory); 4] = [
            (
                "duplicate provider reference",
                protocol_failure("duplicate provider reference"),
                ErrorCategory::Protocol,
            ),
            (
                "provider invocation cancelled",
                cancelled_failure(),
                ErrorCategory::Cancelled,
            ),
            (
                "fake provider script exhausted",
                exhausted_failure(),
                ErrorCategory::Protocol,
            ),
            (
                "fake provider gate was never released",
                gate_failure(),
                ErrorCategory::Protocol,
            ),
        ];
        for (message, event, category) in cases {
            let ProviderEvent::Failed(error) = event else {
                panic!("static failure builders must emit Failed");
            };
            assert_eq!(error.category(), category, "{message}");
            assert_eq!(error.message(), message);
            assert_eq!(error.retry(), RetryGuidance::DoNotRetry, "{message}");
            assert!(error.correlation().is_empty(), "{message}");
        }
    }

    #[test]
    fn continuation_helper_uses_the_declared_adapter_and_scope_constants() {
        let data = continuation(vec![9, 8, 7]);
        assert_eq!(data.adapter(), FAKE_ADAPTER);
        assert_eq!(data.scope(), FAKE_SCOPE);
        assert_eq!(data.bytes(), &[9, 8, 7]);
        assert_eq!(FAKE_ADAPTER, "fake-adapter");
        assert_eq!(FAKE_SCOPE, "fake-model");
        assert!(data.is_compatible_with(FAKE_ADAPTER, FAKE_SCOPE));
    }

    #[test]
    fn gate_private_state_defaults_closed_and_release_is_idempotent() {
        let gate = FakeGate::new();
        {
            let state = gate.inner.state.lock().expect("fake gate readable");
            assert!(!state.entered, "a fresh gate is unentered");
            assert!(!state.released, "a fresh gate is unreleased");
        }
        gate.release();
        gate.release();
        let state = gate.inner.state.lock().expect("fake gate readable");
        assert!(!state.entered, "release never fakes entry");
        assert!(state.released, "release latches");
    }

    #[test]
    fn released_gate_enters_and_returns_without_waiting() {
        let gate = FakeGate::new();
        gate.release();
        assert!(gate.enter_and_wait(), "a pre-released gate never blocks");
        assert!(gate.is_entered() && gate.is_released());
    }

    #[test]
    fn gate_handle_clones_share_private_state() {
        let provider = FakeProvider::gated(vec![stop_turn("kept")]);
        let first = provider.gate().expect("gate handle");
        let second = provider.gate().expect("gate handle");
        assert!(
            Arc::ptr_eq(&first.inner, &second.inner),
            "handles share one coordination state"
        );
        first.release();
        assert!(
            second.is_released(),
            "release through one handle is visible through the other"
        );
    }

    #[test]
    fn pre_cancelled_gated_stream_skips_gate_and_preserves_script() {
        let provider = FakeProvider::gated(vec![stop_turn("kept")]);
        let gate = provider.gate().expect("gate handle");
        let events = provider.stream(&request("profile-a"), &cancelled_context());
        assert_eq!(events, vec![cancelled_failure()]);
        assert!(
            !gate.is_entered(),
            "a cancelled call never blocks at the gate"
        );
        assert_eq!(
            provider.script.lock().expect("fake script readable").len(),
            1
        );
        gate.release();
        assert_eq!(
            provider.stream(&request("profile-a"), &live_context()),
            stop_turn("kept"),
            "the scripted turn survived cancellation"
        );
    }
}
