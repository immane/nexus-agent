#![forbid(unsafe_code)]

//! Review regressions: bounded event transport under saturation and drops.
//!
//! Findings under test:
//! - provisional usage publication is bounded, while the required terminal
//!   and final usage still arrive after the consumer resumes draining;
//! - data-channel drops must not leave holes in the delivered sequence
//!   numbers: published events stay contiguous even when presentation
//!   traffic is truncated, and the truncation stays visible in snapshots.

mod common;

use std::time::Duration;

use nexus_core::{
    CommandReply, EventPayload, FinishReason, GetSnapshotCommand, Limits, ProviderEvent, RequestId,
    RunLifecycle, RunOutcome, TurnFinished, Usage, UsageFinality,
};
use nexus_runtime::{CONTROL_CAPACITY, DATA_CAPACITY};

fn stop_terminal() -> ProviderEvent {
    ProviderEvent::TurnFinished(TurnFinished::new(
        FinishReason::Stop,
        Usage::new(None, None, UsageFinality::Final),
        None,
    ))
}

/// Sends more provisional updates than the runtime's bounded publication
/// budget without a consumer, then resumes draining. The bounded prefix,
/// final usage, and terminal must arrive, and exactly one terminal closes
/// the run.
#[test]
fn provisional_usage_burst_is_bounded_and_terminal_survives_resume() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut turn = Vec::with_capacity(201);
        for index in 0..200u64 {
            // Distinct counters defeat coalescing so the channel genuinely
            // exceeds its capacity.
            turn.push(ProviderEvent::Usage(Usage::new(
                Some(index + 1),
                Some(index + 1),
                UsageFinality::Provisional,
            )));
        }
        turn.push(stop_terminal());
        let mut bed = common::make_bed(vec![turn], common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("control-saturation"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        // Deliberately do not drain while usage publication is bounded.
        tokio::time::sleep(Duration::from_millis(50)).await;
        // This workload completes while the receiver is paused. Its exact
        // terminal-inclusive queue length is therefore also its whole-run peak.
        let control_receiver_peak = bed.control.len();
        assert_eq!(control_receiver_peak, 21);

        // Consumer resumes. The terminal is required and must arrive.
        let (control, finished) = tokio::time::timeout(Duration::from_secs(2), async {
            let mut controls = Vec::new();
            loop {
                match bed.control.recv().await {
                    Some(event) => {
                        let terminal = match event.payload() {
                            nexus_core::EventPayload::RunFinished(finished) => {
                                Some(finished.clone())
                            }
                            _ => None,
                        };
                        controls.push(event);
                        if let Some(finished) = terminal {
                            break (controls, finished);
                        }
                    }
                    None => panic!("control channel closed before terminal"),
                }
            }
        })
        .await
        .expect("required terminal event was not delivered");

        assert_eq!(
            finished.outcome(),
            RunOutcome::Completed,
            "bounded usage publication does not corrupt the run outcome"
        );
        // Exact composition: RunStarted, the bounded provisional updates,
        // final usage (unknown counters, not fabricated zeros), and RunFinished.
        let usages: Vec<Usage> = control
            .iter()
            .filter_map(|event| match event.payload() {
                EventPayload::UsageUpdated(usage) => Some(*usage),
                _ => None,
            })
            .collect();
        assert_eq!(
            usages.len(),
            17,
            "16 bounded provisional updates plus terminal final usage"
        );
        let provisional: Vec<u64> = usages
            .iter()
            .filter(|usage| usage.finality() == UsageFinality::Provisional)
            .filter_map(|usage| usage.input_tokens())
            .collect();
        assert_eq!(
            provisional,
            (1..=16).collect::<Vec<u64>>(),
            "the bounded provisional prefix is delivered in order"
        );
        let terminal_usage = usages.last().expect("usage events observed");
        assert_eq!(
            terminal_usage.finality(),
            UsageFinality::Final,
            "the last usage record is the terminal final usage"
        );
        assert_eq!(
            (
                terminal_usage.input_tokens(),
                terminal_usage.output_tokens()
            ),
            (None, None),
            "missing terminal counters stay unknown, never zero"
        );
        assert_eq!(
            control.len(),
            control_receiver_peak,
            "terminal-inclusive drain matches the paused receiver peak"
        );
        assert_eq!(
            common::terminal_count(&[], &control),
            1,
            "exactly one terminal after the consumer resumes"
        );
        assert!(
            control.last().expect("events").is_terminal(),
            "the terminal is the last delivered control event"
        );

        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-control-saturation-snap").expect("valid"),
                run,
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        assert_eq!(
            snapshot.expect("finalized snapshot").lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed)
        );
    });
}

/// A full control receiver retains a multi-turn run's later control events in
/// the runtime outbox. The terminal remains available after the consumer
/// resumes, and the finalized snapshot reports no invented truncation.
#[test]
fn paused_control_receiver_reaches_capacity_over_the_complete_run() {
    const UPDATES_PER_TURN: u64 = 16;
    let turns = Limits::M0_TEST_MODEL_TURNS_PER_RUN as usize;
    let script = (0..turns)
        .map(|turn| {
            let mut events = Vec::with_capacity(UPDATES_PER_TURN as usize + 1);
            for update in 0..UPDATES_PER_TURN {
                let count = (turn as u64) * UPDATES_PER_TURN + update + 1;
                events.push(ProviderEvent::Usage(Usage::new(
                    Some(count),
                    Some(count),
                    UsageFinality::Provisional,
                )));
            }
            let reason = if turn + 1 == turns {
                FinishReason::Stop
            } else {
                FinishReason::ToolCalls
            };
            if turn + 1 < turns {
                events.push(ProviderEvent::ToolCallReady(common::candidate_with(
                    &format!("item-{turn}"),
                    &format!("prov-ref-{turn}"),
                    "host_read",
                    r#"{"path":"src"}"#,
                )));
            }
            events.push(ProviderEvent::TurnFinished(TurnFinished::new(
                reason,
                Usage::new(None, None, UsageFinality::Final),
                None,
            )));
            events
        })
        .collect();
    let rt = common::test_rt();
    rt.block_on(async {
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed.runtime.submit(common::submit_cmd("control-full-run-peak")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let snapshot = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let (reply, snapshot) = bed.runtime.get_snapshot(GetSnapshotCommand {
                    request: RequestId::new("req-control-full-run-snapshot").unwrap(),
                    run: run.clone(),
                }).await;
                assert_eq!(reply.reply(), CommandReply::Accepted);
                if let Some(snapshot) = snapshot
                    && snapshot.lifecycle() != RunLifecycle::Active
                {
                    break snapshot;
                }
                tokio::task::yield_now().await;
            }
        }).await.expect("full run finalizes with receiver paused");

        let control_peak = bed.control.len();
        assert_eq!(control_peak, CONTROL_CAPACITY);
        assert!(!snapshot.is_content_truncated());
        let (_, control, finished) = common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert!(control.len() > control_peak, "later control events were retained for delivery");
        assert!(control.last().expect("terminal event").is_terminal());
        let usage_events = control.iter().filter(|event| matches!(event.payload(), EventPayload::UsageUpdated(_))).count();
        assert_eq!(usage_events, turns * (UPDATES_PER_TURN as usize + 1));
        println!("paused_control_receiver_peak_whole_run={control_peak}; drained_control_events={}; usage_events={usage_events}; outbox_retained_events={}", control.len(), control.len() - control_peak);
    });
}

/// A 1,300-fragment alternating-item output flood overflows the 1,024-event
/// data channel. Presentation drops are allowed and visible, but the
/// delivered per-run sequence numbers must remain contiguous.
#[test]
fn delivered_sequences_stay_contiguous_under_data_channel_drops() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut turn = Vec::with_capacity(1_301);
        for index in 0..1_300 {
            turn.push(ProviderEvent::TextDelta {
                item_key: format!("item-{}", index % 2),
                text: "x".repeat(64),
            });
        }
        turn.push(stop_terminal());
        let mut bed = common::make_bed(vec![turn], common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("sequence-drops"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert!(
            !data.is_empty(),
            "some presentation traffic survives the flood"
        );
        assert!(
            data.len() <= DATA_CAPACITY,
            "delivered data stays within the locked channel capacity: {}",
            data.len()
        );
        // The locked contract: sequence numbers are assigned after batching
        // so published events remain contiguous. Drops may not leave holes
        // that consumers would read as a lost or reordered event.
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);

        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-sequence-drops-snap").expect("valid"),
                run,
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        assert!(
            snapshot.expect("finalized snapshot").is_content_truncated(),
            "dropped presentation content is reported, not hidden"
        );
    });
}
