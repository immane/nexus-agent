//! Command/event boundary: typed frontend commands, correlated replies,
//! the run-event envelope with per-run sequence scope, and snapshots.
//!
//! The runtime owns execution state and is the sole publisher of
//! authoritative ordering. Frontends submit commands and maintain
//! presentation state from events; they never invoke providers or tools
//! directly. Sequence numbers are assigned after text batching so published
//! events stay contiguous.

use std::time::Duration;

use crate::error::{AgentError, ErrorCategory, RetryGuidance};
use crate::ids::{ApprovalId, CallId, RequestId, RunId, SessionId, TurnId};
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
}

impl SubmitCommand {
    /// Validates bounded input at the frontend boundary.
    pub fn new(
        request: RequestId,
        session: SessionId,
        input: impl Into<String>,
        profile: impl Into<String>,
    ) -> Result<Self, AgentError> {
        let input = input.into();
        let profile = profile.into();
        if input.is_empty() || input.len() > MAX_INPUT_BYTES {
            return Err(command_error("submit input is invalid"));
        }
        if profile.is_empty() || profile.len() > MAX_SUMMARY_BYTES {
            return Err(command_error("submit profile is invalid"));
        }
        Ok(Self {
            request,
            session,
            input,
            profile,
        })
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
    /// Validates a finite nonzero limit.
    pub fn new(request: RequestId, limit: usize) -> Result<Self, AgentError> {
        if limit == 0 || limit > MAX_LIST_LIMIT {
            return Err(command_error("list limit must be finite and nonzero"));
        }
        Ok(Self { request, limit })
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
    /// Validates fragment bounds at the publishing boundary.
    pub fn new(
        turn: TurnId,
        item_key: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<Self, AgentError> {
        let item_key = item_key.into();
        let text = text.into();
        if item_key.is_empty()
            || item_key.len() > crate::content::MAX_ITEM_KEY_LEN
            || text.len() > MAX_TEXT_FRAGMENT_BYTES
        {
            return Err(command_error("assistant text fragment is invalid"));
        }
        Ok(Self {
            turn,
            item_key,
            text,
        })
    }
}

/// Exact approval request: identity, safe action summary, affected scope,
/// and expiry. Previews never authorize; only the grant path through
/// [`ApproveCommand`] does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalNotice {
    /// Grant identity.
    pub approval: ApprovalId,
    /// Bound call.
    pub call: CallId,
    /// Safe action summary with no secrets.
    pub summary: String,
    /// Affected resource scope text with no secrets.
    pub scope_summary: String,
    /// Monotonic expiry reading.
    pub expires_at_elapsed: Duration,
}

impl ApprovalNotice {
    /// Validates secret-free bounded summary and scope text.
    pub fn new(
        approval: ApprovalId,
        call: CallId,
        summary: impl Into<String>,
        scope_summary: impl Into<String>,
        expires_at_elapsed: Duration,
    ) -> Result<Self, AgentError> {
        let summary = AgentError::new(
            ErrorCategory::PermissionDenied,
            summary.into(),
            RetryGuidance::DoNotRetry,
        )
        .map(|safe| safe.message().to_owned())
        .map_err(|_| command_error("approval summary is invalid"))?;
        let scope_summary = AgentError::new(
            ErrorCategory::PermissionDenied,
            scope_summary.into(),
            RetryGuidance::DoNotRetry,
        )
        .map(|safe| safe.message().to_owned())
        .map_err(|_| command_error("approval scope is invalid"))?;
        Ok(Self {
            approval,
            call,
            summary,
            scope_summary,
            expires_at_elapsed,
        })
    }
}

/// An authorized admitted call has entered execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolStartedInfo {
    /// Executing call.
    pub call: CallId,
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
    /// Validates progress against the shared output budget.
    pub fn new(
        call: CallId,
        preview: impl Into<String>,
        truncated: bool,
    ) -> Result<Self, AgentError> {
        let preview = preview.into();
        if preview.len() > Limits::M0_TEST_TOOL_OUTPUT_BYTES {
            return Err(command_error("tool progress exceeds output budget"));
        }
        Ok(Self {
            call,
            preview,
            truncated,
        })
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
    fn usage_event_carries_provisional_or_final_label() {
        let usage = Usage::new(Some(10), None, UsageFinality::Provisional);
        assert_eq!(usage.input_tokens(), Some(10));
        assert_eq!(usage.output_tokens(), None);
        let payload = EventPayload::UsageUpdated(usage);
        assert!(matches!(payload, EventPayload::UsageUpdated(_)));
    }
}
