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

/// Drains both channels until the terminal control event, then collects every
/// still-buffered event from BOTH channels without blocking.
///
/// The post-terminal drain is deliberate: the runtime publishes its terminal
/// event last, so any event still buffered after it (or any duplicate
/// terminal) must be collected instead of hiding behind the first terminal.
/// A first-terminal break followed by a data-only drain would silently miss a
/// duplicate `RunFinished` on the control channel.
pub async fn drain_until_finished(
    data: &mut tokio::sync::mpsc::Receiver<RunEvent>,
    control: &mut tokio::sync::mpsc::Receiver<RunEvent>,
) -> (Vec<RunEvent>, Vec<RunEvent>, RunFinished) {
    let mut datas = Vec::new();
    let mut controls = Vec::new();
    let mut data_open = true;
    let finished = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            tokio::select! {
                event = data.recv(), if data_open => {
                    match event {
                        Some(event) => datas.push(event),
                        // A closed presentation channel must not spin the
                        // select loop; the control terminal is still required.
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
                        None => panic!("control channel closed before terminal"),
                    }
                }
            }
        }
    })
    .await
    .expect("run reaches terminal promptly");
    // Non-blocking drain of both channels until dry across two consecutive
    // quiet passes separated by a scheduler yield. All runtime sends precede
    // `RunFinished` in the driver, but a concurrently scheduled publisher
    // (for example a control-outbox flusher or a duplicate terminal) could
    // otherwise hide behind the first terminal or behind a single dry pass.
    // The round cap keeps a misbehaving producer from looping the helper.
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

/// Counts terminal `RunFinished` events across both channels. Values above
/// one are always a runtime defect, never a consumer allowance.
pub fn terminal_count(data: &[RunEvent], control: &[RunEvent]) -> usize {
    count_payload(data, control, |payload| {
        matches!(payload, EventPayload::RunFinished(_))
    })
}

/// Asserts exactly one terminal event exists across both channels. A
/// first-terminal break without a full drain could hide a duplicate.
pub fn assert_single_terminal(data: &[RunEvent], control: &[RunEvent]) {
    let terminals = terminal_count(data, control);
    assert_eq!(
        terminals, 1,
        "exactly one terminal RunFinished across both channels, saw {terminals}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::{AssistantText, PersistenceState, RunOutcome, TurnId, UsageFinality};

    fn envelope(run: &str, seq: u64, payload: EventPayload) -> RunEvent {
        RunEvent::new(
            SessionId::new("sess-1").expect("valid"),
            nexus_core::RunId::new(run).expect("valid"),
            seq,
            payload,
        )
    }

    fn text_payload(text: &str) -> EventPayload {
        EventPayload::AssistantTextDelta(
            AssistantText::new(TurnId::new("t1-0").expect("valid"), "item-0", text)
                .expect("fragment builds"),
        )
    }

    fn terminal_payload(outcome: RunOutcome) -> EventPayload {
        EventPayload::RunFinished(
            RunFinished::new(outcome, PersistenceState::Ephemeral, None)
                .expect("terminal record builds"),
        )
    }

    /// Regression for the helper bug: after the first terminal, events still
    /// buffered on EITHER channel must be collected. The old helper drained
    /// only the data channel, so a duplicate terminal on control vanished and
    /// duplicate-terminal defects were untestable.
    #[test]
    fn drain_until_finished_collects_post_terminal_events_on_both_channels() {
        let rt = test_rt();
        rt.block_on(async {
            let (data_tx, mut data_rx) = tokio::sync::mpsc::channel(8);
            let (control_tx, mut control_rx) = tokio::sync::mpsc::channel(8);

            data_tx
                .send(envelope("run-1", 0, text_payload("before")))
                .await
                .expect("data send");
            control_tx
                .send(envelope(
                    "run-1",
                    1,
                    EventPayload::RunStarted {
                        request: RequestId::new("req-1").expect("valid"),
                    },
                ))
                .await
                .expect("started send");
            control_tx
                .send(envelope(
                    "run-1",
                    2,
                    terminal_payload(RunOutcome::Completed),
                ))
                .await
                .expect("terminal send");
            // Post-terminal traffic: a trailing usage update plus a duplicate
            // terminal. Both must be observed by a full drain.
            control_tx
                .send(envelope(
                    "run-1",
                    3,
                    EventPayload::UsageUpdated(Usage::new(None, None, UsageFinality::Final)),
                ))
                .await
                .expect("post-terminal usage send");
            control_tx
                .send(envelope(
                    "run-1",
                    4,
                    terminal_payload(RunOutcome::Cancelled),
                ))
                .await
                .expect("duplicate terminal send");
            data_tx
                .send(envelope("run-1", 5, text_payload("after")))
                .await
                .expect("post-terminal data send");
            drop(data_tx);
            drop(control_tx);

            let (data, control, finished) =
                drain_until_finished(&mut data_rx, &mut control_rx).await;

            assert_eq!(finished.outcome(), RunOutcome::Completed);
            assert_eq!(data.len(), 2, "both data events collected: {data:?}");
            assert_eq!(
                control.len(),
                4,
                "all control events collected: {control:?}"
            );
            assert_eq!(
                terminal_count(&data, &control),
                2,
                "the duplicate terminal is visible to the assertion helpers"
            );
            assert_eq!(
                control.last().expect("control events").seq(),
                4,
                "the drain reached the true end of the control channel"
            );
        });
    }
}
