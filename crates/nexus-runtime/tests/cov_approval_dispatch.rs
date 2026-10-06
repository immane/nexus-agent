#![forbid(unsafe_code)]

//! Public-API integration coverage for approval dispatch.
//!
//! These tests drive [`nexus_runtime::Runtime`] exclusively through its public
//! surface (`Runtime::new`, `submit`, `approve`, `deny`, `handle`,
//! `get_snapshot`) with minimal in-file port doubles, so approval behavior is
//! verified from outside the crate the way a frontend sees it:
//!
//! - an exact `(run, approval, call)` tuple dispatches the bound call exactly
//!   once, with the exact bound arguments and policy scope;
//! - mismatched tuples (wrong call, unknown grant, wrong run) are rejected
//!   without dispatch and without consuming the live grant;
//! - a duplicate approval never redispatches, and a late approval on a
//!   finalized run replies `AlreadyFinalized`;
//! - an expired approval denies the call without dispatch, the run continues
//!   honestly, and a late decision cannot resurrect the grant;
//! - denied calls consume the per-run call budget, so an over-budget candidate
//!   is refused at admission with `LimitReached`.
//!
//! Determinism: every wait is a bounded `tokio::time::timeout` failure
//! backstop and every assertion is on recorded counts, identities, outcomes,
//! and terminal state; no test sleeps for a fixed duration or asserts
//! wall-clock timing.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use nexus_core::{
    ApprovalId, ApprovalNotice, ApproveCommand, CallCandidate, CallId, Command, CommandReply,
    DenyCommand, EffectState, ErrorCategory, EventPayload, Evidence, ExecutionStatus, FinishReason,
    GetSnapshotCommand, Limits, M0_REVISION, ModelRequest, ProviderCapabilities, ProviderContext,
    ProviderEvent, ProviderPort, RequestId, RunEvent, RunFinished, RunId, RunLifecycle, RunOutcome,
    SessionId, SubmitCommand, ToolCall, ToolContext, ToolId, ToolOutcome, ToolPort, ToolSpec,
    TurnFinished, Usage, UsageFinality,
};
use nexus_runtime::{Policy, Runtime, RuntimeConfig};
use tokio::sync::mpsc;

/// Scripted provider double: pops one complete event batch per call and
/// records how many turns were requested.
struct ScriptedProvider {
    calls: AtomicUsize,
    script: StdMutex<VecDeque<Vec<ProviderEvent>>>,
}

impl ScriptedProvider {
    fn new(script: Vec<Vec<ProviderEvent>>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            script: StdMutex::new(script.into()),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// One ordinary stop turn with a single text fragment.
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

    /// One tool-call turn proposing `candidates` in declared order.
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
}

impl ProviderPort for ScriptedProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            text: true,
            streaming: true,
            tool_calls: true,
            structured_output: false,
            usage_reporting: false,
            max_context_items: None,
            max_output_bytes: None,
        }
    }

    fn stream(&self, _request: &ModelRequest, _context: &ProviderContext) -> Vec<ProviderEvent> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.script
            .lock()
            .expect("script mutex is never poisoned by a test")
            .pop_front()
            .unwrap_or_else(|| Self::stop_turn("script exhausted"))
    }
}

/// Recording tool double: counts executions, captures the exact argument text
/// and approved scope of every dispatched call, and returns a fixed
/// successful outcome.
struct RecordingTool {
    spec: ToolSpec,
    executed: AtomicUsize,
    seen_args: StdMutex<Vec<String>>,
    seen_scopes: StdMutex<Vec<String>>,
    outcome: ToolOutcome,
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
            seen_args: StdMutex::new(Vec::new()),
            seen_scopes: StdMutex::new(Vec::new()),
            outcome: ToolOutcome::new(
                ExecutionStatus::Succeeded,
                EffectState::KnownApplied,
                Evidence::HostObserved,
                "test double executed",
                false,
            )
            .expect("test outcome is valid"),
        }
    }

    fn execution_count(&self) -> usize {
        self.executed.load(Ordering::SeqCst)
    }

    fn seen_args(&self) -> Vec<String> {
        self.seen_args
            .lock()
            .expect("args mutex is never poisoned by a test")
            .clone()
    }

    fn seen_scopes(&self) -> Vec<String> {
        self.seen_scopes
            .lock()
            .expect("scope mutex is never poisoned by a test")
            .clone()
    }
}

impl ToolPort for RecordingTool {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn execute(&self, call: &ToolCall, context: &ToolContext) -> ToolOutcome {
        self.executed.fetch_add(1, Ordering::SeqCst);
        self.seen_args
            .lock()
            .expect("args mutex is never poisoned by a test")
            .push(call.args().as_str().to_owned());
        self.seen_scopes
            .lock()
            .expect("scope mutex is never poisoned by a test")
            .push(context.scope().as_str().to_owned());
        self.outcome.clone()
    }
}

/// Live runtime plus its two event receivers and inspectable doubles.
struct Bed {
    runtime: Runtime,
    data: mpsc::Receiver<RunEvent>,
    control: mpsc::Receiver<RunEvent>,
    provider: Arc<ScriptedProvider>,
    read_tool: Arc<RecordingTool>,
    write_tool: Arc<RecordingTool>,
}

fn test_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime builds")
}

fn quick_config() -> RuntimeConfig {
    RuntimeConfig {
        limits: Limits::m0_test(),
        policy: Policy::m0_test(),
        has_approval_handler: true,
    }
}

fn submit_cmd(tag: &str) -> SubmitCommand {
    SubmitCommand::new(
        RequestId::new(format!("req-{tag}")).expect("request id is valid"),
        SessionId::new("sess-1").expect("session id is valid"),
        "do work",
        "test-profile",
    )
    .expect("submit command is valid")
}

fn candidate(item: &str, provider_ref: &str, tool: &str, args: &str) -> CallCandidate {
    CallCandidate::new(item, provider_ref, tool, args).expect("candidate is valid")
}

fn make_bed(script: Vec<Vec<ProviderEvent>>, config: RuntimeConfig) -> Bed {
    let provider = Arc::new(ScriptedProvider::new(script));
    let read_tool = Arc::new(RecordingTool::succeeding("host_read"));
    let write_tool = Arc::new(RecordingTool::succeeding("host_write"));
    let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![read_tool.clone(), write_tool.clone()];
    let (runtime, streams) = Runtime::new(config, provider.clone(), tools);
    Bed {
        runtime,
        data: streams.data,
        control: streams.control,
        provider,
        read_tool,
        write_tool,
    }
}

fn is_approval_required(event: &RunEvent) -> bool {
    matches!(event.payload(), EventPayload::ApprovalRequired(_))
}

/// Returns the first approval notice in `events`.
fn approval_notice(events: &[RunEvent]) -> &ApprovalNotice {
    events
        .iter()
        .find_map(|event| match event.payload() {
            EventPayload::ApprovalRequired(notice) => Some(notice),
            _ => None,
        })
        .expect("an approval was requested")
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

/// Asserts exactly one terminal `RunFinished` exists across both channels.
fn assert_single_terminal(data: &[RunEvent], control: &[RunEvent]) {
    let terminals = data
        .iter()
        .chain(control.iter())
        .filter(|event| event.is_terminal())
        .count();
    assert_eq!(
        terminals, 1,
        "exactly one RunFinished terminal, saw {terminals}"
    );
}

/// Asserts per-run sequence numbers are contiguous from zero across both
/// channels. Valid only when no data event was dropped (tiny test output).
fn assert_contiguous(data: &[RunEvent], control: &[RunEvent]) {
    let mut all: Vec<&RunEvent> = data.iter().chain(control.iter()).collect();
    all.sort_by_key(|event| event.seq());
    assert!(!all.is_empty(), "a run publishes events");
    for (index, event) in all.iter().enumerate() {
        assert_eq!(
            event.seq(),
            index as u64,
            "published sequences stay gap-free"
        );
    }
}

/// Drains control events until `stop` matches, with a bounded backstop.
async fn collect_control_until(
    control: &mut mpsc::Receiver<RunEvent>,
    stop: impl Fn(&RunEvent) -> bool,
) -> Vec<RunEvent> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut out = Vec::new();
        loop {
            match control.recv().await {
                Some(event) => {
                    let done = stop(&event);
                    out.push(event);
                    if done {
                        break;
                    }
                }
                None => panic!("control channel closed before the awaited event"),
            }
        }
        out
    })
    .await
    .expect("the awaited control event arrives within the bounded wait")
}

/// Drains both channels until the terminal control event, then keeps draining
/// both channels until two consecutive quiet passes (bounded round cap) so a
/// duplicate terminal or trailing event cannot hide behind the first one.
async fn drain_until_finished(
    data: &mut mpsc::Receiver<RunEvent>,
    control: &mut mpsc::Receiver<RunEvent>,
) -> (Vec<RunEvent>, Vec<RunEvent>, RunFinished) {
    let mut datas = Vec::new();
    let mut controls = Vec::new();
    let mut data_open = true;
    let finished = tokio::time::timeout(Duration::from_secs(10), async {
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

#[test]
fn exact_tuple_approve_dispatches_the_bound_call_once() {
    let args = r#"{"path":"src"}"#;
    let mut bed = make_bed(
        vec![
            ScriptedProvider::tool_turn(vec![candidate(
                "item-0",
                "prov-ref-0",
                "host_write",
                args,
            )]),
            ScriptedProvider::stop_turn("done"),
        ],
        quick_config(),
    );
    let rt = test_rt();
    rt.block_on(async {
        let response = bed.runtime.submit(submit_cmd("approve-exact")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response
            .run()
            .cloned()
            .expect("accepted submit issues a run");

        let requested = collect_control_until(&mut bed.control, is_approval_required).await;
        let notice = approval_notice(&requested);
        let (approval, call) = (notice.approval.clone(), notice.call.clone());
        assert!(
            notice.args_preview().is_some(),
            "the notice carries the exact-arguments preview"
        );
        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "no dispatch happens before a decision"
        );

        let (reply, snapshot) = bed
            .runtime
            .handle(Command::Approve(ApproveCommand {
                request: RequestId::new("req-approve-exact").expect("request id is valid"),
                approval: approval.clone(),
                run: run.clone(),
                call: call.clone(),
            }))
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        assert!(snapshot.is_none(), "approve returns no snapshot");

        let (data, mut control, finished) =
            drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(requested);

        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(
            bed.write_tool.execution_count(),
            1,
            "the bound call dispatches exactly once"
        );
        assert_eq!(
            bed.write_tool.seen_args(),
            vec![args.to_owned()],
            "the tool observes the exact bound arguments"
        );
        assert_eq!(
            bed.write_tool.seen_scopes(),
            vec!["path:src".to_owned()],
            "the approved policy scope crosses the execution boundary"
        );
        assert_eq!(
            bed.read_tool.execution_count(),
            0,
            "tools without an admitted call never dispatch"
        );
        assert_eq!(tool_started_calls(&control), vec![call.clone()]);
        assert_eq!(
            bed.provider.call_count(),
            2,
            "the run continues after the recorded result"
        );
        let outcomes = tool_finished_map(&control);
        assert_eq!(outcomes.len(), 1);
        let outcome = outcomes
            .get(&call)
            .expect("the bound call records exactly one outcome");
        assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
        assert_eq!(outcome.effect(), EffectState::KnownApplied);
        assert_single_terminal(&data, &control);
        assert_contiguous(&data, &control);

        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-approve-exact-snap").expect("request id is valid"),
                run,
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("a finalized run keeps a snapshot");
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed)
        );
        assert!(
            snapshot.pending_approvals().is_empty(),
            "a decided approval leaves the pending set"
        );
        assert_eq!(snapshot.known_outcomes().len(), 1);
        assert_eq!(snapshot.known_outcomes()[0].call, call);
        assert_eq!(
            snapshot.known_outcomes()[0].status,
            ExecutionStatus::Succeeded
        );
    });
}

#[test]
fn mismatched_tuples_are_rejected_without_dispatch_or_grant_loss() {
    let args = r#"{"path":"src"}"#;
    let mut bed = make_bed(
        vec![
            ScriptedProvider::tool_turn(vec![candidate(
                "item-0",
                "prov-ref-0",
                "host_write",
                args,
            )]),
            ScriptedProvider::stop_turn("done"),
        ],
        quick_config(),
    );
    let rt = test_rt();
    rt.block_on(async {
        let response = bed.runtime.submit(submit_cmd("approve-mismatch")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response
            .run()
            .cloned()
            .expect("accepted submit issues a run");
        let requested = collect_control_until(&mut bed.control, is_approval_required).await;
        let notice = approval_notice(&requested);
        let (approval, call) = (notice.approval.clone(), notice.call.clone());

        let wrong_call = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-wrong-call").expect("request id is valid"),
                approval: approval.clone(),
                run: run.clone(),
                call: CallId::new("c-not-the-bound-call").expect("call id is valid"),
            })
            .await;
        assert_eq!(wrong_call.reply(), CommandReply::StaleOrUnknownTarget);

        let unknown_approval = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-unknown-approval").expect("request id is valid"),
                approval: ApprovalId::new("a-not-the-bound-grant").expect("approval id is valid"),
                run: run.clone(),
                call: call.clone(),
            })
            .await;
        assert_eq!(unknown_approval.reply(), CommandReply::StaleOrUnknownTarget);

        let wrong_run = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-wrong-run").expect("request id is valid"),
                approval: approval.clone(),
                run: RunId::new("r-not-the-active-run").expect("run id is valid"),
                call: call.clone(),
            })
            .await;
        assert_eq!(wrong_run.reply(), CommandReply::StaleOrUnknownTarget);

        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "no mismatched decision dispatches"
        );
        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-mismatch-snap").expect("request id is valid"),
                run: run.clone(),
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("a live run has a snapshot");
        assert!(
            snapshot.pending_approvals().contains(&approval),
            "rejected tuples never consume the live grant"
        );

        let accepted = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-matching").expect("request id is valid"),
                approval,
                run,
                call: call.clone(),
            })
            .await;
        assert_eq!(accepted.reply(), CommandReply::Accepted);
        let (data, mut control, finished) =
            drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(requested);
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(
            bed.write_tool.execution_count(),
            1,
            "the exact tuple still dispatches after rejections"
        );
        assert_eq!(tool_started_calls(&control), vec![call]);
        assert_single_terminal(&data, &control);
    });
}

#[test]
fn duplicate_approve_never_redispatches() {
    let args = r#"{"path":"src"}"#;
    let mut bed = make_bed(
        vec![
            ScriptedProvider::tool_turn(vec![candidate(
                "item-0",
                "prov-ref-0",
                "host_write",
                args,
            )]),
            ScriptedProvider::stop_turn("done"),
        ],
        quick_config(),
    );
    let rt = test_rt();
    rt.block_on(async {
        let response = bed.runtime.submit(submit_cmd("approve-duplicate")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response
            .run()
            .cloned()
            .expect("accepted submit issues a run");
        let requested = collect_control_until(&mut bed.control, is_approval_required).await;
        let notice = approval_notice(&requested);
        let (approval, call) = (notice.approval.clone(), notice.call.clone());

        let first = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-approve-first").expect("request id is valid"),
                approval: approval.clone(),
                run: run.clone(),
                call: call.clone(),
            })
            .await;
        assert_eq!(first.reply(), CommandReply::Accepted);

        let second = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-approve-second").expect("request id is valid"),
                approval: approval.clone(),
                run: run.clone(),
                call: call.clone(),
            })
            .await;
        assert_eq!(
            second.reply(),
            CommandReply::StaleOrUnknownTarget,
            "a decided grant never decides twice"
        );

        let (data, mut control, finished) =
            drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(requested);
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(
            bed.write_tool.execution_count(),
            1,
            "the duplicate approval never redispatches"
        );
        assert_eq!(tool_started_calls(&control), vec![call.clone()]);
        assert_eq!(tool_finished_map(&control).len(), 1);
        assert_single_terminal(&data, &control);
        assert_contiguous(&data, &control);

        let late = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-approve-late").expect("request id is valid"),
                approval,
                run,
                call,
            })
            .await;
        assert_eq!(
            late.reply(),
            CommandReply::AlreadyFinalized,
            "a finalized run never accepts a stale approval"
        );
        assert_eq!(bed.write_tool.execution_count(), 1);
    });
}

#[test]
fn expired_approval_denies_without_dispatch_and_cannot_be_resurrected() {
    let mut limits = Limits::m0_test();
    limits.approval_expiry = Duration::from_millis(40);
    let mut bed = make_bed(
        vec![
            ScriptedProvider::tool_turn(vec![candidate(
                "item-0",
                "prov-ref-0",
                "host_write",
                r#"{"path":"src"}"#,
            )]),
            ScriptedProvider::stop_turn("after expiry"),
        ],
        RuntimeConfig {
            limits,
            policy: Policy::m0_test(),
            has_approval_handler: true,
        },
    );
    let rt = test_rt();
    rt.block_on(async {
        let response = bed.runtime.submit(submit_cmd("approve-expiry")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response
            .run()
            .cloned()
            .expect("accepted submit issues a run");
        let requested = collect_control_until(&mut bed.control, is_approval_required).await;
        let notice = approval_notice(&requested);
        let (approval, call) = (notice.approval.clone(), notice.call.clone());
        assert!(
            notice.expires_at_elapsed > Duration::ZERO,
            "the notice carries a positive expiry reading"
        );
        assert_eq!(bed.write_tool.execution_count(), 0);

        // No decision is sent: the expiry path alone must resolve the wait.
        let (data, mut control, finished) =
            drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(requested);

        assert_eq!(
            finished.outcome(),
            RunOutcome::Completed,
            "expiry denies the call and the run continues honestly"
        );
        assert_eq!(
            bed.provider.call_count(),
            2,
            "the loop requests the next model turn after the denial"
        );
        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "the expired call never dispatches"
        );
        assert!(tool_started_calls(&control).is_empty());
        let outcomes = tool_finished_map(&control);
        assert_eq!(outcomes.len(), 1);
        let outcome = outcomes
            .get(&call)
            .expect("the expired call records exactly one denial");
        assert_eq!(outcome.status(), ExecutionStatus::Denied);
        assert_eq!(outcome.effect(), EffectState::NotStarted);
        assert_eq!(outcome.evidence(), Evidence::HostObserved);
        assert_single_terminal(&data, &control);
        assert_contiguous(&data, &control);

        let late = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-approve-too-late").expect("request id is valid"),
                approval: approval.clone(),
                run: run.clone(),
                call: call.clone(),
            })
            .await;
        assert_eq!(
            late.reply(),
            CommandReply::AlreadyFinalized,
            "a finalized run never resurrects an expired grant"
        );
        assert_eq!(bed.write_tool.execution_count(), 0);

        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-expiry-snap").expect("request id is valid"),
                run,
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("a finalized run keeps a snapshot");
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed)
        );
        assert!(
            !snapshot.pending_approvals().contains(&approval),
            "an expired approval is no longer pending"
        );
        assert_eq!(snapshot.known_outcomes().len(), 1);
        assert_eq!(snapshot.known_outcomes()[0].call, call);
        assert_eq!(snapshot.known_outcomes()[0].status, ExecutionStatus::Denied);
    });
}

#[test]
fn denied_calls_consume_the_per_run_call_budget() {
    let mut limits = Limits::m0_test();
    limits.max_tool_calls_per_run = 2;
    let write = |item: &str, reference: &str, path: &str| {
        CallCandidate::new(
            item,
            reference,
            "host_write",
            format!(r#"{{"path":"{path}"}}"#),
        )
        .expect("candidate is valid")
    };
    let mut bed = make_bed(
        vec![
            ScriptedProvider::tool_turn(vec![
                write("item-0", "prov-ref-0", "a"),
                write("item-1", "prov-ref-1", "b"),
            ]),
            ScriptedProvider::tool_turn(vec![write("item-2", "prov-ref-2", "c")]),
        ],
        RuntimeConfig {
            limits,
            policy: Policy::m0_test(),
            has_approval_handler: true,
        },
    );
    let rt = test_rt();
    rt.block_on(async {
        let response = bed.runtime.submit(submit_cmd("approve-budget")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response
            .run()
            .cloned()
            .expect("accepted submit issues a run");

        let first = collect_control_until(&mut bed.control, is_approval_required).await;
        let notice = approval_notice(&first);
        let (first_approval, first_call) = (notice.approval.clone(), notice.call.clone());
        let (reply, snapshot) = bed
            .runtime
            .handle(Command::Deny(DenyCommand {
                request: RequestId::new("req-deny-first").expect("request id is valid"),
                approval: first_approval,
                run: run.clone(),
                call: first_call.clone(),
            }))
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        assert!(snapshot.is_none(), "deny returns no snapshot");

        let second = collect_control_until(&mut bed.control, is_approval_required).await;
        let notice = approval_notice(&second);
        let (second_approval, second_call) = (notice.approval.clone(), notice.call.clone());
        assert_ne!(
            second_call, first_call,
            "each admitted call owns a distinct identity"
        );
        let (reply, _) = bed
            .runtime
            .handle(Command::Deny(DenyCommand {
                request: RequestId::new("req-deny-second").expect("request id is valid"),
                approval: second_approval,
                run,
                call: second_call.clone(),
            }))
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);

        let (data, mut control, finished) =
            drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(first);
        control.extend(second);

        assert_eq!(
            finished.outcome(),
            RunOutcome::LimitReached,
            "the third candidate is refused at admission"
        );
        assert_eq!(
            finished
                .error()
                .expect("a limit terminal carries its typed error")
                .category(),
            ErrorCategory::ResourceLimit
        );
        assert_eq!(
            bed.provider.call_count(),
            2,
            "the second turn is requested before admission refuses"
        );
        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "denied calls never dispatch"
        );
        assert!(tool_started_calls(&control).is_empty());
        let outcomes = tool_finished_map(&control);
        assert_eq!(outcomes.len(), 2, "both denied calls are recorded");
        for call in [&first_call, &second_call] {
            let outcome = outcomes
                .get(call)
                .expect("each denied call records its outcome");
            assert_eq!(outcome.status(), ExecutionStatus::Denied);
            assert_eq!(outcome.effect(), EffectState::NotStarted);
            assert_eq!(outcome.evidence(), Evidence::HostObserved);
        }
        assert_single_terminal(&data, &control);
        assert_contiguous(&data, &control);
    });
}

#[test]
fn read_only_runs_deny_confirmation_tools_without_prompting() {
    let rt = test_rt();
    rt.block_on(async {
        let args = r#"{"path":"src"}"#;
        let mut bed = make_bed(
            vec![
                ScriptedProvider::tool_turn(vec![candidate(
                    "item-0",
                    "prov-ref-0",
                    "host_write",
                    args,
                )]),
                ScriptedProvider::stop_turn("done"),
            ],
            quick_config(),
        );
        let reply = bed
            .runtime
            .submit(submit_cmd("readonly").with_read_only(true))
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let (data, control, _finished) =
            drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert!(
            !control.iter().any(is_approval_required),
            "a read-only run never mints an approval card"
        );
        assert!(
            tool_started_calls(&data).is_empty() && tool_started_calls(&control).is_empty(),
            "the write never dispatches"
        );
        assert_eq!(
            bed.write_tool.execution_count(),
            0,
            "no execution without a grant"
        );
        let finished = tool_finished_map(&control);
        assert_eq!(finished.len(), 1);
        let outcome = finished.values().next().expect("one tool outcome");
        assert_eq!(
            outcome.status(),
            ExecutionStatus::Denied,
            "the call is denied, not failed"
        );
        assert_single_terminal(&data, &control);
    });
}

#[test]
fn read_only_runs_still_execute_automatic_reads() {
    let rt = test_rt();
    rt.block_on(async {
        let args = r#"{"path":"src"}"#;
        let mut bed = make_bed(
            vec![
                ScriptedProvider::tool_turn(vec![candidate(
                    "item-0",
                    "prov-ref-0",
                    "host_read",
                    args,
                )]),
                ScriptedProvider::stop_turn("done"),
            ],
            quick_config(),
        );
        let reply = bed
            .runtime
            .submit(submit_cmd("readonly-read").with_read_only(true))
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let (data, control, _finished) =
            drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            bed.read_tool.execution_count(),
            1,
            "automatic reads proceed in a read-only run"
        );
        assert!(
            !control.iter().any(is_approval_required),
            "automatic tools never prompt"
        );
        assert_single_terminal(&data, &control);
    });
}
