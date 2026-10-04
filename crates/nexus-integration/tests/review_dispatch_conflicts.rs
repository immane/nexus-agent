#![forbid(unsafe_code)]

//! Review regressions: dispatch conflicts and effective budgets.
//!
//! Findings under test:
//! - a tool worker that outlived its per-tool timeout is still live; the
//!   runtime must not dispatch the next queued call into a conflicting
//!   concurrent execution;
//! - candidates denied at admission (unknown tool, invalid arguments) still
//!   consume the per-run call budget, so an adversarial turn cannot exceed
//!   the locked bound with denials;
//! - the effective `max_tool_output_bytes` bound reaches both ports and
//!   bounds recorded outcomes, not only the M0 constant.
//!
//! The gated tool is test-local (see `review_gates`): the shared fakes do
//! not gate a tool worker yet.

mod common;

#[path = "review_gates/mod.rs"]
mod gates;

use std::sync::Arc;
use std::time::Duration;

use gates::{GatedProvider, GatedTool, candidate, next_entry, review_bed, stop_turn, tool_turn};
use nexus_core::{CommandReply, EventPayload, ExecutionStatus, Limits, RunOutcome, ToolPort};
use nexus_runtime::{Policy, RuntimeConfig};

fn config_with(limits: Limits) -> RuntimeConfig {
    RuntimeConfig {
        limits,
        policy: Policy::m0_test(),
        has_approval_handler: true,
    }
}

#[test]
fn timed_out_worker_still_live_blocks_conflicting_next_dispatch() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = Limits::m0_test();
        limits.per_tool_timeout = Duration::from_millis(50);
        let (tool, mut entries) = GatedTool::new("host_read", "gated ok", false, true);
        let tool = Arc::new(tool);
        let (provider, _provider_entries) = GatedProvider::new(
            vec![
                tool_turn(vec![
                    candidate("item-0", "prov-ref-0", "host_read", r#"{"path":"a"}"#),
                    candidate("item-1", "prov-ref-1", "host_read", r#"{"path":"b"}"#),
                ]),
                stop_turn("done"),
            ],
            false,
        );
        let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![tool.clone()];
        let mut bed = review_bed(config_with(limits), Arc::new(provider), tools);
        let response = bed
            .runtime
            .submit(common::submit_cmd("timeout-worker"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let prelude = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ToolFinished(_))
        })
        .await;
        let timed_out = prelude
            .iter()
            .find_map(|event| match event.payload() {
                EventPayload::ToolFinished(info) => Some(info.outcome.status()),
                _ => None,
            })
            .expect("timeout outcome recorded");
        assert_eq!(
            timed_out,
            ExecutionStatus::TimedOut,
            "first call hits its per-tool deadline"
        );
        assert_eq!(tool.live(), 1, "the timed-out worker is still running");

        // While that worker is still live, a second `ToolStarted` means the
        // runtime started a conflicting execution.
        let mut control = prelude;
        let (second_started_while_live, early_terminal) =
            tokio::time::timeout(Duration::from_millis(200), async {
                loop {
                    match bed.control.recv().await {
                        Some(event) => {
                            let terminal = match event.payload() {
                                EventPayload::RunFinished(finished) => Some(finished.clone()),
                                _ => None,
                            };
                            let started =
                                matches!(event.payload(), EventPayload::ToolStarted(_));
                            control.push(event);
                            if let Some(finished) = terminal {
                                break (false, Some(finished));
                            }
                            if started {
                                break (true, None);
                            }
                        }
                        None => break (false, None),
                    }
                }
            })
            .await
            .unwrap_or((false, None));

        // Release the worker so its termination is established and the
        // runtime's ownership of it can end.
        let first = next_entry(&mut entries).await;
        first.release();
        let finished = match early_terminal {
            Some(finished) => finished,
            None => tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    tokio::select! {
                        entry = entries.recv() => {
                            if let Some(entry) = entry {
                                entry.release();
                            }
                        }
                        event = bed.control.recv() => {
                            match event {
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
                                None => panic!("control channel closed before terminal"),
                            }
                        }
                    }
                }
            })
            .await
            .expect("run reaches terminal after the worker is released"),
        };
        while let Ok(event) = bed.data.try_recv() {
            drop(event);
        }

        assert!(
            !second_started_while_live,
            "the next call dispatched while the timed-out worker still owned the tool"
        );
        assert_eq!(
            finished.outcome(),
            RunOutcome::LimitReached,
            "a timeout whose termination is unconfirmed is an explicit limit outcome"
        );
        assert_eq!(
            tool.execution_count(),
            1,
            "the queued second call never dispatches into a live timed-out worker"
        );
        assert_eq!(
            tool.max_live(),
            1,
            "tool executions never overlap across a timeout"
        );
        let statuses: Vec<ExecutionStatus> = control
            .iter()
            .filter_map(|event| match event.payload() {
                EventPayload::ToolFinished(info) => Some(info.outcome.status()),
                _ => None,
            })
            .collect();
        assert_eq!(
            statuses,
            vec![ExecutionStatus::TimedOut],
            "the timed-out call keeps its honest outcome"
        );

        // Ownership is released only after the worker actually terminates, so
        // a new run becomes admissible again once that is established.
        let next = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let response = bed.runtime.submit(common::submit_cmd("after-timeout")).await;
                if response.reply() == CommandReply::Accepted {
                    break response;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("quarantine clears after the worker terminates");
        assert_eq!(next.reply(), CommandReply::Accepted);
        let (_, _, next_finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(next_finished.outcome(), RunOutcome::Completed);
    });
}

#[test]
fn unknown_tool_denial_consumes_the_run_call_budget() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = Limits::m0_test();
        limits.max_tool_calls_per_run = 1;
        let script = vec![
            tool_turn(vec![
                candidate("item-0", "prov-ref-0", "host_missing", r#"{"path":"a"}"#),
                candidate("item-1", "prov-ref-1", "host_read", r#"{"path":"b"}"#),
            ]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, config_with(limits));
        let response = bed
            .runtime
            .submit(common::submit_cmd("denied-unknown-budget"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::LimitReached,
            "the denied candidate consumed the single admitted call slot"
        );
        assert_eq!(
            bed.read_tool.execution_count(),
            0,
            "the valid candidate never dispatches after budget exhaustion"
        );
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::ToolStarted(_)
            )),
            0,
            "no call started"
        );
        let statuses: Vec<ExecutionStatus> = control
            .iter()
            .filter_map(|event| match event.payload() {
                EventPayload::ToolFinished(info) => Some(info.outcome.status()),
                _ => None,
            })
            .collect();
        assert_eq!(statuses, vec![ExecutionStatus::Denied]);
        common::assert_single_terminal(&data, &control);
    });
}

#[test]
fn invalid_argument_denial_consumes_the_run_call_budget() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = Limits::m0_test();
        limits.max_tool_calls_per_run = 1;
        let script = vec![
            tool_turn(vec![
                candidate("item-0", "prov-ref-0", "host_read", "not-an-object"),
                candidate("item-1", "prov-ref-1", "host_read", r#"{"path":"b"}"#),
            ]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, config_with(limits));
        let response = bed
            .runtime
            .submit(common::submit_cmd("denied-args-budget"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            finished.outcome(),
            RunOutcome::LimitReached,
            "the malformed candidate consumed the single admitted call slot"
        );
        assert_eq!(bed.read_tool.execution_count(), 0);
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::ToolStarted(_)
            )),
            0
        );
        common::assert_single_terminal(&data, &control);
    });
}

#[test]
fn effective_output_budget_bounds_recorded_tool_outcome() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut limits = Limits::m0_test();
        limits.max_tool_output_bytes = 16;
        let (tool, mut entries) = GatedTool::new("host_read", &"x".repeat(32), false, false);
        let tool = Arc::new(tool);
        let (provider, mut provider_entries) = GatedProvider::new(
            vec![
                tool_turn(vec![candidate(
                    "item-0",
                    "prov-ref-0",
                    "host_read",
                    r#"{"path":"src"}"#,
                )]),
                stop_turn("done"),
            ],
            false,
        );
        let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![tool.clone()];
        let mut bed = review_bed(config_with(limits), Arc::new(provider), tools);
        let response = bed
            .runtime
            .submit(common::submit_cmd("effective-output-bound"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert!(
            matches!(
                finished.outcome(),
                RunOutcome::Completed | RunOutcome::LimitReached
            ),
            "an oversized outcome is either bounded or an explicit limit, not a silent success: {:?}",
            finished.outcome()
        );

        let request = next_entry(&mut provider_entries).await;
        assert_eq!(
            request.request.output_budget_bytes(),
            16,
            "the effective output bound reaches the provider request"
        );
        let execution = next_entry(&mut entries).await;
        assert_eq!(
            execution.context.output_budget_bytes(),
            16,
            "the effective output bound reaches the tool context"
        );

        let recorded = control
            .iter()
            .find_map(|event| match event.payload() {
                EventPayload::ToolFinished(info) => Some(info.outcome.clone()),
                _ => None,
            })
            .expect("tool outcome recorded");
        assert!(
            recorded.content().len() <= 16,
            "recorded outcome content {} exceeds the effective bound 16",
            recorded.content().len()
        );
        if recorded.content().len() == 16 && recorded.status() == ExecutionStatus::Succeeded {
            assert!(
                recorded.is_truncated(),
                "content cut to the effective bound is marked truncated"
            );
        }
        let _ = data;
    });
}
