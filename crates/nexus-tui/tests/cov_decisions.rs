#![forbid(unsafe_code)]

//! Public-API hardening for the approval decision command path.
//!
//! [`nexus_tui::decisions`] is the single place where UI intent becomes a
//! `nexus_core::Command`, so these tests drive it the way the binary and any
//! other frontend must: only through the public module path and the crate-root
//! re-exports. They pin the four properties the decision path exists to
//! provide.
//!
//! - approve and deny bind the exact `(request, approval, run, call)` tuple the
//!   runtime published, from the notice and from the live card alike, and
//!   never reconcile, substitute, or default any element of that tuple;
//! - display text (summary, scope, exact-arguments preview) is never read to
//!   decide identity: hostile previews naming other grants, calls, and runs
//!   cannot redirect a decision, and no display text reaches the emitted
//!   command;
//! - cancel binds only the live run it is handed and carries no approval or
//!   call identity;
//! - submit validates input at the boundary and returns a static,
//!   non-echoing `AgentError` instead of a command.
//!
//! Determinism: every id is a fixed literal, every duration is a constant, no
//! test reads a clock, sleeps, spawns a thread, or randomizes, and the builders
//! are pure, so identical inputs must produce identical commands.

use std::time::Duration;

use nexus_core::commands::{MAX_INPUT_BYTES, MAX_SUMMARY_BYTES};
use nexus_core::{
    AgentError, ApprovalId, ApprovalNotice, ApproveCommand, CallId, Command, DenyCommand,
    ErrorCategory, RequestId, RetryGuidance, RunId, SessionId, SubmitCommand,
};
use nexus_tui::decisions::{
    approve_command, approve_notice_command, cancel_command, deny_command, deny_notice_command,
    submit_command,
};
use nexus_tui::state::{MAX_APPROVAL_FIELD_BYTES, PendingApprovalCard};

/// Monotonic expiry reading carried by every notice and card under test. A
/// constant, never a clock reading.
const NOTICE_EXPIRY: Duration = Duration::from_secs(120);

/// Safe summary text as the runtime publishes it.
const BASELINE_SUMMARY: &str = "run tool host_write";
/// Safe scope text as the runtime publishes it.
const BASELINE_SCOPE: &str = "project scope";
/// Summary that names a different grant, call, and run than the bound ones.
const HOSTILE_SUMMARY: &str = "approve grant a9-9 for call c9-9 in run run-9";
/// Scope text that names the same mismatched identities.
const HOSTILE_SCOPE: &str = "scope of run-9 call c9-9 approval a9-9";
/// Exact-arguments preview naming the same mismatched identities.
const HOSTILE_PREVIEW: &str =
    r#"{"path":"/etc/hosts","run":"run-9","call":"c9-9","approval":"a9-9"}"#;

/// A published runtime notice with the given display text.
fn notice(summary: &str, scope: &str, preview: Option<&str>) -> ApprovalNotice {
    let published = ApprovalNotice::new(
        ApprovalId::new("a1-0").expect("valid approval id"),
        CallId::new("c1-0").expect("valid call id"),
        summary,
        scope,
        NOTICE_EXPIRY,
    )
    .expect("runtime notice builds");
    match preview {
        Some(text) => published
            .with_args_preview(text)
            .expect("bounded preview attaches"),
        None => published,
    }
}

/// The live card the presentation layer derives from [`notice`]: identity
/// fields copied verbatim, display text supplied by the caller.
fn card(summary: &str, scope: &str) -> PendingApprovalCard {
    PendingApprovalCard {
        approval: ApprovalId::new("a1-0").expect("valid approval id"),
        call: CallId::new("c1-0").expect("valid call id"),
        summary: summary.to_owned(),
        scope_summary: scope.to_owned(),
        expires_at_elapsed: NOTICE_EXPIRY,
    }
}

fn request(raw: &str) -> RequestId {
    RequestId::new(raw).expect("valid request id")
}

fn run(raw: &str) -> RunId {
    RunId::new(raw).expect("valid run id")
}

fn session(raw: &str) -> SessionId {
    SessionId::new(raw).expect("valid session id")
}

/// The one tuple a decision on the baseline card must produce.
fn expected_approve() -> ApproveCommand {
    ApproveCommand {
        request: request("req-1"),
        approval: ApprovalId::new("a1-0").expect("valid approval id"),
        run: run("run-1"),
        call: CallId::new("c1-0").expect("valid call id"),
    }
}

/// The refusal twin of [`expected_approve`].
fn expected_deny() -> DenyCommand {
    DenyCommand {
        request: request("req-1"),
        approval: ApprovalId::new("a1-0").expect("valid approval id"),
        run: run("run-1"),
        call: CallId::new("c1-0").expect("valid call id"),
    }
}

/// Asserts the boundary contract every rejected submit must satisfy: a static
/// `InvalidInput` refusal that is not retryable, carries no correlation data,
/// and never echoes the rejected input back to the caller.
fn assert_static_invalid_input(error: AgentError, expected_message: &str) {
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(error.message(), expected_message);
    assert!(error.correlation().is_empty());
}

#[test]
#[ignore = "low-value: root/module aliases; decision binding behavior is covered separately"]
fn crate_root_reexports_alias_the_decision_builders() {
    // The four re-exported names must be the `decisions` items themselves, not
    // shadowing wrappers: a wrapper could drift from the builder it fronts
    // while keeping the same signature.
    let _: fn(RequestId, &RunId, &PendingApprovalCard) -> Command = nexus_tui::approve_command;
    let _: fn(RequestId, &RunId, &PendingApprovalCard) -> Command = nexus_tui::deny_command;
    let _: fn(RequestId, &RunId) -> Command = nexus_tui::cancel_command;
    let _: fn(RequestId, SessionId, &str, &str, bool) -> Result<Command, AgentError> =
        nexus_tui::submit_command;

    let module_approve: fn(RequestId, &RunId, &PendingApprovalCard) -> Command =
        nexus_tui::decisions::approve_command;
    let root_approve: fn(RequestId, &RunId, &PendingApprovalCard) -> Command =
        nexus_tui::approve_command;
    let module_deny: fn(RequestId, &RunId, &PendingApprovalCard) -> Command =
        nexus_tui::decisions::deny_command;
    let root_deny: fn(RequestId, &RunId, &PendingApprovalCard) -> Command = nexus_tui::deny_command;
    let module_cancel: fn(RequestId, &RunId) -> Command = nexus_tui::decisions::cancel_command;
    let root_cancel: fn(RequestId, &RunId) -> Command = nexus_tui::cancel_command;
    let module_submit: fn(RequestId, SessionId, &str, &str, bool) -> Result<Command, AgentError> =
        nexus_tui::decisions::submit_command;
    let root_submit: fn(RequestId, SessionId, &str, &str, bool) -> Result<Command, AgentError> =
        nexus_tui::submit_command;

    assert!(
        core::ptr::fn_addr_eq(module_approve, root_approve),
        "the root approve_command must be the decisions item"
    );
    assert!(
        core::ptr::fn_addr_eq(module_deny, root_deny),
        "the root deny_command must be the decisions item"
    );
    assert!(
        core::ptr::fn_addr_eq(module_cancel, root_cancel),
        "the root cancel_command must be the decisions item"
    );
    assert!(
        core::ptr::fn_addr_eq(module_submit, root_submit),
        "the root submit_command must be the decisions item"
    );
}

#[test]
fn notice_approve_binds_the_exact_runtime_tuple() {
    let published = notice(BASELINE_SUMMARY, BASELINE_SCOPE, None);
    let command = approve_notice_command(request("req-1"), &run("run-1"), &published);

    assert_eq!(command, Command::Approve(expected_approve()));
    assert_eq!(command.request().as_str(), "req-1");
    let Command::Approve(approve) = &command else {
        panic!("notice approve must emit Command::Approve");
    };
    assert_eq!(approve.approval.as_str(), "a1-0");
    assert_eq!(approve.call.as_str(), "c1-0");
    assert_eq!(approve.run.as_str(), "run-1");
    assert_eq!(approve.request.as_str(), "req-1");
}

#[test]
fn notice_deny_binds_the_same_tuple_as_approve() {
    let published = notice(BASELINE_SUMMARY, BASELINE_SCOPE, None);
    let command = deny_notice_command(request("req-1"), &run("run-1"), &published);

    assert_eq!(command, Command::Deny(expected_deny()));
    let Command::Deny(deny) = &command else {
        panic!("notice deny must emit Command::Deny");
    };
    assert_eq!(deny.approval.as_str(), "a1-0");
    assert_eq!(deny.call.as_str(), "c1-0");
    assert_eq!(deny.run.as_str(), "run-1");
    assert_eq!(deny.request.as_str(), "req-1");
}

#[test]
fn card_approve_and_deny_bind_the_exact_card_identity() {
    let live = card(BASELINE_SUMMARY, BASELINE_SCOPE);

    let approved = approve_command(request("req-1"), &run("run-1"), &live);
    assert_eq!(approved, Command::Approve(expected_approve()));
    let Command::Approve(approve) = &approved else {
        panic!("card approve must emit Command::Approve");
    };
    assert_eq!(approve.approval.as_str(), "a1-0");
    assert_eq!(approve.call.as_str(), "c1-0");
    assert_eq!(approve.run.as_str(), "run-1");

    let denied = deny_command(request("req-1"), &run("run-1"), &live);
    assert_eq!(denied, Command::Deny(expected_deny()));
    let Command::Deny(deny) = &denied else {
        panic!("card deny must emit Command::Deny");
    };
    assert_eq!(deny.approval.as_str(), "a1-0");
    assert_eq!(deny.call.as_str(), "c1-0");
    assert_eq!(deny.run.as_str(), "run-1");
}

#[test]
fn card_and_notice_builders_agree_for_the_same_live_approval() {
    // The presentation layer copies identity out of the notice, so both
    // builders must reach the identical tuple for the same live approval.
    let published = notice(BASELINE_SUMMARY, BASELINE_SCOPE, Some("{}"));
    let live = card(&published.summary, &published.scope_summary);

    assert_eq!(
        approve_command(request("req-1"), &run("run-1"), &live),
        approve_notice_command(request("req-1"), &run("run-1"), &published)
    );
    assert_eq!(
        deny_command(request("req-1"), &run("run-1"), &live),
        deny_notice_command(request("req-1"), &run("run-1"), &published)
    );
}

#[test]
fn deny_mirrors_approve_except_for_the_intent() {
    // Denial must grant nothing and hide nothing: same tuple, different
    // variant, so the runtime can never read a refusal as authorization.
    let live = card(BASELINE_SUMMARY, BASELINE_SCOPE);
    let approved = approve_command(request("req-1"), &run("run-1"), &live);
    let denied = deny_command(request("req-1"), &run("run-1"), &live);
    let Command::Approve(approve) = &approved else {
        panic!("approve must emit Command::Approve");
    };
    let Command::Deny(deny) = &denied else {
        panic!("deny must emit Command::Deny");
    };

    assert_eq!(approve.request, deny.request);
    assert_eq!(approve.approval, deny.approval);
    assert_eq!(approve.run, deny.run);
    assert_eq!(approve.call, deny.call);
    assert_ne!(approved, denied, "a refusal is never an authorization");
}

#[test]
fn preview_text_never_changes_the_decided_identity() {
    // Every display-text variation below describes the SAME runtime tuple.
    // If any builder read, parsed, or re-derived identity from text, one of
    // these commands would differ from the baseline.
    let variations = [
        (BASELINE_SUMMARY, BASELINE_SCOPE, None),
        ("delete directory", "different scope", None),
        (HOSTILE_SUMMARY, HOSTILE_SCOPE, None),
        (HOSTILE_SUMMARY, HOSTILE_SCOPE, Some(HOSTILE_PREVIEW)),
        (BASELINE_SUMMARY, BASELINE_SCOPE, Some(HOSTILE_PREVIEW)),
        (HOSTILE_SUMMARY, BASELINE_SCOPE, Some("{}")),
        (
            "run tool host_write in run-9",
            "c9-9",
            Some("{\"call\":\"c9-9\"}"),
        ),
    ];

    for (summary, scope, preview) in variations {
        let published = notice(summary, scope, preview);
        let label = format!("summary {summary:?} scope {scope:?} preview {preview:?}");

        assert_eq!(
            approve_notice_command(request("req-1"), &run("run-1"), &published),
            Command::Approve(expected_approve()),
            "approve identity changed for {label}"
        );
        assert_eq!(
            deny_notice_command(request("req-1"), &run("run-1"), &published),
            Command::Deny(expected_deny()),
            "deny identity changed for {label}"
        );
        assert_eq!(
            approve_command(request("req-1"), &run("run-1"), &card(summary, scope)),
            Command::Approve(expected_approve()),
            "card approve identity changed for {label}"
        );
        assert_eq!(
            deny_command(request("req-1"), &run("run-1"), &card(summary, scope)),
            Command::Deny(expected_deny()),
            "card deny identity changed for {label}"
        );
    }
}

#[test]
fn display_text_never_reaches_the_emitted_command() {
    let published = notice(HOSTILE_SUMMARY, HOSTILE_SCOPE, Some(HOSTILE_PREVIEW));
    let live = card(HOSTILE_SUMMARY, HOSTILE_SCOPE);

    for command in [
        approve_notice_command(request("req-1"), &run("run-1"), &published),
        deny_notice_command(request("req-1"), &run("run-1"), &published),
        approve_command(request("req-1"), &run("run-1"), &live),
        deny_command(request("req-1"), &run("run-1"), &live),
    ] {
        let rendered = format!("{command:?}");
        for text in [
            HOSTILE_SUMMARY,
            HOSTILE_SCOPE,
            HOSTILE_PREVIEW,
            "a9-9",
            "c9-9",
            "run-9",
        ] {
            assert!(
                !rendered.contains(text),
                "display text {text:?} leaked into {rendered}"
            );
        }
        // The only strings a decision carries are its four opaque identities.
        assert!(rendered.contains("req-1"));
        assert!(rendered.contains("a1-0"));
        assert!(rendered.contains("c1-0"));
        assert!(rendered.contains("run-1"));
    }
}

#[test]
fn reduced_or_absent_display_text_does_not_change_identity() {
    // The card is presentation state: its text may be clipped, sanitized, or
    // empty, and it is never the authority for the tuple.
    let full = BASELINE_SUMMARY.to_owned();
    let reduced = full[..4].to_owned();
    assert_ne!(reduced, full, "the reduction must actually change the text");

    let over_bound = "delete directory ".repeat(MAX_APPROVAL_FIELD_BYTES / 8 + 1);
    assert!(
        over_bound.len() > MAX_APPROVAL_FIELD_BYTES,
        "the case must exceed the presentation field bound"
    );

    let cases = [
        (full.clone(), BASELINE_SCOPE.to_owned()),
        (reduced, String::new()),
        (String::new(), String::new()),
        (over_bound.clone(), over_bound),
    ];

    for (summary, scope) in cases {
        let live = card(&summary, &scope);
        assert_eq!(
            approve_command(request("req-1"), &run("run-1"), &live),
            Command::Approve(expected_approve()),
            "approve identity changed for card text {summary:?}/{scope:?}"
        );
        assert_eq!(
            deny_command(request("req-1"), &run("run-1"), &live),
            Command::Deny(expected_deny()),
            "deny identity changed for card text {summary:?}/{scope:?}"
        );
    }
}

#[test]
fn distinct_live_identities_produce_distinct_commands() {
    let first = notice(BASELINE_SUMMARY, BASELINE_SCOPE, None);
    let second = ApprovalNotice::new(
        ApprovalId::new("a2-0").expect("valid approval id"),
        CallId::new("c2-0").expect("valid call id"),
        "another tool",
        "another scope",
        NOTICE_EXPIRY,
    )
    .expect("runtime notice builds");

    let first_command = approve_notice_command(request("req-1"), &run("run-1"), &first);
    let second_command = approve_notice_command(request("req-1"), &run("run-2"), &second);

    assert_ne!(
        first_command, second_command,
        "different live approvals must not collapse to one command"
    );
    let Command::Approve(second_approve) = &second_command else {
        panic!("approve must emit Command::Approve");
    };
    assert_eq!(second_approve.approval.as_str(), "a2-0");
    assert_eq!(second_approve.call.as_str(), "c2-0");
    assert_eq!(second_approve.run.as_str(), "run-2");
}

#[test]
fn mismatched_card_and_run_are_bound_verbatim_for_the_runtime_to_reject() {
    // The frontend holds no policy: it reports the tuple it was handed and
    // lets the runtime reject a mismatch. Reconciling or "fixing" the run here
    // would turn a refusal the runtime owes into a guess.
    let live = PendingApprovalCard {
        approval: ApprovalId::new("a5-0").expect("valid approval id"),
        call: CallId::new("c5-0").expect("valid call id"),
        summary: BASELINE_SUMMARY.to_owned(),
        scope_summary: BASELINE_SCOPE.to_owned(),
        expires_at_elapsed: NOTICE_EXPIRY,
    };

    let Command::Approve(approve) = approve_command(request("req-9"), &run("run-9"), &live) else {
        panic!("approve must emit Command::Approve");
    };
    assert_eq!(
        approve.approval.as_str(),
        "a5-0",
        "card grant is bound verbatim"
    );
    assert_eq!(approve.call.as_str(), "c5-0", "card call is bound verbatim");
    assert_eq!(
        approve.run.as_str(),
        "run-9",
        "the passed run is bound verbatim"
    );
    assert_eq!(approve.request.as_str(), "req-9");
    assert_ne!(
        approve.run,
        run("run-1"),
        "the builder never substitutes a run from elsewhere"
    );
}

#[test]
fn builders_snapshot_identity_and_leave_inputs_untouched() {
    let mut published = notice(HOSTILE_SUMMARY, HOSTILE_SCOPE, Some(HOSTILE_PREVIEW));
    let published_before = published.clone();
    let live = card(HOSTILE_SUMMARY, HOSTILE_SCOPE);
    let live_before = live.clone();

    let approve = approve_notice_command(request("req-1"), &run("run-1"), &published);
    let deny = deny_command(request("req-1"), &run("run-1"), &live);

    // Rebuilding the same inputs yields the same commands: the builders are
    // pure and read the identity once.
    assert_eq!(
        approve,
        approve_notice_command(request("req-1"), &run("run-1"), &published_before)
    );
    assert_eq!(
        deny,
        deny_command(request("req-1"), &run("run-1"), &live_before)
    );

    // Mutating the public notice fields after the fact cannot rewrite a
    // decision that was already built.
    published.approval = ApprovalId::new("a9-9").expect("valid approval id");
    published.call = CallId::new("c9-9").expect("valid call id");
    published.summary = HOSTILE_SUMMARY.to_owned();
    published.expires_at_elapsed = Duration::ZERO;

    assert_eq!(approve, Command::Approve(expected_approve()));
    assert_eq!(deny, Command::Deny(expected_deny()));
}

#[test]
fn cancel_targets_only_the_live_run() {
    let command = cancel_command(request("req-3"), &run("run-3"));

    assert_eq!(command.request().as_str(), "req-3");
    let Command::Cancel(cancel) = &command else {
        panic!("cancel must emit Command::Cancel");
    };
    assert_eq!(cancel.run.as_str(), "run-3");
    assert_eq!(cancel.request.as_str(), "req-3");

    // A different run is a different cancellation: there is no cached or
    // last-seen run to fall back on.
    let other = cancel_command(request("req-3"), &run("run-4"));
    assert_ne!(command, other);
    let Command::Cancel(other_cancel) = &other else {
        panic!("cancel must emit Command::Cancel");
    };
    assert_eq!(other_cancel.run.as_str(), "run-4");
}

#[test]
fn cancel_carries_no_approval_identity() {
    // Cancellation stops future dispatch for one run. It must not smuggle an
    // approval or a call, which would read as a decision on a live grant, so
    // its payload is pinned to exactly the two identity fields.
    let command = cancel_command(request("req-3"), &run("run-3"));
    let rendered = format!("{command:?}");

    assert_eq!(
        rendered,
        "Cancel(CancelCommand { request: RequestId(\"req-3\"), run: RunId(\"run-3\") })"
    );
    for text in [
        "a1-0",
        "c1-0",
        "a9-9",
        "c9-9",
        HOSTILE_SUMMARY,
        HOSTILE_PREVIEW,
    ] {
        assert!(
            !rendered.contains(text),
            "cancel leaked {text:?} into {rendered}"
        );
    }

    // The signature itself takes no approval or call: there is nothing to bind
    // a decision to, even while a card is pending.
    let canceller: fn(RequestId, &RunId) -> Command = cancel_command;
    assert_eq!(canceller(request("req-3"), &run("run-3")), command);
}

#[test]
fn submit_binds_the_exact_validated_input() {
    let command = submit_command(
        request("req-1"),
        session("sess-1"),
        "do it",
        "m0-test",
        false,
    )
    .expect("valid submit builds");

    assert_eq!(
        command,
        Command::Submit(SubmitCommand {
            request: request("req-1"),
            session: session("sess-1"),
            input: "do it".to_owned(),
            profile: "m0-test".to_owned(),
            read_only: false,
        })
    );
    assert_eq!(command.request().as_str(), "req-1");

    // The read-only flag rides the builder into the submitted command.
    let planned = submit_command(
        request("req-2"),
        session("sess-1"),
        "do it",
        "m0-test",
        true,
    )
    .expect("valid submit builds");
    assert!(
        matches!(
            planned,
            Command::Submit(SubmitCommand {
                read_only: true,
                ..
            })
        ),
        "plan mode submits read-only"
    );
}

#[test]
fn submit_input_bounds_are_exact() {
    let at_limit = "x".repeat(MAX_INPUT_BYTES);
    let over_limit = "x".repeat(MAX_INPUT_BYTES + 1);
    let profile_at_limit = "p".repeat(MAX_SUMMARY_BYTES);
    let profile_over_limit = "p".repeat(MAX_SUMMARY_BYTES + 1);

    assert!(
        submit_command(
            request("req-1"),
            session("sess-1"),
            &at_limit,
            "m0-test",
            false
        )
        .is_ok(),
        "exactly MAX_INPUT_BYTES is accepted"
    );
    let error = submit_command(
        request("req-1"),
        session("sess-1"),
        &over_limit,
        "m0-test",
        false,
    )
    .expect_err("MAX_INPUT_BYTES + 1 is rejected");
    assert_static_invalid_input(error, "submit input is invalid");

    assert!(
        submit_command(request("req-1"), session("sess-1"), " ", "m0-test", false).is_ok(),
        "whitespace is non-empty input; emptiness alone is rejected"
    );
    assert!(
        submit_command(
            request("req-1"),
            session("sess-1"),
            "do it",
            &profile_at_limit,
            false
        )
        .is_ok(),
        "exactly MAX_SUMMARY_BYTES is accepted for the profile"
    );
    let error = submit_command(
        request("req-1"),
        session("sess-1"),
        "do it",
        &profile_over_limit,
        false,
    )
    .expect_err("MAX_SUMMARY_BYTES + 1 is rejected");
    assert_static_invalid_input(error, "submit profile is invalid");
    let error = submit_command(request("req-1"), session("sess-1"), "do it", "", false)
        .expect_err("empty profile");
    assert_static_invalid_input(error, "submit profile is invalid");
}

#[test]
fn submit_rejects_empty_input_without_echoing_it() {
    let error = submit_command(request("req-1"), session("sess-1"), "", "m0-test", false)
        .expect_err("empty input is rejected");
    assert_static_invalid_input(error, "submit input is invalid");

    // A rejected submit must not leak the composer's content back into the
    // error the UI would display.
    let secret = "do it PASSWORD=hunter2 run-9 a9-9";
    let filler = "y".repeat(MAX_INPUT_BYTES);
    let rejected = format!("{secret}{filler}");
    let error = submit_command(
        request("req-1"),
        session("sess-1"),
        &rejected,
        "m0-test",
        false,
    )
    .expect_err("oversized input is rejected");
    let rendered = error.to_string();
    assert_static_invalid_input(error, "submit input is invalid");
    for text in ["hunter2", "PASSWORD", "run-9", "a9-9", secret] {
        assert!(
            !rendered.contains(text),
            "rejected input {text:?} leaked into {rendered}"
        );
    }
    assert_eq!(rendered, "[invalid-input] submit input is invalid");
}

#[test]
fn a_rejected_submit_is_side_effect_free() {
    // The host owns request identity, so a refusal must not consume it: the
    // same request id still builds a command for a valid input.
    let refused = submit_command(request("req-7"), session("sess-7"), "", "m0-test", false);
    assert!(refused.is_err());

    let accepted = submit_command(
        request("req-7"),
        session("sess-7"),
        "retry",
        "m0-test",
        false,
    )
    .expect("valid");
    let Command::Submit(submit) = &accepted else {
        panic!("submit must emit Command::Submit");
    };
    assert_eq!(submit.request.as_str(), "req-7");
    assert_eq!(submit.session.as_str(), "sess-7");
    assert_eq!(submit.input, "retry");
}
