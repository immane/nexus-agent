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

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// Process-wide ownership mirrors for the panic path; one session at a time.
static RAW_OWNED: AtomicBool = AtomicBool::new(false);
static ALTERNATE_OWNED: AtomicBool = AtomicBool::new(false);
static MOUSE_OWNED: AtomicBool = AtomicBool::new(false);

/// Separate ownership of raw mode, the alternate screen, and mouse capture
/// so cleanup never touches state this guard did not acquire.
#[derive(Debug, Clone, Copy, Default)]
struct Ownership {
    raw: bool,
    alternate: bool,
    mouse: bool,
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
        MOUSE_OWNED.store(self.mouse, Ordering::SeqCst);
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
        self.mouse &= MOUSE_OWNED.load(Ordering::SeqCst);
    }
}

/// The terminal operations a guard performs. Production calls
/// crossterm; tests inject failures through this narrow boundary instead of
/// using a terminal or a general mocking framework.
trait TerminalIo {
    fn enable_raw(&mut self) -> io::Result<()>;
    fn disable_raw(&mut self) -> io::Result<()>;
    fn enter_alternate(&mut self) -> io::Result<()>;
    fn leave_alternate(&mut self) -> io::Result<()>;
    fn enable_mouse(&mut self) -> io::Result<()>;
    fn disable_mouse(&mut self) -> io::Result<()>;
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

    fn enable_mouse(&mut self) -> io::Result<()> {
        execute!(self.0, EnableMouseCapture)
    }

    fn disable_mouse(&mut self) -> io::Result<()> {
        execute!(self.0, DisableMouseCapture)
    }
}

/// Acquires raw mode, the alternate screen, and mouse capture, recording each
/// resource at the attempt: a write that fails while flushing can still have
/// emitted part of the sequence, so rollback must cover it. On failure the
/// partial acquisition is rolled back; steps whose rollback fails stay owned.
fn acquire(io: &mut impl TerminalIo, owned: &mut Ownership) -> io::Result<()> {
    io.enable_raw()?;
    owned.raw = true;
    owned.alternate = true;
    owned.mouse = true;
    if let Err(error) = io.enter_alternate().and_then(|()| io.enable_mouse()) {
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
    if owned.mouse {
        match io.disable_mouse() {
            Ok(()) => owned.mouse = false,
            Err(error) => first_error = Some(error),
        }
    }
    if owned.alternate {
        match io.leave_alternate() {
            Ok(()) => owned.alternate = false,
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
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
        self.owned.raw || self.owned.alternate || self.owned.mouse
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
        mouse: MOUSE_OWNED.load(Ordering::SeqCst),
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

        fn enable_mouse(&mut self) -> io::Result<()> {
            self.record("enable_mouse")
        }

        fn disable_mouse(&mut self) -> io::Result<()> {
            self.record("disable_mouse")
        }
    }

    /// Both resources acquired but not yet published (as during setup).
    fn owned() -> Ownership {
        Ownership {
            raw: true,
            alternate: true,
            mouse: true,
            published: false,
        }
    }

    #[test]
    fn normal_setup_and_restore_release_both_resources() {
        let mut io = FakeIo::default();
        let mut state = Ownership::default();
        acquire(&mut io, &mut state).expect("acquire succeeds");
        assert!(state.raw && state.alternate && state.mouse);
        restore(&mut io, &mut state).expect("restore succeeds");
        assert!(!state.raw && !state.alternate && !state.mouse);
        assert_eq!(
            io.calls,
            [
                "enable_raw",
                "enter_alternate",
                "enable_mouse",
                "disable_mouse",
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
        // rollback attempts the leave as well as disabling raw mode. The
        // enter failure happens before mouse capture is attempted.
        assert_eq!(
            io.calls,
            [
                "enable_raw",
                "enter_alternate",
                "disable_mouse",
                "leave_alternate",
                "disable_raw"
            ]
        );
        assert!(
            !state.raw && !state.alternate && !state.mouse,
            "rollback cleared ownership"
        );
    }

    #[test]
    fn failed_mouse_capture_rolls_back_the_full_setup() {
        let mut io = FakeIo {
            failures: vec!["enable_mouse"],
            ..FakeIo::default()
        };
        let mut state = Ownership::default();
        let error = acquire(&mut io, &mut state).expect_err("mouse failure propagates");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(
            io.calls,
            [
                "enable_raw",
                "enter_alternate",
                "enable_mouse",
                "disable_mouse",
                "leave_alternate",
                "disable_raw"
            ]
        );
        assert!(
            !state.raw && !state.alternate && !state.mouse,
            "rollback cleared ownership"
        );
    }

    #[test]
    fn failed_enter_and_failed_rollback_stay_owned_for_retry() {
        let mut io = FakeIo {
            failures: vec!["enter_alternate", "leave_alternate", "disable_raw"],
            ..FakeIo::default()
        };
        let mut state = Ownership::default();
        assert!(acquire(&mut io, &mut state).is_err());
        assert!(
            state.raw && state.alternate && !state.mouse,
            "failed cleanup stays owned; mouse capture was released"
        );
        io.failures.clear();
        restore(&mut io, &mut state).expect("retry succeeds");
        assert!(!state.raw && !state.alternate && !state.mouse);
        assert_eq!(
            io.calls,
            [
                "enable_raw",
                "enter_alternate",
                "disable_mouse",
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
        assert_eq!(
            io.calls,
            ["disable_mouse", "leave_alternate", "disable_raw"]
        );
        assert!(!state.raw, "raw mode was still released");
        assert!(!state.mouse, "mouse capture was still released");
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
        MOUSE_OWNED.store(true, Ordering::SeqCst);

        let mut io = FakeIo::default();
        restore_published(&mut io);
        assert_eq!(
            io.calls,
            ["disable_mouse", "leave_alternate", "disable_raw"]
        );
        assert!(!RAW_OWNED.load(Ordering::SeqCst));
        assert!(!ALTERNATE_OWNED.load(Ordering::SeqCst));
        assert!(!MOUSE_OWNED.load(Ordering::SeqCst));

        // Failed panic cleanup stays published for the guard's Drop retry.
        RAW_OWNED.store(true, Ordering::SeqCst);
        ALTERNATE_OWNED.store(true, Ordering::SeqCst);
        MOUSE_OWNED.store(true, Ordering::SeqCst);
        let mut failing = FakeIo {
            failures: vec!["leave_alternate"],
            ..FakeIo::default()
        };
        restore_published(&mut failing);
        assert_eq!(
            failing.calls,
            ["disable_mouse", "leave_alternate", "disable_raw"]
        );
        assert!(!RAW_OWNED.load(Ordering::SeqCst), "raw mode released");
        assert!(
            !MOUSE_OWNED.load(Ordering::SeqCst),
            "mouse capture released"
        );
        assert!(
            ALTERNATE_OWNED.load(Ordering::SeqCst),
            "failed step kept for retry"
        );

        RAW_OWNED.store(false, Ordering::SeqCst);
        ALTERNATE_OWNED.store(false, Ordering::SeqCst);
        MOUSE_OWNED.store(false, Ordering::SeqCst);
    }

    #[test]
    fn panic_before_first_publish_keeps_acquired_ownership_and_retries() {
        let _lock = PUBLISHED_FLAGS.lock().expect("flag lock");
        RAW_OWNED.store(false, Ordering::SeqCst);
        ALTERNATE_OWNED.store(false, Ordering::SeqCst);
        MOUSE_OWNED.store(false, Ordering::SeqCst);

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

            fn enable_mouse(&mut self) -> io::Result<()> {
                Ok(())
            }

            fn disable_mouse(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut state = Ownership::default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            acquire(&mut PanickingIo, &mut state)
        }));
        assert!(result.is_err(), "simulated panic propagates");
        assert!(
            state.raw && state.alternate && state.mouse,
            "resources acquired before the panic are recorded"
        );

        // Drop runs while unwinding: the sync must not intersect the
        // unpublished bits with the still-false published flags.
        state.sync_from_published();
        assert!(
            state.raw && state.alternate && state.mouse,
            "acquired-unpublished ownership is never lost"
        );

        // Faulted cleanup keeps the still-owned steps; mouse capture was
        // already released by the faulted attempt, so the retry covers only
        // the screen and raw mode.
        let mut failing = FakeIo {
            failures: vec!["leave_alternate", "disable_raw"],
            ..FakeIo::default()
        };
        assert!(restore(&mut failing, &mut state).is_err());
        assert!(
            state.raw && state.alternate && !state.mouse,
            "failed cleanup stays owned"
        );
        let mut healthy = FakeIo::default();
        restore(&mut healthy, &mut state).expect("retry restores");
        assert!(!state.raw && !state.alternate && !state.mouse);
        assert_eq!(healthy.calls, ["leave_alternate", "disable_raw"]);
    }

    #[test]
    fn published_ownership_tracks_the_panic_release() {
        let _lock = PUBLISHED_FLAGS.lock().expect("flag lock");

        let mut state = owned();
        state.publish();
        assert!(state.published);
        // The panic path released every resource and published that.
        RAW_OWNED.store(false, Ordering::SeqCst);
        ALTERNATE_OWNED.store(false, Ordering::SeqCst);
        MOUSE_OWNED.store(false, Ordering::SeqCst);
        state.sync_from_published();
        assert!(
            !state.raw && !state.alternate && !state.mouse,
            "released bits stay released"
        );

        // A partially failed panic release keeps only the failed step.
        let mut state = owned();
        state.publish();
        RAW_OWNED.store(false, Ordering::SeqCst);
        ALTERNATE_OWNED.store(true, Ordering::SeqCst);
        MOUSE_OWNED.store(false, Ordering::SeqCst);
        state.sync_from_published();
        assert!(!state.raw && state.alternate, "failed step stays owned");
        assert!(!state.mouse, "released mouse capture stays released");

        RAW_OWNED.store(false, Ordering::SeqCst);
        ALTERNATE_OWNED.store(false, Ordering::SeqCst);
        MOUSE_OWNED.store(false, Ordering::SeqCst);
    }

    #[test]
    fn teardown_is_idempotent_for_an_inactive_guard() {
        let _lock = PUBLISHED_FLAGS.lock().expect("flag lock");
        RAW_OWNED.store(false, Ordering::SeqCst);
        ALTERNATE_OWNED.store(false, Ordering::SeqCst);
        MOUSE_OWNED.store(false, Ordering::SeqCst);
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

#[cfg(test)]
mod cov_terminal_private {
    //! Coverage hardening for the per-step ownership bookkeeping that the
    //! public API cannot reach without a real terminal: the exact `TerminalIo`
    //! call sequences, what stays owned when a step or a rollback fails, and
    //! the panic window between acquiring a resource and publishing it.
    //!
    //! Deterministic: every operation runs against a recording `TerminalIo`
    //! double, so no TTY, escape sequence, clock, or thread is involved.
    //! Every failing and panicking path is settled from the recorded call log
    //! and the resulting ownership bits.
    //!
    //! Scope limit, deliberate: the process-wide ownership flags are never
    //! read or written here. The sibling `tests` module publishes them under
    //! a lock private to that module, so a second module in the same test
    //! binary could not serialize with it and an unserialized publish would
    //! race its assertions. `TerminalGuard` is therefore only inspected
    //! through `is_active`, on guards that are explicitly forgotten rather
    //! than dropped, because `Drop` publishes ownership. The published-flag
    //! paths (`publish`, `sync_from_published` after a publish, and
    //! `restore_published`) are covered by that module.

    use super::*;

    /// Operations that acquire the session, in call order.
    const SETUP_ORDER: [&str; 3] = ["enable_raw", "enter_alternate", "enable_mouse"];
    /// Operations that release the session, in call order.
    const RELEASE_ORDER: [&str; 3] = ["disable_mouse", "leave_alternate", "disable_raw"];

    /// [`TerminalIo`] double that records every call and can fail or panic on
    /// a chosen operation.
    #[derive(Default)]
    struct RecordingIo {
        calls: Vec<&'static str>,
        failures: Vec<&'static str>,
        /// Operation that panics instead of returning, modelling an interrupt
        /// between a terminal write and its result.
        panic_on: Option<&'static str>,
    }

    impl RecordingIo {
        fn failing<const N: usize>(failures: [&'static str; N]) -> Self {
            Self {
                calls: Vec::new(),
                failures: failures.to_vec(),
                panic_on: None,
            }
        }

        fn panicking(operation: &'static str) -> Self {
            Self {
                calls: Vec::new(),
                failures: Vec::new(),
                panic_on: Some(operation),
            }
        }

        fn record(&mut self, operation: &'static str) -> io::Result<()> {
            self.calls.push(operation);
            if self.panic_on == Some(operation) {
                panic!("simulated interrupt during {operation}");
            }
            if self.failures.contains(&operation) {
                return Err(io::Error::other(format!("injected {operation} failure")));
            }
            Ok(())
        }
    }

    impl TerminalIo for RecordingIo {
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

        fn enable_mouse(&mut self) -> io::Result<()> {
            self.record("enable_mouse")
        }

        fn disable_mouse(&mut self) -> io::Result<()> {
            self.record("disable_mouse")
        }
    }

    /// Both resources acquired but not yet published, as during setup.
    fn acquired_unpublished() -> Ownership {
        Ownership {
            raw: true,
            alternate: true,
            mouse: true,
            published: false,
        }
    }

    #[test]
    fn setup_then_restore_releases_both_resources_in_order() {
        let mut io = RecordingIo::default();
        let mut owned = Ownership::default();
        assert!(!owned.published, "setup starts from an unpublished record");

        acquire(&mut io, &mut owned).expect("setup succeeds");
        assert_eq!(io.calls, SETUP_ORDER);
        assert!(
            owned.raw && owned.alternate && owned.mouse,
            "each step is recorded at the attempt"
        );
        assert!(
            !owned.published,
            "the ownership functions never publish; the guard does"
        );

        restore(&mut io, &mut owned).expect("restore succeeds");
        assert_eq!(io.calls, [SETUP_ORDER, RELEASE_ORDER].concat());
        assert!(
            !owned.raw && !owned.alternate && !owned.mouse,
            "every step released"
        );

        // Releasing an already released session writes nothing.
        restore(&mut io, &mut owned).expect("nothing left to restore");
        assert_eq!(
            io.calls,
            [SETUP_ORDER, RELEASE_ORDER].concat(),
            "a second restore over a released session performs no writes"
        );
    }

    #[test]
    fn a_released_record_can_be_acquired_again() {
        let mut io = RecordingIo::default();
        let mut owned = Ownership::default();
        for _ in 0..2 {
            acquire(&mut io, &mut owned).expect("setup succeeds");
            assert!(owned.raw && owned.alternate && owned.mouse);
            restore(&mut io, &mut owned).expect("restore succeeds");
            assert!(!owned.raw && !owned.alternate && !owned.mouse);
        }
        assert_eq!(
            io.calls,
            [SETUP_ORDER, RELEASE_ORDER, SETUP_ORDER, RELEASE_ORDER].concat(),
            "the bookkeeping is per session, not one-shot"
        );
    }

    #[test]
    fn failed_enable_raw_records_no_ownership_and_skips_rollback() {
        let mut io = RecordingIo::failing(["enable_raw"]);
        let mut owned = Ownership::default();
        let error = acquire(&mut io, &mut owned).expect_err("raw-mode failure surfaces");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(
            error.to_string().contains("enable_raw"),
            "the failing step is named"
        );
        assert_eq!(
            io.calls,
            ["enable_raw"],
            "nothing was acquired, so there is nothing to roll back"
        );
        assert!(
            !owned.raw && !owned.alternate && !owned.mouse,
            "an unacquired resource is never recorded as owned"
        );
    }

    #[test]
    fn failed_enter_rolls_back_the_partial_setup_and_returns_the_enter_error() {
        let mut io = RecordingIo::failing(["enter_alternate"]);
        let mut owned = Ownership::default();
        let error = acquire(&mut io, &mut owned).expect_err("enter failure surfaces");
        assert!(
            error.to_string().contains("enter_alternate"),
            "the caller sees the enter failure, not the rollback's"
        );
        // A failed flush may already have emitted part of the sequence, so
        // rollback covers the alternate screen as well as raw mode. The
        // enter failure happens before mouse capture is attempted.
        assert_eq!(
            io.calls,
            [
                "enable_raw",
                "enter_alternate",
                "disable_mouse",
                "leave_alternate",
                "disable_raw"
            ]
        );
        assert!(
            !owned.raw && !owned.alternate && !owned.mouse,
            "a complete rollback leaves nothing owned"
        );
    }

    #[test]
    fn failed_screen_rollback_keeps_only_the_screen_owned_for_a_retry() {
        let mut io = RecordingIo::failing(["enter_alternate", "leave_alternate"]);
        let mut owned = Ownership::default();
        let error = acquire(&mut io, &mut owned).expect_err("enter failure surfaces");
        assert!(
            error.to_string().contains("enter_alternate"),
            "the rollback error is discarded, not reported as the setup error"
        );
        assert!(!owned.raw, "the raw-mode rollback succeeded");
        assert!(owned.alternate, "the failed leave stays owned");
        assert!(!owned.mouse, "mouse capture was released");

        let mut retry = RecordingIo::default();
        restore(&mut retry, &mut owned).expect("the retry succeeds");
        assert_eq!(
            retry.calls,
            ["leave_alternate"],
            "only the step that is still owned is retried"
        );
        assert!(!owned.raw && !owned.alternate && !owned.mouse);
    }

    #[test]
    fn failed_raw_mode_rollback_keeps_raw_mode_owned_and_releases_the_screen() {
        let mut io = RecordingIo::failing(["enter_alternate", "disable_raw"]);
        let mut owned = Ownership::default();
        assert!(acquire(&mut io, &mut owned).is_err());
        assert!(owned.raw, "the failed raw-mode rollback stays owned");
        assert!(!owned.alternate, "the screen rollback succeeded");
        assert!(!owned.mouse, "mouse capture was released");

        let mut retry = RecordingIo::default();
        restore(&mut retry, &mut owned).expect("the retry succeeds");
        assert_eq!(retry.calls, ["disable_raw"]);
        assert!(!owned.raw && !owned.alternate && !owned.mouse);
    }

    #[test]
    fn restore_attempts_every_owned_step_after_a_failure_and_keeps_only_the_failed_one() {
        let mut io = RecordingIo::failing(["leave_alternate"]);
        let mut owned = acquired_unpublished();
        let error = restore(&mut io, &mut owned).expect_err("the failure surfaces");
        assert!(
            error.to_string().contains("leave_alternate"),
            "the failed step's error is reported"
        );
        assert_eq!(
            io.calls, RELEASE_ORDER,
            "a failed step does not skip the step after it"
        );
        assert!(!owned.raw, "the step after the failure still ran");
        assert!(!owned.mouse, "mouse capture was still released");
        assert!(owned.alternate, "only the failed step stays owned");

        let mut retry = RecordingIo::default();
        restore(&mut retry, &mut owned).expect("the retry succeeds");
        assert_eq!(retry.calls, ["leave_alternate"]);
        assert!(!owned.raw && !owned.alternate && !owned.mouse);
    }

    #[test]
    fn restore_returns_the_first_error_and_retries_every_failed_step() {
        let mut io = RecordingIo::failing(["leave_alternate", "disable_raw"]);
        let mut owned = acquired_unpublished();
        let error = restore(&mut io, &mut owned).expect_err("both steps failed");
        assert!(
            error.to_string().contains("leave_alternate"),
            "the first failure is the one reported"
        );
        assert_eq!(io.calls, RELEASE_ORDER);
        assert!(
            owned.raw && owned.alternate && !owned.mouse,
            "both failed steps stay owned"
        );

        let mut retry = RecordingIo::default();
        restore(&mut retry, &mut owned).expect("the retry succeeds");
        assert_eq!(
            retry.calls,
            ["leave_alternate", "disable_raw"],
            "the retry reattempts every step that is still owned"
        );
        assert!(!owned.raw && !owned.alternate && !owned.mouse);
    }

    #[test]
    fn restore_reports_the_only_owned_step_and_never_touches_the_other_one() {
        // Raw mode still owned: the screen is never written to, and its state
        // cannot be reported because it was never attempted.
        let mut raw_only = RecordingIo::failing(["disable_raw"]);
        let mut owned = Ownership {
            raw: true,
            alternate: false,
            mouse: false,
            published: false,
        };
        let error = restore(&mut raw_only, &mut owned).expect_err("raw-mode failure surfaces");
        assert!(error.to_string().contains("disable_raw"));
        assert_eq!(
            raw_only.calls,
            ["disable_raw"],
            "cleanup never touches state this guard did not acquire"
        );
        assert!(owned.raw, "the failed step stays owned");
        assert!(!owned.alternate);

        // Screen still owned: raw mode is never disabled, not even on failure.
        let mut screen_only = RecordingIo::failing(["leave_alternate"]);
        let mut owned = Ownership {
            raw: false,
            alternate: true,
            mouse: false,
            published: false,
        };
        let error = restore(&mut screen_only, &mut owned).expect_err("leave failure surfaces");
        assert!(error.to_string().contains("leave_alternate"));
        assert_eq!(screen_only.calls, ["leave_alternate"]);
        assert!(owned.alternate, "the failed step stays owned");
        assert!(!owned.raw);
    }

    #[test]
    fn restore_without_ownership_performs_no_writes() {
        let mut io = RecordingIo::default();
        let mut owned = Ownership::default();
        restore(&mut io, &mut owned).expect("nothing to restore is not an error");
        assert!(
            io.calls.is_empty(),
            "a session that owns nothing emits no escape sequences"
        );
        assert!(!owned.raw && !owned.alternate && !owned.published);
    }

    #[test]
    fn panic_during_setup_keeps_acquired_ownership_for_the_teardown_path() {
        let mut io = RecordingIo::panicking("enter_alternate");
        let mut owned = Ownership::default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            acquire(&mut io, &mut owned)
        }));
        assert!(result.is_err(), "the interrupt propagates");
        assert_eq!(
            io.calls,
            ["enable_raw", "enter_alternate"],
            "the step after the interrupt is never attempted"
        );
        assert!(
            owned.raw && owned.alternate && owned.mouse,
            "resources acquired before the interrupt stay recorded for `Drop`"
        );
        assert!(
            !owned.published,
            "nothing was published, so the panic path cannot have released it"
        );

        // The guard syncs from the published flags before cleaning up:
        // ownership that was never published must survive that sync, or the
        // teardown would silently skip the resources it acquired.
        owned.sync_from_published();
        assert!(
            owned.raw && owned.alternate,
            "unpublished ownership is never lost"
        );
        assert!(!owned.published);

        // A faulted teardown keeps the still-owned steps; mouse capture was
        // already released by the faulted attempt, so the retry covers only
        // the screen and raw mode.
        let mut failing = RecordingIo::failing(["leave_alternate", "disable_raw"]);
        assert!(restore(&mut failing, &mut owned).is_err());
        assert!(
            owned.raw && owned.alternate && !owned.mouse,
            "a failed teardown stays owned"
        );

        let mut healthy = RecordingIo::default();
        restore(&mut healthy, &mut owned).expect("the retry restores");
        assert_eq!(healthy.calls, ["leave_alternate", "disable_raw"]);
        assert!(!owned.raw && !owned.alternate && !owned.mouse);
    }

    #[test]
    fn panic_before_any_acquisition_records_no_ownership() {
        let mut io = RecordingIo::panicking("enable_raw");
        let mut owned = Ownership::default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            acquire(&mut io, &mut owned)
        }));
        assert!(result.is_err());
        assert_eq!(io.calls, ["enable_raw"], "no later step was attempted");
        assert!(
            !owned.raw && !owned.alternate,
            "nothing was acquired, so nothing is owned"
        );

        let mut teardown = RecordingIo::default();
        restore(&mut teardown, &mut owned).expect("nothing to restore");
        assert!(
            teardown.calls.is_empty(),
            "the teardown of an interrupted setup writes nothing"
        );
    }

    #[test]
    fn panic_during_restore_leaves_the_remaining_steps_owned() {
        // Documented limitation, pinned rather than endorsed: `restore`
        // propagates a panic out of an operation, so the step after it is not
        // attempted in that call. Ownership is left untouched, which is what
        // keeps the retry honest.
        let mut io = RecordingIo::panicking("leave_alternate");
        let mut owned = acquired_unpublished();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            restore(&mut io, &mut owned)
        }));
        assert!(result.is_err(), "the panic propagates out of restore");
        assert_eq!(io.calls, ["disable_mouse", "leave_alternate"]);
        assert!(owned.alternate, "the interrupted step stays owned");
        assert!(owned.raw, "the unattempted step stays owned");
        assert!(!owned.mouse, "the completed step was released");

        let mut retry = RecordingIo::default();
        restore(&mut retry, &mut owned).expect("the retry restores both");
        assert_eq!(retry.calls, ["leave_alternate", "disable_raw"]);
        assert!(!owned.raw && !owned.alternate && !owned.mouse);
    }

    #[test]
    fn is_active_reports_any_retained_resource() {
        // `mem::forget` keeps `Drop` — which publishes ownership — out of
        // this module; see the scope limit above.
        fn active(raw: bool, alternate: bool, mouse: bool) -> bool {
            let guard = TerminalGuard {
                owned: Ownership {
                    raw,
                    alternate,
                    mouse,
                    published: false,
                },
                stdout: io::stdout(),
            };
            let active = guard.is_active();
            std::mem::forget(guard);
            active
        }
        assert!(!active(false, false, false), "an unowned guard is inactive");
        assert!(
            active(true, false, false),
            "retained raw mode keeps it active"
        );
        assert!(
            active(false, true, false),
            "a retained alternate screen keeps it active"
        );
        assert!(
            active(false, false, true),
            "retained mouse capture keeps it active"
        );
        assert!(active(true, true, true));
    }

    #[test]
    fn ownership_copies_are_independent_and_default_to_nothing_owned() {
        let mut owned = acquired_unpublished();
        let copy = owned;
        assert!(copy.raw && copy.alternate && !copy.published);
        owned.raw = false;
        assert!(copy.raw, "a copy keeps the value it was taken from");
        assert!(!owned.raw);
        assert!(
            !Ownership::default().published,
            "a fresh record is unpublished, so `setup` owns nothing yet"
        );
    }
}
