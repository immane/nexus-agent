//! Provider port: capabilities, request, context, and normalized events.
//!
//! The adapter owns protocol parsing and connections; the runtime owns
//! whether to invoke, retry, or continue the model loop. The trait is
//! synchronous and data-only; asynchronous adaptation lives in the runtime.

use std::time::Duration;

use crate::content::{CallCandidate, ContinuationData};
use crate::error::{AgentError, ErrorCategory, RetryGuidance};
use crate::ids::{RunId, ToolId, TurnId};
use crate::limits::Limits;
use crate::outcomes::{TurnFinished, Usage};

/// Maximum credential-reference length in bytes (M0-TEST choice). References
/// name a secret; values never cross this boundary.
pub const MAX_CREDENTIAL_REF_LEN: usize = 128;
/// Maximum model-profile name length in bytes (M0-TEST choice).
pub const MAX_PROFILE_LEN: usize = 128;

/// Named credential reference resolved only for the selected integration
/// when needed. Never a secret value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRef(String);

impl CredentialRef {
    /// Validates the reference at the configuration boundary.
    pub fn new(raw: impl Into<String>) -> Result<Self, AgentError> {
        let value = raw.into();
        if value.is_empty() || value.len() > MAX_CREDENTIAL_REF_LEN {
            return Err(port_error("credential reference is invalid"));
        }
        Ok(Self(value))
    }

    /// Returns the reference name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Capabilities of one adapter/profile/model combination. Unknown or
/// estimated limits stay [`None`]; exact counts are never claimed from a
/// generic estimate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCapabilities {
    /// Plain text generation is supported.
    pub text: bool,
    /// Incremental streaming is supported.
    pub streaming: bool,
    /// Structured tool calls are supported (no prompt-based imitation).
    pub tool_calls: bool,
    /// Structured output is supported.
    pub structured_output: bool,
    /// The adapter reports usage counters.
    pub usage_reporting: bool,
    /// Known context-item bound, if the adapter declares one.
    pub max_context_items: Option<u32>,
    /// Known output-byte bound, if the adapter declares one.
    pub max_output_bytes: Option<u32>,
}

/// One model-turn request: identities, profile, accepted conversation
/// budget, enabled tools, and compatible continuation state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRequest {
    run: RunId,
    turn: TurnId,
    profile: String,
    enabled_tools: Vec<ToolId>,
    continuation: Option<ContinuationData>,
    output_budget_bytes: usize,
}

impl ModelRequest {
    /// Builds a request with a finite output budget (never infinity).
    pub fn new(
        run: RunId,
        turn: TurnId,
        profile: impl Into<String>,
        enabled_tools: Vec<ToolId>,
        continuation: Option<ContinuationData>,
        output_budget_bytes: usize,
    ) -> Result<Self, AgentError> {
        let profile = profile.into();
        if profile.is_empty() || profile.len() > MAX_PROFILE_LEN {
            return Err(port_error("model profile is invalid"));
        }
        if output_budget_bytes == 0 || output_budget_bytes > Limits::M0_TEST_TOOL_OUTPUT_BYTES {
            return Err(port_error("model output budget is invalid"));
        }
        if enabled_tools.len() > Limits::M0_TEST_TOOL_CALLS_PER_TURN as usize {
            return Err(port_error("model request enables too many tools"));
        }
        Ok(Self {
            run,
            turn,
            profile,
            enabled_tools,
            continuation,
            output_budget_bytes,
        })
    }

    /// Returns the owning run.
    #[must_use]
    pub fn run(&self) -> &RunId {
        &self.run
    }

    /// Returns the turn identity.
    #[must_use]
    pub fn turn(&self) -> &TurnId {
        &self.turn
    }

    /// Returns the selected model profile name.
    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// Returns the enabled tool identities.
    #[must_use]
    pub fn enabled_tools(&self) -> &[ToolId] {
        &self.enabled_tools
    }

    /// Returns the continuation state, if any.
    #[must_use]
    pub fn continuation(&self) -> Option<&ContinuationData> {
        self.continuation.as_ref()
    }

    /// Returns the finite output budget in bytes.
    #[must_use]
    pub fn output_budget_bytes(&self) -> usize {
        self.output_budget_bytes
    }
}

/// Invocation context: cancellation, monotonic deadline, and the credential
/// reference only. Credentials never become conversation fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderContext {
    deadline_elapsed: Duration,
    cancelled: bool,
    credential: Option<CredentialRef>,
}

impl ProviderContext {
    /// Builds the context. `deadline_elapsed` is monotonic.
    #[must_use]
    pub fn new(
        deadline_elapsed: Duration,
        cancelled: bool,
        credential: Option<CredentialRef>,
    ) -> Self {
        Self {
            deadline_elapsed,
            cancelled,
            credential,
        }
    }

    /// Returns the monotonic deadline.
    #[must_use]
    pub fn deadline_elapsed(&self) -> Duration {
        self.deadline_elapsed
    }

    /// Returns the cancellation flag observed at dispatch.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Returns the credential reference, if any.
    #[must_use]
    pub fn credential(&self) -> Option<&CredentialRef> {
        self.credential.as_ref()
    }

    /// Maps a set cancellation flag to an explicit outcome for dispatch.
    pub fn check_not_cancelled(&self) -> Result<(), AgentError> {
        if self.cancelled {
            return Err(port_error("provider invocation cancelled"));
        }
        Ok(())
    }
}

/// Normalized provider events for one invocation. Candidate progress never
/// authorizes execution; exactly one terminal [`InvocationOutcome`] ends a
/// fully consumed invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderEvent {
    /// Ordered text fragment for a stable turn-local item key.
    TextDelta {
        /// Turn-local item key.
        item_key: String,
        /// Fragment text.
        text: String,
    },
    /// Non-executable argument progress for a proposed call.
    ToolCallDelta {
        /// Turn-local item key.
        item_key: String,
        /// Assembled byte length so far.
        assembled_bytes: usize,
    },
    /// Complete parsed call candidate, still unauthorized.
    ToolCallReady(CallCandidate),
    /// Available usage counters with their finality.
    Usage(Usage),
    /// Terminal success: complete turn, finish reason, final usage.
    TurnFinished(TurnFinished),
    /// Terminal failure for this invocation.
    Failed(AgentError),
}

impl ProviderEvent {
    /// Returns true for the two terminal variants.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::TurnFinished(_) | Self::Failed(_))
    }
}

/// Provider port. Narrow by design: describe capabilities without mandatory
/// network discovery, then perform one model turn.
pub trait ProviderPort {
    /// Describes known capabilities without network discovery.
    fn capabilities(&self) -> ProviderCapabilities;

    /// Performs one model turn and returns normalized events ending in
    /// exactly one terminal event.
    fn stream(&self, request: &ModelRequest, context: &ProviderContext) -> Vec<ProviderEvent>;
}

fn port_error(message: &'static str) -> AgentError {
    AgentError::new(
        ErrorCategory::InvalidInput,
        message,
        RetryGuidance::DoNotRetry,
    )
    .expect("static safe provider message builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::M0_REVISION;
    use crate::outcomes::{Usage, UsageFinality};

    fn request() -> ModelRequest {
        ModelRequest::new(
            RunId::new("run-1").expect("valid"),
            TurnId::new("turn-1").expect("valid"),
            "test-profile",
            vec![ToolId::new("host_read", M0_REVISION).expect("valid")],
            None,
            1024,
        )
        .expect("valid request builds")
    }

    #[test]
    fn request_rejects_missing_or_infinite_budget() {
        assert!(
            ModelRequest::new(
                RunId::new("run-1").expect("valid"),
                TurnId::new("turn-1").expect("valid"),
                "p",
                vec![],
                None,
                0,
            )
            .is_err()
        );
    }

    #[test]
    fn credential_reference_names_but_never_carries_secrets() {
        assert!(CredentialRef::new("payments-prod-key").is_ok());
        assert!(CredentialRef::new("").is_err());
        assert!(CredentialRef::new("x".repeat(MAX_CREDENTIAL_REF_LEN + 1)).is_err());
    }

    #[test]
    fn only_turn_finished_or_failed_are_terminal() {
        let usage = Usage::new(None, None, UsageFinality::Provisional);
        let finished = TurnFinished::new(crate::outcomes::FinishReason::Stop, usage, None);
        let events = [
            ProviderEvent::TextDelta {
                item_key: "item-0".to_owned(),
                text: "hi".to_owned(),
            },
            ProviderEvent::Usage(usage),
            ProviderEvent::TurnFinished(finished),
        ];
        let terminal: Vec<&ProviderEvent> =
            events.iter().filter(|event| event.is_terminal()).collect();
        assert_eq!(terminal.len(), 1);
        let _ = request();
    }

    #[test]
    fn cancellation_maps_to_explicit_outcome() {
        let context = ProviderContext::new(Duration::from_secs(60), true, None);
        assert!(context.is_cancelled());
        assert!(context.check_not_cancelled().is_err());
        let live = ProviderContext::new(Duration::from_secs(60), false, None);
        assert!(live.check_not_cancelled().is_ok());
    }
}
