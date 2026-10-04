//! Standard-library execution control: live cancellation and monotonic
//! deadlines.
//!
//! [`CancellationToken`] shares one atomic flag across clones, so
//! cancellation is observed live; a condition variable lets blocked workers
//! wait cooperatively instead of polling. [`Deadline`] carries an [`Instant`]
//! on the monotonic clock and never uses wall-clock timestamps.
//!
//! Contexts in [`crate::provider`] and [`crate::tool`] accept these controls
//! additively. The legacy `bool`/`Duration` constructors stay for fixtures:
//! a captured `bool` is an explicit snapshot, not cooperative state.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Shared live cancellation flag.
///
/// Cloning shares the same flag, so [`CancellationToken::cancel`] on any
/// clone is observed by every clone. A `bool` snapshot captured by a legacy
/// context constructor is *not* cooperative: it never changes after
/// construction, while a token is re-read on every observation.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    inner: Arc<CancelInner>,
}

#[derive(Debug)]
struct CancelInner {
    cancelled: AtomicBool,
    wait_lock: Mutex<()>,
    wait_signal: Condvar,
}

impl CancellationToken {
    /// Creates a live token that is not cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(CancelInner {
                cancelled: AtomicBool::new(false),
                wait_lock: Mutex::new(()),
                wait_signal: Condvar::new(),
            }),
        }
    }

    /// Requests cancellation and wakes cooperative waiters. Idempotent.
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        // Notify while holding the wait lock so a waiter cannot be caught
        // between its predicate check and its wait with no wake-up pending.
        let guard = self
            .inner
            .wait_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.inner.wait_signal.notify_all();
        drop(guard);
    }

    /// Returns true once cancellation was requested. Every observation
    /// re-reads the shared flag.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Waits cooperatively up to `timeout` for cancellation. Returns true
    /// when cancellation is observed, false when the wait timed out.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        if self.is_cancelled() {
            return true;
        }
        let guard = self
            .inner
            .wait_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (guard, _) = self
            .inner
            .wait_signal
            .wait_timeout_while(guard, timeout, |_| !self.is_cancelled())
            .unwrap_or_else(PoisonError::into_inner);
        drop(guard);
        self.is_cancelled()
    }

    /// Waits cooperatively until `deadline` (or cancellation), returning
    /// true when cancellation is observed.
    pub fn wait_until(&self, deadline: Instant) -> bool {
        self.wait_timeout(deadline.saturating_duration_since(Instant::now()))
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl PartialEq for CancellationToken {
    /// Same-token identity: clones compare equal, independent tokens do not.
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for CancellationToken {}

/// A finite deadline on the monotonic clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline {
    at: Instant,
}

impl Deadline {
    /// Wraps an exact monotonic instant.
    #[must_use]
    pub fn at(at: Instant) -> Self {
        Self { at }
    }

    /// Returns `Instant::now() + duration`, or [`None`] when the sum would
    /// overflow; a deadline is never silently infinite.
    #[must_use]
    pub fn after(duration: Duration) -> Option<Self> {
        Instant::now().checked_add(duration).map(Self::at)
    }

    /// Returns the underlying monotonic instant.
    #[must_use]
    pub fn instant(&self) -> Instant {
        self.at
    }

    /// Returns the remaining time, saturating to zero once elapsed.
    #[must_use]
    pub fn remaining(&self) -> Duration {
        self.at.saturating_duration_since(Instant::now())
    }

    /// Returns true when the deadline has been reached.
    #[must_use]
    pub fn is_elapsed(&self) -> bool {
        Instant::now() >= self.at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_live_across_clones() {
        let token = CancellationToken::new();
        let clone = token.clone();
        assert!(!token.is_cancelled());
        assert!(!clone.is_cancelled());
        token.cancel();
        assert!(clone.is_cancelled(), "cancellation is observed live");
        assert_eq!(clone, token, "clones compare as one token");
        assert_ne!(clone, CancellationToken::new());
    }

    #[test]
    fn cooperative_wait_observes_cancellation() {
        let token = CancellationToken::new();
        let waiter = token.clone();
        let handle = std::thread::spawn(move || waiter.wait_timeout(Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(10));
        token.cancel();
        assert!(handle.join().expect("waiter thread joins"));
        assert!(token.wait_timeout(Duration::from_millis(1)));
        assert!(token.wait_until(Instant::now()));
    }

    #[test]
    fn cooperative_wait_times_out_without_cancellation() {
        let token = CancellationToken::new();
        assert!(!token.wait_timeout(Duration::from_millis(1)));
        assert!(!token.is_cancelled());
    }

    #[test]
    fn deadline_is_evaluable_on_the_monotonic_clock() {
        let deadline = Deadline::after(Duration::from_secs(60)).expect("bounded deadline builds");
        assert!(!deadline.is_elapsed());
        assert!(deadline.remaining() <= Duration::from_secs(60));
        let at = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .expect("test clock has history");
        let past = Deadline::at(at);
        assert!(past.is_elapsed());
        assert_eq!(past.remaining(), Duration::ZERO);
        assert_eq!(past.instant(), at);
    }
}
