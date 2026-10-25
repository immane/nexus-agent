#![forbid(unsafe_code)]

//! Startup failures must not silently disable explicitly requested persistence.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn an_unopenable_session_store_aborts_startup_without_disclosing_its_path() {
    let directory =
        std::env::temp_dir().join(format!("nexus-server-store-startup-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let config = directory.join("config.json");
    nexus_config::save(&nexus_config::UserConfig::default_config(), &config).unwrap();
    let blocked = directory.join("private-store-path");
    std::fs::write(&blocked, "not a directory").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_nexus-server"))
        .arg("--config")
        .arg(&config)
        .args(["--port", "0", "--session-store"])
        .arg(&blocked)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!(
                "invalid session store must abort startup rather than serve without persistence"
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "nexus-server: cannot open session store\n"
    );
    std::fs::remove_dir_all(directory).unwrap();
}
