#![forbid(unsafe_code)]

//! Provider refusal hardening, end to end against the real runtime.
//!
//! A refusal is a normal, expected terminal for a model turn, and the M0
//! lock requires it to be reported honestly in both directions: a refusal is
//! never relabeled as complete success, and it never fabricates the artifacts
//! of one. This file pins the observable contract through the public
//! [`Runtime`](nexus_runtime::Runtime) boundary with the real
//! [`FakeProvider`](nexus_fakes::FakeProvider) / [`FakeTool`](nexus_fakes::FakeTool)
//! doubles:
//!
//! - a refusal terminal reports its usage exactly as the provider stated it:
//!   missing counters stay unknown and are never fabricated as zero, while a
//!   provider-reported zero stays a known zero, and provisional counters are
//!   never promoted into the terminal merge;
//! - the run finalizes as `RunOutcome::Refused` without any dispatch: no call
//!   starts, no approval is requested, no outcome is recorded, no host call
//!   identity is consumed, and the slot is released for the next run;
//! - text streamed before a refusal is preserved as presentation traffic on
//!   the data channel, while the refused turn is never retained as completed
//!   assistant content and never triggers a follow-up turn;
//! - candidates carried by a refusal turn are discarded whole: they are never
//!   admitted, so they consume neither an approval nor a recorded outcome,
//!   even though the batch validator deliberately tolerates them;
//! - duplicate and conflicting provider references never dispatch in a
//!   dispatchable success, and the ambiguity tolerance is scoped to
//!   non-dispatching terminals only.
//!
//! Every wait is bounded by `tests/common/mod.rs` helpers and every assertion
//! is on recorded invariants (outcomes, sequences, terminal counts, tool
//! execution logs), never on wall-clock timing. No randomness, no sleeps
//! without a condition, no I/O.

mod common;

use std::time::Duration;

use nexus_core::{
    CallCandidate, CommandReply, EventPayload, FinishReason, GetSnapshotCommand, ModelContextItem,
    PersistenceState, ProviderEvent, RequestId, RunLifecycle, RunOutcome, TurnFinished, Usage,
    UsageFinality,
};

/// Terminal refusal with unknown usage (the provider stated no counters).
fn refusal_terminal() -> ProviderEvent {
    terminal(
        FinishReason::Refusal,
        Usage::new(None, None, UsageFinality::Final),
    )
}

/// One terminal event carrying `reason` and `usage`.
fn terminal(reason: FinishReason, usage: Usage) -> ProviderEvent {
    ProviderEvent::TurnFinished(TurnFinished::new(reason, usage, None))
}

/// One complete but still unauthorized call candidate.
fn ready(item: &str, provider_ref: &str, tool: &str, args: &str) -> ProviderEvent {
    ProviderEvent::ToolCallReady(
        CallCandidate::new(item, provider_ref, tool, args).expect("candidate builds"),
    )
}

/// Non-executable argument progress for one turn-local item key.
fn progress(item: &str, assembled_bytes: usize) -> ProviderEvent {
    ProviderEvent::ToolCallDelta {
        item_key: item.to_owned(),
        assembled_bytes,
    }
}

/// Collects every usage update published on either channel, in arrival order
/// relative to that channel's own sequence.
fn usage_updates(data: &[nexus_core::RunEvent], control: &[nexus_core::RunEvent]) -> Vec<Usage> {
    data.iter()
        .chain(control.iter())
        .filter_map(|event| match event.payload() {
            EventPayload::UsageUpdated(usage) => Some(*usage),
            _ => None,
        })
        .collect()
}

/// Asserts that a refusal run published no authorizing or executing tool
/// traffic: no call started, no approval requested, and no outcome recorded.
/// A discarded candidate is not a denied call, so the absence of
/// `ToolFinished` is the observable difference.
///
/// `ToolCallPreview` is deliberately excluded: argument progress is
/// non-executable presentation traffic that a refusal may legitimately
/// surface, so counting it here would assert the wrong invariant.
fn assert_no_tool_traffic(data: &[nexus_core::RunEvent], control: &[nexus_core::RunEvent]) {
    for (label, matches) in [
        (
            "ToolStarted",
            (|payload: &EventPayload| matches!(payload, EventPayload::ToolStarted(_)))
                as fn(&EventPayload) -> bool,
        ),
        ("ApprovalRequired", |payload: &EventPayload| {
            matches!(payload, EventPayload::ApprovalRequired(_))
        }),
        ("ToolFinished", |payload: &EventPayload| {
            matches!(payload, EventPayload::ToolFinished(_))
        }),
    ] {
        assert_eq!(
            common::count_payload(data, control, matches),
            0,
            "a refused run publishes no {label}"
        );
    }
}

/// Asserts that neither host tool double observed an execution.
fn assert_tools_untouched(bed: &common::Bed) {
    assert_eq!(bed.read_tool.execution_count(), 0, "read tool never ran");
    assert_eq!(bed.write_tool.execution_count(), 0, "write tool never ran");
    assert!(
        bed.read_tool.log().is_empty() && bed.write_tool.log().is_empty(),
        "no admitted call reached either executor"
    );
}

/// Bound for the terminal wait. The scripted provider never blocks, so
/// exceeding it means a hang (for example a run parked on an approval nobody
/// grants) rather than a slow machine.
const WAIT: Duration = Duration::from_secs(5);

/// Drains both channels to a terminal, failing inline on any authorizing or
/// executing control event.
///
/// This is [`common::drain_until_finished`] with the no-dispatch invariant
/// woven into the wait. Asserting only afterwards is not enough: a host that
/// wrongly dispatches a refused candidate parks the run on an approval that
/// never arrives, so the only assertion that could fire is the wait timeout,
/// naming no offending event. Asserting inside the loop reports the exact
/// event instead. The post-terminal drain matches the shared helper so a
/// duplicate terminal stays visible to `common::assert_single_terminal`.
async fn drain_without_dispatch(
    bed: &mut common::Bed,
) -> (
    Vec<nexus_core::RunEvent>,
    Vec<nexus_core::RunEvent>,
    nexus_core::RunFinished,
) {
    let mut datas: Vec<nexus_core::RunEvent> = Vec::new();
    let mut controls: Vec<nexus_core::RunEvent> = Vec::new();
    let terminal = tokio::time::timeout(WAIT, async {
        loop {
            let (from_data, event) = tokio::select! {
                event = bed.data.recv() => (true, event),
                event = bed.control.recv() => (false, event),
            };
            let Some(event) = event else {
                panic!("a channel closed before the terminal");
            };
            assert!(
                !matches!(
                    event.payload(),
                    EventPayload::ApprovalRequired(_) | EventPayload::ToolStarted(_)
                ),
                "no approval may be requested and no call may start, got {event:?}"
            );
            // Record the terminal like every other event before breaking, so
            // `assert_single_terminal` and the post-terminal drain below can
            // still see a duplicate.
            let terminal = match event.payload() {
                EventPayload::RunFinished(finished) => Some(finished.clone()),
                _ => None,
            };
            if from_data {
                datas.push(event);
            } else {
                controls.push(event);
            }
            if let Some(finished) = terminal {
                break finished;
            }
        }
    })
    .await
    .expect("the run reaches its terminal promptly");

    // Post-terminal drain of both channels, matching `drain_until_finished`:
    // a duplicate terminal must be collected, never hidden behind the first.
    let mut quiet_passes = 0;
    let mut rounds = 0;
    while quiet_passes < 2 && rounds < 64 {
        rounds += 1;
        let mut drained = false;
        while let Ok(event) = bed.data.try_recv() {
            datas.push(event);
            drained = true;
        }
        while let Ok(event) = bed.control.try_recv() {
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
    (datas, controls, terminal)
}

/// A refusal terminal reports usage exactly as the provider stated it:
/// unknown counters stay unknown, a provider-reported zero stays a known
/// zero, and neither is fabricated from the other.
#[test]
fn refusal_terminal_reports_usage_without_fabricating_counters() {
    let rt = common::test_rt();
    rt.block_on(async {
        // A refusal that reports no counters: both stay unknown and the
        // published record is final (a consumed invocation cannot report
        // provisional counters).
        let mut unknown = common::make_bed(vec![vec![refusal_terminal()]], common::quick_config());
        let response = unknown
            .runtime
            .submit(common::submit_cmd("refusal-unknown-usage"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let (data, control, finished) =
            common::drain_until_finished(&mut unknown.data, &mut unknown.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Refused);
        assert_eq!(finished.persistence(), PersistenceState::Ephemeral);
        assert_eq!(
            usage_updates(&data, &control),
            vec![Usage::new(None, None, UsageFinality::Final)],
            "the refusal publishes exactly its own terminal usage, with unknown counters intact"
        );
        let reported = usage_updates(&data, &control);
        assert_ne!(reported[0].input_tokens(), Some(0), "unknown is never zero");
        assert_ne!(
            reported[0].output_tokens(),
            Some(0),
            "unknown is never zero"
        );
        assert_eq!(
            reported[0].finality(),
            UsageFinality::Final,
            "a fully consumed invocation reports final counters"
        );
        common::assert_contiguous(&data, &control);

        // A refusal that reports an explicit zero output: the zero is the
        // provider's own reading, so it must survive as a known zero and must
        // not be confused with the unknown case above.
        let explicit = Usage::new(Some(11), Some(0), UsageFinality::Final);
        let mut known = common::make_bed(
            vec![vec![terminal(FinishReason::Refusal, explicit)]],
            common::quick_config(),
        );
        let response = known
            .runtime
            .submit(common::submit_cmd("refusal-known-usage"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let (data, control, finished) =
            common::drain_until_finished(&mut known.data, &mut known.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Refused);
        assert_eq!(
            usage_updates(&data, &control),
            vec![explicit],
            "provider-reported counters pass through unchanged, including an explicit zero"
        );

        // Provisional counters reported before a refusal terminal that omits
        // them: the provisional reading is published as provisional and never
        // promoted into the terminal merge, so the terminal stays unknown.
        let provisional = Usage::new(Some(7), Some(3), UsageFinality::Provisional);
        let mut promoted = common::make_bed(
            vec![vec![ProviderEvent::Usage(provisional), refusal_terminal()]],
            common::quick_config(),
        );
        let response = promoted
            .runtime
            .submit(common::submit_cmd("refusal-provisional-usage"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let (data, control, finished) =
            common::drain_until_finished(&mut promoted.data, &mut promoted.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Refused);
        assert_eq!(
            usage_updates(&data, &control),
            vec![provisional, Usage::new(None, None, UsageFinality::Final),],
            "provisional counters are never promoted into the refusal's terminal merge"
        );
    });
}

/// A refusal finalizes the run as `Refused` and releases every resource the
/// run owned: no dispatch, no approval, no recorded outcome, and the single
/// run slot is immediately reusable.
#[test]
fn refusal_finalizes_the_run_without_any_dispatch() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            vec![refusal_terminal()],
            // A follow-up turn exists only to prove the refusal ends the run
            // instead of continuing the model loop; it must stay unconsumed.
            nexus_fakes::stop_turn("never reached"),
        ];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("refusal-terminal"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let (data, control, finished) = drain_without_dispatch(&mut bed).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Refused,
            "a refused turn finalizes the run as refused, never as completed"
        );
        assert_eq!(finished.persistence(), PersistenceState::Ephemeral);
        assert!(finished.persistence_error().is_none());
        common::assert_single_terminal(&data, &control);
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::RunStarted { .. }
            )),
            1,
            "exactly one run-started"
        );
        common::assert_contiguous(&data, &control);
        assert_no_tool_traffic(&data, &control);
        assert_tools_untouched(&bed);

        // A refusal ends the run rather than continuing the model loop.
        assert_eq!(
            bed.provider.call_count(),
            1,
            "the scripted follow-up turn is never requested after a refusal"
        );

        // The terminal record is authoritative and bounded: no known outcome
        // exists because nothing was admitted.
        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-refusal-snap").expect("valid"),
                run: run.clone(),
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("a known run keeps a snapshot");
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Refused),
            "the snapshot reports the refusal, not a completed run"
        );
        assert!(
            snapshot.known_outcomes().is_empty(),
            "a refusal records no tool outcome"
        );
        assert!(
            snapshot.pending_approvals().is_empty(),
            "a refusal leaves no approval pending"
        );
        assert!(
            !snapshot.is_content_truncated(),
            "a short refused turn is not reported as truncated"
        );

        // The refused run released the single-run slot without quarantine: a
        // provider that returned normally never blocks a new submission.
        let next = bed
            .runtime
            .submit(common::submit_cmd("after-refusal"))
            .await;
        assert_eq!(
            next.reply(),
            CommandReply::Accepted,
            "the refused run frees the run slot immediately"
        );
    });
}

/// Text streamed before a refusal is preserved as presentation traffic on the
/// data channel, while the refused turn is never retained as completed
/// assistant content and never produces a follow-up turn.
#[test]
fn refusal_after_partial_text_keeps_the_text_without_retaining_the_turn() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![vec![
            ProviderEvent::TextDelta {
                item_key: "item-0".to_owned(),
                text: "I ".to_owned(),
            },
            ProviderEvent::TextDelta {
                item_key: "item-0".to_owned(),
                text: "cannot".to_owned(),
            },
            ProviderEvent::TextDelta {
                item_key: "item-1".to_owned(),
                text: " help with that".to_owned(),
            },
            refusal_terminal(),
        ]];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("refusal-partial-text"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) = drain_without_dispatch(&mut bed).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Refused,
            "partial text before a refusal is still a refusal, not a completed answer"
        );

        // The refused turn's text survives on the presentation channel with
        // adjacent same-item fragments coalesced and order preserved.
        let mut fragments: Vec<(u64, &str, &str)> = data
            .iter()
            .filter_map(|event| match event.payload() {
                EventPayload::AssistantTextDelta(fragment) => Some((
                    event.seq(),
                    fragment.item_key.as_str(),
                    fragment.text.as_str(),
                )),
                _ => None,
            })
            .collect();
        fragments.sort_by_key(|(seq, _, _)| *seq);
        assert_eq!(
            fragments,
            vec![(1, "item-0", "I cannot"), (2, "item-1", " help with that"),],
            "refused text is published, coalesced per item, in order"
        );

        // The text is presentation only: the refusal ends the run, so the
        // refused turn is never echoed back as a completed assistant item.
        assert_eq!(
            bed.provider.call_count(),
            1,
            "a refused turn never requests a follow-up turn"
        );
        let requests = bed.provider.requests();
        assert_eq!(requests.len(), 1);
        assert!(
            !requests[0]
                .conversation()
                .iter()
                .any(|item| matches!(item, ModelContextItem::AssistantText { .. })),
            "the refused turn is never retained as completed assistant content"
        );

        common::assert_contiguous(&data, &control);
        common::assert_single_terminal(&data, &control);
        assert_no_tool_traffic(&data, &control);
        assert_tools_untouched(&bed);
    });
}

/// Candidates carried by a refusal turn are discarded whole. The batch
/// validator deliberately tolerates them (a refusal never dispatches), so the
/// runtime's own discard is the only thing keeping them inert: they are never
/// admitted, so no approval is requested, no host call identity is consumed,
/// and no denial outcome is recorded for a call that was never admitted.
#[test]
fn refusal_discards_candidates_without_admission_or_dispatch() {
    let rt = common::test_rt();
    rt.block_on(async {
        // Both an automatic read and an approval-gated mutation, plus
        // argument progress, so a naive host would preview, admit, and
        // dispatch the set.
        let script = vec![vec![
            progress("item-1", 12),
            ready("item-1", "prov-ref-1", "host_read", r#"{"path":"src"}"#),
            progress("item-2", 14),
            ready("item-2", "prov-ref-2", "host_write", r#"{"path":"dst"}"#),
            refusal_terminal(),
        ]];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("refusal-candidates"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let (data, control, finished) = drain_without_dispatch(&mut bed).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Refused,
            "candidates on a refusal turn do not change the terminal outcome"
        );
        common::assert_single_terminal(&data, &control);
        common::assert_contiguous(&data, &control);
        assert_no_tool_traffic(&data, &control);
        assert_tools_untouched(&bed);

        // Argument progress may still surface as non-executable presentation
        // traffic, but it never becomes an approval, a start, or an outcome.
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::ToolCallPreview { .. }
            )),
            2,
            "progress previews are presentation only and never authorize"
        );

        // The discarded candidates were never admitted: no call identity was
        // issued, so the finalized snapshot knows no outcome at all.
        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(GetSnapshotCommand {
                request: RequestId::new("req-refusal-cand-snap").expect("valid"),
                run,
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("a known run keeps a snapshot");
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Refused)
        );
        assert!(
            snapshot.known_outcomes().is_empty(),
            "a discarded candidate is never recorded, not even as a denial"
        );
        assert!(snapshot.pending_approvals().is_empty());
    });
}

/// Duplicate and conflicting provider references are ambiguous identities. In
/// a dispatchable tool-calls success the host cannot choose a binding, so the
/// invocation is rejected as a protocol failure before any admission.
#[test]
fn ambiguous_references_in_successful_turns_never_dispatch() {
    let rt = common::test_rt();
    rt.block_on(async {
        // The same provider reference bound to two different calls.
        let duplicate = vec![vec![
            ready("item-1", "prov-dup", "host_read", r#"{"path":"a"}"#),
            ready("item-2", "prov-dup", "host_read", r#"{"path":"b"}"#),
            terminal(
                FinishReason::ToolCalls,
                Usage::new(None, None, UsageFinality::Final),
            ),
        ]];
        let mut bed = common::make_bed(duplicate, common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("duplicate-ref-success"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) = drain_without_dispatch(&mut bed).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Failed,
            "an ambiguous identity is a protocol failure in a dispatchable success"
        );
        let error = finished
            .error()
            .expect("a failed terminal carries its typed diagnostic");
        assert_eq!(
            error.category(),
            nexus_core::ErrorCategory::Protocol,
            "the rejection is a typed protocol diagnostic, not a silent success"
        );
        assert_eq!(error.retry(), nexus_core::RetryGuidance::DoNotRetry);
        common::assert_single_terminal(&data, &control);
        common::assert_contiguous(&data, &control);
        assert_no_tool_traffic(&data, &control);
        assert_tools_untouched(&bed);

        // One reference bound to two different tools is equally unresolvable:
        // the host must not pick the read or the mutation.
        let conflicting = vec![vec![
            ready("item-1", "prov-dup", "host_read", r#"{"path":"a"}"#),
            ready("item-2", "prov-dup", "host_write", r#"{"path":"b"}"#),
            terminal(
                FinishReason::ToolCalls,
                Usage::new(None, None, UsageFinality::Final),
            ),
        ]];
        let mut bed = common::make_bed(conflicting, common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("conflicting-ref-success"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) = drain_without_dispatch(&mut bed).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Failed,
            "one reference cannot resolve to two tools"
        );
        assert_eq!(
            finished
                .error()
                .expect("a failed terminal carries its typed diagnostic")
                .category(),
            nexus_core::ErrorCategory::Protocol,
        );
        common::assert_single_terminal(&data, &control);
        assert_no_tool_traffic(&data, &control);
        assert_tools_untouched(&bed);
    });
}

/// The tolerance for ambiguous identities is scoped to terminals that never
/// dispatch. The same candidate shapes are tolerated by a refusal (which
/// discards them) and rejected by a tool-calls success (which would dispatch
/// them), so the tolerance is a property of the terminal, not of the batch.
#[test]
fn ambiguous_references_are_tolerated_only_by_non_dispatching_terminals() {
    let rt = common::test_rt();
    rt.block_on(async {
        // Duplicate and conflicting references behind a refusal terminal.
        let cases: [(&str, Vec<Vec<ProviderEvent>>); 2] = [
            (
                "duplicate-refusal",
                vec![vec![
                    ready("item-1", "prov-dup", "host_read", r#"{"path":"a"}"#),
                    ready("item-2", "prov-dup", "host_read", r#"{"path":"b"}"#),
                    refusal_terminal(),
                ]],
            ),
            (
                "conflicting-refusal",
                vec![vec![
                    ready("item-1", "prov-dup", "host_read", r#"{"path":"a"}"#),
                    ready("item-2", "prov-dup", "host_write", r#"{"path":"b"}"#),
                    refusal_terminal(),
                ]],
            ),
        ];
        for (tag, script) in cases {
            let mut bed = common::make_bed(script, common::quick_config());
            let response = bed.runtime.submit(common::submit_cmd(tag)).await;
            assert_eq!(response.reply(), CommandReply::Accepted, "{tag}");

            // Fast-fail on admission: an ambiguous candidate behind a refusal
            // must never reach an approval or a start, so the first such
            // event the runtime publishes would already be the defect.
            let (data, control, finished) = drain_without_dispatch(&mut bed).await;
            assert_eq!(
                finished.outcome(),
                RunOutcome::Refused,
                "{tag}: ambiguity is tolerated only because a refusal never dispatches"
            );
            common::assert_single_terminal(&data, &control);
            common::assert_contiguous(&data, &control);
            assert_no_tool_traffic(&data, &control);
            assert_tools_untouched(&bed);
        }

        // The identical candidate shapes behind a dispatchable success are
        // rejected rather than tolerated, which is what makes the tolerance
        // above terminal-scoped rather than a blanket allowance.
        let success = vec![vec![
            ready("item-1", "prov-dup", "host_read", r#"{"path":"a"}"#),
            ready("item-2", "prov-dup", "host_write", r#"{"path":"b"}"#),
            terminal(
                FinishReason::ToolCalls,
                Usage::new(None, None, UsageFinality::Final),
            ),
        ]];
        let mut bed = common::make_bed(success, common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("ambiguity-scoped"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let (data, control, finished) = drain_without_dispatch(&mut bed).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Failed,
            "the same shape behind a dispatchable terminal is rejected, never tolerated"
        );
        common::assert_single_terminal(&data, &control);
        assert_no_tool_traffic(&data, &control);
        assert_tools_untouched(&bed);
    });
}
