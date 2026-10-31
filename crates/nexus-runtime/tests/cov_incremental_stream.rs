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
    AgentError, AssistantText, CallCandidate, CancelCommand, CommandReply, ErrorCategory,
    EventPayload, FinishReason, Limits, ModelRequest, ProviderCapabilities, ProviderContext,
    ProviderEvent, ProviderPort, RequestId, RetryGuidance, RunEvent, RunFinished, RunId,
    RunOutcome, SessionId, SubmitCommand, TurnFinished, Usage, UsageFinality,
};
use nexus_runtime::{EventStreams, Policy, Runtime, RuntimeConfig};

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

struct GatedBurstProvider {
    inner: StreamingProvider,
    ready: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    finished: std::sync::mpsc::Sender<()>,
}

impl ProviderPort for GatedBurstProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }

    fn supports_incremental_streaming(&self) -> bool {
        true
    }

    fn stream(&self, _: &ModelRequest, _: &ProviderContext) -> Vec<ProviderEvent> {
        unreachable!()
    }

    fn stream_with_sink(
        &self,
        request: &ModelRequest,
        context: &ProviderContext,
        sink: &(dyn Fn(ProviderEvent) + Send + Sync),
    ) -> Vec<ProviderEvent> {
        self.ready.lock().unwrap().take().unwrap().send(()).unwrap();
        self.release.lock().unwrap().recv_timeout(WAIT).unwrap();
        let batch = self.inner.stream_with_sink(request, context, sink);
        let _ = self.finished.send(());
        batch
    }
}

const BURST_FRAGMENTS: usize = 512;

fn burst_text() -> String {
    (0..BURST_FRAGMENTS)
        .map(|index| format!("{index},"))
        .collect()
}

fn burst_provider() -> StreamingProvider {
    StreamingProvider::new(
        (0..BURST_FRAGMENTS)
            .map(|index| text("item-0", &format!("{index},")))
            .collect(),
        vec![
            text("item-0", &burst_text()),
            stop_turn(final_usage(None, None)),
        ],
    )
}

async fn start_backpressured_response(
    runtime_config: RuntimeConfig,
    inner: StreamingProvider,
    wait: Duration,
) -> (Runtime, EventStreams, RunId, std::sync::mpsc::Receiver<()>) {
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (finished_tx, finished_rx) = std::sync::mpsc::channel();
    let provider = Arc::new(GatedBurstProvider {
        inner,
        ready: Mutex::new(Some(ready_tx)),
        release: Mutex::new(release_rx),
        finished: finished_tx,
    });
    let (runtime, streams) = Runtime::new(runtime_config, provider, Vec::new());
    let response = runtime.submit(submit_cmd("burst")).await;
    assert_eq!(response.reply(), CommandReply::Accepted);
    let run = response.run().unwrap().clone();
    tokio::time::timeout(WAIT, ready_rx).await.unwrap().unwrap();
    release_tx.send(()).unwrap();
    // Deliberately hold the current-thread executor here: the worker must
    // encounter a full internal queue before the consumer can run again.
    assert_eq!(
        finished_rx.recv_timeout(wait),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout),
        "a full queue must pause the provider, not cancel it and finish early"
    );
    (runtime, streams, run, finished_rx)
}

async fn burst_terminal(streams: &mut EventStreams) -> (String, RunFinished) {
    tokio::time::timeout(WAIT, async {
        let mut text = String::new();
        loop {
            tokio::select! {
                event = streams.data.recv() => {
                    if let EventPayload::AssistantTextDelta(fragment) = event.unwrap().payload() {
                        text.push_str(&fragment.text);
                    }
                }
                event = streams.control.recv() => {
                    if let EventPayload::RunFinished(done) = event.unwrap().payload() {
                        while let Ok(event) = streams.data.try_recv() {
                            if let EventPayload::AssistantTextDelta(fragment) = event.payload() {
                                text.push_str(&fragment.text);
                            }
                        }
                        return (text, done.clone());
                    }
                }
            }
        }
    })
    .await
    .expect("backpressured run must finish promptly")
}

#[tokio::test(flavor = "current_thread")]
async fn small_response_burst_waits_for_capacity_without_losing_or_replaying_text() {
    let (_runtime, mut streams, _, finished_rx) =
        start_backpressured_response(config(), burst_provider(), Duration::from_millis(30)).await;
    let (text, finished) = burst_terminal(&mut streams).await;
    assert_eq!(finished.outcome(), RunOutcome::Completed);
    assert_eq!(text, burst_text());
    finished_rx.recv_timeout(WAIT).unwrap();
    assert!(streams.data.try_recv().is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_releases_a_provider_waiting_for_stream_capacity() {
    let (runtime, mut streams, run, finished_rx) =
        start_backpressured_response(config(), burst_provider(), Duration::from_millis(30)).await;
    let cancel = CancelCommand {
        request: RequestId::new("cancel-burst").unwrap(),
        run,
    };
    assert_eq!(runtime.cancel(cancel).await.reply(), CommandReply::Accepted);
    let (_, finished) = burst_terminal(&mut streams).await;
    assert_eq!(finished.outcome(), RunOutcome::Cancelled);
    finished_rx.recv_timeout(WAIT).unwrap();
}

/// Local wall-clock samples, not a benchmark of a real provider. Run with
/// `cargo test -p nexus-runtime --test cov_incremental_stream local_perf -- --ignored --nocapture`.
#[tokio::test(flavor = "current_thread")]
#[ignore = "opt-in local performance measurement"]
async fn local_perf_stream_and_backpressure_cancel() {
    use std::time::Instant;

    const SAMPLES: usize = 10;
    let expected = burst_text();
    println!(
        "workload: samples={SAMPLES} fragments={BURST_FRAGMENTS} bytes={} failures cause test failure",
        expected.len()
    );
    for sample in 0..SAMPLES {
        let (runtime, mut streams) = Runtime::new(config(), Arc::new(burst_provider()), Vec::new());
        let start = Instant::now();
        assert_eq!(
            runtime.submit(submit_cmd("perf-stream")).await.reply(),
            CommandReply::Accepted
        );
        let (observed, finished) = burst_terminal(&mut streams).await;
        let elapsed = start.elapsed();
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(observed, expected);
        println!(
            "stream sample={sample} elapsed_us={} bytes_per_sec={:.0}",
            elapsed.as_micros(),
            expected.len() as f64 / elapsed.as_secs_f64()
        );
    }
    for sample in 0..SAMPLES {
        let (runtime, mut streams, run, finished_rx) =
            start_backpressured_response(config(), burst_provider(), Duration::from_millis(30))
                .await;
        let start = Instant::now();
        assert_eq!(
            runtime
                .cancel(CancelCommand {
                    request: RequestId::new("perf-cancel").unwrap(),
                    run,
                })
                .await
                .reply(),
            CommandReply::Accepted
        );
        let (observed, finished) = burst_terminal(&mut streams).await;
        let elapsed = start.elapsed();
        assert_eq!(finished.outcome(), RunOutcome::Cancelled);
        assert!(expected.starts_with(&observed));
        finished_rx.recv_timeout(WAIT).unwrap();
        println!(
            "cancel sample={sample} terminal_us={} delivered_bytes={}",
            elapsed.as_micros(),
            observed.len()
        );
    }
    println!("failures=0; queue high-water mark not instrumented");
}

#[tokio::test(flavor = "current_thread")]
async fn deadline_releases_a_provider_waiting_for_stream_capacity() {
    let mut runtime_config = config();
    runtime_config.limits.run_duration = Duration::from_millis(300);
    let (_runtime, mut streams, _, finished_rx) =
        start_backpressured_response(runtime_config, burst_provider(), Duration::from_millis(400))
            .await;
    let (_, finished) = burst_terminal(&mut streams).await;
    assert_eq!(finished.outcome(), RunOutcome::LimitReached);
    finished_rx.recv_timeout(WAIT).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn output_exhaustion_releases_a_provider_waiting_for_stream_bytes() {
    // Two individually bufferable events exceed the buffered-byte capacity,
    // so the second must wait even though the event queue is nowhere near full.
    let large = "x".repeat(600_000);
    let provider = StreamingProvider::new(
        vec![text("item-0", &large), text("item-0", &large)],
        vec![stop_turn(final_usage(None, None))],
    );
    let (_runtime, mut streams, _, finished_rx) =
        start_backpressured_response(config(), provider, Duration::from_millis(30)).await;
    let (_, finished) = burst_terminal(&mut streams).await;
    assert_eq!(finished.outcome(), RunOutcome::LimitReached);
    assert_eq!(
        finished.error().unwrap().message(),
        "provisional output budget exhausted"
    );
    finished_rx.recv_timeout(WAIT).unwrap();
}

#[tokio::test]
async fn oversized_provisional_event_fails_without_waiting_for_impossible_capacity() {
    let provider = Arc::new(StreamingProvider::new(
        vec![text("item-0", &"x".repeat(1_048_577))],
        vec![stop_turn(final_usage(None, None))],
    ));
    let (_, _, finished) = run_turn(provider, "oversized-event", 0).await;
    assert_eq!(finished.outcome(), RunOutcome::LimitReached);
    assert_eq!(
        finished.error().unwrap().message(),
        "provisional provider event exceeded its byte bound"
    );
}

struct GatedReasoningProvider {
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl ProviderPort for GatedReasoningProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        StreamingProvider::new(Vec::new(), Vec::new()).capabilities()
    }

    fn stream(&self, _: &ModelRequest, _: &ProviderContext) -> Vec<ProviderEvent> {
        unreachable!()
    }

    fn supports_incremental_streaming(&self) -> bool {
        true
    }

    fn stream_with_sink(
        &self,
        _: &ModelRequest,
        _: &ProviderContext,
        sink: &(dyn Fn(ProviderEvent) + Send + Sync),
    ) -> Vec<ProviderEvent> {
        let trace = ProviderEvent::ReasoningDelta {
            text: "live".into(),
        };
        sink(trace.clone());
        self.release.lock().unwrap().recv_timeout(WAIT).unwrap();
        vec![trace, stop_turn(final_usage(None, None))]
    }
}

#[tokio::test]
async fn reasoning_arrives_before_the_provider_finishes() {
    let (release, gate) = std::sync::mpsc::channel();
    let provider = Arc::new(GatedReasoningProvider {
        release: Mutex::new(gate),
    });
    let (runtime, mut streams) = Runtime::new(config(), provider, Vec::new());
    runtime.enable_local_reasoning();
    assert_eq!(
        runtime.submit(submit_cmd("live-reasoning")).await.reply(),
        CommandReply::Accepted
    );
    let event = tokio::time::timeout(WAIT, streams.data.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(event.payload(), EventPayload::AssistantReasoningDelta(fragment) if fragment.text == "live")
    );
    release.send(()).unwrap();
    let finished = tokio::time::timeout(WAIT, async {
        loop {
            if let EventPayload::RunFinished(done) = streams.control.recv().await.unwrap().payload()
            {
                break done.clone();
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(finished.outcome(), RunOutcome::Completed);
    assert!(streams.data.try_recv().is_err());
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
    run_turn_with_reasoning(provider, tag, expected_data, false).await
}

async fn run_turn_with_reasoning(
    provider: Arc<dyn ProviderPort + Send + Sync>,
    tag: &str,
    expected_data: usize,
    enabled: bool,
) -> (Vec<RunEvent>, Vec<RunEvent>, RunFinished) {
    let (runtime, mut streams) = Runtime::new(config(), provider, Vec::new());
    if enabled {
        runtime.enable_local_reasoning();
    }
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

#[tokio::test]
async fn reasoning_is_opt_in_and_streamed_prefix_is_not_replayed() {
    for enabled in [false, true] {
        let provider = Arc::new(StreamingProvider::new(
            vec![ProviderEvent::ReasoningDelta {
                text: "first ".into(),
            }],
            vec![
                ProviderEvent::ReasoningDelta {
                    text: "first second".into(),
                },
                stop_turn(final_usage(None, None)),
            ],
        ));
        let (data, _, finished) =
            run_turn_with_reasoning(provider, "reasoning", if enabled { 2 } else { 0 }, enabled)
                .await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        let traces: Vec<_> = data
            .iter()
            .filter_map(|event| match event.payload() {
                EventPayload::AssistantReasoningDelta(fragment) => Some(fragment.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(traces.concat(), if enabled { "first second" } else { "" });
    }
}

#[tokio::test]
async fn mismatched_reasoning_prefix_fails_the_turn() {
    let provider = Arc::new(StreamingProvider::new(
        vec![ProviderEvent::ReasoningDelta { text: "one".into() }],
        vec![
            ProviderEvent::ReasoningDelta {
                text: "different".into(),
            },
            stop_turn(final_usage(None, None)),
        ],
    ));
    let (_, _, finished) = run_turn_with_reasoning(provider, "reasoning-mismatch", 1, true).await;
    assert_eq!(finished.outcome(), RunOutcome::Failed);
    assert_eq!(
        finished.error().unwrap().category(),
        ErrorCategory::Protocol
    );
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
