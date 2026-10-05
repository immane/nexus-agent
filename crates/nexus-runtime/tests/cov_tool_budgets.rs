#![forbid(unsafe_code)]

//! Public-boundary hardening for effective tool-output budget enforcement.
//!
//! These tests drive the real runtime end to end with minimal in-file doubles:
//! a scripted provider and a scripted tool that deliberately returns oversized
//! or fitting outcomes. The contract under test:
//! - the effective budget is the minimum of the configured policy budget, the
//!   global M0 cap, and a declared provider output capability, and it is the
//!   budget both the tool context and every model request observe;
//! - oversized tool content is cut on a UTF-8 character boundary, flagged
//!   `truncated`, and never turned into a run failure by itself;
//! - fitting content stays byte-for-byte intact, and a caller-provided
//!   truncation flag is never cleared;
//! - status, effect, and evidence survive the cut unchanged;
//! - the bounded result is exactly what the next model turn receives;
//! - tool-call budget exhaustion (per run and per turn) maps to
//!   `RunOutcome::LimitReached` with a typed `ResourceLimit` error and no
//!   fabricated tool outcome, while an execution-timeout exhaustion records
//!   an explicit `TimedOut`/`Unknown`/`Uncertain` tool outcome;
//! - a zero effective budget is rejected at wiring time, never treated as
//!   unbounded.
//!
//! Determinism: fixed scripts and budgets; the only waits are bounded
//! `tokio::time::timeout` backstops and the channel-released blocking worker
//! used by the per-tool timeout case.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nexus_core::{
    AgentError, CallCandidate, CommandReply, EffectState, ErrorCategory, EventPayload, Evidence,
    ExecutionStatus, FinishReason, Limits, M0_REVISION, ModelContextItem, ModelRequest,
    PersistenceState, ProviderCapabilities, ProviderContext, ProviderEvent, ProviderPort,
    RequestId, RetryGuidance, RunEvent, RunFinished, RunOutcome, SessionId, SubmitCommand,
    ToolCall, ToolContext, ToolId, ToolOutcome, ToolPort, ToolSpec, TurnFinished, Usage,
    UsageFinality,
};
use nexus_runtime::{Policy, Runtime, RuntimeConfig};
use tokio::sync::mpsc;

/// The global M0-test output cap that no configured or declared budget may
/// widen.
const GLOBAL_CAP: usize = Limits::M0_TEST_TOOL_OUTPUT_BYTES;

/// Scripted provider: returns one pre-recorded batch per call, records every
/// request, and declares an optional output-byte capability.
struct ScriptedProvider {
    script: Mutex<VecDeque<Vec<ProviderEvent>>>,
    requests: Mutex<Vec<ModelRequest>>,
    max_output_bytes: Option<u32>,
}

impl ScriptedProvider {
    fn new(script: Vec<Vec<ProviderEvent>>, max_output_bytes: Option<u32>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            requests: Mutex::new(Vec::new()),
            max_output_bytes,
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

    fn seen_budgets(&self) -> Vec<usize> {
        self.requests
            .lock()
            .expect("requests readable")
            .iter()
            .map(ModelRequest::output_budget_bytes)
            .collect()
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
            max_output_bytes: self.max_output_bytes,
        }
    }

    fn stream(&self, request: &ModelRequest, _context: &ProviderContext) -> Vec<ProviderEvent> {
        self.requests
            .lock()
            .expect("requests readable")
            .push(request.clone());
        self.script
            .lock()
            .expect("script readable")
            .pop_front()
            .unwrap_or_else(|| Self::stop_turn("idle"))
    }
}

/// One scripted tool outcome.
struct ScriptedOutcome {
    status: ExecutionStatus,
    effect: EffectState,
    evidence: Evidence,
    content: String,
    truncated: bool,
}

/// Tool double returning scripted outcomes and recording the effective output
/// budget of every dispatch.
struct BudgetTool {
    spec: ToolSpec,
    script: Mutex<VecDeque<ScriptedOutcome>>,
    seen_budgets: Mutex<Vec<usize>>,
    executions: AtomicUsize,
}

impl BudgetTool {
    fn new(name: &str) -> Self {
        Self {
            spec: ToolSpec::new(
                ToolId::new(name, M0_REVISION).expect("valid tool id"),
                format!("budget fake {name}"),
                r#"{"type":"object"}"#,
            )
            .expect("spec builds"),
            script: Mutex::new(VecDeque::new()),
            seen_budgets: Mutex::new(Vec::new()),
            executions: AtomicUsize::new(0),
        }
    }

    #[must_use]
    fn push(
        mut self,
        status: ExecutionStatus,
        effect: EffectState,
        evidence: Evidence,
        content: impl Into<String>,
        truncated: bool,
    ) -> Self {
        self.script
            .get_mut()
            .expect("script lock")
            .push_back(ScriptedOutcome {
                status,
                effect,
                evidence,
                content: content.into(),
                truncated,
            });
        self
    }
}

impl ToolPort for BudgetTool {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn execute(&self, _call: &ToolCall, context: &ToolContext) -> ToolOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        self.seen_budgets
            .lock()
            .expect("budgets readable")
            .push(context.output_budget_bytes());
        let scripted = self
            .script
            .lock()
            .expect("script readable")
            .pop_front()
            .expect("scripted outcome exists for every execution");
        ToolOutcome::from_bounded_content(
            scripted.status,
            scripted.effect,
            scripted.evidence,
            scripted.content,
            scripted.truncated,
        )
        .expect("scripted outcome fields satisfy the invariants")
    }
}

/// Blocking tool for the timeout path: waits on a channel that the test
/// releases after the terminal event, bounded by its own five-second cap.
struct BlockingTool {
    spec: ToolSpec,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    executions: AtomicUsize,
}

impl BlockingTool {
    fn new() -> (Self, std::sync::mpsc::Sender<()>) {
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        (
            Self {
                spec: ToolSpec::new(
                    ToolId::new("host_read", M0_REVISION).expect("valid tool id"),
                    "blocking fake",
                    r#"{"type":"object"}"#,
                )
                .expect("spec builds"),
                release: Mutex::new(release_rx),
                executions: AtomicUsize::new(0),
            },
            release_tx,
        )
    }
}

impl ToolPort for BlockingTool {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn execute(&self, _call: &ToolCall, _context: &ToolContext) -> ToolOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        let release = self.release.lock().expect("release lock");
        let _ = release.recv_timeout(Duration::from_secs(5));
        ToolOutcome::new(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            "late ok",
            false,
        )
        .expect("late outcome builds")
    }
}

/// Live runtime plus its event receivers and inspectable doubles.
struct Bed {
    runtime: Runtime,
    data: mpsc::Receiver<RunEvent>,
    control: mpsc::Receiver<RunEvent>,
    provider: Arc<ScriptedProvider>,
    tool: Arc<BudgetTool>,
}

fn bed(
    limits: Limits,
    provider_max: Option<u32>,
    tool: BudgetTool,
    script: Vec<Vec<ProviderEvent>>,
) -> Bed {
    let provider = Arc::new(ScriptedProvider::new(script, provider_max));
    let tool = Arc::new(tool);
    let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![tool.clone()];
    let (runtime, streams) = Runtime::try_new(
        RuntimeConfig {
            limits,
            policy: Policy::m0_test(),
            has_approval_handler: false,
        },
        provider.clone(),
        tools,
    )
    .expect("valid runtime wiring");
    Bed {
        runtime,
        data: streams.data,
        control: streams.control,
        provider,
        tool,
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
        "budget probe",
        "test-profile",
    )
    .expect("submit builds")
}

fn read_candidate(item: &str, reference: &str, path: &str) -> CallCandidate {
    CallCandidate::new(
        item,
        reference,
        "host_read",
        format!(r#"{{"path":"{path}"}}"#),
    )
    .expect("candidate builds")
}

struct Collected {
    data: Vec<RunEvent>,
    control: Vec<RunEvent>,
}

/// Drains both channels until the terminal control event, then keeps
/// collecting anything still buffered so a duplicate terminal cannot hide.
async fn collect_until_finished(
    data: &mut mpsc::Receiver<RunEvent>,
    control: &mut mpsc::Receiver<RunEvent>,
) -> (Collected, RunFinished) {
    let mut datas = Vec::new();
    let mut controls = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
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
                            let terminal = event.is_terminal();
                            controls.push(event);
                            if terminal {
                                break;
                            }
                        }
                        None => panic!("control channel closed before a terminal event"),
                    }
                }
            }
        }
    })
    .await
    .expect("run reaches a terminal event promptly");
    while let Ok(event) = data.try_recv() {
        datas.push(event);
    }
    while let Ok(event) = control.try_recv() {
        controls.push(event);
    }
    let finished = controls
        .iter()
        .find_map(|event| match event.payload() {
            EventPayload::RunFinished(finished) => Some(finished.clone()),
            _ => None,
        })
        .expect("terminal event carries RunFinished");
    (
        Collected {
            data: datas,
            control: controls,
        },
        finished,
    )
}

async fn run_to_terminal(bed: &mut Bed, tag: &str) -> (Collected, RunFinished) {
    let submit = bed.runtime.submit(submit_cmd(tag)).await;
    assert_eq!(submit.reply(), CommandReply::Accepted, "run is accepted");
    collect_until_finished(&mut bed.data, &mut bed.control).await
}

fn tool_finished_outcomes(collected: &Collected) -> Vec<&ToolOutcome> {
    collected
        .control
        .iter()
        .filter_map(|event| match event.payload() {
            EventPayload::ToolFinished(info) => Some(&info.outcome),
            _ => None,
        })
        .collect()
}

fn terminal_events(collected: &Collected) -> usize {
    collected
        .data
        .iter()
        .chain(collected.control.iter())
        .filter(|event| event.is_terminal())
        .count()
}

fn assert_resource_limit(error: &AgentError, message: &str) {
    assert_eq!(error.category(), ErrorCategory::ResourceLimit);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(error.message(), message);
}

/// An oversized successful result is cut to the configured effective budget,
/// flagged, and still completes the run; the tool context, every model
/// request, and the next turn's conversation all observe the bounded bytes.
#[test]
fn oversized_output_is_cut_to_the_effective_budget_with_a_visible_flag() {
    let mut limits = Limits::m0_test();
    limits.max_tool_output_bytes = 64;
    let tool = BudgetTool::new("host_read").push(
        ExecutionStatus::Succeeded,
        EffectState::KnownApplied,
        Evidence::HostObserved,
        "x".repeat(200),
        false,
    );
    let mut bed = bed(
        limits,
        None,
        tool,
        vec![
            ScriptedProvider::tool_turn(vec![read_candidate("item-0", "prov-0", "src")]),
            ScriptedProvider::stop_turn("done"),
        ],
    );
    let rt = test_rt();
    rt.block_on(async {
        let (collected, finished) = run_to_terminal(&mut bed, "oversized").await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Completed,
            "a truncated tool result is not a run failure"
        );
        assert_eq!(
            finished.persistence(),
            PersistenceState::Ephemeral,
            "M0 persistence stays ephemeral"
        );
        assert_eq!(
            bed.tool
                .seen_budgets
                .lock()
                .expect("budgets readable")
                .as_slice(),
            &[64],
            "the tool context observes the effective budget"
        );
        assert_eq!(
            bed.provider.seen_budgets(),
            vec![64, 64],
            "every model request observes the same effective budget"
        );
        assert_eq!(bed.tool.executions.load(Ordering::SeqCst), 1);

        let outcomes = tool_finished_outcomes(&collected);
        assert_eq!(outcomes.len(), 1);
        let outcome = outcomes[0];
        assert_eq!(outcome.content(), "x".repeat(64));
        assert!(outcome.is_truncated(), "the cut is explicitly flagged");
        assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
        assert_eq!(outcome.effect(), EffectState::KnownApplied);
        assert_eq!(outcome.evidence(), Evidence::HostObserved);
        assert_eq!(terminal_events(&collected), 1);

        let requests = bed.provider.requests.lock().expect("requests readable");
        let second = requests.get(1).expect("second model turn was requested");
        let fed_back = second
            .conversation()
            .iter()
            .find_map(|item| match item {
                ModelContextItem::ToolResult { outcome, .. } => Some(outcome),
                _ => None,
            })
            .expect("the bounded result is fed back to the model");
        assert_eq!(fed_back.content(), "x".repeat(64));
        assert!(fed_back.is_truncated());
        assert_eq!(fed_back.status(), ExecutionStatus::Succeeded);
    });
}

/// Content that fits exactly or under the effective budget is intact and not
/// flagged; a caller-provided `true` flag is never cleared.
#[test]
fn within_budget_output_is_intact_and_never_flagged() {
    let mut limits = Limits::m0_test();
    limits.max_tool_output_bytes = 4;
    let tool = BudgetTool::new("host_read")
        // Exactly four bytes: the 2-byte characters must survive whole.
        .push(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            "éé",
            false,
        )
        // Under budget with a caller flag: the flag is preserved, not cleared.
        .push(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            "ok",
            true,
        );
    let mut bed = bed(
        limits,
        None,
        tool,
        vec![
            ScriptedProvider::tool_turn(vec![
                read_candidate("item-0", "prov-0", "a"),
                read_candidate("item-1", "prov-1", "b"),
            ]),
            ScriptedProvider::stop_turn("done"),
        ],
    );
    let rt = test_rt();
    rt.block_on(async {
        let (collected, finished) = run_to_terminal(&mut bed, "within-budget").await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        let outcomes = tool_finished_outcomes(&collected);
        assert_eq!(outcomes.len(), 2);
        assert_eq!(
            outcomes[0].content(),
            "éé",
            "exact-budget content is byte-for-byte intact"
        );
        assert!(!outcomes[0].is_truncated(), "no cut, no fabricated flag");
        assert_eq!(outcomes[0].status(), ExecutionStatus::Succeeded);
        assert_eq!(outcomes[0].effect(), EffectState::KnownApplied);
        assert_eq!(outcomes[0].evidence(), Evidence::HostObserved);
        assert_eq!(outcomes[1].content(), "ok");
        assert!(
            outcomes[1].is_truncated(),
            "a caller-provided true flag is never cleared"
        );
        assert_eq!(outcomes[1].status(), ExecutionStatus::Succeeded);
        assert_eq!(
            bed.tool
                .seen_budgets
                .lock()
                .expect("budgets readable")
                .as_slice(),
            &[4, 4]
        );
    });
}

/// A budget that cannot hold the next character backs off to the previous
/// character boundary instead of splitting a multi-byte sequence.
#[test]
fn oversized_multibyte_output_is_cut_on_a_character_boundary() {
    let mut limits = Limits::m0_test();
    limits.max_tool_output_bytes = 5;
    let tool = BudgetTool::new("host_read").push(
        ExecutionStatus::Succeeded,
        EffectState::KnownApplied,
        Evidence::HostObserved,
        "ééé",
        false,
    );
    let mut bed = bed(
        limits,
        None,
        tool,
        vec![
            ScriptedProvider::tool_turn(vec![read_candidate("item-0", "prov-0", "src")]),
            ScriptedProvider::stop_turn("done"),
        ],
    );
    let rt = test_rt();
    rt.block_on(async {
        let (collected, finished) = run_to_terminal(&mut bed, "multibyte").await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        let outcomes = tool_finished_outcomes(&collected);
        assert_eq!(outcomes.len(), 1);
        let outcome = outcomes[0];
        assert_eq!(
            outcome.content(),
            "éé",
            "the cut backs off the 2-byte character"
        );
        assert_eq!(outcome.content().len(), 4);
        assert!(outcome.content().is_char_boundary(outcome.content().len()));
        assert!(outcome.is_truncated());
        assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
        assert_eq!(outcome.effect(), EffectState::KnownApplied);
        assert_eq!(outcome.evidence(), Evidence::HostObserved);
    });
}

/// A declared provider output capability is part of the effective-budget
/// minimum and narrows both the tool context and every model request.
#[test]
fn provider_output_cap_narrows_the_effective_budget() {
    let mut limits = Limits::m0_test();
    limits.max_tool_output_bytes = 1024;
    let tool = BudgetTool::new("host_read").push(
        ExecutionStatus::Succeeded,
        EffectState::KnownApplied,
        Evidence::HostObserved,
        "y".repeat(32),
        false,
    );
    let mut bed = bed(
        limits,
        Some(8),
        tool,
        vec![
            ScriptedProvider::tool_turn(vec![read_candidate("item-0", "prov-0", "src")]),
            ScriptedProvider::stop_turn("done"),
        ],
    );
    let rt = test_rt();
    rt.block_on(async {
        let (collected, finished) = run_to_terminal(&mut bed, "provider-cap").await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(
            bed.tool
                .seen_budgets
                .lock()
                .expect("budgets readable")
                .as_slice(),
            &[8],
            "the provider cap lowers the tool budget"
        );
        assert_eq!(bed.provider.seen_budgets(), vec![8, 8]);
        let outcomes = tool_finished_outcomes(&collected);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].content(), "y".repeat(8));
        assert!(outcomes[0].is_truncated());
    });
}

/// A configured policy budget above the global M0 cap never widens the
/// effective budget past that cap.
#[test]
fn configured_budget_never_widens_past_the_global_m0_cap() {
    let mut limits = Limits::m0_test();
    limits.max_tool_output_bytes = GLOBAL_CAP + 1;
    let tool = BudgetTool::new("host_read").push(
        ExecutionStatus::Succeeded,
        EffectState::KnownApplied,
        Evidence::HostObserved,
        "z".repeat(GLOBAL_CAP + 1),
        false,
    );
    let mut bed = bed(
        limits,
        None,
        tool,
        vec![
            ScriptedProvider::tool_turn(vec![read_candidate("item-0", "prov-0", "src")]),
            ScriptedProvider::stop_turn("done"),
        ],
    );
    let rt = test_rt();
    rt.block_on(async {
        let (collected, finished) = run_to_terminal(&mut bed, "global-cap").await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(
            bed.tool
                .seen_budgets
                .lock()
                .expect("budgets readable")
                .as_slice(),
            &[GLOBAL_CAP]
        );
        assert_eq!(bed.provider.seen_budgets(), vec![GLOBAL_CAP, GLOBAL_CAP]);
        let outcomes = tool_finished_outcomes(&collected);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].content().len(), GLOBAL_CAP);
        assert!(outcomes[0].is_truncated());
    });
}

/// A failed tool result is still cut to the effective budget with its status,
/// effect, and evidence preserved; truncation alone never fails the run.
#[test]
fn truncation_preserves_failure_status_effect_and_evidence() {
    let mut limits = Limits::m0_test();
    limits.max_tool_output_bytes = 16;
    let tool = BudgetTool::new("host_read").push(
        ExecutionStatus::Failed,
        EffectState::KnownNotApplied,
        Evidence::PluginReported,
        "f".repeat(40),
        false,
    );
    let mut bed = bed(
        limits,
        None,
        tool,
        vec![
            ScriptedProvider::tool_turn(vec![read_candidate("item-0", "prov-0", "src")]),
            ScriptedProvider::stop_turn("done"),
        ],
    );
    let rt = test_rt();
    rt.block_on(async {
        let (collected, finished) = run_to_terminal(&mut bed, "failed-truncated").await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Completed,
            "a failed tool result is still a completed run"
        );
        let outcomes = tool_finished_outcomes(&collected);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].content(), "f".repeat(16));
        assert!(outcomes[0].is_truncated());
        assert_eq!(outcomes[0].status(), ExecutionStatus::Failed);
        assert_eq!(outcomes[0].effect(), EffectState::KnownNotApplied);
        assert_eq!(outcomes[0].evidence(), Evidence::PluginReported);
    });
}

/// Exhausting the per-run tool-call budget during admission maps to
/// `LimitReached` with a typed `ResourceLimit` error; nothing dispatches and
/// no tool outcome is fabricated for the already-admitted candidate.
#[test]
fn per_run_tool_call_budget_exhaustion_maps_to_limit_reached() {
    let mut limits = Limits::m0_test();
    limits.max_tool_calls_per_run = 1;
    let tool = BudgetTool::new("host_read").push(
        ExecutionStatus::Succeeded,
        EffectState::KnownApplied,
        Evidence::HostObserved,
        "ok",
        false,
    );
    let mut bed = bed(
        limits,
        None,
        tool,
        vec![ScriptedProvider::tool_turn(vec![
            read_candidate("item-0", "prov-0", "a"),
            read_candidate("item-1", "prov-1", "b"),
        ])],
    );
    let rt = test_rt();
    rt.block_on(async {
        let (collected, finished) = run_to_terminal(&mut bed, "run-call-budget").await;
        assert_eq!(finished.outcome(), RunOutcome::LimitReached);
        assert_eq!(finished.persistence(), PersistenceState::Ephemeral);
        assert_resource_limit(
            finished
                .error()
                .expect("admission exhaustion carries a typed error"),
            "tool call budget for run exhausted",
        );
        assert_eq!(
            bed.provider.seen_budgets().len(),
            1,
            "the second model turn is never requested"
        );
        assert_eq!(
            bed.tool.executions.load(Ordering::SeqCst),
            0,
            "no admitted candidate dispatches"
        );
        assert!(
            tool_finished_outcomes(&collected).is_empty(),
            "no outcome is fabricated for the discarded candidate"
        );
        assert_eq!(terminal_events(&collected), 1);
    });
}

/// Exhausting the per-turn tool-call budget during admission maps to
/// `LimitReached` with a typed `ResourceLimit` error and the same empty
/// outcome shape.
#[test]
fn per_turn_tool_call_budget_exhaustion_maps_to_limit_reached() {
    let mut limits = Limits::m0_test();
    limits.max_tool_calls_per_turn = 1;
    let tool = BudgetTool::new("host_read").push(
        ExecutionStatus::Succeeded,
        EffectState::KnownApplied,
        Evidence::HostObserved,
        "ok",
        false,
    );
    let mut bed = bed(
        limits,
        None,
        tool,
        vec![ScriptedProvider::tool_turn(vec![
            read_candidate("item-0", "prov-0", "a"),
            read_candidate("item-1", "prov-1", "b"),
        ])],
    );
    let rt = test_rt();
    rt.block_on(async {
        let (collected, finished) = run_to_terminal(&mut bed, "turn-call-budget").await;
        assert_eq!(finished.outcome(), RunOutcome::LimitReached);
        assert_resource_limit(
            finished
                .error()
                .expect("admission exhaustion carries a typed error"),
            "tool call budget for turn exhausted",
        );
        assert_eq!(bed.tool.executions.load(Ordering::SeqCst), 0);
        assert!(tool_finished_outcomes(&collected).is_empty());
        assert_eq!(terminal_events(&collected), 1);
    });
}

/// Exhausting the per-tool deadline maps to `LimitReached` and records an
/// explicit `TimedOut`/`Unknown`/`Uncertain` outcome: an execution timeout is
/// never a truncation and never a fabricated success. The quarantined worker
/// is released afterwards so ownership clears before the runtime shuts down.
#[test]
fn per_tool_timeout_maps_to_limit_reached_with_an_unknown_effect_outcome() {
    let mut limits = Limits::m0_test();
    limits.per_tool_timeout = Duration::from_millis(50);
    let (tool, release) = BlockingTool::new();
    let tool = Arc::new(tool);
    let provider = Arc::new(ScriptedProvider::new(
        vec![ScriptedProvider::tool_turn(vec![read_candidate(
            "item-0", "prov-0", "src",
        )])],
        None,
    ));
    let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![tool.clone()];
    let (runtime, streams) = Runtime::try_new(
        RuntimeConfig {
            limits,
            policy: Policy::m0_test(),
            has_approval_handler: false,
        },
        provider,
        tools,
    )
    .expect("valid runtime wiring");
    let mut data = streams.data;
    let mut control = streams.control;
    let rt = test_rt();
    rt.block_on(async {
        let submit = runtime.submit(submit_cmd("tool-timeout")).await;
        assert_eq!(submit.reply(), CommandReply::Accepted);
        let (collected, finished) = collect_until_finished(&mut data, &mut control).await;
        assert_eq!(finished.outcome(), RunOutcome::LimitReached);
        if let Some(error) = finished.error() {
            assert_eq!(error.category(), ErrorCategory::ResourceLimit);
            assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        }
        let outcomes = tool_finished_outcomes(&collected);
        assert_eq!(outcomes.len(), 1, "the timed-out call keeps one outcome");
        assert_eq!(outcomes[0].status(), ExecutionStatus::TimedOut);
        assert_eq!(outcomes[0].effect(), EffectState::Unknown);
        assert_eq!(outcomes[0].evidence(), Evidence::Uncertain);
        assert!(
            !outcomes[0].is_truncated(),
            "a timeout is not an output cut"
        );
        assert_eq!(terminal_events(&collected), 1);

        // Release the quarantined worker and wait (bounded) for ownership to
        // clear so the blocking thread terminates before the runtime drops.
        release.send(()).expect("release the blocked worker");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let response = runtime.submit(submit_cmd("after-timeout")).await;
                if response.reply() == CommandReply::Accepted {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("quarantine clears after the blocked worker terminates");
        let (_, next) = collect_until_finished(&mut data, &mut control).await;
        assert_eq!(next.outcome(), RunOutcome::Completed);
        assert_eq!(tool.executions.load(Ordering::SeqCst), 1);
    });
}

/// A provider that declares no usable output budget is rejected at wiring
/// time: zero never silently becomes unbounded.
#[test]
fn zero_provider_output_cap_is_rejected_at_wiring() {
    let provider = Arc::new(ScriptedProvider::new(vec![], Some(0)));
    let tool: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![Arc::new(BudgetTool::new("host_read"))];
    let error = match Runtime::try_new(
        RuntimeConfig {
            limits: Limits::m0_test(),
            policy: Policy::m0_test(),
            has_approval_handler: false,
        },
        provider,
        tool,
    ) {
        Ok(_) => panic!("a zero effective output budget must be rejected"),
        Err(error) => error,
    };
    assert_eq!(error.category(), ErrorCategory::UnsupportedCapability);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(error.message(), "provider declares no usable output budget");
}
