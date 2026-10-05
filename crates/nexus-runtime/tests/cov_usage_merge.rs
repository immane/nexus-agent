#![forbid(unsafe_code)]

//! Coverage hardening for terminal usage handling through the public
//! [`Runtime`] boundary.
//!
//! The unit tests inside the crate exercise the merge helper directly; this
//! file pins the observable contract end to end with a minimal in-file
//! scripted provider:
//!
//! - provisional-then-final streams publish every distinct update exactly
//!   once, in order, and a terminal record equal to the last final update is
//!   not duplicated;
//! - provisional counters are never promoted into an unknown terminal and
//!   never merged into a final terminal that reports its own counters;
//! - a terminal record that omits a counter retains the last *final* known
//!   value per counter, without downgrading it to unknown or zero;
//! - counters that stay unknown remain `None` (never fabricated zero), while
//!   an explicit provider zero remains a known zero;
//! - a provider `Failed` terminal after a usage-order violation keeps the
//!   provider's typed error (including a cancelled provider failure) instead
//!   of the protocol ordering diagnostic, and a rejected batch publishes no
//!   partial usage.
//!
//! All waits are bounded and the provider double is synchronous and
//! scripted, so the tests are deterministic.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nexus_core::{
    AgentError, CommandReply, ErrorCategory, EventPayload, FinishReason, Limits, ModelRequest,
    ProviderCapabilities, ProviderContext, ProviderEvent, ProviderPort, RequestId, RetryGuidance,
    RunEvent, RunFinished, RunOutcome, SessionId, SubmitCommand, TurnFinished, Usage,
    UsageFinality,
};
use nexus_runtime::{Policy, Runtime, RuntimeConfig};

/// Bound for every wait. The scripted provider never blocks, so exceeding it
/// means a hang rather than a slow machine.
const WAIT: Duration = Duration::from_secs(5);

/// Minimal scripted provider: one queued event batch per model turn.
struct ScriptedProvider {
    script: Mutex<VecDeque<Vec<ProviderEvent>>>,
}

impl ScriptedProvider {
    fn new(script: Vec<Vec<ProviderEvent>>) -> Self {
        Self {
            script: Mutex::new(script.into()),
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
        self.script
            .lock()
            .expect("script lock is not poisoned")
            .pop_front()
            .unwrap_or_default()
    }
}

fn config() -> RuntimeConfig {
    RuntimeConfig {
        limits: Limits::m0_test(),
        policy: Policy::m0_test(),
        has_approval_handler: false,
    }
}

fn submit_cmd(tag: &str) -> SubmitCommand {
    SubmitCommand::new(
        RequestId::new(format!("req-{tag}")).expect("request id is valid"),
        SessionId::new("sess-usage").expect("session id is valid"),
        "do work",
        "test-profile",
    )
    .expect("submit command is valid")
}

fn provisional(input: Option<u64>, output: Option<u64>) -> Usage {
    Usage::new(input, output, UsageFinality::Provisional)
}

fn final_usage(input: Option<u64>, output: Option<u64>) -> Usage {
    Usage::new(input, output, UsageFinality::Final)
}

fn stop_turn(usage: Usage) -> ProviderEvent {
    ProviderEvent::TurnFinished(TurnFinished::new(FinishReason::Stop, usage, None))
}

fn failed(category: ErrorCategory, message: &'static str) -> ProviderEvent {
    ProviderEvent::Failed(
        AgentError::new(category, message, RetryGuidance::DoNotRetry).expect("static error builds"),
    )
}

/// Submits one scripted run and drains the control channel until its single
/// terminal event, within [`WAIT`]. Asserts the delivered envelope invariants:
/// exactly one terminal, last; strictly increasing sequences; every event
/// owned by the submitted run.
async fn run_script(tag: &str, script: Vec<Vec<ProviderEvent>>) -> (Vec<RunEvent>, RunFinished) {
    let provider = Arc::new(ScriptedProvider::new(script));
    let (runtime, mut streams) = Runtime::new(config(), provider, Vec::new());
    let response = runtime.submit(submit_cmd(tag)).await;
    assert_eq!(response.reply(), CommandReply::Accepted);
    let run = response
        .run()
        .cloned()
        .expect("accepted submit issues a run");

    let (control, finished) = tokio::time::timeout(WAIT, async {
        let mut control = Vec::new();
        loop {
            let event = streams.control.recv().await.expect("control stays open");
            let terminal = match event.payload() {
                EventPayload::RunFinished(finished) => Some(finished.clone()),
                _ => None,
            };
            control.push(event);
            if let Some(finished) = terminal {
                break (control, finished);
            }
        }
    })
    .await
    .expect("run reaches its terminal event within the wait bound");

    assert_eq!(
        control.iter().filter(|event| event.is_terminal()).count(),
        1,
        "exactly one terminal event is delivered"
    );
    assert!(
        control.last().is_some_and(RunEvent::is_terminal),
        "the terminal event is last"
    );
    assert!(
        control.windows(2).all(|pair| pair[0].seq() < pair[1].seq()),
        "control sequences are strictly increasing"
    );
    assert!(
        control.iter().all(|event| event.run() == &run),
        "every delivered event belongs to the submitted run"
    );
    (control, finished)
}

fn usage_updates(control: &[RunEvent]) -> Vec<Usage> {
    control
        .iter()
        .filter_map(|event| match event.payload() {
            EventPayload::UsageUpdated(usage) => Some(*usage),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn provisional_then_final_stream_publishes_each_distinct_update_once() {
    let early = provisional(Some(5), Some(3));
    let latest = provisional(Some(10), Some(8));
    let committed = final_usage(Some(10), Some(8));
    let (control, finished) = run_script(
        "provisional-final",
        vec![vec![
            ProviderEvent::Usage(early),
            ProviderEvent::Usage(early),
            ProviderEvent::Usage(latest),
            ProviderEvent::Usage(committed),
            stop_turn(committed),
        ]],
    )
    .await;

    assert_eq!(finished.outcome(), RunOutcome::Completed);
    assert_eq!(
        usage_updates(&control),
        vec![early, latest, committed],
        "each distinct update is published once, in order, a repeated provisional is deduplicated, and the terminal merge equals the last final record"
    );
}

#[tokio::test]
async fn provisional_counters_are_never_promoted_when_the_terminal_is_unknown() {
    let seen = provisional(Some(10), Some(8));
    let unknown = final_usage(None, None);
    let (control, finished) = run_script(
        "provisional-not-promoted",
        vec![vec![ProviderEvent::Usage(seen), stop_turn(unknown)]],
    )
    .await;

    assert_eq!(finished.outcome(), RunOutcome::Completed);
    let updates = usage_updates(&control);
    assert_eq!(
        updates,
        vec![seen, unknown],
        "the terminal reports its own unknown counters; provisional values are never promoted"
    );
    let terminal = updates.last().expect("terminal usage is published");
    assert_eq!(terminal.input_tokens(), None);
    assert_eq!(terminal.output_tokens(), None);
    assert_eq!(terminal.finality(), UsageFinality::Final);
}

#[tokio::test]
async fn provisional_counters_are_never_merged_into_a_final_terminal() {
    let seen = provisional(Some(20), Some(10));
    let committed = final_usage(Some(10), Some(8));
    let (control, finished) = run_script(
        "provisional-superseded",
        vec![vec![ProviderEvent::Usage(seen), stop_turn(committed)]],
    )
    .await;

    assert_eq!(finished.outcome(), RunOutcome::Completed);
    assert_eq!(
        usage_updates(&control),
        vec![seen, committed],
        "the final terminal supersedes the provisional estimate; no counter is mixed across finality"
    );
}

#[tokio::test]
async fn terminal_merge_retains_a_known_counter_the_terminal_omits() {
    let observed = final_usage(Some(10), Some(8));
    let (control, finished) = run_script(
        "merge-omitted-output",
        vec![vec![
            ProviderEvent::Usage(observed),
            stop_turn(final_usage(Some(10), None)),
        ]],
    )
    .await;

    assert_eq!(finished.outcome(), RunOutcome::Completed);
    let updates = usage_updates(&control);
    assert_eq!(
        updates,
        vec![observed],
        "the merged terminal equals the last final record, so no duplicate update is published"
    );
    assert_eq!(updates[0].input_tokens(), Some(10));
    assert_eq!(
        updates[0].output_tokens(),
        Some(8),
        "the omitted terminal counter retains the last final value instead of downgrading to unknown"
    );
    assert_eq!(updates[0].finality(), UsageFinality::Final);
}

#[tokio::test]
async fn terminal_merge_completes_each_counter_from_its_own_final_source() {
    let merged = final_usage(Some(10), Some(8));

    let input_only = final_usage(Some(10), None);
    let (control, finished) = run_script(
        "merge-per-counter",
        vec![vec![
            ProviderEvent::Usage(input_only),
            stop_turn(final_usage(None, Some(8))),
        ]],
    )
    .await;
    assert_eq!(finished.outcome(), RunOutcome::Completed);
    assert_eq!(
        usage_updates(&control),
        vec![input_only, merged],
        "input is retained from the last final record while output comes from the terminal"
    );

    let output_only = final_usage(None, Some(8));
    let (control, finished) = run_script(
        "merge-per-counter-reverse",
        vec![vec![
            ProviderEvent::Usage(output_only),
            stop_turn(final_usage(Some(10), None)),
        ]],
    )
    .await;
    assert_eq!(finished.outcome(), RunOutcome::Completed);
    assert_eq!(
        usage_updates(&control),
        vec![output_only, merged],
        "output is retained from the last final record while input comes from the terminal"
    );
}

#[tokio::test]
async fn unknown_counters_stay_unknown_and_never_become_zero() {
    let bare = final_usage(None, None);
    let (control, finished) = run_script("unknown-bare", vec![vec![stop_turn(bare)]]).await;
    assert_eq!(finished.outcome(), RunOutcome::Completed);
    assert_eq!(
        usage_updates(&control),
        vec![bare],
        "a terminal that reports nothing publishes unknown counters, never fabricated zero"
    );

    let provisional_unknown = provisional(None, None);
    let (control, finished) = run_script(
        "unknown-provisional",
        vec![vec![
            ProviderEvent::Usage(provisional_unknown),
            stop_turn(bare),
        ]],
    )
    .await;
    assert_eq!(finished.outcome(), RunOutcome::Completed);
    let updates = usage_updates(&control);
    assert_eq!(updates, vec![provisional_unknown, bare]);
    for update in updates {
        assert_eq!(update.input_tokens(), None, "unknown input stays unknown");
        assert_eq!(update.output_tokens(), None, "unknown output stays unknown");
    }
}

#[tokio::test]
async fn explicit_provider_zero_stays_a_known_zero() {
    let input_only = final_usage(Some(10), None);
    let zero_output = final_usage(Some(10), Some(0));
    let (control, finished) = run_script(
        "known-zero",
        vec![vec![
            ProviderEvent::Usage(input_only),
            stop_turn(zero_output),
        ]],
    )
    .await;

    assert_eq!(finished.outcome(), RunOutcome::Completed);
    let updates = usage_updates(&control);
    assert_eq!(updates, vec![input_only, zero_output]);
    let terminal = updates.last().expect("terminal usage is published");
    assert_eq!(terminal.input_tokens(), Some(10));
    assert_eq!(
        terminal.output_tokens(),
        Some(0),
        "an explicit provider zero stays a known zero, never coerced to unknown"
    );
}

#[tokio::test]
async fn provider_failure_after_usage_order_violation_keeps_provider_error() {
    let committed = final_usage(Some(10), Some(8));
    let late = provisional(Some(11), Some(9));
    let (control, finished) = run_script(
        "failed-order-violation",
        vec![vec![
            ProviderEvent::Usage(committed),
            ProviderEvent::Usage(late),
            failed(ErrorCategory::Timeout, "provider timed out"),
        ]],
    )
    .await;

    assert_eq!(finished.outcome(), RunOutcome::Failed);
    let error = finished
        .error()
        .expect("typed provider failure is retained");
    assert_eq!(
        error.category(),
        ErrorCategory::Timeout,
        "the provider's typed failure stays primary over the protocol ordering diagnostic"
    );
    assert_eq!(
        error.message(),
        "provider timed out",
        "the provider's message is preserved verbatim"
    );
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert!(
        usage_updates(&control).is_empty(),
        "a batch rejected for ordering publishes no partial usage"
    );
}

#[tokio::test]
async fn failure_event_cannot_mask_invalid_terminal_ordering() {
    for events in [
        vec![
            failed(ErrorCategory::Timeout, "provider timed out"),
            stop_turn(final_usage(None, None)),
        ],
        vec![
            stop_turn(final_usage(None, None)),
            failed(ErrorCategory::Timeout, "provider timed out"),
        ],
    ] {
        let (_, finished) = run_script("invalid-terminal-order", vec![events]).await;
        assert_eq!(finished.outcome(), RunOutcome::Failed);
        assert_eq!(
            finished.error().unwrap().category(),
            ErrorCategory::Protocol
        );
    }
}

#[tokio::test]
async fn cancelled_provider_failure_after_usage_order_violation_stays_cancelled() {
    let committed = final_usage(Some(10), Some(8));
    let late = provisional(Some(11), Some(9));
    let (control, finished) = run_script(
        "cancelled-order-violation",
        vec![vec![
            ProviderEvent::Usage(committed),
            ProviderEvent::Usage(late),
            failed(ErrorCategory::Cancelled, "provider cancelled mid-stream"),
        ]],
    )
    .await;

    assert_eq!(finished.outcome(), RunOutcome::Cancelled);
    let error = finished
        .error()
        .expect("cancelled provider failure is retained");
    assert_eq!(error.category(), ErrorCategory::Cancelled);
    assert_eq!(error.message(), "provider cancelled mid-stream");
    assert!(usage_updates(&control).is_empty());
}

#[tokio::test]
async fn valid_provider_failure_keeps_error_and_publishes_usage_seen_so_far() {
    let seen = provisional(Some(10), Some(8));
    let (control, finished) = run_script(
        "failed-with-usage",
        vec![vec![
            ProviderEvent::Usage(seen),
            failed(ErrorCategory::Timeout, "provider timed out"),
        ]],
    )
    .await;

    assert_eq!(finished.outcome(), RunOutcome::Failed);
    assert_eq!(
        finished
            .error()
            .expect("typed provider failure is retained")
            .category(),
        ErrorCategory::Timeout
    );
    assert_eq!(
        usage_updates(&control),
        vec![seen],
        "usage reported before an honest failure is published; only a rejected batch withholds it"
    );
}

#[tokio::test]
async fn usage_order_violation_without_provider_failure_is_a_protocol_failure() {
    let committed = final_usage(Some(10), Some(8));
    let late = provisional(Some(11), Some(9));
    let (control, finished) = run_script(
        "protocol-order-violation",
        vec![vec![
            ProviderEvent::Usage(committed),
            ProviderEvent::Usage(late),
            stop_turn(committed),
        ]],
    )
    .await;

    assert_eq!(finished.outcome(), RunOutcome::Failed);
    assert_eq!(
        finished
            .error()
            .expect("protocol failure is retained")
            .category(),
        ErrorCategory::Protocol,
        "a usage update after a final record is rejected before publication"
    );
    assert!(usage_updates(&control).is_empty());
}
