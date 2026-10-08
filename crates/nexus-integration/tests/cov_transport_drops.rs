#![forbid(unsafe_code)]

//! Hardened transport coverage: sequence integrity under data drops, bounded
//! usage publication, and truncation reporting — against the real runtime and
//! the real fakes.
//!
//! Every test keeps both event receivers alive for the whole run and drains
//! nothing until the runtime reports the run finalized through a snapshot.
//! That ordering is deterministic and load-bearing rather than timing-based:
//! the runtime publishes its terminal event last, so a finalized snapshot
//! proves every event of the run was already committed, and the channel
//! capacities were provably exceeded at that moment. Timeouts below are
//! failure backstops only; assertions are on recorded invariants (sequence
//! accounting, payload kinds, terminal outcomes), never on wall-clock timing.
//!
//! Runtime unit tests cover saturated control-outbox delivery; these
//! integration tests cover real provider/fake behavior. The gaps hardened
//! here are:
//! - a dropped presentation event must consume NO sequence number, proven by
//!   exact accounting (dropped count, contiguous terminal sequence) instead
//!   of the weaker "sequences look contiguous" bound;
//! - control records published *after* the data channel started dropping must
//!   still commit gap-free sequence numbers, which is the only place a hole
//!   could hide behind a silent drop;
//! - a full data channel must be reported as content truncation;
//! - bounded usage publication must never violate channel discipline
//!   (required payloads on
//!   control, presentation on data) as observed by the consumer;
//! - undelivered required events must be provably retained, not dropped: the
//!   runtime answers `Busy` for a new run while its control outbox still owns
//!   undelivered events, and accepts one once they are drained.

mod common;

use std::time::Duration;

use nexus_core::{
    CommandReply, EventPayload, FinishReason, GetSnapshotCommand, ProviderEvent, RequestId, RunId,
    RunLifecycle, RunOutcome, Snapshot, TurnFinished, Usage, UsageFinality,
};
use nexus_fakes::stop_turn;
use nexus_runtime::DATA_CAPACITY;

/// Presentation fragments in the data flood: above the locked 1,024-event data
/// bound, so the channel provably saturates while no consumer is attached.
const FRAGMENTS: usize = 1_200;
/// Provisional usage records in the usage burst, above the runtime's bounded
/// publication limit.
const USAGE_RECORDS: u64 = 600;
/// Interleaved fragment/usage pairs for the data-drop-plus-tool run.
const PAIRS: usize = 1_200;
/// Presentation fragments in the control-saturation run: a handful, kept far
/// below the data bound so data saturation is provably NOT part of that run.
const FEW_TEXTS: usize = 4;

/// Stop terminal with unknown (never zero) counters. Mirrors the shared
/// `stop_turn` tail without its extra text fragment, keeping exact data-event
/// counts attributable to the flood alone.
fn stop_terminal() -> ProviderEvent {
    ProviderEvent::TurnFinished(TurnFinished::new(
        FinishReason::Stop,
        Usage::new(None, None, UsageFinality::Final),
        None,
    ))
}

/// One distinct provisional usage record. Distinct counters defeat the
/// runtime's same-value usage coalescing, so every record is published and
/// commits its own sequence number.
fn provisional(counter: u64) -> ProviderEvent {
    ProviderEvent::Usage(Usage::new(
        Some(counter),
        Some(counter),
        UsageFinality::Provisional,
    ))
}

/// One text fragment under an alternating turn-local item key. Alternating
/// keys defeat same-item coalescing, so every fragment becomes its own data
/// event — or its own drop once the data channel is full.
fn fragment(index: usize) -> ProviderEvent {
    ProviderEvent::TextDelta {
        item_key: format!("item-{}", index % 2),
        text: "x".repeat(64),
    }
}

/// Tool-call terminal for the saturation-plus-tool run. The candidate uses
/// `item-2` because the flood alternates the text keys `item-0`/`item-1`, and
/// reusing either would be a protocol text/call key collision.
fn tool_calls_terminal() -> ProviderEvent {
    ProviderEvent::TurnFinished(TurnFinished::new(
        FinishReason::ToolCalls,
        Usage::new(None, None, UsageFinality::Final),
        None,
    ))
}

/// True when `payload` is presentation traffic, which may travel only on the
/// data channel and is the only payload class a full channel may drop.
fn is_presentation(payload: &EventPayload) -> bool {
    matches!(
        payload,
        EventPayload::AssistantTextDelta(_)
            | EventPayload::ToolCallPreview { .. }
            | EventPayload::ToolOutput(_)
    )
}

/// Asserts channel discipline as observed by the consumer: presentation
/// payloads travel only on data and required payloads only on control, even
/// when both channels are saturated.
fn assert_channel_discipline(data: &[nexus_core::RunEvent], control: &[nexus_core::RunEvent]) {
    for event in data {
        assert!(
            is_presentation(event.payload()),
            "presentation traffic belongs on the data channel: {event:?}"
        );
    }
    for event in control {
        assert!(
            !is_presentation(event.payload()),
            "required traffic must never ride the droppable data channel: {event:?}"
        );
    }
}

/// Polls the snapshot until the run is finalized.
///
/// Deterministic because the runtime publishes its terminal event before it
/// records the finished run: a finalized snapshot therefore proves the whole
/// run was committed while the caller still had drained nothing. The timeout
/// is a failure backstop, not a timing assertion.
async fn await_finalized(bed: &common::Bed, run: &RunId, tag: &str) -> Snapshot {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut poll = 0usize;
        loop {
            poll += 1;
            let (reply, snapshot) = bed
                .runtime
                .get_snapshot(GetSnapshotCommand {
                    request: RequestId::new(format!("req-{tag}-poll-{poll}")).expect("valid"),
                    run: run.clone(),
                })
                .await;
            assert_eq!(reply.reply(), CommandReply::Accepted);
            let snapshot = snapshot.expect("a known run keeps a snapshot");
            if matches!(snapshot.lifecycle(), RunLifecycle::Finalized(_)) {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("run finalizes while the consumer holds both event channels")
}

/// A presentation flood of 1,200 fragments overflows the 1,024-event data
/// channel while no consumer is attached, while a light control load commits
/// records on both sides of the drop window.
///
/// The locked accounting: exactly [`DATA_CAPACITY`] fragments are delivered,
/// `FRAGMENTS - DATA_CAPACITY` are dropped, and every dropped fragment
/// consumes NO sequence number. Proof: the terminal sequence equals the
/// delivered count minus one, so the sequence space is exactly the delivered
/// events — a drop that consumed a number would push the terminal past it —
/// and the control records published after the drop window continue the very
/// next sequence numbers, leaving no hole a consumer could misread as a lost
/// or reordered event. Saturation is reported in the snapshot.
#[test]
fn data_drops_consume_no_sequence_numbers() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut turn = Vec::with_capacity(FRAGMENTS + 3);
        turn.push(provisional(1));
        for index in 0..FRAGMENTS {
            turn.push(fragment(index));
        }
        turn.push(provisional(2));
        turn.push(stop_terminal());
        let mut bed = common::make_bed(vec![turn], common::quick_config());

        let response = bed.runtime.submit(common::submit_cmd("drop-seq")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let snapshot = await_finalized(&bed, &run, "drop-seq").await;
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed),
            "presentation drops never change the run outcome"
        );
        assert!(
            snapshot.is_content_truncated(),
            "a full data channel is reported as content truncation, not hidden"
        );

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);
        assert_channel_discipline(&data, &control);

        // Exact drop accounting: with no consumer attached, a bounded channel
        // admits exactly its capacity, so the shortfall is precisely the
        // number of presentation events the full channel refused.
        assert_eq!(
            data.len(),
            DATA_CAPACITY,
            "an undrained data channel admits exactly its locked bound"
        );
        assert!(
            data.iter()
                .all(|event| matches!(event.payload(), EventPayload::AssistantTextDelta(_))),
            "every delivered data event is one flooded text fragment"
        );
        let dropped = FRAGMENTS - data.len();
        assert_eq!(
            dropped,
            FRAGMENTS - DATA_CAPACITY,
            "the refused fragments are counted, not merely 'some'"
        );
        assert_eq!(dropped, 176, "the exact drop count for this flood");

        // The control path stays uncrowded: `RunStarted`, two provisional
        // usage records, the merged final usage record, and the terminal.
        assert_eq!(
            control.len(),
            5,
            "the light control load is delivered whole: {control:?}"
        );

        // Every committed sequence number is delivered exactly once, so the
        // terminal sequence is the delivered count minus one. Had a dropped
        // fragment consumed a number, the terminal would exceed this.
        let terminal = control.last().expect("control events");
        assert!(terminal.is_terminal(), "the terminal closes the control stream");
        let delivered = data.len() + control.len();
        assert_eq!(
            terminal.seq() as usize,
            delivered - 1,
            "the sequence space is exactly the delivered events"
        );
        assert_eq!(
            terminal.seq() as usize,
            DATA_CAPACITY + 4,
            "{dropped} dropped fragments consumed zero sequence numbers"
        );

        // Presentation occupies one contiguous sequence block directly after
        // `RunStarted` and the first usage record.
        let max_data_seq = data
            .iter()
            .map(nexus_core::RunEvent::seq)
            .max()
            .expect("presentation events");
        assert_eq!(
            max_data_seq as usize,
            DATA_CAPACITY + 1,
            "delivered fragments form one contiguous block"
        );

        // The sharp invariant: control records committed AFTER the data
        // channel began dropping take the very next sequence numbers.
        let after: Vec<&nexus_core::RunEvent> = control
            .iter()
            .filter(|event| event.seq() > max_data_seq)
            .collect();
        assert_eq!(
            after.len(),
            3,
            "the second usage record, the merged final usage record, and the terminal follow the drop window"
        );
        assert_eq!(
            after[0].seq(),
            max_data_seq + 1,
            "no hole between the last delivered fragment and the next control record"
        );
        assert_eq!(after[1].seq(), max_data_seq + 2);
        assert_eq!(after[2].seq(), max_data_seq + 3);
        assert!(matches!(
            after[0].payload(),
            EventPayload::UsageUpdated(usage) if usage.input_tokens() == Some(2)
        ));
        assert!(matches!(
            after[1].payload(),
            EventPayload::UsageUpdated(usage) if usage.finality() == UsageFinality::Final
        ));
        assert!(after[2].is_terminal());

        assert_eq!(
            snapshot.last_sequence(),
            Some(terminal.seq()),
            "the snapshot names the terminal sequence"
        );
    });
}

/// A 600-record provisional-usage burst exercises the runtime's publication
/// bound while presentation traffic stays far below the data bound.
///
/// Control backpressure must not lose a single required record: every
/// bounded provisional prefix arrives in order, the merged final usage record
/// and terminal follow, and the resumed delivery is gap-free. The runtime
/// control-outbox saturation contract is covered directly by runtime tests.
#[test]
fn provisional_usage_burst_is_bounded_and_terminal_survives_resume() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut turn = Vec::with_capacity(USAGE_RECORDS as usize + FEW_TEXTS + 1);
        for counter in 1..=USAGE_RECORDS {
            turn.push(provisional(counter));
        }
        for index in 0..FEW_TEXTS {
            turn.push(fragment(index));
        }
        turn.push(stop_terminal());
        // A second scripted turn keeps the post-drain probe run well formed.
        let mut bed = common::make_bed(vec![turn, stop_turn("done")], common::quick_config());

        let response = bed.runtime.submit(common::submit_cmd("ctl-sat")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let snapshot = await_finalized(&bed, &run, "ctl-sat").await;
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed)
        );
        assert!(
            !snapshot.is_content_truncated(),
            "control backpressure is not presentation truncation: nothing was dropped from data"
        );
        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);
        assert_channel_discipline(&data, &control);

        assert_eq!(
            control.len(),
            19,
            "RunStarted, 16 bounded usage updates, final usage, and terminal"
        );
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::RunStarted { .. }
            )),
            1
        );

        // The bounded provisional prefix arrives in order, followed by the
        // terminal's final record.
        let usages: Vec<Usage> = control
            .iter()
            .filter_map(|event| match event.payload() {
                EventPayload::UsageUpdated(usage) => Some(*usage),
                _ => None,
            })
            .collect();
        let provisional_records: Vec<u64> = usages
            .iter()
            .filter(|usage| usage.finality() == UsageFinality::Provisional)
            .filter_map(Usage::input_tokens)
            .collect();
        assert_eq!(
            provisional_records,
            (1..=16).collect::<Vec<u64>>(),
            "the bounded usage prefix remains ordered"
        );
        let final_usage = usages.last().expect("usage records");
        assert_eq!(
            final_usage.finality(),
            UsageFinality::Final,
            "the last usage record is the terminal final record"
        );
        assert_eq!(
            (final_usage.input_tokens(), final_usage.output_tokens()),
            (None, None),
            "missing terminal counters stay unknown, never fabricated zeros"
        );

        // Presentation was never saturated here, so all of it survives.
        assert_eq!(
            data.len(),
            FEW_TEXTS,
            "an unsaturated data channel loses nothing: {data:?}"
        );

        let terminal = control.last().expect("control events");
        assert!(
            terminal.is_terminal(),
            "the terminal is the last control event"
        );
        assert_eq!(
            terminal.seq() as usize,
            data.len() + control.len() - 1,
            "outbox-buffered records keep the sequence space contiguous"
        );
        assert_eq!(
            snapshot.last_sequence(),
            Some(terminal.seq()),
            "the snapshot names the terminal sequence"
        );

        // Once the finalized run has been drained, a new run is accepted.
        let next = bed.runtime.submit(common::submit_cmd("ctl-sat-next")).await;
        assert_eq!(
            next.reply(),
            CommandReply::Accepted,
            "draining the finalized run frees the single-run slot"
        );
        let next_run = next.run().cloned().expect("probe run issued");
        let (next_data, next_control, next_finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            next_finished.outcome(),
            RunOutcome::Completed,
            "the runtime serves a normal run after the usage burst"
        );
        assert_ne!(next_run, run, "the probe run has its own identity");
        common::assert_contiguous(&next_data, &next_control);
        common::assert_single_terminal(&next_data, &next_control);
        assert!(
            !next_data.is_empty() && !next_control.is_empty(),
            "the probe run publishes on both channels again"
        );
    });
}

/// The data channel saturates while a real tool call executes and provisional
/// usage publication remains bounded.
///
/// Saturation must not corrupt required delivery: the tool outcome pair is
/// complete, control records keep committing gap-free sequence numbers after
/// the data channel started dropping, and the terminal is delivered exactly
/// once, last, with the snapshot naming its sequence.
#[test]
fn terminal_survives_data_saturation_while_usage_stays_bounded() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut turn = Vec::with_capacity(PAIRS * 2 + 2);
        for index in 0..PAIRS {
            turn.push(fragment(index));
            turn.push(provisional(index as u64 + 1));
        }
        turn.push(ProviderEvent::ToolCallReady(common::candidate_with(
            "item-2",
            "prov-ref-2",
            "host_read",
            r#"{"path":"src"}"#,
        )));
        turn.push(tool_calls_terminal());
        // Headless config: `host_read` is auto-authorized, so the run drives
        // itself to a terminal without the consumer issuing any command.
        let mut bed = common::make_bed(vec![turn, stop_turn("done")], common::auto_config());

        let response = bed.runtime.submit(common::submit_cmd("dual-sat")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let snapshot = await_finalized(&bed, &run, "dual-sat").await;
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed),
            "saturating both channels never turns a completed run into a failure"
        );
        assert!(
            snapshot.is_content_truncated(),
            "the full data channel is reported as content truncation"
        );
        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);
        assert_channel_discipline(&data, &control);

        // Data: the bound is admitted exactly, and the trailing turn's
        // fragment is dropped like the rest.
        assert_eq!(
            data.len(),
            DATA_CAPACITY,
            "an undrained data channel admits exactly its locked bound"
        );
        assert!(
            data.iter()
                .all(|event| matches!(event.payload(), EventPayload::AssistantTextDelta(_))),
            "only presentation traffic was delivered"
        );

        // Control: RunStarted, at most 16 provisional usage updates, final
        // usage, the tool outcome pair, and the terminal.
        assert_eq!(
            control.len(),
            21,
            "usage updates are bounded while tool outcomes and terminal survive"
        );
        let provisional_records: Vec<u64> = control
            .iter()
            .filter_map(|event| match event.payload() {
                EventPayload::UsageUpdated(usage) => Some(*usage),
                _ => None,
            })
            .filter(|usage| usage.finality() == UsageFinality::Provisional)
            .filter_map(|usage| usage.input_tokens())
            .collect();
        assert_eq!(
            provisional_records,
            (1..=16).collect::<Vec<u64>>(),
            "the bounded provisional usage prefix stays ordered"
        );

        // The tool really executed while both channels were saturated, and its
        // outcome pair survived intact.
        assert_eq!(
            bed.read_tool.execution_count(),
            1,
            "the authorized read ran during saturation"
        );
        let started: Vec<&nexus_core::RunEvent> = control
            .iter()
            .filter(|event| matches!(event.payload(), EventPayload::ToolStarted(_)))
            .collect();
        assert_eq!(started.len(), 1, "one ToolStarted");
        let finished_calls: Vec<&nexus_core::RunEvent> = control
            .iter()
            .filter(|event| matches!(event.payload(), EventPayload::ToolFinished(_)))
            .collect();
        assert_eq!(
            finished_calls.len(),
            1,
            "one ToolFinished: a started call never loses its outcome under saturation"
        );
        assert!(
            matches!(
                started[0].payload(),
                EventPayload::ToolStarted(info) if matches!(
                    finished_calls[0].payload(),
                    EventPayload::ToolFinished(outcome) if outcome.call == info.call
                )
            ),
            "the outcome is correlated with the started call"
        );

        // The terminal: unique, last, and the snapshot agrees.
        let terminal = control.last().expect("control events");
        assert!(
            terminal.is_terminal(),
            "the terminal is the last delivered control event"
        );
        assert_eq!(
            terminal.seq() as usize,
            data.len() + control.len() - 1,
            "control records keep committing numbers after the drop window, leaving no hole"
        );
        assert_eq!(
            snapshot.last_sequence(),
            Some(terminal.seq()),
            "the snapshot names the terminal sequence"
        );

        // Records published after the last delivered fragment prove the data
        // drop window consumed no sequence numbers.
        let max_data_seq = data
            .iter()
            .map(nexus_core::RunEvent::seq)
            .max()
            .expect("presentation events");
        // This is exactly where a hole would open if a dropped fragment had
        // consumed a sequence number.
        let after_drop: Vec<&nexus_core::RunEvent> = control
            .iter()
            .filter(|event| event.seq() > max_data_seq)
            .collect();
        assert_eq!(
            after_drop.len(),
            4,
            "final usage, tool outcome pair, and terminal follow the data-drop window"
        );
        assert_eq!(
            after_drop[0].seq(),
            max_data_seq + 1,
            "no hole between the last delivered fragment and the next required record"
        );
        assert!(
            after_drop.last().expect("tail").is_terminal(),
            "the tail ends at the terminal"
        );

        // A repeat snapshot read is stable: the terminal record survives the
        // saturation episode rather than being rewritten or duplicated.
        let (_, again) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-dual-sat-snap2").expect("valid"),
                run: run.clone(),
            })
            .await;
        let again = again.expect("snapshot persists");
        assert_eq!(again.lifecycle(), snapshot.lifecycle());
        assert_eq!(again.last_sequence(), snapshot.last_sequence());
        assert!(again.is_content_truncated());
    });
}
