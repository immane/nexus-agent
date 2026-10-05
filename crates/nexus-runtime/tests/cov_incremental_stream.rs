#![forbid(unsafe_code)]

//! End-to-end coverage for incremental provider streaming through the
//! public [`Runtime`] boundary.
//!
//! A provider that overrides [`ProviderPort::stream_with_sink`] and opts in
//! via [`ProviderPort::supports_incremental_streaming`] publishes
//! provisional prefixes while its turn is still in flight:
//!
//! - text fragments arrive on the data channel as they stream and are never
//!   replayed when the authoritative batch lands (each prefix publishes
//!   exactly once);
//! - candidates and terminals pushed through the sink are withheld: nothing
//!   streamed dispatches work or concludes a turn before the validated
//!   batch;
//! - provisional text survives an honest provider failure as presentation,
//!   with the typed failure staying primary;
//! - providers without the opt-in keep the legacy whole-batch behavior
//!   bit-for-bit (adjacent fragments still coalesce).
//!
//! All waits are bounded and every provider double is synchronous and
//! scripted, so the tests are deterministic.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use nexus_core::{
    AgentError, AssistantText, CallCandidate, CommandReply, ErrorCategory, EventPayload,
    FinishReason, Limits, ModelRequest, ProviderCapabilities, ProviderContext, ProviderEvent,
    ProviderPort, RequestId, RetryGuidance, RunEvent, RunFinished, RunOutcome, SessionId,
    SubmitCommand, TurnFinished, Usage, UsageFinality,
};
use nexus_runtime::{Policy, Runtime, RuntimeConfig};

/// Bound for every wait. Nothing here blocks on the network, so exceeding it
/// means a hang rather than a slow machine.
const WAIT: Duration = Duration::from_secs(5);

/// Provider double that streams provisional events through the sink before
/// returning its authoritative batch.
struct StreamingProvider {
    sink_script: Vec<ProviderEvent>,
    batch: Mutex<Option<Vec<ProviderEvent>>>,
}

impl StreamingProvider {
    fn new(sink_script: Vec<ProviderEvent>, batch: Vec<ProviderEvent>) -> Self {
        Self {
            sink_script,
            batch: Mutex::new(Some(batch)),
        }
    }
}

impl ProviderPort for StreamingProvider {
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

    fn stream(&self, _request: &ModelRequest, _context: &ProviderContext) -> Vec<ProviderEvent> {
        panic!("incremental tests must go through stream_with_sink");
    }

    fn supports_incremental_streaming(&self) -> bool {
        true
    }

    fn stream_with_sink(
        &self,
        _request: &ModelRequest,
        _context: &ProviderContext,
        sink: &(dyn Fn(ProviderEvent) + Send + Sync),
    ) -> Vec<ProviderEvent> {
        for event in &self.sink_script {
            sink(event.clone());
        }
        self.batch
            .lock()
            .expect("batch lock is not poisoned")
            .take()
            .expect("one turn per provider double")
    }
}

/// Legacy provider double: whole batches only, no sink override.
struct BatchProvider {
    batch: Mutex<Option<Vec<ProviderEvent>>>,
}

impl ProviderPort for BatchProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            text: true,
            streaming: false,
            tool_calls: false,
            structured_output: false,
            usage_reporting: true,
            max_context_items: None,
            max_output_bytes: None,
        }
    }

    fn stream(&self, _request: &ModelRequest, _context: &ProviderContext) -> Vec<ProviderEvent> {
        self.batch
            .lock()
            .expect("batch lock is not poisoned")
            .take()
            .expect("one turn per provider double")
    }
}

fn config() -> RuntimeConfig {
    RuntimeConfig {
        limits: Limits::m0_test(),
        policy: Policy::m0_test(),
        has_approval_handler: false,
    }
}

fn submit_cmd(tag: &str) -> SubmitCommand {
    SubmitCommand::new(
        RequestId::new(format!("req-{tag}")).expect("request id is valid"),
        SessionId::new("sess-stream").expect("session id is valid"),
        "do work",
        "test-profile",
    )
    .expect("submit command is valid")
}

fn text(item: &str, body: &str) -> ProviderEvent {
    ProviderEvent::TextDelta {
        item_key: item.to_owned(),
        text: body.to_owned(),
    }
}

fn stop_turn(usage: Usage) -> ProviderEvent {
    ProviderEvent::TurnFinished(TurnFinished::new(FinishReason::Stop, usage, None))
}

fn failed(category: ErrorCategory, message: &'static str) -> ProviderEvent {
    ProviderEvent::Failed(
        AgentError::new(category, message, RetryGuidance::DoNotRetry).expect("static error builds"),
    )
}

fn provisional(input: Option<u64>, output: Option<u64>) -> Usage {
    Usage::new(input, output, UsageFinality::Provisional)
}

fn final_usage(input: Option<u64>, output: Option<u64>) -> Usage {
    Usage::new(input, output, UsageFinality::Final)
}

/// Submits one turn on `provider` and drains both channels until the control
/// terminal plus `expected_data` data events arrive, within [`WAIT`]. A short
/// grace drain afterwards proves no replayed prefix follows.
async fn run_turn(
    provider: Arc<dyn ProviderPort + Send + Sync>,
    tag: &str,
    expected_data: usize,
) -> (Vec<RunEvent>, Vec<RunEvent>, RunFinished) {
    let (runtime, mut streams) = Runtime::new(config(), provider, Vec::new());
    let response = runtime.submit(submit_cmd(tag)).await;
    assert_eq!(response.reply(), CommandReply::Accepted);
    let run = response
        .run()
        .cloned()
        .expect("accepted submit issues a run");

    let (data, control, finished) = tokio::time::timeout(WAIT, async {
        let mut data = Vec::new();
        let mut control = Vec::new();
        let mut finished = None;
        while finished.is_none() || data.len() < expected_data {
            tokio::select! {
                event = streams.data.recv() => {
                    data.push(event.expect("data stays open"));
                }
                event = streams.control.recv() => {
                    let event = event.expect("control stays open");
                    if let EventPayload::RunFinished(done) = event.payload() {
                        finished = Some(done.clone());
                    }
                    control.push(event);
                }
            }
        }
        // Grace drain: a replayed prefix would surface here.
        let grace = tokio::time::timeout(Duration::from_millis(50), async {
            loop {
                tokio::select! {
                    event = streams.data.recv() => {
                        data.push(event.expect("data stays open"));
                    }
                    event = streams.control.recv() => {
                        control.push(event.expect("control stays open"));
                    }
                }
            }
        })
        .await;
        assert!(
            grace.is_err(),
            "no event follows the drained turn within the grace window"
        );
        (data, control, finished.expect("terminal observed"))
    })
    .await
    .expect("turn drains within the wait bound");

    assert!(
        control.last().is_some_and(RunEvent::is_terminal),
        "the terminal event is last on control"
    );
    assert!(
        control.iter().all(|event| event.run() == &run)
            && data.iter().all(|event| event.run() == &run),
        "every delivered event belongs to the submitted run"
    );
    (data, control, finished)
}

fn data_texts(data: &[RunEvent]) -> Vec<&str> {
    data.iter()
        .map(|event| match event.payload() {
            EventPayload::AssistantTextDelta(fragment) => fragment_text(fragment),
            other => panic!("data channel carries text only, got {other:?}"),
        })
        .collect()
}

fn fragment_text(fragment: &AssistantText) -> &str {
    fragment.text.as_str()
}

fn usage_updates(control: &[RunEvent]) -> Vec<Usage> {
    control
        .iter()
        .filter_map(|event| match event.payload() {
            EventPayload::UsageUpdated(usage) => Some(*usage),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn streamed_fragments_publish_live_and_never_replay() {
    let seen = provisional(Some(10), Some(3));
    let committed = final_usage(Some(10), Some(8));
    let provider = Arc::new(StreamingProvider::new(
        vec![
            text("item-0", "hello "),
            text("item-0", "world"),
            ProviderEvent::Usage(seen),
        ],
        vec![text("item-0", "hello world"), stop_turn(committed)],
    ));
    let (data, control, finished) = run_turn(provider, "live-fragments", 2).await;

    assert_eq!(finished.outcome(), RunOutcome::Completed);
    assert_eq!(
        data_texts(&data),
        vec!["hello ", "world"],
        "each streamed fragment publishes exactly once, in order, with no batch replay"
    );
    assert_eq!(
        usage_updates(&control),
        vec![seen, committed],
        "the provisional estimate streams live and the terminal usage still lands"
    );
}

#[tokio::test]
async fn sinked_candidates_and_terminals_never_dispatch_early() {
    let candidate = ProviderEvent::ToolCallReady(
        CallCandidate::new("item-1", "prov-1", "host_read", "{}").expect("candidate builds"),
    );
    let provider = Arc::new(StreamingProvider::new(
        vec![text("item-0", "partial "), candidate],
        vec![failed(ErrorCategory::Timeout, "provider timed out")],
    ));
    let (data, control, finished) = run_turn(provider, "withheld-dispatch", 1).await;

    assert_eq!(finished.outcome(), RunOutcome::Failed);
    assert_eq!(
        finished
            .error()
            .expect("typed provider failure is retained")
            .category(),
        ErrorCategory::Timeout
    );
    assert_eq!(
        data_texts(&data),
        vec!["partial "],
        "provisional text stays visible as presentation"
    );
    assert!(
        !control.iter().any(|event| matches!(
            event.payload(),
            EventPayload::ToolStarted(_) | EventPayload::ToolFinished(_)
        )),
        "a sinked candidate never executes without batch admission"
    );
}

#[tokio::test]
async fn providers_without_the_opt_in_keep_legacy_coalescing() {
    let provider = Arc::new(BatchProvider {
        batch: Mutex::new(Some(vec![
            text("item-0", "hello "),
            text("item-0", "world"),
            stop_turn(final_usage(Some(1), Some(1))),
        ])),
    });
    let (data, _, finished) = run_turn(provider, "legacy-batch", 1).await;

    assert_eq!(finished.outcome(), RunOutcome::Completed);
    assert_eq!(
        data_texts(&data),
        vec!["hello world"],
        "adjacent same-item fragments still coalesce without the opt-in"
    );
}
