#![forbid(unsafe_code)]

//! Coverage hardening for `nexus_core::execution`: monotonic deadline
//! evaluation and cooperative cancellation waits, exercised through the
//! public API only.
//!
//! Every blocking wait is bounded by a tiny timeout, so a broken
//! implementation fails instead of hanging the harness. Outcome assertions
//! hold regardless of scheduling; the promptness guards only distinguish a
//! real wake-up from a missed notification and leave a wide margin.

use std::time::{Duration, Instant};

use nexus_core::{CancellationToken, Deadline};

// --- Monotonic deadlines ----------------------------------------------------

#[test]
fn zero_horizon_deadline_is_immediately_elapsed() {
    let deadline = Deadline::after(Duration::ZERO).expect("a zero horizon never overflows");
    assert!(
        deadline.is_elapsed(),
        "a zero horizon is reached by the time it is observed"
    );
    assert_eq!(
        deadline.remaining(),
        Duration::ZERO,
        "elapsed deadlines saturate remaining time to zero"
    );
}

#[test]
fn future_deadline_has_positive_bounded_remaining_time() {
    let horizon = Duration::from_secs(60);
    let deadline = Deadline::after(horizon).expect("a bounded horizon builds");
    let first = deadline.remaining();
    assert!(
        !deadline.is_elapsed(),
        "a 60s horizon is not reached immediately"
    );
    assert!(
        first > Duration::ZERO,
        "remaining time is strictly positive"
    );
    assert!(first <= horizon, "remaining time never exceeds the horizon");

    std::thread::sleep(Duration::from_millis(2));
    let second = deadline.remaining();
    assert!(
        second <= first,
        "monotonic time never increases remaining time"
    );
    assert!(
        !deadline.is_elapsed(),
        "a 60s horizon survives a 2ms sample"
    );
}

#[test]
fn past_deadline_is_elapsed_with_zero_remaining() {
    let past = Instant::now()
        .checked_sub(Duration::from_millis(1))
        .expect("the monotonic clock has history");
    let deadline = Deadline::at(past);
    assert!(deadline.is_elapsed(), "a past instant is already reached");
    assert_eq!(deadline.remaining(), Duration::ZERO);
    assert_eq!(deadline.instant(), past, "the exact instant round-trips");
}

#[test]
fn deadline_at_current_instant_is_elapsed() {
    let at = Instant::now();
    let deadline = Deadline::at(at);
    assert!(
        deadline.is_elapsed(),
        "observation never precedes the captured instant"
    );
    assert_eq!(deadline.remaining(), Duration::ZERO);
}

#[test]
fn deadline_identity_follows_the_underlying_instant() {
    let at = Instant::now();
    let later_at = at
        .checked_add(Duration::from_millis(1))
        .expect("the clock accepts a 1ms offset");
    let first = Deadline::at(at);
    let copied = first; // `Deadline` is `Copy`.
    let later = Deadline::at(later_at);

    assert_eq!(first, Deadline::at(at), "equal instants compare equal");
    assert_eq!(copied, first, "copying preserves the deadline");
    assert_ne!(first, later, "different instants are distinct deadlines");
    assert_eq!(first.instant(), at);
    assert_eq!(later.instant(), later_at);
}

#[test]
fn deadline_after_overflow_is_none_not_infinite() {
    assert!(
        Deadline::after(Duration::MAX).is_none(),
        "an overflowing horizon is rejected instead of becoming an infinite deadline"
    );
}

// --- Cooperative cancellation waits -----------------------------------------

#[test]
fn fresh_token_is_uncancelled_and_zero_wait_times_out() {
    let token = CancellationToken::new();
    assert!(!token.is_cancelled());
    assert!(
        !token.wait_timeout(Duration::ZERO),
        "a zero timeout times out on a live token"
    );
    assert!(
        !token.wait_until(Instant::now()),
        "an already-reached deadline does not block a live token"
    );
    assert!(
        !token.is_cancelled(),
        "a timed-out wait never fabricates cancellation"
    );
}

#[test]
fn nonzero_wait_times_out_without_cancellation() {
    let token = CancellationToken::new();
    let started = Instant::now();
    assert!(!token.wait_timeout(Duration::from_millis(1)));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "a tiny timeout returns in bounded time"
    );
    assert!(!token.is_cancelled());
}

#[test]
fn default_token_is_live_and_independent() {
    let token = CancellationToken::default();
    assert!(!token.is_cancelled());
    assert!(!token.wait_timeout(Duration::from_millis(1)));
    assert_ne!(
        CancellationToken::default(),
        token,
        "independently created tokens never compare equal"
    );
}

#[test]
fn cancellation_is_shared_across_clones_and_isolated_between_tokens() {
    let token = CancellationToken::new();
    let clone = token.clone();
    let other = CancellationToken::new();
    assert_eq!(clone, token, "clones are the same token");
    assert_ne!(clone, other, "independent tokens are distinct");

    clone.cancel();
    assert!(
        token.is_cancelled(),
        "the original observes a clone's cancellation"
    );
    assert!(
        clone.wait_timeout(Duration::ZERO),
        "a cancelled token short-circuits a zero wait"
    );
    assert!(
        !other.is_cancelled(),
        "cancellation never leaks to other tokens"
    );
    assert!(
        !other.wait_timeout(Duration::from_millis(1)),
        "an uncancelled token still times out"
    );

    token.cancel();
    assert!(clone.is_cancelled(), "cancel is idempotent");
}

#[test]
fn cancellation_after_a_timed_out_wait_is_still_observed() {
    let token = CancellationToken::new();
    assert!(!token.wait_timeout(Duration::from_millis(1)));
    assert!(!token.is_cancelled());
    token.cancel();
    assert!(
        token.is_cancelled(),
        "the flag is live state, not a construction-time snapshot"
    );
    assert!(
        token.wait_timeout(Duration::from_millis(1)),
        "a wait started after cancellation never times out"
    );
}

#[test]
fn wait_observes_cancellation_from_another_thread() {
    let token = CancellationToken::new();
    let waiter = token.clone();
    let wait_bound = Duration::from_secs(2);
    let handle = std::thread::spawn(move || waiter.wait_timeout(wait_bound));

    std::thread::sleep(Duration::from_millis(5));
    let started = Instant::now();
    token.cancel();
    let observed = handle.join().expect("the waiter thread joins");
    let elapsed = started.elapsed();

    assert!(observed, "a blocked wait observes cancellation");
    assert!(
        elapsed < Duration::from_secs(1),
        "cancellation wakes the waiter promptly instead of exhausting {wait_bound:?}: {elapsed:?}"
    );
    assert!(token.is_cancelled());
}

#[test]
fn cancellation_wakes_every_waiting_clone() {
    let token = CancellationToken::new();
    let first = token.clone();
    let second = token.clone();
    let wait_bound = Duration::from_secs(2);
    let first_handle = std::thread::spawn(move || first.wait_timeout(wait_bound));
    let second_handle = std::thread::spawn(move || second.wait_timeout(wait_bound));

    std::thread::sleep(Duration::from_millis(5));
    let started = Instant::now();
    token.cancel();
    let first_observed = first_handle.join().expect("the first waiter joins");
    let second_observed = second_handle.join().expect("the second waiter joins");
    let elapsed = started.elapsed();

    assert!(
        first_observed && second_observed,
        "every waiting clone observes cancellation"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "notify-all wakes every waiter instead of a single one: {elapsed:?}"
    );
}

#[test]
fn wait_until_elapsed_deadline_times_out_without_cancellation() {
    let token = CancellationToken::new();
    let past = Instant::now()
        .checked_sub(Duration::from_millis(1))
        .expect("the monotonic clock has history");
    assert!(!token.wait_until(past), "an elapsed deadline returns false");
    assert!(!token.is_cancelled());
}

#[test]
fn wait_until_future_deadline_times_out_without_cancellation() {
    let token = CancellationToken::new();
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(2))
        .expect("the clock accepts a 2ms horizon");
    assert!(
        !token.wait_until(deadline),
        "a future deadline without cancellation times out"
    );
    assert!(!token.is_cancelled());
}

#[test]
fn wait_until_future_deadline_observes_cancellation() {
    let token = CancellationToken::new();
    let waiter = token.clone();
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(2))
        .expect("the clock accepts a 2s horizon");
    let handle = std::thread::spawn(move || waiter.wait_until(deadline));

    std::thread::sleep(Duration::from_millis(5));
    token.cancel();
    assert!(
        handle.join().expect("the waiter thread joins"),
        "wait_until delegates to the cooperative wait and observes cancellation"
    );
}

#[test]
fn wait_until_short_circuits_when_already_cancelled() {
    let token = CancellationToken::new();
    token.cancel();
    assert!(
        token.wait_until(Instant::now()),
        "a cancelled token reports cancellation without blocking"
    );
}
