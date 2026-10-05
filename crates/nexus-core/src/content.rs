//! Message and content model.
//!
//! Content variants are explicit: text, proposed tool calls, admitted tool
//! calls, tool results, and bounded opaque continuation bytes. Provider item
//! keys and call references are preserved for adapter round-trips but are
//! never host identities: a [`CallCandidate`] carries no [`CallId`], and only
//! an admitted [`ToolCall`] may reach an executor. Partial streaming output
//! is marked [`TurnCompleteness::Partial`] and can only become executable
//! through [`AssistantTurn::into_completed`], so it is never mistaken for a
//! completed turn.

use crate::approval::NormalizedArgs;
use crate::error::{AgentError, ErrorCategory, RetryGuidance};
use crate::ids::{CallId, RunId, TurnId};
use crate::limits::Limits;

/// Maximum opaque continuation state in bytes (M0-TEST representation
/// choice: continuation state is bounded but the lock sets no table value).
pub const MAX_CONTINUATION_BYTES: usize = 65_536;
/// Maximum provider turn-local item key length in bytes (M0-TEST choice).
pub const MAX_ITEM_KEY_LEN: usize = 128;
/// Maximum provider call-reference length in bytes (M0-TEST choice).
pub const MAX_PROVIDER_REF_LEN: usize = 256;
/// Maximum candidate tool-name length in bytes; charset follows [`crate::ids`].
pub const MAX_TOOL_NAME_LEN: usize = 64;

/// Validated text content within the tool-output budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextContent(String);

impl TextContent {
    /// Validates non-empty text within bounds.
    pub fn new(raw: impl Into<String>) -> Result<Self, AgentError> {
        let value = raw.into();
        if value.is_empty() {
            return Err(content_error("text content is empty"));
        }
        if value.len() > Limits::M0_TEST_TOOL_OUTPUT_BYTES {
            return Err(content_error("text content exceeds output budget"));
        }
        Ok(Self(value))
    }

    /// Returns the text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Complete parsed call candidate from a `ToolCallReady` event.
///
/// This is a provider proposal, not an authorization: it carries the
/// turn-local item key and the original provider reference for round-trips,
/// and carries no run, turn, or host call identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallCandidate {
    item_key: String,
    provider_ref: String,
    tool_name: String,
    arguments_json: String,
}

impl CallCandidate {
    /// Validates the candidate at the adapter boundary.
    pub fn new(
        item_key: impl Into<String>,
        provider_ref: impl Into<String>,
        tool_name: impl Into<String>,
        arguments_json: impl Into<String>,
    ) -> Result<Self, AgentError> {
        let item_key = item_key.into();
        let provider_ref = provider_ref.into();
        let tool_name = tool_name.into();
        let arguments_json = arguments_json.into();
        if item_key.is_empty()
            || item_key.len() > MAX_ITEM_KEY_LEN
            || provider_ref.is_empty()
            || provider_ref.len() > MAX_PROVIDER_REF_LEN
            || tool_name.is_empty()
            || tool_name.len() > MAX_TOOL_NAME_LEN
            || !tool_name.bytes().all(is_tool_name_byte)
        {
            return Err(content_error("tool call candidate identity is invalid"));
        }
        if arguments_json.len() > Limits::M0_TEST_ARG_ASSEMBLY_BYTES {
            return Err(content_error("tool call candidate exceeds assembly budget"));
        }
        Ok(Self {
            item_key,
            provider_ref,
            tool_name,
            arguments_json,
        })
    }

    /// Returns the stable turn-local item key.
    #[must_use]
    pub fn item_key(&self) -> &str {
        &self.item_key
    }

    /// Returns the original provider reference for adapter round-trips.
    #[must_use]
    pub fn provider_ref(&self) -> &str {
        &self.provider_ref
    }

    /// Returns the proposed tool name (not yet resolved or authorized).
    #[must_use]
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    /// Returns the raw proposed arguments (not yet validated).
    #[must_use]
    pub fn arguments_json(&self) -> &str {
        &self.arguments_json
    }
}

const fn is_tool_name_byte(byte: u8) -> bool {
    matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_')
}

/// Admitted tool call: a complete candidate the runtime accepted, assigned a
/// host [`CallId`], resolved to a versioned tool, and validated into
/// immutable [`NormalizedArgs`]. Only this type may reach an executor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    run: RunId,
    turn: TurnId,
    call: CallId,
    tool: crate::ids::ToolId,
    args: NormalizedArgs,
}

impl ToolCall {
    /// Binds an admitted call. All inputs are already-validated types.
    pub fn new(
        run: RunId,
        turn: TurnId,
        call: CallId,
        tool: crate::ids::ToolId,
        args: NormalizedArgs,
    ) -> Self {
        Self {
            run,
            turn,
            call,
            tool,
            args,
        }
    }

    /// Returns the owning run.
    #[must_use]
    pub fn run(&self) -> &RunId {
        &self.run
    }

    /// Returns the owning turn.
    #[must_use]
    pub fn turn(&self) -> &TurnId {
        &self.turn
    }

    /// Returns the host call identity.
    #[must_use]
    pub fn call(&self) -> &CallId {
        &self.call
    }

    /// Returns the resolved tool identity plus revision.
    #[must_use]
    pub fn tool(&self) -> &crate::ids::ToolId {
        &self.tool
    }

    /// Returns the immutable validated arguments.
    #[must_use]
    pub fn args(&self) -> &NormalizedArgs {
        &self.args
    }
}

/// Bounded tool result sharing the operation output budget with progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    call: CallId,
    content: String,
    truncated: bool,
}

impl ToolResult {
    /// Validates content against the shared output budget.
    pub fn new(
        call: CallId,
        content: impl Into<String>,
        truncated: bool,
    ) -> Result<Self, AgentError> {
        let content = content.into();
        if content.len() > Limits::M0_TEST_TOOL_OUTPUT_BYTES {
            return Err(content_error("tool result exceeds output budget"));
        }
        Ok(Self {
            call,
            content,
            truncated,
        })
    }

    /// Returns the owning call identity.
    #[must_use]
    pub fn call(&self) -> &CallId {
        &self.call
    }

    /// Returns the result content.
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

/// Bounded opaque provider continuation state.
///
/// Scoped to its adapter and compatibility scope and versioned for
/// persistence. The core never interprets it as permissions or exposes it
/// as ordinary text; an incompatible switch requires explicit conversion
/// or failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinuationData {
    adapter: String,
    scope: String,
    bytes: Vec<u8>,
}

impl ContinuationData {
    /// Validates scope labels and the byte bound.
    pub fn new(
        adapter: impl Into<String>,
        scope: impl Into<String>,
        bytes: Vec<u8>,
    ) -> Result<Self, AgentError> {
        let adapter = adapter.into();
        let scope = scope.into();
        if adapter.is_empty()
            || adapter.len() > MAX_ITEM_KEY_LEN
            || scope.is_empty()
            || scope.len() > MAX_ITEM_KEY_LEN
        {
            return Err(content_error("continuation scope is invalid"));
        }
        if bytes.len() > MAX_CONTINUATION_BYTES {
            return Err(content_error("continuation state exceeds its bound"));
        }
        Ok(Self {
            adapter,
            scope,
            bytes,
        })
    }

    /// Returns the owning adapter label.
    #[must_use]
    pub fn adapter(&self) -> &str {
        &self.adapter
    }

    /// Returns the compatibility scope.
    #[must_use]
    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// Returns the opaque bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Exact-equality compatibility: same adapter and scope.
    #[must_use]
    pub fn is_compatible_with(&self, adapter: &str, scope: &str) -> bool {
        self.adapter == adapter && self.scope == scope
    }
}

/// Explicit content variants. External content never gains instruction
/// authority by being placed in a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentBlock {
    /// Displayable text.
    Text(TextContent),
    /// Non-executable proposed call from the provider.
    CallProposal(CallCandidate),
    /// Admitted call bound to host identities.
    Call(ToolCall),
    /// Bounded result of an executed call.
    Result(ToolResult),
    /// Opaque provider continuation state.
    Continuation(ContinuationData),
}

/// Whether streamed content is a complete turn. Partial output must never
/// be stored or dispatched as a completed assistant turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnCompleteness {
    /// Still streaming; not eligible for admission or storage as complete.
    Partial,
    /// Fully consumed and eligible for acceptance checks.
    Complete,
}

/// Assistant turn record carrying its completeness marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantTurn {
    run: RunId,
    turn: TurnId,
    blocks: Vec<ContentBlock>,
    completeness: TurnCompleteness,
}

impl AssistantTurn {
    /// Builds a turn; block count is bounded by retained context capacity.
    pub fn new(
        run: RunId,
        turn: TurnId,
        blocks: Vec<ContentBlock>,
        completeness: TurnCompleteness,
    ) -> Result<Self, AgentError> {
        if blocks.len() > Limits::M0_TEST_RETAINED_CONTEXT_ITEMS {
            return Err(content_error("turn exceeds retained context budget"));
        }
        Ok(Self {
            run,
            turn,
            blocks,
            completeness,
        })
    }

    /// Returns true only for fully consumed turns.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.completeness == TurnCompleteness::Complete
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

    /// Returns the content blocks.
    #[must_use]
    pub fn blocks(&self) -> &[ContentBlock] {
        &self.blocks
    }

    /// Consumes a complete turn into an executable record. Partial output
    /// is rejected explicitly and can never convert silently.
    pub fn into_completed(self) -> Result<CompletedTurn, AgentError> {
        if !self.is_complete() {
            return Err(content_error("partial output is not a completed turn"));
        }
        Ok(CompletedTurn(self))
    }
}

/// A turn proven complete. Dispatch and admission take this type, never a
/// bare [`AssistantTurn`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedTurn(AssistantTurn);

impl CompletedTurn {
    /// Returns the underlying turn record.
    #[must_use]
    pub fn inner(&self) -> &AssistantTurn {
        &self.0
    }
}

fn content_error(message: &'static str) -> AgentError {
    AgentError::new(
        ErrorCategory::InvalidInput,
        message,
        RetryGuidance::DoNotRetry,
    )
    .expect("static safe content message builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::M0_REVISION;

    fn test_ids() -> (RunId, TurnId, CallId) {
        (
            RunId::new("run-1").expect("valid"),
            TurnId::new("turn-1").expect("valid"),
            CallId::new("call-1").expect("valid"),
        )
    }

    #[test]
    fn partial_output_can_never_become_completed() {
        let (run, turn, _) = test_ids();
        let partial = AssistantTurn::new(
            run,
            turn,
            vec![ContentBlock::Text(TextContent::new("half").expect("valid"))],
            TurnCompleteness::Partial,
        )
        .expect("partial turn builds");
        assert!(!partial.is_complete());
        assert!(partial.into_completed().is_err());
    }

    #[test]
    fn complete_turn_converts_and_exposes_blocks() {
        let (run, turn, _) = test_ids();
        let complete = AssistantTurn::new(
            run,
            turn,
            vec![ContentBlock::Text(TextContent::new("done").expect("valid"))],
            TurnCompleteness::Complete,
        )
        .expect("complete turn builds");
        assert!(complete.is_complete());
        let completed = complete.into_completed().expect("complete converts");
        assert_eq!(completed.inner().blocks().len(), 1);
    }

    #[test]
    fn candidate_carries_no_host_identity_while_admitted_call_does() {
        let candidate = CallCandidate::new("item-0", "prov-ref-0", "host_read", r#"{"a":1}"#)
            .expect("valid candidate builds");
        assert_eq!(candidate.item_key(), "item-0");
        assert_eq!(candidate.provider_ref(), "prov-ref-0");
        // Candidate has no run/turn/call fields by construction; admission
        // binds them explicitly.
        let (run, turn, call) = test_ids();
        let admitted = ToolCall::new(
            run.clone(),
            turn.clone(),
            call.clone(),
            crate::ids::ToolId::new(candidate.tool_name(), M0_REVISION).expect("valid"),
            NormalizedArgs::new(candidate.arguments_json()).expect("valid args"),
        );
        assert_eq!(admitted.run(), &run);
        assert_eq!(admitted.call(), &call);
        assert_eq!(admitted.args().as_str(), r#"{"a":1}"#);
    }

    #[test]
    fn continuation_is_bounded_and_compat_checked_exactly() {
        let state =
            ContinuationData::new("acme-adapter", "model-x", vec![0u8; 16]).expect("valid builds");
        assert!(state.is_compatible_with("acme-adapter", "model-x"));
        assert!(!state.is_compatible_with("acme-adapter", "model-y"));
        assert!(!state.is_compatible_with("other-adapter", "model-x"));
        assert!(ContinuationData::new("a", "s", vec![0u8; MAX_CONTINUATION_BYTES + 1]).is_err());
    }

    #[test]
    fn oversize_text_and_results_are_rejected() {
        assert!(TextContent::new("x".repeat(Limits::M0_TEST_TOOL_OUTPUT_BYTES + 1)).is_err());
        let (_, _, call) = test_ids();
        assert!(
            ToolResult::new(
                call,
                "x".repeat(Limits::M0_TEST_TOOL_OUTPUT_BYTES + 1),
                true
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod cov_content_private {
    use super::*;

    #[test]
    fn content_error_is_invalid_input_without_retry() {
        let error = content_error("synthetic content failure");
        assert_eq!(error.category(), ErrorCategory::InvalidInput);
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        assert_eq!(error.message(), "synthetic content failure");
        assert!(error.correlation().is_empty());
    }

    #[test]
    fn tool_name_byte_charset_is_exactly_the_lock_charset() {
        for byte in b'A'..=b'Z' {
            assert!(is_tool_name_byte(byte), "{byte}");
        }
        for byte in b'a'..=b'z' {
            assert!(is_tool_name_byte(byte), "{byte}");
        }
        for byte in b'0'..=b'9' {
            assert!(is_tool_name_byte(byte), "{byte}");
        }
        for byte in [b'-', b'_'] {
            assert!(is_tool_name_byte(byte), "{byte}");
        }
        for byte in [0u8, b'.', b'/', b' ', b':', b'@', b'\\', b'~', 0xFF] {
            assert!(!is_tool_name_byte(byte), "{byte}");
        }
    }

    #[test]
    fn private_turn_state_converts_only_when_complete() {
        let run = RunId::new("run-private").expect("valid run id");
        let turn = TurnId::new("turn-private").expect("valid turn id");

        let partial = AssistantTurn {
            run: run.clone(),
            turn: turn.clone(),
            blocks: vec![ContentBlock::Text(
                TextContent::new("half").expect("valid text"),
            )],
            completeness: TurnCompleteness::Partial,
        };
        assert!(!partial.is_complete());
        assert!(partial.into_completed().is_err());

        let complete = AssistantTurn {
            run,
            turn,
            blocks: vec![ContentBlock::Text(
                TextContent::new("done").expect("valid text"),
            )],
            completeness: TurnCompleteness::Complete,
        };
        let completed = complete
            .clone()
            .into_completed()
            .expect("complete converts");
        assert_eq!(completed.0, complete);
        assert_eq!(completed.inner(), &complete);
        assert!(completed.inner().is_complete());
    }
}
