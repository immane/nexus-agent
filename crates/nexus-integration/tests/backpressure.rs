#![forbid(unsafe_code)]

//! Backpressure, slow consumers, text batching, and cancellation under load
//! against the real runtime and real fakes.

mod common;

use std::time::Duration;

use nexus_core::{
    ApproveCommand, CommandReply, DenyCommand, EffectState, EventPayload, ExecutionStatus,
    RequestId, RunLifecycle, RunOutcome,
};
use nexus_fakes::{FakeTool, stop_turn};

/// The control path stays responsive while the data channel saturates: a
/// 1,500-fragment output flood (over the 1,024-event data bound) is driven
/// without draining data, yet the denial and the later approval are both
/// honored, no required control event is lost, and the snapshot records the
/// presentation truncation.
#[test]
fn control_stays_responsive_when_data_channel_saturates() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            common::flood_turn(
                1_500,
                vec![
                    common::candidate_with("item-9", "prov-ref-9", "host_write", r#"{"path":"a"}"#),
                    common::candidate_with(
                        "item-10",
                        "prov-ref-10",
                        "host_write",
                        r#"{"path":"b"}"#,
                    ),
                ],
            ),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed.runtime.submit(common::submit_cmd("flood")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        // The data channel is never drained here, so the flood saturates it;
        // approvals still arrive on the independent control channel.
        let first = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let (denied_approval, denied_call) =
            common::find_approval(&first).expect("first approval requested");
        let deny = bed
            .runtime
            .deny(DenyCommand {
                request: RequestId::new("req-flood-deny").expect("valid"),
                approval: denied_approval,
                run: run.clone(),
                call: denied_call.clone(),
            })
            .await;
        assert_eq!(deny.reply(), CommandReply::Accepted);

        let second = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let (granted_approval, granted_call) =
            common::find_approval(&second).expect("second approval requested");
        assert_ne!(granted_call, denied_call, "distinct queued calls");
        let approve = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-flood-approve").expect("valid"),
                approval: granted_approval,
                run: run.clone(),
                call: granted_call.clone(),
            })
            .await;
        assert_eq!(approve.reply(), CommandReply::Accepted);

        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(first);
        control.extend(second);
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(bed.write_tool.execution_count(), 1, "only the grant runs");

        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::RunStarted { .. }
            )),
            1
        );
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::ApprovalRequired(_)
            )),
            2
        );
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::ToolStarted(_)
            )),
            1,
            "denied call never starts"
        );
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::RunFinished(_)
            )),
            1
        );
        let outcomes: Vec<(ExecutionStatus, EffectState)> = control
            .iter()
            .filter_map(|event| match event.payload() {
                EventPayload::ToolFinished(info) => {
                    Some((info.outcome.status(), info.outcome.effect()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(outcomes.len(), 2);
        assert!(
            outcomes.contains(&(ExecutionStatus::Denied, EffectState::NotStarted)),
            "denial recorded without effects: {outcomes:?}"
        );
        assert!(
            outcomes.contains(&(ExecutionStatus::Succeeded, EffectState::KnownApplied)),
            "grant executed: {outcomes:?}"
        );

        let (reply, snapshot) = bed
            .runtime
            .get_snapshot(nexus_core::GetSnapshotCommand {
                request: RequestId::new("req-flood-snap").expect("valid"),
                run: run.clone(),
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let snapshot = snapshot.expect("finished run keeps a snapshot");
        assert_eq!(
            snapshot.lifecycle(),
            RunLifecycle::Finalized(RunOutcome::Completed)
        );
        assert!(
            snapshot.is_content_truncated(),
            "saturated data channel is reported, not hidden"
        );
        let terminal = control
            .iter()
            .find(|event| event.is_terminal())
            .expect("terminal observed");
        assert_eq!(
            snapshot.last_sequence(),
            Some(terminal.seq()),
            "snapshot names the terminal sequence"
        );
    });
}

/// Adapter batching is invisible to consumers: adjacent same-item fragments
/// arrive coalesced with content and order preserved (`FRAGMENTED_TEXT`
/// before the follow-up turn text), previews stay separate, and sequences
/// stay contiguous.
#[test]
fn text_batching_preserves_content_and_order() {
    use nexus_core::ProviderEvent;

    let rt = common::test_rt();
    rt.block_on(async {
        let turn = vec![
            ProviderEvent::TextDelta {
                item_key: "item-0".to_owned(),
                text: "h".to_owned(),
            },
            ProviderEvent::TextDelta {
                item_key: "item-0".to_owned(),
                text: "éllo ".to_owned(),
            },
            ProviderEvent::TextDelta {
                item_key: "item-0".to_owned(),
                text: "🌍".to_owned(),
            },
            ProviderEvent::ToolCallDelta {
                item_key: "item-1".to_owned(),
                assembled_bytes: 7,
            },
            ProviderEvent::ToolCallReady(common::candidate("host_read", r#"{"path":"src"}"#)),
            ProviderEvent::TurnFinished(nexus_core::TurnFinished::new(
                nexus_core::FinishReason::ToolCalls,
                nexus_core::Usage::new(None, None, nexus_core::UsageFinality::Final),
                None,
            )),
        ];
        let mut bed = common::make_bed(vec![turn, stop_turn("done")], common::quick_config());
        let response = bed.runtime.submit(common::submit_cmd("batch")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        common::assert_contiguous(&data, &control);

        let mut texts: Vec<(u64, &str)> = Vec::new();
        for event in &data {
            if let EventPayload::AssistantTextDelta(fragment) = event.payload() {
                texts.push((event.seq(), fragment.text.as_str()));
            }
        }
        texts.sort_by_key(|(seq, _)| *seq);
        let joined: String = texts.iter().map(|(_, text)| *text).collect();
        assert_eq!(
            joined,
            format!("{}done", nexus_fakes::provider::FRAGMENTED_TEXT),
            "batched fragments keep content and order"
        );
        assert_eq!(texts.len(), 2, "one coalesced event per turn");
        assert_eq!(texts[0].1, nexus_fakes::provider::FRAGMENTED_TEXT);
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::ToolCallPreview { .. }
            )),
            1,
            "progress previews never become text"
        );
    });
}

/// Prompt cancellation under a saturated output flood: the terminal outcome
/// is cancelled, every started tool has its outcome, and exactly one
/// terminal event exists.
#[test]
fn prompt_cancellation_under_output_load() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            common::flood_turn(
                1_500,
                vec![common::candidate("host_read", r#"{"path":"src"}"#)],
            ),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed_with_tools(
            script,
            common::auto_config(),
            FakeTool::delayed("host_read", Duration::from_millis(200)),
            FakeTool::mutation(),
        );
        let response = bed.runtime.submit(common::submit_cmd("cancel-load")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        // Data stays saturated (never drained); cancellation still lands.
        let prelude = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ToolStarted(_))
        })
        .await;
        let cancel = bed
            .runtime
            .cancel(nexus_core::CancelCommand {
                request: RequestId::new("req-load-cancel").expect("valid"),
                run: run.clone(),
            })
            .await;
        assert_eq!(cancel.reply(), CommandReply::Accepted);

        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(prelude);
        assert_eq!(finished.outcome(), RunOutcome::Cancelled);
        let started: Vec<nexus_core::CallId> = control
            .iter()
            .filter_map(|event| match event.payload() {
                EventPayload::ToolStarted(info) => Some(info.call.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(started.len(), 1);
        for call in &started {
            assert!(
                control.iter().any(|event| match event.payload() {
                    EventPayload::ToolFinished(info) => &info.call == call,
                    _ => false,
                }),
                "every ToolStarted has an outcome"
            );
        }
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::RunFinished(_)
            )),
            1
        );
    });
}
