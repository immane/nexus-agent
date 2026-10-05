#![forbid(unsafe_code)]

//! Registration-scale coverage: many registered tools and the registration
//! gates that decide whether a run can start at all.
//!
//! Findings under test:
//! - with both a read and a mutation tool registered, each proposed call must
//!   reach its own tool: no argument crosses over to the sibling port, and the
//!   two calls keep distinct host identities and distinct effect states;
//! - a candidate naming an unregistered tool is denied at admission, before
//!   any dispatch, so the denial costs no execution and the sibling known
//!   call in the same turn still runs;
//! - a registered tool set exactly at `MAX_TOOL_DEFINITIONS` is accepted *and*
//!   usable: the model request carries every registered name in registration
//!   order, while one tool past the bound is refused;
//! - a provider without the `tool_calls` capability is refused while any tool
//!   is registered and accepted with zero tools, where the run then carries no
//!   enabled tools and no tool definitions.
//!
//! Determinism: scripted providers, zero-delay doubles, and recorded
//! invariants only (tool logs, call identities, observed requests, terminal
//! outcomes). No randomness, sleeps, or wall-clock assertions.

mod common;

use std::sync::Arc;
use std::time::Duration;

use nexus_core::{
    ApproveCommand, CallId, CommandReply, EffectState, ErrorCategory, EventPayload,
    ExecutionStatus, MAX_TOOL_DEFINITIONS, ModelRequest, ProviderCapabilities, ProviderContext,
    ProviderEvent, ProviderPort, RequestId, RunOutcome, ToolPort,
};
use nexus_fakes::{FakeProvider, FakeTool, stop_turn, tool_turn};
use nexus_runtime::{EventStreams, Runtime, RuntimeConfig};

/// Named registration doubles: the shared fake exposes arbitrary tool names
/// through its delayed constructor, and a zero delay keeps a filler tool from
/// ever sleeping if it is ever dispatched.
fn named_tools(count: usize) -> Vec<Arc<dyn ToolPort + Send + Sync>> {
    (0..count)
        .map(|index| {
            Arc::new(FakeTool::delayed(
                &format!("filler_{index}"),
                Duration::ZERO,
            )) as Arc<dyn ToolPort + Send + Sync>
        })
        .collect()
}

/// Provider double with the shared fake's scripting but the `tool_calls`
/// capability withdrawn. Only the capability gate is under test, so the
/// scripted turns stay identical to every other registration test.
struct TextOnlyProvider {
    inner: FakeProvider,
}

impl TextOnlyProvider {
    fn new(script: Vec<Vec<ProviderEvent>>) -> Self {
        Self {
            inner: FakeProvider::new(script),
        }
    }
}

impl ProviderPort for TextOnlyProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            text: true,
            streaming: true,
            tool_calls: false,
            structured_output: false,
            usage_reporting: true,
            max_context_items: None,
            max_output_bytes: None,
        }
    }

    fn stream(&self, request: &ModelRequest, context: &ProviderContext) -> Vec<ProviderEvent> {
        self.inner.stream(request, context)
    }

    fn adapter_identity(&self) -> &str {
        self.inner.adapter_identity()
    }

    fn continuation_scope(&self, profile: &str) -> String {
        self.inner.continuation_scope(profile)
    }
}

/// Both registered tools receive exactly their own call: the read port sees
/// only the read arguments under the automatic read scope, the mutation port
/// sees only the mutation arguments under the approved scope, and each call
/// keeps its own identity and effect state.
#[test]
fn read_and_mutation_tools_each_receive_only_their_own_call() {
    let rt = common::test_rt();
    rt.block_on(async {
        let read_args = r#"{"path":"src"}"#;
        let write_args = r#"{"path":"dst"}"#;
        let script = vec![
            tool_turn(vec![
                common::candidate_with("item-0", "prov-ref-0", "host_read", read_args),
                common::candidate_with("item-1", "prov-ref-1", "host_write", write_args),
            ]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("many-tools-dispatch"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        // The read is automatic, so the mutation approval is the only decision
        // the test has to make.
        let approvals = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let (approval, call) =
            common::find_approval(&approvals).expect("mutation approval requested");
        let approve = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-many-tools-approve").expect("valid"),
                approval,
                run: run.clone(),
                call,
            })
            .await;
        assert_eq!(approve.reply(), CommandReply::Accepted);

        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(approvals);

        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::ToolStarted(_)
            )),
            2,
            "each registered tool dispatched exactly once"
        );

        let read_log = bed.read_tool.log();
        assert_eq!(read_log.len(), 1, "the read tool ran once");
        assert_eq!(
            read_log[0].args, read_args,
            "the read port receives only its own arguments"
        );
        assert_eq!(
            read_log[0].scope.as_str(),
            "read:src",
            "the automatic read scope reaches the read port unchanged"
        );
        let write_log = bed.write_tool.log();
        assert_eq!(write_log.len(), 1, "the mutation tool ran once");
        assert_eq!(
            write_log[0].args, write_args,
            "the mutation port receives only its own arguments"
        );
        assert_eq!(
            write_log[0].scope.as_str(),
            "path:dst",
            "the approved scope reaches the mutation port unchanged"
        );
        assert_ne!(
            read_log[0].call, write_log[0].call,
            "the two dispatches own distinct host call identities"
        );

        // Correlate outcomes by call identity rather than channel position: the
        // read's `ToolFinished` was already consumed while waiting for the
        // mutation approval, and `control` now carries both halves.
        let outcomes: Vec<(CallId, ExecutionStatus, EffectState)> = control
            .iter()
            .filter_map(|event| match event.payload() {
                EventPayload::ToolFinished(info) => Some((
                    info.call.clone(),
                    info.outcome.status(),
                    info.outcome.effect(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(outcomes.len(), 2, "both calls report an outcome");
        let effect_of = |call: &CallId| {
            outcomes
                .iter()
                .find(|(observed, _, _)| observed == call)
                .map(|(_, status, effect)| (*status, *effect))
        };
        assert_eq!(
            effect_of(&read_log[0].call),
            Some((ExecutionStatus::Succeeded, EffectState::KnownNotApplied)),
            "the read reports its honest not-applied effect"
        );
        assert_eq!(
            effect_of(&write_log[0].call),
            Some((ExecutionStatus::Succeeded, EffectState::KnownApplied)),
            "the mutation reports its applied effect"
        );
        assert_eq!(
            bed.provider.call_count(),
            2,
            "one invocation per model turn"
        );
        common::assert_single_terminal(&data, &control);
    });
}

/// A candidate naming an unregistered tool is denied at admission: no tool
/// starts, the denial is published before the sibling known call is dispatched,
/// and the run continues honestly with the known call still executed.
#[test]
fn unknown_tool_is_denied_without_dispatch() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![
                common::candidate_with("item-0", "prov-ref-0", "host_absent", r#"{"path":"src"}"#),
                common::candidate_with("item-1", "prov-ref-1", "host_read", r#"{"path":"src"}"#),
            ]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, common::auto_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("unknown-tool-denial"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);

        let started: Vec<CallId> = control
            .iter()
            .chain(data.iter())
            .filter_map(|event| match event.payload() {
                EventPayload::ToolStarted(info) => Some(info.call.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            started.len(),
            1,
            "an unknown tool never enters execution: {started:?}"
        );
        let read_log = bed.read_tool.log();
        assert_eq!(
            read_log.len(),
            1,
            "the known call in the same turn still runs"
        );
        assert_eq!(bed.write_tool.execution_count(), 0);

        let denials: Vec<(CallId, ExecutionStatus, EffectState, String)> = control
            .iter()
            .chain(data.iter())
            .filter_map(|event| match event.payload() {
                EventPayload::ToolFinished(info) => Some((
                    info.call.clone(),
                    info.outcome.status(),
                    info.outcome.effect(),
                    info.outcome.content().to_owned(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(denials.len(), 2, "both admitted calls report an outcome");
        let denied = &denials[0];
        assert_eq!(denied.1, ExecutionStatus::Denied);
        assert_eq!(
            denied.2,
            EffectState::NotStarted,
            "a denied call reports that nothing started"
        );
        assert!(
            denied.3.contains("unknown tool"),
            "the denial names the unregistered tool as the reason: {}",
            denied.3
        );
        assert_ne!(
            denied.0, started[0],
            "the denied call owns a distinct host identity"
        );
        assert!(
            denial_published_before_dispatch(&denied.0, &started[0], &data, &control),
            "the denial is published at admission, before the surviving call dispatches"
        );
        assert_eq!(denials[1].0, started[0], "the surviving call is correlated");
        assert_eq!(denials[1].1, ExecutionStatus::Succeeded);

        assert_eq!(bed.provider.call_count(), 2, "the run continued one turn");
        common::assert_single_terminal(&data, &control);

        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(nexus_core::GetSnapshotCommand {
                request: RequestId::new("req-unknown-tool-snap").expect("valid"),
                run,
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("finalized snapshot");
        let known = snapshot.known_outcomes();
        assert_eq!(known.len(), 2, "the denial is a known outcome, not a gap");
        assert_eq!(known[0].status, ExecutionStatus::Denied);
        assert_eq!(known[0].effect, EffectState::NotStarted);
        assert_eq!(known[0].evidence, nexus_core::Evidence::HostObserved);
        assert_eq!(known[0].call, denied.0);
    });
}

/// The unknown-tool denial is admitted, so it spends the per-run call budget
/// like any other candidate: with a budget of one, the denial consumes it and
/// the sibling known call is refused as a limit rather than dispatched.
#[test]
fn unknown_tool_denial_consumes_the_per_run_call_budget() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = nexus_core::Limits::m0_test();
        limits.max_tool_calls_per_run = 1;
        let config = RuntimeConfig {
            limits,
            policy: nexus_runtime::Policy::m0_test(),
            has_approval_handler: false,
        };
        let script = vec![tool_turn(vec![
            common::candidate_with("item-0", "prov-ref-0", "host_absent", r#"{"path":"src"}"#),
            common::candidate_with("item-1", "prov-ref-1", "host_read", r#"{"path":"src"}"#),
        ])];
        let mut bed = common::make_bed(script, config);
        let response = bed
            .runtime
            .submit(common::submit_cmd("denial-budget"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::LimitReached,
            "the exhausted budget ends the run as a limit, never as a fabricated success"
        );
        assert_eq!(
            bed.read_tool.execution_count(),
            0,
            "the sibling call never dispatches once the budget is spent"
        );
        assert_eq!(
            bed.provider.call_count(),
            1,
            "an exhausted budget never requests another turn"
        );
        common::assert_single_terminal(&data, &control);
    });
}

/// A tool set exactly at `MAX_TOOL_DEFINITIONS` registers and stays usable: the
/// runtime accepts it and the model request carries every registered name in
/// registration order with an equal number of definitions.
#[test]
fn tool_count_boundary_registers_and_reaches_every_provider_request() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut tools = named_tools(MAX_TOOL_DEFINITIONS - 1);
        // Anchor the boundary set with the real read double so the accepted
        // registration is proven usable, not merely constructible.
        let read_tool = Arc::new(FakeTool::read_only());
        tools.push(read_tool.clone());
        assert_eq!(
            tools.len(),
            MAX_TOOL_DEFINITIONS,
            "the boundary set is exactly at the registration bound"
        );

        let provider = Arc::new(FakeProvider::new(vec![stop_turn("done")]));
        let (runtime, streams): (Runtime, EventStreams) =
            Runtime::new(common::quick_config(), provider.clone(), tools);
        let mut data = streams.data;
        let mut control = streams.control;
        let response = runtime
            .submit(common::submit_cmd("tool-count-boundary"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (_, _, finished) = common::drain_until_finished(&mut data, &mut control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);

        let expected: Vec<String> = (0..MAX_TOOL_DEFINITIONS - 1)
            .map(|index| format!("filler_{index}"))
            .chain(std::iter::once("host_read".to_owned()))
            .collect();
        let requests = provider.requests();
        assert_eq!(requests.len(), 1, "one invocation for the stop turn");
        let request = &requests[0];
        let enabled: Vec<&str> = request
            .enabled_tools()
            .iter()
            .map(|tool| tool.name())
            .collect();
        assert_eq!(
            enabled,
            expected.iter().map(String::as_str).collect::<Vec<_>>(),
            "every registered tool is enabled in registration order"
        );
        let definitions: Vec<&str> = request
            .tool_definitions()
            .iter()
            .map(|spec| spec.id().name())
            .collect();
        assert_eq!(
            definitions, enabled,
            "definitions match the enabled set one for one at the boundary"
        );
        assert_eq!(
            read_tool.execution_count(),
            0,
            "a text-only turn runs no tool"
        );

        // One tool past the bound is refused instead of silently truncated.
        let mut over = named_tools(MAX_TOOL_DEFINITIONS);
        over.push(read_tool);
        let error = Runtime::try_new(
            common::quick_config(),
            Arc::new(FakeProvider::new(vec![])),
            over,
        )
        .err()
        .expect("one tool past the registration bound is rejected");
        assert_eq!(error.category(), ErrorCategory::ResourceLimit);
        assert_eq!(error.message(), "too many registered tools");
    });
}

/// A provider that cannot call tools is refused while any tool is registered,
/// and accepted with zero tools: the resulting run enables nothing and defines
/// no tools, so a text-only turn completes without any tool traffic.
#[test]
fn text_only_provider_is_refused_with_tools_and_accepted_without() {
    let rt = common::test_rt();
    rt.block_on(async {
        let provider = Arc::new(TextOnlyProvider::new(vec![stop_turn("text only")]));
        assert!(
            !ProviderPort::capabilities(&*provider).tool_calls,
            "the double really withdraws the tool-call capability"
        );
        let tools = vec![
            Arc::new(FakeTool::read_only()) as Arc<dyn ToolPort + Send + Sync>,
            Arc::new(FakeTool::mutation()) as Arc<dyn ToolPort + Send + Sync>,
        ];
        let error = Runtime::try_new(common::quick_config(), provider.clone(), tools)
            .err()
            .expect("registered tools require the tool-call capability");
        assert_eq!(error.category(), ErrorCategory::UnsupportedCapability);
        assert_eq!(error.message(), "provider cannot call tools");

        let (runtime, streams) =
            Runtime::try_new(common::quick_config(), provider.clone(), Vec::new())
                .expect("a text-only provider is valid with zero tools");
        let mut data = streams.data;
        let mut control = streams.control;
        let response = runtime
            .submit(common::submit_cmd("text-only-registration"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) = common::drain_until_finished(&mut data, &mut control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        let requests = provider.inner.requests();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].enabled_tools().is_empty(),
            "a zero-tool registration enables no tools"
        );
        assert!(
            requests[0].tool_definitions().is_empty(),
            "a zero-tool registration defines no tools"
        );
        common::assert_single_terminal(&data, &control);
    });
}

/// True when the denial's publication sequence precedes the dispatched call's
/// `ToolStarted` sequence, proving the denial was published at admission time
/// rather than after execution.
fn denial_published_before_dispatch(
    denied: &CallId,
    started: &CallId,
    data: &[nexus_core::RunEvent],
    control: &[nexus_core::RunEvent],
) -> bool {
    let seq_of = |target: &CallId| {
        data.iter()
            .chain(control.iter())
            .find_map(|event| match event.payload() {
                EventPayload::ToolFinished(info) if &info.call == target => Some(event.seq()),
                EventPayload::ToolStarted(info) if &info.call == target => Some(event.seq()),
                _ => None,
            })
    };
    match (seq_of(denied), seq_of(started)) {
        (Some(denied_seq), Some(started_seq)) => denied_seq < started_seq,
        _ => false,
    }
}
