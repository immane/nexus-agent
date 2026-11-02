//! Exercise real interactive startup with a blocking credential file and a local listener.

#[test]
#[cfg(unix)]
fn interactive_startup_defers_credential_file_and_never_auto_calls_selected_model() {
    let status = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/credential_startup.py"
        ))
        .arg(env!("CARGO_BIN_EXE_nexus-tui"))
        .arg(if cfg!(feature = "demo") {
            "demo"
        } else {
            "default"
        })
        .status()
        .expect("python3 is required for the credential startup regression");
    assert!(status.success());
}
