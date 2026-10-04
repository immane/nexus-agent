#![forbid(unsafe_code)]

//! Causal reconciliation, busy rejection, and terminal uniqueness against
//! the real runtime and real fakes.

mod common;

use nexus_core::{ApproveCommand, CommandReply, EventPayload, RequestId, RunOutcome};
use nexus_fakes::{stop_turn, tool_turn};

/// `Submit` acceptance reconciles with the `RunStarted` event carrying the
/// same host-issued run and the originating request identity.
#[test]
fn submit_reply_reconciles_with_run_started() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut bed = common::make_bed(vec![stop_turn("done")], common::quick_config());
        let command = common::submit_cmd("causal");
        let request = command.request.clone();
        let response = bed.runtime.submit(command).await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response
            .run()
            .cloned()
            .expect("accepted submit issues a run");

        let controls = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::RunStarted { .. })
        })
        .await;
        let started = controls.last().expect("run-started arrives");
        assert_eq!(started.run(), &run, "event reconciles with the reply run");
        match started.payload() {
            EventPayload::RunStarted { request: seen } => {
                assert_eq!(seen, &request, "event carries the originating request");
            }
            other => panic!("expected run-started, got {other:?}"),
        }

        let (data, rest, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        let _ = (data, rest);
    });
}

/// A second submit while a run is active is rejected busy and leaves the
/// active run undisturbed: it still reaches its approval and completes with
/// exactly one dispatch.
#[test]
fn second_submit_is_busy_without_disturbing_active() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![common::candidate("host_write", r#"{"path":"dst"}"#)]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, common::quick_config());
        let first = bed.runtime.submit(common::submit_cmd("busy-first")).await;
        assert_eq!(first.reply(), CommandReply::Accepted);
        let run = first.run().cloned().expect("run issued");

        let approvals = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let (approval, call) = common::find_approval(&approvals).expect("approval requested");

        let second = bed.runtime.submit(common::submit_cmd("busy-second")).await;
        assert_eq!(second.reply(), CommandReply::Busy);
        assert_eq!(second.run(), Some(&run), "busy names the active run");

        let reply = bed
            .runtime
            .approve(ApproveCommand {
                request: RequestId::new("req-busy-decide").expect("valid"),
                approval,
                run,
                call,
            })
            .await;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        let (data, mut control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(approvals);
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(bed.write_tool.execution_count(), 1);
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::ToolStarted(_)
            )),
            1
        );
    });
}

/// One run publishes exactly one `RunStarted` (first, sequence zero) and one
/// terminal `RunFinished` (last), with contiguous sequences and an outcome
/// for every started call.
#[test]
fn single_terminal_outcome_with_contiguous_sequences() {
    let rt = common::test_rt();
    rt.block_on(async {
        let script = vec![
            tool_turn(vec![common::candidate("host_read", r#"{"path":"src"}"#)]),
            stop_turn("done"),
        ];
        let mut bed = common::make_bed(script, common::quick_config());
        let response = bed.runtime.submit(common::submit_cmd("terminal")).await;
        assert_eq!(response.reply(), CommandReply::Accepted);

        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        common::assert_contiguous(&data, &control);

        let mut all: Vec<&nexus_core::RunEvent> = data.iter().chain(control.iter()).collect();
        all.sort_by_key(|event| event.seq());
        assert!(
            matches!(
                all.first().expect("events").payload(),
                EventPayload::RunStarted { .. }
            ),
            "first event opens the run"
        );
        assert_eq!(all.first().expect("events").seq(), 0);
        assert!(all.last().expect("events").is_terminal());
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::RunStarted { .. }
            )),
            1,
            "exactly one run-started"
        );
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::RunFinished(_)
            )),
            1,
            "exactly one terminal outcome"
        );

        let started: Vec<nexus_core::CallId> = control
            .iter()
            .filter_map(|event| match event.payload() {
                EventPayload::ToolStarted(info) => Some(info.call.clone()),
                _ => None,
            })
            .collect();
        let finished_calls: Vec<nexus_core::CallId> = control
            .iter()
            .filter_map(|event| match event.payload() {
                EventPayload::ToolFinished(info) => Some(info.call.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(started.len(), 1);
        assert_eq!(started, finished_calls, "every start has its outcome");
        assert_eq!(bed.read_tool.execution_count(), 1);
    });
}
