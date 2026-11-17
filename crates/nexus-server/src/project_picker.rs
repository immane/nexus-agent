//! Native folder selection on the server's desktop; never a shell or upload.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::projects::MAX_PROJECT_PATH_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickerError {
    Unavailable,
    Failed,
    TimedOut,
}

impl PickerError {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Unavailable => {
                "Native folder selection requires a desktop session on the server (macOS, or Linux with zenity/kdialog)."
            }
            Self::Failed => {
                "The folder dialog could not complete. Check the server's desktop session and try again."
            }
            Self::TimedOut => "Folder selection timed out. Open the folder dialog again.",
        }
    }
}

pub(crate) fn pick_folder() -> Result<Option<PathBuf>, PickerError> {
    #[cfg(target_os = "macos")]
    {
        run_dialog(
            "/usr/bin/osascript",
            &[
                "-e",
                "try\nreturn POSIX path of (choose folder with prompt \"Choose a project folder for Nexus\")\non error number -128\nreturn \"\"\nend try",
            ],
            Duration::from_secs(120),
        )
    }
    #[cfg(target_os = "linux")]
    {
        match run_dialog(
            "zenity",
            &[
                "--file-selection",
                "--directory",
                "--title=Choose a project folder for Nexus",
            ],
            Duration::from_secs(120),
        ) {
            Err(PickerError::Unavailable) => run_dialog(
                "kdialog",
                &[
                    "--getexistingdirectory",
                    ".",
                    "--title",
                    "Choose a project folder for Nexus",
                ],
                Duration::from_secs(120),
            ),
            result => result,
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(PickerError::Unavailable)
    }
}

fn run_dialog(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<Option<PathBuf>, PickerError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Desktop routing only; do not pass server/provider credentials to the picker.
    for name in [
        "HOME",
        "PATH",
        "TMPDIR",
        "LANG",
        "LC_ALL",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "DBUS_SESSION_BUS_ADDRESS",
        "XDG_RUNTIME_DIR",
        "XAUTHORITY",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            PickerError::Unavailable
        } else {
            PickerError::Failed
        }
    })?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(if result.is_err() {
                    PickerError::Failed
                } else {
                    PickerError::TimedOut
                });
            }
        }
    };
    let mut bytes = Vec::new();
    child
        .stdout
        .take()
        .ok_or(PickerError::Failed)?
        .take((MAX_PROJECT_PATH_BYTES + 3) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| PickerError::Failed)?;
    let mut diagnostic = Vec::new();
    child
        .stderr
        .take()
        .ok_or(PickerError::Failed)?
        .take(4096)
        .read_to_end(&mut diagnostic)
        .map_err(|_| PickerError::Failed)?;
    // Linux pickers report Cancel with exit 1 and no diagnostic. In particular,
    // a missing desktop/display must not silently masquerade as cancellation.
    if status.code() == Some(1)
        && program != "/usr/bin/osascript"
        && bytes.is_empty()
        && diagnostic.is_empty()
    {
        return Ok(None);
    }
    if !status.success() || bytes.len() > MAX_PROJECT_PATH_BYTES + 2 {
        return Err(PickerError::Failed);
    }
    let text = String::from_utf8(bytes).map_err(|_| PickerError::Failed)?;
    // Remove only the protocol newline. Other control characters cannot be
    // represented faithfully in the Web form's single-line path display.
    let text = text.strip_suffix('\n').unwrap_or(&text);
    if text.len() > MAX_PROJECT_PATH_BYTES || text.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(PickerError::Failed);
    }
    if text.is_empty() {
        Ok(None)
    } else {
        Ok(Some(PathBuf::from(text)))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn dialog_output_preserves_spaces_and_distinguishes_cancel_and_failure() {
        let run = |script| run_dialog("/bin/sh", &["-c", script], Duration::from_secs(2));
        assert_eq!(
            run("printf '/project with spaces/\\n'").unwrap(),
            Some(PathBuf::from("/project with spaces/"))
        );
        assert_eq!(run("printf '\\n'").unwrap(), None);
        assert_eq!(run("exit 1").unwrap(), None);
        assert_eq!(run("exit 2"), Err(PickerError::Failed));
        assert_eq!(
            run("echo 'no desktop' >&2; exit 1"),
            Err(PickerError::Failed)
        );
        assert_eq!(run("printf '\\377'"), Err(PickerError::Failed));
        assert_eq!(run("printf '/folder\\n\\n'"), Err(PickerError::Failed));
        assert_eq!(run("head -c 5000 /dev/zero"), Err(PickerError::Failed));
    }

    #[test]
    fn dialog_is_bounded_and_missing_desktop_tools_are_explicit() {
        assert_eq!(
            run_dialog("/nexus/nonexistent-picker", &[], Duration::from_secs(1)),
            Err(PickerError::Unavailable)
        );
        assert_eq!(
            run_dialog(
                "/bin/sh",
                &["-c", "while :; do :; done"],
                Duration::from_millis(30)
            ),
            Err(PickerError::TimedOut)
        );
    }
}
