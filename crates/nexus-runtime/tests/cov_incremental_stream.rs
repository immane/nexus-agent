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

use nexus_core::commands::MAX_TEXT_FRAGMENT_BYTES;
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

#[tokio::test]
async fn provisional_output_exhaustion_is_a_limit_not_cancellation() {
    let provider = Arc::new(StreamingProvider::new(
        vec![text("item-0", "a"), text("item-0", "b")],
        vec![text("item-0", "ab"), stop_turn(final_usage(None, None))],
    ));
    let mut runtime_config = config();
    runtime_config.limits.max_tool_output_bytes = 1;
    let (runtime, mut streams) = Runtime::new(runtime_config, provider, Vec::new());
    assert_eq!(
        runtime.submit(submit_cmd("stream-budget")).await.reply(),
        CommandReply::Accepted
    );
    let finished = tokio::time::timeout(WAIT, async {
        loop {
            if let EventPayload::RunFinished(finished) = streams
                .control
                .recv()
                .await
                .expect("control remains open")
                .payload()
            {
                break finished.clone();
            }
        }
    })
    .await
    .expect("run finishes within the wait bound");
    assert_eq!(finished.outcome(), RunOutcome::LimitReached);
    assert_eq!(
        finished.error().map(AgentError::category),
        Some(ErrorCategory::ResourceLimit)
    );
}

#[tokio::test]
async fn authoritative_text_must_extend_the_published_prefix() {
    let provider = Arc::new(StreamingProvider::new(
        vec![text("item-0", "partial")],
        vec![
            text("item-0", "different"),
            stop_turn(final_usage(None, None)),
        ],
    ));
    let (data, _, finished) = run_turn(provider, "prefix-mismatch", 1).await;
    assert_eq!(data_texts(&data), vec!["partial"]);
    assert_eq!(finished.outcome(), RunOutcome::Failed);
    assert_eq!(
        finished.error().map(AgentError::category),
        Some(ErrorCategory::Protocol)
    );
}

#[tokio::test]
async fn usage_updates_are_throttled_but_final_counters_remain_authoritative() {
    let updates: Vec<_> = (0..20)
        .map(|count| ProviderEvent::Usage(provisional(Some(count), None)))
        .collect();
    let mut batch = updates.clone();
    let final_usage = final_usage(Some(99), Some(7));
    batch.push(ProviderEvent::Usage(final_usage));
    batch.push(stop_turn(final_usage));
    let provider = Arc::new(StreamingProvider::new(updates, batch));
    let (_, control, finished) = run_turn(provider, "usage-throttle", 0).await;
    let published = usage_updates(&control);
    assert_eq!(published.len(), 17);
    assert_eq!(published.last(), Some(&final_usage));
    assert_eq!(finished.outcome(), RunOutcome::Completed);
}

#[tokio::test]
async fn streamed_usage_prefix_is_not_replayed_from_the_authoritative_batch() {
    let streamed: Vec<_> = (1..=3)
        .map(|count| ProviderEvent::Usage(provisional(Some(count), Some(count + 1))))
        .collect();
    let authoritative = final_usage(Some(9), Some(8));
    let mut batch = streamed.clone();
    batch.push(stop_turn(authoritative));
    let provider = Arc::new(StreamingProvider::new(streamed.clone(), batch));

    let (_, control, finished) = run_turn(provider, "usage-prefix", 0).await;

    let published = usage_updates(&control);
    assert_eq!(finished.outcome(), RunOutcome::Completed);
    assert_eq!(published.len(), 4, "three estimates and one final only");
    assert_eq!(
        published[..3],
        streamed
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::Usage(usage) => Some(*usage),
                _ => None,
            })
            .collect::<Vec<_>>()
    );
    assert_eq!(published.last(), Some(&authoritative));
}

#[tokio::test]
async fn failed_batch_does_not_replay_streamed_provisional_usage() {
    let first = provisional(Some(2), Some(1));
    let second = provisional(Some(2), Some(3));
    let provider = Arc::new(StreamingProvider::new(
        vec![ProviderEvent::Usage(first), ProviderEvent::Usage(second)],
        vec![
            ProviderEvent::Usage(first),
            ProviderEvent::Usage(second),
            failed(ErrorCategory::Timeout, "provider timed out"),
        ],
    ));

    let (_, control, finished) = run_turn(provider, "failed-usage-prefix", 0).await;

    assert_eq!(finished.outcome(), RunOutcome::Failed);
    assert_eq!(usage_updates(&control), vec![first, second]);
    assert!(matches!(
        control.last().map(RunEvent::payload),
        Some(EventPayload::RunFinished(done)) if done.error().is_some_and(|error| error.category() == ErrorCategory::Timeout)
    ));
}

#[tokio::test]
async fn batch_text_keeps_interleaving_and_splits_large_fragments() {
    let large = "x".repeat(MAX_TEXT_FRAGMENT_BYTES + 17);
    let provider = Arc::new(StreamingProvider::new(
        Vec::new(),
        vec![
            text("item-a", "a1"),
            text("item-b", "b1"),
            text("item-a", "a2"),
            text("item-c", &large),
            stop_turn(final_usage(None, None)),
        ],
    ));
    let (data, _, finished) = run_turn(provider, "batch-order", 5).await;
    assert_eq!(finished.outcome(), RunOutcome::Completed);
    assert_eq!(
        data_texts(&data),
        vec![
            "a1",
            "b1",
            "a2",
            &large[..MAX_TEXT_FRAGMENT_BYTES],
            &large[MAX_TEXT_FRAGMENT_BYTES..]
        ]
    );
}

#[tokio::test]
async fn provisional_text_splits_fragments_larger_than_the_core_limit() {
    let large = "x".repeat(MAX_TEXT_FRAGMENT_BYTES + 17);
    let provider = Arc::new(StreamingProvider::new(
        vec![text("item-large", &large)],
        vec![
            text("item-large", &large),
            stop_turn(final_usage(None, None)),
        ],
    ));
    let (data, _, finished) = run_turn(provider, "large-provisional", 1).await;
    assert_eq!(finished.outcome(), RunOutcome::Completed);
    assert_eq!(
        data_texts(&data),
        vec![
            &large[..MAX_TEXT_FRAGMENT_BYTES],
            &large[MAX_TEXT_FRAGMENT_BYTES..]
        ]
    );
}

#[tokio::test]
async fn invalid_provisional_item_keys_never_publish_text_or_preview() {
    for item_key in ["x".repeat(129), String::new()] {
        for event in [
            text(&item_key, "hidden"),
            ProviderEvent::ToolCallDelta {
                item_key: item_key.clone(),
                assembled_bytes: 1,
            },
        ] {
            let provider = Arc::new(StreamingProvider::new(
                vec![event],
                vec![stop_turn(final_usage(None, None))],
            ));
            let (data, _, finished) = run_turn(provider, "invalid-provisional-key", 0).await;
            assert!(
                data.is_empty(),
                "invalid identities must not reach presentation"
            );
            assert_eq!(finished.outcome(), RunOutcome::Failed);
            assert_eq!(
                finished.error().map(AgentError::category),
                Some(ErrorCategory::InvalidInput)
            );
        }
    }
}
