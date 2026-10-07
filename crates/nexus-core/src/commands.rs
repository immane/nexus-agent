//! Command/event boundary: typed frontend commands, correlated replies,
//! the run-event envelope with per-run sequence scope, and snapshots.
//!
//! The runtime owns execution state and is the sole publisher of
//! authoritative ordering. Frontends submit commands and maintain
//! presentation state from events; they never invoke providers or tools
//! directly. Sequence numbers are assigned after text batching so published
//! events stay contiguous.

use std::time::Duration;

use crate::error::{AgentError, ErrorCategory, RetryGuidance, is_bounded_safe_text};
use crate::ids::{ApprovalId, CallId, RequestId, RunId, SessionId, ToolId, TurnId};
use crate::limits::Limits;
use crate::outcomes::{
    CommandReply, EffectState, Evidence, ExecutionStatus, RunFinished, RunOutcome, ToolOutcome,
    Usage,
};

/// Maximum frontend input length in bytes (M0-TEST choice).
pub const MAX_INPUT_BYTES: usize = 65_536;
/// Maximum safe summary length in bytes for approval and preview text.
pub const MAX_SUMMARY_BYTES: usize = 1024;
/// Maximum text-fragment length in bytes per event (M0-TEST choice).
pub const MAX_TEXT_FRAGMENT_BYTES: usize = 65_536;
/// Maximum list limit for metadata lookups (M0-TEST choice; finite only).
pub const MAX_LIST_LIMIT: usize = 1024;

/// Submit a correlated request with session, input, and execution profile.
/// Returns a host-issued [`RunId`] on acceptance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitCommand {
    /// Correlation identity (not an idempotency guarantee).
    pub request: RequestId,
    /// Target session.
    pub session: SessionId,
    /// Submitted input text.
    pub input: String,
    /// Selected execution profile name.
    pub profile: String,
    /// True when the run must deny confirmation-required tools without
    /// prompting. Set from the frontend's resolved agent mode (`plan` sets
    /// it); the runtime enforces it at tool dispatch. Defaults to false so
    /// existing full-capability flows are unchanged.
    pub read_only: bool,
}

impl SubmitCommand {
    /// Builds a submit command, validating bounded input.
    pub fn new(
        request: RequestId,
        session: SessionId,
        input: impl Into<String>,
        profile: impl Into<String>,
    ) -> Result<Self, AgentError> {
        let command = Self {
            request,
            session,
            input: input.into(),
            profile: profile.into(),
            read_only: false,
        };
        command.validate()?;
        Ok(command)
    }

    /// Marks the run read-only. Booleans need no validation; call
    /// [`Self::validate`] again only after changing text fields.
    #[must_use]
    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Validates bounded input at the frontend trust boundary. Reused by
    /// [`Self::new`]; call it again for values whose public fields were
    /// changed after construction. Diagnostics are static and never
    /// interpolate the rejected content.
    pub fn validate(&self) -> Result<(), AgentError> {
        if self.input.is_empty() || self.input.len() > MAX_INPUT_BYTES {
            return Err(command_error("submit input is invalid"));
        }
        if self.profile.is_empty() || self.profile.len() > MAX_SUMMARY_BYTES {
            return Err(command_error("submit profile is invalid"));
        }
        Ok(())
    }
}

/// Cancel a live run: stops future dispatch and requests termination of
/// active work without claiming rollback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelCommand {
    /// Correlation identity.
    pub request: RequestId,
    /// Run to cancel.
    pub run: RunId,
}

/// Approve a live approval without changing its bound arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApproveCommand {
    /// Correlation identity.
    pub request: RequestId,
    /// Grant to approve.
    pub approval: ApprovalId,
    /// Owning run.
    pub run: RunId,
    /// Bound call.
    pub call: CallId,
}

/// Refuse a live approval without executing its call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenyCommand {
    /// Correlation identity.
    pub request: RequestId,
    /// Grant to refuse.
    pub approval: ApprovalId,
    /// Owning run.
    pub run: RunId,
    /// Bound call.
    pub call: CallId,
}

/// Request a bounded consistent view of a known run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetSnapshotCommand {
    /// Correlation identity.
    pub request: RequestId,
    /// Run to inspect.
    pub run: RunId,
}

/// Explicit bounded history-metadata lookup; loads no conversation contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListSessionsCommand {
    /// Correlation identity.
    pub request: RequestId,
    /// Maximum entries; finite and nonzero.
    pub limit: usize,
}

impl ListSessionsCommand {
    /// Builds a list command, validating a finite nonzero limit.
    pub fn new(request: RequestId, limit: usize) -> Result<Self, AgentError> {
        let command = Self { request, limit };
        command.validate()?;
        Ok(command)
    }

    /// Validates a finite nonzero limit at the boundary. Reused by
    /// [`Self::new`] and available for publicly mutated limits.
    pub fn validate(&self) -> Result<(), AgentError> {
        if self.limit == 0 || self.limit > MAX_LIST_LIMIT {
            return Err(command_error("list limit must be finite and nonzero"));
        }
        Ok(())
    }
}

/// Load selected bounded history into an idle session without replaying
/// tools or restoring old per-call grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreSessionCommand {
    /// Correlation identity.
    pub request: RequestId,
    /// Session to restore.
    pub session: SessionId,
}

/// Typed frontend commands over one runtime port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Accept a new run or reject explicitly.
    Submit(SubmitCommand),
    /// Stop future dispatch for a run.
    Cancel(CancelCommand),
    /// Approve a live approval.
    Approve(ApproveCommand),
    /// Approve the bound call and its displayed external directory for this
    /// session only. The runtime, not the client, determines the directory.
    ApproveSessionDirectory(ApproveCommand),
    /// Refuse a live approval.
    Deny(DenyCommand),
    /// Request a bounded run snapshot.
    GetSnapshot(GetSnapshotCommand),
    /// Bounded history-metadata lookup.
    ListSessions(ListSessionsCommand),
    /// Bounded history load without replay or old grants.
    RestoreSession(RestoreSessionCommand),
}

impl Command {
    /// Returns the correlation identity for every command.
    #[must_use]
    pub fn request(&self) -> &RequestId {
        match self {
            Self::Submit(command) => &command.request,
            Self::Cancel(command) => &command.request,
            Self::Approve(command) => &command.request,
            Self::ApproveSessionDirectory(command) => &command.request,
            Self::Deny(command) => &command.request,
            Self::GetSnapshot(command) => &command.request,
            Self::ListSessions(command) => &command.request,
            Self::RestoreSession(command) => &command.request,
        }
    }
}

/// Correlated response to one processed command. A second run while one is
/// active replies [`CommandReply::Busy`]; repeated approvals or
/// cancellations never cause duplicate dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResponse {
    request: RequestId,
    reply: CommandReply,
    run: Option<RunId>,
}

impl CommandResponse {
    /// Builds a correlated response.
    #[must_use]
    pub fn new(request: RequestId, reply: CommandReply, run: Option<RunId>) -> Self {
        Self {
            request,
            reply,
            run,
        }
    }

    /// Returns the correlation identity.
    #[must_use]
    pub fn request(&self) -> &RequestId {
        &self.request
    }

    /// Returns the reply outcome.
    #[must_use]
    pub fn reply(&self) -> CommandReply {
        self.reply
    }

    /// Returns the accepted run, if any.
    #[must_use]
    pub fn run(&self) -> Option<&RunId> {
        self.run.as_ref()
    }
}

/// Per-run event sequence number. Scoped to the owning run and assigned by
/// the runtime after batching; frontends must reject stale-run updates.
pub type EventSequence = u64;

/// Returns the sequence following `last`, or [`None`] on overflow.
/// Overflow is reported, never wrapped silently.
#[must_use]
pub fn checked_next_sequence(last: EventSequence) -> Option<EventSequence> {
    last.checked_add(1)
}

/// Ordered presentation fragment for an identified turn and item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantText {
    /// Owning turn.
    pub turn: TurnId,
    /// Stable turn-local item key.
    pub item_key: String,
    /// Fragment text.
    pub text: String,
}

impl AssistantText {
    /// Builds a text fragment, validating bounds at the publishing boundary.
    pub fn new(
        turn: TurnId,
        item_key: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<Self, AgentError> {
        let fragment = Self {
            turn,
            item_key: item_key.into(),
            text: text.into(),
        };
        fragment.validate()?;
        Ok(fragment)
    }

    /// Validates the fragment bounds. Reused by [`Self::new`]; call it again
    /// when public fields are changed after construction.
    pub fn validate(&self) -> Result<(), AgentError> {
        if self.item_key.is_empty()
            || self.item_key.len() > crate::content::MAX_ITEM_KEY_LEN
            || self.text.len() > MAX_TEXT_FRAGMENT_BYTES
        {
            return Err(command_error("assistant text fragment is invalid"));
        }
        Ok(())
    }
}

/// Exact approval request: identity, safe action summary, affected scope,
/// bounded exact-arguments preview, and expiry. Previews never authorize;
/// only the grant path through [`ApproveCommand`] does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalNotice {
    /// Grant identity.
    pub approval: ApprovalId,
    /// Bound call.
    pub call: CallId,
    /// Caller-redacted safe action summary. Bounded and marker-checked;
    /// the marker net is best-effort, not proof of secret-freedom.
    pub summary: String,
    /// Caller-redacted affected resource scope text with the same bounds as
    /// [`Self::summary`].
    pub scope_summary: String,
    /// Caller-redacted bounded preview of the exact normalized arguments
    /// that would execute, if one was attached.
    pub args_preview: Option<String>,
    /// Optional canonical external directory offered for session-only read/
    /// write access, including descendants. Absent in strict mode.
    pub session_directory: Option<String>,
    /// Monotonic expiry reading.
    pub expires_at_elapsed: Duration,
}

impl ApprovalNotice {
    /// Builds a notice with bounded scope and summary text. The caller must
    /// pre-redact both fields; the marker net only rejects known secret
    /// shapes. The existing signature is preserved; attach the optional
    /// exact-arguments preview with [`Self::with_args_preview`].
    pub fn new(
        approval: ApprovalId,
        call: CallId,
        summary: impl Into<String>,
        scope_summary: impl Into<String>,
        expires_at_elapsed: Duration,
    ) -> Result<Self, AgentError> {
        let notice = Self {
            approval,
            call,
            summary: summary.into(),
            scope_summary: scope_summary.into(),
            args_preview: None,
            session_directory: None,
            expires_at_elapsed,
        };
        notice.validate()?;
        Ok(notice)
    }

    /// Attaches a caller-redacted bounded preview of the exact normalized
    /// arguments that would execute. Additive builder: [`Self::new`] keeps
    /// its signature and an absent preview stays [`None`].
    pub fn with_args_preview(
        mut self,
        args_preview: impl Into<String>,
    ) -> Result<Self, AgentError> {
        let args_preview = args_preview.into();
        if !is_bounded_safe_text(&args_preview, MAX_SUMMARY_BYTES) {
            return Err(command_error("approval args preview is invalid"));
        }
        self.args_preview = Some(args_preview);
        Ok(self)
    }

    /// Returns the exact-arguments preview, if one was attached.
    #[must_use]
    pub fn args_preview(&self) -> Option<&str> {
        self.args_preview.as_deref()
    }

    /// Validates bounded, caller-redacted summary, scope, and preview text.
    /// Reused by [`Self::new`] and [`Self::with_args_preview`]; call it
    /// again for publicly mutated fields. Diagnostics are static and never
    /// interpolate the rejected content.
    pub fn validate(&self) -> Result<(), AgentError> {
        if self
            .session_directory
            .as_ref()
            .is_some_and(|directory| !is_bounded_safe_text(directory, MAX_SUMMARY_BYTES))
        {
            return Err(command_error("approval session directory is invalid"));
        }
        if !is_bounded_safe_text(&self.summary, MAX_SUMMARY_BYTES) {
            return Err(command_error("approval summary is invalid"));
        }
        if !is_bounded_safe_text(&self.scope_summary, MAX_SUMMARY_BYTES) {
            return Err(command_error("approval scope is invalid"));
        }
        if let Some(preview) = &self.args_preview
            && !is_bounded_safe_text(preview, MAX_SUMMARY_BYTES)
        {
            return Err(command_error("approval args preview is invalid"));
        }
        Ok(())
    }
}

/// An authorized admitted call has entered execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolStartedInfo {
    /// Executing call.
    pub call: CallId,
    /// Admitted tool identity (not an authorization grant).
    pub tool: ToolId,
    /// Bounded, policy-redacted operation preview; absent when it cannot be
    /// displayed safely. Never raw model arguments.
    pub args_preview: Option<String>,
}

impl ToolStartedInfo {
    /// Revalidates public preview text at the event publishing boundary.
    pub fn validate(&self) -> Result<(), AgentError> {
        if self
            .args_preview
            .as_ref()
            .is_some_and(|preview| !is_bounded_safe_text(preview, MAX_SUMMARY_BYTES))
        {
            return Err(command_error("tool started preview is invalid"));
        }
        Ok(())
    }
}

/// Optional bounded progress with explicit truncation state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolProgress {
    /// Executing call.
    pub call: CallId,
    /// Progress preview text.
    pub preview: String,
    /// True when progress was cut to fit the shared output budget.
    pub truncated: bool,
}

impl ToolProgress {
    /// Builds progress bounded by the shared output budget.
    pub fn new(
        call: CallId,
        preview: impl Into<String>,
        truncated: bool,
    ) -> Result<Self, AgentError> {
        let progress = Self {
            call,
            preview: preview.into(),
            truncated,
        };
        progress.validate()?;
        Ok(progress)
    }

    /// Validates the progress preview against the shared output budget.
    /// Reused by [`Self::new`]; call it again for publicly mutated fields.
    pub fn validate(&self) -> Result<(), AgentError> {
        if self.preview.len() > Limits::M0_TEST_TOOL_OUTPUT_BYTES {
            return Err(command_error("tool progress exceeds output budget"));
        }
        Ok(())
    }
}

/// Actual outcome and effect/evidence summary for a finished call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolFinishedInfo {
    /// Finished call.
    pub call: CallId,
    /// Actual outcome.
    pub outcome: ToolOutcome,
}

/// Typed run-event payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventPayload {
    /// Accepted run identity and originating request for reconciliation.
    RunStarted {
        /// Originating request.
        request: RequestId,
    },
    /// Ordered presentation fragment.
    AssistantTextDelta(AssistantText),
    /// Non-executable proposed-call progress.
    ToolCallPreview {
        /// Turn-local item key.
        item_key: String,
    },
    /// Exact approval request with safe summary and expiry.
    ApprovalRequired(ApprovalNotice),
    /// An authorized call entered execution.
    ToolStarted(ToolStartedInfo),
    /// Optional bounded progress.
    ToolOutput(ToolProgress),
    /// Actual call outcome.
    ToolFinished(ToolFinishedInfo),
    /// Available usage information, provisional or final.
    UsageUpdated(Usage),
    /// Terminal outcome and persistence state.
    RunFinished(RunFinished),
}

impl EventPayload {
    /// Validates payload-level bounds at the publishing boundary. Types with
    /// private, constructor-validated fields need no further checks; payload
    /// variants carrying public text are revalidated here. Runtime-owned
    /// ordering and lifecycle rules are not part of this check.
    pub fn validate(&self) -> Result<(), AgentError> {
        match self {
            Self::AssistantTextDelta(text) => text.validate(),
            Self::ToolCallPreview { item_key } => {
                if item_key.is_empty() || item_key.len() > crate::content::MAX_ITEM_KEY_LEN {
                    return Err(command_error("tool call preview is invalid"));
                }
                Ok(())
            }
            Self::ApprovalRequired(notice) => notice.validate(),
            Self::ToolStarted(info) => info.validate(),
            Self::ToolOutput(progress) => progress.validate(),
            Self::RunStarted { .. }
            | Self::ToolFinished(_)
            | Self::UsageUpdated(_)
            | Self::RunFinished(_) => Ok(()),
        }
    }
}

/// Authoritative run event: owning session and run plus a contiguous
/// per-run sequence number assigned by the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEvent {
    session: SessionId,
    run: RunId,
    seq: EventSequence,
    payload: EventPayload,
}

impl RunEvent {
    /// Builds an envelope. Contiguity is the runtime's responsibility.
    #[must_use]
    pub fn new(session: SessionId, run: RunId, seq: EventSequence, payload: EventPayload) -> Self {
        Self {
            session,
            run,
            seq,
            payload,
        }
    }

    /// Returns the owning session.
    #[must_use]
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// Returns the owning run.
    #[must_use]
    pub fn run(&self) -> &RunId {
        &self.run
    }

    /// Returns the per-run sequence number.
    #[must_use]
    pub fn seq(&self) -> EventSequence {
        self.seq
    }

    /// Returns the payload.
    #[must_use]
    pub fn payload(&self) -> &EventPayload {
        &self.payload
    }

    /// Returns true for the terminal run payload.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self.payload, EventPayload::RunFinished(_))
    }

    /// Validates the contained payload bounds at the publishing boundary.
    /// Sequence contiguity and ownership are assigned and enforced by the
    /// runtime, not by this check.
    pub fn validate(&self) -> Result<(), AgentError> {
        self.payload.validate()
    }
}

/// Run lifecycle for snapshots: active or finalized with its outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunLifecycle {
    /// The run has no terminal event yet.
    Active,
    /// The run reached its terminal event.
    Finalized(RunOutcome),
}

/// One known tool outcome summarized for snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeSummary {
    /// Finished call.
    pub call: CallId,
    /// Execution status.
    pub status: ExecutionStatus,
    /// Known effect state.
    pub effect: EffectState,
    /// Evidence class.
    pub evidence: Evidence,
}

/// Bounded snapshot: last published sequence, lifecycle, pending approvals,
/// known tool outcomes, and whether retained presentation content was cut.
/// Never a full replay log or permission to resume interrupted work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    session: SessionId,
    run: RunId,
    last_sequence: Option<EventSequence>,
    lifecycle: RunLifecycle,
    pending_approvals: Vec<ApprovalId>,
    known_outcomes: Vec<OutcomeSummary>,
    content_truncated: bool,
}

impl Snapshot {
    /// Builds a snapshot, bounding pending approvals by the concurrency
    /// budget and known outcomes by the per-run call budget.
    pub fn new(
        session: SessionId,
        run: RunId,
        last_sequence: Option<EventSequence>,
        lifecycle: RunLifecycle,
        pending_approvals: Vec<ApprovalId>,
        known_outcomes: Vec<OutcomeSummary>,
        content_truncated: bool,
    ) -> Result<Self, AgentError> {
        if pending_approvals.len() > Limits::M0_TEST_MAX_CONCURRENT_OPS {
            return Err(command_error("snapshot exceeds pending approval bound"));
        }
        if known_outcomes.len() > Limits::M0_TEST_TOOL_CALLS_PER_RUN as usize {
            return Err(command_error("snapshot exceeds known outcome bound"));
        }
        Ok(Self {
            session,
            run,
            last_sequence,
            lifecycle,
            pending_approvals,
            known_outcomes,
            content_truncated,
        })
    }

    /// Returns the owning session.
    #[must_use]
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// Returns the snapshotted run.
    #[must_use]
    pub fn run(&self) -> &RunId {
        &self.run
    }

    /// Returns the last published sequence, if any event was published.
    #[must_use]
    pub fn last_sequence(&self) -> Option<EventSequence> {
        self.last_sequence
    }

    /// Returns the lifecycle state.
    #[must_use]
    pub fn lifecycle(&self) -> RunLifecycle {
        self.lifecycle
    }

    /// Returns the pending approvals.
    #[must_use]
    pub fn pending_approvals(&self) -> &[ApprovalId] {
        &self.pending_approvals
    }

    /// Returns the known tool outcomes.
    #[must_use]
    pub fn known_outcomes(&self) -> &[OutcomeSummary] {
        &self.known_outcomes
    }

    /// Returns true when retained presentation content was truncated.
    #[must_use]
    pub fn is_content_truncated(&self) -> bool {
        self.content_truncated
    }
}

fn command_error(message: &'static str) -> AgentError {
    AgentError::new(
        ErrorCategory::InvalidInput,
        message,
        RetryGuidance::DoNotRetry,
    )
    .expect("static safe command message builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcomes::{PersistenceState, UsageFinality};

    fn session_run() -> (SessionId, RunId) {
        (
            SessionId::new("sess-1").expect("valid"),
            RunId::new("run-1").expect("valid"),
        )
    }

    #[test]
    fn every_command_carries_its_request() {
        let request = RequestId::new("req-1").expect("valid");
        let (session, run) = session_run();
        let commands = vec![
            Command::Submit(
                SubmitCommand::new(request.clone(), session.clone(), "go", "p")
                    .expect("valid submit builds"),
            ),
            Command::Cancel(CancelCommand {
                request: request.clone(),
                run: run.clone(),
            }),
            Command::GetSnapshot(GetSnapshotCommand {
                request: request.clone(),
                run: run.clone(),
            }),
            Command::ListSessions(
                ListSessionsCommand::new(request.clone(), 10).expect("valid list builds"),
            ),
            Command::RestoreSession(RestoreSessionCommand {
                request: request.clone(),
                session,
            }),
        ];
        for command in &commands {
            assert_eq!(command.request(), &request);
        }
        let _ = run;
    }

    #[test]
    fn replies_cover_busy_finalized_and_stale_targets() {
        let request = RequestId::new("req-1").expect("valid");
        let (session, run) = session_run();
        for reply in [
            CommandReply::Accepted,
            CommandReply::Rejected,
            CommandReply::AlreadyFinalized,
            CommandReply::StaleOrUnknownTarget,
            CommandReply::Busy,
        ] {
            let response = CommandResponse::new(request.clone(), reply, Some(run.clone()));
            assert_eq!(response.reply(), reply);
            assert_eq!(response.run(), Some(&run));
        }
        let _ = session;
    }

    #[test]
    fn run_event_envelope_carries_owner_and_contiguous_sequence() {
        let (session, run) = session_run();
        let first = RunEvent::new(
            session.clone(),
            run.clone(),
            0,
            EventPayload::RunStarted {
                request: RequestId::new("req-1").expect("valid"),
            },
        );
        assert!(!first.is_terminal());
        let next = checked_next_sequence(first.seq()).expect("sequence advances");
        assert_eq!(next, 1);
        let finished = RunEvent::new(
            session,
            run,
            next,
            EventPayload::RunFinished(
                RunFinished::new(RunOutcome::Completed, PersistenceState::Ephemeral, None)
                    .expect("terminal record builds"),
            ),
        );
        assert!(finished.is_terminal());
        assert_eq!(checked_next_sequence(EventSequence::MAX), None);
    }

    #[test]
    fn approval_notice_rejects_secret_summaries() {
        let notice = ApprovalNotice::new(
            ApprovalId::new("appr-1").expect("valid"),
            CallId::new("call-1").expect("valid"),
            "delete directory",
            "project scope",
            Duration::from_secs(120),
        )
        .expect("safe summary builds");
        assert_eq!(notice.summary, "delete directory");
        assert_eq!(notice.scope_summary, "project scope");
        assert!(
            ApprovalNotice::new(
                ApprovalId::new("appr-1").expect("valid"),
                CallId::new("call-1").expect("valid"),
                "using api_key=AAAA",
                "project scope",
                Duration::from_secs(120),
            )
            .is_err(),
            "secret-bearing summaries never become notices"
        );
        assert!(
            ApprovalNotice::new(
                ApprovalId::new("appr-1").expect("valid"),
                CallId::new("call-1").expect("valid"),
                "delete directory",
                "secret=AAAA",
                Duration::from_secs(120),
            )
            .is_err(),
            "secret-bearing scopes never become notices"
        );
    }

    #[test]
    fn snapshot_bounds_pending_and_known_outcomes() {
        let (session, run) = session_run();
        let pending = (0..Limits::M0_TEST_MAX_CONCURRENT_OPS + 1)
            .map(|index| ApprovalId::new(format!("appr-{index}")).expect("valid"))
            .collect();
        assert!(
            Snapshot::new(
                session.clone(),
                run.clone(),
                Some(4),
                RunLifecycle::Active,
                pending,
                vec![],
                false,
            )
            .is_err()
        );
        let snapshot = Snapshot::new(
            session,
            run,
            Some(4),
            RunLifecycle::Finalized(RunOutcome::Completed),
            vec![],
            vec![],
            true,
        )
        .expect("bounded snapshot builds");
        assert_eq!(snapshot.last_sequence(), Some(4));
        assert!(snapshot.is_content_truncated());
    }

    #[test]
    fn list_limit_must_be_finite_and_nonzero() {
        let request = RequestId::new("req-1").expect("valid");
        assert!(ListSessionsCommand::new(request.clone(), 0).is_err());
        assert!(ListSessionsCommand::new(request.clone(), MAX_LIST_LIMIT + 1).is_err());
        assert!(ListSessionsCommand::new(request, 1).is_ok());
    }

    #[test]
    fn public_mutation_validation_catches_boundary_bypasses() {
        let request = RequestId::new("req-1").expect("valid");
        let (session, run) = session_run();
        let call = CallId::new("call-1").expect("valid");

        let mut submit = SubmitCommand::new(request.clone(), session.clone(), "go", "p")
            .expect("valid submit builds");
        submit.input.clear();
        assert!(submit.validate().is_err(), "empty input is rejected");
        submit.input = "x".repeat(MAX_INPUT_BYTES + 1);
        assert!(submit.validate().is_err(), "oversized input is rejected");
        submit.input = "go".to_owned();
        submit.profile.clear();
        assert!(submit.validate().is_err(), "empty profile is rejected");

        let mut list = ListSessionsCommand::new(request.clone(), 1).expect("valid list builds");
        list.limit = 0;
        assert!(list.validate().is_err(), "mutated zero limit is rejected");
        list.limit = MAX_LIST_LIMIT + 1;
        assert!(
            list.validate().is_err(),
            "mutated oversized limit is rejected"
        );

        let mut fragment =
            AssistantText::new(TurnId::new("turn-1").expect("valid"), "item-0", "hi")
                .expect("valid fragment builds");
        fragment.item_key.clear();
        assert!(fragment.validate().is_err(), "empty item key is rejected");
        fragment.item_key = "item-0".to_owned();
        fragment.text = "x".repeat(MAX_TEXT_FRAGMENT_BYTES + 1);
        assert!(fragment.validate().is_err(), "oversized text is rejected");

        let mut progress =
            ToolProgress::new(call.clone(), "ok", false).expect("valid progress builds");
        progress.preview = "x".repeat(Limits::M0_TEST_TOOL_OUTPUT_BYTES + 1);
        assert!(
            progress.validate().is_err(),
            "mutated progress exceeds the output budget"
        );

        let mut notice = ApprovalNotice::new(
            ApprovalId::new("appr-1").expect("valid"),
            call,
            "delete directory",
            "project scope",
            Duration::from_secs(120),
        )
        .expect("valid notice builds");
        notice.summary = "api_key=AAAA".to_owned();
        assert!(
            notice.validate().is_err(),
            "mutated secret summary is rejected"
        );
        notice.summary = "delete directory".to_owned();
        notice.args_preview = Some("secret=AAAA".to_owned());
        assert!(
            notice.validate().is_err(),
            "mutated secret preview is rejected"
        );

        let valid = RunEvent::new(
            session.clone(),
            run.clone(),
            0,
            EventPayload::RunStarted {
                request: request.clone(),
            },
        );
        assert!(valid.validate().is_ok());
        let invalid = RunEvent::new(
            session,
            run,
            1,
            EventPayload::ToolCallPreview {
                item_key: String::new(),
            },
        );
        assert!(
            invalid.validate().is_err(),
            "empty preview item key is rejected"
        );
    }

    #[test]
    fn approval_notice_args_preview_is_additive_and_bounded() {
        fn base() -> ApprovalNotice {
            ApprovalNotice::new(
                ApprovalId::new("appr-1").expect("valid"),
                CallId::new("call-1").expect("valid"),
                "run tool host_write",
                "project scope",
                Duration::from_secs(120),
            )
            .expect("valid notice builds")
        }
        assert_eq!(
            base().args_preview(),
            None,
            "old constructor adds no preview"
        );
        let notice = base()
            .with_args_preview(r#"{"path":"src","mode":"read"}"#)
            .expect("safe preview attaches");
        assert_eq!(
            notice.args_preview(),
            Some(r#"{"path":"src","mode":"read"}"#)
        );
        assert_eq!(notice.summary, "run tool host_write");
        assert!(
            base().with_args_preview("password=hunter2").is_err(),
            "secret-bearing preview never attaches"
        );
        assert!(
            base()
                .with_args_preview("x".repeat(MAX_SUMMARY_BYTES + 1))
                .is_err(),
            "oversized preview never attaches"
        );
        assert!(
            base().with_args_preview("").is_err(),
            "empty preview is invalid"
        );
    }

    #[test]
    fn usage_event_carries_provisional_or_final_label() {
        let usage = Usage::new(Some(10), None, UsageFinality::Provisional);
        assert_eq!(usage.input_tokens(), Some(10));
        assert_eq!(usage.output_tokens(), None);
        let payload = EventPayload::UsageUpdated(usage);
        assert!(matches!(payload, EventPayload::UsageUpdated(_)));
    }
}

#[cfg(test)]
mod cov_commands_private {
    use super::*;
    use crate::error::is_bounded_safe_text;

    #[test]
    fn command_error_is_static_invalid_input_without_retry() {
        let error = command_error("submit input is invalid");
        assert_eq!(error.category(), ErrorCategory::InvalidInput);
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        assert_eq!(error.message(), "submit input is invalid");
        assert_eq!(error.to_string(), "[invalid-input] submit input is invalid");
        assert!(error.correlation().is_empty());
    }

    #[test]
    fn bounded_safe_text_enforces_empty_bound_and_marker_rules() {
        assert!(is_bounded_safe_text("delete directory", MAX_SUMMARY_BYTES));
        assert!(!is_bounded_safe_text("", MAX_SUMMARY_BYTES));
        assert!(!is_bounded_safe_text("x", 0));

        let exact = "x".repeat(MAX_SUMMARY_BYTES);
        assert!(is_bounded_safe_text(&exact, MAX_SUMMARY_BYTES));
        assert!(!is_bounded_safe_text(
            &"x".repeat(MAX_SUMMARY_BYTES + 1),
            MAX_SUMMARY_BYTES
        ));

        for marker in [
            "PASSWORD=",
            "Bearer ",
            "API_KEY=",
            "Client_Secret",
            "-----BEGIN",
            "sk-",
            "AKIAIOSFODNN7EXAMPLE",
            "ghp_",
            "xoxb-",
            "passwd=",
            "secret=",
            "apikey=",
        ] {
            assert!(
                !is_bounded_safe_text(marker, MAX_SUMMARY_BYTES),
                "marker {marker:?} must be rejected"
            );
        }

        // Documented limitation: the marker net is best-effort, not a secret
        // detector; callers must still pre-redact.
        assert!(is_bounded_safe_text("hunter2", MAX_SUMMARY_BYTES));
    }

    #[test]
    fn private_field_construction_matches_read_accessors() {
        let event = RunEvent {
            session: SessionId::new("sess-1").expect("valid"),
            run: RunId::new("run-1").expect("valid"),
            seq: 9,
            payload: EventPayload::RunStarted {
                request: RequestId::new("req-1").expect("valid"),
            },
        };
        assert_eq!(event.session().as_str(), "sess-1");
        assert_eq!(event.run().as_str(), "run-1");
        assert_eq!(event.seq(), 9);
        assert!(matches!(event.payload(), EventPayload::RunStarted { .. }));
        assert!(!event.is_terminal());
        event.validate().expect("valid envelope validates");

        let response = CommandResponse {
            request: RequestId::new("req-1").expect("valid"),
            reply: CommandReply::Accepted,
            run: None,
        };
        assert_eq!(response.request().as_str(), "req-1");
        assert_eq!(response.reply(), CommandReply::Accepted);
        assert!(response.run().is_none());
    }
}
