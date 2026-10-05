#![forbid(unsafe_code)]

//! Public-boundary hardening for [`nexus_core::CancellationToken`].
//!
//! Cancellation is live across clones, latched after the first request, and
//! idempotent. Every wait here is deterministic: durations are zero or
//! already elapsed, and no test sleeps. The one cross-thread test uses a
//! channel handshake plus a bounded timeout, so a missed wake-up fails an
//! assertion instead of hanging the run.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use nexus_core::{
    ApprovedScope, CancellationToken, Deadline, ErrorCategory, ProviderContext, ToolContext,
};

/// Bound for waits that must finish because cancellation is latched.
const BOUNDED_WAIT: Duration = Duration::from_millis(250);

fn future_deadline() -> Instant {
    Deadline::after(Duration::from_secs(60))
        .expect("bounded deadline builds")
        .instant()
}

fn past_instant() -> Instant {
    Instant::now()
        .checked_sub(Duration::from_millis(1))
        .expect("test clock has history")
}

#[test]
fn cancellation_is_live_across_clone_graph() {
    let root = CancellationToken::new();
    let first = root.clone();
    let second = first.clone();
    let third = second.clone();
    let independent = CancellationToken::new();

    for token in [&root, &first, &second, &third, &independent] {
        assert!(!token.is_cancelled(), "tokens start live");
    }

    second.cancel();

    for token in [&root, &first, &second, &third] {
        assert!(token.is_cancelled(), "every clone observes the shared flag");
    }
    assert!(
        !independent.is_cancelled(),
        "independent tokens never observe another token's cancellation"
    );
}

#[test]
fn is_cancelled_rereads_the_shared_flag_on_every_observation() {
    let token = CancellationToken::new();
    let observer = token.clone();

    assert!(!token.is_cancelled());
    assert!(!observer.is_cancelled());

    token.cancel();

    assert!(
        observer.is_cancelled(),
        "clone sees cancel from the original"
    );
    assert!(token.is_cancelled());
    assert!(observer.is_cancelled(), "the flag stays latched on re-read");
    assert!(token.is_cancelled());
}

#[test]
fn cancel_is_idempotent_across_clones() {
    let token = CancellationToken::new();
    let clones = [token.clone(), token.clone(), token.clone()];

    token.cancel();
    token.cancel();
    for clone in &clones {
        clone.cancel();
        clone.cancel();
    }

    assert!(token.is_cancelled());
    assert!(token.wait_timeout(Duration::ZERO));
    for clone in &clones {
        assert!(clone.is_cancelled());
        assert!(clone.wait_timeout(Duration::ZERO));
    }
}

#[test]
fn post_cancel_waits_are_latched_and_clone_safe() {
    let token = CancellationToken::new();
    token.cancel();

    assert!(token.wait_timeout(Duration::ZERO));
    assert!(token.wait_timeout(BOUNDED_WAIT));
    assert!(token.wait_until(Instant::now()));
    assert!(token.wait_until(past_instant()));

    let late_clone = token.clone();
    assert!(
        late_clone.is_cancelled(),
        "a clone made after cancel sees it"
    );
    assert!(late_clone.wait_timeout(Duration::ZERO));
    assert_eq!(late_clone, token);
}

#[test]
fn uncancelled_waits_report_no_cancellation() {
    let token = CancellationToken::new();
    assert!(!token.wait_timeout(Duration::ZERO));
    assert!(!token.wait_until(past_instant()));
    assert!(!token.is_cancelled());
    assert!(!CancellationToken::default().is_cancelled());
}

#[test]
fn equality_is_token_identity_not_cancellation_state() {
    let token = CancellationToken::new();
    let clone = token.clone();
    assert_eq!(clone, token);

    let other = CancellationToken::new();
    assert_ne!(other, token);

    token.cancel();
    other.cancel();
    assert_eq!(clone, token, "clones stay equal after cancellation");
    assert_ne!(other, token, "separate cancelled tokens stay distinct");
}

#[test]
fn live_cancel_wakes_a_blocked_waiter_without_sleeping() {
    let token = CancellationToken::new();
    let waiter = token.clone();
    let (started_tx, started_rx) = mpsc::channel();

    let handle = thread::spawn(move || {
        started_tx.send(()).expect("main thread receives start");
        waiter.wait_timeout(BOUNDED_WAIT)
    });

    started_rx.recv().expect("waiter reports before waiting");
    token.cancel();

    assert!(
        handle.join().expect("waiter joins"),
        "cancel must wake the waiter instead of letting it time out"
    );
    assert!(token.is_cancelled());
}

#[test]
fn tool_context_check_active_tracks_live_cancellation() {
    let scope = ApprovedScope::new("project-read").expect("scope is valid");
    let token = CancellationToken::new();
    let deadline = future_deadline();
    let context = ToolContext::new(1024, Duration::from_secs(60), false, scope.clone())
        .expect("context builds")
        .with_control(token.clone(), deadline);
    let observer = context.clone();

    assert_eq!(context.deadline(), Some(deadline));
    assert!(!context.is_cancelled());
    assert!(context.check_active().is_ok());
    assert!(context.check_not_cancelled().is_ok());

    token.cancel();

    assert!(context.is_cancelled(), "context re-reads the live token");
    assert!(observer.is_cancelled(), "cloned context shares the token");
    assert_eq!(
        context.check_active().expect_err("live cancel").category(),
        ErrorCategory::Cancelled
    );
    assert_eq!(
        observer
            .check_not_cancelled()
            .expect_err("live cancel")
            .category(),
        ErrorCategory::Cancelled
    );

    let precedence = ToolContext::new(1024, Duration::from_secs(60), false, scope.clone())
        .expect("context builds")
        .with_control(token.clone(), past_instant());
    assert_eq!(
        precedence
            .check_active()
            .expect_err("cancellation wins over deadline")
            .category(),
        ErrorCategory::Cancelled
    );

    let expired = ToolContext::new(1024, Duration::from_secs(60), false, scope)
        .expect("context builds")
        .with_control(CancellationToken::new(), past_instant());
    assert_eq!(
        expired
            .check_active()
            .expect_err("deadline passed")
            .category(),
        ErrorCategory::Timeout
    );
}

#[test]
fn provider_context_check_active_tracks_live_cancellation() {
    let token = CancellationToken::new();
    let deadline = future_deadline();
    let context = ProviderContext::new(Duration::from_secs(60), false, None)
        .with_control(token.clone(), deadline);
    let observer = context.clone();

    assert_eq!(context.deadline(), Some(deadline));
    assert!(!context.is_cancelled());
    assert!(context.check_active().is_ok());
    assert!(context.check_not_cancelled().is_ok());

    token.cancel();

    assert!(context.is_cancelled(), "context re-reads the live token");
    assert!(observer.is_cancelled(), "cloned context shares the token");
    assert_eq!(
        context.check_active().expect_err("live cancel").category(),
        ErrorCategory::Cancelled
    );
    assert_eq!(
        observer
            .check_not_cancelled()
            .expect_err("live cancel")
            .category(),
        ErrorCategory::Cancelled
    );

    let precedence = ProviderContext::new(Duration::from_secs(60), false, None)
        .with_control(token.clone(), past_instant());
    assert_eq!(
        precedence
            .check_active()
            .expect_err("cancellation wins over deadline")
            .category(),
        ErrorCategory::Cancelled
    );

    let expired = ProviderContext::new(Duration::from_secs(60), false, None)
        .with_control(CancellationToken::new(), past_instant());
    assert_eq!(
        expired
            .check_active()
            .expect_err("deadline passed")
            .category(),
        ErrorCategory::Timeout
    );
}
