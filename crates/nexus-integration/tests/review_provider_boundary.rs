#![forbid(unsafe_code)]

//! Review regressions: provider invocation boundary.
//!
//! Findings under test:
//! - cancellation that arrives after the provider entered `stream` must be
//!   observable on the live context the worker re-reads, and the run must
//!   still finalize as cancelled;
//! - a run deadline that passes while the provider is blocked must end the
//!   run as an explicit limit outcome, never as a fabricated success;
//! - each model turn invokes the provider exactly once, with the effective
//!   output budget, and the additive `ModelRequest` conversation carries the
//!   submitted task, the admitted call, and the recorded result once the
//!   runtime populates it.
//!
//! The gated provider is test-local (see `review_gates`): the shared fakes
//! do not gate a provider worker yet.

mod common;

#[path = "review_gates/mod.rs"]
mod gates;

use std::sync::Arc;
use std::time::Duration;

use gates::{GatedProvider, candidate, next_entry, review_bed, stop_turn, tool_turn};
use nexus_core::{
    CancelCommand, CommandReply, ErrorCategory, Limits, ModelContextItem, RequestId, RunOutcome,
    ToolPort,
};
use nexus_fakes::FakeTool;
use nexus_runtime::{Policy, RuntimeConfig};

fn tools() -> Vec<Arc<dyn ToolPort + Send + Sync>> {
    vec![
        Arc::new(FakeTool::read_only()),
        Arc::new(FakeTool::mutation()),
    ]
}

#[test]
fn provider_observes_cancellation_after_entry_and_run_cancels() {
    let rt = common::test_rt();
    rt.block_on(async {
        let (provider, mut entries) = GatedProvider::new(vec![stop_turn("late")], true);
        let provider = Arc::new(provider);
        let mut bed = review_bed(common::quick_config(), provider.clone(), tools());
        let response = bed
            .runtime
            .submit(common::submit_cmd("provider-cancel"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let entry = next_entry(&mut entries).await;
        assert!(
            entry.context.deadline().is_some(),
            "the runtime must install a live evaluable deadline for the provider invocation"
        );

        let cancel = bed
            .runtime
            .cancel(CancelCommand {
                request: RequestId::new("req-provider-cancel").expect("valid"),
                run,
            })
            .await;
        assert_eq!(cancel.reply(), CommandReply::Accepted);
        assert!(
            entry.context.is_cancelled(),
            "the live token reflects cancellation that arrived after provider entry"
        );

        entry.release();
        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::Cancelled,
            "late provider output after cancellation never completes the run"
        );
        assert_eq!(provider.call_count(), 1, "no replacement invocation");
        common::assert_single_terminal(&data, &control);
    });
}

#[test]
fn provider_deadline_after_entry_yields_limit_reached_without_dispatch() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = Limits::m0_test();
        limits.run_duration = Duration::from_millis(150);
        let config = RuntimeConfig {
            limits,
            policy: Policy::m0_test(),
            has_approval_handler: true,
        };
        let (provider, mut entries) = GatedProvider::new(vec![stop_turn("late")], true);
        let provider = Arc::new(provider);
        let mut bed = review_bed(config, provider.clone(), tools());
        let response = bed
            .runtime
            .submit(common::submit_cmd("provider-deadline"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let entry = next_entry(&mut entries).await;
        assert!(
            entry.context.deadline().is_some(),
            "the runtime must install a live evaluable deadline for the provider invocation"
        );

        // Let the run deadline elapse while the provider worker is blocked.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            entry
                .context
                .check_active()
                .expect_err("live deadline elapsed")
                .category(),
            ErrorCategory::Timeout,
            "the blocked worker observes the run deadline after entry"
        );

        entry.release();
        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::LimitReached,
            "a provider invocation that outlives the run deadline is a limit outcome, not success"
        );
        assert_eq!(
            provider.call_count(),
            1,
            "the expired invocation is never retried inside the same run"
        );
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                nexus_core::EventPayload::ToolStarted(_)
            )),
            0,
            "no tool dispatch follows an expired invocation"
        );
        common::assert_single_terminal(&data, &control);
    });
}

#[test]
fn request_inspected_once_per_turn_with_effective_budget_and_additive_conversation() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![candidate(
                "item-0",
                "prov-ref-0",
                "host_read",
                r#"{"path":"src"}"#,
            )]),
            stop_turn("done"),
        ];
        let (provider, mut entries) = GatedProvider::new(script, false);
        let provider = Arc::new(provider);
        let config = common::quick_config();
        let effective_budget = config.limits.max_tool_output_bytes;
        let mut bed = review_bed(config, provider.clone(), tools());
        let response = bed
            .runtime
            .submit(common::submit_cmd("request-inspection"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response.run().cloned().expect("run issued");

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(provider.call_count(), 2, "one invocation per model turn");

        let first = next_entry(&mut entries).await;
        let second = next_entry(&mut entries).await;
        assert!(
            entries.try_recv().is_err(),
            "no duplicate invocation beyond the two turns"
        );

        for (label, request) in [("first", &first.request), ("second", &second.request)] {
            assert_eq!(request.run(), &run, "{label} request owns the run");
            assert_eq!(request.profile(), "m0-test", "{label} profile");
            assert_eq!(
                request.output_budget_bytes(),
                effective_budget,
                "{label} request carries the effective output bound"
            );
            let names: Vec<&str> = request
                .enabled_tools()
                .iter()
                .map(|tool| tool.name())
                .collect();
            assert!(
                names.contains(&"host_read") && names.contains(&"host_write"),
                "{label} request enables the registered tools: {names:?}"
            );
            // Additive tool definitions never exceed the enabled set; when
            // the runtime installs them they must match exactly.
            let definitions: Vec<&str> = request
                .tool_definitions()
                .iter()
                .map(|spec| spec.id().name())
                .collect();
            assert!(
                definitions.len() <= names.len(),
                "{label} definitions stay within the enabled bound: {definitions:?}"
            );
            if !definitions.is_empty() {
                for name in &definitions {
                    assert!(names.contains(name), "{label} definition {name} is enabled");
                }
            }
        }
        assert_ne!(
            first.request.turn(),
            second.request.turn(),
            "each turn has its own request identity"
        );

        let first_conversation = first.request.conversation();
        assert_eq!(
            conversation_count(first_conversation, |item| matches!(
                item,
                ModelContextItem::UserText(_)
            )),
            1,
            "the submitted task is inspected once on the first request: {first_conversation:?}"
        );
        assert_eq!(
            conversation_count(first_conversation, |item| matches!(
                item,
                ModelContextItem::ToolResult { .. }
            )),
            0,
            "no result exists before the first turn"
        );

        let started_call = control
            .iter()
            .find_map(|event| match event.payload() {
                nexus_core::EventPayload::ToolStarted(info) => Some(info.call.clone()),
                _ => None,
            })
            .expect("admitted call started");
        let second_conversation = second.request.conversation();
        assert_eq!(
            conversation_count(second_conversation, |item| matches!(
                item,
                ModelContextItem::UserText(_)
            )),
            1,
            "the task appears exactly once in the follow-up request: {second_conversation:?}"
        );
        assert_eq!(
            conversation_count(second_conversation, |item| matches!(
                item,
                ModelContextItem::AssistantCall { .. }
            )),
            1,
            "the admitted call appears exactly once: {second_conversation:?}"
        );
        let result = second_conversation
            .iter()
            .find_map(|item| match item {
                ModelContextItem::ToolResult { call, outcome, .. } => {
                    Some((call.clone(), outcome.status()))
                }
                _ => None,
            })
            .expect("the recorded tool result is carried additively");
        assert_eq!(
            result.0, started_call,
            "result correlates to the admitted call"
        );
        assert_eq!(
            result.1,
            nexus_core::ExecutionStatus::Succeeded,
            "result carries the observed outcome"
        );
        let _ = data;
    });
}

fn conversation_count(
    items: &[ModelContextItem],
    matches: impl Fn(&ModelContextItem) -> bool,
) -> usize {
    items.iter().filter(|item| matches(item)).count()
}
