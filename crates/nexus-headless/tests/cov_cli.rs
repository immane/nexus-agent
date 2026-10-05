#![forbid(unsafe_code)]

//! Hardened CLI-boundary coverage for the `nexus-headless` binary.
//!
//! `tests/headless.rs` pins the headline contract. This file attacks the same
//! contract from the outside and proves it structurally instead of by
//! substring: every stdout record is parsed and checked against the documented
//! machine encoding (revision prefix, closed `type`/`kind` vocabulary, no
//! duplicated keys, strictly increasing sequences, one run id, and values that
//! are canonically percent-encoded and recoverable through the public
//! [`nexus_headless::percent_decode`]); the argv trust boundary is exercised
//! with every class of undecodable, blank, and oversized input; each documented
//! exit code is pinned to the public mapping; and the report is checked to be
//! reproducible and immune to record forgery through the task text.
//!
//! Deterministic: fixed fake scripts, no threads, no clocks, no network, no
//! environment or filesystem influence, and no shared state between cases.
//! Every child completes on the fast path, so no case waits on a timeout.

use std::path::PathBuf;
use std::process::{Command, Output};

use nexus_core::RunOutcome;
use nexus_core::commands::MAX_INPUT_BYTES;
use nexus_core::ids::MAX_ID_LEN;
use nexus_headless::{
    EXIT_STDOUT_CLOSED, FAKE_BANNER, HeadlessError, OUTPUT_REV, PercentDecodeError, USAGE,
    exit_code_for, outcome_name, percent_decode, sanitize,
};

/// The documented diagnostic for argv that is not valid UTF-8. The binary keeps
/// it private, so it is pinned here by value: changing it changes the documented
/// CLI contract.
const INVALID_ARGV_DIAGNOSTIC: &str = "nexus-headless: error: arguments must be valid UTF-8";

/// Exit codes as the operating system reports them, taken from the public
/// mapping so a changed constant cannot silently disagree with the assertions.
const EXIT_USAGE: i32 = HeadlessError::USAGE_EXIT_CODE;
const EXIT_OPERATION: i32 = HeadlessError::OPERATION_EXIT_CODE;
const EXIT_STDOUT_CLOSED_CODE: i32 = EXIT_STDOUT_CLOSED;

/// Every `kind` the binary can emit, with the keys a parser may rely on. Extra
/// keys are additive and allowed; a missing key is a contract break.
const REQUIRED_KEYS: &[(&str, &[&str])] = &[
    ("run-started", &["request"]),
    ("text", &["item", "text-len", "text"]),
    ("preview", &["item"]),
    (
        "approval-required",
        &["approval", "call", "scope", "args-preview"],
    ),
    ("tool-started", &["call"]),
    ("tool-output", &["call", "truncated", "preview-len"]),
    (
        "tool-finished",
        &[
            "call",
            "status",
            "effect",
            "evidence",
            "truncated",
            "content-len",
            "content",
        ],
    ),
    ("usage", &["input", "output", "finality"]),
    (
        "run-finished",
        &["outcome", "persistence", "error", "error-correlation"],
    ),
];

/// The exact key set of the single trailing result record. It is the summary
/// contract, so an added or dropped key here breaks every consumer.
const RESULT_KEYS: &[&str] = &[
    "rev",
    "type",
    "outcome",
    "denied",
    "tool-started",
    "tool-finished",
    "run",
];

/// Every terminal run outcome a record may name, so an undocumented outcome
/// word cannot reach a consumer.
const OUTCOMES: [RunOutcome; 5] = [
    RunOutcome::Completed,
    RunOutcome::Failed,
    RunOutcome::Refused,
    RunOutcome::Cancelled,
    RunOutcome::LimitReached,
];

fn bin_path() -> PathBuf {
    let raw = option_env!("CARGO_BIN_EXE_nexus-headless")
        .or(option_env!("CARGO_BIN_EXE_nexus_headless"))
        .expect("headless binary is built");
    PathBuf::from(raw)
}

/// Captured child streams plus the exit code.
#[derive(Debug)]
struct Invocation {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Invocation {
    /// Asserts the exit code, naming the case so a failure is self-describing.
    fn assert_exit(&self, expected: i32, context: &str) {
        assert_eq!(self.code, Some(expected), "{context}");
    }

    /// Parses stdout under the full machine-record contract.
    fn records(&self) -> Vec<Record> {
        parse_records(&self.stdout)
    }

    /// Asserts the stderr run summary describes exactly the result record, so
    /// diagnostics and data can never disagree about the same run.
    fn assert_summary(&self, result: &Record) {
        let line = self
            .stderr
            .lines()
            .find(|line| line.starts_with("nexus-headless: run="))
            .unwrap_or_else(|| panic!("stderr carries the run summary: {}", self.stderr));
        let fields = parse_fields(line.trim_start_matches("nexus-headless: "));
        assert_eq!(fields.len(), 5, "summary shape: {fields:?}");
        for key in ["run", "outcome", "denied", "tool-started", "tool-finished"] {
            let reported = fields
                .iter()
                .find(|(name, _)| name.as_str() == key)
                .map(|(_, value)| value.as_str());
            assert_eq!(
                reported,
                Some(result.require(key)),
                "stderr summary {key} matches the result record"
            );
        }
    }
}

fn capture(output: Output) -> Invocation {
    Invocation {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("stdout is utf-8"),
        stderr: String::from_utf8(output.stderr).expect("stderr is utf-8"),
    }
}

fn invoke(args: &[&str]) -> Invocation {
    capture(
        Command::new(bin_path())
            .args(args)
            .output()
            .expect("headless binary runs"),
    )
}

#[cfg(unix)]
fn invoke_os(args: &[std::ffi::OsString]) -> Invocation {
    capture(
        Command::new(bin_path())
            .args(args)
            .output()
            .expect("headless binary runs"),
    )
}

/// One parsed stdout record: the raw line plus its decoded field structure.
#[derive(Debug)]
struct Record {
    line: String,
    fields: Vec<(String, String)>,
}

impl Record {
    fn new(line: &str) -> Self {
        Self {
            line: line.to_owned(),
            fields: parse_fields(line),
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(name, _)| name.as_str() == key)
            .map(|(_, value)| value.as_str())
    }

    fn require(&self, key: &str) -> &str {
        self.get(key)
            .unwrap_or_else(|| panic!("missing field {key} in {}", self.line))
    }

    /// The record kind, absent on the trailing result record.
    fn kind(&self) -> Option<&str> {
        self.get("kind")
    }

    /// A documented numeric counter field.
    fn count(&self, key: &str) -> usize {
        self.require(key)
            .parse::<usize>()
            .unwrap_or_else(|error| panic!("{key} is a count in {}: {error}", self.line))
    }
}

fn parse_fields(text: &str) -> Vec<(String, String)> {
    text.split(' ')
        .map(|field| {
            let (key, value) = field.split_once('=').expect("space-separated key=value");
            (key.to_owned(), value.to_owned())
        })
        .collect()
}

/// Asserts stderr is exactly `expected`, so no diagnostic can be added,
/// dropped, or reordered without failing.
fn assert_stderr_lines(stderr: &str, expected: &[&str]) {
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(lines, expected, "stderr line order is pinned");
    assert!(stderr.ends_with('\n'), "every stderr line is terminated");
}

/// Asserts the full structural contract for one machine stream: the documented
/// revision on every record, one bounded run id, no duplicated keys, canonically
/// encoded values recoverable through the public API, a closed `type`/`kind`
/// vocabulary, strictly increasing sequences, and exactly one trailing result
/// record with its documented key set.
fn parse_records(stdout: &str) -> Vec<Record> {
    assert!(stdout.is_ascii(), "the machine stream is ASCII: {stdout}");
    assert!(
        stdout.ends_with('\n'),
        "records are newline terminated: {stdout}"
    );
    assert!(!stdout.contains('\r'), "no carriage return: {stdout}");
    assert!(!stdout.contains('\x1b'), "no escape code: {stdout}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(!lines.is_empty(), "a run reports at least one record");
    assert!(lines.iter().all(|line| !line.is_empty()), "no blank record");
    let prefix = format!("rev={OUTPUT_REV} ");
    let records: Vec<Record> = lines.iter().copied().map(Record::new).collect();

    let run = records[0].require("run").to_owned();
    assert!(
        !run.is_empty() && run.len() <= MAX_ID_LEN,
        "the run id is a bounded core id: {run}"
    );
    assert!(
        run.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "the run id stays in the core id charset: {run}"
    );

    let mut previous: Option<u64> = None;
    let mut results = 0;
    for (index, record) in records.iter().enumerate() {
        assert!(
            record.line.starts_with(&prefix),
            "every record carries the revision: {}",
            record.line
        );
        assert_eq!(record.require("rev"), OUTPUT_REV, "{}", record.line);
        assert_eq!(
            record.require("run"),
            run,
            "one run per report: {}",
            record.line
        );

        let mut keys: Vec<&str> = record.fields.iter().map(|(key, _)| key.as_str()).collect();
        keys.sort_unstable();
        let key_count = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), key_count, "no duplicated key: {}", record.line);
        assert!(
            keys.iter().all(|key| !key.is_empty()
                && key
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'-')),
            "keys stay in the documented charset: {}",
            record.line
        );
        for (key, value) in &record.fields {
            let decoded = percent_decode(value)
                .unwrap_or_else(|error| panic!("{key} decodes in {}: {error}", record.line));
            assert_eq!(
                sanitize(&decoded),
                *value,
                "{key} is canonically encoded in {}",
                record.line
            );
        }

        match record.require("type") {
            "event" => {
                let seq = record
                    .require("seq")
                    .parse::<u64>()
                    .unwrap_or_else(|error| {
                        panic!("seq is an integer in {}: {error}", record.line)
                    });
                if let Some(last) = previous {
                    assert!(seq > last, "records are sequence ordered: {}", record.line);
                }
                previous = Some(seq);
                let kind = record.require("kind");
                let required = REQUIRED_KEYS
                    .iter()
                    .find(|(name, _)| *name == kind)
                    .map(|(_, required)| *required)
                    .unwrap_or_else(|| {
                        panic!("undocumented record kind {kind} in {}", record.line)
                    });
                for key in required {
                    record.require(key);
                }
            }
            "result" => {
                results += 1;
                assert_eq!(
                    index + 1,
                    records.len(),
                    "the result record is last: {}",
                    record.line
                );
                let mut expected: Vec<&str> = RESULT_KEYS.to_vec();
                expected.sort_unstable();
                let mut actual = keys.clone();
                actual.sort_unstable();
                assert_eq!(actual, expected, "result record shape: {}", record.line);
                let outcome = record.require("outcome");
                assert!(
                    OUTCOMES
                        .iter()
                        .any(|candidate| outcome_name(*candidate) == outcome),
                    "documented outcome word: {}",
                    record.line
                );
                record.count("denied");
                record.count("tool-started");
                record.count("tool-finished");
            }
            other => panic!("undocumented record type {other}: {}", record.line),
        }
    }
    assert_eq!(results, 1, "exactly one result record");
    records
}

fn count_kind(records: &[Record], kind: &str) -> usize {
    records
        .iter()
        .filter(|record| record.kind() == Some(kind))
        .count()
}

fn first_kind<'a>(records: &'a [Record], kind: &str) -> &'a Record {
    records
        .iter()
        .find(|record| record.kind() == Some(kind))
        .unwrap_or_else(|| panic!("a {kind} record is present"))
}

fn result_record(records: &[Record]) -> &Record {
    records
        .last()
        .expect("the report ends with a result record")
}

/// The per-run event sequences, in emitted order. The trailing result record
/// carries no sequence.
fn sequence(records: &[Record]) -> Vec<u64> {
    records
        .iter()
        .filter(|record| record.get("type") == Some("event"))
        .map(|record| {
            record
                .require("seq")
                .parse::<u64>()
                .expect("event sequences are integers")
        })
        .collect()
}

/// Splits a runtime-issued identity into its incarnation segment and the
/// ordinal tail that follows it.
///
/// The runtime mints `r{incarnation:x}-{run_n}`,
/// `c{incarnation:x}-{run_n}-{call_seq}`, and
/// `a{incarnation:x}-{run_n}-{approval_n}`. The incarnation is the only part a
/// fresh process is free to change, so it is the only part a comparison across
/// two processes may ignore.
fn split_issued(raw: &str, kind: char) -> (&str, &str) {
    let rest = raw
        .strip_prefix(kind)
        .unwrap_or_else(|| panic!("{raw} is not a {kind}-scoped identity"));
    let (incarnation, ordinals) = rest
        .split_once('-')
        .unwrap_or_else(|| panic!("{raw} carries no incarnation segment"));
    assert!(!incarnation.is_empty(), "{raw} has an empty incarnation");
    (incarnation, ordinals)
}

/// The identity keys a record can carry, each with the prefix the runtime uses
/// to scope it to an incarnation.
const ISSUED_KEYS: [(&str, char); 3] = [("run", 'r'), ("call", 'c'), ("approval", 'a')];

/// The report with every process-issued identity reduced to its ordinal tail,
/// the one value a fresh process is free to differ in.
///
/// Only the incarnation segment is folded away, so `r<incarnation>-1`,
/// `c<incarnation>-1-0`, and `a<incarnation>-1-0` remain under comparison: a
/// differing call or approval ordinal, a lost or added ordinal component, or an
/// identity borrowed from another run still fails the comparison. Every
/// identity in a report is also checked to carry the run's own incarnation, so
/// a record that leaks a foreign id cannot hide behind the normalization.
fn without_incarnation(records: &[Record]) -> Vec<String> {
    let run = records
        .first()
        .expect("a run always reports records")
        .require("run");
    let (incarnation, _) = split_issued(run, 'r');
    records
        .iter()
        .map(|record| {
            let mut line = record.line.clone();
            for (key, kind) in ISSUED_KEYS {
                let Some(value) = record.get(key) else {
                    continue;
                };
                let (found, ordinals) = split_issued(value, kind);
                assert_eq!(
                    found, incarnation,
                    "{key} carries the run's incarnation: {}",
                    record.line
                );
                // A percent-encoded correlation pair embeds the same identity
                // text, so replacing the raw token covers both spellings.
                line = line.replace(value, &format!("{kind}<incarnation>-{ordinals}"));
            }
            line
        })
        .collect()
}

/// Encoding round-trip for one text, including the canonical-form property a
/// decoder can rely on: no separator or control byte survives, and re-encoding
/// a decoded value reproduces the original token.
fn assert_round_trip(text: &str) {
    let encoded = sanitize(text);
    assert!(encoded.is_ascii(), "encoded values are ASCII: {text:?}");
    assert!(
        !encoded.contains([' ', '=', '\n', '\r', '\x1b']),
        "no separator or control byte survives: {text:?}"
    );
    let decoded =
        percent_decode(&encoded).unwrap_or_else(|error| panic!("round-trips {text:?}: {error}"));
    assert_eq!(decoded, text, "round-trips {text:?}");
    assert_eq!(sanitize(&decoded), encoded, "canonical form: {text:?}");
}

/// The three argv-reachable script paths, with the exit code each must report.
const RUNNABLE_TASKS: [(&str, i32); 3] = [("hello", 0), ("deny: write it", 3), ("refuse: no", 1)];

#[test]
fn completed_run_reports_one_started_and_one_finished_tool_record() {
    let invocation = invoke(&["hello"]);
    invocation.assert_exit(0, "the completed path exits zero");
    assert_eq!(
        Some(exit_code_for(RunOutcome::Completed, 0)),
        invocation.code,
        "the CLI code matches the public mapping"
    );
    let records = invocation.records();
    assert_eq!(
        count_kind(&records, "tool-started"),
        1,
        "the read tool starts"
    );
    assert_eq!(
        count_kind(&records, "tool-finished"),
        1,
        "and finishes once"
    );
    assert_eq!(
        count_kind(&records, "approval-required"),
        0,
        "a scoped read needs no approval"
    );

    let finished = first_kind(&records, "tool-finished");
    assert_eq!(finished.require("status"), "succeeded", "{}", finished.line);
    assert_eq!(finished.require("effect"), "known-not-applied");
    assert_eq!(finished.require("evidence"), "host-observed");
    assert_eq!(finished.require("truncated"), "false");
    let content = percent_decode(finished.require("content")).expect("content decodes");
    assert!(
        !content.is_empty(),
        "a succeeded tool reports content: {}",
        finished.line
    );
    assert_eq!(
        content.len(),
        finished.count("content-len"),
        "content-len describes the decoded bytes: {}",
        finished.line
    );

    let terminal = first_kind(&records, "run-finished");
    assert_eq!(
        terminal.require("outcome"),
        outcome_name(RunOutcome::Completed)
    );
    assert_eq!(terminal.require("persistence"), "ephemeral");
    assert_eq!(terminal.require("error"), "none");
    assert_eq!(terminal.require("error-correlation"), "none");

    let result = result_record(&records);
    assert_eq!(
        result.require("outcome"),
        outcome_name(RunOutcome::Completed)
    );
    assert_eq!(result.count("denied"), 0);
    assert_eq!(result.count("tool-started"), 1);
    assert_eq!(result.count("tool-finished"), 1);
    invocation.assert_summary(result);
}

#[test]
fn denial_without_handler_exits_three_with_no_tool_started() {
    let invocation = invoke(&["deny: write it"]);
    invocation.assert_exit(3, "a denied call is its own exit class");
    assert_eq!(
        Some(exit_code_for(RunOutcome::Completed, 1)),
        invocation.code,
        "the CLI code matches the public mapping"
    );
    let records = invocation.records();
    assert_eq!(
        count_kind(&records, "tool-started"),
        0,
        "denied calls never start: {}",
        invocation.stdout
    );
    assert_eq!(
        count_kind(&records, "approval-required"),
        0,
        "no handler consumes approvals: {}",
        invocation.stdout
    );
    assert_eq!(count_kind(&records, "tool-finished"), 1);
    let finished = first_kind(&records, "tool-finished");
    assert_eq!(finished.require("status"), "denied", "{}", finished.line);
    assert_eq!(finished.require("effect"), "not-started");
    let result = result_record(&records);
    assert_eq!(
        result.require("outcome"),
        outcome_name(RunOutcome::Completed)
    );
    assert_eq!(result.count("denied"), 1);
    assert_eq!(result.count("tool-started"), 0);
    assert_eq!(result.count("tool-finished"), 1);
    invocation.assert_summary(result);
}

#[test]
fn refusal_run_reports_an_operation_failure_without_executing() {
    let invocation = invoke(&["refuse: no"]);
    invocation.assert_exit(1, "a refusal is an operational failure");
    assert_eq!(
        Some(exit_code_for(RunOutcome::Refused, 0)),
        invocation.code,
        "the CLI code matches the public mapping"
    );
    let records = invocation.records();
    assert_eq!(
        count_kind(&records, "tool-started"),
        0,
        "a refusal executes nothing: {}",
        invocation.stdout
    );
    assert_eq!(count_kind(&records, "tool-finished"), 0);
    assert_eq!(
        first_kind(&records, "run-finished").require("outcome"),
        outcome_name(RunOutcome::Refused)
    );
    let result = result_record(&records);
    assert_eq!(result.require("outcome"), outcome_name(RunOutcome::Refused));
    assert_eq!(result.count("denied"), 0);
    assert_eq!(result.count("tool-started"), 0);
    invocation.assert_summary(result);
}

/// Every class of undecodable argv byte is the same value-free usage error: no
/// byte is echoed, nothing is lossily replaced, no task is submitted, and the
/// diagnostic never varies with the input.
#[cfg(unix)]
#[test]
fn invalid_utf8_argv_is_a_static_value_free_usage_error() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let sentinel = "ARGV_SENTINEL";
    let classes: [Vec<u8>; 6] = [
        vec![0x80],                         // lone continuation byte
        vec![0xC3],                         // truncated two-byte lead
        vec![0xC0, 0x80],                   // overlong encoding
        vec![0xED, 0xA0, 0x80],             // UTF-16 surrogate half
        vec![0xF5, 0x80, 0x80, 0x80],       // above U+10FFFF
        vec![b't', b'a', b's', b'k', 0xFF], // separator and control bytes
    ];
    let mut expected: Option<String> = None;
    for class in &classes {
        let mut raw = sentinel.as_bytes().to_vec();
        raw.extend_from_slice(class);
        let invocation = invoke_os(&[OsString::from_vec(raw)]);
        invocation.assert_exit(EXIT_USAGE, &format!("byte class {class:?}"));
        assert!(
            invocation.stdout.is_empty(),
            "no data output for {class:?}: {}",
            invocation.stdout
        );
        assert_stderr_lines(
            &invocation.stderr,
            &[FAKE_BANNER, INVALID_ARGV_DIAGNOSTIC, USAGE],
        );
        assert!(
            !invocation.stderr.contains(sentinel),
            "argv bytes are never echoed: {}",
            invocation.stderr
        );
        assert!(
            !invocation.stderr.contains('\u{FFFD}'),
            "no lossy replacement character: {}",
            invocation.stderr
        );
        assert!(
            !invocation.stderr.contains("panicked"),
            "no panic: {}",
            invocation.stderr
        );
        assert!(
            !invocation.stderr.contains("run="),
            "no run is reported for unusable argv: {}",
            invocation.stderr
        );
        match &expected {
            None => expected = Some(invocation.stderr.clone()),
            Some(first) => assert_eq!(
                *first, invocation.stderr,
                "the diagnostic is static, never input-derived"
            ),
        }
    }

    // Every argv entry is decoded, not just the first: a valid leading task
    // must not smuggle an unusable entry past the trust boundary.
    let mut raw = b"hello ".to_vec();
    raw.extend_from_slice(&[0xFE, 0xFF, 0x80]);
    let invocation = invoke_os(&[OsString::from("deny: write it"), OsString::from_vec(raw)]);
    invocation.assert_exit(EXIT_USAGE, "an invalid later entry is still a usage error");
    assert!(invocation.stdout.is_empty());
    assert_stderr_lines(
        &invocation.stderr,
        &[FAKE_BANNER, INVALID_ARGV_DIAGNOSTIC, USAGE],
    );
    assert!(
        !invocation.stderr.contains("write it"),
        "the decodable task is not echoed either: {}",
        invocation.stderr
    );
}

#[test]
fn missing_or_blank_task_is_a_usage_error() {
    let cases: &[(&[&str], &str)] = &[
        (&[], "no arguments"),
        (&[""], "one empty argument"),
        (&[" "], "one space"),
        (&["\t\n"], "tab and newline"),
        (&["  ", "  "], "joined whitespace"),
        (&["\r\n \t"], "carriage return and whitespace"),
    ];
    for (args, context) in cases {
        let invocation = invoke(args);
        invocation.assert_exit(EXIT_USAGE, context);
        assert!(
            invocation.stdout.is_empty(),
            "{context}: no data output on a usage error"
        );
        assert_stderr_lines(&invocation.stderr, &[FAKE_BANNER, USAGE]);
        assert!(
            !invocation.stderr.contains("panicked"),
            "{context}: no panic: {}",
            invocation.stderr
        );
    }
    assert_eq!(cases.len(), 6, "every blank-argv class is pinned");
}

#[test]
fn oversize_task_is_a_usage_error_without_echoing_input() {
    let sentinel = "OVERSIZE_ARGV_SENTINEL";
    let filler = "x".repeat(MAX_INPUT_BYTES);
    let task = format!("{sentinel}{filler}");
    assert!(
        task.len() > MAX_INPUT_BYTES,
        "the task exceeds the frontend input budget"
    );

    let invocation = invoke(&[&task]);
    invocation.assert_exit(EXIT_USAGE, "an oversize task is a usage error");
    assert!(
        invocation.stdout.is_empty(),
        "no data output on a usage error"
    );
    let lines: Vec<&str> = invocation.stderr.lines().collect();
    assert_eq!(lines.len(), 3, "banner, one diagnostic, usage: {lines:?}");
    assert_eq!(lines[0], FAKE_BANNER);
    assert!(
        lines[1].starts_with("nexus-headless: error:"),
        "the rejection is a diagnostic: {lines:?}"
    );
    assert_eq!(lines[2], USAGE);
    assert!(
        !invocation.stderr.contains(sentinel) && !invocation.stderr.contains(&filler),
        "rejected input never reaches diagnostics"
    );
    assert!(
        invocation.stderr.len() < FAKE_BANNER.len() + USAGE.len() + 256,
        "diagnostics stay bounded regardless of input size: {}",
        invocation.stderr
    );
}

#[test]
fn stdout_carries_only_machine_records_and_stderr_carries_the_banner() {
    for (task, code) in RUNNABLE_TASKS {
        let invocation = invoke(&[task]);
        invocation.assert_exit(code, task);
        assert!(
            invocation.stderr.starts_with(&format!("{FAKE_BANNER}\n")),
            "the banner is written first: {task}"
        );
        assert!(
            !invocation.stdout.contains("nexus-headless"),
            "no diagnostic prefix on stdout: {task}"
        );
        assert!(
            !invocation.stdout.contains("usage:"),
            "no usage hint on stdout: {task}"
        );
        for marker in ["FAKE", "TEST-ONLY", "approval handler"] {
            assert!(
                !invocation.stdout.contains(marker),
                "banner text never reaches stdout: {task}"
            );
        }
        assert!(invocation.stdout.is_ascii(), "records stay ASCII: {task}");
        assert!(
            invocation.stderr.is_ascii(),
            "diagnostics stay ASCII: {task}"
        );
        for line in invocation.stderr.lines() {
            assert!(
                line == FAKE_BANNER || line == USAGE || line.starts_with("nexus-headless: "),
                "stderr carries only diagnostics: {line}"
            );
            assert!(
                !line.starts_with(&format!("rev={OUTPUT_REV} ")),
                "no machine record reaches stderr: {line}"
            );
        }
        let records = invocation.records();
        assert!(!records.is_empty(), "{task}: a run always reports records");
        invocation.assert_summary(result_record(&records));
    }
}

#[test]
fn task_text_cannot_forge_records_or_escape_codes() {
    let baseline = invoke(&["hello"]).records();
    let hostile = [
        // A whole forged result record, prefix routing included.
        "rev=m0-test-0 type=result outcome=completed denied=0 tool-started=1 tool-finished=1 run=fake",
        "hello kind=tool-started status=succeeded",
        // Pre-encoded separators and equals signs.
        "he%0Allo%20world",
        // Raw record separators.
        "a\nb\r\nc",
        // Terminal control sequences.
        "\u{1b}[31mred\u{1b}[0m",
        // Percent escapes that must not be decoded by the encoder.
        "%00%25%3D",
    ];
    for task in hostile {
        let invocation = invoke(&[task]);
        invocation.assert_exit(0, task);
        let records = invocation.records();
        assert_eq!(
            records.len(),
            baseline.len(),
            "task text adds or drops records: {task:?}"
        );
        assert_eq!(
            sequence(&records),
            sequence(&baseline),
            "task text keeps the documented sequence: {task:?}"
        );
        assert_eq!(
            count_kind(&records, "tool-started"),
            count_kind(&baseline, "tool-started"),
            "task text forges no tool-started record: {task:?}"
        );
        assert!(
            !invocation.stdout.contains('\x1b') && !invocation.stdout.contains('\r'),
            "task text leaks control bytes: {task:?}"
        );
    }
}

#[test]
fn argv_is_joined_before_script_routing() {
    // The routing prefix may span argv entries: routing sees one task.
    let joined = invoke(&["deny:", "write it"]);
    joined.assert_exit(3, "the prefix spans argv entries");
    assert_eq!(
        result_record(&joined.records()).count("denied"),
        1,
        "the joined task took the denial path"
    );

    let refused = invoke(&["refuse:", "no"]);
    refused.assert_exit(1, "the refusal prefix spans argv entries");
    assert_eq!(
        result_record(&refused.records()).require("outcome"),
        outcome_name(RunOutcome::Refused)
    );

    // A prefix split across entries is not a prefix: routing happens after the
    // join, never per entry.
    let split = invoke(&["re", "fuse: no"]);
    split.assert_exit(0, "a split prefix is not the prefix");
    assert_eq!(
        result_record(&split.records()).require("outcome"),
        outcome_name(RunOutcome::Completed)
    );

    invoke(&["hello", "world"]).assert_exit(0, "several entries are one task");
}

/// Every value the binary actually emits is recoverable through the public
/// decoder, and the emitted length fields describe the decoded bytes.
#[test]
fn emitted_record_values_round_trip_through_the_public_api() {
    for (task, _) in RUNNABLE_TASKS {
        let invocation = invoke(&[task]);
        let mut answer = String::new();
        for record in invocation.records() {
            for (key, value) in &record.fields {
                let decoded = percent_decode(value).unwrap_or_else(|error| {
                    panic!("{key} decodes in {task:?} record {}: {error}", record.line)
                });
                assert_eq!(
                    sanitize(&decoded),
                    *value,
                    "{key} is canonical in {task:?} record {}",
                    record.line
                );
            }
            if record.kind() == Some("text") {
                let decoded = percent_decode(record.require("text")).expect("text decodes");
                assert_eq!(
                    decoded.len(),
                    record.count("text-len"),
                    "text-len describes the decoded bytes: {task:?} record {}",
                    record.line
                );
                answer.push_str(&decoded);
            }
            if record.kind() == Some("tool-finished") {
                let decoded = percent_decode(record.require("content")).expect("content decodes");
                assert_eq!(
                    decoded.len(),
                    record.count("content-len"),
                    "content-len describes the decoded bytes: {task:?} record {}",
                    record.line
                );
                assert!(
                    !decoded.contains('\u{FFFD}'),
                    "decoding never substitutes: {task:?}"
                );
            }
        }
        if task == "hello" {
            assert_eq!(
                answer, "fake-answer",
                "the provider answer survives the machine encoding"
            );
        }
    }
}

/// The public machine encoding is reversible for any text a parser can be
/// handed, escapes are canonical, and the decoder rejects what the encoder
/// cannot produce.
#[test]
fn machine_encoding_round_trips_through_the_public_api() {
    // Every BMP code point in one payload, then a block of astral scalars.
    let bmp: String = (0u32..=0xFFFF).filter_map(char::from_u32).collect();
    assert_round_trip(&bmp);
    let astral: String = (0x1_0000u32..=0x1_02FF)
        .filter_map(char::from_u32)
        .collect();
    assert_round_trip(&astral);
    for text in [
        "",
        "hello",
        "héllo 🌍 世界",
        "a=b c\nnewline\t\ttab\rreturn",
        "\u{1b}[31mred\u{1b}[0m",
        "%25 already encoded",
        "%",
        "~-._azAZ09",
        "\u{200B}\u{202E}\u{FEFF}\u{FFFD}",
        "quote\" backslash\\ comma,",
        "nul\u{0}end",
    ] {
        assert_round_trip(text);
    }
    // Scalars that matter on their own: an encoder bug on one of them must not
    // hide inside the bulk payloads above.
    for scalar in [
        '\u{0}',
        '\u{7f}',
        '\u{1b}',
        '\n',
        '\r',
        ' ',
        '=',
        '%',
        '"',
        '\\',
        '\u{a0}',
        '\u{2028}',
        '\u{2029}',
        '\u{feff}',
        '\u{fffd}',
        '\u{1f300}',
        '\u{10ffff}',
    ] {
        let single = scalar.to_string();
        assert_round_trip(&single);
    }

    let cases: [(&str, Result<String, PercentDecodeError>); 7] = [
        ("%", Err(PercentDecodeError::MalformedEscape)),
        ("%2", Err(PercentDecodeError::MalformedEscape)),
        ("%GG", Err(PercentDecodeError::MalformedEscape)),
        ("%2g", Err(PercentDecodeError::MalformedEscape)),
        ("plain", Ok("plain".to_owned())),
        ("%c3%a9", Ok("é".to_owned())),
        ("a%20b", Ok("a b".to_owned())),
    ];
    for (encoded, expected) in cases {
        assert_eq!(percent_decode(encoded), expected, "decode {encoded:?}");
    }
    assert_eq!(percent_decode("%FF"), Err(PercentDecodeError::InvalidUtf8));
    assert_eq!(
        percent_decode("%ED%A0%80"),
        Err(PercentDecodeError::InvalidUtf8),
        "a surrogate half is not UTF-8"
    );
}

/// Two identical runs report identical records: the script is fixed and nothing
/// in the report depends on wall-clock time or process identity. The runtime
/// stamps a fresh incarnation into every run, call, and approval id, so that
/// segment is the one value two processes may legitimately differ in — and
/// every other value, including each id's ordinal tail, must match exactly.
#[test]
fn repeated_runs_report_the_same_records() {
    let first = invoke(&["hello"]);
    let second = invoke(&["hello"]);
    first.assert_exit(0, "the completed path exits zero");
    second.assert_exit(0, "the completed path exits zero");
    let first_records = first.records();
    let second_records = second.records();
    assert!(!first_records.is_empty(), "a run always reports records");
    let first_report = without_incarnation(&first_records);
    let second_report = without_incarnation(&second_records);

    // The normalization must actually be exercised: two fresh processes get
    // distinct incarnations, so if the ids matched verbatim the test would be
    // passing for the wrong reason and would stop proving reproducibility.
    let first_run = first_records[0].require("run");
    let second_run = second_records[0].require("run");
    assert_ne!(
        first_run, second_run,
        "two fresh processes issue distinct run ids"
    );
    assert!(
        first_report
            .iter()
            .any(|line| line.contains("<incarnation>")),
        "the report carries incarnation-scoped identities to normalize"
    );

    assert_eq!(first_report, second_report, "the report is reproducible");
    assert_eq!(
        sequence(&first_records),
        sequence(&second_records),
        "sequences are reproducible"
    );
}

/// With both streams pointed at one pipe, the banner still lands first, the
/// records follow, and the run summary closes the stream: a caller merging
/// `> file 2>&1` sees diagnostics before data and no record inside the banner.
#[test]
fn merged_stream_orders_the_banner_before_the_records() {
    use std::io::Read;
    use std::os::fd::OwnedFd;
    use std::process::Stdio;

    let (mut reader, writer) = std::io::pipe().expect("pipe opens");
    let diagnostics: OwnedFd = writer.try_clone().expect("pipe duplicates").into();
    let mut child = Command::new(bin_path())
        .arg("hello")
        .stdout(Stdio::from(writer))
        .stderr(Stdio::from(diagnostics))
        .spawn()
        .expect("binary spawns");
    let mut merged = String::new();
    reader
        .read_to_string(&mut merged)
        .expect("merged stream drains");
    let status = child.wait().expect("binary waits");
    assert_eq!(status.code(), Some(0), "the completed path exits zero");

    let lines: Vec<&str> = merged.lines().collect();
    assert_eq!(
        lines.first(),
        Some(&FAKE_BANNER),
        "the banner is the first thing written: {lines:?}"
    );
    let summary = lines.last().expect("the merged stream is not empty");
    assert!(
        summary.starts_with("nexus-headless: run="),
        "the run summary is last: {lines:?}"
    );
    let body = &lines[1..lines.len() - 1];
    assert!(!body.is_empty(), "records follow the banner: {lines:?}");
    let prefix = format!("rev={OUTPUT_REV} ");
    for line in body {
        assert!(
            line.starts_with(&prefix),
            "only machine records sit between banner and summary: {line}"
        );
    }
    // The interleaved records still satisfy the machine contract.
    assert!(!parse_records(&format!("{}\n", body.join("\n"))).is_empty());
}

/// A consumer that closes stdout before the run finishes gets the documented
/// closed-consumer code: no write panic, no echo of the operating-system error,
/// and no claim that the report was delivered. A closed diagnostic stream
/// changes nothing, and the code wins over the run's own mapping.
#[cfg(unix)]
#[test]
fn broken_stdout_pipe_exits_without_panic() {
    use std::os::fd::OwnedFd;
    use std::process::Stdio;

    assert_eq!(EXIT_STDOUT_CLOSED_CODE, 6, "the documented code is stable");
    let closed_pipe = || -> OwnedFd {
        let (reader, writer) = std::io::pipe().expect("pipe opens");
        drop(reader);
        writer.into()
    };

    // The read end is closed before the child starts, so the first stdout write
    // fails with EPIPE while stderr still works.
    let child = Command::new(bin_path())
        .arg("hello")
        .stdout(Stdio::from(closed_pipe()))
        .stderr(Stdio::piped())
        .spawn()
        .expect("binary spawns");
    let invocation = capture(child.wait_with_output().expect("binary waits"));
    invocation.assert_exit(EXIT_STDOUT_CLOSED_CODE, "a closed pipe has its own code");
    assert!(invocation.stdout.is_empty(), "no stdout survived the pipe");
    assert_stderr_lines(&invocation.stderr, &[FAKE_BANNER]);
    for marker in ["panicked", "Broken pipe", "BrokenPipe", "run="] {
        assert!(
            !invocation.stderr.contains(marker),
            "the closed consumer stays quiet: {}",
            invocation.stderr
        );
    }

    // A closed diagnostic stream is best effort: the exit code is unchanged and
    // the process still terminates instead of panicking on a failed write.
    let mut child = Command::new(bin_path())
        .arg("hello")
        .stdout(Stdio::from(closed_pipe()))
        .stderr(Stdio::null())
        .spawn()
        .expect("binary spawns");
    assert_eq!(
        child.wait().expect("binary waits").code(),
        Some(EXIT_STDOUT_CLOSED_CODE),
        "a closed stderr does not change the code"
    );

    // The closed-pipe code wins over the code the run would have reported.
    for (task, own_code) in [("deny: write it", 3), ("refuse: no", 1)] {
        let mut child = Command::new(bin_path())
            .arg(task)
            .stdout(Stdio::from(closed_pipe()))
            .stderr(Stdio::null())
            .spawn()
            .expect("binary spawns");
        assert_eq!(
            child.wait().expect("binary waits").code(),
            Some(EXIT_STDOUT_CLOSED_CODE),
            "task {task:?} exits {own_code} with an open pipe"
        );
    }
}

/// Every documented exit class is distinct and the argv-reachable ones are what
/// the public mapping promises. Codes 4 (cancelled) and 5 (limit reached) have
/// no argv spelling in M0: the fake scripts always reach a terminal record.
#[test]
fn exit_codes_are_distinct_across_the_documented_mapping() {
    let mapping = [
        exit_code_for(RunOutcome::Completed, 0),
        exit_code_for(RunOutcome::Completed, 1),
        exit_code_for(RunOutcome::Failed, 0),
        exit_code_for(RunOutcome::Refused, 0),
        exit_code_for(RunOutcome::Cancelled, 0),
        exit_code_for(RunOutcome::LimitReached, 0),
        HeadlessError::USAGE_EXIT_CODE,
        HeadlessError::OPERATION_EXIT_CODE,
        EXIT_STDOUT_CLOSED,
    ];
    let mut distinct = mapping.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(
        distinct,
        vec![0, 1, 2, 3, 4, 5, 6],
        "every class owns a distinct exit code"
    );
    assert_eq!(EXIT_USAGE, 2, "usage errors exit two");
    assert_eq!(EXIT_OPERATION, 1, "operation failures exit one");
    assert_eq!(
        EXIT_STDOUT_CLOSED_CODE, 6,
        "the closed consumer is neither success nor an error class"
    );

    let cases: &[(&[&str], i32)] = &[
        (&["hello"], exit_code_for(RunOutcome::Completed, 0)),
        (&["refuse: no"], exit_code_for(RunOutcome::Refused, 0)),
        (&[], HeadlessError::USAGE_EXIT_CODE),
        (&["deny: write it"], exit_code_for(RunOutcome::Completed, 1)),
    ];
    for (args, expected) in cases {
        invoke(args).assert_exit(*expected, &format!("args {args:?}"));
    }
}
