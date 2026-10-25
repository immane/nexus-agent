//! Real terminal regression: unlike TestBackend, this combines stdout/stderr
//! and checks the screen reconstructed from incremental terminal writes.

#[test]
#[cfg(unix)]
#[ignore = "requires python3 and a Unix pseudo-terminal"]
fn session_commands_preserve_the_terminal_screen() {
    let status = std::process::Command::new("python3")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/pty_render.py"))
        .arg(env!("CARGO_BIN_EXE_nexus-tui"))
        .status()
        .expect("python3 is required for the PTY rendering regression");
    assert!(status.success(), "PTY rendering regression failed");
}
