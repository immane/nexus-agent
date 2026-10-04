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

    /// Builds an outcome from content that may exceed the global M0 output
    /// cap ([`Limits::M0_TEST_TOOL_OUTPUT_BYTES`]): the content is cut on a
    /// UTF-8 character boundary to fit, and `status`, `effect`, and
    /// `evidence` are recorded unchanged. The truncation flag is set when
    /// content was cut; a caller-provided `true` is preserved. The
    /// denied-implies-not-started invariant still applies. For the effective
    /// configured operation budget, apply [`Self::enforce_budget`] to the
    /// result.
    pub fn from_bounded_content(
        status: ExecutionStatus,
        effect: EffectState,
        evidence: Evidence,
        content: impl Into<String>,
        truncated: bool,
    ) -> Result<Self, AgentError> {
        let mut content = content.into();
        let cut = truncate_utf8_bytes(&mut content, Limits::M0_TEST_TOOL_OUTPUT_BYTES);
        Self::new(status, effect, evidence, content, truncated || cut)
    }

    /// Enforces an effective configured operation budget on an already
    /// bounded outcome. `budget` is the policy-selected operation budget,
    /// not the global M0 cap, and must be finite and within
    /// `1..=Limits::M0_TEST_TOOL_OUTPUT_BYTES`; an out-of-range budget is
    /// rejected with a static diagnostic rather than silently widened.
    ///
    /// Content is cut on a UTF-8 character boundary, so a budget too small
    /// for the leading character yields empty content with an explicit
    /// truncation flag, never a split character. `status`, `effect`, and
    /// `evidence` are preserved, and an existing `truncated` flag is never
    /// cleared.
    pub fn enforce_budget(mut self, budget: usize) -> Result<Self, AgentError> {
        if budget == 0 || budget > Limits::M0_TEST_TOOL_OUTPUT_BYTES {
            return Err(outcome_error("tool output budget is invalid"));
        }
        let cut = truncate_utf8_bytes(&mut self.content, budget);
        self.truncated = self.truncated || cut;
        Ok(self)
    }
}

/// Cuts `text` to at most `max_bytes` bytes on a UTF-8 character boundary,
/// returning true when bytes were removed. Characters are never split.
fn truncate_utf8_bytes(text: &mut String, max_bytes: usize) -> bool {
    if text.len() <= max_bytes {
        return false;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    true
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
    /// Builds a finished-turn record for legacy call sites. This constructor
    /// is unchecked: it does not guarantee final usage and is not the host
    /// protocol guarantee. Host/provider boundaries MUST enforce the terminal
    /// invariant with [`Self::validate`] (or build through [`Self::try_new`]),
    /// which rejects provisional counters.
    #[must_use]
    pub fn new(reason: FinishReason, usage: Usage, continuation: Option<ContinuationData>) -> Self {
        Self {
            reason,
            usage,
            continuation,
        }
    }

    /// Builds a finished-turn record and rejects provisional usage: a
    /// terminal turn carries final counters only (provider contract).
    pub fn try_new(
        reason: FinishReason,
        usage: Usage,
        continuation: Option<ContinuationData>,
    ) -> Result<Self, AgentError> {
        let finished = Self::new(reason, usage, continuation);
        finished.validate()?;
        Ok(finished)
    }

    /// Validates terminal invariants: usage must be final because no further
    /// update can follow a fully consumed turn.
    pub fn validate(&self) -> Result<(), AgentError> {
        if self.usage.finality() != UsageFinality::Final {
            return Err(outcome_error("finished turn requires final usage"));
        }
        Ok(())
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
/// without losing already observed tool effects. A typed execution failure
/// can be retained additively beside the lifecycle outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunFinished {
    outcome: RunOutcome,
    persistence: PersistenceState,
    persistence_error: Option<AgentError>,
    error: Option<AgentError>,
}

impl RunFinished {
    /// Builds a terminal record. A persistence error is allowed only with
    /// [`PersistenceState::SaveFailed`], and is required with it. No
    /// execution error is attached; use [`Self::with_error`] for that.
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
            error: None,
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

    /// Attaches the typed execution failure for this terminal record.
    /// Additive builder; [`Self::new`] keeps its signature. Attach an error
    /// for failed, refused, cancelled, or limit-reached outcomes; a record
    /// whose lifecycle completed normally should not carry one.
    #[must_use]
    pub fn with_error(mut self, error: AgentError) -> Self {
        self.error = Some(error);
        self
    }

    /// Returns the retained typed execution failure, if any.
    #[must_use]
    pub fn error(&self) -> Option<&AgentError> {
        self.error.as_ref()
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
    fn bounded_content_cuts_utf8_on_a_char_boundary_and_preserves_fields() {
        let budget = Limits::M0_TEST_TOOL_OUTPUT_BYTES;
        // The 2-byte character starts one byte before the budget; a naive cut
        // at `budget` would split it.
        let mut content = "a".repeat(budget - 1);
        content.push('é');
        content.push_str("tail");
        let outcome = ToolOutcome::from_bounded_content(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            content,
            false,
        )
        .expect("bounded outcome builds");
        assert_eq!(outcome.content().len(), budget - 1);
        assert!(outcome.content().is_char_boundary(outcome.content().len()));
        assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
        assert_eq!(outcome.effect(), EffectState::KnownApplied);
        assert_eq!(outcome.evidence(), Evidence::HostObserved);
        assert!(outcome.is_truncated());

        let exact = "x".repeat(budget);
        let kept = ToolOutcome::from_bounded_content(
            ExecutionStatus::Failed,
            EffectState::Unknown,
            Evidence::Uncertain,
            exact,
            true,
        )
        .expect("exact-budget content builds");
        assert_eq!(kept.content().len(), budget);
        assert!(kept.is_truncated(), "caller truncation flag is preserved");

        assert!(
            ToolOutcome::from_bounded_content(
                ExecutionStatus::Denied,
                EffectState::KnownApplied,
                Evidence::HostObserved,
                "x",
                false,
            )
            .is_err(),
            "denial invariant still applies to bounded construction"
        );
    }

    #[test]
    fn enforce_budget_cuts_utf8_and_preserves_status_effect_and_evidence() {
        let multibyte = ToolOutcome::new(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            "é-tail",
            false,
        )
        .expect("globally bounded content builds");
        // A 1-byte budget cannot hold a 2-byte character: empty content with
        // an explicit truncation flag is a legitimate bounded outcome.
        let cut = multibyte.enforce_budget(1).expect("finite budget accepts");
        assert!(cut.content().is_empty());
        assert!(cut.is_truncated());
        assert_eq!(cut.status(), ExecutionStatus::Succeeded);
        assert_eq!(cut.effect(), EffectState::KnownApplied);
        assert_eq!(cut.evidence(), Evidence::HostObserved);

        let failed = ToolOutcome::new(
            ExecutionStatus::Failed,
            EffectState::KnownNotApplied,
            Evidence::PluginReported,
            "failure detail",
            false,
        )
        .expect("failure outcome builds");
        let cut = failed.enforce_budget(7).expect("finite budget accepts");
        assert_eq!(cut.content(), "failure");
        assert!(cut.is_truncated());
        assert_eq!(cut.status(), ExecutionStatus::Failed);
        assert_eq!(cut.effect(), EffectState::KnownNotApplied);
        assert_eq!(cut.evidence(), Evidence::PluginReported);

        let denied = ToolOutcome::new(
            ExecutionStatus::Denied,
            EffectState::NotStarted,
            Evidence::HostObserved,
            "confirmation required",
            false,
        )
        .expect("denied outcome builds");
        let cut = denied.enforce_budget(4).expect("finite budget accepts");
        assert_eq!(cut.content(), "conf");
        assert!(cut.is_truncated());
        assert_eq!(cut.status(), ExecutionStatus::Denied);
        assert_eq!(cut.effect(), EffectState::NotStarted);
        assert_eq!(cut.evidence(), Evidence::HostObserved);

        let exact = ToolOutcome::new(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            "abc",
            true,
        )
        .expect("exact content builds");
        let kept = exact.enforce_budget(3).expect("fitting budget accepts");
        assert_eq!(kept.content(), "abc");
        assert!(
            kept.is_truncated(),
            "an existing truncation flag is never cleared"
        );

        let build = || {
            ToolOutcome::new(
                ExecutionStatus::Succeeded,
                EffectState::KnownApplied,
                Evidence::HostObserved,
                "x",
                false,
            )
            .expect("outcome builds")
        };
        assert!(
            build().enforce_budget(0).is_err(),
            "zero budget is rejected"
        );
        assert!(
            build()
                .enforce_budget(Limits::M0_TEST_TOOL_OUTPUT_BYTES + 1)
                .is_err(),
            "a budget above the global cap is rejected"
        );
        assert!(
            build()
                .enforce_budget(Limits::M0_TEST_TOOL_OUTPUT_BYTES)
                .is_ok()
        );
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
    fn turn_finished_rejects_provisional_terminal_usage() {
        let provisional = Usage::new(Some(10), Some(5), UsageFinality::Provisional);
        assert!(
            TurnFinished::try_new(FinishReason::Stop, provisional, None).is_err(),
            "a consumed turn cannot carry provisional usage"
        );
        let compatibility = TurnFinished::new(FinishReason::Stop, provisional, None);
        assert!(
            compatibility.validate().is_err(),
            "the compatibility constructor still exposes a detectable violation"
        );

        let committed = Usage::new(Some(10), Some(8), UsageFinality::Final);
        let finished =
            TurnFinished::try_new(FinishReason::Stop, committed, None).expect("final usage builds");
        assert_eq!(finished.usage(), committed);
        assert!(finished.validate().is_ok());
    }

    #[test]
    fn run_finished_retains_typed_execution_error() {
        let typed = timeout_error();
        let finished = RunFinished::new(RunOutcome::Failed, PersistenceState::Ephemeral, None)
            .expect("failed record builds")
            .with_error(typed.clone());
        assert_eq!(finished.outcome(), RunOutcome::Failed);
        assert_eq!(finished.error(), Some(&typed));
        assert!(finished.persistence_error().is_none());
        assert_eq!(finished.clone(), finished, "typed error survives a clone");
        assert!(
            RunFinished::new(RunOutcome::Completed, PersistenceState::Ephemeral, None)
                .expect("completed record builds")
                .error()
                .is_none(),
            "no execution error is fabricated"
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
