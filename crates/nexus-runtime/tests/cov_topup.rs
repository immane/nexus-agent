#![forbid(unsafe_code)]

//! Top-up coverage for the construction, command-routing, and
//! consumer-disconnect paths of the runtime through the public API.
//!
//! `nexus-fakes` is not a dependency of this crate, so these tests carry
//! minimal in-file `ProviderPort`/`ToolPort` doubles. Every wait is bounded
//! (`tokio::time::timeout` for channel-driven runs, a bounded snapshot poll
//! for runs whose only consumer has disconnected), so passing runs settle
//! deterministically.
//!
//! Covered contract:
//! - a provider that declares no usable retained-context budget is refused at
//!   construction, while the same provider with any positive declared bound
//!   is accepted;
//! - `handle` routes `GetSnapshot` to the snapshot path for both a finalized
//!   run (a bounded view) and an unknown run (an explicit stale refusal);
//! - a submit whose bounded input or profile is invalid is rejected without
//!   issuing a run or reaching the provider, and the runtime still accepts the
//!   next valid submit;
//! - a provider-reported cancellation ends the run as `Cancelled`, an
//!   incompatible continuation state fails the run instead of being applied,
//!   and an `OutputLimit` turn is a `LimitReached` limit outcome, never a
//!   success;
//! - a control consumer that disconnects mid-run is classified, never
//!   silently dropped: an approval-required call is cancelled before any
//!   dispatch, a dispatch already authorized still runs and records its actual
//!   outcome, a denied unknown tool is still recorded, and every such run
//!   reports a truncated retained view;
//! - an automatic tool that the policy no longer authorizes is denied
//!   immediately before dispatch, with the exact denial diagnostic and no
//!   execution;
//! - the retained snapshot bounds known outcomes by the M0 call budget even
//!   when a run admitted more candidates, and every admitted denial is still
//!   delivered on the control channel.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use nexus_core::{
    AgentError, CallCandidate, CallId, Command, CommandReply, ContinuationData, EffectState,
    ErrorCategory, EventPayload, Evidence, ExecutionStatus, FinishReason, GetSnapshotCommand,
    Limits, M0_REVISION, ModelRequest, OutcomeSummary, ProviderCapabilities, ProviderContext,
    ProviderEvent, ProviderPort, RequestId, RetryGuidance, RunEvent, RunFinished, RunId,
    RunLifecycle, RunOutcome, SessionId, Snapshot, SubmitCommand, ToolCall, ToolContext, ToolId,
    ToolOutcome, ToolPort, ToolSpec, TurnFinished, Usage, UsageFinality,
};
use nexus_runtime::{Policy, Runtime, RuntimeConfig};
use tokio::sync::mpsc;

/// Bounded failure backstop: passing runs settle in milliseconds, so this only
/// bounds a defect.
const WAIT: Duration = Duration::from_secs(10);
/// Poll interval for a run whose only consumer has disconnected and whose
/// progress is therefore not observable on a channel.
const POLL: Duration = Duration::from_millis(5);

/// Scripted provider double: pops one complete event batch per call and
/// reports a caller-chosen capability set.
struct ScriptedProvider {
    calls: AtomicUsize,
    capabilities: ProviderCapabilities,
    script: StdMutex<VecDeque<Vec<ProviderEvent>>>,
    continuations: StdMutex<Vec<Option<ContinuationData>>>,
}

impl ScriptedProvider {
    fn new(capabilities: ProviderCapabilities, script: Vec<Vec<ProviderEvent>>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            capabilities,
            script: StdMutex::new(script.into()),
            continuations: StdMutex::new(Vec::new()),
        }
    }

    /// Text, streaming, and tool calls; no declared output or context bound,
    /// so the runtime's own budgets stay in force.
    fn standard(script: Vec<Vec<ProviderEvent>>) -> Self {
        Self::new(
            ProviderCapabilities {
                text: true,
                streaming: true,
                tool_calls: true,
                structured_output: false,
                usage_reporting: true,
                max_context_items: None,
                max_output_bytes: None,
            },
            script,
        )
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// The continuation state carried by every request this adapter received.
    fn requested_continuations(&self) -> Vec<Option<ContinuationData>> {
        self.continuations
            .lock()
            .expect("continuation mutex is never poisoned by a test")
            .clone()
    }
}

impl ProviderPort for ScriptedProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities.clone()
    }

    fn stream(&self, request: &ModelRequest, _context: &ProviderContext) -> Vec<ProviderEvent> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.continuations
            .lock()
            .expect("continuation mutex is never poisoned by a test")
            .push(request.continuation().cloned());
        self.script
            .lock()
            .expect("script mutex is never poisoned by a test")
            .pop_front()
            .unwrap_or_else(|| stop_turn("script exhausted"))
    }
}

/// Tool double that counts dispatches and always reports an observed success.
struct RecordingTool {
    spec: ToolSpec,
    executed: AtomicUsize,
}

impl RecordingTool {
    fn succeeding(name: &str) -> Self {
        Self {
            spec: ToolSpec::new(
                ToolId::new(name, M0_REVISION).expect("test tool identity is valid"),
                format!("test double for {name}"),
                r#"{"type":"object"}"#,
            )
            .expect("test tool spec is valid"),
            executed: AtomicUsize::new(0),
        }
    }

    fn execution_count(&self) -> usize {
        self.executed.load(Ordering::SeqCst)
    }
}

impl ToolPort for RecordingTool {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn execute(&self, _call: &ToolCall, _context: &ToolContext) -> ToolOutcome {
        self.executed.fetch_add(1, Ordering::SeqCst);
        ToolOutcome::new(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            "test double executed",
            false,
        )
        .expect("test outcome is valid")
    }
}

/// Live runtime plus its two event receivers and inspectable doubles.
struct Bed {
    runtime: Runtime,
    data: mpsc::Receiver<RunEvent>,
    control: mpsc::Receiver<RunEvent>,
    provider: Arc<ScriptedProvider>,
    read: Arc<RecordingTool>,
    write: Arc<RecordingTool>,
}

fn make_bed(config: RuntimeConfig, script: Vec<Vec<ProviderEvent>>) -> Bed {
    let provider = Arc::new(ScriptedProvider::standard(script));
    let read = Arc::new(RecordingTool::succeeding("host_read"));
    let write = Arc::new(RecordingTool::succeeding("host_write"));
    let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![read.clone(), write.clone()];
    let (runtime, streams) = Runtime::new(config, provider.clone(), tools);
    Bed {
        runtime,
        data: streams.data,
        control: streams.control,
        provider,
        read,
        write,
    }
}

fn test_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime builds")
}

fn config(limits: Limits, policy: Policy, has_approval_handler: bool) -> RuntimeConfig {
    RuntimeConfig {
        limits,
        policy,
        has_approval_handler,
    }
}

fn submit_cmd(tag: &str) -> SubmitCommand {
    SubmitCommand::new(
        RequestId::new(format!("req-{tag}")).expect("request id is valid"),
        SessionId::new("sess-topup").expect("session id is valid"),
        "do work",
        "test-profile",
    )
    .expect("submit command is valid")
}

fn candidate(item: &str, provider_ref: &str, tool: &str, args: &str) -> CallCandidate {
    CallCandidate::new(item, provider_ref, tool, args).expect("candidate is valid")
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
    tool_turn_with(candidates, None)
}

fn tool_turn_with(
    candidates: Vec<CallCandidate>,
    continuation: Option<ContinuationData>,
) -> Vec<ProviderEvent> {
    let mut events: Vec<ProviderEvent> = candidates
        .into_iter()
        .map(ProviderEvent::ToolCallReady)
        .collect();
    events.push(ProviderEvent::TurnFinished(TurnFinished::new(
        FinishReason::ToolCalls,
        Usage::new(None, None, UsageFinality::Final),
        continuation,
    )));
    events
}

/// One single-event terminal turn for the given finish reason, optionally
/// carrying continuation state.
fn terminal_turn(
    reason: FinishReason,
    continuation: Option<ContinuationData>,
) -> Vec<ProviderEvent> {
    vec![ProviderEvent::TurnFinished(TurnFinished::new(
        reason,
        Usage::new(Some(3), Some(4), UsageFinality::Final),
        continuation,
    ))]
}

/// Drains both channels until the terminal control event, then keeps draining
/// until two consecutive quiet passes (bounded round cap) so a duplicate
/// terminal or trailing event cannot hide behind the first one.
async fn collect_until_finished(
    data: &mut mpsc::Receiver<RunEvent>,
    control: &mut mpsc::Receiver<RunEvent>,
) -> (Vec<RunEvent>, Vec<RunEvent>, RunFinished) {
    let mut datas = Vec::new();
    let mut controls = Vec::new();
    let mut data_open = true;
    let finished = tokio::time::timeout(WAIT, async {
        loop {
            tokio::select! {
                event = data.recv(), if data_open => match event {
                    Some(event) => datas.push(event),
                    None => data_open = false,
                },
                event = control.recv() => match event {
                    Some(event) => {
                        let finished = match event.payload() {
                            EventPayload::RunFinished(finished) => Some(finished.clone()),
                            _ => None,
                        };
                        controls.push(event);
                        if let Some(finished) = finished {
                            break finished;
                        }
                    }
                    None => panic!("control channel closed before the terminal event"),
                },
            }
        }
    })
    .await
    .expect("the run reaches a terminal within the bounded wait");
    let mut quiet_passes = 0;
    let mut rounds = 0;
    while quiet_passes < 2 && rounds < 64 {
        rounds += 1;
        let mut drained = false;
        while let Ok(event) = data.try_recv() {
            datas.push(event);
            drained = true;
        }
        while let Ok(event) = control.try_recv() {
            controls.push(event);
            drained = true;
        }
        if drained {
            quiet_passes = 0;
        } else {
            quiet_passes += 1;
            tokio::task::yield_now().await;
        }
    }
    (datas, controls, finished)
}

/// Bounded poll of the retained snapshot for a run whose event consumers are
/// gone: the snapshot, not a channel, is the only observable record.
async fn await_finalized(runtime: &Runtime, run: &RunId) -> Snapshot {
    tokio::time::timeout(WAIT, async {
        loop {
            let (_, snapshot) = runtime
                .get_snapshot(GetSnapshotCommand {
                    request: RequestId::new("req-await-finalized").expect("request id is valid"),
                    run: run.clone(),
                })
                .await;
            if let Some(snapshot) = snapshot
                && matches!(snapshot.lifecycle(), RunLifecycle::Finalized(_))
            {
                return snapshot;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the run finalizes within the bounded wait")
}

/// The recorded status/effect/content of every delivered call outcome.
fn recorded_outcomes(events: &[RunEvent]) -> Vec<(CallId, ExecutionStatus, EffectState, String)> {
    events
        .iter()
        .filter_map(|event| match event.payload() {
            EventPayload::ToolFinished(info) => Some((
                info.call.clone(),
                info.outcome.status(),
                info.outcome.effect(),
                info.outcome.content().to_owned(),
            )),
            _ => None,
        })
        .collect()
}

fn terminal_count(events: &[RunEvent]) -> usize {
    events.iter().filter(|event| event.is_terminal()).count()
}

#[test]
fn a_provider_without_a_usable_context_budget_is_refused_at_construction() {
    let with_context_items = |declared: Option<u32>| ProviderCapabilities {
        text: true,
        streaming: true,
        tool_calls: true,
        structured_output: false,
        usage_reporting: true,
        max_context_items: declared,
        max_output_bytes: None,
    };
    let limits = Limits::m0_test();

    // A declared bound of zero can never admit a context item, so the runtime
    // refuses it instead of building a run that always exhausts immediately.
    let error = Runtime::try_new(
        config(limits, Policy::m0_test(), false),
        Arc::new(ScriptedProvider::new(
            with_context_items(Some(0)),
            Vec::new(),
        )),
        Vec::new(),
    )
    .err()
    .expect("a provider with no usable context budget is refused");
    assert_eq!(error.category(), ErrorCategory::UnsupportedCapability);
    assert_eq!(
        error.message(),
        "provider declares no usable context budget"
    );
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);

    // The same provider with any positive declared bound is accepted: the
    // zero refusal is about the bound, not about declaring one.
    for declared in [Some(1), Some(4), None] {
        let (_runtime, mut streams) = Runtime::try_new(
            config(limits, Policy::m0_test(), false),
            Arc::new(ScriptedProvider::new(
                with_context_items(declared),
                Vec::new(),
            )),
            Vec::new(),
        )
        .expect("a positive declared context budget is usable");
        assert!(streams.data.try_recv().is_err());
        assert!(streams.control.try_recv().is_err());
    }
}

#[test]
fn handle_routes_a_snapshot_command_for_known_and_unknown_runs() {
    let mut bed = make_bed(
        config(Limits::m0_test(), Policy::m0_test(), false),
        vec![stop_turn("done")],
    );
    let rt = test_rt();
    rt.block_on(async {
        let accepted = bed.runtime.submit(submit_cmd("snapshot-routed")).await;
        assert_eq!(accepted.reply(), CommandReply::Accepted);
        let run = accepted
            .run()
            .cloned()
            .expect("an accepted submit issues a run");
        let (_data, control, finished) =
            collect_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);

        // The finalized run has a bounded retained view, not a replay log.
        let request = RequestId::new("req-snapshot-finalized").expect("request id is valid");
        let (reply, snapshot) = bed
            .runtime
            .handle(Command::GetSnapshot(GetSnapshotCommand {
                request: request.clone(),
                run: run.clone(),
            }))
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        assert_eq!(reply.request(), &request, "the reply stays correlated");
        assert_eq!(reply.run(), Some(&run));
        let snapshot = snapshot.expect("a known finalized run has a snapshot");
        assert_eq!(snapshot.run(), &run);
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed)
        );
        assert!(snapshot.known_outcomes().is_empty());
        assert!(!snapshot.is_content_truncated());

        // An unknown run is refused explicitly and never invents a view.
        let unknown = RunId::new("run-unknown-snapshot").expect("valid run id");
        let (reply, snapshot) = bed
            .runtime
            .handle(Command::GetSnapshot(GetSnapshotCommand {
                request: RequestId::new("req-snapshot-unknown").expect("request id is valid"),
                run: unknown.clone(),
            }))
            .await;
        assert_eq!(reply.reply(), CommandReply::StaleOrUnknownTarget);
        assert!(reply.run().is_none());
        assert!(snapshot.is_none());

        // The query path publishes nothing.
        assert_eq!(terminal_count(&control), 1);
        assert!(bed.control.try_recv().is_err());
    });
}

#[test]
fn an_invalid_submit_is_rejected_without_a_run_or_a_provider_call() {
    let mut bed = make_bed(
        config(Limits::m0_test(), Policy::m0_test(), false),
        vec![stop_turn("done")],
    );
    let rt = test_rt();
    rt.block_on(async {
        let mut empty_input = submit_cmd("empty-input");
        empty_input.input = String::new();
        let mut oversized = submit_cmd("oversized-input");
        oversized.input = "x".repeat(nexus_core::commands::MAX_INPUT_BYTES + 1);
        let mut empty_profile = submit_cmd("empty-profile");
        empty_profile.profile = String::new();
        let mut oversized_profile = submit_cmd("oversized-profile");
        oversized_profile.profile = "p".repeat(nexus_core::commands::MAX_SUMMARY_BYTES + 1);

        for command in [empty_input, oversized, empty_profile, oversized_profile] {
            let request = command.request.clone();
            let response = bed.runtime.submit(command).await;
            assert_eq!(response.reply(), CommandReply::Rejected);
            assert_eq!(response.request(), &request, "the reply stays correlated");
            assert!(
                response.run().is_none(),
                "an invalid submit never issues a run"
            );
        }
        assert_eq!(
            bed.provider.call_count(),
            0,
            "an invalid submit never reaches the provider"
        );

        // The refusals leave the runtime usable: the next valid submit is
        // accepted and runs to completion.
        let accepted = bed.runtime.submit(submit_cmd("after-invalid")).await;
        assert_eq!(accepted.reply(), CommandReply::Accepted);
        let (_data, _control, finished) =
            collect_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(bed.provider.call_count(), 1);
    });
}

#[test]
fn a_provider_reported_cancellation_finishes_the_run_as_cancelled() {
    let reported = AgentError::new(
        ErrorCategory::Cancelled,
        "provider reported cancellation",
        RetryGuidance::DoNotRetry,
    )
    .expect("test diagnostic builds");
    let mut bed = make_bed(
        config(Limits::m0_test(), Policy::m0_test(), false),
        vec![vec![ProviderEvent::Failed(reported)]],
    );
    let rt = test_rt();
    rt.block_on(async {
        let accepted = bed.runtime.submit(submit_cmd("provider-cancelled")).await;
        assert_eq!(accepted.reply(), CommandReply::Accepted);
        let run = accepted
            .run()
            .cloned()
            .expect("an accepted submit issues a run");
        let (data, control, finished) =
            collect_until_finished(&mut bed.data, &mut bed.control).await;

        assert_eq!(
            finished.outcome(),
            RunOutcome::Cancelled,
            "the provider's own cancellation classification is preserved"
        );
        let error = finished
            .error()
            .expect("a cancelled run keeps its typed cause");
        assert_eq!(error.category(), ErrorCategory::Cancelled);
        assert_eq!(error.message(), "provider reported cancellation");
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        assert_eq!(terminal_count(&control), 1);
        assert!(recorded_outcomes(&control).is_empty());
        assert!(
            matches!(
                control.first().map(RunEvent::payload),
                Some(EventPayload::RunStarted { .. })
            ),
            "RunStarted is still delivered first, before the cancelled terminal"
        );
        assert!(
            data.is_empty(),
            "the cancelled turn published no data traffic"
        );

        let (_, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-provider-cancelled").expect("request id is valid"),
                run,
            })
            .await;
        let snapshot = snapshot.expect("the finalized run has a snapshot");
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Cancelled)
        );
    });
}

#[test]
fn continuation_state_is_applied_only_when_it_matches_the_adapter_and_profile() {
    let compatible = ContinuationData::new(
        nexus_core::DEFAULT_ADAPTER_IDENTITY,
        "test-profile",
        vec![1, 2, 3],
    )
    .expect("compatible continuation builds");
    let foreign = ContinuationData::new("other-adapter", "other-model", vec![4, 5, 6])
        .expect("valid continuation builds");
    let rt = test_rt();

    let mut bed = make_bed(
        config(Limits::m0_test(), Policy::m0_test(), false),
        vec![
            tool_turn_with(
                vec![candidate(
                    "item-0",
                    "prov-ref-0",
                    "host_read",
                    r#"{"path":"src"}"#,
                )],
                Some(compatible.clone()),
            ),
            stop_turn("done"),
        ],
    );
    rt.block_on(async {
        let accepted = bed.runtime.submit(submit_cmd("continuation-ok")).await;
        assert_eq!(accepted.reply(), CommandReply::Accepted);
        let (_data, _control, finished) =
            collect_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(bed.provider.call_count(), 2);
        assert_eq!(
            bed.provider.requested_continuations(),
            vec![None, Some(compatible)],
            "a compatible continuation state is carried into the next turn"
        );
    });

    let mut bed = make_bed(
        config(Limits::m0_test(), Policy::m0_test(), false),
        vec![
            tool_turn_with(
                vec![candidate(
                    "item-0",
                    "prov-ref-0",
                    "host_read",
                    r#"{"path":"src"}"#,
                )],
                Some(foreign),
            ),
            stop_turn("done"),
        ],
    );
    rt.block_on(async {
        let accepted = bed.runtime.submit(submit_cmd("continuation-foreign")).await;
        assert_eq!(accepted.reply(), CommandReply::Accepted);
        let run = accepted
            .run()
            .cloned()
            .expect("an accepted submit issues a run");
        let (_data, control, finished) =
            collect_until_finished(&mut bed.data, &mut bed.control).await;

        assert_eq!(
            finished.outcome(),
            RunOutcome::Failed,
            "an incompatible continuation is never applied to the next turn"
        );
        let error = finished
            .error()
            .expect("a failed run keeps its typed cause");
        assert_eq!(error.category(), ErrorCategory::UnsupportedCapability);
        assert_eq!(
            error.message(),
            "provider continuation is incompatible with this profile"
        );
        assert_eq!(
            bed.provider.call_count(),
            1,
            "the run stops instead of issuing a second turn"
        );
        assert_eq!(
            bed.provider.requested_continuations(),
            vec![None],
            "the rejected state never reaches a later request"
        );
        assert_eq!(bed.read.execution_count(), 0);
        assert_eq!(terminal_count(&control), 1);
        let (_, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-continuation").expect("request id is valid"),
                run,
            })
            .await;
        assert_eq!(
            snapshot
                .expect("the finalized run has a snapshot")
                .lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Failed)
        );
    });
}

#[test]
fn an_output_limit_turn_is_a_limit_outcome_and_never_a_success() {
    let mut bed = make_bed(
        config(Limits::m0_test(), Policy::m0_test(), false),
        vec![terminal_turn(FinishReason::OutputLimit, None)],
    );
    let rt = test_rt();
    rt.block_on(async {
        let accepted = bed.runtime.submit(submit_cmd("output-limit")).await;
        assert_eq!(accepted.reply(), CommandReply::Accepted);
        let run = accepted
            .run()
            .cloned()
            .expect("an accepted submit issues a run");
        let (_data, control, finished) =
            collect_until_finished(&mut bed.data, &mut bed.control).await;

        assert_eq!(finished.outcome(), RunOutcome::LimitReached);
        let error = finished
            .error()
            .expect("a limit outcome keeps its typed cause");
        assert_eq!(error.category(), ErrorCategory::ResourceLimit);
        assert_eq!(error.message(), "model output limit reached");
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        assert_eq!(bed.provider.call_count(), 1, "the run stops at the limit");
        assert_eq!(terminal_count(&control), 1);
        assert!(recorded_outcomes(&control).is_empty());

        let (_, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-output-limit").expect("request id is valid"),
                run,
            })
            .await;
        assert_eq!(
            snapshot
                .expect("the finalized run has a snapshot")
                .lifecycle(),
            RunLifecycle::Finalized(RunOutcome::LimitReached)
        );
    });
}

#[test]
fn a_control_consumer_that_disconnects_cancels_an_unanswered_approval() {
    // A short approval expiry keeps the disconnect path bounded: the live
    // approval can only be answered by the consumer that just left.
    let mut limits = Limits::m0_test();
    limits.approval_expiry = Duration::from_millis(20);
    let Bed {
        runtime,
        control,
        provider,
        write,
        ..
    } = make_bed(
        config(limits, Policy::m0_test(), true),
        vec![
            tool_turn(vec![candidate(
                "item-0",
                "prov-ref-0",
                "host_write",
                r#"{"path":"dst"}"#,
            )]),
            stop_turn("unreachable after the disconnect"),
        ],
    );
    drop(control);
    let rt = test_rt();
    rt.block_on(async {
        let accepted = runtime.submit(submit_cmd("control-gone")).await;
        assert_eq!(accepted.reply(), CommandReply::Accepted);
        let run = accepted
            .run()
            .cloned()
            .expect("an accepted submit issues a run");

        let snapshot = await_finalized(&runtime, &run).await;
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Cancelled),
            "an approval nobody can answer ends the run as a cancellation"
        );
        assert_eq!(
            write.execution_count(),
            0,
            "an unanswered approval never dispatches its call"
        );
        assert_eq!(
            provider.call_count(),
            1,
            "the run stops instead of issuing another turn"
        );
        let summaries = known_outcome_summaries(&snapshot);
        assert_eq!(summaries.len(), 1, "the abandoned call is still recorded");
        assert_eq!(summaries[0].status, ExecutionStatus::Denied);
        assert_eq!(summaries[0].effect, EffectState::NotStarted);
        assert_eq!(summaries[0].evidence, Evidence::HostObserved);
        assert!(
            snapshot.is_content_truncated(),
            "a disconnected control consumer is reported, never hidden"
        );
    });
}

#[test]
fn a_control_consumer_that_disconnects_still_records_the_dispatched_outcome() {
    let Bed {
        runtime,
        control,
        provider: _,
        read,
        write,
        ..
    } = make_bed(
        config(Limits::m0_test(), Policy::m0_test(), false),
        vec![
            tool_turn(vec![candidate(
                "item-0",
                "prov-ref-0",
                "host_read",
                r#"{"path":"src"}"#,
            )]),
            stop_turn("unreachable after the disconnect"),
        ],
    );
    drop(control);
    let rt = test_rt();
    rt.block_on(async {
        let accepted = runtime.submit(submit_cmd("dispatch-then-gone")).await;
        assert_eq!(accepted.reply(), CommandReply::Accepted);
        let run = accepted
            .run()
            .cloned()
            .expect("an accepted submit issues a run");

        let snapshot = await_finalized(&runtime, &run).await;
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Cancelled),
            "the closed control channel forces a cancellation terminal for the run"
        );
        assert_eq!(
            read.execution_count(),
            1,
            "an already authorized automatic call still dispatches once"
        );
        assert_eq!(write.execution_count(), 0);

        let summaries = known_outcome_summaries(&snapshot);
        assert_eq!(summaries.len(), 1);
        assert_eq!(
            summaries[0].status,
            ExecutionStatus::Succeeded,
            "the recorded outcome stays the tool's actual result"
        );
        assert_eq!(summaries[0].effect, EffectState::KnownApplied);
        assert!(snapshot.is_content_truncated());
    });
}

#[test]
fn a_denied_unknown_tool_is_recorded_even_without_a_control_consumer() {
    let Bed {
        runtime,
        control,
        read,
        write,
        ..
    } = make_bed(
        config(Limits::m0_test(), Policy::m0_test(), false),
        vec![
            tool_turn(vec![candidate(
                "item-0",
                "prov-ref-0",
                "host_missing",
                "{}",
            )]),
            stop_turn("done"),
        ],
    );
    drop(control);
    let rt = test_rt();
    rt.block_on(async {
        let accepted = runtime.submit(submit_cmd("denial-without-consumer")).await;
        assert_eq!(accepted.reply(), CommandReply::Accepted);
        let run = accepted
            .run()
            .cloned()
            .expect("an accepted submit issues a run");

        let snapshot = await_finalized(&runtime, &run).await;
        assert_eq!(read.execution_count(), 0);
        assert_eq!(write.execution_count(), 0);
        let summaries = known_outcome_summaries(&snapshot);
        assert_eq!(summaries.len(), 1, "the denial is never lost");
        assert_eq!(summaries[0].status, ExecutionStatus::Denied);
        assert_eq!(summaries[0].effect, EffectState::NotStarted);
        assert_eq!(summaries[0].evidence, Evidence::HostObserved);
        assert!(
            snapshot.is_content_truncated(),
            "the disconnected control channel is reported as truncation"
        );
        // A denial is not a dispatched call: no recording step consumes the
        // forced terminal, so the run still ends on its honest model outcome.
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed)
        );
    });
}

#[test]
fn a_tool_the_policy_no_longer_authorizes_is_denied_before_dispatch() {
    // The compatibility constructor admits any auto-set; the authorization
    // boundary then refuses the automatic mutation it classifies.
    let policy = Policy::new(vec!["host_write".to_owned()], M0_REVISION);
    let mut bed = make_bed(
        config(Limits::m0_test(), policy, true),
        vec![
            tool_turn(vec![candidate(
                "item-0",
                "prov-ref-0",
                "host_write",
                r#"{"path":"dst"}"#,
            )]),
            stop_turn("done"),
        ],
    );
    let rt = test_rt();
    rt.block_on(async {
        let accepted = bed
            .runtime
            .submit(submit_cmd("unauthorized-dispatch"))
            .await;
        assert_eq!(accepted.reply(), CommandReply::Accepted);
        let run = accepted
            .run()
            .cloned()
            .expect("an accepted submit issues a run");
        let (_data, control, finished) =
            collect_until_finished(&mut bed.data, &mut bed.control).await;

        assert_eq!(
            finished.outcome(),
            RunOutcome::Completed,
            "a refused dispatch is recorded, not a run failure"
        );
        assert_eq!(
            bed.write.execution_count(),
            0,
            "the call never reaches its executor"
        );
        let outcomes = recorded_outcomes(&control);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].1, ExecutionStatus::Denied);
        assert_eq!(outcomes[0].2, EffectState::NotStarted);
        assert_eq!(
            outcomes[0].3, "policy no longer authorizes dispatch",
            "the denial states the exact reason"
        );
        assert_eq!(bed.provider.call_count(), 2);

        let (_, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-unauthorized").expect("request id is valid"),
                run,
            })
            .await;
        let summaries =
            known_outcome_summaries(&snapshot.expect("the finalized run has a snapshot"));
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].status, ExecutionStatus::Denied);
    });
}

#[test]
fn the_retained_snapshot_bounds_known_outcomes_by_the_m0_call_budget() {
    // One more admitted call than the M0 snapshot bound allows.
    let admitted = Limits::M0_TEST_TOOL_CALLS_PER_RUN as usize + 1;
    let mut limits = Limits::m0_test();
    limits.max_tool_calls_per_run = admitted as u32;
    limits.max_tool_calls_per_turn = admitted as u32;
    let candidates: Vec<CallCandidate> = (0..admitted)
        .map(|index| {
            candidate(
                &format!("item-{index}"),
                &format!("prov-ref-{index}"),
                "host_missing",
                "{}",
            )
        })
        .collect();
    let mut bed = make_bed(
        config(limits, Policy::m0_test(), false),
        vec![tool_turn(candidates), stop_turn("done")],
    );
    let rt = test_rt();
    rt.block_on(async {
        let accepted = bed
            .runtime
            .submit(submit_cmd("snapshot-outcome-bound"))
            .await;
        assert_eq!(accepted.reply(), CommandReply::Accepted);
        let run = accepted
            .run()
            .cloned()
            .expect("an accepted submit issues a run");
        let (_data, control, finished) =
            collect_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);

        // Every admitted denial is still delivered on the control channel:
        // bounding the retained view never hides a recorded outcome.
        let outcomes = recorded_outcomes(&control);
        assert_eq!(outcomes.len(), admitted);
        assert!(
            outcomes
                .iter()
                .all(|(_, status, _, _)| *status == ExecutionStatus::Denied),
            "each unknown-tool candidate is denied"
        );
        assert_eq!(bed.read.execution_count(), 0);
        assert_eq!(bed.write.execution_count(), 0);

        let (_, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-outcome-bound").expect("request id is valid"),
                run,
            })
            .await;
        let snapshot = snapshot.expect("the finalized run has a snapshot");
        let bounded = known_outcome_summaries(&snapshot);
        assert_eq!(
            bounded.len(),
            Limits::M0_TEST_TOOL_CALLS_PER_RUN as usize,
            "the retained view drops the oldest excess outcome"
        );
        assert!(
            snapshot.is_content_truncated(),
            "truncation is stated, not silently applied"
        );
        assert_eq!(
            bounded[0].call, outcomes[1].0,
            "the oldest recorded outcome is the one dropped"
        );
    });
}

/// Projects the snapshot's known outcomes into the shape the assertions read.
fn known_outcome_summaries(snapshot: &Snapshot) -> Vec<OutcomeSummary> {
    snapshot.known_outcomes().to_vec()
}
