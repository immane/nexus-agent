#![forbid(unsafe_code)]
// Shared helpers are used selectively per test target; silence per-target
// dead-code lints rather than duplicating helpers into every file.
#![allow(dead_code)]

//! Shared deterministic helpers for the P6 integration tests.
//!
//! Every helper drives the real [`Runtime`](nexus_runtime::Runtime) with the
//! real [`FakeProvider`](nexus_fakes::FakeProvider) /
//! [`FakeTool`](nexus_fakes::FakeTool) doubles. Timeouts below are generous
//! failure backstops only: passing runs settle in milliseconds and assert on
//! recorded invariants (counts, sequences, terminal outcomes), never on
//! wall-clock timing.

use std::sync::Arc;
use std::time::Duration;

use nexus_core::{
    ApprovalId, CallCandidate, CallId, EventPayload, FinishReason, Limits, ProviderEvent,
    RequestId, RunEvent, RunFinished, SessionId, SubmitCommand, ToolPort, TurnFinished, Usage,
    UsageFinality,
};
use nexus_fakes::{FakeProvider, FakeTool};
use nexus_runtime::{EventStreams, Policy, Runtime, RuntimeConfig};

/// Live runtime plus its two event receivers and inspectable doubles.
pub struct Bed {
    pub runtime: Runtime,
    pub data: tokio::sync::mpsc::Receiver<RunEvent>,
    pub control: tokio::sync::mpsc::Receiver<RunEvent>,
    pub provider: Arc<FakeProvider>,
    pub read_tool: Arc<FakeTool>,
    pub write_tool: Arc<FakeTool>,
}

/// Single-threaded driver for the async runtime APIs.
pub fn test_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime builds")
}

/// M0-test budgets with an approval handler present.
pub fn quick_config() -> RuntimeConfig {
    RuntimeConfig {
        limits: Limits::m0_test(),
        policy: Policy::m0_test(),
        has_approval_handler: true,
    }
}

/// M0-test budgets with no approval handler (headless denial semantics).
pub fn auto_config() -> RuntimeConfig {
    RuntimeConfig {
        has_approval_handler: false,
        ..quick_config()
    }
}

/// Correlated submit for the shared `sess-1` session.
pub fn submit_cmd(tag: &str) -> SubmitCommand {
    SubmitCommand::new(
        RequestId::new(format!("req-{tag}")).expect("request builds"),
        SessionId::new("sess-1").expect("session builds"),
        "do work",
        "m0-test",
    )
    .expect("submit builds")
}

/// Single call candidate with fixed turn-local identity.
pub fn candidate(tool: &str, args: &str) -> CallCandidate {
    candidate_with("item-0", "prov-ref-0", tool, args)
}

/// Call candidate with explicit turn-local identity.
pub fn candidate_with(item: &str, provider_ref: &str, tool: &str, args: &str) -> CallCandidate {
    CallCandidate::new(item, provider_ref, tool, args).expect("candidate builds")
}

/// Builds a runtime bed with read-only `host_read` and mutating `host_write`.
pub fn make_bed(script: Vec<Vec<ProviderEvent>>, config: RuntimeConfig) -> Bed {
    make_bed_with_tools(script, config, FakeTool::read_only(), FakeTool::mutation())
}

/// Builds a runtime bed with caller-chosen tool doubles.
pub fn make_bed_with_tools(
    script: Vec<Vec<ProviderEvent>>,
    config: RuntimeConfig,
    read: FakeTool,
    write: FakeTool,
) -> Bed {
    let provider = Arc::new(FakeProvider::new(script));
    let read_tool = Arc::new(read);
    let write_tool = Arc::new(write);
    let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![read_tool.clone(), write_tool.clone()];
    let (runtime, streams): (Runtime, EventStreams) = Runtime::new(config, provider.clone(), tools);
    Bed {
        runtime,
        data: streams.data,
        control: streams.control,
        provider,
        read_tool,
        write_tool,
    }
}

/// One model turn of `fragments` alternating-item text deltas (an output
/// flood once `fragments` exceeds the data-channel bound) followed by the
/// given tool candidates and a tool-calls terminal.
pub fn flood_turn(fragments: usize, candidates: Vec<CallCandidate>) -> Vec<ProviderEvent> {
    let mut turn = Vec::with_capacity(fragments + candidates.len() + 1);
    for index in 0..fragments {
        turn.push(ProviderEvent::TextDelta {
            item_key: format!("item-{}", index % 2),
            text: "x".repeat(64),
        });
    }
    for ready in candidates {
        turn.push(ProviderEvent::ToolCallReady(ready));
    }
    turn.push(ProviderEvent::TurnFinished(TurnFinished::new(
        FinishReason::ToolCalls,
        Usage::new(None, None, UsageFinality::Final),
        None,
    )));
    turn
}

/// Drains both channels until the terminal control event, then collects any
/// still-buffered data events without blocking.
pub async fn drain_until_finished(
    data: &mut tokio::sync::mpsc::Receiver<RunEvent>,
    control: &mut tokio::sync::mpsc::Receiver<RunEvent>,
) -> (Vec<RunEvent>, Vec<RunEvent>, RunFinished) {
    let mut datas = Vec::new();
    let mut controls = Vec::new();
    let finished = tokio::time::timeout(Duration::from_secs(10), async {
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
                            let finished = match event.payload() {
                                EventPayload::RunFinished(finished) => Some(finished.clone()),
                                _ => None,
                            };
                            controls.push(event);
                            if let Some(finished) = finished {
                                break finished;
                            }
                        }
                        None => panic!("control channel closed before terminal"),
                    }
                }
            }
        }
    })
    .await
    .expect("run reaches terminal promptly");
    while let Ok(event) = data.try_recv() {
        datas.push(event);
    }
    (datas, controls, finished)
}

/// Collects control events only (leaving the data channel to saturate) until
/// `stop` matches. Proves the control path stays responsive under load.
pub async fn collect_control_until(
    control: &mut tokio::sync::mpsc::Receiver<RunEvent>,
    stop: impl Fn(&RunEvent) -> bool,
) -> Vec<RunEvent> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut out = Vec::new();
        while let Some(event) = control.recv().await {
            let done = stop(&event);
            out.push(event);
            if done {
                break;
            }
        }
        out
    })
    .await
    .expect("control event arrives promptly")
}

/// Finds the first live approval request in `events`.
pub fn find_approval(events: &[RunEvent]) -> Option<(ApprovalId, CallId)> {
    events.iter().find_map(|event| match event.payload() {
        EventPayload::ApprovalRequired(notice) => {
            Some((notice.approval.clone(), notice.call.clone()))
        }
        _ => None,
    })
}

/// Finds every approval request in `events`, in arrival order.
pub fn all_approvals(events: &[RunEvent]) -> Vec<(ApprovalId, CallId)> {
    events
        .iter()
        .filter_map(|event| match event.payload() {
            EventPayload::ApprovalRequired(notice) => {
                Some((notice.approval.clone(), notice.call.clone()))
            }
            _ => None,
        })
        .collect()
}

/// Asserts per-run sequence numbers are contiguous from zero across both
/// channels. Only valid when no data event was dropped (no flood).
pub fn assert_contiguous(data: &[RunEvent], control: &[RunEvent]) {
    let mut all: Vec<&RunEvent> = data.iter().chain(control.iter()).collect();
    assert!(!all.is_empty(), "run publishes events");
    all.sort_by_key(|event| event.seq());
    for (index, event) in all.iter().enumerate() {
        assert_eq!(
            event.seq(),
            index as u64,
            "sequence numbers stay contiguous"
        );
    }
}

/// Counts events with a matching payload across both channels.
pub fn count_payload(
    data: &[RunEvent],
    control: &[RunEvent],
    matches: impl Fn(&EventPayload) -> bool,
) -> usize {
    data.iter()
        .chain(control.iter())
        .filter(|event| matches(event.payload()))
        .count()
}
