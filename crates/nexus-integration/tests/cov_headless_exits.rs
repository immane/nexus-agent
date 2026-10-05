#![forbid(unsafe_code)]

//! Hardening for the M0 headless contract, modeled on
//! `history_and_headless.rs`, against the real components.
//!
//! The contract under test:
//! - every terminal run outcome reaches the process through a distinct exit
//!   shape. `Completed` splits on whether any call was denied, `Refused` and
//!   `Failed` share the failed code deliberately, and `Cancelled` and
//!   `LimitReached` keep codes no other outcome can claim. The denied-call
//!   count only ever moves the completed shape; it never rewrites the
//!   cancelled, refused, failed, or limit shapes.
//! - the cancellation and limit outcomes used for the mapping are produced by
//!   the real runtime here (a cancelled approval wait and an exhausted
//!   per-run tool-call budget), so the mapping is checked against outcomes the
//!   runtime actually publishes rather than only against literals.
//! - stdout carries machine records only: the revision prefix, `key=value`
//!   fields with non-empty keys, percent-encoded values, no diagnostics, no
//!   escape codes, no line breaks, and no argv echo. The banner and usage hint
//!   exist only on stderr and never appear in any record.
//! - history commands are rejected by the M0 runtime without starting work:
//!   no run is issued, no event is published, no provider turn or tool
//!   execution is consumed, and the slot stays idle for a normal submission.
//!
//! Determinism: fixed scripts and budgets; the only waits are the shared
//! drain backstops. `run_task` builds its own internal runtime, so the
//! headless-report tests stay synchronous and never nest a `block_on`.

mod common;

use nexus_core::{
    CancelCommand, Command, CommandReply, ErrorCategory, EventPayload, ExecutionStatus, Limits,
    ListSessionsCommand, RequestId, RestoreSessionCommand, RunOutcome, SessionId,
};
use nexus_fakes::{stop_turn, tool_turn};
use nexus_runtime::{Policy, RuntimeConfig};

/// Reads the value of `key` from a `key=value` stdout record.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split(' ')
        .skip(1)
        .filter_map(|token| token.split_once('='))
        .find_map(|(name, value)| (name == key).then_some(value))
}

/// Returns the single trailing result record, which must be the last line.
fn result_line(lines: &[String]) -> &str {
    let line = lines.last().expect("stdout carries a result record");
    assert!(
        line.starts_with(&format!("rev={} type=result ", nexus_headless::OUTPUT_REV)),
        "the last record is the result record: {line}"
    );
    line
}

/// Returns the `outcome=` token of the result record.
fn result_outcome(line: &str) -> &str {
    field(line, "outcome").expect("result records carry an outcome")
}

/// Asserts the result record's counters agree with the report counters and
/// with the exit code the mapping derived, so the machine record and the
/// process exit can never disagree.
fn assert_result_agrees_with_report(report: &nexus_headless::HeadlessReport) {
    let line = result_line(&report.lines);
    assert_eq!(
        lines_of_kind(&report.lines, "run-finished"),
        1,
        "exactly one terminal record: {line}"
    );
    assert_eq!(
        result_outcome(line),
        nexus_headless::outcome_name(report.outcome),
        "result outcome matches the reported outcome: {line}"
    );
    assert_eq!(
        field(line, "denied").expect("result carries denied"),
        report.denied_calls.to_string()
    );
    assert_eq!(
        field(line, "tool-started").expect("result carries tool-started"),
        report.tool_started.to_string()
    );
    assert_eq!(
        field(line, "tool-finished").expect("result carries tool-finished"),
        report.tool_finished.to_string()
    );
    assert_eq!(
        field(line, "run").expect("result carries the run"),
        report.run
    );
    assert_eq!(
        nexus_headless::exit_code_for(report.outcome, report.denied_calls),
        report.exit_code,
        "the exit code is the mapping of the recorded outcome and denial count"
    );
}

/// Counts stdout records of one event kind (`kind=<name>`).
fn lines_of_kind(lines: &[String], kind: &str) -> usize {
    lines
        .iter()
        .filter(|line| field(line, "kind") == Some(kind))
        .count()
}

/// Asserts every stdout record is a machine record: revision prefix,
/// `key=value` fields with non-empty keys, fully percent-encoded values, no
/// escape code, no line break, and no stderr diagnostic.
fn assert_stdout_hygiene(lines: &[String]) {
    assert!(!lines.is_empty(), "stdout records exist");
    for line in lines {
        assert!(
            line.starts_with(&format!("rev={} ", nexus_headless::OUTPUT_REV)),
            "every record carries the revision: {line}"
        );
        assert!(!line.contains('\x1b'), "no escape codes: {line}");
        assert!(!line.contains(['\n', '\r']), "one record per line: {line}");
        assert!(!line.contains("FAKE"), "no banner leakage: {line}");
        assert!(!line.contains("usage:"), "no usage-hint leakage: {line}");
        assert!(
            !line.contains("nexus-headless"),
            "no stderr preamble: {line}"
        );
        for token in line.split(' ').skip(1) {
            let (key, value) = token.split_once('=').expect("key=value fields");
            assert!(!key.is_empty(), "non-empty field key: {line}");
            assert_encoded_value(value, line);
        }
    }
}

/// Every value byte is either RFC 3986 unreserved or the start of a complete
/// `%XX` escape, so no raw separator or control byte can survive encoding.
fn assert_encoded_value(value: &str, line: &str) {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let pair = value.get(index + 1..index + 3).expect("complete escape");
            assert!(
                pair.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "escape uses hex digits: {line}"
            );
            index += 3;
        } else {
            assert!(
                bytes[index].is_ascii_alphanumeric()
                    || matches!(bytes[index], b'-' | b'.' | b'_' | b'~'),
                "unreserved byte required: {line}"
            );
            index += 1;
        }
    }
}

/// A completed run and a run that hit the headless denial path share the
/// `Completed` outcome but must reach the caller through different exit
/// shapes, with the denial recorded only in stdout records.
#[test]
fn completed_and_denied_headless_reports_have_distinct_exit_shapes() {
    let completed = nexus_headless::run_task("hello").expect("completed script runs");
    assert_eq!(completed.outcome, RunOutcome::Completed);
    assert_eq!(completed.exit_code, 0);
    assert_eq!(completed.denied_calls, 0);
    assert_eq!(completed.tool_started, 1);
    assert_eq!(completed.tool_finished, 1);
    assert_eq!(completed.approval_required, 0);
    assert!(!completed.truncated, "a small run fits the report budget");
    assert_result_agrees_with_report(&completed);
    assert_stdout_hygiene(&completed.lines);
    assert_eq!(
        result_outcome(result_line(&completed.lines)),
        "completed",
        "the completed shape names itself"
    );

    let denied = nexus_headless::run_task("deny: write it").expect("denial script runs");
    assert_eq!(denied.outcome, RunOutcome::Completed);
    assert_eq!(denied.exit_code, 3);
    assert_eq!(denied.denied_calls, 1);
    assert_eq!(denied.tool_started, 0, "a denied call never starts");
    assert_eq!(denied.approval_required, 0, "no handler consumes approvals");
    assert_eq!(denied.tool_finished, 1);
    assert_result_agrees_with_report(&denied);
    assert_stdout_hygiene(&denied.lines);

    // Same run-level outcome, deliberately different exit shapes.
    assert_eq!(completed.outcome, denied.outcome);
    assert_ne!(completed.exit_code, denied.exit_code);
    assert_eq!(
        field(result_line(&denied.lines), "denied"),
        Some("1"),
        "the denial is recorded in the machine record"
    );
    assert_eq!(field(result_line(&completed.lines), "denied"), Some("0"));
    let tool_finished = denied
        .lines
        .iter()
        .find(|line| field(line, "kind") == Some("tool-finished"))
        .expect("denial recorded in stdout");
    assert!(tool_finished.contains("status=denied"), "{tool_finished}");
    assert!(
        tool_finished.contains("effect=not-started"),
        "{tool_finished}"
    );
    assert_eq!(
        lines_of_kind(&denied.lines, "tool-started"),
        0,
        "no started call on the denial path: {:?}",
        denied.lines
    );
}

/// A refusal ends the run as `Refused` with the failed exit code, and it
/// touches no tool lifecycle: the refused shape never looks like the denial
/// shape or a completed one.
#[test]
fn refusal_headless_report_uses_the_failed_exit_shape() {
    let refused = nexus_headless::run_task("refuse: no thanks").expect("refusal script runs");
    assert_eq!(refused.outcome, RunOutcome::Refused);
    assert_eq!(refused.exit_code, 1);
    assert_eq!(refused.denied_calls, 0);
    assert_eq!(refused.tool_started, 0);
    assert_eq!(refused.tool_finished, 0);
    assert_eq!(refused.approval_required, 0);
    assert_result_agrees_with_report(&refused);
    assert_stdout_hygiene(&refused.lines);

    let result = result_line(&refused.lines);
    assert_eq!(result_outcome(result), "refused");
    for kind in ["tool-started", "tool-finished", "approval-required"] {
        assert_eq!(
            lines_of_kind(&refused.lines, kind),
            0,
            "refusal records no {kind} record"
        );
    }
    let terminal = refused
        .lines
        .iter()
        .find(|line| field(line, "kind") == Some("run-finished"))
        .expect("terminal record");
    assert!(terminal.contains("outcome=refused"), "{terminal}");
    assert!(
        terminal.contains("error=none"),
        "a refusal is not an operation failure: {terminal}"
    );
}

/// Cancellation and limit exhaustion come from the real runtime, and each maps
/// to an exit code and wire outcome that no other terminal outcome claims.
#[test]
fn runtime_cancelled_and_limit_outcomes_map_to_distinct_exit_shapes() {
    let rt = common::test_rt();
    let (cancelled, limit) = rt.block_on(async {
        // Cancelled: cancel while the run waits on an undecided approval. The
        // runtime publishes `Cancelled` without dispatching the call.
        let mut bed = common::make_bed(
            vec![
                tool_turn(vec![common::candidate("host_write", r#"{"path":"dst"}"#)]),
                stop_turn("never requested after cancellation"),
            ],
            common::quick_config(),
        );
        let response = bed
            .runtime
            .submit(common::submit_cmd("headless-cancel"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let run = response
            .run()
            .cloned()
            .expect("accepted submit issues a run");
        let approvals = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        common::find_approval(&approvals).expect("approval requested before cancellation");
        let cancel = bed
            .runtime
            .cancel(CancelCommand {
                request: RequestId::new("req-headless-cancel").expect("valid"),
                run: run.clone(),
            })
            .await;
        assert_eq!(cancel.reply(), CommandReply::Accepted);
        let (data, mut control, cancelled) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        control.extend(approvals);
        assert_eq!(
            cancelled.outcome(),
            RunOutcome::Cancelled,
            "the real runtime publishes a cancelled run"
        );
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::ToolStarted(_)
            )),
            0,
            "a cancelled run dispatches nothing"
        );
        common::assert_single_terminal(&data, &control);
        common::assert_contiguous(&data, &control);

        // LimitReached: two calls proposed in one turn against a per-run
        // budget of one. The runtime exhausts the budget and publishes a
        // typed `ResourceLimit` terminal.
        let mut limits = Limits::m0_test();
        limits.max_tool_calls_per_run = 1;
        let mut bed = common::make_bed(
            vec![tool_turn(vec![
                common::candidate("host_read", r#"{"path":"src"}"#),
                common::candidate_with("item-1", "prov-ref-1", "host_read", r#"{"path":"other"}"#),
            ])],
            RuntimeConfig {
                limits,
                policy: Policy::m0_test(),
                has_approval_handler: false,
            },
        );
        let response = bed
            .runtime
            .submit(common::submit_cmd("headless-limit"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        let (data, control, limit) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(
            limit.outcome(),
            RunOutcome::LimitReached,
            "the real runtime publishes an exhausted budget"
        );
        let error = limit
            .error()
            .expect("limit exhaustion carries a typed error");
        assert_eq!(error.category(), ErrorCategory::ResourceLimit);
        assert_eq!(
            bed.read_tool.execution_count(),
            0,
            "budget exhaustion at admission dispatches nothing"
        );
        assert_eq!(
            bed.provider.call_count(),
            1,
            "the next model turn is never requested"
        );
        let statuses: Vec<ExecutionStatus> = data
            .iter()
            .chain(control.iter())
            .filter_map(|event| match event.payload() {
                EventPayload::ToolFinished(info) => Some(info.outcome.status()),
                _ => None,
            })
            .collect();
        assert!(
            statuses.is_empty(),
            "no tool outcome is fabricated for the discarded candidate: {statuses:?}"
        );
        assert_eq!(
            common::count_payload(&data, &control, |payload| matches!(
                payload,
                EventPayload::ToolStarted(_)
            )),
            0,
            "no call starts once the budget is exhausted"
        );
        common::assert_single_terminal(&data, &control);
        common::assert_contiguous(&data, &control);

        (cancelled, limit)
    });

    assert_eq!(cancelled.outcome(), RunOutcome::Cancelled);
    assert_eq!(nexus_headless::exit_code_for(cancelled.outcome(), 0), 4);
    assert_eq!(
        nexus_headless::outcome_name(cancelled.outcome()),
        "cancelled"
    );

    assert_eq!(limit.outcome(), RunOutcome::LimitReached);
    assert_eq!(nexus_headless::exit_code_for(limit.outcome(), 0), 5);
    assert_eq!(
        nexus_headless::outcome_name(limit.outcome()),
        "limit-reached"
    );

    // A denial count never rewrites a non-completed shape.
    for outcome in [RunOutcome::Cancelled, RunOutcome::LimitReached] {
        for denied in [0, 1, 7] {
            assert_eq!(
                nexus_headless::exit_code_for(outcome, denied),
                nexus_headless::exit_code_for(outcome, 0),
                "{outcome:?} ignores the denial count (denied={denied})"
            );
        }
    }
}

/// Every exit code in the documented headless surface is either distinct or
/// shares a code only where the contract says it does.
#[test]
fn headless_exit_codes_cover_every_outcome_with_documented_collisions() {
    // Distinct shapes: completed success, headless denial, cancelled, limit.
    let distinct = [
        nexus_headless::exit_code_for(RunOutcome::Completed, 0),
        nexus_headless::exit_code_for(RunOutcome::Completed, 1),
        nexus_headless::exit_code_for(RunOutcome::Cancelled, 0),
        nexus_headless::exit_code_for(RunOutcome::LimitReached, 0),
    ];
    for (index, code) in distinct.iter().enumerate() {
        for other in &distinct[index + 1..] {
            assert_ne!(code, other, "distinct exit shapes: {distinct:?}");
        }
    }

    // Documented collision: a refused or failed run shares the failed code
    // with an operational entry-point failure, and no code is reused for a
    // usage mistake or a closed stdout consumer.
    let failed = nexus_headless::exit_code_for(RunOutcome::Failed, 0);
    assert_eq!(
        failed,
        nexus_headless::exit_code_for(RunOutcome::Refused, 0)
    );
    assert_eq!(failed, nexus_headless::HeadlessError::OPERATION_EXIT_CODE);
    let usage = nexus_headless::HeadlessError::USAGE_EXIT_CODE;
    let closed = nexus_headless::EXIT_STDOUT_CLOSED;
    for code in distinct.iter().copied().chain([failed]) {
        assert_ne!(code, usage, "usage mistakes keep their own code: {code}");
        assert_ne!(code, closed, "a closed consumer keeps its own code: {code}");
    }

    // Every outcome has a distinct lowercase wire name, so a parser can never
    // confuse two terminal shapes by name.
    let names = [
        nexus_headless::outcome_name(RunOutcome::Completed),
        nexus_headless::outcome_name(RunOutcome::Refused),
        nexus_headless::outcome_name(RunOutcome::Failed),
        nexus_headless::outcome_name(RunOutcome::Cancelled),
        nexus_headless::outcome_name(RunOutcome::LimitReached),
    ];
    for (index, name) in names.iter().enumerate() {
        assert!(
            name.bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'-'),
            "wire names are lowercase and dash-separated: {name}"
        );
        assert!(
            !names[index + 1..].contains(name),
            "outcome names are distinct: {names:?}"
        );
    }
}

/// The stderr-only diagnostics exist to be separated, and no stdout record
/// from any headless outcome echoes the argv or either diagnostic.
#[test]
fn headless_stdout_records_carry_no_diagnostics_escapes_or_argv() {
    for diagnostic in [nexus_headless::FAKE_BANNER, nexus_headless::USAGE] {
        assert!(
            !diagnostic.is_empty(),
            "stderr diagnostics exist to separate"
        );
        assert!(
            !diagnostic.contains(&format!("rev={}", nexus_headless::OUTPUT_REV)),
            "no diagnostic impersonates a machine record: {diagnostic}"
        );
    }

    // The argv carries separators and a `key=value` pair: if it were echoed
    // verbatim, a record would gain a field with an empty key or a value
    // holding a raw space.
    let sentinel = "ARGV_SENTINEL payload=x";
    let completed = nexus_headless::run_task(sentinel).expect("completed script runs");
    assert_eq!(completed.exit_code, 0);
    assert_result_agrees_with_report(&completed);
    assert_stdout_hygiene(&completed.lines);
    for line in &completed.lines {
        assert!(
            !line.contains("ARGV_SENTINEL"),
            "argv never reaches stdout: {line}"
        );
    }

    let denied = nexus_headless::run_task("deny: write it").expect("denial script runs");
    let refused = nexus_headless::run_task("refuse: no thanks").expect("refusal script runs");
    for report in [&denied, &refused] {
        assert_result_agrees_with_report(report);
        assert_stdout_hygiene(&report.lines);
    }

    // Records are sequenced, and the result record is the only trailing one.
    let mut previous = None;
    for line in completed.lines.iter().filter(|l| field(l, "seq").is_some()) {
        let seq: u64 = field(line, "seq")
            .expect("event records carry a sequence")
            .parse()
            .expect("sequences are decimal");
        if let Some(previous) = previous {
            assert!(
                seq > previous,
                "event sequences ascend: {seq} after {previous}"
            );
        }
        previous = Some(seq);
    }
    assert_eq!(
        completed
            .lines
            .iter()
            .filter(|line| field(line, "type") == Some("result"))
            .count(),
        1,
        "exactly one result record"
    );
    result_line(&completed.lines);
}

/// History commands are rejected by the M0 runtime without starting work: no
/// run is issued, nothing is published, no provider turn or tool execution is
/// consumed, and the slot stays idle for the next submission.
#[test]
fn runtime_rejects_history_commands_without_starting_work() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut bed = common::make_bed(vec![stop_turn("done")], common::quick_config());

        let list_request = RequestId::new("req-list").expect("valid");
        let (list_reply, list_snapshot) = bed
            .runtime
            .handle(Command::ListSessions(
                ListSessionsCommand::new(list_request.clone(), 10).expect("list builds"),
            ))
            .await;
        assert_eq!(list_reply.reply(), CommandReply::Rejected);
        assert_eq!(list_reply.request(), &list_request, "reply is correlated");
        assert!(list_reply.run().is_none(), "no run is issued");
        assert!(list_snapshot.is_none(), "no listing payload is returned");

        let restore_request = RequestId::new("req-restore").expect("valid");
        let (restore_reply, restored) = bed
            .runtime
            .handle(Command::RestoreSession(RestoreSessionCommand {
                request: restore_request.clone(),
                session: SessionId::new("sess-history").expect("valid"),
            }))
            .await;
        assert_eq!(restore_reply.reply(), CommandReply::Rejected);
        assert_eq!(restore_reply.request(), &restore_request);
        assert!(restore_reply.run().is_none(), "no run is issued");
        assert!(restored.is_none(), "no history payload is returned");

        // Nothing was started, so nothing was published or consumed.
        assert!(
            bed.data.try_recv().is_err(),
            "a rejected history command publishes no data event"
        );
        assert!(
            bed.control.try_recv().is_err(),
            "a rejected history command publishes no control event"
        );
        assert_eq!(bed.provider.call_count(), 0, "no provider turn is served");
        assert_eq!(bed.read_tool.execution_count(), 0);
        assert_eq!(bed.write_tool.execution_count(), 0);
        assert!(bed.provider.requests().is_empty(), "no request was issued");

        // The slot is idle: a normal submission still starts and completes.
        let response = bed
            .runtime
            .submit(common::submit_cmd("after-history"))
            .await;
        assert_eq!(
            response.reply(),
            CommandReply::Accepted,
            "the rejected commands left no conflicting work"
        );
        let (data, control, finished) =
            common::drain_until_finished(&mut bed.data, &mut bed.control).await;
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        common::assert_single_terminal(&data, &control);
        common::assert_contiguous(&data, &control);
        assert_eq!(bed.provider.call_count(), 1, "exactly one run was started");
    });
}
