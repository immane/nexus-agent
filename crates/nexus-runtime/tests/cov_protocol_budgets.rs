#![forbid(unsafe_code)]

//! Integration coverage for the provider-batch finite budgets.
//!
//! `nexus_runtime::protocol::validate_batch` is crate-private, so the only
//! public surface that reaches it is the real [`Runtime`]: every scripted
//! provider batch is validated in the model-ingest step before any admission,
//! and a budget failure becomes the typed `RunFinished` error. These probes
//! exercise the effective-budget boundaries end to end: aggregate payload
//! bytes, aggregate text bytes, argument assembly, event count, terminal
//! continuation bytes, and item-key length.
//!
//! Unit-only gaps that this file cannot observe through the runtime:
//! - a `Failed` terminal masks the protocol diagnostic by design (the
//!   provider error stays primary), so the payload contribution of a failed
//!   terminal's message and correlation stays covered by `src/protocol.rs`;
//! - the internal `event_payload_bytes` sum is asserted by construction
//!   (exact acceptance at the bound, rejection one byte over), not read
//!   directly;
//! - malformed shape, duplicate identities, ordering, and usage-agreement
//!   checks remain unit-only in `src/protocol.rs`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nexus_core::{
    CallCandidate, CommandReply, ContinuationData, DEFAULT_ADAPTER_IDENTITY, ErrorCategory,
    EventPayload, ExecutionStatus, FinishReason, Limits, MAX_CONTINUATION_BYTES, MAX_ITEM_KEY_LEN,
    ModelRequest, ProviderCapabilities, ProviderContext, ProviderEvent, ProviderPort, RequestId,
    RunEvent, RunFinished, RunOutcome, SessionId, SubmitCommand, TurnFinished, Usage,
    UsageFinality,
};
use nexus_runtime::protocol::{MAX_BATCH_BYTES, MAX_BATCH_EVENTS};
use nexus_runtime::{Policy, Runtime, RuntimeConfig};

/// Deterministic provider double: pops one scripted batch per invocation and
/// falls back to a fixed batch once the script is exhausted. A validation
/// failure consumes exactly one invocation; a validation pass that continues
/// the model loop consumes the fallback turn, so the invocation count
/// distinguishes the two.
struct ScriptedProvider {
    batches: Mutex<VecDeque<Vec<ProviderEvent>>>,
    calls: AtomicUsize,
    fallback: Vec<ProviderEvent>,
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
        let scripted = self
            .batches
            .lock()
            .expect("script queue is readable")
            .pop_front();
        scripted.unwrap_or_else(|| self.fallback.clone())
    }
}

/// Live runtime plus its two event receivers and the inspectable provider.
struct Bed {
    runtime: Runtime,
    data: tokio::sync::mpsc::Receiver<RunEvent>,
    control: tokio::sync::mpsc::Receiver<RunEvent>,
}

/// Everything one settled run exposes to a budget assertion.
struct Probe {
    finished: RunFinished,
    calls: usize,
    data: Vec<RunEvent>,
    control: Vec<RunEvent>,
}

fn test_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("coverage test runtime builds")
}

fn make_bed(
    limits: Limits,
    script: Vec<Vec<ProviderEvent>>,
    fallback: Vec<ProviderEvent>,
) -> (Bed, Arc<ScriptedProvider>) {
    let provider = Arc::new(ScriptedProvider {
        batches: Mutex::new(script.into()),
        calls: AtomicUsize::new(0),
        fallback,
    });
    let config = RuntimeConfig {
        limits,
        policy: Policy::m0_test(),
        has_approval_handler: false,
    };
    let (runtime, streams) = Runtime::try_new(config, provider.clone(), Vec::new())
        .expect("coverage runtime wiring is valid");
    (
        Bed {
            runtime,
            data: streams.data,
            control: streams.control,
        },
        provider,
    )
}

/// Submits one run, drains both channels through the terminal event, and
/// collects every still-buffered event after it.
async fn submit_and_settle(bed: &mut Bed, provider: &ScriptedProvider, tag: &str) -> Probe {
    let command = SubmitCommand::new(
        RequestId::new(format!("req-{tag}")).expect("request id is valid"),
        SessionId::new("sess-protocol-budgets").expect("session id is valid"),
        "coverage probe",
        "m0-test",
    )
    .expect("submit command is valid");
    let response = bed.runtime.submit(command).await;
    assert_eq!(
        response.reply(),
        CommandReply::Accepted,
        "{tag}: submit is accepted"
    );

    let mut data = Vec::new();
    let mut control = Vec::new();
    let finished = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            tokio::select! {
                event = bed.data.recv() => match event {
                    Some(event) => data.push(event),
                    None => panic!("{tag}: data channel closed before the terminal event"),
                },
                event = bed.control.recv() => match event {
                    Some(event) => {
                        let terminal = match event.payload() {
                            EventPayload::RunFinished(finished) => Some(finished.clone()),
                            _ => None,
                        };
                        control.push(event);
                        if let Some(finished) = terminal {
                            break finished;
                        }
                    }
                    None => panic!("{tag}: control channel closed before the terminal event"),
                },
            }
        }
    })
    .await
    .expect("run settles promptly");

    // The terminal is published last, but a scheduled outbox flusher could
    // still race a single non-blocking pass; drain to two quiet passes.
    let mut quiet = 0;
    let mut rounds = 0;
    while quiet < 2 && rounds < 64 {
        rounds += 1;
        let mut drained = false;
        while let Ok(event) = bed.data.try_recv() {
            data.push(event);
            drained = true;
        }
        while let Ok(event) = bed.control.try_recv() {
            control.push(event);
            drained = true;
        }
        if drained {
            quiet = 0;
        } else {
            quiet += 1;
            tokio::task::yield_now().await;
        }
    }

    Probe {
        finished,
        calls: provider.calls.load(Ordering::SeqCst),
        data,
        control,
    }
}

fn assert_resource_limit_failure(probe: &Probe, message: &'static str) {
    assert_eq!(
        probe.finished.outcome(),
        RunOutcome::Failed,
        "budget exhaustion is a failed run"
    );
    let error = probe
        .finished
        .error()
        .expect("failed run retains its typed error");
    assert_eq!(
        error.category(),
        ErrorCategory::ResourceLimit,
        "message: {}",
        error.message()
    );
    assert_eq!(
        error.message(),
        message,
        "the exhausted budget is identified exactly"
    );
    assert_eq!(probe.calls, 1, "a rejected batch is never retried");
}

fn assert_invalid_input_failure(probe: &Probe, message: &'static str) {
    assert_eq!(
        probe.finished.outcome(),
        RunOutcome::Failed,
        "an invalid bounded identity is a failed run"
    );
    let error = probe
        .finished
        .error()
        .expect("failed run retains its typed error");
    assert_eq!(
        error.category(),
        ErrorCategory::InvalidInput,
        "message: {}",
        error.message()
    );
    assert_eq!(
        error.message(),
        message,
        "the invalid identity is identified exactly"
    );
    assert_eq!(probe.calls, 1, "a rejected batch is never retried");
}

fn final_usage() -> Usage {
    Usage::new(None, None, UsageFinality::Final)
}

fn stop() -> ProviderEvent {
    ProviderEvent::TurnFinished(TurnFinished::new(FinishReason::Stop, final_usage(), None))
}

fn stop_with_continuation(continuation: ContinuationData) -> ProviderEvent {
    ProviderEvent::TurnFinished(TurnFinished::new(
        FinishReason::Stop,
        final_usage(),
        Some(continuation),
    ))
}

fn tool_calls_terminal() -> ProviderEvent {
    ProviderEvent::TurnFinished(TurnFinished::new(
        FinishReason::ToolCalls,
        final_usage(),
        None,
    ))
}

fn text(item_key: &str, body: &str) -> ProviderEvent {
    ProviderEvent::TextDelta {
        item_key: item_key.to_owned(),
        text: body.to_owned(),
    }
}

fn delta(item_key: &str, assembled_bytes: usize) -> ProviderEvent {
    ProviderEvent::ToolCallDelta {
        item_key: item_key.to_owned(),
        assembled_bytes,
    }
}

fn ready(item_key: &str, provider_ref: &str, arguments_json: &str) -> ProviderEvent {
    ProviderEvent::ToolCallReady(
        CallCandidate::new(item_key, provider_ref, "host_read", arguments_json)
            .expect("coverage candidate is bounded"),
    )
}

fn limits_with_output(max_tool_output_bytes: usize) -> Limits {
    let mut limits = Limits::m0_test();
    limits.max_tool_output_bytes = max_tool_output_bytes;
    limits
}

fn event_count_batch(text_events: usize) -> Vec<ProviderEvent> {
    let mut events: Vec<ProviderEvent> = (0..text_events)
        .map(|index| text(&format!("k{index}"), "x"))
        .collect();
    events.push(stop());
    events
}

#[test]
fn batch_payload_budget_boundary_is_exact() {
    let runtime = test_runtime();
    runtime.block_on(async {
        let limits = limits_with_output(MAX_BATCH_BYTES);

        // Payload is exactly `item_key.len() + text.len()` == MAX_BATCH_BYTES.
        let exact_text = "x".repeat(MAX_BATCH_BYTES - "item-0".len());
        let (mut bed, provider) = make_bed(
            limits,
            vec![vec![text("item-0", &exact_text), stop()]],
            vec![stop()],
        );
        let probe = submit_and_settle(&mut bed, &provider, "payload-exact").await;
        assert_eq!(
            probe.finished.outcome(),
            RunOutcome::Completed,
            "a payload exactly at MAX_BATCH_BYTES is accepted"
        );
        assert!(
            probe.finished.error().is_none(),
            "accepted batch has no error"
        );

        // One more text byte crosses the same payload bound; the text budget
        // is set above the payload bound so only the payload check can fire.
        let over_text = "x".repeat(MAX_BATCH_BYTES - "item-0".len() + 1);
        let (mut bed, provider) = make_bed(
            limits,
            vec![vec![text("item-0", &over_text), stop()]],
            vec![stop()],
        );
        let probe = submit_and_settle(&mut bed, &provider, "payload-over").await;
        assert_resource_limit_failure(&probe, "provider batch exceeds the payload byte budget");
    });
}

#[test]
fn aggregate_text_budget_is_the_effective_configuration() {
    let runtime = test_runtime();
    runtime.block_on(async {
        let mut limits = Limits::m0_test();
        limits.max_tool_output_bytes = 8;

        // Exactly the effective budget, aggregated across two items, passes.
        let (mut bed, provider) = make_bed(
            limits,
            vec![vec![text("item-0", "1234"), text("item-1", "5678"), stop()]],
            vec![stop()],
        );
        let probe = submit_and_settle(&mut bed, &provider, "text-exact").await;
        assert_eq!(
            probe.finished.outcome(),
            RunOutcome::Completed,
            "8 aggregate text bytes pass the effective budget"
        );

        // One byte over the effective budget fails even though the M0
        // constant is far larger.
        let (mut bed, provider) = make_bed(
            limits,
            vec![vec![
                text("item-0", "1234"),
                text("item-1", "56789"),
                stop(),
            ]],
            vec![stop()],
        );
        let probe = submit_and_settle(&mut bed, &provider, "text-over").await;
        assert_resource_limit_failure(&probe, "provider text exceeds the output byte budget");

        // The budget is aggregate per batch, not per fragment: two fragments
        // of one item sum into the same bound.
        let (mut bed, provider) = make_bed(
            limits,
            vec![vec![
                text("item-0", "1234"),
                text("item-0", "56789"),
                stop(),
            ]],
            vec![stop()],
        );
        let probe = submit_and_settle(&mut bed, &provider, "text-over-same-item").await;
        assert_resource_limit_failure(&probe, "provider text exceeds the output byte budget");
    });
}

#[test]
fn assembly_budget_is_the_effective_configuration() {
    let runtime = test_runtime();
    runtime.block_on(async {
        let mut limits = Limits::m0_test();
        limits.max_arg_assembly_bytes = 4;

        // Advertised progress above the effective bound is rejected even
        // though the M0 constant admits it.
        let over_progress = vec![
            delta("item-1", 5),
            ready("item-1", "prov-ref-1", r#"{"path":"a"}"#),
            tool_calls_terminal(),
        ];
        let (mut bed, provider) = make_bed(limits, vec![over_progress], vec![stop()]);
        let probe = submit_and_settle(&mut bed, &provider, "assembly-progress-over").await;
        assert_resource_limit_failure(
            &probe,
            "tool argument progress exceeds the assembly byte budget",
        );

        // A complete candidate whose argument text exceeds the effective
        // bound is rejected at the same check.
        let over_candidate = vec![
            ready("item-1", "prov-ref-1", r#"{"path":"a"}"#),
            tool_calls_terminal(),
        ];
        let (mut bed, provider) = make_bed(limits, vec![over_candidate], vec![stop()]);
        let probe = submit_and_settle(&mut bed, &provider, "assembly-candidate-over").await;
        assert_resource_limit_failure(
            &probe,
            "tool call candidate exceeds the assembly byte budget",
        );

        // Exactly at the effective bound both the progress and the candidate
        // pass validation, so the runtime proceeds past the batch; the
        // unregistered candidate is denied on the next lifecycle step and the
        // fallback turn completes the run.
        let boundary = vec![
            delta("item-1", 4),
            ready("item-1", "prov-ref-1", "1234"),
            tool_calls_terminal(),
        ];
        let (mut bed, provider) = make_bed(limits, vec![boundary], vec![stop()]);
        let probe = submit_and_settle(&mut bed, &provider, "assembly-boundary").await;
        assert_eq!(
            probe.finished.outcome(),
            RunOutcome::Completed,
            "an assembly exactly at the effective bound passes validation"
        );
        assert_eq!(
            probe.calls, 2,
            "validation passed and the model loop continued to the fallback turn"
        );
        let denied: Vec<ExecutionStatus> = probe
            .control
            .iter()
            .filter_map(|event| match event.payload() {
                EventPayload::ToolFinished(info) => Some(info.outcome.status()),
                _ => None,
            })
            .collect();
        assert_eq!(
            denied,
            vec![ExecutionStatus::Denied],
            "the admitted candidate is denied because no tool is registered"
        );
    });
}

#[test]
fn event_count_budget_boundary_is_exact() {
    let runtime = test_runtime();
    runtime.block_on(async {
        let at_bound = event_count_batch(MAX_BATCH_EVENTS - 1);
        assert_eq!(at_bound.len(), MAX_BATCH_EVENTS);
        let (mut bed, provider) = make_bed(Limits::m0_test(), vec![at_bound], vec![stop()]);
        let probe = submit_and_settle(&mut bed, &provider, "events-exact").await;
        assert_eq!(
            probe.finished.outcome(),
            RunOutcome::Completed,
            "exactly MAX_BATCH_EVENTS events (terminal included) are accepted"
        );

        let over_bound = event_count_batch(MAX_BATCH_EVENTS);
        assert_eq!(over_bound.len(), MAX_BATCH_EVENTS + 1);
        let (mut bed, provider) = make_bed(Limits::m0_test(), vec![over_bound], vec![stop()]);
        let probe = submit_and_settle(&mut bed, &provider, "events-over").await;
        assert_resource_limit_failure(&probe, "provider batch exceeds the event-count budget");
    });
}

#[test]
fn terminal_continuation_counts_against_the_payload_budget() {
    let runtime = test_runtime();
    runtime.block_on(async {
        let limits = limits_with_output(MAX_BATCH_BYTES);
        let continuation = ContinuationData::new(
            DEFAULT_ADAPTER_IDENTITY,
            "m0-test",
            vec![0u8; MAX_CONTINUATION_BYTES],
        )
        .expect("bounded continuation builds");
        let overhead = DEFAULT_ADAPTER_IDENTITY.len() + "m0-test".len() + MAX_CONTINUATION_BYTES;

        // Payload with the continuation is exactly MAX_BATCH_BYTES: the
        // adapter label, scope label, and opaque bytes all count. The
        // continuation is compatible with the default provider identity and
        // profile scope, so the run completes.
        let exact_text = "x".repeat(MAX_BATCH_BYTES - "item-0".len() - overhead);
        let (mut bed, provider) = make_bed(
            limits,
            vec![vec![
                text("item-0", &exact_text),
                stop_with_continuation(continuation.clone()),
            ]],
            vec![stop()],
        );
        let probe = submit_and_settle(&mut bed, &provider, "continuation-exact").await;
        assert_eq!(
            probe.finished.outcome(),
            RunOutcome::Completed,
            "continuation bytes exactly at the payload bound are accepted"
        );

        // One more text byte crosses the payload bound, proving the
        // continuation bytes were summed into it.
        let over_text = "x".repeat(MAX_BATCH_BYTES - "item-0".len() - overhead + 1);
        let (mut bed, provider) = make_bed(
            limits,
            vec![vec![
                text("item-0", &over_text),
                stop_with_continuation(continuation),
            ]],
            vec![stop()],
        );
        let probe = submit_and_settle(&mut bed, &provider, "continuation-over").await;
        assert_resource_limit_failure(&probe, "provider batch exceeds the payload byte budget");

        // Control: the same text without a continuation stays under the
        // payload bound and completes; only the continuation pushed it over.
        let (mut bed, provider) = make_bed(
            limits,
            vec![vec![text("item-0", &over_text), stop()]],
            vec![stop()],
        );
        let probe = submit_and_settle(&mut bed, &provider, "continuation-control").await;
        assert_eq!(
            probe.finished.outcome(),
            RunOutcome::Completed,
            "without continuation the same text fits the payload bound"
        );
    });
}

#[test]
fn item_key_budget_boundary_is_128_of_129() {
    let runtime = test_runtime();
    runtime.block_on(async {
        let exact = "k".repeat(MAX_ITEM_KEY_LEN);
        let (mut bed, provider) = make_bed(
            Limits::m0_test(),
            vec![vec![text(&exact, "x"), stop()]],
            vec![stop()],
        );
        let probe = submit_and_settle(&mut bed, &provider, "item-key-exact").await;
        assert_eq!(
            probe.finished.outcome(),
            RunOutcome::Completed,
            "a 128-byte item key is accepted"
        );
        let emitted = probe.data.iter().find_map(|event| match event.payload() {
            EventPayload::AssistantTextDelta(fragment) => Some(fragment.item_key.clone()),
            _ => None,
        });
        assert_eq!(
            emitted.as_deref(),
            Some(exact.as_str()),
            "the 128-byte key survives to publication"
        );

        // One byte over is rejected identically for text and progress keys.
        let over = "k".repeat(MAX_ITEM_KEY_LEN + 1);
        for (tag, batch) in [
            ("item-key-text-over", vec![text(&over, "x"), stop()]),
            ("item-key-delta-over", vec![delta(&over, 0), stop()]),
        ] {
            let (mut bed, provider) = make_bed(Limits::m0_test(), vec![batch], vec![stop()]);
            let probe = submit_and_settle(&mut bed, &provider, tag).await;
            assert_invalid_input_failure(&probe, "provider item key is invalid");
        }
    });
}
