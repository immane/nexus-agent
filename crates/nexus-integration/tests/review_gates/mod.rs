#![allow(dead_code)]
// Test-local port gates for the review regressions.
//
// These doubles exist because the shared fakes do not yet cover a gated
// tool worker, and because the review findings require deterministic
// hand-offs at the synchronous port boundary (provider entered, tool
// entered, release) without touching production crates. Each worker
// announces entry before it blocks, so a test can act (cancel, expire,
// time out) while the worker still owns the invocation. A bounded release
// timeout keeps a forgotten release from hanging the whole test run.
//
// The gates are not runtime cancellation tokens: core contexts carry live
// control additively (`is_cancelled`, `check_active`, `deadline`), and these
// doubles observe those getters after the gate so a live-context regression
// can be asserted without changing the gate API.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, mpsc};
use std::time::Duration;

use std::sync::Arc;

use nexus_core::{
    AgentError, CallCandidate, ErrorCategory, FinishReason, ModelRequest, ProviderCapabilities,
    ProviderContext, ProviderEvent, ProviderPort, RetryGuidance, RunEvent, ToolCall, ToolContext,
    ToolId, ToolOutcome, ToolPort, ToolSpec, TurnFinished, Usage, UsageFinality,
};
use nexus_runtime::{EventStreams, Runtime, RuntimeConfig};

/// Bounded wait for a test release. A forgotten release degrades to a plain
/// result after this long instead of deadlocking the suite.
pub const GATE_TIMEOUT: Duration = Duration::from_secs(10);

/// Live runtime over caller-chosen gated doubles, with both event streams.
pub struct ReviewBed {
    pub runtime: Runtime,
    pub data: tokio::sync::mpsc::Receiver<RunEvent>,
    pub control: tokio::sync::mpsc::Receiver<RunEvent>,
}

/// Builds a review bed from a provider and tool set.
pub fn review_bed(
    config: RuntimeConfig,
    provider: Arc<dyn ProviderPort + Send + Sync>,
    tools: Vec<Arc<dyn ToolPort + Send + Sync>>,
) -> ReviewBed {
    let (runtime, streams): (Runtime, EventStreams) = Runtime::new(config, provider, tools);
    ReviewBed {
        runtime,
        data: streams.data,
        control: streams.control,
    }
}

/// Awaits the next port entry with a bounded failure instead of hanging.
pub async fn next_entry<T>(entries: &mut tokio::sync::mpsc::UnboundedReceiver<T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), entries.recv())
        .await
        .expect("port entry arrives promptly")
        .expect("port entry channel stays open")
}

fn failed(category: ErrorCategory, message: &'static str) -> ProviderEvent {
    ProviderEvent::Failed(
        AgentError::new(category, message, RetryGuidance::DoNotRetry)
            .expect("static safe gate message builds"),
    )
}

/// A single stop turn with unknown (never zero) usage.
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

/// A tool-calls turn for the given candidates with unknown usage.
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

/// One bounded call candidate for adversarial scripts.
pub fn candidate(
    item_key: &str,
    provider_ref: &str,
    tool_name: &str,
    arguments_json: &str,
) -> CallCandidate {
    CallCandidate::new(item_key, provider_ref, tool_name, arguments_json)
        .expect("gate candidate builds")
}

/// One blocked provider invocation held open for the test.
pub struct ProviderEntry {
    /// Request the runtime handed to `stream`.
    pub request: ModelRequest,
    /// Context the runtime handed to `stream`; its live token is shared, so
    /// `is_cancelled` observed here after a cancel reflects what the worker
    /// observes at its post-gate re-read.
    pub context: ProviderContext,
    release: Sender<()>,
}

impl ProviderEntry {
    /// Releases the blocked worker.
    pub fn release(self) {
        let _ = self.release.send(());
    }
}

/// Scripted provider with an optional gate at the top of `stream`.
///
/// Ungated instances record requests only, which doubles as the
/// request-inspection double. Gated instances block each invocation until
/// the test releases the entry.
///
/// The worker re-reads the live context after the gate: a cancelled token
/// yields a typed `Cancelled` failure, and an elapsed live deadline yields a
/// typed `Timeout` failure. Neither path consumes the script, so the caller
/// can tell "never reached the script" from "script exhausted".
pub struct GatedProvider {
    script: Mutex<VecDeque<Vec<ProviderEvent>>>,
    entries: tokio::sync::mpsc::UnboundedSender<ProviderEntry>,
    calls: AtomicUsize,
    gated: bool,
}

impl GatedProvider {
    /// Builds the provider and the test side of its entry channel.
    pub fn new(
        script: Vec<Vec<ProviderEvent>>,
        gated: bool,
    ) -> (Self, tokio::sync::mpsc::UnboundedReceiver<ProviderEntry>) {
        let (entries_tx, entries_rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Self {
                script: Mutex::new(script.into()),
                entries: entries_tx,
                calls: AtomicUsize::new(0),
                gated,
            },
            entries_rx,
        )
    }

    /// Returns the number of `stream` calls observed, including gated ones.
    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ProviderPort for GatedProvider {
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
        let (release_tx, release_rx) = mpsc::channel();
        let _ = self.entries.send(ProviderEntry {
            request: request.clone(),
            context: context.clone(),
            release: release_tx,
        });
        if self.gated && release_rx.recv_timeout(GATE_TIMEOUT).is_err() {
            return vec![failed(
                ErrorCategory::Cancelled,
                "gated provider was never released",
            )];
        }
        if context.is_cancelled() {
            return vec![failed(
                ErrorCategory::Cancelled,
                "provider observed cancellation after entry",
            )];
        }
        if context
            .check_active()
            .err()
            .is_some_and(|error| error.category() == ErrorCategory::Timeout)
        {
            return vec![failed(
                ErrorCategory::Timeout,
                "provider observed deadline after entry",
            )];
        }
        self.script
            .lock()
            .expect("gate script readable")
            .pop_front()
            .unwrap_or_else(|| {
                vec![failed(
                    ErrorCategory::Protocol,
                    "gated provider script exhausted",
                )]
            })
    }
}

/// One tool execution announced to the test before any gate wait.
pub struct ToolEntry {
    /// Call the runtime dispatched.
    pub call: ToolCall,
    /// Context the runtime attached at dispatch.
    pub context: ToolContext,
    release: Sender<()>,
}

impl ToolEntry {
    /// Releases the blocked worker.
    pub fn release(self) {
        let _ = self.release.send(());
    }
}

/// Scripted failing/succeeding tool with an optional gate and a
/// live-concurrency high-water mark. The mark is how a "timed-out worker
/// still owns the tool while the next call dispatches" regression becomes
/// observable.
pub struct GatedTool {
    spec: ToolSpec,
    entries: tokio::sync::mpsc::UnboundedSender<ToolEntry>,
    outcome: ToolOutcome,
    gated: bool,
    executions: AtomicUsize,
    live: AtomicUsize,
    max_live: AtomicUsize,
}

impl GatedTool {
    /// Builds a succeeding gate tool; `content` bounds the recorded outcome.
    pub fn new(
        tool_name: &str,
        content: &str,
        applied: bool,
        gated: bool,
    ) -> (Self, tokio::sync::mpsc::UnboundedReceiver<ToolEntry>) {
        let (entries_tx, entries_rx) = tokio::sync::mpsc::unbounded_channel();
        let spec = ToolSpec::new(
            ToolId::new(tool_name, nexus_core::M0_REVISION).expect("gate tool id builds"),
            "gated review tool",
            r#"{"type":"object"}"#,
        )
        .expect("gate tool spec builds");
        let outcome = ToolOutcome::new(
            nexus_core::ExecutionStatus::Succeeded,
            if applied {
                nexus_core::EffectState::KnownApplied
            } else {
                nexus_core::EffectState::KnownNotApplied
            },
            nexus_core::Evidence::HostObserved,
            content.to_owned(),
            false,
        )
        .expect("gate outcome builds");
        (
            Self {
                spec,
                entries: entries_tx,
                outcome,
                gated,
                executions: AtomicUsize::new(0),
                live: AtomicUsize::new(0),
                max_live: AtomicUsize::new(0),
            },
            entries_rx,
        )
    }

    /// Returns the number of `execute` calls observed.
    pub fn execution_count(&self) -> usize {
        self.executions.load(Ordering::SeqCst)
    }

    /// Returns the highest number of concurrent `execute` calls observed.
    pub fn max_live(&self) -> usize {
        self.max_live.load(Ordering::SeqCst)
    }

    /// Returns the number of workers currently inside `execute`.
    pub fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }
}

impl ToolPort for GatedTool {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn execute(&self, call: &ToolCall, context: &ToolContext) -> ToolOutcome {
        let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_live.fetch_max(live, Ordering::SeqCst);
        self.executions.fetch_add(1, Ordering::SeqCst);
        let (release_tx, release_rx) = mpsc::channel();
        let _ = self.entries.send(ToolEntry {
            call: call.clone(),
            context: context.clone(),
            release: release_tx,
        });
        if self.gated {
            let _ = release_rx.recv_timeout(GATE_TIMEOUT);
        }
        self.live.fetch_sub(1, Ordering::SeqCst);
        self.outcome.clone()
    }
}
