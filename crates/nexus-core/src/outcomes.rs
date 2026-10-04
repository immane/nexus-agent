//! Outcome types: execution status, effect state, evidence, provider
//! terminal events, run terminal outcomes, and command replies.
//!
//! These concepts stay separate: what finished, what effects are known,
//! what evidence supports that, and whether the user's task succeeded.
//! Truncated, refused, or incomplete output is never labeled complete
//! success; missing usage stays unknown, never zero.

use crate::content::ContinuationData;
use crate::error::{AgentError, ErrorCategory, RetryGuidance};
use crate::limits::Limits;

/// What finished, failed, or was interrupted during tool execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionStatus {
    /// The tool ran to its own completion.
    Succeeded,
    /// The tool ran and reported failure.
    Failed,
    /// Authorization or approval refused execution; nothing started.
    Denied,
    /// Cancellation stopped or interrupted execution.
    Cancelled,
    /// The per-tool deadline elapsed with uncertain effects.
    TimedOut,
}

/// What changes are known to have happened. Cancellation never implies
/// rollback: effects that raced with cancellation stay recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectState {
    /// Execution never started (denials, pre-dispatch failures).
    NotStarted,
    /// Started and known to have applied nothing.
    KnownNotApplied,
    /// Started and known to have applied.
    KnownApplied,
    /// Started but the result is inconclusive; blind retry is forbidden.
    Unknown,
}

/// Whether an outcome is host-observed, plugin-reported, or uncertain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// The host directly observed the result.
    HostObserved,
    /// The plugin reported the result without host observation.
    PluginReported,
    /// Neither host nor plugin can establish what happened.
    Uncertain,
}

/// Actual tool outcome: status, bounded content, effect/evidence, and an
/// explicit truncation flag. Denied outcomes must carry [`EffectState::NotStarted`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    status: ExecutionStatus,
    effect: EffectState,
    evidence: Evidence,
    content: String,
    truncated: bool,
}

impl ToolOutcome {
    /// Builds an outcome, enforcing the shared output budget and the
    /// denied-implies-not-started invariant.
    pub fn new(
        status: ExecutionStatus,
        effect: EffectState,
        evidence: Evidence,
        content: impl Into<String>,
        truncated: bool,
    ) -> Result<Self, AgentError> {
        let content = content.into();
        if content.len() > Limits::M0_TEST_TOOL_OUTPUT_BYTES {
            return Err(outcome_error("tool outcome exceeds output budget"));
        }
        if status == ExecutionStatus::Denied && effect != EffectState::NotStarted {
            return Err(outcome_error(
                "denied outcome must have not-started effects",
            ));
        }
        Ok(Self {
            status,
            effect,
            evidence,
            content,
            truncated,
        })
    }

    /// Returns the execution status.
    #[must_use]
    pub fn status(&self) -> ExecutionStatus {
        self.status
    }

    /// Returns the known effect state.
    #[must_use]
    pub fn effect(&self) -> EffectState {
        self.effect
    }

    /// Returns the evidence class.
    #[must_use]
    pub fn evidence(&self) -> Evidence {
        self.evidence
    }

    /// Returns the bounded outcome content.
    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Returns true when content was cut to fit the budget.
    #[must_use]
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }
}

/// Why a fully consumed model invocation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    /// Ordinary stop with complete content.
    Stop,
    /// Ended proposing tool calls.
    ToolCalls,
    /// Ended because an output limit was reached; never complete success.
    OutputLimit,
    /// The model refused.
    Refusal,
    /// Incomplete or error outcome.
    Incomplete,
}

/// Whether reported usage counters are provisional or final.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageFinality {
    /// Counters may still be updated.
    Provisional,
    /// Final counters for the invocation.
    Final,
}

/// Available usage counters. Missing counters stay [`None`] (unknown) and
/// are never fabricated as zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    finality: UsageFinality,
}

impl Usage {
    /// Builds a usage record; [`None`] means unknown.
    #[must_use]
    pub fn new(
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        finality: UsageFinality,
    ) -> Self {
        Self {
            input_tokens,
            output_tokens,
            finality,
        }
    }

    /// Returns known input tokens, or [`None`] when unknown.
    #[must_use]
    pub fn input_tokens(&self) -> Option<u64> {
        self.input_tokens
    }

    /// Returns known output tokens, or [`None`] when unknown.
    #[must_use]
    pub fn output_tokens(&self) -> Option<u64> {
        self.output_tokens
    }

    /// Returns whether the counters are provisional or final.
    #[must_use]
    pub fn finality(&self) -> UsageFinality {
        self.finality
    }
}

/// Complete normalized assistant turn with finish reason, final usage, and
/// optional continuation data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnFinished {
    reason: FinishReason,
    usage: Usage,
    continuation: Option<ContinuationData>,
}

impl TurnFinished {
    /// Builds a finished-turn record.
    #[must_use]
    pub fn new(reason: FinishReason, usage: Usage, continuation: Option<ContinuationData>) -> Self {
        Self {
            reason,
            usage,
            continuation,
        }
    }

    /// Returns the finish reason.
    #[must_use]
    pub fn reason(&self) -> FinishReason {
        self.reason
    }

    /// Returns the usage counters.
    #[must_use]
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// Returns the continuation data, if any.
    #[must_use]
    pub fn continuation(&self) -> Option<&ContinuationData> {
        self.continuation.as_ref()
    }
}

/// Terminal event for one fully consumed provider invocation: exactly one
/// of these follows the stream, then no further events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvocationOutcome {
    /// Complete normalized turn.
    TurnFinished(TurnFinished),
    /// Typed terminal failure for this invocation.
    Failed(AgentError),
}

impl InvocationOutcome {
    /// Returns the finish reason for successful turns.
    #[must_use]
    pub fn finish_reason(&self) -> Option<FinishReason> {
        match self {
            Self::TurnFinished(finished) => Some(finished.reason()),
            Self::Failed(_) => None,
        }
    }
}

/// Terminal run outcomes. Completion describes the runtime lifecycle, not
/// proof of every requested business outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// The run reached its lifecycle end normally.
    Completed,
    /// The model refused the task.
    Refused,
    /// The run failed with a typed error.
    Failed,
    /// Cancellation stopped the run; effects stay as observed.
    Cancelled,
    /// A finite budget was exhausted with an explicit outcome.
    LimitReached,
}

/// How durable the terminal run record is. M0 uses ephemeral storage that
/// self-identifies as non-durable (lock section 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistenceState {
    /// Ephemeral record only; crash recovery is unavailable.
    Ephemeral,
    /// Record reached the required durability level.
    Saved,
    /// Required persistence failed; observed effects are preserved.
    SaveFailed,
}

/// Terminal run record. A save failure is reported alongside the outcome
/// without losing already observed tool effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunFinished {
    outcome: RunOutcome,
    persistence: PersistenceState,
    persistence_error: Option<AgentError>,
}

impl RunFinished {
    /// Builds a terminal record. A persistence error is allowed only with
    /// [`PersistenceState::SaveFailed`], and is required with it.
    pub fn new(
        outcome: RunOutcome,
        persistence: PersistenceState,
        persistence_error: Option<AgentError>,
    ) -> Result<Self, AgentError> {
        if persistence == PersistenceState::SaveFailed && persistence_error.is_none() {
            return Err(outcome_error("save failure requires its error"));
        }
        if persistence != PersistenceState::SaveFailed && persistence_error.is_some() {
            return Err(outcome_error("persistence error without save failure"));
        }
        Ok(Self {
            outcome,
            persistence,
            persistence_error,
        })
    }

    /// Returns the terminal outcome.
    #[must_use]
    pub fn outcome(&self) -> RunOutcome {
        self.outcome
    }

    /// Returns the persistence state.
    #[must_use]
    pub fn persistence(&self) -> PersistenceState {
        self.persistence
    }

    /// Returns the persistence failure, if any.
    #[must_use]
    pub fn persistence_error(&self) -> Option<&AgentError> {
        self.persistence_error.as_ref()
    }
}

/// Correlated reply to a processed command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandReply {
    /// The command was accepted.
    Accepted,
    /// The command was rejected with an explicit reason.
    Rejected,
    /// The target run already reached its terminal event.
    AlreadyFinalized,
    /// The target run, approval, or sequence is stale or unknown.
    StaleOrUnknownTarget,
    /// One run is already active; the baseline rejects a second run.
    Busy,
}

fn outcome_error(message: &'static str) -> AgentError {
    AgentError::new(
        ErrorCategory::InvalidInput,
        message,
        RetryGuidance::DoNotRetry,
    )
    .expect("static safe outcome message builds")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timeout_error() -> AgentError {
        AgentError::new(
            ErrorCategory::Timeout,
            "tool deadline exceeded",
            RetryGuidance::DoNotRetry,
        )
        .expect("static safe message builds")
    }

    #[test]
    fn denied_outcome_requires_not_started_effects() {
        let denied = ToolOutcome::new(
            ExecutionStatus::Denied,
            EffectState::NotStarted,
            Evidence::HostObserved,
            "confirmation required",
            false,
        )
        .expect("denied with not-started builds");
        assert_eq!(denied.status(), ExecutionStatus::Denied);
        assert_eq!(denied.effect(), EffectState::NotStarted);
        assert!(!denied.is_truncated());
        assert!(
            ToolOutcome::new(
                ExecutionStatus::Denied,
                EffectState::KnownApplied,
                Evidence::HostObserved,
                "x",
                false,
            )
            .is_err(),
            "denial must not claim applied effects"
        );
    }

    #[test]
    fn uncertain_effects_survive_cancellation_and_timeout() {
        let outcome = ToolOutcome::new(
            ExecutionStatus::TimedOut,
            EffectState::Unknown,
            Evidence::Uncertain,
            "deadline hit",
            true,
        )
        .expect("uncertain timeout builds");
        assert_eq!(outcome.effect(), EffectState::Unknown);
        assert_eq!(outcome.evidence(), Evidence::Uncertain);
        assert!(outcome.is_truncated());
    }

    #[test]
    fn missing_usage_stays_unknown_never_zero() {
        let usage = Usage::new(None, None, UsageFinality::Final);
        assert_eq!(usage.input_tokens(), None);
        assert_eq!(usage.output_tokens(), None);
        let finished = TurnFinished::new(FinishReason::Stop, usage, None);
        assert_eq!(
            InvocationOutcome::TurnFinished(finished).finish_reason(),
            Some(FinishReason::Stop)
        );
        assert_eq!(
            InvocationOutcome::Failed(timeout_error()).finish_reason(),
            None
        );
    }

    #[test]
    fn run_finished_reports_save_failure_without_losing_outcome() {
        let finished = RunFinished::new(
            RunOutcome::Completed,
            PersistenceState::SaveFailed,
            Some(timeout_error()),
        )
        .expect("save failure with error builds");
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert!(finished.persistence_error().is_some());
        assert!(
            RunFinished::new(RunOutcome::Completed, PersistenceState::SaveFailed, None).is_err()
        );
        assert!(
            RunFinished::new(
                RunOutcome::Completed,
                PersistenceState::Saved,
                Some(timeout_error())
            )
            .is_err()
        );
    }
}
