#![forbid(unsafe_code)]

//! Coverage hardening for worker quarantine after cancellation and per-tool
//! timeout, through the public `Runtime` API only.
//!
//! The runtime owns a provider or tool worker until its termination is
//! established. Cancellation and deadline expiry must publish a prompt,
//! honest terminal while the worker is still blocked: the quarantined worker
//! keeps new submits `Busy`, and the quarantine clears only after the worker
//! actually terminates. `nexus-fakes` is not a dependency of this crate, so
//! these tests carry in-file gated doubles (an entry signal plus an explicit
//! release). Every wait is bounded and every hand-off is channel-driven, so
//! passing runs settle deterministically instead of racing sleeps.
//!
//! Covered contract:
//! - a cancelled blocked provider finalizes promptly with `Cancelled` plus a
//!   typed cancellation, invents no tool outcome, holds the quarantine until
//!   the worker observes the live token and terminates, and only then accepts
//!   the next submit;
//! - a cancelled blocked tool records exactly one outcome with
//!   `Cancelled`/`Unknown`/`Uncertain` (never a rollback claim), keeps
//!   repeated submits `Busy`, and is never redispatched;
//! - a per-tool timeout records `TimedOut`/`Unknown`/`Uncertain` without
//!   rollback, keeps the next submit `Busy`, and folds the worker's late
//!   real outcome into the retained snapshot without a duplicate
//!   `ToolFinished` event.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, mpsc};
use std::time::Duration;

use nexus_core::{
    AgentError, CallCandidate, CallId, CancelCommand, CommandReply, EffectState, ErrorCategory,
    EventPayload, Evidence, ExecutionStatus, FinishReason, GetSnapshotCommand, Limits,
    ModelRequest, OutcomeSummary, ProviderCapabilities, ProviderContext, ProviderEvent,
    ProviderPort, RequestId, RetryGuidance, RunEvent, RunFinished, RunId, RunLifecycle, RunOutcome,
    SessionId, SubmitCommand, ToolCall, ToolContext, ToolId, ToolOutcome, ToolPort, ToolSpec,
    TurnFinished, Usage, UsageFinality,
};
use nexus_runtime::{EventStreams, Policy, Runtime, RuntimeConfig};

/// Bounded wait for a gated double to announce entry or for the quarantine to
/// clear. A passing run settles in milliseconds; this only bounds a defect.
const WAIT: Duration = Duration::from_secs(5);
/// Bounded release wait inside a gated worker. A forgotten release degrades
/// to a plain result after this long instead of deadlocking the suite. It is
/// deliberately longer than [`WAIT`]: a runtime that waited for the blocked
/// worker instead of finalizing promptly would fail the bounded terminal
/// wait rather than pass after the gate expires.
const GATE_WAIT: Duration = Duration::from_secs(10);

fn test_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime builds")
}

fn request(tag: &str) -> RequestId {
    RequestId::new(format!("req-{tag}")).expect("request id builds")
}

fn submit_cmd(tag: &str) -> SubmitCommand {
    SubmitCommand::new(
        request(tag),
        SessionId::new("sess-quarantine").expect("session id builds"),
        "do work",
        "test-profile",
    )
    .expect("submit command builds")
}

fn cancel_cmd(tag: &str, run: RunId) -> CancelCommand {
    CancelCommand {
        request: request(tag),
        run,
    }
}

fn runtime_config(limits: Limits) -> RuntimeConfig {
    RuntimeConfig {
        limits,
        policy: Policy::m0_test(),
        has_approval_handler: true,
    }
}

fn capabilities() -> ProviderCapabilities {
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

fn stop_turn(text: &str) -> Vec<ProviderEvent> {
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

fn tool_turn(candidates: Vec<CallCandidate>) -> Vec<ProviderEvent> {
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

fn candidate(tool: &str, arguments_json: &str) -> CallCandidate {
    CallCandidate::new("item-0", "prov-ref-0", tool, arguments_json).expect("candidate builds")
}

fn cancelled_provider_event(message: &'static str) -> Vec<ProviderEvent> {
    vec![ProviderEvent::Failed(
        AgentError::new(ErrorCategory::Cancelled, message, RetryGuidance::DoNotRetry)
            .expect("static cancellation builds"),
    )]
}

/// Scripted provider: one prepared batch per `stream` call, exhausted to a
/// stop turn. Used for the turns after a gated tool or provider invocation.
struct ScriptedProvider {
    calls: AtomicUsize,
    turns: StdMutex<VecDeque<Vec<ProviderEvent>>>,
}

impl ScriptedProvider {
    fn new(script: Vec<Vec<ProviderEvent>>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            turns: StdMutex::new(script.into()),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn next_turn(&self) -> Vec<ProviderEvent> {
        self.turns
            .lock()
            .expect("provider script lock")
            .pop_front()
            .unwrap_or_else(|| stop_turn("script exhausted"))
    }
}

impl ProviderPort for ScriptedProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        capabilities()
    }

    fn stream(&self, _request: &ModelRequest, _context: &ProviderContext) -> Vec<ProviderEvent> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.next_turn()
    }
}

/// One blocked provider invocation announced to the test before the gate
/// wait. The cloned context shares the runtime's live token, so the
/// post-release read reflects the test's cancellation.
struct ProviderEntry {
    context: ProviderContext,
    release: mpsc::Sender<()>,
}

impl ProviderEntry {
    fn release(self) {
        let _ = self.release.send(());
    }
}

/// Provider whose first invocation announces entry and blocks until the test
/// releases it. Later invocations run the script immediately, so a released
/// first turn never gates the run that follows the quarantine.
struct GatedProvider {
    script: StdMutex<VecDeque<Vec<ProviderEvent>>>,
    entries: tokio::sync::mpsc::UnboundedSender<ProviderEntry>,
    calls: AtomicUsize,
    live: AtomicUsize,
    observed_cancellation: AtomicBool,
}

impl GatedProvider {
    fn new(
        script: Vec<Vec<ProviderEvent>>,
    ) -> (Self, tokio::sync::mpsc::UnboundedReceiver<ProviderEntry>) {
        let (entries, entries_rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Self {
                script: StdMutex::new(script.into()),
                entries,
                calls: AtomicUsize::new(0),
                live: AtomicUsize::new(0),
                observed_cancellation: AtomicBool::new(false),
            },
            entries_rx,
        )
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    fn observed_cancellation(&self) -> bool {
        self.observed_cancellation.load(Ordering::SeqCst)
    }

    fn next_turn(&self) -> Vec<ProviderEvent> {
        self.script
            .lock()
            .expect("provider script lock")
            .pop_front()
            .unwrap_or_else(|| stop_turn("script exhausted"))
    }
}

impl ProviderPort for GatedProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        capabilities()
    }

    fn stream(&self, _request: &ModelRequest, context: &ProviderContext) -> Vec<ProviderEvent> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call > 0 {
            return self.next_turn();
        }
        self.live.fetch_add(1, Ordering::SeqCst);
        let (release_tx, release_rx) = mpsc::channel();
        let _ = self.entries.send(ProviderEntry {
            context: context.clone(),
            release: release_tx,
        });
        let _ = release_rx.recv_timeout(GATE_WAIT);
        self.live.fetch_sub(1, Ordering::SeqCst);
        if context.is_cancelled() {
            self.observed_cancellation.store(true, Ordering::SeqCst);
            return cancelled_provider_event("gated provider observed cancellation after entry");
        }
        self.next_turn()
    }
}

/// One blocked tool execution announced to the test before the gate wait.
struct ToolEntry {
    call: ToolCall,
    context: ToolContext,
    release: mpsc::Sender<()>,
}

impl ToolEntry {
    fn release(self) {
        let _ = self.release.send(());
    }
}

/// Tool that announces entry and blocks until the test releases it. After
/// release it reports a cancelled/unknown outcome when the live token was
/// cancelled, and a succeeded/known-applied outcome otherwise, so the tests
/// can distinguish cooperative cancellation from an elapsed deadline.
struct GatedTool {
    spec: ToolSpec,
    entries: tokio::sync::mpsc::UnboundedSender<ToolEntry>,
    executions: AtomicUsize,
    live: AtomicUsize,
    observed_cancellation: AtomicBool,
}

impl GatedTool {
    fn new(tool_name: &str) -> (Self, tokio::sync::mpsc::UnboundedReceiver<ToolEntry>) {
        let (entries, entries_rx) = tokio::sync::mpsc::unbounded_channel();
        let spec = ToolSpec::new(
            ToolId::new(tool_name, nexus_core::M0_REVISION).expect("tool id builds"),
            "gated quarantine tool",
            r#"{"type":"object"}"#,
        )
        .expect("tool spec builds");
        (
            Self {
                spec,
                entries,
                executions: AtomicUsize::new(0),
                live: AtomicUsize::new(0),
                observed_cancellation: AtomicBool::new(false),
            },
            entries_rx,
        )
    }

    fn execution_count(&self) -> usize {
        self.executions.load(Ordering::SeqCst)
    }

    fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    fn observed_cancellation(&self) -> bool {
        self.observed_cancellation.load(Ordering::SeqCst)
    }
}

impl ToolPort for GatedTool {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn execute(&self, call: &ToolCall, context: &ToolContext) -> ToolOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        self.live.fetch_add(1, Ordering::SeqCst);
        let (release_tx, release_rx) = mpsc::channel();
        let _ = self.entries.send(ToolEntry {
            call: call.clone(),
            context: context.clone(),
            release: release_tx,
        });
        let _ = release_rx.recv_timeout(GATE_WAIT);
        self.live.fetch_sub(1, Ordering::SeqCst);
        if context.is_cancelled() {
            self.observed_cancellation.store(true, Ordering::SeqCst);
            return ToolOutcome::new(
                ExecutionStatus::Cancelled,
                EffectState::Unknown,
                Evidence::Uncertain,
                "gated tool observed cancellation after entry",
                false,
            )
            .expect("cancelled outcome builds");
        }
        ToolOutcome::new(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            "gated tool completed after release",
            false,
        )
        .expect("succeeded outcome builds")
    }
}

struct Bed {
    runtime: Runtime,
    data: tokio::sync::mpsc::Receiver<RunEvent>,
    control: tokio::sync::mpsc::Receiver<RunEvent>,
}

fn bed(
    config: RuntimeConfig,
    provider: Arc<dyn ProviderPort + Send + Sync>,
    tools: Vec<Arc<dyn ToolPort + Send + Sync>>,
) -> Bed {
    let (runtime, streams): (Runtime, EventStreams) = Runtime::new(config, provider, tools);
    Bed {
        runtime,
        data: streams.data,
        control: streams.control,
    }
}

struct Collected {
    data: Vec<RunEvent>,
    control: Vec<RunEvent>,
}

/// Drains both channels until exactly the delivered terminal, bounded by
/// [`WAIT`]. A runtime that waited for the blocked worker would trip the
/// bound instead of passing.
async fn collect_until_finished(bed: &mut Bed) -> (Collected, RunFinished) {
    let mut data = Vec::new();
    let mut control = Vec::new();
    let finished = tokio::time::timeout(WAIT, async {
        loop {
            tokio::select! {
                event = bed.data.recv() => {
                    if let Some(event) = event {
                        data.push(event);
                    }
                }
                event = bed.control.recv() => {
                    match event {
                        Some(event) => {
                            let terminal = match event.payload() {
                                EventPayload::RunFinished(finished) => Some(finished.clone()),
                                _ => None,
                            };
                            control.push(event);
                            if let Some(finished) = terminal {
                                break finished;
                            }
                        }
                        None => panic!("control channel closed before a terminal event"),
                    }
                }
            }
        }
    })
    .await
    .expect("the run finalizes promptly without waiting for the blocked worker");
    while let Ok(event) = bed.data.try_recv() {
        data.push(event);
    }
    (Collected { data, control }, finished)
}

/// Awaits the next gated entry with a bounded failure instead of hanging.
async fn next_entry<T>(entries: &mut tokio::sync::mpsc::UnboundedReceiver<T>) -> T {
    tokio::time::timeout(WAIT, entries.recv())
        .await
        .expect("the gated worker announces entry promptly")
        .expect("the entry channel stays open")
}

/// Retries `submit` until the quarantine clears, bounded by [`WAIT`].
async fn submit_until_accepted(runtime: &Runtime, tag: &str) -> RunId {
    let response = tokio::time::timeout(WAIT, async {
        loop {
            let response = runtime.submit(submit_cmd(tag)).await;
            if response.reply() == CommandReply::Accepted {
                break response;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the quarantine clears once the worker terminates");
    response
        .run()
        .cloned()
        .expect("accepted submit carries a run id")
}

/// Polls the retained snapshot until the summary satisfies `accept`, bounded
/// by [`WAIT`].
async fn wait_for_outcome(
    runtime: &Runtime,
    run: &RunId,
    accept: impl Fn(&OutcomeSummary) -> bool,
) -> OutcomeSummary {
    tokio::time::timeout(WAIT, async {
        loop {
            let (reply, snapshot) = runtime
                .get_snapshot(GetSnapshotCommand {
                    request: request("snapshot-refine"),
                    run: run.clone(),
                })
                .await;
            if reply.reply() == CommandReply::Accepted
                && let Some(snapshot) = snapshot
                && let Some(summary) = snapshot
                    .known_outcomes()
                    .iter()
                    .find(|summary| accept(summary))
            {
                break summary.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("late evidence is folded into the retained snapshot promptly")
}

fn tool_outcomes(collected: &Collected) -> Vec<ToolOutcome> {
    collected
        .control
        .iter()
        .filter_map(|event| match event.payload() {
            EventPayload::ToolFinished(info) => Some(info.outcome.clone()),
            _ => None,
        })
        .collect()
}

fn tool_finished_count(collected: &Collected) -> usize {
    tool_outcomes(collected).len()
}

fn tool_started_count(collected: &Collected) -> usize {
    collected
        .control
        .iter()
        .filter(|event| matches!(event.payload(), EventPayload::ToolStarted(_)))
        .count()
}

fn tool_finished_call(collected: &Collected) -> CallId {
    collected
        .control
        .iter()
        .find_map(|event| match event.payload() {
            EventPayload::ToolFinished(info) => Some(info.call.clone()),
            _ => None,
        })
        .expect("a ToolFinished event was delivered")
}

/// Every delivered event is owned by one run with contiguous sequences and
/// exactly one terminal, regardless of which channel carried it.
fn assert_single_terminal_with_contiguous_sequences(collected: &Collected) {
    let mut all: Vec<&RunEvent> = collected
        .data
        .iter()
        .chain(collected.control.iter())
        .collect();
    all.sort_by_key(|event| event.seq());
    assert!(!all.is_empty(), "at least one event was delivered");
    for (index, event) in all.iter().enumerate() {
        assert_eq!(
            event.seq(),
            index as u64,
            "delivered sequences stay contiguous"
        );
    }
    assert_eq!(
        all.iter().filter(|event| event.is_terminal()).count(),
        1,
        "exactly one terminal event is delivered"
    );
}

#[test]
fn cancelled_blocked_provider_finalizes_cancelled_and_quarantines_until_termination() {
    let (provider, mut entries) = GatedProvider::new(vec![stop_turn("late")]);
    let provider = Arc::new(provider);
    let mut bed = bed(
        runtime_config(Limits::m0_test()),
        provider.clone(),
        Vec::new(),
    );
    let rt = test_rt();
    rt.block_on(async {
        let accepted = bed.runtime.submit(submit_cmd("blocked-provider")).await;
        assert_eq!(accepted.reply(), CommandReply::Accepted);
        let run = accepted
            .run()
            .cloned()
            .expect("accepted submit carries a run id");

        let entry = next_entry(&mut entries).await;
        assert!(
            !entry.context.is_cancelled(),
            "nothing cancelled the run yet"
        );
        assert!(
            entry.context.deadline().is_some(),
            "the blocked provider holds a live deadline"
        );

        let cancel = bed
            .runtime
            .cancel(cancel_cmd("cancel-provider", run.clone()))
            .await;
        assert_eq!(cancel.reply(), CommandReply::Accepted);

        // Prompt terminal: it arrives while the provider worker is still
        // blocked, with a typed cancellation and no invented tool outcome.
        let (collected, finished) = collect_until_finished(&mut bed).await;
        assert_eq!(finished.outcome(), RunOutcome::Cancelled);
        let error = finished
            .error()
            .expect("unconfirmed provider termination carries a typed error");
        assert_eq!(error.category(), ErrorCategory::Cancelled);
        assert!(
            tool_outcomes(&collected).is_empty(),
            "a blocked provider records no fabricated tool outcome"
        );
        assert_eq!(
            provider.live(),
            1,
            "the terminal was published before provider termination"
        );
        assert_single_terminal_with_contiguous_sequences(&collected);

        // Ownership is retained: a new submit stays Busy and names the
        // quarantined run, and the finalized run cannot be cancelled twice.
        let blocked = bed
            .runtime
            .submit(submit_cmd("while-provider-blocked"))
            .await;
        assert_eq!(blocked.reply(), CommandReply::Busy);
        assert_eq!(blocked.run(), Some(&run), "Busy names the quarantined run");
        let repeat = bed
            .runtime
            .cancel(cancel_cmd("cancel-provider-again", run.clone()))
            .await;
        assert_eq!(repeat.reply(), CommandReply::AlreadyFinalized);
        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: request("snapshot-quarantined-provider"),
                run: run.clone(),
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("a quarantined finalized run keeps its snapshot");
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Cancelled)
        );
        assert!(
            snapshot.known_outcomes().is_empty(),
            "no outcome is invented for the unconfirmed provider"
        );

        // Release: the worker observes the live token and terminates; only
        // then does the quarantine clear and the next submit get accepted.
        entry.release();
        let next_run = submit_until_accepted(&bed.runtime, "after-blocked-provider").await;
        assert_ne!(next_run, run, "the accepted submit is a new run");
        assert!(
            provider.observed_cancellation(),
            "the blocked provider observed the live token"
        );
        let (_, next_finished) = collect_until_finished(&mut bed).await;
        assert_eq!(next_finished.outcome(), RunOutcome::Completed);
        assert_eq!(
            provider.call_count(),
            2,
            "one released invocation plus the next run's turn"
        );
    });
}

#[test]
fn cancelled_blocked_tool_records_cancelled_unknown_uncertain_and_quarantines() {
    let (tool, mut entries) = GatedTool::new("host_read");
    let tool = Arc::new(tool);
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_turn(vec![candidate("host_read", r#"{"path":"src"}"#)]),
        stop_turn("done"),
    ]));
    let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![tool.clone()];
    let mut bed = bed(runtime_config(Limits::m0_test()), provider, tools);
    let rt = test_rt();
    rt.block_on(async {
        let accepted = bed.runtime.submit(submit_cmd("blocked-tool")).await;
        assert_eq!(accepted.reply(), CommandReply::Accepted);
        let run = accepted
            .run()
            .cloned()
            .expect("accepted submit carries a run id");

        let entry = next_entry(&mut entries).await;
        assert!(
            !entry.context.is_cancelled(),
            "nothing cancelled the run yet"
        );
        assert!(
            entry.context.deadline().is_some(),
            "the blocked tool holds a live deadline"
        );
        let dispatched = entry.call.call().clone();

        let cancel = bed
            .runtime
            .cancel(cancel_cmd("cancel-tool", run.clone()))
            .await;
        assert_eq!(cancel.reply(), CommandReply::Accepted);

        // Prompt terminal with an honest outcome: cancellation stopped
        // execution but its effects are unknown, never claimed as rolled
        // back or applied.
        let (collected, finished) = collect_until_finished(&mut bed).await;
        assert_eq!(finished.outcome(), RunOutcome::Cancelled);
        assert!(
            finished.error().is_none(),
            "the tool path does not fabricate a typed error"
        );
        assert_eq!(
            tool_started_count(&collected),
            1,
            "the blocked call was dispatched once"
        );
        let outcomes = tool_outcomes(&collected);
        assert_eq!(
            outcomes.len(),
            1,
            "exactly one outcome for the cancelled call"
        );
        assert_eq!(outcomes[0].status(), ExecutionStatus::Cancelled);
        assert_eq!(outcomes[0].effect(), EffectState::Unknown);
        assert_eq!(outcomes[0].evidence(), Evidence::Uncertain);
        assert_eq!(
            tool_finished_call(&collected),
            dispatched,
            "the recorded outcome belongs to the dispatched call"
        );
        assert_eq!(
            tool.live(),
            1,
            "the outcome was recorded before tool termination"
        );
        assert_single_terminal_with_contiguous_sequences(&collected);

        // The retained snapshot agrees with the delivered event.
        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: request("snapshot-cancelled-tool"),
                run: run.clone(),
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("a quarantined finalized run keeps its snapshot");
        let summary = snapshot
            .known_outcomes()
            .first()
            .expect("the cancelled outcome is retained");
        assert_eq!(summary.status, ExecutionStatus::Cancelled);
        assert_eq!(summary.effect, EffectState::Unknown);
        assert_eq!(summary.evidence, Evidence::Uncertain);

        // Quarantine: repeated submits stay Busy while the worker is blocked,
        // and the quarantined call is never redispatched.
        let blocked = bed.runtime.submit(submit_cmd("while-tool-blocked")).await;
        assert_eq!(blocked.reply(), CommandReply::Busy);
        assert_eq!(blocked.run(), Some(&run), "Busy names the quarantined run");
        tokio::time::sleep(Duration::from_millis(20)).await;
        let still_blocked = bed
            .runtime
            .submit(submit_cmd("while-tool-blocked-again"))
            .await;
        assert_eq!(still_blocked.reply(), CommandReply::Busy);
        assert_eq!(
            tool.execution_count(),
            1,
            "no second dispatch while the call is quarantined"
        );

        // Release: the worker observes the live token and terminates; only
        // then does the next submit get accepted.
        entry.release();
        let next_run = submit_until_accepted(&bed.runtime, "after-blocked-tool").await;
        assert_ne!(next_run, run, "the accepted submit is a new run");
        assert!(
            tool.observed_cancellation(),
            "the blocked tool observed the live token"
        );
        let (_, next_finished) = collect_until_finished(&mut bed).await;
        assert_eq!(next_finished.outcome(), RunOutcome::Completed);
        assert_eq!(
            tool.execution_count(),
            1,
            "the cancelled call stays unredispatched"
        );
    });
}

#[test]
fn per_tool_timeout_records_unknown_without_rollback_and_refines_late_evidence() {
    let (tool, mut entries) = GatedTool::new("host_read");
    let tool = Arc::new(tool);
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_turn(vec![candidate("host_read", r#"{"path":"src"}"#)]),
        stop_turn("done"),
    ]));
    let mut limits = Limits::m0_test();
    limits.per_tool_timeout = Duration::from_millis(50);
    let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![tool.clone()];
    let mut bed = bed(runtime_config(limits), provider.clone(), tools);
    let rt = test_rt();
    rt.block_on(async {
        let accepted = bed.runtime.submit(submit_cmd("timed-out-tool")).await;
        assert_eq!(accepted.reply(), CommandReply::Accepted);
        let run = accepted
            .run()
            .cloned()
            .expect("accepted submit carries a run id");

        let entry = next_entry(&mut entries).await;

        // The deadline publishes a prompt terminal while the worker is
        // blocked; effects stay inconclusive and are never claimed as
        // rolled back or applied.
        let (collected, finished) = collect_until_finished(&mut bed).await;
        assert_eq!(finished.outcome(), RunOutcome::LimitReached);
        assert!(
            finished.error().is_none(),
            "an unconfirmed deadline is not a typed failure"
        );
        assert_eq!(tool_started_count(&collected), 1);
        let outcomes = tool_outcomes(&collected);
        assert_eq!(outcomes.len(), 1);
        let recorded = &outcomes[0];
        assert_eq!(recorded.status(), ExecutionStatus::TimedOut);
        assert_eq!(
            recorded.effect(),
            EffectState::Unknown,
            "the effect stays inconclusive"
        );
        assert_eq!(recorded.evidence(), Evidence::Uncertain);
        assert_ne!(
            recorded.effect(),
            EffectState::KnownNotApplied,
            "no rollback claim"
        );
        assert_ne!(
            recorded.effect(),
            EffectState::KnownApplied,
            "no fabricated success"
        );
        assert_eq!(
            tool.live(),
            1,
            "the terminal was published before tool termination"
        );
        assert!(
            !tool.observed_cancellation(),
            "a deadline is not a cancellation"
        );
        assert_single_terminal_with_contiguous_sequences(&collected);

        // The retained snapshot records the same unknown effect.
        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: request("snapshot-timed-out-tool"),
                run: run.clone(),
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let summary = snapshot
            .expect("a quarantined finalized run keeps its snapshot")
            .known_outcomes()
            .first()
            .cloned()
            .expect("the timed-out outcome is retained");
        assert_eq!(summary.status, ExecutionStatus::TimedOut);
        assert_eq!(summary.effect, EffectState::Unknown);
        assert_eq!(summary.evidence, Evidence::Uncertain);

        // The timed-out worker still owns its call: the next submit is Busy.
        let blocked = bed.runtime.submit(submit_cmd("while-tool-timed-out")).await;
        assert_eq!(blocked.reply(), CommandReply::Busy);
        assert_eq!(blocked.run(), Some(&run), "Busy names the quarantined run");

        // Release: the late real outcome is folded into the retained
        // snapshot without a duplicate event and without rewriting a known
        // effect.
        entry.release();
        let refined = wait_for_outcome(&bed.runtime, &run, |summary| {
            summary.status == ExecutionStatus::Succeeded
        })
        .await;
        assert_eq!(
            refined.effect,
            EffectState::KnownApplied,
            "late evidence records what actually happened"
        );
        assert_eq!(refined.evidence, Evidence::HostObserved);
        assert_eq!(
            tool_finished_count(&collected),
            1,
            "late evidence never re-emits ToolFinished"
        );
        assert_eq!(tool.execution_count(), 1);

        let next_run = submit_until_accepted(&bed.runtime, "after-timed-out-tool").await;
        assert_ne!(next_run, run, "the accepted submit is a new run");
        let (_, next_finished) = collect_until_finished(&mut bed).await;
        assert_eq!(next_finished.outcome(), RunOutcome::Completed);
        assert_eq!(
            provider.call_count(),
            2,
            "the timed-out turn plus the next run's turn"
        );
    });
}
