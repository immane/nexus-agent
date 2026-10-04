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
