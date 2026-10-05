#![forbid(unsafe_code)]

//! Thinking-trace echo: a turn's `reasoning_content` must reach the next
//! turn's request once per assistant turn. Vendors running a thinking mode
//! (DeepSeek) reject follow-up requests that drop it with a 400, so a lost
//! trace turns every multi-turn tool run into a failure after the first
//! successful dispatch.
//!
//! Drives the real runtime with scripted turns shaped like the observed
//! live run: turn 1 carries text plus a thinking trace plus a read call,
//! turn 2 stops. The assertion inspects the recorded turn-2 request.

mod common;

use nexus_core::{
    FinishReason, ModelContextItem, ProviderEvent, RunOutcome, TurnFinished, Usage, UsageFinality,
};

/// Turn 1 of the observed live run: assistant text, a thinking trace, and
/// an automatic read call.
fn read_turn_with_thinking() -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::TextDelta {
            item_key: "item-0".to_owned(),
            text: "I will read the file first.".to_owned(),
        },
        ProviderEvent::ReasoningDelta {
            text: "the user wants test.txt; read it before answering".to_owned(),
        },
        ProviderEvent::ToolCallReady(common::candidate_with(
            "item-1",
            "prov-ref-1",
            "host_read",
            r#"{"path":"test.txt"}"#,
        )),
        ProviderEvent::TurnFinished(TurnFinished::new(
            FinishReason::ToolCalls,
            Usage::new(None, None, UsageFinality::Final),
            None,
        )),
    ]
}

#[test]
fn turn_two_request_echoes_turn_one_thinking_once_per_assistant_turn() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut bed = common::make_bed(
            vec![
                read_turn_with_thinking(),
                nexus_fakes::stop_turn("summarized"),
            ],
            common::quick_config(),
        );
        let response = bed.runtime.submit(common::submit_cmd("reasoning")).await;
        assert_eq!(
            response.reply(),
            nexus_core::CommandReply::Accepted,
            "submit is accepted"
        );
        let (_data, _control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        common::assert_single_terminal(&_data, &_control);

        let requests = bed.provider.requests();
        assert_eq!(requests.len(), 2, "two turns ran: {requests:?}");
        let conversation = requests[1].conversation();
        let mut assistant_seen = 0;
        let mut traces = Vec::new();
        for item in conversation {
            match item {
                ModelContextItem::AssistantText { reasoning, .. }
                | ModelContextItem::AssistantCall { reasoning, .. } => {
                    assistant_seen += 1;
                    if let Some(trace) = reasoning {
                        traces.push(trace.as_str());
                    }
                }
                _ => {}
            }
        }
        assert_eq!(
            assistant_seen, 2,
            "text and call items both echo: {conversation:?}"
        );
        assert_eq!(
            traces,
            ["the user wants test.txt; read it before answering"]
        );
    });
}

#[test]
fn reasoning_free_turns_record_no_trace() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut bed = common::make_bed(
            vec![
                nexus_fakes::tool_turn(vec![common::candidate(
                    "host_read",
                    r#"{"path":"test.txt"}"#,
                )]),
                nexus_fakes::stop_turn("done"),
            ],
            common::quick_config(),
        );
        let response = bed.runtime.submit(common::submit_cmd("no-reasoning")).await;
        assert_eq!(response.reply(), nexus_core::CommandReply::Accepted);
        let (_data, _control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);

        let requests = bed.provider.requests();
        assert_eq!(requests.len(), 2);
        for item in requests[1].conversation() {
            assert_eq!(
                item.reasoning(),
                None,
                "no trace in, no trace out: {item:?}"
            );
        }
    });
}
