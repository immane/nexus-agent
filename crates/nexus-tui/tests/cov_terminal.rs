//! Public-surface coverage for terminal setup and restoration
//! (`nexus_tui::terminal`).
//!
//! The unit tests inside `terminal.rs` drive the ownership bookkeeping
//! against a recording `TerminalIo` double. This file covers what an
//! integration test can actually observe: the headless half of the public
//! surface, plus the entry-point contracts callers depend on.
//!
//! - `install_panic_hook` passes the *original* payload through unchanged,
//!   delegates to the hook that was installed before it exactly once per
//!   panic — however many wrappers are composed and however many unwinding
//!   frames the panic crosses — and leaves the process able to keep running
//!   after a caught panic;
//! - the wrapper replaces the chain rather than appending to it, so the
//!   previously installed hook is the one reporting afterwards, and the hook
//!   can be installed again once it has been replaced;
//! - `restore_for_panic` with no owned session is a no-op: it returns, is
//!   idempotent, is safe to call while unwinding, is safe from a `Drop` that
//!   runs during unwinding (a panic there would abort instead of unwind), and
//!   is safe to call from several threads at once;
//! - the entry points keep their signatures — `restore`/`teardown` take
//!   `&mut self` so a failed step can be retried, `is_active` takes `&self` —
//!   and `TerminalGuard` stays `Send`/`Sync`/`UnwindSafe` so its `Drop` can
//!   still restore after a panic unwinds through its scope.
//!
//! Determinism and safety: nothing here performs a terminal operation. The
//! process-wide ownership flags are private and can only be published by
//! `TerminalGuard::setup`, which this file never calls, so every
//! `restore_for_panic` in this binary is provably headless — the same state as
//! a piped `--nocapture` run — and no escape sequence is ever emitted.
//! `setup` is deliberately untested here because it cannot be: crossterm
//! reaches `/dev/tty` regardless of stdout, so calling it from a test would
//! toggle the developer's own terminal state. Those paths are covered
//! in-crate against the recording double (`terminal::cov_terminal_private`).
//!
//! Every panic below is caught, and the previous hook is reinstalled before
//! any assertion runs, so a failing test is still reported by the harness
//! rather than swallowed by a leftover recorder.

#![forbid(unsafe_code)]

use std::any::Any;
use std::panic::{PanicHookInfo, RefUnwindSafe, UnwindSafe};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};

use nexus_tui::{TerminalGuard, install_panic_hook, restore_for_panic};

/// Serializes this file's tests: the panic hook is process-wide, so two tests
/// must never observe each other's hook chain. A poisoned lock is recovered
/// rather than propagated, because a failing assertion panics while holding
/// it and must not cascade into unrelated tests.
fn hook_exclusive() -> MutexGuard<'static, ()> {
    static HOOK_LOCK: StdMutex<()> = StdMutex::new(());
    HOOK_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Records panic payloads into `sink`. The hook never panics and never
/// asserts: it runs while a panic is already in flight, where a second panic
/// would abort the process instead of unwinding.
fn recorder(sink: Arc<StdMutex<Vec<String>>>) -> Box<dyn Fn(&PanicHookInfo<'_>) + Send + Sync> {
    Box::new(move |info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string payload>".to_owned());
        if let Ok(mut entries) = sink.lock() {
            entries.push(payload);
        }
    })
}

/// Payloads observed by a recorder, oldest first.
fn observed(sink: &Arc<StdMutex<Vec<String>>>) -> Vec<String> {
    sink.lock()
        .map(|entries| entries.clone())
        .unwrap_or_default()
}

/// The `&str` payload of a caught panic.
///
/// Takes the boxed payload and reads it through `Box::as_ref`: coercing
/// `&Box<dyn Any + Send>` to `&dyn Any + Send` at the call site does not
/// reliably carry the payload's vtable, which silently downcasts to `None`.
fn caught_text(payload: &Box<dyn Any + Send>) -> Option<&str> {
    payload.as_ref().downcast_ref::<&str>().copied()
}

/// Runs the headless restore while unwinding, mirroring `Drop` on a guard
/// whose scope is being left by a panic.
struct RestoreOnUnwind;

impl Drop for RestoreOnUnwind {
    fn drop(&mut self) {
        restore_for_panic();
    }
}

#[test]
fn panic_hook_delegates_the_original_payload_and_the_process_keeps_running() {
    let _lock = hook_exclusive();
    let previous = std::panic::take_hook();
    let seen = Arc::new(StdMutex::new(Vec::new()));
    std::panic::set_hook(recorder(Arc::clone(&seen)));
    install_panic_hook();

    let first = std::panic::catch_unwind(|| panic!("first caught panic"));
    let second = std::panic::catch_unwind(|| panic!("second caught panic"));

    // Reinstate the harness hook before asserting, so a failed assertion is
    // reported by the harness instead of by a leftover recorder.
    std::panic::set_hook(previous);
    let detached = std::panic::catch_unwind(|| panic!("after the hook was restored"));

    let first_payload = first.expect_err("the panic reached the caller");
    let second_payload = second.expect_err("the second panic was caught");
    assert_eq!(
        caught_text(&first_payload),
        Some("first caught panic"),
        "the wrapper does not replace the payload"
    );
    assert_eq!(
        caught_text(&second_payload),
        Some("second caught panic"),
        "a second panic is delegated the same way"
    );
    assert!(
        detached.is_err(),
        "a panic after the restore is still an ordinary panic"
    );
    assert_eq!(
        observed(&seen),
        ["first caught panic", "second caught panic"],
        "every panic reaches the previously installed hook with its payload \
         intact, and nothing reaches it once the wrapper is replaced"
    );
}

#[test]
fn panic_hook_runs_once_per_panic_not_once_per_unwound_frame() {
    let _lock = hook_exclusive();
    let previous = std::panic::take_hook();
    let seen = Arc::new(StdMutex::new(Vec::new()));
    std::panic::set_hook(recorder(Arc::clone(&seen)));
    // Two composed wrappers: the pre-existing hook must still be reached, and
    // reached once, however many wrappers sit in front of it.
    install_panic_hook();
    install_panic_hook();

    let caught = std::panic::catch_unwind(|| {
        std::panic::catch_unwind(|| panic!("nested catch")).expect_err("the inner boundary caught");
        panic!("outer panic after the inner boundary");
    });

    std::panic::set_hook(previous);
    let detached = std::panic::catch_unwind(|| panic!("after the hook was restored"));

    let payload = caught.expect_err("the panic crossed both boundaries");
    assert_eq!(
        caught_text(&payload),
        Some("outer panic after the inner boundary"),
        "the payload is not rewritten while crossing boundaries"
    );
    assert!(detached.is_err());
    assert_eq!(
        observed(&seen),
        ["nested catch", "outer panic after the inner boundary"],
        "each panic is delegated exactly once no matter how many unwinding \
         frames or wrappers it crosses"
    );
}

#[test]
fn restore_for_panic_is_a_reentrant_headless_no_op() {
    let _lock = hook_exclusive();

    // Nothing is owned in this binary (only `setup` publishes ownership, and
    // it is never called), so every call below is the no-ownership path that
    // must issue no terminal write at all.
    for _ in 0..8 {
        restore_for_panic();
    }

    let previous = std::panic::take_hook();
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    std::panic::set_hook(Box::new(move |_info| {
        // Runs while unwinding: the restore must not panic here, or the
        // process aborts instead of unwinding to `catch_unwind`.
        restore_for_panic();
        if let Ok(mut entries) = recorded.lock() {
            entries.push("hook ran".to_owned());
        }
    }));
    install_panic_hook();

    let caught = std::panic::catch_unwind(|| {
        // `RestoreOnUnwind::drop` runs in the same unwinding window as the
        // hook's restore; a panic from either would abort the process.
        let _cleanup_on_unwind = RestoreOnUnwind;
        panic!("panic with a teardown in scope");
    });

    std::panic::set_hook(previous);
    let detached = std::panic::catch_unwind(|| panic!("after the hook was restored"));

    // Concurrent callers share the published flags and stdout; with nothing
    // owned, the only shared work is the flag load and store.
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                for _ in 0..8 {
                    restore_for_panic();
                }
            });
        }
    });

    let payload = caught.as_ref().expect_err("the panic was caught");
    assert_eq!(
        caught_text(payload),
        Some("panic with a teardown in scope"),
        "the restore in the hook and in `Drop` did not disturb the payload"
    );
    assert!(detached.is_err());
    assert_eq!(
        observed(&seen),
        ["hook ran"],
        "the headless restore is safe while unwinding and from a `Drop` in \
         that same window"
    );
}

#[test]
fn panic_hook_is_reinstallable_after_being_replaced() {
    let _lock = hook_exclusive();
    let original = std::panic::take_hook();
    let seen = Arc::new(StdMutex::new(Vec::new()));
    std::panic::set_hook(recorder(Arc::clone(&seen)));
    install_panic_hook();

    // A repeated install composes with the earlier one instead of dropping a
    // hook from the chain, and the panic still reaches the caller.
    install_panic_hook();
    let composed = std::panic::catch_unwind(|| panic!("through two installs"));

    // Unwind the process back to the hook the harness had installed.
    std::panic::set_hook(original);
    let after_restore = std::panic::catch_unwind(|| panic!("after the hook was restored"));

    // Installing again over the restored hook works: the wrapper now delegates
    // straight to the harness hook, and the dropped recorder stays dropped.
    let harness = std::panic::take_hook();
    install_panic_hook();
    let reinstalled = std::panic::catch_unwind(|| panic!("through a reinstalled hook"));
    std::panic::set_hook(harness);

    let composed_payload = composed.expect_err("the panic was caught");
    let reinstalled_payload = reinstalled.expect_err("the panic was caught");
    assert_eq!(
        caught_text(&composed_payload),
        Some("through two installs"),
        "a double install still unwinds to the caller, exactly once"
    );
    assert_eq!(
        caught_text(&reinstalled_payload),
        Some("through a reinstalled hook"),
        "the hook can be installed again after it was replaced"
    );
    assert!(after_restore.is_err());
    assert_eq!(
        observed(&seen),
        ["through two installs"],
        "the wrapper chain is dropped wholesale, so the recorder behind it goes \
         with it and only the harness hook reports afterwards"
    );
}

#[test]
fn terminal_entry_points_keep_their_signatures() {
    let _lock = hook_exclusive();

    // Pin the shape callers bind to: `setup` takes nothing and hands back the
    // guard, `restore`/`teardown` need `&mut self` so a failed step stays
    // retryable, `is_active` only reads, and the panic entry points are plain
    // functions that never return a result.
    let _setup: fn() -> std::io::Result<TerminalGuard> = TerminalGuard::setup;
    let _restore: fn(&mut TerminalGuard) -> std::io::Result<()> = TerminalGuard::restore;
    let _teardown: fn(&mut TerminalGuard) = TerminalGuard::teardown;
    let _is_active: fn(&TerminalGuard) -> bool = TerminalGuard::is_active;
    let _restore_for_panic: fn() = restore_for_panic;
    let _install_panic_hook: fn() = install_panic_hook;
}

#[test]
fn terminal_guard_stays_send_sync_and_unwind_safe() {
    let _lock = hook_exclusive();
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    fn assert_unwind_safe<T: UnwindSafe>() {}
    fn assert_ref_unwind_safe<T: RefUnwindSafe>() {}

    // The guard is held across the event loop and dropped on every exit path,
    // including an unwind, so moving it between threads and unwinding through
    // its scope must stay legal for callers.
    assert_send::<TerminalGuard>();
    assert_sync::<TerminalGuard>();
    assert_unwind_safe::<TerminalGuard>();
    assert_ref_unwind_safe::<TerminalGuard>();
}
