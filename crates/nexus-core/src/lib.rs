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

#[cfg(test)]
mod cov_lib_private {
    //! Unit coverage for the crate root itself: every declared module stays
    //! reachable and every root re-export aliases its defining module item
    //! rather than a shadowing wrapper. Compile-time checks only.

    fn identity<T>(value: T) -> T {
        value
    }

    #[test]
    #[ignore = "low-value: root/module type aliases; public behavioral suites remain enabled"]
    fn root_reexports_alias_defining_modules() {
        macro_rules! alias_all {
            ($module:ident :: { $($name:ident),+ $(,)? }) => {
                $(
                    let _: fn(crate::$module::$name) -> crate::$name = identity;
                    let _: fn(crate::$name) -> crate::$module::$name = identity;
                )+
            };
        }

        alias_all!(approval::{ApprovalBinding, ApprovedScope, NormalizedArgs});
        alias_all!(commands::{
            ApprovalNotice, ApproveCommand, AssistantText, CancelCommand, Command, CommandResponse,
            DenyCommand, EventPayload, GetSnapshotCommand, ListSessionsCommand, OutcomeSummary,
            RestoreSessionCommand, RunEvent, RunLifecycle, Snapshot, SubmitCommand,
            ToolFinishedInfo, ToolProgress, ToolStartedInfo,
        });
        alias_all!(content::{
            AssistantTurn, CallCandidate, CompletedTurn, ContentBlock, ContinuationData,
            TextContent, ToolCall, ToolResult, TurnCompleteness,
        });
        alias_all!(error::{
            AgentError, CorrelationData, ErrorBuildError, ErrorCategory, RetryGuidance,
        });
        alias_all!(execution::{CancellationToken, Deadline});
        alias_all!(ids::{
            ApprovalId, CallId, IdError, RequestId, RunId, SessionId, ToolId, TurnId,
        });
        alias_all!(limits::{Limits});
        alias_all!(outcomes::{
            CommandReply, EffectState, Evidence, ExecutionStatus, FinishReason, InvocationOutcome,
            PersistenceState, RunFinished, RunOutcome, ToolOutcome, TurnFinished, Usage,
            UsageFinality,
        });
        alias_all!(provider::{
            CredentialRef, ItemKey, ModelContextItem, ModelRequest, ProviderCapabilities,
            ProviderContext, ProviderEvent, ProviderRef,
        });
        alias_all!(store::{
            SessionCheckpoint, SessionMetadata, ToolIntentRecord, ToolOutcomeRecord,
        });
        alias_all!(tool::{ToolContext, ToolSpec});
    }

    #[test]
    #[ignore = "low-value: root/module trait aliases checked by compilation of callers"]
    fn root_traits_alias_defining_modules() {
        fn takes_provider_port(_: &dyn crate::ProviderPort) {}
        let _: fn(&dyn crate::provider::ProviderPort) = takes_provider_port;
        fn takes_session_store(_: &dyn crate::SessionStore) {}
        let _: fn(&dyn crate::store::SessionStore) = takes_session_store;
        fn takes_tool_port(_: &dyn crate::ToolPort) {}
        let _: fn(&dyn crate::tool::ToolPort) = takes_tool_port;
    }

    #[test]
    #[ignore = "low-value: root/module constant aliases; boundary tests remain enabled"]
    fn root_constants_alias_defining_modules() {
        assert_eq!(crate::M0_REVISION, crate::ids::M0_REVISION);
        assert_eq!(
            crate::STORE_FORMAT_REVISION,
            crate::store::STORE_FORMAT_REVISION
        );
        assert_eq!(
            crate::MAX_CONTINUATION_BYTES,
            crate::content::MAX_CONTINUATION_BYTES
        );
        assert_eq!(crate::MAX_ITEM_KEY_LEN, crate::content::MAX_ITEM_KEY_LEN);
        assert_eq!(
            crate::MAX_PROVIDER_REF_LEN,
            crate::content::MAX_PROVIDER_REF_LEN
        );
        assert_eq!(
            crate::MAX_CONVERSATION_ITEMS,
            crate::provider::MAX_CONVERSATION_ITEMS
        );
        assert_eq!(
            crate::MAX_CONVERSATION_BYTES,
            crate::provider::MAX_CONVERSATION_BYTES
        );
        assert_eq!(
            crate::MAX_TOOL_DEFINITIONS,
            crate::provider::MAX_TOOL_DEFINITIONS
        );
        assert_eq!(
            crate::MAX_TOOL_DEFINITION_BYTES,
            crate::provider::MAX_TOOL_DEFINITION_BYTES
        );
        assert_eq!(
            crate::DEFAULT_ADAPTER_IDENTITY,
            crate::provider::DEFAULT_ADAPTER_IDENTITY
        );
    }
}
