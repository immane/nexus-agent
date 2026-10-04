//! Nexus core domain types, errors, limits, and port traits.
//!
//! Data lives here; behavior lives in adapters and the runtime (P3).
//! This crate is dependency-free (standard library only) and contains
//! no terminal, HTTP, SDK, or OS-handle types.

#![forbid(unsafe_code)]

pub mod approval;
pub mod commands;
pub mod content;
pub mod error;
pub mod execution;
pub mod ids;
pub mod limits;
pub mod outcomes;
pub mod provider;
pub mod store;
pub mod tool;

pub use approval::{ApprovalBinding, ApprovedScope, NormalizedArgs};
pub use commands::{
    ApprovalNotice, ApproveCommand, AssistantText, CancelCommand, Command, CommandResponse,
    DenyCommand, EventPayload, GetSnapshotCommand, ListSessionsCommand, OutcomeSummary,
    RestoreSessionCommand, RunEvent, RunLifecycle, Snapshot, SubmitCommand, ToolFinishedInfo,
    ToolProgress, ToolStartedInfo,
};
pub use content::{
    AssistantTurn, CallCandidate, CompletedTurn, ContentBlock, ContinuationData,
    MAX_CONTINUATION_BYTES, MAX_ITEM_KEY_LEN, MAX_PROVIDER_REF_LEN, TextContent, ToolCall,
    ToolResult, TurnCompleteness,
};
pub use error::{AgentError, CorrelationData, ErrorBuildError, ErrorCategory, RetryGuidance};
pub use execution::{CancellationToken, Deadline};
pub use ids::{
    ApprovalId, CallId, IdError, M0_REVISION, RequestId, RunId, SessionId, ToolId, TurnId,
};
pub use limits::Limits;
pub use outcomes::{
    CommandReply, EffectState, Evidence, ExecutionStatus, FinishReason, InvocationOutcome,
    PersistenceState, RunFinished, RunOutcome, ToolOutcome, TurnFinished, Usage, UsageFinality,
};
pub use provider::{
    CredentialRef, DEFAULT_ADAPTER_IDENTITY, ItemKey, MAX_CONVERSATION_BYTES,
    MAX_CONVERSATION_ITEMS, MAX_TOOL_DEFINITION_BYTES, MAX_TOOL_DEFINITIONS, ModelContextItem,
    ModelRequest, ProviderCapabilities, ProviderContext, ProviderEvent, ProviderPort, ProviderRef,
};
pub use store::{
    STORE_FORMAT_REVISION, SessionCheckpoint, SessionMetadata, SessionStore, ToolIntentRecord,
    ToolOutcomeRecord,
};
pub use tool::{ToolContext, ToolPort, ToolSpec};
