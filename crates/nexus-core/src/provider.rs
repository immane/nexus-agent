//! Provider port: capabilities, request, context, and normalized events.
//!
//! The adapter owns protocol parsing and connections; the runtime owns
//! whether to invoke, retry, or continue the model loop. The trait is
//! synchronous and data-only; asynchronous adaptation lives in the runtime.

use std::time::{Duration, Instant};

use crate::content::{
    CallCandidate, ContinuationData, MAX_ITEM_KEY_LEN, MAX_PROVIDER_REF_LEN, TextContent, ToolCall,
};
use crate::error::{AgentError, ErrorCategory, RetryGuidance};
use crate::execution::CancellationToken;
use crate::ids::{CallId, RunId, ToolId, TurnId};
use crate::limits::Limits;
use crate::outcomes::{ToolOutcome, TurnFinished, Usage};
use crate::tool::ToolSpec;

/// Maximum credential-reference length in bytes (M0-TEST choice). References
/// name a secret; values never cross this boundary.
pub const MAX_CREDENTIAL_REF_LEN: usize = 128;
/// Maximum model-profile name length in bytes (M0-TEST choice).
pub const MAX_PROFILE_LEN: usize = 128;
/// Maximum conversation items accepted by one request (M0-TEST choice: the
/// lock's retained-context table value).
pub const MAX_CONVERSATION_ITEMS: usize = Limits::M0_TEST_RETAINED_CONTEXT_ITEMS;
/// Maximum aggregate conversation payload in bytes (M0-TEST representation
/// choice; the lock sets no table value for aggregate conversation bytes).
pub const MAX_CONVERSATION_BYTES: usize = 1_048_576;
/// Maximum tool definitions accepted by one request (M0-TEST choice: the
/// per-turn declared-call bound).
pub const MAX_TOOL_DEFINITIONS: usize = Limits::M0_TEST_TOOL_CALLS_PER_TURN as usize;
/// Maximum aggregate tool-definition payload in bytes: tool identity name
/// plus description plus schema text (M0-TEST representation choice).
pub const MAX_TOOL_DEFINITION_BYTES: usize = 1_048_576;

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

/// Bounded provider turn-local item key.
///
/// Keys are provider-facing round-trip identities, never host identities and
/// never filesystem paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemKey(String);

impl ItemKey {
    /// Validates a non-empty key within [`MAX_ITEM_KEY_LEN`].
    pub fn new(raw: impl Into<String>) -> Result<Self, AgentError> {
        let value = raw.into();
        if value.is_empty() || value.len() > MAX_ITEM_KEY_LEN {
            return Err(port_error("provider item key is invalid"));
        }
        Ok(Self(value))
    }

    /// Returns the key text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Bounded opaque provider call reference, preserved for adapter round-trips.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRef(String);

impl ProviderRef {
    /// Validates a non-empty reference within [`MAX_PROVIDER_REF_LEN`].
    pub fn new(raw: impl Into<String>) -> Result<Self, AgentError> {
        let value = raw.into();
        if value.is_empty() || value.len() > MAX_PROVIDER_REF_LEN {
            return Err(port_error("provider call reference is invalid"));
        }
        Ok(Self(value))
    }

    /// Returns the reference text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One bounded conversation item supplied to a model turn.
///
/// Items correlate provider round-trip identities (item key, provider
/// reference) with host identities ([`CallId`], [`ToolId`]) but never
/// authorize anything: a recorded result states what happened and cannot
/// dispatch, retry, or re-approve. Payload bounds are enforced per item by
/// the validated payload types and per request by
/// [`ModelRequest::with_conversation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelContextItem {
    /// User text.
    UserText(TextContent),
    /// Assistant text for one provider turn-local item key.
    AssistantText {
        /// Turn-local item key.
        item_key: ItemKey,
        /// Assistant text.
        text: TextContent,
        /// Thinking trace to echo back on later turns. Vendors running a
        /// thinking mode (DeepSeek `reasoning_content`) reject follow-up
        /// requests that drop it; it is never presented as an answer and
        /// never authorizes anything.
        reasoning: Option<String>,
    },
    /// Assistant call admitted by the host, correlated to its provider item.
    AssistantCall {
        /// Turn-local item key.
        item_key: ItemKey,
        /// Original provider reference.
        provider_ref: ProviderRef,
        /// Admitted host call carrying run, turn, call, tool, and arguments.
        call: ToolCall,
        /// Thinking trace to echo back, as above.
        reasoning: Option<String>,
    },
    /// Rejected proposal, retained verbatim for provider correlation only.
    /// Unlike `AssistantCall`, it carries no executable, validated call.
    AssistantDeniedCall {
        /// Host identity of the denial.
        call: CallId,
        /// Original bounded proposal, including unvalidated argument text.
        candidate: CallCandidate,
        /// Thinking trace for the assistant turn.
        reasoning: Option<String>,
    },
    /// A completed thinking-only turn, with no fabricated visible answer.
    AssistantReasoning {
        /// Bounded trace, never presented as answer text.
        reasoning: String,
    },
    /// Recorded tool outcome correlated to its host call and provider item.
    ToolResult {
        /// Host call identity.
        call: CallId,
        /// Turn-local item key.
        item_key: ItemKey,
        /// Original provider reference.
        provider_ref: ProviderRef,
        /// Resolved tool identity plus revision.
        tool: ToolId,
        /// Actual recorded outcome.
        outcome: ToolOutcome,
    },
}

impl ModelContextItem {
    /// Builds a reasoning-only assistant record without inventing content.
    pub fn assistant_reasoning(reasoning: String) -> Result<Self, AgentError> {
        validate_reasoning(Some(&reasoning))?;
        Ok(Self::AssistantReasoning { reasoning })
    }

    /// Records a rejected proposal; this type cannot reach a tool executor.
    pub fn assistant_denied_call(
        call: CallId,
        candidate: CallCandidate,
        reasoning: Option<String>,
    ) -> Result<Self, AgentError> {
        validate_reasoning(reasoning.as_deref())?;
        Ok(Self::AssistantDeniedCall {
            call,
            candidate,
            reasoning: reasoning.filter(|trace| !trace.is_empty()),
        })
    }
    /// Builds bounded user text.
    pub fn user_text(text: impl Into<String>) -> Result<Self, AgentError> {
        Ok(Self::UserText(TextContent::new(text)?))
    }

    /// Builds bounded assistant text for one item key.
    pub fn assistant_text(
        item_key: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<Self, AgentError> {
        Self::assistant_text_with_reasoning(item_key, text, None)
    }

    /// Builds bounded assistant text plus an optional thinking trace to
    /// echo on later turns. An empty trace normalizes to absent.
    pub fn assistant_text_with_reasoning(
        item_key: impl Into<String>,
        text: impl Into<String>,
        reasoning: Option<String>,
    ) -> Result<Self, AgentError> {
        validate_reasoning(reasoning.as_deref())?;
        Ok(Self::AssistantText {
            item_key: ItemKey::new(item_key)?,
            text: TextContent::new(text)?,
            reasoning: reasoning.filter(|trace| !trace.is_empty()),
        })
    }

    /// Builds an assistant call correlated to its provider item key and
    /// reference.
    pub fn assistant_call(
        item_key: impl Into<String>,
        provider_ref: impl Into<String>,
        call: ToolCall,
    ) -> Result<Self, AgentError> {
        Self::assistant_call_with_reasoning(item_key, provider_ref, call, None)
    }

    /// Builds an assistant call plus an optional thinking trace to echo on
    /// later turns. An empty trace normalizes to absent.
    pub fn assistant_call_with_reasoning(
        item_key: impl Into<String>,
        provider_ref: impl Into<String>,
        call: ToolCall,
        reasoning: Option<String>,
    ) -> Result<Self, AgentError> {
        validate_reasoning(reasoning.as_deref())?;
        Ok(Self::AssistantCall {
            item_key: ItemKey::new(item_key)?,
            provider_ref: ProviderRef::new(provider_ref)?,
            call,
            reasoning: reasoning.filter(|trace| !trace.is_empty()),
        })
    }

    /// Returns the thinking trace to echo on later turns, if any.
    #[must_use]
    pub fn reasoning(&self) -> Option<&str> {
        match self {
            Self::AssistantReasoning { reasoning } => Some(reasoning),
            Self::AssistantText { reasoning, .. }
            | Self::AssistantCall { reasoning, .. }
            | Self::AssistantDeniedCall { reasoning, .. } => reasoning.as_deref(),
            Self::UserText(_) | Self::ToolResult { .. } => None,
        }
    }

    /// Builds a recorded result correlated to its host call and provider
    /// item. The outcome is carried as observed; it never authorizes a
    /// retry or a new dispatch.
    pub fn tool_result(
        call: CallId,
        item_key: impl Into<String>,
        provider_ref: impl Into<String>,
        tool: ToolId,
        outcome: ToolOutcome,
    ) -> Result<Self, AgentError> {
        Ok(Self::ToolResult {
            call,
            item_key: ItemKey::new(item_key)?,
            provider_ref: ProviderRef::new(provider_ref)?,
            tool,
            outcome,
        })
    }

    /// Returns the owned UTF-8 string payload bytes counted against the
    /// request bound: item keys, provider references, host identities
    /// (run/turn/call and tool name), arguments, text, reasoning traces, and
    /// outcome content. This is not total allocated overhead: per-item
    /// object, `Vec`, and revision-field overhead is finite and covered by
    /// the request's count bound.
    #[must_use]
    pub fn payload_bytes(&self) -> usize {
        match self {
            Self::AssistantReasoning { reasoning } => reasoning.len(),
            Self::AssistantDeniedCall {
                call,
                candidate,
                reasoning,
            } => {
                call.as_str().len()
                    + candidate.item_key().len()
                    + candidate.provider_ref().len()
                    + candidate.tool_name().len()
                    + candidate.arguments_json().len()
                    + reasoning.as_deref().map_or(0, str::len)
            }
            Self::UserText(text) => text.as_str().len(),
            Self::AssistantText {
                item_key,
                text,
                reasoning,
            } => {
                item_key.as_str().len()
                    + text.as_str().len()
                    + reasoning.as_deref().map_or(0, str::len)
            }
            Self::AssistantCall {
                item_key,
                provider_ref,
                call,
                reasoning,
            } => {
                item_key.as_str().len()
                    + provider_ref.as_str().len()
                    + call.run().as_str().len()
                    + call.turn().as_str().len()
                    + call.call().as_str().len()
                    + call.tool().name().len()
                    + call.args().as_str().len()
                    + reasoning.as_deref().map_or(0, str::len)
            }
            Self::ToolResult {
                call,
                item_key,
                provider_ref,
                tool,
                outcome,
            } => {
                call.as_str().len()
                    + item_key.as_str().len()
                    + provider_ref.as_str().len()
                    + tool.name().len()
                    + outcome.content().len()
            }
        }
    }
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
    conversation: Vec<ModelContextItem>,
    tool_definitions: Vec<ToolSpec>,
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
            conversation: Vec::new(),
            tool_definitions: Vec::new(),
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

    /// Installs a bounded conversation, replacing any previously accepted
    /// items. Count and aggregate payload bytes are checked here.
    pub fn with_conversation(
        mut self,
        conversation: Vec<ModelContextItem>,
    ) -> Result<Self, AgentError> {
        if conversation.len() > MAX_CONVERSATION_ITEMS {
            return Err(port_limit_error(
                "model conversation exceeds retained context budget",
            ));
        }
        for item in &conversation {
            validate_reasoning(item.reasoning())?;
        }
        let total = conversation
            .iter()
            .fold(0usize, |acc, item| acc.saturating_add(item.payload_bytes()));
        if total > MAX_CONVERSATION_BYTES {
            return Err(port_limit_error(
                "model conversation exceeds aggregate byte budget",
            ));
        }
        self.conversation = conversation;
        Ok(self)
    }

    /// Returns the accepted conversation items.
    #[must_use]
    pub fn conversation(&self) -> &[ModelContextItem] {
        &self.conversation
    }

    /// Installs bounded tool definitions, replacing any previously accepted
    /// definitions. Count and aggregate identity-plus-description-plus-schema
    /// bytes are checked here.
    pub fn with_tool_definitions(
        mut self,
        tool_definitions: Vec<ToolSpec>,
    ) -> Result<Self, AgentError> {
        if tool_definitions.len() > MAX_TOOL_DEFINITIONS {
            return Err(port_limit_error(
                "model request carries too many tool definitions",
            ));
        }
        let total = tool_definitions_bytes(&tool_definitions);
        if total > MAX_TOOL_DEFINITION_BYTES {
            return Err(port_limit_error(
                "model tool definitions exceed aggregate byte budget",
            ));
        }
        self.tool_definitions = tool_definitions;
        Ok(self)
    }

    /// Returns the accepted tool definitions.
    #[must_use]
    pub fn tool_definitions(&self) -> &[ToolSpec] {
        &self.tool_definitions
    }
}

/// Invocation context: live cancellation, a monotonic deadline, and the
/// credential reference only. Credentials never become conversation fields.
///
/// [`ProviderContext::new`] keeps the legacy fixture shape: a `Duration`
/// elapsed reading and a `bool` cancellation snapshot, neither cooperative.
/// [`ProviderContext::with_control`] additively installs a live
/// [`CancellationToken`] and an evaluable [`Instant`] deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderContext {
    deadline_elapsed: Duration,
    cancelled: bool,
    credential: Option<CredentialRef>,
    token: Option<CancellationToken>,
    deadline_at: Option<Instant>,
}

impl ProviderContext {
    /// Builds the legacy context. `deadline_elapsed` is a monotonic reading
    /// and `cancelled` is a snapshot observed at dispatch.
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
            token: None,
            deadline_at: None,
        }
    }

    /// Additively installs live cancellation and monotonic deadline control,
    /// replacing any previously installed control. The legacy snapshot
    /// fields stay untouched for fixtures.
    #[must_use]
    pub fn with_control(mut self, token: CancellationToken, deadline: Instant) -> Self {
        self.token = Some(token);
        self.deadline_at = Some(deadline);
        self
    }

    /// Returns the monotonic elapsed reading from the legacy constructor.
    #[must_use]
    pub fn deadline_elapsed(&self) -> Duration {
        self.deadline_elapsed
    }

    /// Returns the live monotonic deadline, or [`None`] for legacy
    /// constructor snapshots: an elapsed `Duration` carries no evaluable
    /// instant without the run's start reading.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline_at
    }

    /// Returns cancellation observed live from the token, falling back to
    /// the dispatch-time snapshot for legacy fixtures. A `bool` snapshot is
    /// never cooperative.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.token
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
            || self.cancelled
    }

    /// Returns the credential reference, if any.
    #[must_use]
    pub fn credential(&self) -> Option<&CredentialRef> {
        self.credential.as_ref()
    }

    /// Explicit combined dispatch check: cancellation first, then the live
    /// deadline. Cancellation maps to [`ErrorCategory::Cancelled`], an
    /// elapsed deadline to [`ErrorCategory::Timeout`].
    pub fn check_active(&self) -> Result<(), AgentError> {
        if self.is_cancelled() {
            return Err(port_state_error(
                ErrorCategory::Cancelled,
                "provider invocation cancelled",
            ));
        }
        if self
            .deadline_at
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(port_state_error(
                ErrorCategory::Timeout,
                "provider invocation deadline exceeded",
            ));
        }
        Ok(())
    }

    /// Maps cancellation to an explicit outcome for dispatch, observing the
    /// live token when control was installed.
    pub fn check_not_cancelled(&self) -> Result<(), AgentError> {
        if self.is_cancelled() {
            return Err(port_state_error(
                ErrorCategory::Cancelled,
                "provider invocation cancelled",
            ));
        }
        Ok(())
    }
}

/// Normalized provider events for one invocation. Candidate progress never
/// authorizes execution; exactly one terminal [`crate::InvocationOutcome`] ends a
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
    /// Thinking-trace fragment for the current turn. Withheld from
    /// provisional sinks like tool previews: it is recorded for echo on
    /// later turns (vendors running a thinking mode reject follow-ups that
    /// drop it), never presented as an answer and never dispatched.
    ReasoningDelta {
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

/// Reserved adapter identity returned by [`ProviderPort::adapter_identity`]
/// when an implementor does not override it. Never a secret and never a
/// compatibility claim.
pub const DEFAULT_ADAPTER_IDENTITY: &str = "unknown-adapter";

/// Provider port. Narrow by design: describe capabilities without mandatory
/// network discovery, then perform one model turn. The two identity methods
/// have stable defaults so existing implementors keep compiling; adapters
/// that emit or consume continuation state override them to match their
/// continuation labels.
pub trait ProviderPort {
    /// Describes known capabilities without network discovery.
    fn capabilities(&self) -> ProviderCapabilities;

    /// Performs one model turn and returns normalized events ending in
    /// exactly one terminal event.
    fn stream(&self, request: &ModelRequest, context: &ProviderContext) -> Vec<ProviderEvent>;

    /// Performs one model turn while offering provisional events to `sink`
    /// as they arrive. Only presentation-safe prefixes travel the sink:
    /// `TextDelta`, `ToolCallDelta`, and `Usage`. Candidates
    /// (`ToolCallReady`) and terminal events are withheld from the sink and
    /// arrive only in the returned batch, so nothing streamed can dispatch
    /// or conclude a turn before the full batch validates. Callers must
    /// treat sinked events as provisional presentation: the returned batch
    /// stays authoritative for validation, dispatch, and records.
    ///
    /// The default replays the completed batch prefix through the sink, so
    /// implementors that cannot stream incrementally keep compiling and
    /// behave exactly like [`ProviderPort::stream`]. Runtimes consult
    /// [`ProviderPort::supports_incremental_streaming`] before relying on
    /// any fast-path timing.
    fn stream_with_sink(
        &self,
        request: &ModelRequest,
        context: &ProviderContext,
        sink: &(dyn Fn(ProviderEvent) + Send + Sync),
    ) -> Vec<ProviderEvent> {
        let events = self.stream(request, context);
        for event in events.iter().filter(|event| {
            matches!(
                event,
                ProviderEvent::TextDelta { .. }
                    | ProviderEvent::ToolCallDelta { .. }
                    | ProviderEvent::Usage(_)
            )
        }) {
            sink(event.clone());
        }
        events
    }

    /// True when [`ProviderPort::stream_with_sink`] delivers provisional
    /// events genuinely ahead of turn completion (wire-level streaming).
    /// Defaults to false: the default [`ProviderPort::stream_with_sink`]
    /// replays after completion, which is presentation-equivalent but never
    /// early. Adapters that push live deltas override this to true.
    fn supports_incremental_streaming(&self) -> bool {
        false
    }

    /// Returns the stable adapter identity used for continuation
    /// compatibility matching. The default is the reserved
    /// [`DEFAULT_ADAPTER_IDENTITY`] placeholder; implementors that emit
    /// continuation state override it with a stable, non-secret label and
    /// keep it aligned with their [`ContinuationData::adapter`] label.
    fn adapter_identity(&self) -> &str {
        DEFAULT_ADAPTER_IDENTITY
    }

    /// Returns the stable continuation scope for `profile`. The default is
    /// the profile name exactly; override when compatibility depends on more
    /// than the profile label. The result is compared with
    /// [`ContinuationData::scope`] using exact equality.
    fn continuation_scope(&self, profile: &str) -> String {
        profile.to_owned()
    }
}

fn port_error(message: &'static str) -> AgentError {
    AgentError::new(
        ErrorCategory::InvalidInput,
        message,
        RetryGuidance::DoNotRetry,
    )
    .expect("static safe provider message builds")
}

fn validate_reasoning(reasoning: Option<&str>) -> Result<(), AgentError> {
    if reasoning.is_some_and(|trace| trace.len() > Limits::M0_TEST_TOOL_OUTPUT_BYTES) {
        return Err(port_limit_error(
            "assistant reasoning exceeds its byte budget",
        ));
    }
    Ok(())
}

fn port_limit_error(message: &'static str) -> AgentError {
    AgentError::new(
        ErrorCategory::ResourceLimit,
        message,
        RetryGuidance::DoNotRetry,
    )
    .expect("static safe provider message builds")
}

fn port_state_error(category: ErrorCategory, message: &'static str) -> AgentError {
    AgentError::new(category, message, RetryGuidance::DoNotRetry)
        .expect("static safe provider message builds")
}

/// Owned string bytes of one tool-definition set: identity name, description,
/// and schema text. Object and `Vec` overhead is covered by the count bound.
fn tool_definitions_bytes(tool_definitions: &[ToolSpec]) -> usize {
    tool_definitions.iter().fold(0usize, |acc, spec| {
        acc.saturating_add(spec.id().name().len())
            .saturating_add(spec.description().len())
            .saturating_add(spec.input_schema_json().len())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::NormalizedArgs;
    use crate::ids::M0_REVISION;
    use crate::outcomes::{EffectState, Evidence, ExecutionStatus, Usage, UsageFinality};

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
    fn reasoning_is_bounded_per_item_and_in_aggregate_even_for_direct_variants() {
        let exact = "x".repeat(Limits::M0_TEST_TOOL_OUTPUT_BYTES);
        assert!(
            ModelContextItem::assistant_text_with_reasoning("k", "text", Some(exact.clone()))
                .is_ok()
        );
        assert!(
            ModelContextItem::assistant_call_with_reasoning(
                "k",
                "ref",
                test_tool_call(),
                Some(format!("{exact}x"))
            )
            .is_err()
        );
        let invalid = ModelContextItem::AssistantReasoning {
            reasoning: format!("{exact}x"),
        };
        assert_eq!(
            request()
                .with_conversation(vec![invalid])
                .unwrap_err()
                .category(),
            ErrorCategory::ResourceLimit
        );
        let item = ModelContextItem::assistant_reasoning(exact).unwrap();
        assert_eq!(item.payload_bytes(), Limits::M0_TEST_TOOL_OUTPUT_BYTES);
        assert!(
            request()
                .clone()
                .with_conversation(vec![item.clone(); 4])
                .is_ok()
        );
        assert_eq!(
            request()
                .with_conversation(vec![item; 5])
                .unwrap_err()
                .category(),
            ErrorCategory::ResourceLimit
        );
    }

    fn test_tool_call() -> ToolCall {
        ToolCall::new(
            RunId::new("run-1").expect("valid"),
            TurnId::new("turn-1").expect("valid"),
            CallId::new("call-1").expect("valid"),
            ToolId::new("host_read", M0_REVISION).expect("valid"),
            NormalizedArgs::new(r#"{"path":"src"}"#).expect("valid args"),
        )
    }

    fn test_outcome() -> ToolOutcome {
        ToolOutcome::new(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            "listed src",
            false,
        )
        .expect("valid outcome builds")
    }

    /// Result item with `content_len` bytes of content and fixed identities:
    /// `call-1` (6) + `item-1` (6) + `prov-ref-1` (10) + `host_read` (9).
    fn result_item(content_len: usize) -> ModelContextItem {
        ModelContextItem::tool_result(
            CallId::new("call-1").expect("valid"),
            "item-1",
            "prov-ref-1",
            ToolId::new("host_read", M0_REVISION).expect("valid"),
            ToolOutcome::new(
                ExecutionStatus::Succeeded,
                EffectState::KnownApplied,
                Evidence::HostObserved,
                "x".repeat(content_len),
                false,
            )
            .expect("bounded outcome content"),
        )
        .expect("bounded result item")
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
    fn legacy_snapshot_cancellation_still_maps_to_explicit_outcome() {
        let context = ProviderContext::new(Duration::from_secs(60), true, None);
        assert!(context.is_cancelled());
        assert!(context.check_not_cancelled().is_err());
        assert_eq!(
            context
                .check_active()
                .expect_err("snapshot cancels")
                .category(),
            ErrorCategory::Cancelled
        );
        let live = ProviderContext::new(Duration::from_secs(60), false, None);
        assert!(live.check_not_cancelled().is_ok());
    }

    #[test]
    fn live_control_observes_cancellation_and_evaluable_deadline() {
        let token = CancellationToken::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        let context = ProviderContext::new(Duration::from_secs(300), false, None)
            .with_control(token.clone(), deadline);
        assert_eq!(context.deadline(), Some(deadline));
        assert!(!context.is_cancelled());
        context.check_active().expect("live control is active");

        token.cancel();
        assert!(
            context.is_cancelled(),
            "token is re-read after construction"
        );
        assert_eq!(
            context.check_active().expect_err("live cancel").category(),
            ErrorCategory::Cancelled
        );

        let past = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .expect("test clock has history");
        let expired = ProviderContext::new(Duration::from_secs(300), false, None)
            .with_control(CancellationToken::new(), past);
        assert!(!expired.is_cancelled());
        assert_eq!(
            expired
                .check_active()
                .expect_err("deadline passed")
                .category(),
            ErrorCategory::Timeout
        );
    }

    #[test]
    fn context_item_bounds_reject_empty_and_oversize_refs() {
        assert!(ItemKey::new("").is_err());
        assert!(ItemKey::new("k".repeat(MAX_ITEM_KEY_LEN + 1)).is_err());
        assert!(ProviderRef::new("").is_err());
        assert!(ProviderRef::new("r".repeat(MAX_PROVIDER_REF_LEN + 1)).is_err());
        assert!(ModelContextItem::user_text("").is_err());
        assert!(ModelContextItem::assistant_text("item-0", "").is_err());
        assert!(ModelContextItem::assistant_call("item-0", "prov-ref-0", test_tool_call()).is_ok());
    }

    #[test]
    fn conversation_bounds_count_and_aggregate_bytes() {
        let base = request();
        let accepted = base
            .clone()
            .with_conversation(vec![
                ModelContextItem::user_text("hello").expect("valid text"),
            ])
            .expect("bounded conversation builds");
        assert_eq!(accepted.conversation().len(), 1);

        let too_many: Vec<ModelContextItem> = (0..=MAX_CONVERSATION_ITEMS)
            .map(|_| ModelContextItem::user_text("x").expect("valid text"))
            .collect();
        assert_eq!(
            base.clone()
                .with_conversation(too_many)
                .expect_err("count bound rejects")
                .category(),
            ErrorCategory::ResourceLimit
        );

        let oversize: Vec<ModelContextItem> = (0..5)
            .map(|_| {
                ModelContextItem::user_text("x".repeat(Limits::M0_TEST_TOOL_OUTPUT_BYTES))
                    .expect("bounded text")
            })
            .collect();
        assert!(
            oversize
                .iter()
                .map(ModelContextItem::payload_bytes)
                .sum::<usize>()
                > MAX_CONVERSATION_BYTES
        );
        assert_eq!(
            base.clone()
                .with_conversation(oversize)
                .expect_err("aggregate bound rejects")
                .category(),
            ErrorCategory::ResourceLimit
        );
    }

    #[test]
    fn conversation_preserves_call_and_result_correlations() {
        let call = test_tool_call();
        let assistant = ModelContextItem::assistant_call("item-1", "prov-ref-1", call.clone())
            .expect("valid assistant call");
        let result = ModelContextItem::tool_result(
            call.call().clone(),
            "item-1",
            "prov-ref-1",
            call.tool().clone(),
            test_outcome(),
        )
        .expect("valid result");
        let request = request()
            .with_conversation(vec![assistant, result])
            .expect("bounded conversation builds");
        assert_eq!(request.conversation().len(), 2);

        match &request.conversation()[0] {
            ModelContextItem::AssistantCall {
                item_key,
                provider_ref,
                call: carried,
                reasoning,
            } => {
                assert_eq!(item_key.as_str(), "item-1");
                assert_eq!(provider_ref.as_str(), "prov-ref-1");
                assert_eq!(carried.call(), call.call());
                assert_eq!(carried.tool(), call.tool());
                assert_eq!(reasoning, &None);
            }
            other => panic!("unexpected conversation item {other:?}"),
        }
        match &request.conversation()[1] {
            ModelContextItem::ToolResult {
                call: result_call,
                item_key,
                provider_ref,
                tool,
                outcome,
            } => {
                assert_eq!(result_call, call.call(), "host call identity preserved");
                assert_eq!(item_key.as_str(), "item-1");
                assert_eq!(provider_ref.as_str(), "prov-ref-1");
                assert_eq!(tool, call.tool(), "tool identity plus revision preserved");
                assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
            }
            other => panic!("unexpected conversation item {other:?}"),
        }
    }

    #[test]
    fn payload_bytes_counts_all_owned_identities() {
        let call = test_tool_call();
        let assistant = ModelContextItem::assistant_call("item-1", "prov-ref-1", call.clone())
            .expect("valid assistant call");
        assert_eq!(
            assistant.payload_bytes(),
            "item-1".len()
                + "prov-ref-1".len()
                + "run-1".len()
                + "turn-1".len()
                + "call-1".len()
                + "host_read".len()
                + r#"{"path":"src"}"#.len(),
            "assistant call counts item key, provider ref, run, turn, call, tool name, args"
        );
        assert_eq!(
            result_item("listed src".len()).payload_bytes(),
            "call-1".len()
                + "item-1".len()
                + "prov-ref-1".len()
                + "host_read".len()
                + "listed src".len(),
            "result counts host call, item key, provider ref, tool name, content"
        );
    }

    #[test]
    fn conversation_aggregate_cap_includes_owned_metadata() {
        // Owned metadata per result item beyond content: call-1 (6) +
        // item-1 (6) + prov-ref-1 (10) + host_read (9) = 31 bytes.
        const METADATA_BYTES: usize = 31;
        let exact: Vec<ModelContextItem> = (0..4)
            .map(|_| result_item((MAX_CONVERSATION_BYTES / 4) - METADATA_BYTES))
            .collect();
        assert_eq!(
            exact
                .iter()
                .map(ModelContextItem::payload_bytes)
                .sum::<usize>(),
            MAX_CONVERSATION_BYTES,
            "aggregate includes owned metadata and lands exactly on the cap"
        );
        request()
            .with_conversation(exact)
            .expect("aggregate at the cap is accepted");

        // A set whose content plus key/ref bytes were once under the cap:
        // the host call and tool-name metadata omitted from the old count
        // pushes it over, so it must now be rejected.
        let undercounted: Vec<ModelContextItem> = (0..4)
            .map(|_| result_item((MAX_CONVERSATION_BYTES / 4) - 24))
            .collect();
        assert!(
            undercounted
                .iter()
                .map(ModelContextItem::payload_bytes)
                .sum::<usize>()
                > MAX_CONVERSATION_BYTES
        );
        assert_eq!(
            request()
                .with_conversation(undercounted)
                .expect_err("metadata-inclusive aggregate rejects")
                .category(),
            ErrorCategory::ResourceLimit
        );
    }

    #[test]
    fn tool_definition_bytes_include_tool_id_name() {
        assert_eq!(tool_definitions_bytes(&[]), 0);
        let spec = ToolSpec::new(
            ToolId::new("host_read", M0_REVISION).expect("valid"),
            "read files",
            r#"{"type":"object"}"#,
        )
        .expect("valid spec");
        assert_eq!(
            tool_definitions_bytes(std::slice::from_ref(&spec)),
            "host_read".len() + "read files".len() + r#"{"type":"object"}"#.len(),
            "identity name is part of the aggregate"
        );
        assert_eq!(tool_definitions_bytes(std::slice::from_ref(&spec)), 36);
    }

    #[test]
    fn tool_definitions_are_bounded_and_exposed() {
        let spec = ToolSpec::new(
            ToolId::new("host_read", M0_REVISION).expect("valid"),
            "read files",
            r#"{"type":"object"}"#,
        )
        .expect("valid spec");
        let base = request()
            .with_tool_definitions(vec![spec.clone()])
            .expect("bounded definitions build");
        assert_eq!(base.tool_definitions(), std::slice::from_ref(&spec));

        let too_many: Vec<ToolSpec> = (0..=MAX_TOOL_DEFINITIONS)
            .map(|index| {
                ToolSpec::new(
                    ToolId::new(format!("tool_{index}"), M0_REVISION).expect("valid"),
                    "description",
                    r#"{"type":"object"}"#,
                )
                .expect("valid spec")
            })
            .collect();
        assert_eq!(
            request()
                .with_tool_definitions(too_many)
                .expect_err("count bound rejects")
                .category(),
            ErrorCategory::ResourceLimit
        );
    }

    struct MinimalProvider;

    impl ProviderPort for MinimalProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                text: true,
                streaming: false,
                tool_calls: false,
                structured_output: false,
                usage_reporting: false,
                max_context_items: None,
                max_output_bytes: None,
            }
        }

        fn stream(
            &self,
            _request: &ModelRequest,
            _context: &ProviderContext,
        ) -> Vec<ProviderEvent> {
            Vec::new()
        }
    }

    #[test]
    fn provider_defaults_are_stable_for_compatibility_matching() {
        let provider = MinimalProvider;
        assert_eq!(
            provider.adapter_identity(),
            DEFAULT_ADAPTER_IDENTITY,
            "reserved default is stable"
        );
        assert_eq!(provider.continuation_scope("profile-a"), "profile-a");
        assert_ne!(
            provider.continuation_scope("profile-a"),
            provider.continuation_scope("profile-b"),
            "distinct profiles never share a default scope"
        );
        assert!(
            !provider.supports_incremental_streaming(),
            "the default sink replays after completion, never early"
        );
    }

    #[test]
    fn default_sink_replays_the_completed_prefix_without_the_terminal() {
        use std::sync::{Arc, Mutex};
        struct FixedProvider;
        impl ProviderPort for FixedProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                MinimalProvider.capabilities()
            }

            fn stream(
                &self,
                _request: &ModelRequest,
                _context: &ProviderContext,
            ) -> Vec<ProviderEvent> {
                vec![
                    ProviderEvent::ReasoningDelta {
                        text: "hidden trace".to_owned(),
                    },
                    ProviderEvent::ToolCallReady(
                        CallCandidate::new("item-1", "ref-1", "host_read", "{}").unwrap(),
                    ),
                    ProviderEvent::TextDelta {
                        item_key: "item-0".to_owned(),
                        text: "hi".to_owned(),
                    },
                    ProviderEvent::TurnFinished(TurnFinished::new(
                        crate::outcomes::FinishReason::Stop,
                        Usage::new(None, None, UsageFinality::Final),
                        None,
                    )),
                ]
            }
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let pushed = Arc::clone(&seen);
        let sink = move |event: ProviderEvent| {
            pushed.lock().expect("sink log writable").push(event);
        };
        let batch = FixedProvider.stream_with_sink(
            &request(),
            &ProviderContext::new(Duration::from_secs(60), false, None),
            &sink,
        );
        assert_eq!(batch.len(), 4, "the returned batch stays authoritative");
        let seen = seen.lock().expect("sink log readable");
        assert_eq!(
            seen.len(),
            1,
            "only the non-terminal prefix travels the default sink"
        );
        assert!(
            matches!(
                &seen[0],
                ProviderEvent::TextDelta { item_key, text }
                if item_key == "item-0" && text == "hi"
            ),
            "the replayed prefix preserves content and order"
        );
    }
}

#[cfg(test)]
mod cov_provider_private {
    use super::*;

    fn past_instant() -> Instant {
        Instant::now()
            .checked_sub(Duration::from_millis(1))
            .expect("test clock has history")
    }

    fn future_instant() -> Instant {
        Instant::now()
            .checked_add(Duration::from_secs(60))
            .expect("test clock has room")
    }

    #[test]
    fn legacy_constructor_leaves_private_control_state_empty() {
        let credential = CredentialRef::new("payments-prod-key").expect("valid reference");
        let context = ProviderContext::new(Duration::from_secs(60), true, Some(credential.clone()));
        assert_eq!(context.deadline_elapsed, Duration::from_secs(60));
        assert!(context.cancelled);
        assert_eq!(context.credential.as_ref(), Some(&credential));
        assert!(
            context.token.is_none(),
            "legacy contexts carry no live token"
        );
        assert!(
            context.deadline_at.is_none(),
            "an elapsed reading is not an evaluable instant"
        );
    }

    #[test]
    fn with_control_sets_only_private_control_fields() {
        let token = CancellationToken::new();
        let deadline = future_instant();
        let context = ProviderContext::new(Duration::from_secs(300), false, None)
            .with_control(token.clone(), deadline);
        assert!(context.token.as_ref().is_some_and(|held| held == &token));
        assert_eq!(context.deadline_at, Some(deadline));
        assert_eq!(
            context.deadline_elapsed,
            Duration::from_secs(300),
            "legacy reading untouched"
        );
        assert!(!context.cancelled, "legacy snapshot untouched");
        assert!(context.credential.is_none());
    }

    #[test]
    fn with_control_replaces_private_control_state() {
        let first_token = CancellationToken::new();
        let second_token = CancellationToken::new();
        let first_deadline = future_instant();
        let second_deadline = first_deadline
            .checked_add(Duration::from_secs(60))
            .expect("test clock has room");
        let context = ProviderContext::new(Duration::ZERO, false, None)
            .with_control(first_token.clone(), first_deadline)
            .with_control(second_token.clone(), second_deadline);
        assert!(
            context
                .token
                .as_ref()
                .is_some_and(|held| held == &second_token)
        );
        assert!(
            !context
                .token
                .as_ref()
                .is_some_and(|held| held == &first_token),
            "the previous token no longer controls the context"
        );
        assert_eq!(context.deadline_at, Some(second_deadline));
    }

    #[test]
    fn is_cancelled_ors_live_token_and_legacy_snapshot() {
        let inactive = ProviderContext {
            deadline_elapsed: Duration::ZERO,
            cancelled: false,
            credential: None,
            token: None,
            deadline_at: None,
        };
        assert!(
            !inactive.is_cancelled(),
            "no token and no snapshot is active"
        );

        let snapshot = ProviderContext {
            deadline_elapsed: Duration::ZERO,
            cancelled: true,
            credential: None,
            token: None,
            deadline_at: None,
        };
        assert!(snapshot.is_cancelled(), "legacy snapshot alone cancels");

        let cancelled_token = CancellationToken::new();
        cancelled_token.cancel();
        let live = ProviderContext {
            deadline_elapsed: Duration::ZERO,
            cancelled: false,
            credential: None,
            token: Some(cancelled_token),
            deadline_at: None,
        };
        assert!(live.is_cancelled(), "cancelled live token alone cancels");

        let snapshot_over_live = ProviderContext {
            deadline_elapsed: Duration::ZERO,
            cancelled: true,
            credential: None,
            token: Some(CancellationToken::new()),
            deadline_at: None,
        };
        assert!(
            snapshot_over_live.is_cancelled(),
            "a live token never clears the legacy snapshot"
        );
    }

    #[test]
    fn check_active_prefers_cancellation_over_elapsed_deadline() {
        let token = CancellationToken::new();
        token.cancel();
        let context = ProviderContext {
            deadline_elapsed: Duration::ZERO,
            cancelled: false,
            credential: None,
            token: Some(token),
            deadline_at: Some(past_instant()),
        };
        let error = context.check_active().expect_err("cancellation wins");
        assert_eq!(error.category(), ErrorCategory::Cancelled);
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        assert!(error.correlation().is_empty());
    }

    #[test]
    fn check_not_cancelled_ignores_private_deadline_state() {
        let context = ProviderContext {
            deadline_elapsed: Duration::MAX,
            cancelled: false,
            credential: None,
            token: None,
            deadline_at: Some(past_instant()),
        };
        assert!(
            context.check_not_cancelled().is_ok(),
            "the cancellation-only check never evaluates the deadline"
        );
        assert_eq!(
            context
                .check_active()
                .expect_err("deadline elapsed")
                .category(),
            ErrorCategory::Timeout
        );
    }

    #[test]
    fn port_state_error_keeps_category_message_and_no_retry() {
        let error = port_state_error(
            ErrorCategory::Timeout,
            "provider invocation deadline exceeded",
        );
        assert_eq!(error.category(), ErrorCategory::Timeout);
        assert_eq!(error.message(), "provider invocation deadline exceeded");
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        assert!(error.correlation().is_empty());
    }
}
