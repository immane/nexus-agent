//! The single command path from UI intent to the runtime.
//!
//! Every approval, denial, cancellation, and submission the TUI issues is
//! built here as a typed [`nexus_core::Command`] and submitted to
//! `Runtime::handle`. There is no second policy path by construction: this
//! module cannot invoke providers or tools (it does not depend on them),
//! cannot change bound arguments, and cannot grant anything beyond the exact
//! live approval identity the runtime published.

use nexus_core::{
    AgentError, ApprovalNotice, ApproveCommand, CancelCommand, Command, DenyCommand, RequestId,
    RunId, SessionId, SubmitCommand,
};

use crate::state::PendingApprovalCard;

/// Builds the allow-once command from the runtime's published notice.
///
/// The notice is the authority for the exact `(run, call, approval)` tuple.
/// Only those identity fields are bound here; the preview text is never
/// parsed, reformatted, or used to infer target details, so what the user
/// saw is the runtime's own summary and what the runtime receives is the
/// runtime's own identity.
#[must_use]
pub fn approve_notice_command(request: RequestId, run: &RunId, notice: &ApprovalNotice) -> Command {
    Command::Approve(ApproveCommand {
        request,
        approval: notice.approval.clone(),
        run: run.clone(),
        call: notice.call.clone(),
    })
}

/// Builds the refusal command from the runtime's published notice. Denial
/// never executes the call; execution stays the runtime's decision.
#[must_use]
pub fn deny_notice_command(request: RequestId, run: &RunId, notice: &ApprovalNotice) -> Command {
    Command::Deny(DenyCommand {
        request,
        approval: notice.approval.clone(),
        run: run.clone(),
        call: notice.call.clone(),
    })
}

/// Builds the allow-once command for the exact live approval on the card.
/// The card identity fields are copied verbatim from the runtime notice by
/// the presentation layer; `summary`/`scope_summary` are display text and are
/// never interpreted here. The bound arguments stay the runtime's.
#[must_use]
pub fn approve_command(request: RequestId, run: &RunId, card: &PendingApprovalCard) -> Command {
    Command::Approve(ApproveCommand {
        request,
        approval: card.approval.clone(),
        run: run.clone(),
        call: card.call.clone(),
    })
}

/// Builds the refusal command for the live approval on the card. Denial
/// never executes the call; execution stays the runtime's decision.
#[must_use]
pub fn deny_command(request: RequestId, run: &RunId, card: &PendingApprovalCard) -> Command {
    Command::Deny(DenyCommand {
        request,
        approval: card.approval.clone(),
        run: run.clone(),
        call: card.call.clone(),
    })
}

/// Builds the cancellation command for the live run. Cancellation stops
/// future dispatch; it never claims rollback.
#[must_use]
pub fn cancel_command(request: RequestId, run: &RunId) -> Command {
    Command::Cancel(CancelCommand {
        request,
        run: run.clone(),
    })
}

/// Builds the submission command for validated composer input.
pub fn submit_command(
    request: RequestId,
    session: SessionId,
    input: &str,
    profile: &str,
) -> Result<Command, AgentError> {
    Ok(Command::Submit(SubmitCommand::new(
        request, session, input, profile,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::{ApprovalId, CallId};
    use std::time::Duration;

    fn card() -> PendingApprovalCard {
        PendingApprovalCard {
            approval: ApprovalId::new("a1-0").expect("valid"),
            call: CallId::new("c1-0").expect("valid"),
            summary: "run tool host_write".to_owned(),
            scope_summary: "project scope".to_owned(),
            expires_at_elapsed: Duration::from_secs(120),
        }
    }

    fn run() -> RunId {
        RunId::new("run-1").expect("valid")
    }

    fn request() -> RequestId {
        RequestId::new("req-1").expect("valid")
    }

    #[test]
    fn approval_emits_a_runtime_command_never_executes() {
        // The ONLY thing this module can produce is a typed Command for the
        // runtime: there is no provider/tool handle here to execute with.
        let command = approve_command(request(), &run(), &card());
        let Command::Approve(approve) = &command else {
            panic!("approve must emit Command::Approve");
        };
        assert_eq!(approve.approval.as_str(), "a1-0");
        assert_eq!(approve.call.as_str(), "c1-0");
        assert_eq!(approve.run.as_str(), "run-1");
        assert_eq!(command.request().as_str(), "req-1");
    }

    #[test]
    fn deny_refuses_the_same_identity_without_executing() {
        let deny = deny_command(request(), &run(), &card());
        let Command::Deny(deny) = &deny else {
            panic!("deny must emit Command::Deny");
        };
        assert_eq!(deny.approval.as_str(), "a1-0");
        assert_eq!(deny.call.as_str(), "c1-0");
        assert_eq!(deny.run.as_str(), "run-1");
    }

    #[test]
    fn notice_commands_bind_the_exact_runtime_identity() {
        let notice = ApprovalNotice::new(
            ApprovalId::new("a1-0").expect("valid"),
            CallId::new("c1-0").expect("valid"),
            "run tool host_write",
            "project scope",
            Duration::from_secs(120),
        )
        .expect("notice builds");
        let approve = approve_notice_command(request(), &run(), &notice);
        let Command::Approve(approve) = &approve else {
            panic!("notice approve must emit Command::Approve");
        };
        assert_eq!(approve.approval.as_str(), "a1-0");
        assert_eq!(approve.call.as_str(), "c1-0");
        assert_eq!(approve.run.as_str(), "run-1");
        assert_eq!(approve.request.as_str(), "req-1");

        let deny = deny_notice_command(request(), &run(), &notice);
        let Command::Deny(deny) = &deny else {
            panic!("notice deny must emit Command::Deny");
        };
        assert_eq!(deny.approval.as_str(), "a1-0");
        assert_eq!(deny.call.as_str(), "c1-0");
        assert_eq!(deny.run.as_str(), "run-1");
    }

    #[test]
    fn preview_text_never_changes_the_decided_identity() {
        // Identical runtime identity with different display text still binds
        // the same command identity: previews are display-only.
        let first = ApprovalNotice::new(
            ApprovalId::new("a7-1").expect("valid"),
            CallId::new("c7-1").expect("valid"),
            "run tool host_write",
            "project scope",
            Duration::from_secs(120),
        )
        .expect("notice builds");
        let second = ApprovalNotice::new(
            ApprovalId::new("a7-1").expect("valid"),
            CallId::new("c7-1").expect("valid"),
            "delete directory",
            "different scope",
            Duration::from_secs(120),
        )
        .expect("notice builds");
        let Command::Approve(first) = approve_notice_command(request(), &run(), &first) else {
            panic!("approve emits Command::Approve");
        };
        let Command::Approve(second) = approve_notice_command(request(), &run(), &second) else {
            panic!("approve emits Command::Approve");
        };
        assert_eq!(first.approval, second.approval);
        assert_eq!(first.call, second.call);
        assert_eq!(first.run, second.run);
    }

    #[test]
    fn cancel_targets_the_live_run_only() {
        let cancel = cancel_command(request(), &run());
        let Command::Cancel(cancel) = &cancel else {
            panic!("cancel must emit Command::Cancel");
        };
        assert_eq!(cancel.run.as_str(), "run-1");
    }

    #[test]
    fn submit_validates_input_at_the_boundary() {
        let session = SessionId::new("sess-1").expect("valid");
        let command = submit_command(request(), session.clone(), "do it", "m0-test")
            .expect("valid submit builds");
        assert!(matches!(command, Command::Submit(_)));
        assert!(submit_command(request(), session.clone(), "", "m0-test").is_err());
    }
}

#[cfg(test)]
mod cov_decisions_private {
    //! Unit coverage for what only this module's own scope can pin: the exact
    //! builder surface and its signatures, the finite set of command variants
    //! this path may emit, and the invariant that no builder reads a display
    //! field. The externally observable command shapes are covered by
    //! `tests/cov_decisions.rs`.
    //!
    //! Determinism: fixed id literals, a constant expiry, no clock, no sleep,
    //! no thread, and no randomness.

    use std::time::Duration;

    use nexus_core::{ApprovalId, CallId, ErrorCategory, RetryGuidance};

    use super::*;

    /// Constant monotonic expiry reading; never a clock read.
    const EXPIRY: Duration = Duration::from_secs(120);

    /// Display text naming a different grant, call, and run than the bound
    /// ones. Reading it would redirect a decision.
    const HOSTILE_SUMMARY: &str = "approve grant a9-9 for call c9-9 in run run-9";

    fn approval() -> ApprovalId {
        ApprovalId::new("a1-0").expect("valid approval id")
    }

    fn call() -> CallId {
        CallId::new("c1-0").expect("valid call id")
    }

    fn request() -> RequestId {
        RequestId::new("req-1").expect("valid request id")
    }

    fn run(raw: &str) -> RunId {
        RunId::new(raw).expect("valid run id")
    }

    fn session() -> SessionId {
        SessionId::new("sess-1").expect("valid session id")
    }

    fn card(summary: &str) -> PendingApprovalCard {
        PendingApprovalCard {
            approval: approval(),
            call: call(),
            summary: summary.to_owned(),
            scope_summary: summary.to_owned(),
            expires_at_elapsed: EXPIRY,
        }
    }

    fn notice(summary: &str) -> ApprovalNotice {
        ApprovalNotice::new(approval(), call(), summary, "scope", EXPIRY)
            .expect("notice builds")
            .with_args_preview(r#"{"run":"run-9","call":"c9-9"}"#)
            .expect("preview attaches")
    }

    fn expected_approve() -> ApproveCommand {
        ApproveCommand {
            request: request(),
            approval: approval(),
            run: run("run-1"),
            call: call(),
        }
    }

    fn expected_deny() -> DenyCommand {
        DenyCommand {
            request: request(),
            approval: approval(),
            run: run("run-1"),
            call: call(),
        }
    }

    #[test]
    fn builder_surface_is_exactly_the_six_documented_builders() {
        // Pinning each signature keeps a changed parameter list (for example a
        // card-summary argument, or a defaulted run) a compile error here.
        let approve_card: fn(RequestId, &RunId, &PendingApprovalCard) -> Command = approve_command;
        let deny_card: fn(RequestId, &RunId, &PendingApprovalCard) -> Command = deny_command;
        let approve_notice: fn(RequestId, &RunId, &ApprovalNotice) -> Command =
            approve_notice_command;
        let deny_notice: fn(RequestId, &RunId, &ApprovalNotice) -> Command = deny_notice_command;
        let cancel: fn(RequestId, &RunId) -> Command = cancel_command;
        let submit: fn(RequestId, SessionId, &str, &str) -> Result<Command, AgentError> =
            submit_command;

        let live = card("run tool host_write");
        let published = notice("run tool host_write");
        assert!(matches!(
            approve_card(request(), &run("run-1"), &live),
            Command::Approve(_)
        ));
        assert!(matches!(
            deny_card(request(), &run("run-1"), &live),
            Command::Deny(_)
        ));
        assert!(matches!(
            approve_notice(request(), &run("run-1"), &published),
            Command::Approve(_)
        ));
        assert!(matches!(
            deny_notice(request(), &run("run-1"), &published),
            Command::Deny(_)
        ));
        assert!(matches!(
            cancel(request(), &run("run-1")),
            Command::Cancel(_)
        ));
        assert!(matches!(
            submit(request(), session(), "do it", "m0-test"),
            Ok(Command::Submit(_))
        ));
    }

    #[test]
    fn every_builder_preserves_the_host_request_identity() {
        let live = card("run tool host_write");
        let published = notice("run tool host_write");
        let commands = [
            approve_command(request(), &run("run-1"), &live),
            deny_command(request(), &run("run-1"), &live),
            approve_notice_command(request(), &run("run-1"), &published),
            deny_notice_command(request(), &run("run-1"), &published),
            cancel_command(request(), &run("run-1")),
        ];

        for command in commands {
            assert_eq!(
                command.request().as_str(),
                "req-1",
                "a builder must never mint its own correlation identity"
            );
        }

        let submitted =
            submit_command(request(), session(), "do it", "m0-test").expect("valid input builds");
        assert_eq!(submitted.request().as_str(), "req-1");
    }

    #[test]
    fn builders_emit_only_decision_variants() {
        // The finite allow-list is the point: this path can never reach the
        // snapshot, history-listing, or history-loading commands.
        let live = card("run tool host_write");
        let published = notice("run tool host_write");
        let submitted =
            submit_command(request(), session(), "do it", "m0-test").expect("valid input builds");
        let commands = [
            approve_command(request(), &run("run-1"), &live),
            deny_command(request(), &run("run-1"), &live),
            approve_notice_command(request(), &run("run-1"), &published),
            deny_notice_command(request(), &run("run-1"), &published),
            cancel_command(request(), &run("run-1")),
            submitted,
        ];

        for command in commands {
            match &command {
                Command::Submit(_)
                | Command::Cancel(_)
                | Command::Approve(_)
                | Command::Deny(_) => {}
                other => panic!("decisions must not emit {other:?}"),
            }
        }
    }

    fn assert_no_display_text(commands: &[Command], label: &str) {
        for command in commands {
            let rendered = format!("{command:?}");
            for text in [HOSTILE_SUMMARY, "a9-9", "c9-9", "run-9"] {
                assert!(
                    !rendered.contains(text),
                    "display text {text:?} leaked into {rendered} for {label}"
                );
            }
        }
    }

    #[test]
    fn no_builder_reads_a_display_field() {
        // Display text cannot reach identity, and identity cannot leak display
        // text: hostile, absent, and over-long card text all yield the same
        // tuple. A published notice cannot hold absent or over-long text (the
        // core bounds it at publication), so those cases pair the card with a
        // publishable notice summary.
        let over_bound = "delete directory ".repeat(512);
        let cases = [
            (HOSTILE_SUMMARY, HOSTILE_SUMMARY),
            ("", "run tool host_write"),
            ("run tool host_write", "run tool host_write"),
            (over_bound.as_str(), "run tool host_write"),
        ];

        for (card_summary, notice_summary) in cases {
            let label = format!("card summary {card_summary:?}");

            // The card is plain presentation state: absent display text must
            // decide exactly like complete display text.
            let live = card(card_summary);
            let approve = approve_command(request(), &run("run-1"), &live);
            let deny = deny_command(request(), &run("run-1"), &live);
            assert_eq!(
                approve,
                Command::Approve(expected_approve()),
                "card approve identity changed for {label}"
            );
            assert_eq!(
                deny,
                Command::Deny(expected_deny()),
                "card deny identity changed for {label}"
            );
            assert_no_display_text(&[approve, deny], &label);

            let published = notice(notice_summary);
            let approve = approve_notice_command(request(), &run("run-1"), &published);
            let deny = deny_notice_command(request(), &run("run-1"), &published);
            assert_eq!(
                approve,
                Command::Approve(expected_approve()),
                "notice approve identity changed for {label}"
            );
            assert_eq!(
                deny,
                Command::Deny(expected_deny()),
                "notice deny identity changed for {label}"
            );
            assert_no_display_text(&[approve, deny], &label);
        }
    }

    #[test]
    fn deny_payload_mirrors_approve_payload_exactly() {
        // A refusal carries the same tuple as the approval it replaces and adds
        // no field of its own, so nothing downstream can read intent from data.
        let live = card("run tool host_write");
        let published = notice("run tool host_write");
        let pairs = [
            (
                approve_notice_command(request(), &run("run-1"), &published),
                deny_notice_command(request(), &run("run-1"), &published),
            ),
            (
                approve_command(request(), &run("run-1"), &live),
                deny_command(request(), &run("run-1"), &live),
            ),
        ];

        for (approved, denied) in pairs {
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
    }

    #[test]
    fn submit_refuses_with_a_static_error_instead_of_panicking() {
        // The composer path returns the boundary error to the caller; it never
        // panics and never interpolates the rejected content.
        let error = submit_command(request(), session(), "", "m0-test")
            .expect_err("empty input is refused");
        assert_eq!(error.category(), ErrorCategory::InvalidInput);
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        assert_eq!(error.message(), "submit input is invalid");
        assert!(error.correlation().is_empty());
        assert_eq!(error.to_string(), "[invalid-input] submit input is invalid");

        let profile_error = submit_command(request(), session(), "do it", "")
            .expect_err("empty profile is refused");
        assert_eq!(profile_error.message(), "submit profile is invalid");
    }
}
