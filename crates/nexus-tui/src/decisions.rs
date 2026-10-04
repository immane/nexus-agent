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
