#![forbid(unsafe_code)]

//! Binary-level checks: stdout/stderr separation, banner, exit codes.

use std::path::PathBuf;
use std::process::Command;

fn bin_path() -> PathBuf {
    let raw = option_env!("CARGO_BIN_EXE_nexus-headless")
        .or(option_env!("CARGO_BIN_EXE_nexus_headless"))
        .expect("headless binary is built");
    PathBuf::from(raw)
}

fn run_with(args: &[&str]) -> std::process::Output {
    Command::new(bin_path())
        .args(args)
        .output()
        .expect("binary runs")
}

#[test]
fn stdout_carries_only_machine_lines_and_stderr_carries_banner() {
    let output = run_with(&["hello"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr is utf-8");
    assert!(stderr.contains("FAKE"), "stderr self-identifies: {stderr}");
    assert!(stderr.contains("TEST-ONLY"), "{stderr}");
    assert!(!stdout.contains("FAKE"), "banner never leaks to stdout");
    assert!(!stdout.contains('\x1b'), "no escape codes in stdout");
    assert!(!stdout.is_empty());
    for line in stdout.lines() {
        assert!(line.starts_with("rev=m0-test-0 "), "{line}");
    }
    assert!(
        stdout
            .lines()
            .last()
            .expect("result")
            .starts_with("rev=m0-test-0 type=result ")
    );
}

#[test]
fn denial_without_handler_exits_three_with_no_tool_started() {
    let output = run_with(&["deny: write it"]);
    assert_eq!(output.status.code(), Some(3));
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf-8");
    assert!(!stdout.contains('\x1b'));
    assert!(
        !stdout.contains("kind=tool-started"),
        "denied calls never start: {stdout}"
    );
    assert!(
        !stdout.contains("kind=approval-required"),
        "no handler consumes approvals: {stdout}"
    );
    assert!(stdout.contains("status=denied"), "{stdout}");
    assert!(stdout.contains("effect=not-started"), "{stdout}");
}

#[test]
fn missing_task_is_a_usage_error() {
    let output = run_with(&[]);
    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr is utf-8");
    assert!(stdout.is_empty(), "no data output on usage error");
    assert!(stderr.contains("FAKE"), "banner still printed: {stderr}");
    assert!(stderr.contains("usage:"), "{stderr}");
}
