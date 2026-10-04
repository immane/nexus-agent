#![forbid(unsafe_code)]

//! Review regressions: adversarial provider turn shapes that must never
//! dispatch.
//!
//! Findings under test (provider contract, assembly and finish reasons):
//! duplicate references, conflicting references, malformed candidates, and
//! unfinished invocations must not result in dispatch even when the provider
//! claims a successful `TurnFinished(ToolCalls)`; invalid terminal framing
//! (missing, duplicated, or non-final terminal) is a protocol failure.

mod common;

use nexus_core::{
    CallCandidate, EventPayload, FinishReason, ProviderEvent, RunOutcome, TurnFinished, Usage,
    UsageFinality,
};
use nexus_fakes::stop_turn;

fn tool_calls_terminal() -> ProviderEvent {
    ProviderEvent::TurnFinished(TurnFinished::new(
        FinishReason::ToolCalls,
        Usage::new(None, None, UsageFinality::Final),
        None,
    ))
}

fn ready(item: &str, provider_ref: &str, tool: &str, args: &str) -> ProviderEvent {
    ProviderEvent::ToolCallReady(
        CallCandidate::new(item, provider_ref, tool, args).expect("candidate builds"),
    )
}

async fn assert_no_dispatch(bed: &mut common::Bed, expected: RunOutcome) {
    let (data, control, finished) =
        common::drain_until_finished(&mut bed.data, &mut bed.control).await;
    assert_eq!(
        finished.outcome(),
        expected,
        "terminal outcome for the adversarial turn"
    );
    assert_eq!(
        common::count_payload(&data, &control, |payload| matches!(
            payload,
            EventPayload::ToolStarted(_)
        )),
        0,
        "no call may start"
    );
    assert_eq!(bed.read_tool.execution_count(), 0, "read tool never ran");
    assert_eq!(bed.write_tool.execution_count(), 0, "write tool never ran");
    common::assert_single_terminal(&data, &control);
}

#[test]
fn malformed_candidate_in_successful_turn_never_dispatches() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            vec![
                ready("item-0", "prov-ref-0", "host_read", "not-an-object"),
                tool_calls_terminal(),
            ],
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("malformed-success"))
            .await;
        assert_eq!(response.reply(), nexus_core::CommandReply::Accepted);

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::ToolStarted(_)
            )),
            0
        );
        assert_eq!(bed.read_tool.execution_count(), 0);
        let denied: Vec<_> = control
            .iter()
            .filter_map(|event| match event.payload() {
                EventPayload::ToolFinished(info) => Some(info.outcome.status()),
                _ => None,
            })
            .collect();
        assert_eq!(
            denied,
            vec![nexus_core::ExecutionStatus::Denied],
            "malformed arguments are denied explicitly"
        );
        common::assert_single_terminal(&data, &control);
    });
}

#[test]
fn duplicate_provider_reference_in_successful_turn_never_dispatches() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![vec![
            ready("item-1", "prov-dup", "host_read", r#"{"path":"a"}"#),
            ready("item-2", "prov-dup", "host_read", r#"{"path":"a"}"#),
            tool_calls_terminal(),
        ]];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("duplicate-success"))
            .await;
        assert_eq!(response.reply(), nexus_core::CommandReply::Accepted);

        assert_no_dispatch(&mut bed, RunOutcome::Failed).await;
    });
}

#[test]
fn conflicting_provider_reference_in_successful_turn_never_dispatches() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![vec![
            ready("item-1", "prov-dup", "host_read", r#"{"path":"a"}"#),
            ready("item-2", "prov-dup", "host_write", r#"{"path":"b"}"#),
            tool_calls_terminal(),
        ]];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("conflicting-success"))
            .await;
        assert_eq!(response.reply(), nexus_core::CommandReply::Accepted);

        assert_no_dispatch(&mut bed, RunOutcome::Failed).await;
    });
}

#[test]
fn unfinished_invocation_never_dispatches() {
    let rt = common::test_rt();
    rt.block_on(async {
        // No terminal event at all: the invocation was cut short.
        let script = vec![vec![ready(
            "item-0",
            "prov-ref-0",
            "host_read",
            r#"{"path":"a"}"#,
        )]];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed.runtime.submit(common::submit_cmd("unfinished")).await;
        assert_eq!(response.reply(), nexus_core::CommandReply::Accepted);

        assert_no_dispatch(&mut bed, RunOutcome::Failed).await;
    });
}

#[test]
fn terminal_not_last_never_dispatches() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![vec![
            ready("item-0", "prov-ref-0", "host_read", r#"{"path":"a"}"#),
            tool_calls_terminal(),
            ProviderEvent::TextDelta {
                item_key: "item-0".to_owned(),
                text: "after terminal".to_owned(),
            },
        ]];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("terminal-not-last"))
            .await;
        assert_eq!(response.reply(), nexus_core::CommandReply::Accepted);

        assert_no_dispatch(&mut bed, RunOutcome::Failed).await;
    });
}

#[test]
fn duplicate_terminal_never_dispatches() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![vec![
            ready("item-0", "prov-ref-0", "host_read", r#"{"path":"a"}"#),
            tool_calls_terminal(),
            tool_calls_terminal(),
        ]];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed
            .runtime
            .submit(common::submit_cmd("duplicate-terminal"))
            .await;
        assert_eq!(response.reply(), nexus_core::CommandReply::Accepted);

        assert_no_dispatch(&mut bed, RunOutcome::Failed).await;
    });
}

#[test]
fn incomplete_finish_reason_never_dispatches() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![vec![
            ready("item-0", "prov-ref-0", "host_read", r#"{"path":"a"}"#),
            ProviderEvent::TurnFinished(TurnFinished::new(
                FinishReason::Incomplete,
                Usage::new(None, None, UsageFinality::Final),
                None,
            )),
        ]];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed.runtime.submit(common::submit_cmd("incomplete")).await;
        assert_eq!(response.reply(), nexus_core::CommandReply::Accepted);

        assert_no_dispatch(&mut bed, RunOutcome::Failed).await;
    });
}
