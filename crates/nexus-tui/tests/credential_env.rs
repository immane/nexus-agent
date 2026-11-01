//! Observe libc `getenv` names during real interactive startup and submission.

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn interactive_startup_never_looks_up_provider_credential_variables() {
    let status = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/credential_env.py"
        ))
        .arg(env!("CARGO_BIN_EXE_nexus-tui"))
        .status()
        .expect("python3 and cc are required for the credential lookup regression");
    assert!(status.success());
}
