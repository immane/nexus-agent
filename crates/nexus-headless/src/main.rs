#![forbid(unsafe_code)]

//! Headless binary: one argv task, fake-wired runtime, sequenced stdout.
//!
//! The entry point is deliberately panic-free on the trust boundary: argv is
//! decoded with [`std::env::args_os`] so non-UTF-8 arguments never panic and
//! are never echoed, and stdout is written fallibly so a consumer that
//! closes the pipe early ends the process deliberately instead of through a
//! write panic.

use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;

use nexus_headless::{
    EXIT_STDOUT_CLOSED, FAKE_BANNER, HeadlessError, USAGE, outcome_name, run_task,
    write_report_lines,
};

/// Static diagnostic for argv that cannot be decoded as UTF-8. Argument
/// bytes are never echoed: they may contain sensitive data and are not known
/// to be printable.
const INVALID_ARGV: &str = "nexus-headless: error: arguments must be valid UTF-8";

/// Static diagnostic when the finite report budget dropped event records.
const TRUNCATED_REPORT: &str =
    "nexus-headless: note: report truncated by the event retention budget";

fn main() -> ExitCode {
    write_stderr_line(FAKE_BANNER);
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.is_empty() {
        write_stderr_line(USAGE);
        return ExitCode::from(exit_code_u8(HeadlessError::USAGE_EXIT_CODE));
    }
    let mut parts = Vec::with_capacity(args.len());
    for arg in args {
        match arg.into_string() {
            Ok(text) => parts.push(text),
            Err(_) => {
                write_stderr_line(INVALID_ARGV);
                write_stderr_line(USAGE);
                return ExitCode::from(exit_code_u8(HeadlessError::USAGE_EXIT_CODE));
            }
        }
    }
    let task = parts.join(" ");
    if task.trim().is_empty() {
        write_stderr_line(USAGE);
        return ExitCode::from(exit_code_u8(HeadlessError::USAGE_EXIT_CODE));
    }
    match run_task(&task) {
        Ok(report) => {
            if let Err(error) = write_report_lines(&mut io::stdout().lock(), &report.lines) {
                if error.kind() == io::ErrorKind::BrokenPipe {
                    // The consumer closed the pipe on purpose. Stop writing
                    // without a panic and without claiming full delivery.
                    return ExitCode::from(exit_code_u8(EXIT_STDOUT_CLOSED));
                }
                write_stderr_line(&format!(
                    "nexus-headless: error: stdout write failed: {error}"
                ));
                return ExitCode::from(exit_code_u8(HeadlessError::OPERATION_EXIT_CODE));
            }
            if report.truncated {
                write_stderr_line(TRUNCATED_REPORT);
            }
            write_stderr_line(&format!(
                "nexus-headless: run={} outcome={} denied={} tool-started={} tool-finished={}",
                report.run,
                outcome_name(report.outcome),
                report.denied_calls,
                report.tool_started,
                report.tool_finished,
            ));
            ExitCode::from(exit_code_u8(report.exit_code))
        }
        Err(error) => {
            write_stderr_line(&format!("nexus-headless: error: {error}"));
            if error.is_usage() {
                write_stderr_line(USAGE);
            }
            ExitCode::from(exit_code_u8(error.exit_code()))
        }
    }
}

/// Narrows an exit code to the platform byte without panicking on a bad
/// range; only the operation failure code is used as the fallback.
fn exit_code_u8(code: i32) -> u8 {
    u8::try_from(code).unwrap_or(HeadlessError::OPERATION_EXIT_CODE as u8)
}

/// Best-effort stderr diagnostic: a closed diagnostic stream must not turn
/// into a panic, and diagnostics never carry report data.
fn write_stderr_line(line: &str) {
    let mut stderr = io::stderr().lock();
    let _ = writeln!(stderr, "{line}");
    let _ = stderr.flush();
}

#[cfg(test)]
mod cov_main_private {
    //! Coverage for the entry point's private CLI helpers, which no external
    //! caller can reach: the byte narrowing behind every `ExitCode`, the two
    //! static stderr diagnostics, and the best-effort diagnostic writer.
    //!
    //! Every assertion uses fixed literals with no clock, randomness,
    //! environment, or external process I/O, so the suite is deterministic.
    //!
    //! The argv decoding loop itself lives inside `main`, which reads the real
    //! process argv and therefore cannot be invoked twice from a test
    //! harness; its end-to-end behaviour (missing, blank, and non-UTF-8 argv)
    //! is covered against the built binary in `tests/headless.rs` instead.

    use std::process::ExitCode;

    use nexus_core::RunOutcome;

    use super::{INVALID_ARGV, TRUNCATED_REPORT, exit_code_u8, write_stderr_line};
    use nexus_headless::{EXIT_STDOUT_CLOSED, FAKE_BANNER, HeadlessError, USAGE, exit_code_for};

    /// Every exit code the entry point can produce must survive the narrowing
    /// unchanged, so the documented mapping is what the process actually
    /// reports.
    #[test]
    fn documented_exit_codes_survive_byte_narrowing() {
        let documented = [
            ("completed", exit_code_for(RunOutcome::Completed, 0), 0),
            ("operation failure", HeadlessError::OPERATION_EXIT_CODE, 1),
            ("usage error", HeadlessError::USAGE_EXIT_CODE, 2),
            (
                "headless denial",
                exit_code_for(RunOutcome::Completed, 1),
                3,
            ),
            ("cancelled", exit_code_for(RunOutcome::Cancelled, 0), 4),
            (
                "limit reached",
                exit_code_for(RunOutcome::LimitReached, 0),
                5,
            ),
            ("stdout consumer closed", EXIT_STDOUT_CLOSED, 6),
        ];
        for (name, code, expected) in documented {
            assert_eq!(code, expected, "{name} maps to {expected}");
            let narrowed = exit_code_u8(code);
            assert_eq!(
                narrowed,
                u8::try_from(expected).expect("expected code fits a byte"),
                "{name} keeps its code"
            );
            // The exact conversion `main` performs must accept that byte.
            let _ = ExitCode::from(narrowed);
        }
        assert_eq!(exit_code_u8(0), 0);
        assert_eq!(exit_code_u8(u8::MAX as i32), u8::MAX);
    }

    /// A code outside the platform byte degrades to the operation failure code
    /// instead of panicking or truncating into an unrelated one.
    #[test]
    fn out_of_range_codes_degrade_to_the_operation_code() {
        for code in [-1, -2, -256, 256, 257, 1_000, i32::MIN, i32::MAX] {
            assert_eq!(
                exit_code_u8(code),
                HeadlessError::OPERATION_EXIT_CODE as u8,
                "out-of-range code {code} narrows to the operation failure code"
            );
        }
    }

    /// The fallback must not be mistaken for success, for the deliberate
    /// closed-pipe stop, or for a usage mistake.
    #[test]
    fn narrowing_failure_cannot_imply_success_pipe_close_or_usage() {
        for code in [-1, i32::MIN, 256, i32::MAX] {
            let narrowed = exit_code_u8(code);
            assert_ne!(narrowed, 0, "code {code} must not read as success");
            assert_ne!(
                narrowed, EXIT_STDOUT_CLOSED as u8,
                "code {code} must not read as a closed consumer"
            );
            assert_ne!(
                narrowed,
                HeadlessError::USAGE_EXIT_CODE as u8,
                "code {code} must not read as a usage error"
            );
        }
    }

    /// The error branch of `main` narrows each error class to its own byte and
    /// keeps the two classes distinguishable.
    #[test]
    fn error_exit_codes_narrow_to_their_documented_bytes() {
        let usage = HeadlessError::Usage("missing task".to_owned());
        let operation = HeadlessError::Operation("submit refused".to_owned());
        assert!(usage.is_usage());
        assert!(!operation.is_usage());
        assert_eq!(exit_code_u8(usage.exit_code()), 2);
        assert_eq!(exit_code_u8(operation.exit_code()), 1);
        assert_ne!(
            exit_code_u8(usage.exit_code()),
            exit_code_u8(operation.exit_code()),
            "usage and operation failures stay distinguishable"
        );
    }

    /// The non-UTF-8 argv diagnostic is static and value free: it cannot carry
    /// an argument byte and stays one printable ASCII line.
    #[test]
    fn invalid_argv_diagnostic_is_a_static_single_ascii_line() {
        assert_eq!(
            INVALID_ARGV,
            "nexus-headless: error: arguments must be valid UTF-8"
        );
        assert!(INVALID_ARGV.is_ascii(), "diagnostic stays ASCII");
        assert!(
            INVALID_ARGV.starts_with("nexus-headless: error: "),
            "diagnostic is self-identifying"
        );
        assert!(INVALID_ARGV.contains("valid UTF-8"), "{INVALID_ARGV}");
        assert!(
            !INVALID_ARGV.contains('{'),
            "no format placeholder can echo a value: {INVALID_ARGV}"
        );
        assert_ne!(INVALID_ARGV, USAGE, "diagnostic is not the usage hint");
    }

    /// Dropping presentation records is reported as a note about the report,
    /// never as a run failure.
    #[test]
    fn truncated_report_note_is_a_note_and_not_an_error() {
        assert_eq!(
            TRUNCATED_REPORT,
            "nexus-headless: note: report truncated by the event retention budget"
        );
        assert!(TRUNCATED_REPORT.is_ascii(), "note stays ASCII");
        assert!(
            TRUNCATED_REPORT.starts_with("nexus-headless: note: "),
            "note is self-identifying"
        );
        assert!(
            !TRUNCATED_REPORT.contains("error:"),
            "budget loss is not a failure: {TRUNCATED_REPORT}"
        );
        assert!(TRUNCATED_REPORT.contains("truncated"), "{TRUNCATED_REPORT}");
        assert_ne!(TRUNCATED_REPORT, INVALID_ARGV);
    }

    /// Static stderr text carries no report data and no escape bytes, and each
    /// diagnostic stays a single ASCII line.
    #[test]
    fn static_diagnostics_carry_no_report_or_argv_data() {
        for diagnostic in [INVALID_ARGV, TRUNCATED_REPORT, USAGE, FAKE_BANNER] {
            assert!(diagnostic.is_ascii(), "ASCII only: {diagnostic}");
            assert!(
                !diagnostic.contains(['\n', '\r']),
                "single line: {diagnostic}"
            );
            assert!(
                !diagnostic.contains('\x1b'),
                "no escape codes: {diagnostic}"
            );
            for field in [
                "run=",
                "outcome=",
                "denied=",
                "tool-started=",
                "tool-finished=",
            ] {
                assert!(
                    !diagnostic.contains(field),
                    "no report field {field}: {diagnostic}"
                );
            }
        }
        assert!(FAKE_BANNER.contains("FAKE"), "banner still self-identifies");
        for pair in [
            (INVALID_ARGV, TRUNCATED_REPORT),
            (INVALID_ARGV, USAGE),
            (TRUNCATED_REPORT, USAGE),
        ] {
            assert_ne!(pair.0, pair.1, "diagnostics stay distinct");
        }
    }

    /// The diagnostic writer is best-effort and receives runtime-derived text
    /// as a format argument, so braces, newlines, and escape bytes in an error
    /// message cannot panic the process or re-interpret the template.
    #[test]
    fn write_stderr_line_accepts_runtime_derived_text_without_panicking() {
        let lines = [
            String::new(),
            "plain diagnostic".to_owned(),
            "embedded\nnewline\r\nand\ttab".to_owned(),
            r#"unescaped {braces} {0} {{}} \" quote '"#.to_owned(),
            "\u{1b}[31mcolour\u{1b}[0m".to_owned(),
            "\u{FFFD} \u{2028} 世界".to_owned(),
            "x".repeat(64 * 1024),
            INVALID_ARGV.to_owned(),
            TRUNCATED_REPORT.to_owned(),
            format!(
                "nexus-headless: error: {error}",
                error = HeadlessError::Usage("missing task".to_owned())
            ),
            format!(
                "nexus-headless: run={} outcome={}",
                "run-1",
                nexus_headless::outcome_name(RunOutcome::Completed)
            ),
        ];
        for line in &lines {
            write_stderr_line(line.as_str());
        }
        // Repeated writes stay inert: the writer takes a fresh lock per call.
        write_stderr_line(INVALID_ARGV);
        write_stderr_line(TRUNCATED_REPORT);
    }
}
