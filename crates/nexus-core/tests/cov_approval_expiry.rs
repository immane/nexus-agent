#![forbid(unsafe_code)]

//! Coverage hardening for approval expiry and staleness checks.
//!
//! `ApprovalBinding::check_valid_for_dispatch` is the authorization gate
//! evaluated immediately before dispatch. Its expiry input is an explicit
//! monotonic run-elapsed reading, so every instant in this suite is a literal
//! `Duration` and nothing sleeps. All assertions go through the public
//! `nexus_core` API; the runtime that supplies `now_elapsed` and publishes
//! notices lives outside this crate.

use std::time::Duration;

use nexus_core::approval::MAX_SCOPE_BYTES;
use nexus_core::{
    AgentError, ApprovalBinding, ApprovalId, ApprovalNotice, ApprovedScope, CallId, ErrorCategory,
    EventPayload, Limits, M0_REVISION, NormalizedArgs, RetryGuidance, RunId, ToolId,
};

const EXPIRY: Duration = Duration::from_secs(120);
const ONE_NANOSECOND: Duration = Duration::from_nanos(1);

fn run(id: &str) -> RunId {
    RunId::new(id).expect("valid run id")
}

fn call(id: &str) -> CallId {
    CallId::new(id).expect("valid call id")
}

fn tool(name: &str, revision: u32) -> ToolId {
    ToolId::new(name, revision).expect("valid tool id")
}

fn args(text: &str) -> NormalizedArgs {
    NormalizedArgs::new(text).expect("valid normalized args")
}

fn scope(text: &str) -> ApprovedScope {
    ApprovedScope::new(text).expect("valid approved scope")
}

/// The exact tuple the canonical grant binds, rebuilt per test so one
/// dimension can be varied at a time.
fn bound_tuple() -> (RunId, CallId, ToolId, NormalizedArgs) {
    (
        run("run-1"),
        call("call-1"),
        tool("host_read", M0_REVISION),
        args(r#"{"path":"src"}"#),
    )
}

/// Builds the canonical grant with an explicit expiry reading and policy
/// revision.
fn grant_at(expires_at_elapsed: Duration, policy_revision: u32) -> ApprovalBinding {
    ApprovalBinding::new(
        ApprovalId::new("appr-1").expect("valid approval id"),
        run("run-1"),
        call("call-1"),
        tool("host_read", M0_REVISION),
        args(r#"{"path":"src"}"#),
        scope("project-read"),
        expires_at_elapsed,
        policy_revision,
    )
}

/// Asserts a dispatch check was denied with the stable permission-denied
/// diagnostic for the given reason.
fn assert_denied(result: Result<(), AgentError>, expected_message: &str) {
    let error = result.expect_err("stale or expired approval must be denied");
    assert_eq!(error.category(), ErrorCategory::PermissionDenied);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(error.message(), expected_message);
}

/// Asserts an approval carrier constructor rejected invalid input with the
/// stable invalid-input diagnostic.
fn assert_input_denied<T: std::fmt::Debug>(result: Result<T, AgentError>, expected_message: &str) {
    let error = result.expect_err("invalid approval input must be rejected");
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(error.message(), expected_message);
}

#[test]
fn expiry_is_inclusive_and_one_nanosecond_before_expiry_authorizes() {
    let grant = grant_at(EXPIRY, M0_REVISION);
    let (r, c, t, a) = bound_tuple();
    let just_before = EXPIRY - ONE_NANOSECOND;

    assert!(!grant.is_expired(Duration::ZERO));
    assert!(!grant.is_expired(just_before));
    assert!(grant.is_expired(EXPIRY), "the expiry instant is expired");
    assert!(grant.is_expired(EXPIRY + ONE_NANOSECOND));
    assert!(grant.is_expired(Duration::MAX));

    grant
        .check_valid_for_dispatch(&r, &c, &t, &a, M0_REVISION, Duration::ZERO)
        .expect("a fresh approval authorizes dispatch");
    grant
        .check_valid_for_dispatch(&r, &c, &t, &a, M0_REVISION, just_before)
        .expect("one nanosecond before expiry still authorizes");

    assert_denied(
        grant.check_valid_for_dispatch(&r, &c, &t, &a, M0_REVISION, EXPIRY),
        "approval expired",
    );
    assert_denied(
        grant.check_valid_for_dispatch(&r, &c, &t, &a, M0_REVISION, EXPIRY + ONE_NANOSECOND),
        "approval expired",
    );
    assert_denied(
        grant.check_valid_for_dispatch(&r, &c, &t, &a, M0_REVISION, Duration::MAX),
        "approval expired",
    );
}

#[test]
fn zero_lifetime_grant_is_expired_at_elapsed_zero() {
    let grant = grant_at(Duration::ZERO, M0_REVISION);
    let (r, c, t, a) = bound_tuple();

    assert_eq!(grant.expires_at_elapsed(), Duration::ZERO);
    assert!(grant.is_expired(Duration::ZERO));
    assert_denied(
        grant.check_valid_for_dispatch(&r, &c, &t, &a, M0_REVISION, Duration::ZERO),
        "approval expired",
    );
}

#[test]
fn maximum_expiry_reading_stays_valid_until_the_type_maximum() {
    let grant = grant_at(Duration::MAX, M0_REVISION);
    let (r, c, t, a) = bound_tuple();

    assert!(!grant.is_expired(Duration::ZERO));
    assert!(!grant.is_expired(EXPIRY));
    grant
        .check_valid_for_dispatch(&r, &c, &t, &a, M0_REVISION, EXPIRY)
        .expect("finite elapsed stays before a maximum expiry");
    assert!(grant.is_expired(Duration::MAX));
}

#[test]
fn stale_run_is_denied_while_the_grant_is_still_fresh() {
    let grant = grant_at(EXPIRY, M0_REVISION);
    let (_, c, t, a) = bound_tuple();
    assert_denied(
        grant.check_valid_for_dispatch(&run("run-2"), &c, &t, &a, M0_REVISION, Duration::ZERO),
        "approval run mismatch",
    );
}

#[test]
fn stale_call_is_denied_while_the_grant_is_still_fresh() {
    let grant = grant_at(EXPIRY, M0_REVISION);
    let (r, _, t, a) = bound_tuple();
    assert_denied(
        grant.check_valid_for_dispatch(&r, &call("call-2"), &t, &a, M0_REVISION, Duration::ZERO),
        "approval call mismatch",
    );
}

#[test]
fn tool_name_or_revision_change_is_denied_while_the_grant_is_still_fresh() {
    let grant = grant_at(EXPIRY, M0_REVISION);
    let (r, c, _, a) = bound_tuple();

    for stale_tool in [
        tool("host_read", M0_REVISION + 1),
        tool("host_write", M0_REVISION),
    ] {
        assert_denied(
            grant.check_valid_for_dispatch(&r, &c, &stale_tool, &a, M0_REVISION, Duration::ZERO),
            "approval tool identity or revision mismatch",
        );
    }
}

#[test]
fn changed_arguments_are_denied_while_the_grant_is_still_fresh() {
    let grant = grant_at(EXPIRY, M0_REVISION);
    let (r, c, t, _) = bound_tuple();

    for changed in [
        args(r#"{"path":"other"}"#),
        args(r#"{"path":"src","extra":true}"#),
        // The carrier compares the exact text it was given: canonicalization
        // belongs to admission, so a whitespace-only variant is a different
        // binding.
        args(r#"{ "path":"src" }"#),
    ] {
        assert_denied(
            grant.check_valid_for_dispatch(&r, &c, &t, &changed, M0_REVISION, Duration::ZERO),
            "approval arguments changed",
        );
    }
}

#[test]
fn policy_revision_requires_exact_equality_not_ordering() {
    let grant = grant_at(EXPIRY, 7);
    let (r, c, t, a) = bound_tuple();

    grant
        .check_valid_for_dispatch(&r, &c, &t, &a, 7, Duration::ZERO)
        .expect("the issued policy revision authorizes");

    for stale_revision in [6, 8, M0_REVISION, u32::MAX] {
        assert_denied(
            grant.check_valid_for_dispatch(&r, &c, &t, &a, stale_revision, Duration::ZERO),
            "approval policy revision mismatch",
        );
    }
}

#[test]
fn fully_stale_check_reports_the_first_dimension() {
    let grant = grant_at(EXPIRY, M0_REVISION);
    assert_denied(
        grant.check_valid_for_dispatch(
            &run("run-2"),
            &call("call-2"),
            &tool("host_write", M0_REVISION + 1),
            &args(r#"{"path":"other"}"#),
            M0_REVISION + 1,
            EXPIRY,
        ),
        "approval run mismatch",
    );
}

#[test]
fn exact_tuple_round_trips_through_accessors() {
    let grant = grant_at(EXPIRY, M0_REVISION);
    let (r, c, t, a) = bound_tuple();

    assert_eq!(grant.approval().as_str(), "appr-1");
    assert_eq!(grant.run(), &r);
    assert_eq!(grant.call(), &c);
    assert_eq!(grant.tool(), &t);
    assert_eq!(grant.tool().name(), "host_read");
    assert_eq!(grant.tool().revision(), M0_REVISION);
    assert_eq!(grant.args(), &a);
    assert_eq!(grant.args().as_str(), r#"{"path":"src"}"#);
    assert_eq!(grant.scope().as_str(), "project-read");
    assert_eq!(grant.policy_revision(), M0_REVISION);
    assert_eq!(grant.expires_at_elapsed(), EXPIRY);

    assert_eq!(grant, grant.clone(), "clones preserve the exact binding");
    assert_ne!(
        grant,
        grant_at(EXPIRY, M0_REVISION + 1),
        "a different policy revision is a different binding"
    );
}

#[test]
fn normalized_args_object_root_and_assembly_boundaries() {
    let max = Limits::M0_TEST_ARG_ASSEMBLY_BYTES;

    assert_eq!(
        NormalizedArgs::new("{}")
            .expect("empty object accepted")
            .as_str(),
        "{}"
    );

    let padded = r#"  { "path" : "src" }  "#;
    assert_eq!(
        NormalizedArgs::new(padded)
            .expect("trimmed object root accepted")
            .as_str(),
        padded,
        "the carrier preserves the supplied text; canonicalization is admission's job"
    );

    let at_bound = format!("{{{}}}", "a".repeat(max - 2));
    assert_eq!(at_bound.len(), max);
    assert_eq!(
        NormalizedArgs::new(at_bound.clone())
            .expect("the assembly bound is inclusive")
            .as_str(),
        at_bound
    );

    assert_input_denied(
        NormalizedArgs::new("x".repeat(max + 1)),
        "tool arguments exceed assembly budget",
    );
    assert_input_denied(NormalizedArgs::new(""), "tool arguments are empty");
    for non_object in ["[1,2]", "{", "}", "{}tail", "head{}", "   "] {
        assert_input_denied(
            NormalizedArgs::new(non_object),
            "tool arguments must be an object-root value",
        );
    }

    // Exactly at the byte bound but not object-root: the size check passes
    // and the shape check reports, proving the bound is not off by one.
    assert_input_denied(
        NormalizedArgs::new("x".repeat(max)),
        "tool arguments must be an object-root value",
    );
}

#[test]
fn approved_scope_empty_and_byte_boundaries() {
    assert_eq!(
        ApprovedScope::new("")
            .expect("empty scope means the call itself")
            .as_str(),
        ""
    );

    let at_bound = "s".repeat(MAX_SCOPE_BYTES);
    assert_eq!(
        ApprovedScope::new(at_bound)
            .expect("the scope bound is inclusive")
            .as_str()
            .len(),
        MAX_SCOPE_BYTES
    );
    assert_input_denied(
        ApprovedScope::new("s".repeat(MAX_SCOPE_BYTES + 1)),
        "approved scope exceeds its bound",
    );

    // The bound counts bytes, not characters: 512 two-byte characters fit
    // exactly; one more character crosses it.
    assert_eq!(MAX_SCOPE_BYTES % 2, 0);
    let multibyte_at_bound = "é".repeat(MAX_SCOPE_BYTES / 2);
    assert_eq!(multibyte_at_bound.len(), MAX_SCOPE_BYTES);
    ApprovedScope::new(multibyte_at_bound).expect("byte bound is inclusive for multibyte text");
    let multibyte_over = "é".repeat(MAX_SCOPE_BYTES / 2 + 1);
    assert!(multibyte_over.len() > MAX_SCOPE_BYTES);
    assert_input_denied(
        ApprovedScope::new(multibyte_over),
        "approved scope exceeds its bound",
    );
}

#[test]
fn approval_expiry_limit_boundaries_are_validated() {
    assert_eq!(Limits::M0_TEST_APPROVAL_EXPIRY_SECS, 120);
    let limits = Limits::m0_test();
    assert_eq!(
        limits.approval_expiry,
        Duration::from_secs(Limits::M0_TEST_APPROVAL_EXPIRY_SECS)
    );
    limits
        .validate()
        .expect("M0-test approval expiry validates");

    let mut zero = Limits::m0_test();
    zero.approval_expiry = Duration::ZERO;
    let error = zero
        .validate()
        .expect_err("zero approval expiry is rejected");
    assert_eq!(error.category(), ErrorCategory::ResourceLimit);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(error.message(), "limit duration must be nonzero");

    let mut at_max = Limits::m0_test();
    at_max.approval_expiry = Limits::M0_TEST_MAX_DURATION;
    at_max
        .validate()
        .expect("approval expiry at the practical maximum validates");

    let mut over_max = Limits::m0_test();
    over_max.approval_expiry = Limits::M0_TEST_MAX_DURATION + Duration::from_secs(1);
    let error = over_max
        .validate()
        .expect_err("approval expiry above the practical maximum is rejected");
    assert_eq!(error.category(), ErrorCategory::ResourceLimit);
    assert_eq!(error.message(), "approval expiry exceeds practical maximum");
}

#[test]
fn approval_notice_carries_expiry_as_data() {
    let notice = ApprovalNotice::new(
        ApprovalId::new("appr-1").expect("valid approval id"),
        call("call-1"),
        "read file under the project root",
        "project-read",
        EXPIRY,
    )
    .expect("bounded notice builds")
    .with_args_preview(r#"{"path":"src"}"#)
    .expect("bounded preview attaches");

    assert_eq!(notice.expires_at_elapsed, EXPIRY);
    assert_eq!(notice.args_preview(), Some(r#"{"path":"src"}"#));
    EventPayload::ApprovalRequired(notice.clone())
        .validate()
        .expect("a bounded notice payload validates");

    // Notice validation checks text bounds only; the monotonic expiry is
    // carried for the runtime's dispatch check and is not evaluated here.
    let mut expired = notice;
    expired.expires_at_elapsed = Duration::ZERO;
    expired
        .validate()
        .expect("zero expiry is data, not a notice-text violation");
}
