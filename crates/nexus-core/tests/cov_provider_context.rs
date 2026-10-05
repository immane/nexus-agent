#![forbid(unsafe_code)]

//! Coverage hardening: live execution control on provider and tool contexts.
//!
//! The additive `with_control` contract is pinned from the public API:
//! - it installs a shared `CancellationToken` and an evaluable monotonic
//!   deadline while leaving the legacy `Duration`/`bool` snapshot fields
//!   untouched;
//! - cancellation is re-read from the shared token on every observation, so a
//!   cancel that arrives after construction is visible to the context;
//! - an elapsed live deadline maps to `ErrorCategory::Timeout` and a cancelled
//!   token to `ErrorCategory::Cancelled`, with cancellation winning when both
//!   apply;
//! - legacy contexts built without control expose no evaluable deadline and
//!   never fabricate a timeout from their elapsed `Duration` reading.
//!
//! All timing derives from monotonic instants captured in-test; there are no
//! sleeps, threads, or wall-clock reads.

use std::time::{Duration, Instant};

use nexus_core::{
    ApprovedScope, CancellationToken, CredentialRef, ErrorCategory, ProviderContext, RetryGuidance,
    ToolContext,
};

fn past_instant() -> Instant {
    Instant::now()
        .checked_sub(Duration::from_millis(1))
        .expect("test clock has history")
}

fn future_instant() -> Instant {
    Instant::now()
        .checked_add(Duration::from_secs(60))
        .expect("test clock has room")
}

fn scope() -> ApprovedScope {
    ApprovedScope::new("project-read").expect("valid scope")
}

#[test]
fn provider_with_control_installs_live_token_and_evaluable_deadline() {
    let token = CancellationToken::new();
    let deadline = future_instant();
    let context =
        ProviderContext::new(Duration::from_secs(300), false, None).with_control(token, deadline);
    assert_eq!(context.deadline(), Some(deadline));
    assert_eq!(context.deadline_elapsed(), Duration::from_secs(300));
    assert!(!context.is_cancelled());
    assert!(context.check_active().is_ok());
    assert!(context.check_not_cancelled().is_ok());
}

#[test]
fn provider_cancellation_is_observed_live_after_construction() {
    let token = CancellationToken::new();
    let context = ProviderContext::new(Duration::from_secs(300), false, None)
        .with_control(token.clone(), future_instant());
    assert!(!context.is_cancelled());

    // Cancel through a different clone: the shared flag is re-read, not a
    // construction-time snapshot.
    token.cancel();
    assert!(context.is_cancelled());
    let error = context
        .check_active()
        .expect_err("live cancellation observed");
    assert_eq!(error.category(), ErrorCategory::Cancelled);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(
        context
            .check_not_cancelled()
            .expect_err("live cancellation observed")
            .category(),
        ErrorCategory::Cancelled
    );
}

#[test]
fn provider_expired_deadline_is_timeout_without_cancellation() {
    let context = ProviderContext::new(Duration::from_secs(300), false, None)
        .with_control(CancellationToken::new(), past_instant());
    assert!(
        !context.is_cancelled(),
        "an elapsed deadline is not cancellation"
    );
    assert!(
        context.check_not_cancelled().is_ok(),
        "the cancellation-only check ignores the deadline"
    );
    let error = context.check_active().expect_err("live deadline elapsed");
    assert_eq!(error.category(), ErrorCategory::Timeout);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
}

#[test]
fn provider_deadline_boundary_at_now_is_already_elapsed() {
    // `Instant::now()` is monotonic and the check uses `>=`, so a deadline
    // captured as "now" is deterministically reached at the next check.
    let boundary = Instant::now();
    let context = ProviderContext::new(Duration::from_secs(300), false, None)
        .with_control(CancellationToken::new(), boundary);
    assert_eq!(context.deadline(), Some(boundary));
    assert_eq!(
        context
            .check_active()
            .expect_err("reached boundary is elapsed")
            .category(),
        ErrorCategory::Timeout
    );
}

#[test]
fn provider_cancellation_precedes_expired_deadline() {
    let token = CancellationToken::new();
    token.cancel();
    let context = ProviderContext::new(Duration::from_secs(300), false, None)
        .with_control(token, past_instant());
    assert!(context.is_cancelled());
    assert_eq!(
        context
            .check_active()
            .expect_err("both controls failed")
            .category(),
        ErrorCategory::Cancelled,
        "cancellation is checked before the deadline"
    );
}

#[test]
fn provider_with_control_replaces_previous_control() {
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let first = ProviderContext::new(Duration::from_secs(300), false, None)
        .with_control(cancelled, past_instant());
    assert_eq!(
        first
            .check_active()
            .expect_err("first control failed")
            .category(),
        ErrorCategory::Cancelled
    );

    let replacement = future_instant();
    let replaced = first
        .clone()
        .with_control(CancellationToken::new(), replacement);
    assert!(!replaced.is_cancelled(), "replacement token is live");
    assert_eq!(replaced.deadline(), Some(replacement));
    assert!(
        replaced.check_active().is_ok(),
        "replacement deadline is in the future"
    );
    assert!(
        first.is_cancelled(),
        "the original context keeps its installed control"
    );
}

#[test]
fn provider_legacy_context_has_no_evaluable_deadline() {
    let context = ProviderContext::new(Duration::MAX, false, None);
    assert_eq!(
        context.deadline(),
        None,
        "an elapsed reading carries no evaluable instant"
    );
    assert_eq!(context.deadline_elapsed(), Duration::MAX);
    assert!(
        context.check_active().is_ok(),
        "an elapsed reading alone never fabricates a timeout"
    );
    assert!(context.check_not_cancelled().is_ok());
    assert!(!context.is_cancelled());
}

#[test]
fn provider_legacy_snapshot_cancellation_survives_additive_control() {
    let context = ProviderContext::new(Duration::from_secs(300), true, None)
        .with_control(CancellationToken::new(), future_instant());
    assert!(
        context.is_cancelled(),
        "a live token never clears the legacy snapshot"
    );
    assert_eq!(
        context
            .check_active()
            .expect_err("snapshot cancels")
            .category(),
        ErrorCategory::Cancelled
    );
    assert_eq!(
        context
            .check_not_cancelled()
            .expect_err("snapshot cancels")
            .category(),
        ErrorCategory::Cancelled
    );
}

#[test]
fn provider_with_control_preserves_legacy_fields_and_credential() {
    let credential = CredentialRef::new("payments-prod-key").expect("valid reference");
    let context = ProviderContext::new(Duration::from_secs(300), false, Some(credential.clone()))
        .with_control(CancellationToken::new(), future_instant());
    assert_eq!(context.credential(), Some(&credential));
    assert_eq!(context.deadline_elapsed(), Duration::from_secs(300));
    assert!(context.check_active().is_ok());
}

#[test]
fn provider_context_equality_follows_token_identity() {
    let token = CancellationToken::new();
    let deadline = future_instant();
    let left = ProviderContext::new(Duration::from_secs(60), false, None)
        .with_control(token.clone(), deadline);
    let right =
        ProviderContext::new(Duration::from_secs(60), false, None).with_control(token, deadline);
    assert_eq!(left, right, "clones of one token are the same live control");
    let independent = ProviderContext::new(Duration::from_secs(60), false, None)
        .with_control(CancellationToken::new(), deadline);
    assert_ne!(
        left, independent,
        "independent tokens are distinct live control"
    );
}

#[test]
fn tool_with_control_installs_live_token_and_evaluable_deadline() {
    let token = CancellationToken::new();
    let deadline = future_instant();
    let context = ToolContext::new(1024, Duration::from_secs(300), false, scope())
        .expect("valid context builds")
        .with_control(token, deadline);
    assert_eq!(context.deadline(), Some(deadline));
    assert_eq!(context.deadline_elapsed(), Duration::from_secs(300));
    assert_eq!(context.output_budget_bytes(), 1024);
    assert_eq!(context.scope().as_str(), "project-read");
    assert!(!context.is_cancelled());
    assert!(context.check_active().is_ok());
    assert!(context.check_not_cancelled().is_ok());
}

#[test]
fn tool_cancellation_and_timeout_are_explicit_and_ordered() {
    let token = CancellationToken::new();
    let context = ToolContext::new(1024, Duration::from_secs(300), false, scope())
        .expect("valid context builds")
        .with_control(token.clone(), past_instant());
    assert!(
        !context.is_cancelled(),
        "a past deadline alone is not cancellation"
    );
    assert!(context.check_not_cancelled().is_ok());
    assert_eq!(
        context
            .check_active()
            .expect_err("live deadline elapsed")
            .category(),
        ErrorCategory::Timeout
    );

    token.cancel();
    assert!(
        context.is_cancelled(),
        "token is re-read after construction"
    );
    let error = context.check_active().expect_err("both controls failed");
    assert_eq!(
        error.category(),
        ErrorCategory::Cancelled,
        "cancellation is checked before the deadline"
    );
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(
        context
            .check_not_cancelled()
            .expect_err("live cancellation observed")
            .category(),
        ErrorCategory::Cancelled
    );
}

#[test]
fn tool_with_control_replaces_previous_control() {
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let original = ToolContext::new(1024, Duration::from_secs(300), false, scope())
        .expect("valid context builds")
        .with_control(cancelled, past_instant());
    assert_eq!(
        original
            .check_active()
            .expect_err("first control failed")
            .category(),
        ErrorCategory::Cancelled
    );

    let replacement = future_instant();
    let replaced = original
        .clone()
        .with_control(CancellationToken::new(), replacement);
    assert!(!replaced.is_cancelled(), "replacement token is live");
    assert_eq!(replaced.deadline(), Some(replacement));
    assert!(
        replaced.check_active().is_ok(),
        "replacement deadline is in the future"
    );
    assert!(
        original.is_cancelled(),
        "the original context keeps its installed control"
    );
}

#[test]
fn tool_legacy_context_has_no_evaluable_deadline() {
    let context =
        ToolContext::new(1024, Duration::MAX, false, scope()).expect("valid context builds");
    assert_eq!(
        context.deadline(),
        None,
        "an elapsed reading carries no evaluable instant"
    );
    assert_eq!(context.deadline_elapsed(), Duration::MAX);
    assert!(
        context.check_active().is_ok(),
        "an elapsed reading alone never fabricates a timeout"
    );
    assert!(context.check_not_cancelled().is_ok());
    assert!(!context.is_cancelled());
}

#[test]
fn tool_legacy_snapshot_cancellation_survives_additive_control() {
    let deadline = future_instant();
    let context = ToolContext::new(1024, Duration::from_secs(300), true, scope())
        .expect("valid context builds")
        .with_control(CancellationToken::new(), deadline);
    assert_eq!(context.deadline(), Some(deadline));
    assert!(
        context.is_cancelled(),
        "a live token never clears the legacy snapshot"
    );
    assert_eq!(
        context
            .check_active()
            .expect_err("snapshot cancels")
            .category(),
        ErrorCategory::Cancelled
    );
}
