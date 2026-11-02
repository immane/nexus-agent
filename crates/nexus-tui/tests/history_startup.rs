//! Exercise default interactive startup against an isolated historical session.

#[test]
#[cfg(unix)]
fn interactive_startup_does_not_restore_historical_body() {
    let status = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/history_startup.py"
        ))
        .arg(env!("CARGO_BIN_EXE_nexus-tui"))
        .status()
        .expect("python3 is required for the history startup regression");
    assert!(status.success());
}
