//! The current demo startup still validates user configuration before choosing
//! the scripted headless path. This is not the historical M0 no-file contract.

#[test]
#[cfg(feature = "demo")]
fn headless_demo_rejects_invalid_user_configuration() {
    use std::fs;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after the epoch")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "nexus-tui-demo-config-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&directory).expect("create isolated directory");
    let config = directory.join("config.json");
    fs::write(&config, b"not valid json").expect("write synthetic config");

    let output = Command::new(env!("CARGO_BIN_EXE_nexus-tui"))
        .arg("--demo")
        .env_clear()
        .env("HOME", &directory)
        .env("XDG_CONFIG_HOME", &directory)
        .env("NEXUS_CONFIG", &config)
        .current_dir(&directory)
        .output()
        .expect("run demo without a terminal");

    fs::remove_file(&config).expect("remove synthetic config");
    fs::remove_dir(&directory).expect("remove isolated directory");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid user configuration"), "{stderr}");
    assert!(output.stdout.is_empty());
}

#[test]
#[cfg(not(feature = "demo"))]
fn demo_is_rejected_without_feature() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_nexus-tui"))
        .arg("--demo")
        .output()
        .expect("run TUI");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Demo support: disabled"), "{stderr}");
    assert!(stderr.contains("--features demo"), "{stderr}");
}
