//! Terminal setup/teardown with restoration on every exit path.
//!
//! [`TerminalGuard::setup`] enters raw mode and the alternate screen;
//! [`TerminalGuard::teardown`] (also run from [`Drop`]) leaves both, and is
//! idempotent so normal returns, error returns, and panics all restore the
//! terminal. [`install_panic_hook`] restores first, then delegates to the
//! previous hook so panic reports still print.

use std::io::{self, Stdout};

use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// RAII terminal session. Setup and teardown are explicit; dropping the
/// guard always attempts teardown, so error paths cannot skip it.
#[derive(Debug)]
pub struct TerminalGuard {
    active: bool,
    stdout: Stdout,
}

impl TerminalGuard {
    /// Enters raw mode and the alternate screen.
    pub fn setup() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(Self {
            active: true,
            stdout,
        })
    }

    /// Leaves the alternate screen and raw mode. Safe to call repeatedly;
    /// errors are swallowed because teardown runs on failure paths where a
    /// second error must not mask the first.
    pub fn teardown(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let _ = execute!(self.stdout, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }

    /// Whether the terminal session is still held.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// Best-effort restoration for the panic path: no allocation, no panics,
/// errors ignored.
pub fn restore_for_panic() {
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
    let _ = disable_raw_mode();
}

/// Installs a panic hook that restores the terminal before delegating to
/// the previously installed hook.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_for_panic();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teardown_is_idempotent_and_panic_restore_never_panics() {
        // No terminal is entered here, so teardown must be a silent no-op.
        let mut guard = TerminalGuard {
            active: false,
            stdout: io::stdout(),
        };
        assert!(!guard.is_active());
        guard.teardown();
        guard.teardown();
        restore_for_panic();
    }
}
