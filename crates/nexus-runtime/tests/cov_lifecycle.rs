#![forbid(unsafe_code)]

//! Coverage hardening for the single-run lifecycle through the public
//! `Runtime` API.
//!
//! `nexus-fakes` is not a dependency of this crate, so these tests carry
//! minimal in-file `ProviderPort`/`ToolPort` doubles instead of adding one.
//! Every wait is bounded by `tokio::time::timeout`, and blocking work is
//! gated on channels rather than sleeps, so passing runs settle
//! deterministically.
//!
//! Covered contract:
//! - a first `submit` is accepted with a host-issued run id; a second
//!   `submit` while that run is live replies `Busy` naming the live run and
//!   never reaches the provider;
//! - `RunStarted` is delivered first, exactly one terminal event is delivered
//!   last, and per-run sequences stay contiguous across both channels;
//! - stale run identities are rejected on both the cancel and the
//!   approve/deny paths: `AlreadyFinalized` for the retained run and
//!   `StaleOrUnknownTarget` for an unknown run, without disturbing a live run;
//! - run ids never alias across independent runtime instances.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use nexus_core::{
    ApprovalId, ApproveCommand, CallCandidate, CallId, CancelCommand, CommandReply, DenyCommand,
    EffectState, EventPayload, Evidence, ExecutionStatus, FinishReason, Limits, ModelRequest,
    PersistenceState, ProviderCapabilities, ProviderContext, ProviderEvent, ProviderPort,
    RequestId, RunEvent, RunFinished, RunId, RunOutcome, SessionId, SubmitCommand, ToolCall,
    ToolContext, ToolId, ToolOutcome, ToolPort, ToolSpec, TurnFinished, Usage, UsageFinality,
};
use nexus_runtime::{EventStreams, Policy, Runtime, RuntimeConfig};
use tokio::sync::mpsc;

/// Bounded wait for a gated double to enter its blocking section.
const GATE_WAIT: Duration = Duration::from_secs(5);
/// Bounded failure backstop for terminal delivery; passing runs settle in
/// milliseconds, so this only bounds a defect.
const TERMINAL_WAIT: Duration = Duration::from_secs(10);

/// Scripted provider: one prepared event batch per `stream` call, plus an
/// optional entry/release gate that keeps a run deterministically live.
struct ScriptedProvider {
    calls: AtomicUsize,
    turns: StdMutex<VecDeque<Vec<ProviderEvent>>>,
    gate: Option<ProviderGate>,
}

struct ProviderGate {
    entered: mpsc::UnboundedSender<()>,
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
        entered: mpsc::UnboundedSender<()>,
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            let _ = gate.entered.send(());
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
    outcome: ToolOutcome,
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
            outcome: ToolOutcome::new(
                ExecutionStatus::Succeeded,
                EffectState::KnownApplied,
                Evidence::HostObserved,
                "observed success",
                false,
            )
            .expect("valid tool outcome"),
        }
    }
}

impl ToolPort for ScriptedTool {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn execute(&self, _call: &ToolCall, _context: &ToolContext) -> ToolOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        self.outcome.clone()
    }
}

/// Live runtime plus its two event receivers and inspectable doubles.
struct Bed {
    runtime: Runtime,
    data: mpsc::Receiver<RunEvent>,
    control: mpsc::Receiver<RunEvent>,
    provider: Arc<ScriptedProvider>,
    read: Arc<ScriptedTool>,
    write: Arc<ScriptedTool>,
}

fn build_bed(provider: Arc<ScriptedProvider>, config: RuntimeConfig) -> Bed {
    let read = Arc::new(ScriptedTool::succeeding("host_read"));
    let write = Arc::new(ScriptedTool::succeeding("host_write"));
    let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![read.clone(), write.clone()];
    let (runtime, streams): (Runtime, EventStreams) = Runtime::new(config, provider.clone(), tools);
    Bed {
        runtime,
        data: streams.data,
        control: streams.control,
        provider,
        read,
        write,
    }
}

fn make_bed(script: Vec<Vec<ProviderEvent>>, config: RuntimeConfig) -> Bed {
    build_bed(Arc::new(ScriptedProvider::new(script)), config)
}

/// Bed whose provider blocks on the first `stream` call until released. The
/// test observes entry through the receiver and releases through the sender.
fn make_gated_bed(
    script: Vec<Vec<ProviderEvent>>,
    config: RuntimeConfig,
) -> (
    Bed,
    mpsc::UnboundedReceiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (entered_tx, entered_rx) = mpsc::unbounded_channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let provider = Arc::new(ScriptedProvider::gated(script, entered_tx, release_rx));
    (build_bed(provider, config), entered_rx, release_tx)
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
        RequestId::new(format!("req-{tag}")).expect("valid request id"),
        SessionId::new("sess-1").expect("valid session id"),
        "do work",
        "m0-test",
    )
    .expect("valid submit")
}

/// Single call candidate; the item key differs from the text item key so the
/// batch never mixes text and call identity.
fn candidate(tool: &str, args: &str) -> CallCandidate {
    CallCandidate::new("call-item-0", "prov-ref-0", tool, args).expect("valid candidate")
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

/// One tool-call turn: coalescible text, argument progress (which the runtime
/// publishes as a preview), one complete candidate, and the terminal.
fn tool_turn(text: &str, candidates: Vec<CallCandidate>) -> Vec<ProviderEvent> {
    let mut events = vec![
        ProviderEvent::TextDelta {
            item_key: "item-0".to_owned(),
            text: text.to_owned(),
        },
        ProviderEvent::ToolCallDelta {
            item_key: "call-item-0".to_owned(),
            assembled_bytes: 12,
        },
    ];
    events.extend(candidates.into_iter().map(ProviderEvent::ToolCallReady));
    events.push(ProviderEvent::TurnFinished(TurnFinished::new(
        FinishReason::ToolCalls,
        Usage::new(None, None, UsageFinality::Final),
        None,
    )));
    events
}

/// Drains both channels until the terminal control event, then performs a
/// bounded non-blocking drain so a duplicate terminal or trailing event
/// cannot hide behind the first `RunFinished`.
async fn drain_until_terminal(
    data: &mut mpsc::Receiver<RunEvent>,
    control: &mut mpsc::Receiver<RunEvent>,
) -> (Vec<RunEvent>, Vec<RunEvent>, RunFinished) {
    let mut datas = Vec::new();
    let mut controls = Vec::new();
    let mut data_open = true;
    let finished = tokio::time::timeout(TERMINAL_WAIT, async {
        loop {
            tokio::select! {
                event = data.recv(), if data_open => {
                    match event {
                        Some(event) => datas.push(event),
                        None => data_open = false,
                    }
                }
                event = control.recv() => {
                    match event {
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
                        None => panic!("control channel closed before a terminal event"),
                    }
                }
            }
        }
    })
    .await
    .expect("run reaches its terminal event within the bound");

    for _ in 0..3 {
        while let Ok(event) = data.try_recv() {
            datas.push(event);
        }
        while let Ok(event) = control.try_recv() {
            controls.push(event);
        }
        tokio::task::yield_now().await;
    }
    (datas, controls, finished)
}

fn count_payload(
    data: &[RunEvent],
    control: &[RunEvent],
    predicate: impl Fn(&EventPayload) -> bool,
) -> usize {
    data.iter()
        .chain(control.iter())
        .filter(|event| predicate(event.payload()))
        .count()
}

/// Asserts per-run sequences are contiguous from zero across both channels.
fn assert_contiguous(data: &[RunEvent], control: &[RunEvent]) {
    let mut all: Vec<&RunEvent> = data.iter().chain(control.iter()).collect();
    assert!(!all.is_empty(), "a run publishes events");
    all.sort_by_key(|event| event.seq());
    for (index, event) in all.iter().enumerate() {
        assert_eq!(
            event.seq(),
            index as u64,
            "per-run sequences stay contiguous from zero"
        );
    }
}

#[test]
fn submit_is_accepted_and_started_and_terminal_are_delivered() {
    let mut bed = make_bed(vec![stop_turn("done")], quick_config());
    let rt = test_rt();
    rt.block_on(async {
        let request = RequestId::new("req-accept").expect("valid request id");
        let submit = bed
            .runtime
            .submit(
                SubmitCommand::new(
                    request.clone(),
                    SessionId::new("sess-1").expect("valid session id"),
                    "do work",
                    "m0-test",
                )
                .expect("valid submit"),
            )
            .await;
        assert_eq!(submit.reply(), CommandReply::Accepted);
        assert_eq!(submit.request(), &request, "the reply echoes the request");
        let run = submit
            .run()
            .cloned()
            .expect("accepted submit carries a run id");

        let (data, control, finished) = drain_until_terminal(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(finished.persistence(), PersistenceState::Ephemeral);
        assert!(
            finished.error().is_none(),
            "a completed run carries no execution error"
        );

        assert_contiguous(&data, &control);
        assert_eq!(
            count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::RunStarted { .. }
            )),
            1,
            "exactly one RunStarted is delivered"
        );
        assert_eq!(
            count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::RunFinished(_)
            )),
            1,
            "exactly one terminal is delivered"
        );

        let mut all: Vec<&RunEvent> = data.iter().chain(control.iter()).collect();
        all.sort_by_key(|event| event.seq());
        assert!(
            all.iter().all(|event| event.run() == &run),
            "every delivered event is owned by the accepted run"
        );
        let first = all.first().expect("events exist");
        assert!(
            matches!(
                first.payload(),
                EventPayload::RunStarted { request: seen } if seen == &request
            ),
            "RunStarted arrives first and correlates with the submit request"
        );
        assert!(all.last().expect("events exist").is_terminal());
        assert_eq!(bed.provider.calls.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn second_submit_while_live_is_busy_and_leaves_the_run_untouched() {
    let (mut bed, mut entered, release) = make_gated_bed(vec![stop_turn("done")], quick_config());
    let rt = test_rt();
    rt.block_on(async {
        let first = bed.runtime.submit(submit_cmd("first")).await;
        assert_eq!(first.reply(), CommandReply::Accepted);
        let live = first.run().cloned().expect("run issued");

        tokio::time::timeout(GATE_WAIT, entered.recv())
            .await
            .expect("provider enters its first turn within the bound")
            .expect("gate stays open");

        let second = bed.runtime.submit(submit_cmd("second")).await;
        assert_eq!(second.reply(), CommandReply::Busy);
        assert_eq!(second.run(), Some(&live), "Busy names the live run");
        assert_eq!(
            second.request(),
            &RequestId::new("req-second").expect("valid request id"),
            "the rejected request is echoed"
        );

        release.send(()).expect("release the gated provider");
        let (data, control, finished) = drain_until_terminal(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_contiguous(&data, &control);
        assert_eq!(
            count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::RunStarted { .. }
            )),
            1,
            "the rejected submit never starts a run"
        );
        assert_eq!(
            count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::RunFinished(_)
            )),
            1
        );
        assert_eq!(
            bed.provider.calls.load(Ordering::SeqCst),
            1,
            "the rejected submit never reaches the provider"
        );
    });
}

#[test]
fn started_is_first_terminal_is_last_and_sequences_are_contiguous() {
    let mut bed = make_bed(
        vec![
            tool_turn(
                "checking",
                vec![candidate("host_read", r#"{"path":"src"}"#)],
            ),
            stop_turn("finished"),
        ],
        quick_config(),
    );
    let rt = test_rt();
    rt.block_on(async {
        let submit = bed.runtime.submit(submit_cmd("sequence")).await;
        assert_eq!(submit.reply(), CommandReply::Accepted);
        let (data, control, finished) = drain_until_terminal(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);

        assert!(
            !data.is_empty(),
            "presentation traffic is delivered on the data channel"
        );
        assert!(data.iter().all(|event| matches!(
            event.payload(),
            EventPayload::AssistantTextDelta(_) | EventPayload::ToolCallPreview { .. }
        )));
        assert_eq!(
            count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::RunStarted { .. }
            )),
            1
        );
        assert_eq!(
            count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::ToolStarted(_)
            )),
            1
        );
        assert_eq!(
            count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::ToolFinished(_)
            )),
            1
        );
        assert_eq!(
            count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::RunFinished(_)
            )),
            1
        );

        assert_contiguous(&data, &control);
        let mut all: Vec<&RunEvent> = data.iter().chain(control.iter()).collect();
        all.sort_by_key(|event| event.seq());
        assert!(matches!(
            all.first().expect("events exist").payload(),
            EventPayload::RunStarted { .. }
        ));
        assert!(all.last().expect("events exist").is_terminal());
        assert_eq!(bed.read.executions.load(Ordering::SeqCst), 1);
        assert_eq!(bed.write.executions.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn stale_run_identities_are_rejected_on_cancel_and_approve_paths() {
    let mut bed = make_bed(vec![stop_turn("done")], quick_config());
    let rt = test_rt();
    rt.block_on(async {
        let submit = bed.runtime.submit(submit_cmd("stale")).await;
        assert_eq!(submit.reply(), CommandReply::Accepted);
        let run = submit.run().cloned().expect("run issued");
        let (data, control, finished) = drain_until_terminal(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_contiguous(&data, &control);

        // The retained run: both command paths answer AlreadyFinalized and
        // echo the run.
        let late_cancel = bed
            .runtime
            .cancel(CancelCommand {
                request: RequestId::new("req-late-cancel").expect("valid request id"),
                run: run.clone(),
            })
            .await;
        assert_eq!(late_cancel.reply(), CommandReply::AlreadyFinalized);
        assert_eq!(late_cancel.run(), Some(&run));

        let late_approve = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-late-approve").expect("valid request id"),
                approval: ApprovalId::new("a-dead-0").expect("valid approval id"),
                run: run.clone(),
                call: CallId::new("c-dead-0").expect("valid call id"),
            })
            .await;
        assert_eq!(late_approve.reply(), CommandReply::AlreadyFinalized);
        assert_eq!(late_approve.run(), Some(&run));

        let late_deny = bed
            .runtime
            .deny(DenyCommand {
                request: RequestId::new("req-late-deny").expect("valid request id"),
                approval: ApprovalId::new("a-dead-0").expect("valid approval id"),
                run: run.clone(),
                call: CallId::new("c-dead-0").expect("valid call id"),
            })
            .await;
        assert_eq!(late_deny.reply(), CommandReply::AlreadyFinalized);

        // An identity this runtime never issued: both paths answer
        // StaleOrUnknownTarget and echo no run.
        let unknown = RunId::new("r-never-issued").expect("valid run id");
        let unknown_cancel = bed
            .runtime
            .cancel(CancelCommand {
                request: RequestId::new("req-unknown-cancel").expect("valid request id"),
                run: unknown.clone(),
            })
            .await;
        assert_eq!(unknown_cancel.reply(), CommandReply::StaleOrUnknownTarget);
        assert_eq!(unknown_cancel.run(), None);

        let unknown_approve = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-unknown-approve").expect("valid request id"),
                approval: ApprovalId::new("a-unknown-0").expect("valid approval id"),
                run: unknown.clone(),
                call: CallId::new("c-unknown-0").expect("valid call id"),
            })
            .await;
        assert_eq!(unknown_approve.reply(), CommandReply::StaleOrUnknownTarget);
        assert_eq!(unknown_approve.run(), None);
    });
}

#[test]
fn stale_run_identities_do_not_disturb_a_live_run() {
    let (mut bed, mut entered, release) = make_gated_bed(vec![stop_turn("done")], quick_config());
    let rt = test_rt();
    rt.block_on(async {
        let submit = bed.runtime.submit(submit_cmd("live")).await;
        assert_eq!(submit.reply(), CommandReply::Accepted);
        tokio::time::timeout(GATE_WAIT, entered.recv())
            .await
            .expect("provider enters its first turn within the bound")
            .expect("gate stays open");

        let unknown = RunId::new("r-unknown-live").expect("valid run id");
        let cancel = bed
            .runtime
            .cancel(CancelCommand {
                request: RequestId::new("req-stale-cancel").expect("valid request id"),
                run: unknown.clone(),
            })
            .await;
        assert_eq!(cancel.reply(), CommandReply::StaleOrUnknownTarget);
        assert_eq!(cancel.run(), None);

        let approve = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-stale-approve").expect("valid request id"),
                approval: ApprovalId::new("a-unknown-live").expect("valid approval id"),
                run: unknown.clone(),
                call: CallId::new("c-unknown-live").expect("valid call id"),
            })
            .await;
        assert_eq!(approve.reply(), CommandReply::StaleOrUnknownTarget);

        let deny = bed
            .runtime
            .deny(DenyCommand {
                request: RequestId::new("req-stale-deny").expect("valid request id"),
                approval: ApprovalId::new("a-unknown-live").expect("valid approval id"),
                run: unknown,
                call: CallId::new("c-unknown-live").expect("valid call id"),
            })
            .await;
        assert_eq!(deny.reply(), CommandReply::StaleOrUnknownTarget);

        release.send(()).expect("release the gated provider");
        let (data, control, finished) = drain_until_terminal(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Completed,
            "stale identities never cancel the live run"
        );
        assert_contiguous(&data, &control);
        assert_eq!(
            count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::RunFinished(_)
            )),
            1
        );
    });
}

#[test]
fn run_ids_never_alias_across_independent_runtime_instances() {
    let mut first = make_bed(vec![stop_turn("first")], quick_config());
    let mut second = make_bed(vec![stop_turn("second")], quick_config());
    let mut third = make_bed(vec![stop_turn("third")], quick_config());
    let rt = test_rt();
    rt.block_on(async {
        let a = first.runtime.submit(submit_cmd("instance-a")).await;
        let b = second.runtime.submit(submit_cmd("instance-b")).await;
        let c = third.runtime.submit(submit_cmd("instance-c")).await;
        for reply in [&a, &b, &c] {
            assert_eq!(reply.reply(), CommandReply::Accepted);
        }
        let ids = [
            a.run().cloned().expect("run issued"),
            b.run().cloned().expect("run issued"),
            c.run().cloned().expect("run issued"),
        ];
        let unique: HashSet<&RunId> = ids.iter().collect();
        assert_eq!(
            unique.len(),
            ids.len(),
            "run ids never alias across runtime instances"
        );

        // An id issued by another instance is unknown here, live or finalized.
        let cross_live = first
            .runtime
            .cancel(CancelCommand {
                request: RequestId::new("req-cross-live").expect("valid request id"),
                run: ids[1].clone(),
            })
            .await;
        assert_eq!(cross_live.reply(), CommandReply::StaleOrUnknownTarget);

        let (d1, c1, f1) = drain_until_terminal(&mut first.data, &mut first.control).await;
        let (d2, c2, f2) = drain_until_terminal(&mut second.data, &mut second.control).await;
        let (d3, c3, f3) = drain_until_terminal(&mut third.data, &mut third.control).await;
        assert_eq!(f1.outcome(), RunOutcome::Completed);
        assert_eq!(f2.outcome(), RunOutcome::Completed);
        assert_eq!(f3.outcome(), RunOutcome::Completed);
        for (data, control) in [(&d1, &c1), (&d2, &c2), (&d3, &c3)] {
            assert_contiguous(data, control);
        }

        let cross_finalized = second
            .runtime
            .cancel(CancelCommand {
                request: RequestId::new("req-cross-finalized").expect("valid request id"),
                run: ids[0].clone(),
            })
            .await;
        assert_eq!(cross_finalized.reply(), CommandReply::StaleOrUnknownTarget);
        assert_eq!(cross_finalized.run(), None);

        let own_finalized = second
            .runtime
            .cancel(CancelCommand {
                request: RequestId::new("req-own-finalized").expect("valid request id"),
                run: ids[1].clone(),
            })
            .await;
        assert_eq!(own_finalized.reply(), CommandReply::AlreadyFinalized);
        assert_eq!(own_finalized.run(), Some(&ids[1]));
    });
}
