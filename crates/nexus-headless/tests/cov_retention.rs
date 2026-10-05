//! Public-boundary hardening for headless report retention.
//!
//! `nexus-headless` retains a bounded subset of formatted event lines: mandatory
//! control records (`run-started`, approvals, tool lifecycle, usage, terminal)
//! and presentation records (text, previews, progress) share one encoded-byte
//! budget but separate count budgets. These tests pin the part of that contract
//! that is observable from outside the crate, using only the public API:
//!
//! - every scripted run keeps a complete mandatory control spine, including
//!   while presentation records share the same report, and the report counters
//!   still agree with the retained control records;
//! - retained lines stay inside the published count and byte caps, and the
//!   written payload is exactly the retained encoded bytes;
//! - every retained value is a complete percent-encoded or unreserved token, so
//!   a retained line can never be re-split by its own content, and the reported
//!   raw lengths match the decoded values;
//! - approval argument previews and run-finished correlation pairs occupy one
//!   encoded token each, so no raw argument text or free-form error message can
//!   enter a retained line;
//! - the mandatory reserve cannot be exhausted by one M0 run (derived from the
//!   published `Limits`), and every refusal is an error value with its own exit
//!   code rather than a report that silently dropped a mandatory record.
//!
//! Retention internals (`Retention`, the record formatter) are private and are
//! covered by the crate's unit tests; a public test cannot add presentation
//! pressure, because the M0 scripts emit a fixed handful of records and the
//! task text is never echoed into a record. The checks below therefore pin the
//! boundary invariants a caller depends on, not the eviction order.
//!
//! No clocks, randomness, environment, filesystem, threads, or subprocesses are
//! involved: every assertion derives from one scripted run, so all checks are
//! deterministic.

#![forbid(unsafe_code)]

use nexus_core::{Limits, RunOutcome};
use nexus_headless::{
    EXIT_STDOUT_CLOSED, FAKE_BANNER, HeadlessError, HeadlessReport, MAX_MANDATORY_BYTES,
    MAX_MANDATORY_EVENTS, MAX_REPORT_BYTES, MAX_REPORT_EVENTS, OUTPUT_REV, USAGE, exit_code_for,
    outcome_name, percent_decode, run_task, sanitize, write_report_lines,
};

/// The unstable revision a parser must exact-match, pinned literally so a
/// silent bump of `OUTPUT_REV` cannot make these checks follow the drift.
const WIRE_REV: &str = "m0-test-0";

/// The published classification of retained records. Written out here from the
/// documented control/presentation split so a change inside the crate's private
/// classifier has to be re-argued in this file instead of being inherited.
const MANDATORY_KINDS: [&str; 6] = [
    "run-started",
    "approval-required",
    "tool-started",
    "tool-finished",
    "usage",
    "run-finished",
];

const PRESENTATION_KINDS: [&str; 3] = ["text", "preview", "tool-output"];

/// Every kind this crate can emit, so the sweep over a report rejects a kind
/// that neither list claims rather than quietly ignoring it.
const ALL_KINDS: [&str; 9] = [
    "run-started",
    "usage",
    "tool-started",
    "approval-required",
    "tool-output",
    "tool-finished",
    "preview",
    "text",
    "run-finished",
];

/// One scripted run of the fake-wired M0 composition root.
fn scripted(task: &str) -> HeadlessReport {
    run_task(task).expect("scripted run completes")
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

fn seq(line: &str) -> u64 {
    field(line, "seq")
        .parse()
        .unwrap_or_else(|error| panic!("seq is numeric in {line}: {error}"))
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

/// The documented lowercase wire token for an outcome, written out literally so
/// a drift in the crate's `outcome_name` table fails here instead of being
/// echoed back by the same table.
fn expected_outcome_name(outcome: RunOutcome) -> &'static str {
    match outcome {
        RunOutcome::Completed => "completed",
        RunOutcome::Refused => "refused",
        RunOutcome::Failed => "failed",
        RunOutcome::Cancelled => "cancelled",
        RunOutcome::LimitReached => "limit-reached",
    }
}

fn result_line(report: &HeadlessReport) -> &str {
    report
        .lines
        .last()
        .expect("every report ends with a result record")
}

/// Published classification of retained records: these kinds are mandatory
/// control content and are bounded only by the mandatory reserve.
fn is_mandatory_kind(kind: &str) -> bool {
    MANDATORY_KINDS.contains(&kind)
}

#[test]
fn the_wire_names_and_exit_codes_are_the_documented_tokens() {
    // Every outcome, including the two a scripted M0 run cannot reach, so a
    // drift in the public mapping tables cannot hide behind an unexercised arm.
    for (outcome, name, no_denials, with_denial) in [
        (RunOutcome::Completed, "completed", 0, 3),
        (RunOutcome::Refused, "refused", 1, 1),
        (RunOutcome::Failed, "failed", 1, 1),
        (RunOutcome::Cancelled, "cancelled", 4, 4),
        (RunOutcome::LimitReached, "limit-reached", 5, 5),
    ] {
        assert_eq!(outcome_name(outcome), name, "wire name for {outcome:?}");
        assert_eq!(outcome_name(outcome), expected_outcome_name(outcome));
        assert_eq!(exit_code_for(outcome, 0), no_denials, "exit for {name}");
        assert_eq!(exit_code_for(outcome, 7), with_denial, "exit for {name}");
    }

    // Denial only moves a completed run off success; it never turns a failure
    // into a success or a refusal into a denial code.
    assert_eq!(exit_code_for(RunOutcome::Completed, 1), 3);
    assert_eq!(exit_code_for(RunOutcome::Completed, usize::MAX), 3);
}

#[test]
fn the_published_kind_classification_covers_every_documented_kind() {
    assert_eq!(
        MANDATORY_KINDS.len() + PRESENTATION_KINDS.len(),
        ALL_KINDS.len(),
        "the two classes partition the documented kinds"
    );
    for kind in ALL_KINDS {
        assert!(
            MANDATORY_KINDS.contains(&kind) ^ PRESENTATION_KINDS.contains(&kind),
            "{kind} belongs to exactly one retention class"
        );
        assert!(
            documented_tail(kind).is_some(),
            "{kind} has a documented field set"
        );
    }
    assert!(
        documented_tail("not-a-kind").is_none(),
        "an unknown kind has no field set and must fail a report sweep"
    );
}

/// Tail field names of every documented record, after `rev`, `type`, `seq`,
/// `run`, and `kind`. A closed set per kind keeps the retained byte cost of a
/// line fully accounted; an undocumented kind fails the sweep instead of being
/// skipped.
fn documented_tail(kind: &str) -> Option<&'static [&'static str]> {
    let tail: &'static [&'static str] = match kind {
        "run-started" => &["request"],
        "usage" => &["input", "output", "finality"],
        "tool-started" => &["call"],
        "approval-required" => &["approval", "call", "scope", "args-preview"],
        "tool-output" => &["call", "truncated", "preview-len"],
        "tool-finished" => &[
            "call",
            "status",
            "effect",
            "evidence",
            "truncated",
            "content-len",
            "content",
        ],
        "preview" => &["item"],
        "text" => &["item", "text-len", "text"],
        "run-finished" => &["outcome", "persistence", "error", "error-correlation"],
        _ => return None,
    };
    Some(tail)
}

fn keys_of(line: &str) -> Vec<&str> {
    line.split(' ')
        .map(|token| token.split_once('=').expect("key=value token").0)
        .collect()
}

/// Every value byte is either unreserved or the start of a complete `%XX`
/// escape. This is what makes a retained line's byte count exact and stops a
/// value from inventing fields, lines, or escape sequences of its own.
fn assert_encoded_value(value: &str, line: &str) {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let escape = value
                .get(index + 1..index + 3)
                .unwrap_or_else(|| panic!("truncated escape in {line}"));
            assert!(
                escape.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "escape uses hex digits only: {line}"
            );
            index += 3;
        } else {
            assert!(
                bytes[index].is_ascii_alphanumeric()
                    || matches!(bytes[index], b'-' | b'.' | b'_' | b'~'),
                "value byte {:#04x} must be escaped: {line}",
                bytes[index]
            );
            index += 1;
        }
    }
}

/// One retained record: revision prefix, documented field set, ordered shape,
/// ASCII-only content, and a reversible value encoding.
fn assert_retained_record(line: &str) {
    let prefix = format!("rev={WIRE_REV} ");
    assert!(line.starts_with(&prefix), "record carries the rev: {line}");
    assert_eq!(OUTPUT_REV, WIRE_REV, "the wire revision does not drift");
    assert!(line.is_ascii(), "record is ASCII: {line}");
    assert!(
        !line.contains(['\x1b', '\n', '\r']),
        "single clean line: {line}"
    );

    let keys = keys_of(line);
    assert_eq!(&keys[..2], ["rev", "type"], "record prefix: {line}");
    assert_eq!(
        field(line, "type"),
        "event",
        "retained records are events: {line}"
    );
    let tail =
        documented_tail(kind(line)).unwrap_or_else(|| panic!("undocumented record kind in {line}"));
    let mut expected = vec!["rev", "type", "seq", "run", "kind"];
    expected.extend_from_slice(tail);
    assert_eq!(keys, expected, "closed field set for {}", kind(line));

    for token in line.split(' ').skip(1) {
        let (_, value) = token.split_once('=').expect("key=value token");
        assert_encoded_value(value, line);
    }
}

/// A value that must survive as exactly one field token: no raw separator, and
/// no raw `%` that could be mistaken for the start of an escape.
fn assert_single_token(value: &str, context: &str) {
    assert!(
        !value.contains([' ', '\n', '\r', '\x1b', '=', ',']),
        "{context} must stay one encoded token: {value}"
    );
    assert_encoded_value(value, context);
    percent_decode(value).unwrap_or_else(|error| panic!("{context} decodes: {error}"));
}

/// Strips the host-issued run and call identities, which the runtime mints per
/// incarnation, so two runs of one task can be compared field for field.
fn normalized_lines(report: &HeadlessReport) -> Vec<String> {
    let incarnation = report
        .run
        .strip_prefix('r')
        .and_then(|rest| rest.rsplit_once('-'))
        .unwrap_or_else(|| panic!("run id shape: {}", report.run))
        .0;
    report
        .lines
        .iter()
        .map(|line| line.replace(incarnation, "<incarnation>"))
        .collect()
}

fn encoded_len(lines: &[String]) -> usize {
    lines.iter().map(String::len).sum()
}

#[test]
fn every_scripted_run_keeps_the_complete_mandatory_control_spine() {
    // (task, mandatory kinds that must survive, kinds that must not appear)
    let paths: [(&str, &[&str], &[&str]); 3] = [
        (
            "hello",
            &[
                "run-started",
                "usage",
                "tool-started",
                "tool-finished",
                "run-finished",
            ],
            &["approval-required", "preview", "tool-output"],
        ),
        (
            "deny: write it",
            &["run-started", "usage", "tool-finished", "run-finished"],
            &[
                "tool-started",
                "approval-required",
                "preview",
                "tool-output",
            ],
        ),
        (
            "refuse: no",
            &["run-started", "usage", "run-finished"],
            &[
                "tool-started",
                "tool-finished",
                "approval-required",
                "preview",
                "tool-output",
            ],
        ),
    ];

    for (task, required, absent) in paths {
        let report = scripted(task);
        let events = event_lines(&report);
        assert!(!events.is_empty(), "a run retains records: {task}");

        let kinds: Vec<&str> = events.iter().map(|line| kind(line)).collect();
        // Every retained record is one of the documented kinds, and the set of
        // retained kinds is exactly the spine plus presentation: a mandatory
        // kind may not go missing and no extra kind may appear.
        assert_eq!(
            kinds.iter().filter(|kind| is_mandatory_kind(kind)).count(),
            required.len(),
            "{task} retains every mandatory kind once: {kinds:?}"
        );
        for required_kind in required {
            assert!(
                kinds.contains(required_kind),
                "{task} loses mandatory {required_kind}: {kinds:?}"
            );
        }
        for absent_kind in absent {
            assert!(
                !kinds.contains(absent_kind),
                "{task} must not record {absent_kind}: {kinds:?}"
            );
        }

        // The spine is ordered: run-started first, terminal record last.
        assert_eq!(kinds[0], "run-started", "{task} opens with run-started");
        assert_eq!(
            kinds[kinds.len() - 1],
            "run-finished",
            "{task} closes with the terminal record"
        );
        let sequences: Vec<u64> = events.iter().map(|line| seq(line)).collect();
        assert!(
            sequences.windows(2).all(|pair| pair[0] < pair[1]),
            "retained records keep run-sequence order: {sequences:?}"
        );

        // Nothing was dropped at this size, so the counters describe the whole
        // run and the terminal record agrees with the report-level outcome.
        assert!(!report.truncated, "{task} fits the report budgets");
        // `exit_code_for` must agree with the report, and the published wire
        // name of the outcome must be the exact documented token.
        assert_eq!(
            report.exit_code,
            exit_code_for(report.outcome, report.denied_calls)
        );
        assert_eq!(
            outcome_name(report.outcome),
            expected_outcome_name(report.outcome),
            "{task}: the wire name is the documented token"
        );
        assert_eq!(
            field(result_line(&report), "outcome"),
            outcome_name(report.outcome),
            "{task} result agrees with the report outcome"
        );
        assert_eq!(field(result_line(&report), "run"), report.run);
        for line in events {
            assert_retained_record(line);
        }
    }
}

#[test]
fn presentation_records_never_displace_mandatory_control_records() {
    let report = scripted("hello");
    let events = event_lines(&report);

    let mandatory: Vec<&String> = events
        .iter()
        .filter(|line| is_mandatory_kind(kind(line)))
        .collect();
    let presentation: Vec<&String> = events
        .iter()
        .filter(|line| !is_mandatory_kind(kind(line)))
        .collect();

    // Presentation shares the report, so the check is not vacuous: the
    // presentation record is retained next to, not instead of, control records.
    assert!(
        !presentation.is_empty(),
        "the completed path retains presentation text: {events:?}"
    );
    assert_eq!(
        mandatory.len() + presentation.len(),
        events.len(),
        "every retained record is classified"
    );

    // Counters are computed over observed events, so they must still match the
    // retained mandatory records after presentation retention.
    assert_eq!(
        mandatory
            .iter()
            .filter(|line| kind(line) == "tool-started")
            .count(),
        report.tool_started,
        "tool-started records back the counter"
    );
    assert_eq!(
        mandatory
            .iter()
            .filter(|line| kind(line) == "tool-finished")
            .count(),
        report.tool_finished,
        "tool-finished records back the counter"
    );
    assert_eq!(
        mandatory
            .iter()
            .filter(|line| kind(line) == "tool-finished" && field(line, "status") == "denied")
            .count(),
        report.denied_calls,
        "denied outcomes are counted from retained records"
    );
    assert_eq!(report.approval_required, 0, "no handler consumes approvals");
    assert_eq!(report.tool_started, 1, "the read tool ran exactly once");

    // The denial path is the interesting asymmetry: the denied call produces a
    // mandatory outcome record with no started record behind it.
    let denied = scripted("deny: write it");
    let denied_events = event_lines(&denied);
    let finished = denied_events
        .iter()
        .find(|line| kind(line) == "tool-finished")
        .expect("a denied call still records its outcome");
    assert_eq!(field(finished, "status"), "denied");
    assert_eq!(field(finished, "effect"), "not-started");
    assert_eq!(denied.denied_calls, 1);
    assert_eq!(denied.tool_started, 0);
    assert!(!denied.truncated, "the denial report is complete");
}

#[test]
fn retained_records_stay_within_the_published_count_and_byte_caps() {
    let report = scripted("hello");
    let events = event_lines(&report);

    let presentation = events
        .iter()
        .filter(|line| !is_mandatory_kind(kind(line)))
        .count();
    assert!(
        presentation <= MAX_REPORT_EVENTS,
        "presentation count cap: {presentation} > {MAX_REPORT_EVENTS}"
    );
    assert!(
        events.len() <= MAX_REPORT_EVENTS + MAX_MANDATORY_EVENTS,
        "event records use the presentation cap plus the mandatory reserve: {} > {}",
        events.len(),
        MAX_REPORT_EVENTS + MAX_MANDATORY_EVENTS
    );
    assert_eq!(
        report.lines.len(),
        events.len() + 1,
        "exactly one trailing result record"
    );

    // Caps are finite budgets, not placeholders that mean infinity.
    const {
        assert!(MAX_REPORT_EVENTS > 0 && MAX_REPORT_BYTES > 0);
        assert!(MAX_MANDATORY_EVENTS > 0 && MAX_MANDATORY_BYTES > 0);
        assert!(MAX_MANDATORY_BYTES > MAX_REPORT_BYTES);
        assert!(MAX_REPORT_BYTES >= Limits::M0_TEST_TOOL_OUTPUT_BYTES);
    }
    // The presentation count cap has to admit a whole M0 run's records, or a
    // complete run would report itself as truncated.
    let limits = Limits::m0_test();
    let worst_case_records = 2
        + limits.max_model_turns_per_run as usize
        + 3 * limits.max_tool_calls_per_run as usize
        + limits.max_tool_calls_per_run as usize;
    assert!(
        MAX_REPORT_EVENTS > worst_case_records,
        "the presentation count cap {MAX_REPORT_EVENTS} must admit an M0 worst case of \
         {worst_case_records} records"
    );

    let event_bytes = encoded_len(events);
    let mandatory_bytes: usize = events
        .iter()
        .filter(|line| is_mandatory_kind(kind(line)))
        .map(|line| line.len())
        .sum();
    assert!(
        event_bytes <= MAX_REPORT_BYTES || mandatory_bytes > MAX_REPORT_BYTES,
        "combined retained bytes fit the report budget: {event_bytes} > {MAX_REPORT_BYTES} \
         (mandatory {mandatory_bytes})"
    );
    assert_eq!(
        result_line(&report).len() + event_bytes,
        encoded_len(&report.lines),
        "byte accounting covers the appended result record too"
    );
}

#[test]
fn retention_byte_accounting_is_exact_through_the_public_writer() {
    let report = scripted("hello");
    let mut written = Vec::new();
    write_report_lines(&mut written, &report.lines).expect("report writes to a buffer");
    assert_eq!(
        written.len(),
        encoded_len(&report.lines) + report.lines.len(),
        "the writer adds only the trailing newline per retained record"
    );
    assert!(
        written.ends_with(b"\n"),
        "every record is newline terminated"
    );
    assert_eq!(
        String::from_utf8(written).expect("written report is utf-8"),
        format!("{}\n", report.lines.join("\n")),
        "the written payload is exactly the retained encoded lines, one per line"
    );
}

#[test]
fn retained_values_decode_and_match_their_reported_raw_lengths() {
    for task in ["hello", "deny: write it", "refuse: no"] {
        let report = scripted(task);
        for line in event_lines(&report) {
            // Every retained value is reversible, so the encoded line is a
            // faithful and lossless stand-in for the observed record.
            for token in line.split(' ').skip(1) {
                let (_, value) = token.split_once('=').expect("key=value token");
                percent_decode(value).unwrap_or_else(|error| panic!("{value} decodes: {error}"));
            }

            // Reported raw lengths describe the decoded payload, which is why
            // byte accounting has to run on the encoded line instead.
            if kind(line) == "tool-finished" {
                let content = field(line, "content");
                let raw_len: usize = field(line, "content-len")
                    .parse()
                    .unwrap_or_else(|error| panic!("content-len is numeric: {error}"));
                assert_eq!(
                    percent_decode(content).expect("content decodes").len(),
                    raw_len,
                    "content-len counts decoded bytes: {line}"
                );
                assert!(
                    content.len() >= raw_len,
                    "encoded content is never shorter than the raw payload: {line}"
                );
            }
            if kind(line) == "text" {
                let text = field(line, "text");
                let raw_len: usize = field(line, "text-len")
                    .parse()
                    .unwrap_or_else(|error| panic!("text-len is numeric: {error}"));
                assert_eq!(
                    percent_decode(text).expect("text decodes").len(),
                    raw_len,
                    "text-len counts decoded bytes: {line}"
                );
            }
        }
    }
}

#[test]
fn approval_args_preview_occupies_one_encoded_token() {
    // The preview is retained inside the approval line, so its bytes count
    // against the report budget and must not be able to add fields or lines.
    let preview = r#"{"path":"dst"}"#;
    let encoded = sanitize(preview);
    assert_eq!(encoded, "%7B%22path%22%3A%22dst%22%7D");
    assert_single_token(&encoded, "encoded args preview");
    assert_eq!(percent_decode(&encoded).expect("preview decodes"), preview);

    // The unreserved set passes through byte for byte, so an encoded preview
    // that needs no escaping is stored at its exact cost.
    for unreserved in ["host_read", "m0-test-grant", "a.b_c~d-0123456789", "ABCXYZ"] {
        assert_eq!(
            sanitize(unreserved),
            unreserved,
            "unreserved text is stored verbatim"
        );
    }
    let mixed = "a.b_c~d-0123456789ABCXYZ";
    assert_eq!(
        sanitize(mixed).len(),
        mixed.len(),
        "no expansion for unreserved"
    );

    // A preview that fights the line grammar is neutralised completely.
    let hostile = "{\"cmd\":\"a b=c%d\",\"note\":\"e\nf\"}\x1b";
    let encoded = sanitize(hostile);
    assert_single_token(&encoded, "encoded hostile preview");
    assert_eq!(percent_decode(&encoded).expect("preview decodes"), hostile);
    assert!(!encoded.contains('{'), "no raw argument byte survives");

    // No retained record ever carries raw argument bytes, on any scripted path.
    for task in ["hello", "deny: write it", "refuse: no"] {
        for line in event_lines(&scripted(task)) {
            assert!(
                !line.contains('{') && !line.contains('"') && !line.contains(':'),
                "raw argument text never enters a retained record: {line}"
            );
            // Nothing outside the documented field set smuggles a value in, so
            // an unencoded preview could not hide behind a new key either.
            assert_retained_record(line);
        }
    }
}

#[test]
fn run_finished_exposes_only_a_safe_terminal_record() {
    for task in ["hello", "deny: write it", "refuse: no"] {
        let report = scripted(task);
        let terminal = event_lines(&report)
            .iter()
            .find(|line| kind(line) == "run-finished")
            .expect("every run retains its terminal record");
        assert_eq!(
            field(terminal, "outcome"),
            expected_outcome_name(report.outcome),
            "{task}: terminal outcome agrees with the report"
        );
        assert!(
            ["ephemeral", "saved", "save-failed"].contains(&field(terminal, "persistence")),
            "{task}: persistence is a static category name: {terminal}"
        );
        let error = field(terminal, "error");
        assert!(
            error == "none"
                || error
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'-'),
            "{task}: the failure category is a static token, never free text: {terminal}"
        );
        let correlation = field(terminal, "error-correlation");
        assert_single_token(correlation, "encoded error correlation");
        assert_eq!(
            correlation, "none",
            "{task}: no correlation without an error"
        );
    }

    // Correlation pairs arrive pre-redacted and stay one token: the joined
    // `key=value` pairs are percent-encoded so `=` and `,` cannot be mistaken
    // for record separators, and a decoder recovers exactly the pairs.
    let pairs = "call=c1-0,scope=m0-test-grant";
    let encoded = sanitize(pairs);
    assert_eq!(encoded, "call%3Dc1-0%2Cscope%3Dm0-test-grant");
    assert_single_token(&encoded, "encoded correlation pairs");
    assert_eq!(
        percent_decode(&encoded).expect("correlation decodes"),
        pairs,
        "correlation round-trips losslessly"
    );
}

#[test]
fn the_mandatory_reserve_cannot_be_exhausted_by_one_m0_run() {
    // Count reserve: one run-started, one terminal record, at most one usage
    // record per model turn, and three mandatory records per tool call (start,
    // confirmation request, outcome). Headroom stays wide, so overflowing the
    // reserve stays an explicit refusal rather than a routine drop.
    let limits = Limits::m0_test();
    let worst_mandatory =
        2 + limits.max_model_turns_per_run as usize + 3 * limits.max_tool_calls_per_run as usize;
    assert!(
        worst_mandatory < MAX_MANDATORY_EVENTS,
        "M0 worst case {worst_mandatory} mandatory records must fit {MAX_MANDATORY_EVENTS}"
    );

    // Byte reserve: every tool payload at the full output budget, percent-encoded
    // at the documented worst case of three characters per byte.
    let encoded_payload = Limits::M0_TEST_TOOL_OUTPUT_BYTES * 3;
    let worst_mandatory_bytes = Limits::M0_TEST_TOOL_CALLS_PER_RUN as usize * encoded_payload;
    assert!(
        worst_mandatory_bytes <= MAX_MANDATORY_BYTES,
        "M0 worst case {worst_mandatory_bytes} encoded mandatory bytes must fit \
         {MAX_MANDATORY_BYTES}"
    );
    assert!(
        worst_mandatory_bytes < MAX_MANDATORY_BYTES,
        "the reserve keeps headroom over the encoded M0 worst case"
    );
}

#[test]
fn mandatory_overflow_is_an_error_value_never_a_shortened_report() {
    // A refused run has no report to inspect, so a dropped mandatory record can
    // never be reported as a truncated success. Both error classes are checked
    // exhaustively: a new variant has to state its exit code here.
    for (error, exit_code) in [
        (
            HeadlessError::Usage("usage".to_owned()),
            HeadlessError::USAGE_EXIT_CODE,
        ),
        (
            HeadlessError::Operation("operation".to_owned()),
            HeadlessError::OPERATION_EXIT_CODE,
        ),
    ] {
        assert_eq!(error.exit_code(), exit_code);
        assert_eq!(error.message(), error.to_string());
        assert!(!error.message().is_empty(), "a refusal explains itself");
    }
    assert_ne!(
        HeadlessError::USAGE_EXIT_CODE,
        HeadlessError::OPERATION_EXIT_CODE
    );
    assert_eq!(HeadlessError::USAGE_EXIT_CODE, 2, "usage maps to exit 2");
    assert_eq!(
        HeadlessError::OPERATION_EXIT_CODE,
        1,
        "operation maps to exit 1"
    );
    assert_eq!(EXIT_STDOUT_CLOSED, 6, "a closed consumer maps to exit 6");
    assert!(
        HeadlessError::OPERATION_EXIT_CODE != exit_code_for(RunOutcome::Completed, 0),
        "a refused run never shares the success exit code"
    );

    // A run that is reported at all carries the documented exit code for its
    // outcome: success 0, a denial 3, a refusal 1. A denial is a completed run
    // whose denied call keeps the distinct code.
    assert_eq!(scripted("hello").exit_code, 0);
    assert_eq!(scripted("deny: write it").exit_code, 3);
    assert_eq!(scripted("refuse: no").exit_code, 1);

    // An over-budget input is refused too: the input budget is exact, so the
    // boundary byte is accepted and one more is a usage error. Nothing is
    // partially reported at either side of the boundary.
    assert!(
        run_task(&"x".repeat(nexus_core::commands::MAX_INPUT_BYTES)).is_ok(),
        "input at the exact budget is accepted"
    );
    assert!(
        run_task(&"x".repeat(nexus_core::commands::MAX_INPUT_BYTES + 1)).is_err(),
        "input past the budget is refused, not partially reported"
    );

    // A refused input produces no report at all, not a partial one. The result
    // is matched rather than unwrapped because `HeadlessReport` deliberately has
    // no `Debug` implementation and must not gain one for a test.
    let refused = match run_task("") {
        Err(error) => error,
        Ok(report) => panic!(
            "an empty task is refused, not reported: {} lines",
            report.lines.len()
        ),
    };
    assert!(refused.is_usage(), "{refused}");
    assert!(
        USAGE.contains("nexus-headless"),
        "usage text stays available"
    );

    // The banner names the same revision the records carry: a fake-wired run can
    // never be mistaken for a real one because the revision moved.
    assert!(FAKE_BANNER.contains(WIRE_REV), "banner names the wire rev");
    assert!(FAKE_BANNER.contains("NO approval handler"), "{FAKE_BANNER}");
    for line in event_lines(&scripted("refuse: no")) {
        assert!(
            !line.contains("FAKE"),
            "no banner leakage into a record: {line}"
        );
    }

    // The only loss signal on a report is `truncated`, which is presentation
    // only: mandatory control records stay in the report (see the control-spine
    // test). Destructuring every field keeps a new retention-loss field from
    // being added without a decision here.
    let report = scripted("refuse: no");
    let HeadlessReport {
        run,
        lines,
        outcome,
        denied_calls,
        tool_started,
        approval_required,
        tool_finished,
        truncated,
        exit_code,
    } = report;
    assert!(!run.is_empty() && !lines.is_empty());
    assert_eq!(outcome, RunOutcome::Refused);
    assert_eq!(exit_code, exit_code_for(outcome, denied_calls));
    assert_eq!(
        (denied_calls, tool_started, approval_required, tool_finished),
        (0, 0, 0, 0)
    );
    assert!(
        !truncated,
        "presentation truncation is reported, never silent"
    );
}

#[test]
fn one_task_produces_one_report_modulo_host_issued_identities() {
    let first = scripted("deny: write it");
    let second = scripted("deny: write it");

    assert_eq!(
        normalized_lines(&first),
        normalized_lines(&second),
        "the same task yields the same records in the same order"
    );
    assert_eq!(first.outcome, second.outcome);
    assert_eq!(first.truncated, second.truncated);
    assert_eq!(
        (first.denied_calls, first.tool_started, first.tool_finished),
        (
            second.denied_calls,
            second.tool_started,
            second.tool_finished
        ),
        "counters are stable across runs"
    );
}
