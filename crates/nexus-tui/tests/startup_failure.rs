//! Exercise failed interactive startup with an isolated session store.

#[test]
#[cfg(unix)]
fn corrupt_session_metadata_fails_without_recovery_or_provider_access() {
    let status = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/startup_failure.py"
        ))
        .arg(env!("CARGO_BIN_EXE_nexus-tui"))
        .status()
        .expect("python3 is required for the startup failure regression");
    assert!(status.success());
}
