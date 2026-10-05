#![forbid(unsafe_code)]

//! Public-surface top-up: the invariants that explain the private paths.
//!
//! Every record shape this crate can format has more arms than the M0
//! composition root can reach: no approval handler exists, the scripted fake
//! provider never emits call-preview or progress traffic, usage is always
//! final, the runtime store is always ephemeral, and the registered fake tools
//! only ever succeed or get denied. The private formatting and counting arms
//! behind those facts are covered by the crate's own `cov_topup_private`
//! module; this file pins the *public* half of the same contract so a change
//! that starts reaching one of the uncovered arms has to update a public test
//! instead of silently staying uncovered.
//!
//! The one production path this file drives directly is `write_report_lines`:
//! a writer whose `write` succeeds and whose `flush` fails must surface the
//! failure as an error, which is the case the in-crate `ClosedPipe` double
//! cannot reach (its `write` fails first).
//!
//! Deterministic: fixed scripts, fixed writers, no clock, filesystem,
//! subprocess, or randomness.

use std::io::{self, Write};

use nexus_core::commands::MAX_INPUT_BYTES;
use nexus_headless::{
    HeadlessReport, OUTPUT_REV, USAGE, percent_decode, run_task, sanitize, write_report_lines,
};

/// The unstable revision a parser must exact-match, written out literally so a
/// silent bump of `OUTPUT_REV` fails here instead of following the drift.
const WIRE_REV: &str = "m0-test-0";

/// The three task routings the composition root recognises: completed,
/// headless denial, and provider refusal.
const ROUTED_TASKS: [&str; 3] = ["hello", "deny: write it", "refuse: no"];

/// Kinds the formatter can emit that the M0 composition root can never
/// produce: there is no approval handler, and the scripted provider advertises
/// neither call-preview deltas nor tool progress.
const UNREACHABLE_KINDS: [&str; 3] = ["approval-required", "preview", "tool-output"];

/// Execution statuses a registered fake tool can report: it either succeeds
/// or is denied before dispatch. `failed`, `cancelled`, and `timed-out` need a
/// behaviour the composition root never registers.
const REACHABLE_STATUSES: [&str; 2] = ["succeeded", "denied"];

/// Effect states reachable through the same argument: a read is known not
/// applied, a write or command is known applied, and a denial never started.
const REACHABLE_EFFECTS: [&str; 3] = ["known-applied", "known-not-applied", "not-started"];

/// Every fake tool reports host-observed evidence; the other two classes need a
/// plugin or an inconclusive outcome.
const REACHABLE_EVIDENCE: [&str; 1] = ["host-observed"];

fn scripted(task: &str) -> HeadlessReport {
    run_task(task).unwrap_or_else(|error| panic!("{task} completes: {error}"))
}

/// Value of `key` in a space-separated `key=value` record. Values are
/// space-free by construction, so the token boundary is unambiguous.
fn field<'a>(line: &'a str, key: &str) -> &'a str {
    line.split(' ')
        .skip(1)
        .find_map(|token| token.strip_prefix(key)?.strip_prefix('='))
        .unwrap_or_else(|| panic!("{key} is missing from {line}"))
}

fn kind(line: &str) -> &str {
    field(line, "kind")
}

/// Retained event lines: every line except the trailing result record.
fn event_lines(report: &HeadlessReport) -> &[String] {
    let (result, events) = report
        .lines
        .split_last()
        .expect("every report ends with a result record");
    assert!(
        result.starts_with(&format!("rev={WIRE_REV} type=result ")),
        "the trailing record is the result: {result}"
    );
    events
}

/// Writer that accepts every line and then fails only on `flush`.
struct FailsOnFlush {
    bytes: usize,
}

impl Write for FailsOnFlush {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes += buffer.len();
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
    }
}

#[test]
fn write_report_lines_surfaces_a_flush_failure_after_every_line_was_written() {
    // The whole report reaches the consumer and only the final flush fails, so
    // the caller can still map the incomplete write instead of seeing a silent
    // success. `write_report_lines` never retries, flushes once, and hands the
    // error back unchanged.
    let report = scripted("hello");
    assert!(report.lines.len() > 1, "the scripted report has records");

    let mut writer = FailsOnFlush { bytes: 0 };
    let error = write_report_lines(&mut writer, &report.lines)
        .expect_err("a flush failure is surfaced, not swallowed");
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(
        writer.bytes,
        report
            .lines
            .iter()
            .map(|line| line.len() + 1)
            .sum::<usize>(),
        "every record was written before the flush failed"
    );

    // An empty slice still flushes, so a caller cannot mistake "nothing to
    // write" for "nothing to flush".
    let mut writer = FailsOnFlush { bytes: 0 };
    let error = write_report_lines(&mut writer, &[]).expect_err("an empty report still flushes");
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(writer.bytes, 0, "no line means no bytes");

    // The same lines reach an infallible sink unchanged, so the failing flush
    // above is the only difference between the two paths.
    let mut buffer: Vec<u8> = Vec::new();
    write_report_lines(&mut buffer, &report.lines).expect("report writes to a buffer");
    assert_eq!(
        buffer,
        report
            .lines
            .iter()
            .flat_map(|line| {
                line.as_bytes()
                    .iter()
                    .copied()
                    .chain(std::iter::once(b'\n'))
            })
            .collect::<Vec<u8>>(),
        "the flushed payload is exactly the retained lines, one per line"
    );
}

#[test]
fn no_public_run_emits_approval_preview_or_progress_records() {
    // The M0 composition root registers no approval handler, so a
    // confirmation-required call is denied before it is ever requested. This is
    // why the approval counter and the approval/preview/progress formatting
    // arms are only reachable from the crate's private tests: the public
    // routing can never produce one of those records, on any task.
    for task in ROUTED_TASKS {
        let report = scripted(task);
        assert_eq!(
            report.approval_required, 0,
            "{task}: no approval is ever requested"
        );
        for line in event_lines(&report) {
            assert!(
                !UNREACHABLE_KINDS.contains(&kind(line)),
                "{task} must not record {}: {line}",
                kind(line)
            );
            // The absence is a property of the routing, not of a run that
            // happened to be small, so the spine is still fully present.
            assert!(
                event_lines(&report)
                    .first()
                    .is_some_and(|first| kind(first) == "run-started"),
                "{task}: the run still opens with a start record"
            );
        }
    }
}

#[test]
fn every_public_run_reports_ephemeral_persistence_and_final_usage() {
    // The runtime store is ephemeral by construction and the scripted provider
    // only reports final usage, so `saved`, `save-failed`, and `provisional`
    // never appear on stdout. Keeping that exact means a future durable store
    // or a provisional usage update has to come with test updates here.
    for task in ROUTED_TASKS {
        let report = scripted(task);
        let terminal = event_lines(&report)
            .iter()
            .find(|line| kind(line) == "run-finished")
            .unwrap_or_else(|| panic!("{task}: every run retains its terminal record"));
        assert_eq!(
            field(terminal, "persistence"),
            "ephemeral",
            "{task}: the M0 store is ephemeral"
        );
        assert_eq!(
            field(terminal, "error"),
            "none",
            "{task}: a scripted run records no execution failure"
        );
        assert_eq!(
            field(terminal, "error-correlation"),
            "none",
            "{task}: correlation stays empty without an error"
        );

        let usage: Vec<&String> = event_lines(&report)
            .iter()
            .filter(|line| kind(line) == "usage")
            .collect();
        assert!(!usage.is_empty(), "{task}: usage is reported at least once");
        for line in usage {
            assert_eq!(
                field(line, "finality"),
                "final",
                "{task}: scripted usage is always final: {line}"
            );
        }
    }
}

#[test]
fn public_tool_records_stay_inside_the_reachable_status_effect_and_evidence_set() {
    // The registered fake tools succeed or get denied, never fail, time out, or
    // report cancellation, and they always report host-observed evidence. The
    // remaining status/effect/evidence arms are therefore private-only, and
    // this test is what makes that boundary explicit.
    for task in ROUTED_TASKS {
        let report = scripted(task);
        let finished: Vec<&String> = event_lines(&report)
            .iter()
            .filter(|line| kind(line) == "tool-finished")
            .collect();
        assert_eq!(
            finished.len(),
            report.tool_finished,
            "{task}: every retained outcome backs the counter"
        );

        for line in finished {
            let status = field(line, "status");
            assert!(
                REACHABLE_STATUSES.contains(&status),
                "{task}: unreachable status {status} in {line}"
            );
            assert!(
                REACHABLE_EFFECTS.contains(&field(line, "effect")),
                "{task}: unreachable effect in {line}"
            );
            let evidence = field(line, "evidence");
            assert!(
                REACHABLE_EVIDENCE.contains(&evidence),
                "{task}: unreachable evidence {evidence} in {line}"
            );
            assert_eq!(
                field(line, "truncated"),
                "false",
                "{task}: the fake tool output is not truncated: {line}"
            );
            // A denial never leaves a started record behind it.
            if status == "denied" {
                assert_eq!(
                    report.tool_started, 0,
                    "{task}: a denied call never started"
                );
                assert_eq!(report.denied_calls, 1, "{task}: the denial is counted once");
                assert_eq!(
                    field(line, "effect"),
                    "not-started",
                    "{task}: a denial implies no applied effect: {line}"
                );
            }
        }

        let started = event_lines(&report)
            .iter()
            .filter(|line| kind(line) == "tool-started")
            .count();
        assert_eq!(started, report.tool_started, "{task}: started counter");
    }
}

#[test]
fn the_public_report_contract_is_reproducible_for_every_routing() {
    // Usage text and the usage banner are public surface too: a parser reads
    // them, so their exact shape is pinned here rather than left implicit.
    for task in ROUTED_TASKS {
        let report = scripted(task);
        assert_eq!(OUTPUT_REV, WIRE_REV, "the wire revision does not drift");
        assert!(
            USAGE.contains("nexus-headless") && USAGE.contains("deny:"),
            "usage text still documents the routings: {USAGE}"
        );

        // The task text is never echoed, and every retained value still
        // decodes, so the report stays a reversible machine encoding.
        for line in &report.lines {
            assert!(line.starts_with(&format!("rev={WIRE_REV} ")), "{line}");
            assert!(
                line.is_ascii() && !line.contains(['\n', '\r', '\x1b']),
                "{line}"
            );
            for token in line.split(' ').skip(1) {
                let (key, value) = token.split_once('=').expect("key=value token");
                assert!(!key.is_empty(), "empty field key in {line}");
                percent_decode(value).unwrap_or_else(|error| panic!("{value} decodes: {error}"));
            }
        }
    }

    // The boundary of the input budget is exact and public: the last accepted
    // byte is reported, the next one is a usage refusal, and neither outcome
    // echoes the submitted text.
    let accepted = run_task(&"x".repeat(MAX_INPUT_BYTES)).expect("exact budget is accepted");
    assert!(
        !accepted.truncated,
        "a boundary run is still fully retained"
    );
    let refused = run_task(&"x".repeat(MAX_INPUT_BYTES + 1))
        .err()
        .expect("one byte past the budget is refused");
    assert!(refused.is_usage(), "{refused}");

    // The encoding primitives the report depends on stay reversible for the
    // exact inputs a record can carry.
    for raw in [
        "host_read",
        "{\"path\":\"dst\"}",
        "line\nbreak",
        "%25",
        "héllo 🌍",
    ] {
        let encoded = sanitize(raw);
        assert!(!encoded.contains([' ', '=', '\n', '\x1b']), "{encoded}");
        assert_eq!(percent_decode(&encoded).expect("round trip"), raw);
    }
}
