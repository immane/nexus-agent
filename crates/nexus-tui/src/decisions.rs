//! The single command path from UI intent to the runtime.
//!
//! Every approval, denial, cancellation, and submission the TUI issues is
//! built here as a typed [`nexus_core::Command`] and submitted to
//! `Runtime::handle`. There is no second policy path by construction: this
//! module cannot invoke providers or tools (it does not depend on them),
//! cannot change bound arguments, and cannot grant anything beyond the exact
//! live approval identity the runtime published.

use nexus_core::{
    AgentError, ApproveCommand, CancelCommand, Command, DenyCommand, RequestId, RunId, SessionId,
    SubmitCommand,
};

use crate::state::PendingApprovalCard;

/// Builds the allow-once command for the exact live approval on the card.
/// The bound arguments are the runtime's, never edited here.
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
