//! Terminal setup/teardown with restoration on every exit path.
//!
//! [`TerminalGuard::setup`] enters raw mode and the alternate screen and
//! records ownership of each resource separately. [`TerminalGuard::restore`]
//! attempts every owned step even when an earlier step fails and keeps a
//! failed step owned so a later call (including [`Drop`]) retries it.
//! [`TerminalGuard::teardown`] keeps the previous best-effort behavior for
//! existing callers. [`install_panic_hook`] restores an owned session before
//! delegating to the previous hook and performs no terminal writes when no
//! session is owned, so headless panic output stays free of escape codes.
//!
//! One terminal session per process is supported (M0): the panic path reads
//! the same ownership flags the guard publishes. Ownership acquired before
//! the first publish stays with the guard even if a panic interrupts setup:
//! the panic-path sync is skipped until the guard has published once, so
//! `Drop` still restores it.

use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// Process-wide ownership mirrors for the panic path; one session at a time.
static RAW_OWNED: AtomicBool = AtomicBool::new(false);
static ALTERNATE_OWNED: AtomicBool = AtomicBool::new(false);

/// Separate ownership of raw mode and the alternate screen so cleanup never
/// touches state this guard did not acquire.
#[derive(Debug, Clone, Copy, Default)]
struct Ownership {
    raw: bool,
    alternate: bool,
    /// True once these bits have been mirrored into the process-wide flags.
    /// Until the first publish the panic path cannot have released them, so
    /// [`Ownership::sync_from_published`] must leave them alone.
    published: bool,
}

impl Ownership {
    /// Publishes current ownership for the panic path.
    fn publish(&mut self) {
        RAW_OWNED.store(self.raw, Ordering::SeqCst);
        ALTERNATE_OWNED.store(self.alternate, Ordering::SeqCst);
        self.published = true;
    }

    /// Intersects ownership with the published flags: a resource already
    /// released by the panic path must not be touched again. Before the
    /// first publish nothing was visible to the panic path, so the guard's
    /// own record is authoritative and must never be cleared here.
    fn sync_from_published(&mut self) {
        if !self.published {
            return;
        }
        self.raw &= RAW_OWNED.load(Ordering::SeqCst);
        self.alternate &= ALTERNATE_OWNED.load(Ordering::SeqCst);
    }
}

/// The four terminal operations a guard performs. Production calls
/// crossterm; tests inject failures through this narrow boundary instead of
/// using a terminal or a general mocking framework.
trait TerminalIo {
    fn enable_raw(&mut self) -> io::Result<()>;
    fn disable_raw(&mut self) -> io::Result<()>;
    fn enter_alternate(&mut self) -> io::Result<()>;
    fn leave_alternate(&mut self) -> io::Result<()>;
}

/// Crossterm-backed [`TerminalIo`] over stdout.
struct CrosstermIo<'a>(&'a mut Stdout);

impl TerminalIo for CrosstermIo<'_> {
    fn enable_raw(&mut self) -> io::Result<()> {
        enable_raw_mode()
    }

    fn disable_raw(&mut self) -> io::Result<()> {
        disable_raw_mode()
    }

    fn enter_alternate(&mut self) -> io::Result<()> {
        execute!(self.0, EnterAlternateScreen)
    }

    fn leave_alternate(&mut self) -> io::Result<()> {
        execute!(self.0, LeaveAlternateScreen)
    }
}

/// Acquires raw mode and the alternate screen, recording each resource at
/// the attempt: a write that fails while flushing can still have emitted
/// part of the sequence, so rollback must cover it. On failure the partial
/// acquisition is rolled back; steps whose rollback fails stay owned.
fn acquire(io: &mut impl TerminalIo, owned: &mut Ownership) -> io::Result<()> {
    io.enable_raw()?;
    owned.raw = true;
    owned.alternate = true;
    if let Err(error) = io.enter_alternate() {
        let _ = restore(io, owned);
        return Err(error);
    }
    Ok(())
}

/// Attempts every owned restoration step even when an earlier step fails,
/// and clears ownership only for steps that succeeded. A failed step stays
/// owned so the next `restore` (or `Drop`) retries it. Returns the first
/// error.
fn restore(io: &mut impl TerminalIo, owned: &mut Ownership) -> io::Result<()> {
    let mut first_error = None;
    if owned.alternate {
        match io.leave_alternate() {
            Ok(()) => owned.alternate = false,
            Err(error) => first_error = Some(error),
        }
    }
    if owned.raw {
        match io.disable_raw() {
            Ok(()) => owned.raw = false,
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// RAII terminal session. Setup and teardown are explicit; dropping the
/// guard always attempts teardown, and a failed step stays owned so the
/// next attempt can retry it.
#[derive(Debug)]
pub struct TerminalGuard {
    owned: Ownership,
    stdout: Stdout,
}

impl TerminalGuard {
    /// Enters raw mode and the alternate screen.
    pub fn setup() -> io::Result<Self> {
        let mut guard = Self {
            owned: Ownership::default(),
            stdout: io::stdout(),
        };
        let result = {
            let mut io = CrosstermIo(&mut guard.stdout);
            acquire(&mut io, &mut guard.owned)
        };
        guard.owned.publish();
        result?;
        Ok(guard)
    }

    /// Leaves the alternate screen and disables raw mode, attempting every
    /// owned step even when an earlier step fails. A failed step stays owned
    /// so a later call retries it. Returns the first error.
    pub fn restore(&mut self) -> io::Result<()> {
        self.owned.sync_from_published();
        let result = {
            let mut io = CrosstermIo(&mut self.stdout);
            restore(&mut io, &mut self.owned)
        };
        self.owned.publish();
        result
    }

    /// Best-effort restoration for existing callers and [`Drop`]: identical
    /// to [`TerminalGuard::restore`] but the error is discarded. A failed
    /// step stays owned and is retried on the next call.
    pub fn teardown(&mut self) {
        let _ = self.restore();
    }

    /// Whether this guard still owns terminal state.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.owned.raw || self.owned.alternate
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// Restores whatever ownership is currently published, performing no
/// terminal writes when none is (headless runs).
fn restore_published(io: &mut impl TerminalIo) {
    let mut owned = Ownership {
        raw: RAW_OWNED.load(Ordering::SeqCst),
        alternate: ALTERNATE_OWNED.load(Ordering::SeqCst),
        published: true,
    };
    let _ = restore(io, &mut owned);
    owned.publish();
}

/// Best-effort restoration for the panic path: no allocation, no panics,
/// errors ignored. A step that fails stays published so the guard's `Drop`
/// can retry it; with no owned session this writes nothing.
pub fn restore_for_panic() {
    let mut stdout = io::stdout();
    restore_published(&mut CrosstermIo(&mut stdout));
}

/// Installs a panic hook that restores an owned terminal session before
/// delegating to the previously installed hook. Headless runs own nothing,
/// so the hook emits no escape sequences there.
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
    use std::sync::Mutex;

    /// Serializes the tests that read or write the process-wide ownership
    /// flags used by the panic path.
    static PUBLISHED_FLAGS: Mutex<()> = Mutex::new(());

    #[derive(Default)]
    struct FakeIo {
        calls: Vec<&'static str>,
        failures: Vec<&'static str>,
    }

    impl FakeIo {
        fn record(&mut self, operation: &'static str) -> io::Result<()> {
            self.calls.push(operation);
            if self.failures.contains(&operation) {
                Err(io::Error::other(format!("injected {operation} failure")))
            } else {
                Ok(())
            }
        }
    }

    impl TerminalIo for FakeIo {
        fn enable_raw(&mut self) -> io::Result<()> {
            self.record("enable_raw")
        }

        fn disable_raw(&mut self) -> io::Result<()> {
            self.record("disable_raw")
        }

        fn enter_alternate(&mut self) -> io::Result<()> {
            self.record("enter_alternate")
        }

        fn leave_alternate(&mut self) -> io::Result<()> {
            self.record("leave_alternate")
        }
    }

    /// Both resources acquired but not yet published (as during setup).
    fn owned() -> Ownership {
        Ownership {
            raw: true,
            alternate: true,
            published: false,
        }
    }

    #[test]
    fn normal_setup_and_restore_release_both_resources() {
        let mut io = FakeIo::default();
        let mut state = Ownership::default();
        acquire(&mut io, &mut state).expect("acquire succeeds");
        assert!(state.raw && state.alternate);
        restore(&mut io, &mut state).expect("restore succeeds");
        assert!(!state.raw && !state.alternate);
        assert_eq!(
            io.calls,
            [
                "enable_raw",
                "enter_alternate",
                "leave_alternate",
                "disable_raw"
            ]
        );
    }

    #[test]
    fn failed_enter_rolls_back_a_partial_setup() {
        let mut io = FakeIo {
            failures: vec!["enter_alternate"],
            ..FakeIo::default()
        };
        let mut state = Ownership::default();
        let error = acquire(&mut io, &mut state).expect_err("enter failure propagates");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        // A failed flush may still have written part of the sequence, so
        // rollback attempts the leave as well as disabling raw mode.
        assert_eq!(
            io.calls,
            [
                "enable_raw",
                "enter_alternate",
                "leave_alternate",
                "disable_raw"
            ]
        );
        assert!(!state.raw && !state.alternate, "rollback cleared ownership");
    }

    #[test]
    fn failed_enter_and_failed_rollback_stay_owned_for_retry() {
        let mut io = FakeIo {
            failures: vec!["enter_alternate", "leave_alternate", "disable_raw"],
            ..FakeIo::default()
        };
        let mut state = Ownership::default();
        assert!(acquire(&mut io, &mut state).is_err());
        assert!(state.raw && state.alternate, "failed cleanup stays owned");
        io.failures.clear();
        restore(&mut io, &mut state).expect("retry succeeds");
        assert!(!state.raw && !state.alternate);
        assert_eq!(
            io.calls,
            [
                "enable_raw",
                "enter_alternate",
                "leave_alternate",
                "disable_raw",
                "leave_alternate",
                "disable_raw"
            ]
        );
    }

    #[test]
    fn restore_attempts_every_step_and_keeps_only_failed_steps_owned() {
        let mut io = FakeIo {
            failures: vec!["leave_alternate"],
            ..FakeIo::default()
        };
        let mut state = owned();
        let error = restore(&mut io, &mut state).expect_err("first error surfaces");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(io.calls, ["leave_alternate", "disable_raw"]);
        assert!(!state.raw, "raw mode was still released");
        assert!(state.alternate, "failed step stays owned for retry");
    }

    #[test]
    fn restore_without_ownership_performs_no_writes() {
        // Models the headless panic path: without an acquired session the
        // hook must not emit escape sequences.
        let mut io = FakeIo::default();
        let mut state = Ownership::default();
        restore(&mut io, &mut state).expect("nothing to restore");
        assert!(io.calls.is_empty());
    }

    #[test]
    fn panic_restore_uses_published_ownership() {
        let _lock = PUBLISHED_FLAGS.lock().expect("flag lock");
        RAW_OWNED.store(true, Ordering::SeqCst);
        ALTERNATE_OWNED.store(true, Ordering::SeqCst);

        let mut io = FakeIo::default();
        restore_published(&mut io);
        assert_eq!(io.calls, ["leave_alternate", "disable_raw"]);
        assert!(!RAW_OWNED.load(Ordering::SeqCst));
        assert!(!ALTERNATE_OWNED.load(Ordering::SeqCst));

        // Failed panic cleanup stays published for the guard's Drop retry.
        RAW_OWNED.store(true, Ordering::SeqCst);
        ALTERNATE_OWNED.store(true, Ordering::SeqCst);
        let mut failing = FakeIo {
            failures: vec!["leave_alternate"],
            ..FakeIo::default()
        };
        restore_published(&mut failing);
        assert_eq!(failing.calls, ["leave_alternate", "disable_raw"]);
        assert!(!RAW_OWNED.load(Ordering::SeqCst), "raw mode released");
        assert!(
            ALTERNATE_OWNED.load(Ordering::SeqCst),
            "failed step kept for retry"
        );

        RAW_OWNED.store(false, Ordering::SeqCst);
        ALTERNATE_OWNED.store(false, Ordering::SeqCst);
    }

    #[test]
    fn panic_before_first_publish_keeps_acquired_ownership_and_retries() {
        let _lock = PUBLISHED_FLAGS.lock().expect("flag lock");
        RAW_OWNED.store(false, Ordering::SeqCst);
        ALTERNATE_OWNED.store(false, Ordering::SeqCst);

        struct PanickingIo;

        impl TerminalIo for PanickingIo {
            fn enable_raw(&mut self) -> io::Result<()> {
                Ok(())
            }

            fn disable_raw(&mut self) -> io::Result<()> {
                Ok(())
            }

            fn enter_alternate(&mut self) -> io::Result<()> {
                panic!("simulated panic during alternate-screen setup");
            }

            fn leave_alternate(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut state = Ownership::default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            acquire(&mut PanickingIo, &mut state)
        }));
        assert!(result.is_err(), "simulated panic propagates");
        assert!(
            state.raw && state.alternate,
            "resources acquired before the panic are recorded"
        );

        // Drop runs while unwinding: the sync must not intersect the
        // unpublished bits with the still-false published flags.
        state.sync_from_published();
        assert!(
            state.raw && state.alternate,
            "acquired-unpublished ownership is never lost"
        );

        // Faulted cleanup keeps both steps owned; the retry releases them.
        let mut failing = FakeIo {
            failures: vec!["leave_alternate", "disable_raw"],
            ..FakeIo::default()
        };
        assert!(restore(&mut failing, &mut state).is_err());
        assert!(state.raw && state.alternate, "failed cleanup stays owned");
        let mut healthy = FakeIo::default();
        restore(&mut healthy, &mut state).expect("retry restores");
        assert!(!state.raw && !state.alternate);
        assert_eq!(healthy.calls, ["leave_alternate", "disable_raw"]);
    }

    #[test]
    fn published_ownership_tracks_the_panic_release() {
        let _lock = PUBLISHED_FLAGS.lock().expect("flag lock");

        let mut state = owned();
        state.publish();
        assert!(state.published);
        // The panic path released both resources and published that.
        RAW_OWNED.store(false, Ordering::SeqCst);
        ALTERNATE_OWNED.store(false, Ordering::SeqCst);
        state.sync_from_published();
        assert!(
            !state.raw && !state.alternate,
            "released bits stay released"
        );

        // A partially failed panic release keeps only the failed step.
        let mut state = owned();
        state.publish();
        RAW_OWNED.store(false, Ordering::SeqCst);
        ALTERNATE_OWNED.store(true, Ordering::SeqCst);
        state.sync_from_published();
        assert!(!state.raw && state.alternate, "failed step stays owned");

        RAW_OWNED.store(false, Ordering::SeqCst);
        ALTERNATE_OWNED.store(false, Ordering::SeqCst);
    }

    #[test]
    fn teardown_is_idempotent_for_an_inactive_guard() {
        let _lock = PUBLISHED_FLAGS.lock().expect("flag lock");
        RAW_OWNED.store(false, Ordering::SeqCst);
        ALTERNATE_OWNED.store(false, Ordering::SeqCst);
        let mut guard = TerminalGuard {
            owned: Ownership::default(),
            stdout: io::stdout(),
        };
        assert!(!guard.is_active());
        guard.teardown();
        guard.teardown();
        assert!(!guard.is_active());
    }
}
