#![forbid(unsafe_code)]

//! Coverage hardening for run-duration exhaustion across the runtime's three
//! wait sites.
//!
//! The suite drives the real [`Runtime`] through its public API with minimal
//! in-file doubles. Every wait is bounded by `tokio::time::timeout`, and
//! blocking work is gated on channels rather than fixed sleeps. The gated
//! doubles report whether their live deadline was still in the future when
//! they entered, so a stalled scheduler fails the test instead of silently
//! exercising the pre-dispatch path.
//!
//! Covered contract:
//! - provider in flight when the run deadline fires: `LimitReached` with a
//!   typed resource-limit reason, no model batch ingested, no dispatch, and
//!   provider ownership quarantined until the blocked worker terminates;
//! - run deadline during an approval wait: the pending grant is abandoned,
//!   the call is recorded `TimedOut`/`Unknown` without dispatch, a late deny
//!   is `AlreadyFinalized`, and the runtime stays usable (no deadlock);
//! - tool in flight when the run deadline fires: ownership is retained until
//!   termination, exactly one `ToolFinished` records `TimedOut`/`Unknown`
//!   with uncertain evidence (never applied effects or rollback), and late
//!   evidence refines only the retained snapshot without rewriting the
//!   published terminal or duplicating the outcome event.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use nexus_core::{
    ApprovalId, CallCandidate, CallId, CommandReply, CommandResponse, DenyCommand, EffectState,
    ErrorCategory, EventPayload, Evidence, ExecutionStatus, FinishReason, GetSnapshotCommand,
    Limits, ModelRequest, OutcomeSummary, PersistenceState, ProviderCapabilities, ProviderContext,
    ProviderEvent, ProviderPort, RequestId, RetryGuidance, RunEvent, RunFinished, RunId,
    RunLifecycle, RunOutcome, SessionId, SubmitCommand, ToolCall, ToolContext, ToolId, ToolOutcome,
    ToolPort, ToolSpec, TurnFinished, Usage, UsageFinality,
};
use nexus_runtime::{Policy, Runtime, RuntimeConfig};
use tokio::sync::mpsc;

/// Bounded wait for a gated double to enter its blocking section.
const GATE_WAIT: Duration = Duration::from_secs(5);
/// Bounded failure backstop for terminal delivery; passing runs settle in
/// milliseconds, so this only bounds a defect.
const TERMINAL_WAIT: Duration = Duration::from_secs(10);

/// Scripted provider: one prepared event batch per `stream` call, plus an
/// optional gate that blocks only the first call. While gated, the provider
/// reports whether its live deadline was still in the future at entry.
struct ScriptedProvider {
    calls: AtomicUsize,
    turns: StdMutex<VecDeque<Vec<ProviderEvent>>>,
    gate: Option<ProviderGate>,
}

struct ProviderGate {
    entered: mpsc::UnboundedSender<bool>,
    release: StdMutex<std::sync::mpsc::Receiver<()>>,
}

impl ScriptedProvider {
    fn new(script: Vec<Vec<ProviderEvent>>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            turns: StdMutex::new(script.into()),
            gate: None,
        }
    }

    fn gated(
        script: Vec<Vec<ProviderEvent>>,
        entered: mpsc::UnboundedSender<bool>,
        release: std::sync::mpsc::Receiver<()>,
    ) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            turns: StdMutex::new(script.into()),
            gate: Some(ProviderGate {
                entered,
                release: StdMutex::new(release),
            }),
        }
    }
}

impl ProviderPort for ScriptedProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        capabilities()
    }

    fn stream(&self, _request: &ModelRequest, context: &ProviderContext) -> Vec<ProviderEvent> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0
            && let Some(gate) = &self.gate
        {
            let _ = gate.entered.send(
                context
                    .deadline()
                    .is_some_and(|deadline| std::time::Instant::now() < deadline),
            );
            let release = gate.release.lock().expect("gate release lock");
            let _ = release.recv_timeout(GATE_WAIT);
        }
        self.turns
            .lock()
            .expect("provider script lock")
            .pop_front()
            .unwrap_or_else(|| stop_turn("script exhausted"))
    }
}

/// Minimal tool double with an execution counter and a fixed success outcome.
struct ScriptedTool {
    spec: ToolSpec,
    executions: AtomicUsize,
}

impl ScriptedTool {
    fn succeeding(name: &str) -> Self {
        Self {
            spec: ToolSpec::new(
                ToolId::new(name, nexus_core::M0_REVISION).expect("valid tool id"),
                format!("test double for {name}"),
                r#"{"type":"object"}"#,
            )
            .expect("valid tool spec"),
            executions: AtomicUsize::new(0),
        }
    }
}

impl ToolPort for ScriptedTool {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn execute(&self, _call: &ToolCall, _context: &ToolContext) -> ToolOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        ToolOutcome::new(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            "observed success",
            false,
        )
        .expect("valid tool outcome")
    }
}

/// Tool double that blocks its first execution until released and then
/// reports a host-observed success, mirroring a worker that finished after
/// the runtime stopped waiting. While gated, it reports whether its live
/// deadline was still in the future at entry.
struct GatedTool {
    spec: ToolSpec,
    entered: mpsc::UnboundedSender<bool>,
    release: StdMutex<std::sync::mpsc::Receiver<()>>,
    executions: AtomicUsize,
}

impl GatedTool {
    fn new(entered: mpsc::UnboundedSender<bool>, release: std::sync::mpsc::Receiver<()>) -> Self {
        Self {
            spec: ToolSpec::new(
                ToolId::new("host_read", nexus_core::M0_REVISION).expect("valid tool id"),
                "gated host read double",
                r#"{"type":"object"}"#,
            )
            .expect("valid tool spec"),
            entered,
            release: StdMutex::new(release),
            executions: AtomicUsize::new(0),
        }
    }
}

impl ToolPort for GatedTool {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn execute(&self, _call: &ToolCall, context: &ToolContext) -> ToolOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        let _ = self.entered.send(
            context
                .deadline()
                .is_some_and(|deadline| std::time::Instant::now() < deadline),
        );
        let release = self.release.lock().expect("gate release lock");
        let _ = release.recv_timeout(GATE_WAIT);
        if context.is_cancelled() {
            ToolOutcome::new(
                ExecutionStatus::Cancelled,
                EffectState::Unknown,
                Evidence::Uncertain,
                "blocked tool observed cancellation",
                false,
            )
            .expect("valid cancelled outcome")
        } else {
            ToolOutcome::new(
                ExecutionStatus::Succeeded,
                EffectState::KnownApplied,
                Evidence::HostObserved,
                "late observed success",
                false,
            )
            .expect("valid late outcome")
        }
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

fn test_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime builds")
}

fn submit_cmd(tag: &str) -> SubmitCommand {
    SubmitCommand::new(
        RequestId::new(format!("req-{tag}")).expect("valid request id"),
        SessionId::new("sess-1").expect("valid session id"),
        "do work",
        "cov-run-deadline",
    )
    .expect("valid submit command")
}

fn candidate(tool: &str, args: &str) -> CallCandidate {
    CallCandidate::new("item-0", "prov-ref-0", tool, args).expect("valid call candidate")
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

/// Bounded wait for a gated double's entry report.
async fn wait_for_flag(entered: &mut mpsc::UnboundedReceiver<bool>, what: &str) -> bool {
    tokio::time::timeout(GATE_WAIT, entered.recv())
        .await
        .unwrap_or_else(|_| panic!("{what} did not arrive within the bounded wait"))
        .expect("gate sender stays alive until the signal")
}

/// Bounded retry until retained worker ownership clears; only `Busy` is a
/// legitimate intermediate reply.
async fn wait_for_accept(runtime: &Runtime, tag: &str) -> CommandResponse {
    tokio::time::timeout(GATE_WAIT, async {
        loop {
            let response = runtime.submit(submit_cmd(tag)).await;
            if response.reply() == CommandReply::Accepted {
                return response;
            }
            assert_eq!(
                response.reply(),
                CommandReply::Busy,
                "only retained ownership may reject the retry"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("retained ownership clears within the bounded wait")
}

/// Bounded wait until late worker evidence is folded into the retained
/// snapshot for `run`.
async fn wait_for_known_applied(runtime: &Runtime, run: &RunId) -> OutcomeSummary {
    tokio::time::timeout(GATE_WAIT, async {
        loop {
            let (reply, snapshot) = runtime
                .get_snapshot(GetSnapshotCommand {
                    request: RequestId::new("req-poll-late-evidence").expect("valid"),
                    run: run.clone(),
                })
                .await;
            if reply.reply() == CommandReply::Accepted
                && let Some(snapshot) = snapshot
                && let Some(summary) = snapshot.known_outcomes().first()
                && summary.effect == EffectState::KnownApplied
            {
                return summary.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("late evidence is folded within the bounded wait")
}

/// Collects both channels until the next control payload matches `stop`.
async fn collect_until(
    data: &mut mpsc::Receiver<RunEvent>,
    control: &mut mpsc::Receiver<RunEvent>,
    stop: impl Fn(&EventPayload) -> bool,
) -> (Vec<RunEvent>, Vec<RunEvent>) {
    let mut datas = Vec::new();
    let mut controls = Vec::new();
    tokio::time::timeout(TERMINAL_WAIT, async {
        loop {
            tokio::select! {
                event = data.recv() => {
                    if let Some(event) = event {
                        datas.push(event);
                    }
                }
                event = control.recv() => {
                    match event {
                        Some(event) => {
                            let done = stop(event.payload());
                            controls.push(event);
                            if done {
                                break;
                            }
                        }
                        None => panic!("control channel closed before the awaited event"),
                    }
                }
            }
        }
    })
    .await
    .expect("awaited control event arrives within the bounded wait");
    (datas, controls)
}

/// Drains both channels until the terminal control event, then collects every
/// still-buffered event without blocking. The post-terminal drain is
/// deliberate: a duplicate terminal must be visible to the assertions.
async fn drain_until_finished(
    data: &mut mpsc::Receiver<RunEvent>,
    control: &mut mpsc::Receiver<RunEvent>,
) -> (Vec<RunEvent>, Vec<RunEvent>, RunFinished) {
    let mut datas = Vec::new();
    let mut controls = Vec::new();
    let finished = tokio::time::timeout(TERMINAL_WAIT, async {
        loop {
            tokio::select! {
                event = data.recv() => {
                    if let Some(event) = event {
                        datas.push(event);
                    }
                }
                event = control.recv() => {
                    match event {
                        Some(event) => {
                            let terminal = match event.payload() {
                                EventPayload::RunFinished(finished) => Some(finished.clone()),
                                _ => None,
                            };
                            controls.push(event);
                            if let Some(finished) = terminal {
                                break finished;
                            }
                        }
                        None => panic!("control channel closed before the terminal event"),
                    }
                }
            }
        }
    })
    .await
    .expect("run reaches its terminal within the bounded wait");
    while let Ok(event) = data.try_recv() {
        datas.push(event);
    }
    while let Ok(event) = control.try_recv() {
        controls.push(event);
    }
    (datas, controls, finished)
}

fn find_approval(events: &[RunEvent]) -> Option<(ApprovalId, CallId)> {
    events.iter().find_map(|event| match event.payload() {
        EventPayload::ApprovalRequired(notice) => {
            Some((notice.approval.clone(), notice.call.clone()))
        }
        _ => None,
    })
}

fn tool_started_calls(events: &[RunEvent]) -> Vec<CallId> {
    events
        .iter()
        .filter_map(|event| match event.payload() {
            EventPayload::ToolStarted(info) => Some(info.call.clone()),
            _ => None,
        })
        .collect()
}

fn tool_finished_map(events: &[RunEvent]) -> HashMap<CallId, ToolOutcome> {
    events
        .iter()
        .filter_map(|event| match event.payload() {
            EventPayload::ToolFinished(info) => Some((info.call.clone(), info.outcome.clone())),
            _ => None,
        })
        .collect()
}

fn terminal_count(data: &[RunEvent], control: &[RunEvent]) -> usize {
    data.iter()
        .chain(control.iter())
        .filter(|event| event.is_terminal())
        .count()
}

/// Asserts per-run sequence numbers are contiguous from zero across both
/// channels. Valid only when no data event was dropped.
fn assert_contiguous(data: &[RunEvent], control: &[RunEvent]) {
    let mut all: Vec<&RunEvent> = data.iter().chain(control.iter()).collect();
    assert!(!all.is_empty(), "run publishes events");
    all.sort_by_key(|event| event.seq());
    for (index, event) in all.iter().enumerate() {
        assert_eq!(
            event.seq(),
            index as u64,
            "per-run sequences stay contiguous"
        );
    }
}

#[test]
fn provider_deadline_after_entry_is_limit_reached_without_dispatch() {
    let mut limits = Limits::m0_test();
    limits.run_duration = Duration::from_millis(500);

    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let provider = Arc::new(ScriptedProvider::gated(
        vec![stop_turn("never reached")],
        entered_tx,
        release_rx,
    ));
    let read_tool = Arc::new(ScriptedTool::succeeding("host_read"));
    let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![read_tool.clone()];
    let (runtime, mut streams) = Runtime::new(
        RuntimeConfig {
            limits,
            policy: Policy::m0_test(),
            has_approval_handler: true,
        },
        provider.clone(),
        tools,
    );
    let rt = test_rt();
    rt.block_on(async {
        let submit = runtime.submit(submit_cmd("provider-deadline")).await;
        assert_eq!(submit.reply(), CommandReply::Accepted);
        let run = submit.run().cloned().expect("run issued");

        // The provider entered its invocation while the live run deadline was
        // still in the future: this cannot pass on the pre-dispatch path.
        assert!(
            wait_for_flag(&mut entered_rx, "provider entry").await,
            "the provider entered before its run deadline"
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);

        let (data, control, finished) =
            drain_until_finished(&mut streams.data, &mut streams.control).await;
        assert_eq!(finished.outcome(), RunOutcome::LimitReached);
        let error = finished
            .error()
            .expect("run-duration exhaustion carries a typed reason");
        assert_eq!(error.category(), ErrorCategory::ResourceLimit);
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        assert_eq!(
            error.message(),
            "run duration exhausted while awaiting provider"
        );
        assert_eq!(finished.persistence(), PersistenceState::Ephemeral);
        assert!(finished.persistence_error().is_none());

        // No model batch was ingested, so no call was admitted or dispatched.
        assert!(tool_started_calls(&control).is_empty());
        assert!(tool_finished_map(&control).is_empty());
        assert_eq!(read_tool.executions.load(Ordering::SeqCst), 0);
        assert_eq!(terminal_count(&data, &control), 1);
        assert_contiguous(&data, &control);

        // Ownership is retained until the blocked worker actually terminates.
        let blocked = runtime.submit(submit_cmd("while-quarantined")).await;
        assert_eq!(blocked.reply(), CommandReply::Busy);
        assert_eq!(blocked.run(), Some(&run));

        release_tx.send(()).expect("release the blocked provider");
        let accepted = wait_for_accept(&runtime, "after-provider-release").await;
        assert_eq!(accepted.reply(), CommandReply::Accepted);

        let (next_data, next_control, next_finished) =
            drain_until_finished(&mut streams.data, &mut streams.control).await;
        assert_eq!(next_finished.outcome(), RunOutcome::Completed);
        assert_eq!(terminal_count(&next_data, &next_control), 1);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn run_deadline_during_approval_wait_denies_without_dispatch_or_deadlock() {
    let mut limits = Limits::m0_test();
    limits.run_duration = Duration::from_millis(300);
    limits.approval_expiry = Duration::from_secs(60);

    let provider = Arc::new(ScriptedProvider::new(vec![tool_turn(vec![candidate(
        "host_write",
        r#"{"path":"src"}"#,
    )])]));
    let write_tool = Arc::new(ScriptedTool::succeeding("host_write"));
    let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![write_tool.clone()];
    let (runtime, mut streams) = Runtime::new(
        RuntimeConfig {
            limits,
            policy: Policy::m0_test(),
            has_approval_handler: true,
        },
        provider.clone(),
        tools,
    );
    let rt = test_rt();
    rt.block_on(async {
        let submit = runtime.submit(submit_cmd("approval-deadline")).await;
        assert_eq!(submit.reply(), CommandReply::Accepted);
        let run = submit.run().cloned().expect("run issued");

        let (data, control) = collect_until(&mut streams.data, &mut streams.control, |payload| {
            matches!(
                payload,
                EventPayload::ApprovalRequired(_) | EventPayload::RunFinished(_)
            )
        })
        .await;
        let (approval, call) =
            find_approval(&control).expect("approval requested before the deadline");
        assert!(tool_started_calls(&control).is_empty());
        assert_eq!(write_tool.executions.load(Ordering::SeqCst), 0);

        // No decision is sent: the run deadline ends the wait, not the
        // approval expiry.
        let (rest_data, rest_control, finished) =
            drain_until_finished(&mut streams.data, &mut streams.control).await;
        let mut all_data = data;
        all_data.extend(rest_data);
        let mut all_control = control;
        all_control.extend(rest_control);

        assert_eq!(finished.outcome(), RunOutcome::LimitReached);
        assert_eq!(terminal_count(&all_data, &all_control), 1);
        assert_contiguous(&all_data, &all_control);
        assert!(tool_started_calls(&all_control).is_empty());
        assert_eq!(write_tool.executions.load(Ordering::SeqCst), 0);
        let outcomes = tool_finished_map(&all_control);
        assert_eq!(outcomes.len(), 1, "the abandoned call is recorded once");
        let outcome = outcomes.values().next().expect("deadline outcome recorded");
        assert_eq!(outcome.status(), ExecutionStatus::TimedOut);
        assert_eq!(outcome.effect(), EffectState::Unknown);
        assert_eq!(outcome.evidence(), Evidence::Uncertain);
        assert_eq!(
            outcome.content(),
            "run deadline passed while awaiting approval"
        );
        assert!(!outcome.content().to_ascii_lowercase().contains("roll"));

        // A late decision after finalization never dispatches or hangs.
        let late = runtime
            .deny(DenyCommand {
                request: RequestId::new("req-late-deny").expect("valid"),
                run: run.clone(),
                call,
                approval,
            })
            .await;
        assert_eq!(late.reply(), CommandReply::AlreadyFinalized);
        assert_eq!(write_tool.executions.load(Ordering::SeqCst), 0);

        // No pending grant and no lingering worker: the runtime is idle.
        let (snapshot_reply, snapshot) = runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-snapshot").expect("valid"),
                run: run.clone(),
            })
            .await;
        assert_eq!(snapshot_reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("finalized run keeps a snapshot");
        assert!(snapshot.pending_approvals().is_empty());
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::LimitReached)
        );
        assert_eq!(snapshot.known_outcomes().len(), 1);

        // Not deadlocked: a fresh run is accepted and completes.
        let next = runtime.submit(submit_cmd("after-approval-deadline")).await;
        assert_eq!(next.reply(), CommandReply::Accepted);
        let (next_data, next_control, next_finished) =
            drain_until_finished(&mut streams.data, &mut streams.control).await;
        assert_eq!(next_finished.outcome(), RunOutcome::Completed);
        assert_eq!(terminal_count(&next_data, &next_control), 1);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn tool_deadline_watchdog_keeps_unknown_effects_without_rollback_claims() {
    let mut limits = Limits::m0_test();
    limits.run_duration = Duration::from_millis(300);
    limits.per_tool_timeout = Duration::from_secs(60);

    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_turn(vec![candidate("host_read", r#"{"path":"src"}"#)]),
        stop_turn("after"),
    ]));
    let tool = Arc::new(GatedTool::new(entered_tx, release_rx));
    let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![tool.clone()];
    let (runtime, mut streams) = Runtime::new(
        RuntimeConfig {
            limits,
            policy: Policy::m0_test(),
            has_approval_handler: false,
        },
        provider.clone(),
        tools,
    );
    let rt = test_rt();
    rt.block_on(async {
        let submit = runtime.submit(submit_cmd("tool-watchdog")).await;
        assert_eq!(submit.reply(), CommandReply::Accepted);
        let run = submit.run().cloned().expect("run issued");

        let (data, control) = collect_until(&mut streams.data, &mut streams.control, |payload| {
            matches!(
                payload,
                EventPayload::ToolStarted(_) | EventPayload::RunFinished(_)
            )
        })
        .await;
        assert_eq!(
            tool_started_calls(&control).len(),
            1,
            "the call was dispatched once"
        );
        assert!(
            wait_for_flag(&mut entered_rx, "tool entry").await,
            "the tool entered before its effective deadline"
        );

        let (rest_data, rest_control, finished) =
            drain_until_finished(&mut streams.data, &mut streams.control).await;
        let mut all_data = data;
        all_data.extend(rest_data);
        let mut all_control = control;
        all_control.extend(rest_control);

        assert_eq!(finished.outcome(), RunOutcome::LimitReached);
        assert_eq!(finished.persistence(), PersistenceState::Ephemeral);
        assert!(finished.persistence_error().is_none());
        assert_eq!(terminal_count(&all_data, &all_control), 1);
        assert_contiguous(&all_data, &all_control);

        let outcomes = tool_finished_map(&all_control);
        assert_eq!(outcomes.len(), 1, "exactly one outcome event for the call");
        let outcome = outcomes.values().next().expect("outcome recorded");
        assert_eq!(outcome.status(), ExecutionStatus::TimedOut);
        assert_eq!(
            outcome.effect(),
            EffectState::Unknown,
            "unconfirmed effects are never claimed as applied"
        );
        assert_eq!(outcome.evidence(), Evidence::Uncertain);
        assert_eq!(
            outcome.content(),
            "tool deadline exceeded; termination unconfirmed"
        );
        assert!(!outcome.content().to_ascii_lowercase().contains("roll"));

        // Ownership is retained: no new run races the blocked worker.
        let blocked = runtime.submit(submit_cmd("while-tool-quarantined")).await;
        assert_eq!(blocked.reply(), CommandReply::Busy);
        assert_eq!(blocked.run(), Some(&run));

        release_tx.send(()).expect("release the blocked tool");
        let late = wait_for_known_applied(&runtime, &run).await;
        assert_eq!(late.status, ExecutionStatus::Succeeded);
        assert_eq!(late.effect, EffectState::KnownApplied);
        assert_eq!(late.evidence, Evidence::HostObserved);

        // The late evidence refines only the retained snapshot; the published
        // event stream and its single outcome event are never rewritten.
        assert_eq!(tool_finished_map(&all_control).len(), 1);
        assert_eq!(
            outcomes
                .values()
                .next()
                .expect("published outcome")
                .status(),
            ExecutionStatus::TimedOut
        );
        assert_eq!(tool.executions.load(Ordering::SeqCst), 1);

        let accepted = wait_for_accept(&runtime, "after-tool-release").await;
        assert_eq!(accepted.reply(), CommandReply::Accepted);
        let (next_data, next_control, next_finished) =
            drain_until_finished(&mut streams.data, &mut streams.control).await;
        assert_eq!(next_finished.outcome(), RunOutcome::Completed);
        assert_eq!(terminal_count(&next_data, &next_control), 1);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    });
}
