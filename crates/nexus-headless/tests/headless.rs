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

/// Public machine encoding: every text sample survives the documented
/// percent round-trip, and the encoded form is ASCII with no raw ESC.
#[test]
fn machine_encoding_roundtrips_through_the_public_api() {
    for text in [
        "hello",
        "héllo 🌍 世界",
        "a=b c\nnewline\t\ttab",
        "\u{1b}[31mred\u{1b}[0m",
        "%25 not double encoded",
        "",
    ] {
        let encoded = nexus_headless::sanitize(text);
        assert!(encoded.is_ascii(), "encoded output is ASCII: {encoded}");
        assert!(!encoded.contains('\x1b'), "no raw ESC: {encoded}");
        assert_eq!(
            nexus_headless::percent_decode(&encoded).expect("decodes"),
            text
        );
    }
}

/// Non-UTF-8 argv is a usage error: exit 2, a static value-free diagnostic,
/// and no argv byte echoed back to stdout or stderr.
#[cfg(unix)]
#[test]
fn invalid_utf8_argv_exits_two_with_static_diagnostic_and_no_sentinel() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let sentinel = "SENTINEL_ARGV_SECRET";
    let mut raw = sentinel.as_bytes().to_vec();
    raw.extend_from_slice(&[0xFF, 0xFE, 0x80, b'\n', b'\x1b']);
    let output = Command::new(bin_path())
        .arg(OsString::from_vec(raw))
        .output()
        .expect("binary runs");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty(), "no stdout data on invalid argv");
    assert!(output.stderr.is_ascii(), "diagnostics stay ASCII");
    let stderr = String::from_utf8(output.stderr).expect("stderr is utf-8");
    assert!(
        !stderr.contains(sentinel),
        "argv bytes are never echoed: {stderr}"
    );
    assert!(
        !stderr.contains('\u{FFFD}'),
        "no lossy replacement character: {stderr}"
    );
    assert!(stderr.contains("valid UTF-8"), "{stderr}");
    assert!(stderr.contains("usage:"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

/// A stdout consumer that closes the pipe before the run finishes gets a
/// deliberate exit code instead of a write panic. The read end is closed
/// before the child starts, so the first stdout write fails with EPIPE.
#[cfg(unix)]
#[test]
fn broken_stdout_pipe_exits_without_panic() {
    use std::os::fd::OwnedFd;
    use std::process::Stdio;

    let (reader, writer) = std::io::pipe().expect("pipe opens");
    drop(reader);
    let writer: OwnedFd = writer.into();
    let child = Command::new(bin_path())
        .arg("hello")
        .stdout(Stdio::from(writer))
        .stderr(Stdio::piped())
        .spawn()
        .expect("binary spawns");
    let output = child.wait_with_output().expect("binary waits");
    assert_eq!(
        output.status.code(),
        Some(nexus_headless::EXIT_STDOUT_CLOSED),
        "broken pipe has a deliberate exit code"
    );
    assert!(
        output.stdout.is_empty(),
        "no stdout survived the closed pipe"
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr is utf-8");
    assert!(!stderr.contains("panicked"), "no panic output: {stderr}");
    assert!(
        !stderr.contains("Broken pipe") && !stderr.contains("BrokenPipe"),
        "closed consumer stays quiet: {stderr}"
    );
}
