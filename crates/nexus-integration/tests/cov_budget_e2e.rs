#![forbid(unsafe_code)]

//! End-to-end budget enforcement against the real runtime and the real fakes.
//!
//! These proofs cover the four budgets whose behavior only exists in an
//! assembled run, where the effective budget is the minimum of configuration
//! and provider capability and where exhaustion is observed through the event
//! stream rather than a return value:
//!
//! - the tool-output budget cuts an over-budget recorded outcome to the
//!   effective budget on a UTF-8 character boundary, marks it truncated, and
//!   leaves `status`, `effect`, and `evidence` untouched, so truncated output
//!   is never presented as complete (lock 12);
//! - retained-context exhaustion ends the run `LimitReached` before the next
//!   model invocation is even requested;
//! - the per-run call budget ends the run `LimitReached` at admission time, so
//!   the over-budget candidate and any already-admitted remainder reach neither
//!   the host port nor an outcome record;
//! - the per-turn call budget does the same with a fresh per-run budget, and
//!   resets every turn while the per-run budget accumulates across turns.
//!
//! Every boundary is asserted from both sides: content exactly at the budget
//! is recorded whole and untruncated, a conversation exactly at the retained
//! bound still calls the model, and a call budget one above the exhausted
//! bound completes the run. Timeouts below come from the shared helpers and
//! are generous failure backstops only: assertions read recorded invariants,
//! never wall-clock timing.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use nexus_core::{
    CallId, CommandReply, EffectState, ErrorCategory, EventPayload, Evidence, ExecutionStatus,
    GetSnapshotCommand, Limits, M0_REVISION, ProviderEvent, RequestId, RetryGuidance, RunEvent,
    RunFinished, RunLifecycle, RunOutcome, ToolCall, ToolContext, ToolId, ToolOutcome, ToolPort,
    ToolSpec,
};
use nexus_fakes::{FakeProvider, stop_turn, tool_turn};
use nexus_runtime::{EventStreams, Policy, Runtime, RuntimeConfig};

/// Effective tool-output budget for the output-bound tests. Small enough that
/// a cut is unmistakable, and far above the provider text-delta bound check
/// applied to the same scripted turns.
const OUTPUT_BUDGET: usize = 64;

/// Headless M0 policy with caller-chosen budgets. Automatic scoped reads
/// dispatch without an approver, so every assertion below observes budget
/// enforcement rather than the absence of an approval handler.
fn config_with(limits: Limits) -> RuntimeConfig {
    RuntimeConfig {
        limits,
        policy: Policy::m0_test(),
        has_approval_handler: false,
    }
}

/// Limits with `max_tool_output_bytes` set to the effective test budget.
fn output_limited() -> Limits {
    let mut limits = Limits::m0_test();
    limits.max_tool_output_bytes = OUTPUT_BUDGET;
    limits
}

/// Every recorded `ToolFinished` outcome across both channels, in sequence
/// order. Outcomes ride the control channel, so a recorded outcome is the only
/// proof that a call dispatched at all.
fn recorded_outcomes(data: &[RunEvent], control: &[RunEvent]) -> Vec<(CallId, ToolOutcome)> {
    let mut all: Vec<&RunEvent> = data.iter().chain(control.iter()).collect();
    all.sort_by_key(|event| event.seq());
    all.iter()
        .filter_map(|event| match event.payload() {
            EventPayload::ToolFinished(info) => Some((info.call.clone(), info.outcome.clone())),
            _ => None,
        })
        .collect()
}

/// Distinct call identities that entered execution, in sequence order.
fn started_calls(data: &[RunEvent], control: &[RunEvent]) -> Vec<CallId> {
    let mut all: Vec<&RunEvent> = data.iter().chain(control.iter()).collect();
    all.sort_by_key(|event| event.seq());
    all.iter()
        .filter_map(|event| match event.payload() {
            EventPayload::ToolStarted(info) => Some(info.call.clone()),
            _ => None,
        })
        .collect()
}

/// Asserts a budget-exhaustion terminal: `LimitReached` carrying its static
/// resource-limit cause, never a fabricated completion and never a silent
/// fallback that hides which budget bound.
fn assert_limit_terminal(finished: &RunFinished, message: &str) {
    assert_eq!(finished.outcome(), RunOutcome::LimitReached);
    let error = finished
        .error()
        .expect("budget exhaustion records its exact cause");
    assert_eq!(error.category(), ErrorCategory::ResourceLimit);
    assert_eq!(error.message(), message);
    assert_eq!(
        error.retry(),
        RetryGuidance::DoNotRetry,
        "an exhausted budget is never retried into the same bound"
    );
}

/// Asserts that not one call reached a host port or produced an outcome. An
/// admission-time budget refusal must abandon the whole remainder, including
/// calls already admitted earlier in the same turn.
fn assert_never_dispatched(data: &[RunEvent], control: &[RunEvent], executions: usize) {
    assert!(
        started_calls(data, control).is_empty(),
        "an exhausted call budget starts no execution: {data:?} {control:?}"
    );
    assert!(
        recorded_outcomes(data, control).is_empty(),
        "a call that never dispatched records no outcome"
    );
    assert_eq!(executions, 0, "the host port is never invoked");
}

/// Test-local tool port that answers with fixed content regardless of the
/// dispatch-time output budget.
///
/// [`FakeTool::oversized`] truncates itself to the budget it is handed, so no
/// shipped double can return an over-budget outcome for the runtime's own
/// recording bound to act on. Ignoring the announced budget is exactly the
/// misbehaving host this test needs in order to observe the runtime cut.
struct FixedOutputTool {
    spec: ToolSpec,
    content: String,
    executions: AtomicUsize,
    observed_budgets: Mutex<Vec<usize>>,
}

impl FixedOutputTool {
    fn new(content: &str) -> Self {
        let spec = ToolSpec::new(
            ToolId::new("host_read", M0_REVISION).expect("fixed tool id builds"),
            "returns fixed content over budget",
            r#"{"type":"object"}"#,
        )
        .expect("fixed tool spec builds");
        Self {
            spec,
            content: content.to_owned(),
            executions: AtomicUsize::new(0),
            observed_budgets: Mutex::new(Vec::new()),
        }
    }

    fn execution_count(&self) -> usize {
        self.executions.load(Ordering::SeqCst)
    }

    /// The output budget each dispatch announced, in dispatch order.
    fn observed_budgets(&self) -> Vec<usize> {
        self.observed_budgets
            .lock()
            .expect("observed budgets readable")
            .clone()
    }
}

impl ToolPort for FixedOutputTool {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn execute(&self, _call: &ToolCall, context: &ToolContext) -> ToolOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        self.observed_budgets
            .lock()
            .expect("observed budgets writable")
            .push(context.output_budget_bytes());
        ToolOutcome::new(
            ExecutionStatus::Succeeded,
            EffectState::KnownNotApplied,
            Evidence::HostObserved,
            self.content.clone(),
            false,
        )
        .expect("fixed content stays inside the global M0 cap")
    }
}

/// Live runtime over the fixed-output double, with both event streams. The
/// shared [`common::Bed`] is typed to `Arc<FakeTool>` and cannot carry a
/// test-local port, so this mirrors its shape without editing the helper.
struct FixedBed {
    runtime: Runtime,
    data: tokio::sync::mpsc::Receiver<RunEvent>,
    control: tokio::sync::mpsc::Receiver<RunEvent>,
    provider: Arc<FakeProvider>,
    tool: Arc<FixedOutputTool>,
}

/// Builds the fixed-output bed: the over-budget double as the only automatic
/// read tool, plus the real scripted provider.
fn fixed_output_bed(script: Vec<Vec<ProviderEvent>>, limits: Limits, content: &str) -> FixedBed {
    let provider = Arc::new(FakeProvider::new(script));
    let tool = Arc::new(FixedOutputTool::new(content));
    let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![tool.clone()];
    let (runtime, streams): (Runtime, EventStreams) =
        Runtime::new(config_with(limits), provider.clone(), tools);
    FixedBed {
        runtime,
        data: streams.data,
        control: streams.control,
        provider,
        tool,
    }
}

/// Content whose 64th byte is the first byte of a three-byte `€`, followed by
/// more ASCII. A byte-wise cut at the budget would split the character, so the
/// recorded prefix must stop one byte earlier and still be valid text.
fn straddling_content() -> String {
    let mut content = "a".repeat(OUTPUT_BUDGET - 1);
    content.push('€');
    content.push_str(&"b".repeat(OUTPUT_BUDGET));
    content
}

/// A single automatic read call, then a stop turn the run is expected never to
/// reach when a budget binds first.
fn read_then_stop() -> Vec<Vec<ProviderEvent>> {
    vec![
        tool_turn(vec![common::candidate_with(
            "item-0",
            "prov-ref-0",
            "host_read",
            r#"{"path":"a"}"#,
        )]),
        stop_turn("done"),
    ]
}

/// The over-budget outcome is cut to the effective budget, flagged as
/// incomplete, and keeps its status, effect, and evidence.
///
/// A host that ignores the budget it was handed must not be able to push
/// unbounded text into the event stream: the runtime records the bounded
/// prefix and marks it truncated so nothing downstream can read the cut as a
/// complete answer. The run itself stays `Completed` because the bound is a
/// record-level limit, not a lifecycle failure, and the run is retried into
/// the same bound.
#[test]
fn output_budget_cuts_recorded_outcome_on_a_character_boundary() {
    let rt = common::test_rt();
    rt.block_on(async {
        let declared = straddling_content();
        assert!(
            declared.len() > OUTPUT_BUDGET,
            "the host declares more than the budget"
        );
        let mut bed = fixed_output_bed(read_then_stop(), output_limited(), &declared);
        let response = bed.runtime.submit(common::submit_cmd("outcut")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(
            bed.provider.call_count(),
            2,
            "the truncated record still lets the run finish normally"
        );
        assert_eq!(bed.tool.execution_count(), 1);
        assert_eq!(
            bed.tool.observed_budgets(),
            vec![OUTPUT_BUDGET],
            "the host is told the effective budget and is expected to ignore it"
        );

        let outcomes = recorded_outcomes(&data, &control);
        assert_eq!(outcomes.len(), 1, "exactly one recorded outcome");
        let outcome = &outcomes[0].1;
        assert!(
            outcome.is_truncated(),
            "cut content is explicitly marked incomplete, never complete success"
        );
        assert!(
            outcome.content().len() <= OUTPUT_BUDGET,
            "recorded content fits the budget: {} bytes",
            outcome.content().len()
        );
        assert!(
            declared.starts_with(outcome.content()),
            "the record is a prefix of what the host declared, not a rewrite"
        );
        assert!(
            declared.len() > outcome.content().len(),
            "bytes were actually removed"
        );
        // Byte 64 lands inside the three-byte character, so a correct cut
        // stops at byte 63 and the recorded prefix is the leading ASCII run.
        assert_eq!(
            outcome.content(),
            "a".repeat(OUTPUT_BUDGET - 1),
            "the cut backs up to the nearest character boundary"
        );
        assert_eq!(
            (outcome.status(), outcome.effect(), outcome.evidence()),
            (
                ExecutionStatus::Succeeded,
                EffectState::KnownNotApplied,
                Evidence::HostObserved
            ),
            "bounding content rewrites nothing but the text"
        );
    });
}

/// Content exactly at the budget is recorded whole and untruncated.
///
/// The cut above must not become an unconditional trim: a host that honors
/// the announced budget exactly is recorded verbatim with the truncation flag
/// left clear.
#[test]
fn output_exactly_at_the_budget_is_recorded_whole_and_untruncated() {
    let rt = common::test_rt();
    rt.block_on(async {
        let declared = "c".repeat(OUTPUT_BUDGET);
        assert_eq!(declared.len(), OUTPUT_BUDGET);
        let mut bed = fixed_output_bed(read_then_stop(), output_limited(), &declared);
        let response = bed.runtime.submit(common::submit_cmd("out-exact")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(bed.tool.execution_count(), 1);

        let outcomes = recorded_outcomes(&data, &control);
        assert_eq!(outcomes.len(), 1);
        let outcome = &outcomes[0].1;
        assert!(
            !outcome.is_truncated(),
            "content at the budget is complete and is not flagged"
        );
        assert_eq!(
            outcome.content(),
            declared,
            "the whole declared content is recorded"
        );
    });
}

/// Retained-context exhaustion ends the run before the next model call.
///
/// The bound is checked against the accumulated conversation, not against the
/// items a single turn adds. Once the retained bound is passed, the run must
/// report `LimitReached` with the resource-limit cause and must not issue
/// another model request: the check runs ahead of the provider call, so a
/// provider that only fails after being asked is never consulted here.
#[test]
fn retained_context_exhaustion_ends_the_run_before_the_next_model_call() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = Limits::m0_test();
        // The conversation is the submitted user item plus, per call, an
        // assistant-call item and a tool-result item: three items after one
        // call, so a bound of two is passed before the second model turn.
        limits.retained_context_items = 2;
        let mut bed = common::make_bed(read_then_stop(), config_with(limits));
        let response = bed.runtime.submit(common::submit_cmd("ctx")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);
        assert_limit_terminal(&finished, "retained context budget exhausted");
        assert_eq!(
            bed.provider.call_count(),
            1,
            "the bound is checked before the provider is asked again"
        );

        // The call that fit the bound still ran to a real, recorded outcome:
        // exhaustion truncates the future, never the recorded past.
        assert_eq!(bed.read_tool.execution_count(), 1);
        assert_eq!(started_calls(&data, &control).len(), 1);
        let outcomes = recorded_outcomes(&data, &control);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].1.status(), ExecutionStatus::Succeeded);
        assert!(outcomes[0].1.content().contains("read ok"));
        assert_eq!(
            outcomes[0].0,
            started_calls(&data, &control)[0],
            "the recorded outcome belongs to the started call"
        );

        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-ctx-snap").expect("valid"),
                run,
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("a finished run keeps a snapshot");
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::LimitReached)
        );
        assert_eq!(snapshot.known_outcomes().len(), 1);
    });
}

/// A conversation exactly at the retained bound still calls the model.
///
/// The context check rejects strictly above the bound, so one item of headroom
/// below the limit must complete the run normally; otherwise exhaustion would
/// fire a turn early and truncate honest work.
#[test]
fn retained_context_exactly_at_the_bound_still_calls_the_model() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = Limits::m0_test();
        limits.retained_context_items = 3;
        let mut bed = common::make_bed(read_then_stop(), config_with(limits));
        let response = bed.runtime.submit(common::submit_cmd("ctx-exact")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert!(
            finished.error().is_none(),
            "a conversation at the bound is not an error"
        );
        assert_eq!(
            bed.provider.call_count(),
            2,
            "the follow-up model turn is issued when the bound is met exactly"
        );
        assert_eq!(bed.read_tool.execution_count(), 1);
        assert_eq!(recorded_outcomes(&data, &control).len(), 1);
    });
}

/// Per-run call exhaustion stops admission, so nothing dispatches.
///
/// The bound is checked while admitting candidates, before any of them is
/// validated or dispatched. Exhausting it must end the run `LimitReached` and
/// abandon every candidate in the turn, including the ones already admitted
/// before the refused one: a refused budget cannot leave work in flight.
#[test]
fn per_run_call_budget_exhaustion_admits_nothing_and_dispatches_nothing() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = Limits::m0_test();
        limits.max_tool_calls_per_run = 2;
        let mut bed = common::make_bed(three_read_calls(), config_with(limits));
        let response = bed.runtime.submit(common::submit_cmd("runbudget")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);
        assert_limit_terminal(&finished, "tool call budget for run exhausted");
        assert_eq!(
            bed.provider.call_count(),
            1,
            "exhaustion ends the run at the first turn that cannot be admitted"
        );
        assert_never_dispatched(&data, &control, bed.read_tool.execution_count());
        assert_eq!(bed.write_tool.execution_count(), 0);
    });
}

/// Per-turn call exhaustion stops admission the same way, with a per-run
/// budget that is nowhere near exhausted.
///
/// Only the per-turn bound can bind here, so the identical observable shape as
/// the per-run proof pins the check to the per-turn counter instead of a
/// cumulative one.
#[test]
fn per_turn_call_budget_exhaustion_admits_nothing_and_dispatches_nothing() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = Limits::m0_test();
        limits.max_tool_calls_per_turn = 2;
        assert!(
            limits.max_tool_calls_per_run > limits.max_tool_calls_per_turn,
            "only the per-turn bound can bind in this run"
        );
        let mut bed = common::make_bed(three_read_calls(), config_with(limits));
        let response = bed.runtime.submit(common::submit_cmd("turnbudget")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);
        assert_limit_terminal(&finished, "tool call budget for turn exhausted");
        assert_eq!(bed.provider.call_count(), 1);
        assert_never_dispatched(&data, &control, bed.read_tool.execution_count());
    });
}

/// The per-run budget accumulates across turns and abandons the remainder.
///
/// Two turns of two calls against a per-run bound of three admit three calls
/// and refuse the fourth. The three admitted calls are real: the first turn
/// dispatches and records both of its calls, while the call admitted in the
/// exhausting turn is abandoned with no start and no outcome. The refusal is
/// therefore a property of the cumulative counter, not of a single turn's size.
#[test]
fn per_run_call_budget_accumulates_across_turns_and_abandons_the_remainder() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = Limits::m0_test();
        limits.max_tool_calls_per_run = 3;
        let mut bed = common::make_bed(
            vec![
                tool_turn(two_read_calls("a", "b")),
                tool_turn(two_read_calls("c", "d")),
            ],
            config_with(limits),
        );
        let response = bed
            .runtime
            .submit(common::submit_cmd("run-budget-cross-turn"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);
        assert_limit_terminal(&finished, "tool call budget for run exhausted");
        assert_eq!(
            bed.provider.call_count(),
            2,
            "the second turn is admitted and is the one that cannot finish"
        );

        let started = started_calls(&data, &control);
        assert_eq!(started.len(), 2, "only the first turn dispatched");
        assert_eq!(bed.read_tool.execution_count(), 2);
        let outcomes = recorded_outcomes(&data, &control);
        assert_eq!(
            outcomes.len(),
            2,
            "both executed calls have an outcome, the abandoned third has none"
        );
        for (call, outcome) in &outcomes {
            assert!(
                started.contains(call),
                "recorded outcomes belong to started calls only: {call:?}"
            );
            assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
        }
    });
}

/// A per-run budget one above the exhausted point completes the run.
///
/// The exhaustion above fires exactly at the bound. One extra call of headroom
/// must admit all four candidates and finish the run, so the bound is not an
/// off-by-one that refuses honest work.
#[test]
fn per_run_call_budget_one_above_the_bound_completes() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = Limits::m0_test();
        limits.max_tool_calls_per_run = 4;
        let mut bed = common::make_bed(
            vec![
                tool_turn(two_read_calls("a", "b")),
                tool_turn(two_read_calls("c", "d")),
                stop_turn("done"),
            ],
            config_with(limits),
        );
        let response = bed
            .runtime
            .submit(common::submit_cmd("run-budget-headroom"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert!(finished.error().is_none());
        assert_eq!(bed.read_tool.execution_count(), 4);
        assert_eq!(started_calls(&data, &control).len(), 4);
        assert_eq!(recorded_outcomes(&data, &control).len(), 4);
    });
}

/// The per-turn budget resets every turn while the per-run budget keeps count.
///
/// Two calls per turn against a per-turn bound of two exhausts nothing: the
/// counter is per turn, so the second turn admits its own two calls and the run
/// completes. This is what separates the per-turn check from the cumulative
/// one, and it is the regression a shared counter would silently introduce.
#[test]
fn per_turn_call_budget_resets_every_turn() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = Limits::m0_test();
        limits.max_tool_calls_per_turn = 2;
        let mut bed = common::make_bed(
            vec![
                tool_turn(two_read_calls("a", "b")),
                tool_turn(two_read_calls("c", "d")),
                stop_turn("done"),
            ],
            config_with(limits),
        );
        let response = bed
            .runtime
            .submit(common::submit_cmd("turn-budget-reset"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert!(finished.error().is_none());
        assert_eq!(
            bed.provider.call_count(),
            3,
            "both tool turns plus the closing stop turn"
        );
        assert_eq!(
            bed.read_tool.execution_count(),
            4,
            "a per-turn bound does not accumulate across turns"
        );
        assert_eq!(started_calls(&data, &control).len(), 4);
        assert_eq!(recorded_outcomes(&data, &control).len(), 4);
    });
}

/// Three distinct automatic read candidates in a single tool turn, which is
/// one call over both two-call bounds under test.
fn three_read_calls() -> Vec<Vec<ProviderEvent>> {
    vec![tool_turn(vec![
        common::candidate_with("item-0", "prov-ref-0", "host_read", r#"{"path":"a"}"#),
        common::candidate_with("item-1", "prov-ref-1", "host_read", r#"{"path":"b"}"#),
        common::candidate_with("item-2", "prov-ref-2", "host_read", r#"{"path":"c"}"#),
    ])]
}

/// Two distinct automatic read candidates addressing `first` and `second`.
/// Identities are turn-local, so a second turn reuses the same keys and
/// references without colliding with the first.
fn two_read_calls(first: &str, second: &str) -> Vec<nexus_core::CallCandidate> {
    vec![
        common::candidate_with(
            "item-0",
            "prov-ref-0",
            "host_read",
            &format!(r#"{{"path":"{first}"}}"#),
        ),
        common::candidate_with(
            "item-1",
            "prov-ref-1",
            "host_read",
            &format!(r#"{{"path":"{second}"}}"#),
        ),
    ]
}
