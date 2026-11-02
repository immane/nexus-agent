//! Exercise input batching and cancellation while a local provider streams.

#[test]
#[cfg(unix)]
fn composer_input_and_cancel_progress_under_streaming_output() {
    let status = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/input_pressure.py"
        ))
        .arg(env!("CARGO_BIN_EXE_nexus-tui"))
        .arg(if cfg!(target_os = "linux") {
            "--both"
        } else {
            "--single"
        })
        .status()
        .expect("python3 is required for the input pressure regression");
    assert!(status.success());
}
