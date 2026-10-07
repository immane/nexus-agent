#![forbid(unsafe_code)]

//! Public-surface coverage for the `nexus-core` crate root.
//!
//! Imports every root re-export from `lib.rs` once, so a missing or renamed
//! item fails compilation, then smoke-constructs one value per type family
//! through the public API only. Module-level items the re-export block does
//! not list (`StoredMessage`, `EventSequence`, checked helpers, bound
//! constants) are pinned as well, because they are part of the reachable
//! public surface. Deterministic: fixed inputs, no threads, no I/O, no
//! sleeping, standard library only.

use std::time::{Duration, Instant};

use nexus_core::approval::MAX_SCOPE_BYTES;
use nexus_core::commands::{
    EventSequence, MAX_INPUT_BYTES, MAX_LIST_LIMIT, MAX_SUMMARY_BYTES, MAX_TEXT_FRAGMENT_BYTES,
    checked_next_sequence,
};
use nexus_core::content::MAX_TOOL_NAME_LEN;
use nexus_core::error::{
    MAX_CORRELATION_ENTRIES, MAX_CORRELATION_KEY_LEN, MAX_CORRELATION_VALUE_LEN, MAX_MESSAGE_LEN,
};
use nexus_core::ids::MAX_ID_LEN;
use nexus_core::provider::{MAX_CREDENTIAL_REF_LEN, MAX_PROFILE_LEN};
use nexus_core::store::{
    MAX_INTENT_RECORDS, MAX_OUTCOME_RECORDS, MAX_SESSIONS, MAX_STORED_MESSAGES, StoredMessage,
    check_checkpoint_bounds, check_format_revision,
};
use nexus_core::tool::{MAX_SCHEMA_BYTES, MAX_TOOL_DESCRIPTION_LEN};
use nexus_core::{
    AgentError, ApprovalBinding, ApprovalId, ApprovalNotice, ApproveCommand, ApprovedScope,
    AssistantText, AssistantTurn, CallCandidate, CallId, CancelCommand, CancellationToken, Command,
    CommandReply, CommandResponse, CompletedTurn, ContentBlock, ContinuationData, CorrelationData,
    CredentialRef, DEFAULT_ADAPTER_IDENTITY, Deadline, DenyCommand, EffectState, ErrorBuildError,
    ErrorCategory, EventPayload, Evidence, ExecutionStatus, FinishReason, GetSnapshotCommand,
    IdError, InvocationOutcome, ItemKey, Limits, ListSessionsCommand, M0_REVISION,
    MAX_CONTINUATION_BYTES, MAX_CONVERSATION_BYTES, MAX_CONVERSATION_ITEMS, MAX_ITEM_KEY_LEN,
    MAX_PROVIDER_REF_LEN, MAX_TOOL_DEFINITION_BYTES, MAX_TOOL_DEFINITIONS, ModelContextItem,
    ModelRequest, NormalizedArgs, OutcomeSummary, PersistenceState, ProviderCapabilities,
    ProviderContext, ProviderEvent, ProviderPort, ProviderRef, RequestId, RestoreSessionCommand,
    RetryGuidance, RunEvent, RunFinished, RunId, RunLifecycle, RunOutcome, STORE_FORMAT_REVISION,
    SessionCheckpoint, SessionId, SessionMetadata, SessionStore, Snapshot, SubmitCommand,
    TextContent, ToolCall, ToolContext, ToolFinishedInfo, ToolId, ToolIntentRecord, ToolOutcome,
    ToolOutcomeRecord, ToolPort, ToolProgress, ToolResult, ToolSpec, ToolStartedInfo,
    TurnCompleteness, TurnFinished, TurnId, Usage, UsageFinality,
};

fn run_id() -> RunId {
    RunId::new("run-1").expect("valid run id")
}

fn turn_id() -> TurnId {
    TurnId::new("turn-1").expect("valid turn id")
}

fn call_id() -> CallId {
    CallId::new("call-1").expect("valid call id")
}

fn request_id() -> RequestId {
    RequestId::new("req-1").expect("valid request id")
}

fn session_id() -> SessionId {
    SessionId::new("sess-1").expect("valid session id")
}

fn approval_id() -> ApprovalId {
    ApprovalId::new("appr-1").expect("valid approval id")
}

fn tool_id() -> ToolId {
    ToolId::new("host_read", M0_REVISION).expect("valid tool id")
}

fn normalized_args() -> NormalizedArgs {
    NormalizedArgs::new(r#"{"path":"src"}"#).expect("valid normalized args")
}

fn tool_call() -> ToolCall {
    ToolCall::new(run_id(), turn_id(), call_id(), tool_id(), normalized_args())
}

fn tool_spec() -> ToolSpec {
    ToolSpec::new(tool_id(), "read files", r#"{"type":"object"}"#).expect("valid tool spec")
}

fn safe_error() -> AgentError {
    AgentError::new(
        ErrorCategory::Internal,
        "internal failure",
        RetryGuidance::DoNotRetry,
    )
    .expect("static safe diagnostic builds")
}

fn succeeded_outcome(content: &str) -> ToolOutcome {
    ToolOutcome::new(
        ExecutionStatus::Succeeded,
        EffectState::KnownApplied,
        Evidence::HostObserved,
        content,
        false,
    )
    .expect("bounded outcome builds")
}

/// Asserts every element of a finite enum-variant list is distinct, so the
/// list covers the variants without relying on `PartialEq` against itself.
fn assert_variants_distinct<T: PartialEq + std::fmt::Debug>(label: &str, values: &[T]) {
    for (index, value) in values.iter().enumerate() {
        let matches = values.iter().filter(|other| *other == value).count();
        assert_eq!(matches, 1, "{label}: entry {index} must be unique");
    }
}

#[test]
fn ids_surface_smoke() {
    macro_rules! check_id {
        ($type:ty, $label:literal) => {{
            let id = <$type>::new("id-1").expect("valid id");
            assert_eq!(id.as_str(), "id-1", $label);
            assert_eq!(id.to_string(), "id-1", $label);
            assert_eq!(AsRef::<str>::as_ref(&id), "id-1", $label);
            assert_eq!(<$type>::new(""), Err(IdError::Empty), $label);
            assert_eq!(<$type>::new("a b"), Err(IdError::IllegalChar), $label);
            assert_eq!(
                <$type>::new("x".repeat(MAX_ID_LEN + 1)),
                Err(IdError::TooLong),
                $label
            );
        }};
    }

    check_id!(SessionId, "SessionId");
    check_id!(RunId, "RunId");
    check_id!(TurnId, "TurnId");
    check_id!(CallId, "CallId");
    check_id!(RequestId, "RequestId");
    check_id!(ApprovalId, "ApprovalId");

    let tool = tool_id();
    assert_eq!(tool.name(), "host_read");
    assert_eq!(tool.revision(), M0_REVISION);
    assert_eq!(tool.to_string(), "host_read@0");
    assert!(tool.is_compatible_with(&tool.clone()));
    let newer = ToolId::new("host_read", M0_REVISION + 1).expect("valid tool id");
    assert!(!tool.is_compatible_with(&newer));
    assert_eq!(M0_REVISION, 0);
    assert_eq!(MAX_ID_LEN, 64);

    assert_eq!(IdError::Empty.to_string(), "identifier is empty");
    assert_eq!(
        IdError::TooLong.to_string(),
        "identifier exceeds 64 characters"
    );
    assert_eq!(
        IdError::IllegalChar.to_string(),
        "identifier contains illegal characters"
    );
    let _: &dyn std::error::Error = &IdError::Empty;
}

#[test]
fn error_surface_smoke() {
    let categories = [
        (ErrorCategory::InvalidInput, "invalid-input"),
        (
            ErrorCategory::UnsupportedCapability,
            "unsupported-capability",
        ),
        (ErrorCategory::Authentication, "authentication"),
        (ErrorCategory::PermissionDenied, "permission-denied"),
        (ErrorCategory::RateLimited, "rate-limited"),
        (ErrorCategory::Protocol, "protocol"),
        (ErrorCategory::Timeout, "timeout"),
        (ErrorCategory::Cancelled, "cancelled"),
        (ErrorCategory::ResourceLimit, "resource-limit"),
        (ErrorCategory::ToolFailure, "tool-failure"),
        (ErrorCategory::StorageFailure, "storage-failure"),
        (ErrorCategory::UncertainOutcome, "uncertain-outcome"),
        (ErrorCategory::Internal, "internal"),
    ];
    for (category, name) in categories {
        assert_eq!(category.as_str(), name);
    }

    for retry in [
        RetryGuidance::DoNotRetry,
        RetryGuidance::RetryAfterBackoff,
        RetryGuidance::SafeToRetry,
    ] {
        let error = AgentError::new(ErrorCategory::Internal, "internal failure", retry)
            .expect("static safe diagnostic builds");
        assert_eq!(error.category(), ErrorCategory::Internal);
        assert_eq!(error.message(), "internal failure");
        assert_eq!(error.retry(), retry);
        assert!(error.correlation().is_empty());
        assert_eq!(error.to_string(), "[internal] internal failure");
        let _: &dyn std::error::Error = &error;
    }

    let mut correlation = CorrelationData::new();
    correlation.push("run", "run-1").expect("safe entry builds");
    assert_eq!(correlation.len(), 1);
    assert!(!correlation.is_empty());
    assert_eq!(correlation.iter().count(), 1);
    let error = AgentError::with_correlation(
        ErrorCategory::Timeout,
        "tool deadline exceeded",
        correlation.clone(),
        RetryGuidance::SafeToRetry,
    )
    .expect("bounded correlation builds");
    assert_eq!(error.correlation(), &correlation);

    assert_eq!(
        AgentError::new(ErrorCategory::Internal, "", RetryGuidance::DoNotRetry),
        Err(ErrorBuildError::Empty)
    );
    assert_eq!(
        AgentError::new(
            ErrorCategory::Internal,
            "x".repeat(MAX_MESSAGE_LEN + 1),
            RetryGuidance::DoNotRetry,
        ),
        Err(ErrorBuildError::TooLong)
    );
    assert_eq!(
        AgentError::new(
            ErrorCategory::Authentication,
            "sk-live-0000",
            RetryGuidance::DoNotRetry,
        ),
        Err(ErrorBuildError::SuspectedSecret)
    );

    let mut full = CorrelationData::new();
    for index in 0..MAX_CORRELATION_ENTRIES {
        full.push(format!("key-{index}"), "value")
            .expect("entry within bound");
    }
    assert_eq!(full.push("key", "value"), Err(ErrorBuildError::TooLong));
    assert_eq!(
        CorrelationData::new().push("", "value"),
        Err(ErrorBuildError::TooLong)
    );
    assert_eq!(
        CorrelationData::new().push("k".repeat(MAX_CORRELATION_KEY_LEN + 1), "value"),
        Err(ErrorBuildError::TooLong)
    );
    assert_eq!(
        CorrelationData::new().push("key", "v".repeat(MAX_CORRELATION_VALUE_LEN + 1)),
        Err(ErrorBuildError::TooLong)
    );

    for build_error in [
        ErrorBuildError::Empty,
        ErrorBuildError::TooLong,
        ErrorBuildError::SuspectedSecret,
    ] {
        assert!(!build_error.to_string().is_empty());
        let _: &dyn std::error::Error = &build_error;
    }
}

#[test]
fn content_surface_smoke() {
    let text = TextContent::new("hello").expect("valid text");
    assert_eq!(text.as_str(), "hello");
    assert!(TextContent::new("").is_err());

    let candidate = CallCandidate::new("item-1", "prov-ref-1", "host_read", r#"{"a":1}"#)
        .expect("valid candidate builds");
    assert_eq!(candidate.item_key(), "item-1");
    assert_eq!(candidate.provider_ref(), "prov-ref-1");
    assert_eq!(candidate.tool_name(), "host_read");
    assert_eq!(candidate.arguments_json(), r#"{"a":1}"#);

    let call = tool_call();
    assert_eq!(call.run(), &run_id());
    assert_eq!(call.turn(), &turn_id());
    assert_eq!(call.call(), &call_id());
    assert_eq!(call.tool(), &tool_id());
    assert_eq!(call.args(), &normalized_args());

    let result = ToolResult::new(call_id(), "output", false).expect("valid result builds");
    assert_eq!(result.call(), &call_id());
    assert_eq!(result.content(), "output");
    assert!(!result.is_truncated());

    let continuation = ContinuationData::new("adapter-a", "scope-a", vec![1, 2, 3])
        .expect("valid continuation builds");
    assert_eq!(continuation.adapter(), "adapter-a");
    assert_eq!(continuation.scope(), "scope-a");
    assert_eq!(continuation.bytes(), &[1, 2, 3]);
    assert!(continuation.is_compatible_with("adapter-a", "scope-a"));
    assert!(!continuation.is_compatible_with("adapter-a", "scope-b"));
    assert!(ContinuationData::new("a", "s", vec![0u8; MAX_CONTINUATION_BYTES + 1]).is_err());

    let blocks = vec![
        ContentBlock::Text(text),
        ContentBlock::CallProposal(candidate),
        ContentBlock::Call(call),
        ContentBlock::Result(result),
        ContentBlock::Continuation(continuation),
    ];
    assert_eq!(blocks.len(), 5);
    assert_ne!(TurnCompleteness::Partial, TurnCompleteness::Complete);

    let partial = AssistantTurn::new(
        run_id(),
        turn_id(),
        blocks.clone(),
        TurnCompleteness::Partial,
    )
    .expect("partial turn builds");
    assert!(!partial.is_complete());
    assert!(partial.into_completed().is_err());

    let completed: CompletedTurn =
        AssistantTurn::new(run_id(), turn_id(), blocks, TurnCompleteness::Complete)
            .expect("complete turn builds")
            .into_completed()
            .expect("complete turn converts");
    assert_eq!(completed.inner().run(), &run_id());
    assert_eq!(completed.inner().turn(), &turn_id());
    assert_eq!(completed.inner().blocks().len(), 5);

    assert_eq!(MAX_CONTINUATION_BYTES, 65_536);
    assert_eq!(MAX_ITEM_KEY_LEN, 128);
    assert_eq!(MAX_PROVIDER_REF_LEN, 256);
    assert_eq!(MAX_TOOL_NAME_LEN, 64);
}

#[test]
fn approval_surface_smoke() {
    let args = normalized_args();
    assert_eq!(args.as_str(), r#"{"path":"src"}"#);
    assert!(NormalizedArgs::new("not-object").is_err());

    let scope = ApprovedScope::new("project-read").expect("valid scope builds");
    assert_eq!(scope.as_str(), "project-read");
    assert!(ApprovedScope::new("x".repeat(MAX_SCOPE_BYTES + 1)).is_err());

    let binding = ApprovalBinding::new(
        approval_id(),
        run_id(),
        call_id(),
        tool_id(),
        args.clone(),
        scope.clone(),
        Duration::from_secs(120),
        M0_REVISION,
    );
    assert_eq!(binding.approval(), &approval_id());
    assert_eq!(binding.run(), &run_id());
    assert_eq!(binding.call(), &call_id());
    assert_eq!(binding.tool(), &tool_id());
    assert_eq!(binding.args(), &args);
    assert_eq!(binding.scope(), &scope);
    assert_eq!(binding.policy_revision(), M0_REVISION);
    assert_eq!(binding.expires_at_elapsed(), Duration::from_secs(120));
    assert!(!binding.is_expired(Duration::from_secs(119)));
    assert!(binding.is_expired(Duration::from_secs(120)));

    binding
        .check_valid_for_dispatch(
            &run_id(),
            &call_id(),
            &tool_id(),
            &args,
            M0_REVISION,
            Duration::ZERO,
        )
        .expect("exact tuple authorizes");
    assert!(
        binding
            .check_valid_for_dispatch(
                &run_id(),
                &call_id(),
                &tool_id(),
                &args,
                M0_REVISION,
                Duration::from_secs(120),
            )
            .is_err(),
        "an expired grant denies dispatch"
    );
    assert_eq!(MAX_SCOPE_BYTES, 1024);
}

#[test]
fn execution_surface_smoke() {
    let token = CancellationToken::new();
    let clone = token.clone();
    assert!(!token.is_cancelled());
    assert_eq!(token, clone);
    assert_ne!(token, CancellationToken::new());
    assert!(!CancellationToken::default().wait_timeout(Duration::ZERO));
    token.cancel();
    assert!(clone.is_cancelled(), "clones share the live flag");
    assert!(clone.wait_timeout(Duration::ZERO));
    assert!(clone.wait_until(Instant::now()));

    let at = Instant::now() + Duration::from_secs(60);
    let future = Deadline::at(at);
    assert_eq!(future.instant(), at);
    assert!(!future.is_elapsed());
    assert!(future.remaining() <= Duration::from_secs(60));
    assert!(Deadline::after(Duration::from_secs(60)).is_some());

    let past = Instant::now()
        .checked_sub(Duration::from_millis(1))
        .expect("monotonic clock has history");
    let elapsed = Deadline::at(past);
    assert!(elapsed.is_elapsed());
    assert_eq!(elapsed.remaining(), Duration::ZERO);
}

#[test]
fn limits_surface_smoke() {
    let limits = Limits::m0_test();
    assert_eq!(
        limits.max_model_turns_per_run,
        Limits::M0_TEST_MODEL_TURNS_PER_RUN
    );
    assert_eq!(
        limits.max_tool_calls_per_run,
        Limits::M0_TEST_TOOL_CALLS_PER_RUN
    );
    assert_eq!(
        limits.max_tool_calls_per_turn,
        Limits::M0_TEST_TOOL_CALLS_PER_TURN
    );
    assert_eq!(
        limits.run_duration,
        Duration::from_secs(Limits::M0_TEST_RUN_DURATION_SECS)
    );
    assert_eq!(
        limits.per_tool_timeout,
        Duration::from_secs(Limits::M0_TEST_PER_TOOL_TIMEOUT_SECS)
    );
    assert_eq!(
        limits.retained_context_items,
        Limits::M0_TEST_RETAINED_CONTEXT_ITEMS
    );
    assert_eq!(
        limits.max_arg_assembly_bytes,
        Limits::M0_TEST_ARG_ASSEMBLY_BYTES
    );
    assert_eq!(
        limits.max_tool_output_bytes,
        Limits::M0_TEST_TOOL_OUTPUT_BYTES
    );
    assert_eq!(
        limits.event_data_capacity,
        Limits::M0_TEST_EVENT_DATA_CAPACITY
    );
    assert_eq!(
        limits.event_control_capacity,
        Limits::M0_TEST_EVENT_CONTROL_CAPACITY
    );
    assert_eq!(
        limits.max_concurrent_ops,
        Limits::M0_TEST_MAX_CONCURRENT_OPS
    );
    assert_eq!(
        limits.approval_expiry,
        Duration::from_secs(Limits::M0_TEST_APPROVAL_EXPIRY_SECS)
    );
    assert_eq!(
        Limits::M0_TEST_MAX_DURATION,
        Duration::from_secs(24 * 60 * 60)
    );
    limits.validate().expect("M0-test budgets are valid");

    assert!(limits.check_model_turns(0).is_ok());
    assert!(
        limits
            .check_model_turns(limits.max_model_turns_per_run)
            .is_err()
    );
    assert!(
        limits
            .check_tool_calls_for_run(limits.max_tool_calls_per_run - 1)
            .is_ok()
    );
    assert!(
        limits
            .check_tool_calls_for_run(limits.max_tool_calls_per_run)
            .is_err()
    );
    assert!(
        limits
            .check_tool_calls_for_turn(limits.max_tool_calls_per_turn)
            .is_err()
    );
    assert!(
        limits
            .check_arg_assembly_bytes(limits.max_arg_assembly_bytes)
            .is_ok()
    );
    assert!(
        limits
            .check_arg_assembly_bytes(limits.max_arg_assembly_bytes + 1)
            .is_err()
    );
    assert!(
        limits
            .check_tool_output_bytes(limits.max_tool_output_bytes)
            .is_ok()
    );
    assert!(
        limits
            .check_tool_output_bytes(limits.max_tool_output_bytes + 1)
            .is_err()
    );
    assert!(
        limits
            .check_context_items(limits.retained_context_items)
            .is_ok()
    );
    assert!(
        limits
            .check_context_items(limits.retained_context_items + 1)
            .is_err()
    );
    assert!(
        limits
            .check_concurrent_ops(limits.max_concurrent_ops - 1)
            .is_ok()
    );
    assert!(
        limits
            .check_concurrent_ops(limits.max_concurrent_ops)
            .is_err()
    );
    assert!(
        limits
            .check_event_data_buffered(limits.event_data_capacity - 1)
            .is_ok()
    );
    assert!(
        limits
            .check_event_data_buffered(limits.event_data_capacity)
            .is_err()
    );
    assert!(
        limits
            .check_event_control_buffered(limits.event_control_capacity - 1)
            .is_ok()
    );
    assert!(
        limits
            .check_event_control_buffered(limits.event_control_capacity)
            .is_err()
    );
    assert!(
        limits
            .check_run_elapsed(limits.run_duration - Duration::from_millis(1))
            .is_ok()
    );
    assert!(limits.check_run_elapsed(limits.run_duration).is_err());

    let mut zeroed = limits;
    zeroed.max_model_turns_per_run = 0;
    assert!(zeroed.validate().is_err(), "zero never means infinity");
}

#[test]
fn outcomes_surface_smoke() {
    assert_variants_distinct(
        "ExecutionStatus",
        &[
            ExecutionStatus::Succeeded,
            ExecutionStatus::Failed,
            ExecutionStatus::Denied,
            ExecutionStatus::Cancelled,
            ExecutionStatus::TimedOut,
        ],
    );
    assert_variants_distinct(
        "EffectState",
        &[
            EffectState::NotStarted,
            EffectState::KnownNotApplied,
            EffectState::KnownApplied,
            EffectState::Unknown,
        ],
    );
    assert_variants_distinct(
        "Evidence",
        &[
            Evidence::HostObserved,
            Evidence::PluginReported,
            Evidence::Uncertain,
        ],
    );
    assert_variants_distinct(
        "FinishReason",
        &[
            FinishReason::Stop,
            FinishReason::ToolCalls,
            FinishReason::OutputLimit,
            FinishReason::Refusal,
            FinishReason::Incomplete,
        ],
    );
    assert_variants_distinct(
        "RunOutcome",
        &[
            RunOutcome::Completed,
            RunOutcome::Refused,
            RunOutcome::Failed,
            RunOutcome::Cancelled,
            RunOutcome::LimitReached,
        ],
    );
    assert_variants_distinct(
        "PersistenceState",
        &[
            PersistenceState::Ephemeral,
            PersistenceState::Saved,
            PersistenceState::SaveFailed,
        ],
    );
    assert_variants_distinct(
        "CommandReply",
        &[
            CommandReply::Accepted,
            CommandReply::Rejected,
            CommandReply::AlreadyFinalized,
            CommandReply::StaleOrUnknownTarget,
            CommandReply::Busy,
        ],
    );

    let outcome = succeeded_outcome("ok");
    assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    assert_eq!(outcome.effect(), EffectState::KnownApplied);
    assert_eq!(outcome.evidence(), Evidence::HostObserved);
    assert_eq!(outcome.content(), "ok");
    assert!(!outcome.is_truncated());
    assert!(
        ToolOutcome::new(
            ExecutionStatus::Denied,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            "x",
            false,
        )
        .is_err(),
        "denied implies not-started"
    );

    let bounded = ToolOutcome::from_bounded_content(
        ExecutionStatus::Succeeded,
        EffectState::KnownApplied,
        Evidence::HostObserved,
        "x".repeat(Limits::M0_TEST_TOOL_OUTPUT_BYTES + 1),
        false,
    )
    .expect("bounded construction cuts to fit");
    assert_eq!(bounded.content().len(), Limits::M0_TEST_TOOL_OUTPUT_BYTES);
    assert!(bounded.is_truncated());

    let cut = succeeded_outcome("abcdef")
        .enforce_budget(3)
        .expect("finite budget accepts");
    assert_eq!(cut.content(), "abc");
    assert!(cut.is_truncated());
    assert!(succeeded_outcome("x").enforce_budget(0).is_err());

    let usage = Usage::new(Some(10), None, UsageFinality::Final);
    assert_eq!(usage.input_tokens(), Some(10));
    assert_eq!(usage.output_tokens(), None);
    assert_eq!(usage.finality(), UsageFinality::Final);
    assert_eq!(
        Usage::new(None, None, UsageFinality::Provisional).finality(),
        UsageFinality::Provisional
    );

    let finished =
        TurnFinished::try_new(FinishReason::Stop, usage, None).expect("final usage builds");
    assert_eq!(finished.reason(), FinishReason::Stop);
    assert_eq!(finished.usage(), usage);
    assert!(finished.continuation().is_none());
    assert!(finished.validate().is_ok());
    assert!(
        TurnFinished::try_new(
            FinishReason::Stop,
            Usage::new(None, None, UsageFinality::Provisional),
            None,
        )
        .is_err(),
        "a consumed turn requires final usage"
    );

    assert_eq!(
        InvocationOutcome::TurnFinished(finished).finish_reason(),
        Some(FinishReason::Stop)
    );
    assert_eq!(
        InvocationOutcome::Failed(safe_error()).finish_reason(),
        None
    );

    let record = RunFinished::new(RunOutcome::Completed, PersistenceState::Ephemeral, None)
        .expect("completed record builds");
    assert_eq!(record.outcome(), RunOutcome::Completed);
    assert_eq!(record.persistence(), PersistenceState::Ephemeral);
    assert!(record.persistence_error().is_none());
    assert!(record.error().is_none());
    let failed = record.with_error(safe_error());
    assert_eq!(failed.error(), Some(&safe_error()));
    assert!(RunFinished::new(RunOutcome::Failed, PersistenceState::SaveFailed, None).is_err());
    let saved_failure = RunFinished::new(
        RunOutcome::Failed,
        PersistenceState::SaveFailed,
        Some(safe_error()),
    )
    .expect("save failure with its error builds");
    assert_eq!(saved_failure.persistence_error(), Some(&safe_error()));
}

#[test]
fn command_surface_smoke() {
    let request = request_id();
    let session = session_id();
    let run = run_id();
    let call = call_id();
    let approval = approval_id();

    let submit = SubmitCommand::new(request.clone(), session.clone(), "hello", "default")
        .expect("valid submit builds");
    assert_eq!(submit.request, request);
    assert_eq!(submit.session, session);
    assert_eq!(submit.input, "hello");
    assert_eq!(submit.profile, "default");
    assert!(SubmitCommand::new(request_id(), session_id(), "", "default").is_err());
    assert!(SubmitCommand::new(request_id(), session_id(), "hello", "").is_err());

    let list = ListSessionsCommand::new(request.clone(), 10).expect("valid list builds");
    assert_eq!(list.request, request);
    assert_eq!(list.limit, 10);
    assert!(ListSessionsCommand::new(request.clone(), 0).is_err());
    assert!(ListSessionsCommand::new(request.clone(), MAX_LIST_LIMIT + 1).is_err());

    let commands = vec![
        Command::Submit(submit.clone()),
        Command::Cancel(CancelCommand {
            request: request.clone(),
            run: run.clone(),
        }),
        Command::Approve(ApproveCommand {
            request: request.clone(),
            approval: approval.clone(),
            run: run.clone(),
            call: call.clone(),
        }),
        Command::Deny(DenyCommand {
            request: request.clone(),
            approval: approval.clone(),
            run: run.clone(),
            call: call.clone(),
        }),
        Command::GetSnapshot(GetSnapshotCommand {
            request: request.clone(),
            run: run.clone(),
        }),
        Command::ListSessions(list),
        Command::RestoreSession(RestoreSessionCommand {
            request: request.clone(),
            session: session.clone(),
        }),
    ];
    assert_eq!(commands.len(), 7);
    for command in &commands {
        assert_eq!(command.request(), &request);
    }
    match &commands[0] {
        Command::Submit(command) => assert_eq!(command.input, "hello"),
        other => panic!("expected submit command, got {other:?}"),
    }
    match &commands[1] {
        Command::Cancel(command) => assert_eq!(command.run, run),
        other => panic!("expected cancel command, got {other:?}"),
    }
    match &commands[2] {
        Command::Approve(command) => {
            assert_eq!(command.approval, approval);
            assert_eq!(command.run, run);
            assert_eq!(command.call, call);
        }
        other => panic!("expected approve command, got {other:?}"),
    }
    match &commands[3] {
        Command::Deny(command) => {
            assert_eq!(command.approval, approval);
            assert_eq!(command.call, call);
        }
        other => panic!("expected deny command, got {other:?}"),
    }
    match &commands[4] {
        Command::GetSnapshot(command) => assert_eq!(command.run, run),
        other => panic!("expected snapshot command, got {other:?}"),
    }
    match &commands[5] {
        Command::ListSessions(command) => assert_eq!(command.limit, 10),
        other => panic!("expected list command, got {other:?}"),
    }
    match &commands[6] {
        Command::RestoreSession(command) => assert_eq!(command.session, session),
        other => panic!("expected restore command, got {other:?}"),
    }

    let response = CommandResponse::new(request.clone(), CommandReply::Accepted, Some(run.clone()));
    assert_eq!(response.request(), &request);
    assert_eq!(response.reply(), CommandReply::Accepted);
    assert_eq!(response.run(), Some(&run));

    let text = AssistantText::new(turn_id(), "item-1", "hi").expect("valid text fragment");
    assert_eq!(text.turn, turn_id());
    assert_eq!(text.item_key, "item-1");
    assert_eq!(text.text, "hi");
    assert!(AssistantText::new(turn_id(), "", "hi").is_err());

    let notice = ApprovalNotice::new(
        approval.clone(),
        call.clone(),
        "run host_write",
        "project scope",
        Duration::from_secs(120),
    )
    .expect("safe summary builds");
    assert_eq!(notice.approval, approval);
    assert_eq!(notice.call, call);
    assert_eq!(notice.summary, "run host_write");
    assert_eq!(notice.scope_summary, "project scope");
    assert_eq!(notice.expires_at_elapsed, Duration::from_secs(120));
    assert!(notice.args_preview().is_none());
    let notice = notice
        .with_args_preview(r#"{"path":"src"}"#)
        .expect("safe preview attaches");
    assert_eq!(notice.args_preview(), Some(r#"{"path":"src"}"#));
    notice.validate().expect("notice stays valid");

    let started = ToolStartedInfo {
        call: call.clone(),
        tool: nexus_core::ToolId::new("host_read", nexus_core::M0_REVISION).unwrap(),
        args_preview: None,
    };
    assert_eq!(started.call, call);
    let progress = ToolProgress::new(call.clone(), "working", false).expect("valid progress");
    assert_eq!(progress.call, call);
    assert_eq!(progress.preview, "working");
    assert!(!progress.truncated);
    assert!(
        ToolProgress::new(
            call.clone(),
            "x".repeat(Limits::M0_TEST_TOOL_OUTPUT_BYTES + 1),
            false,
        )
        .is_err()
    );

    let outcome = succeeded_outcome("ok");
    let finished = ToolFinishedInfo {
        call: call.clone(),
        outcome: outcome.clone(),
    };
    assert_eq!(finished.call, call);
    assert_eq!(finished.outcome, outcome);

    let payloads = vec![
        EventPayload::RunStarted {
            request: request.clone(),
        },
        EventPayload::AssistantTextDelta(text),
        EventPayload::ToolCallPreview {
            item_key: "item-1".to_owned(),
        },
        EventPayload::ApprovalRequired(notice),
        EventPayload::ToolStarted(started),
        EventPayload::ToolOutput(progress),
        EventPayload::ToolFinished(finished),
        EventPayload::UsageUpdated(Usage::new(Some(1), Some(2), UsageFinality::Final)),
        EventPayload::RunFinished(
            RunFinished::new(RunOutcome::Completed, PersistenceState::Ephemeral, None)
                .expect("terminal record builds"),
        ),
    ];
    assert_eq!(payloads.len(), 9);
    for payload in &payloads {
        payload.validate().expect("payload within bounds");
    }

    let event = RunEvent::new(session.clone(), run.clone(), 0, payloads[0].clone());
    assert_eq!(event.session(), &session);
    assert_eq!(event.run(), &run);
    assert_eq!(event.seq(), 0);
    assert!(!event.is_terminal());
    event.validate().expect("event payload valid");
    let terminal = RunEvent::new(
        session.clone(),
        run.clone(),
        checked_next_sequence(0).expect("sequence advances"),
        payloads[8].clone(),
    );
    assert!(terminal.is_terminal());
    assert_eq!(checked_next_sequence(EventSequence::MAX), None);
    let _: EventSequence = 1;

    let summary = OutcomeSummary {
        call: call_id(),
        status: ExecutionStatus::Succeeded,
        effect: EffectState::KnownApplied,
        evidence: Evidence::HostObserved,
    };
    assert_eq!(summary.status, ExecutionStatus::Succeeded);
    assert_eq!(summary.effect, EffectState::KnownApplied);
    assert_eq!(summary.evidence, Evidence::HostObserved);

    let snapshot = Snapshot::new(
        session.clone(),
        run.clone(),
        Some(0),
        RunLifecycle::Active,
        vec![approval_id()],
        vec![summary],
        false,
    )
    .expect("bounded snapshot builds");
    assert_eq!(snapshot.session(), &session);
    assert_eq!(snapshot.run(), &run);
    assert_eq!(snapshot.last_sequence(), Some(0));
    assert_eq!(snapshot.lifecycle(), RunLifecycle::Active);
    assert_eq!(snapshot.pending_approvals().len(), 1);
    assert_eq!(snapshot.pending_approvals()[0], approval_id());
    assert_eq!(snapshot.known_outcomes().len(), 1);
    assert!(!snapshot.is_content_truncated());
    assert_ne!(
        RunLifecycle::Active,
        RunLifecycle::Finalized(RunOutcome::Completed)
    );

    assert_eq!(MAX_INPUT_BYTES, 65_536);
    assert_eq!(MAX_SUMMARY_BYTES, 1024);
    assert_eq!(MAX_TEXT_FRAGMENT_BYTES, 65_536);
    assert_eq!(MAX_LIST_LIMIT, 1024);
}

struct SmokeProvider;

impl ProviderPort for SmokeProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            text: true,
            streaming: true,
            tool_calls: true,
            structured_output: false,
            usage_reporting: true,
            max_context_items: Some(128),
            max_output_bytes: Some(4096),
        }
    }

    fn stream(&self, request: &ModelRequest, context: &ProviderContext) -> Vec<ProviderEvent> {
        assert!(request.output_budget_bytes() > 0);
        assert!(context.check_active().is_ok());
        let usage = Usage::new(Some(3), Some(5), UsageFinality::Final);
        vec![
            ProviderEvent::TextDelta {
                item_key: "item-1".to_owned(),
                text: "hi".to_owned(),
            },
            ProviderEvent::ToolCallDelta {
                item_key: "item-2".to_owned(),
                assembled_bytes: 12,
            },
            ProviderEvent::Usage(usage),
            ProviderEvent::TurnFinished(
                TurnFinished::try_new(FinishReason::Stop, usage, None).expect("final usage builds"),
            ),
        ]
    }
}

#[test]
fn provider_surface_smoke() {
    let credential = CredentialRef::new("model-key-ref").expect("valid reference");
    assert_eq!(credential.as_str(), "model-key-ref");
    assert!(CredentialRef::new("").is_err());
    assert!(CredentialRef::new("x".repeat(MAX_CREDENTIAL_REF_LEN + 1)).is_err());

    let item_key = ItemKey::new("item-1").expect("valid item key");
    assert_eq!(item_key.as_str(), "item-1");
    let provider_ref = ProviderRef::new("prov-ref-1").expect("valid provider ref");
    assert_eq!(provider_ref.as_str(), "prov-ref-1");
    assert!(ItemKey::new("").is_err());
    assert!(ProviderRef::new("").is_err());

    let capabilities = SmokeProvider.capabilities();
    assert!(capabilities.text);
    assert!(capabilities.streaming);
    assert!(capabilities.tool_calls);
    assert!(!capabilities.structured_output);
    assert!(capabilities.usage_reporting);
    assert_eq!(capabilities.max_context_items, Some(128));
    assert_eq!(capabilities.max_output_bytes, Some(4096));

    let items = vec![
        ModelContextItem::user_text("hello").expect("valid user text"),
        ModelContextItem::assistant_text("item-1", "hi").expect("valid assistant text"),
        ModelContextItem::assistant_call("item-1", "prov-ref-1", tool_call())
            .expect("valid assistant call"),
        ModelContextItem::tool_result(
            call_id(),
            "item-1",
            "prov-ref-1",
            tool_id(),
            succeeded_outcome("ok"),
        )
        .expect("valid tool result"),
    ];
    for item in &items {
        assert!(item.payload_bytes() > 0);
    }

    let request = ModelRequest::new(run_id(), turn_id(), "default", vec![tool_id()], None, 1024)
        .expect("valid request builds");
    assert_eq!(request.run(), &run_id());
    assert_eq!(request.turn(), &turn_id());
    assert_eq!(request.profile(), "default");
    assert_eq!(request.enabled_tools(), std::slice::from_ref(&tool_id()));
    assert!(request.continuation().is_none());
    assert_eq!(request.output_budget_bytes(), 1024);
    let request = request
        .with_conversation(items)
        .expect("bounded conversation builds");
    assert_eq!(request.conversation().len(), 4);
    let request = request
        .with_tool_definitions(vec![tool_spec()])
        .expect("bounded definitions build");
    assert_eq!(request.tool_definitions().len(), 1);
    assert!(ModelRequest::new(run_id(), turn_id(), "default", vec![], None, 0).is_err());

    let context = ProviderContext::new(Duration::from_secs(30), false, Some(credential.clone()));
    assert_eq!(context.deadline_elapsed(), Duration::from_secs(30));
    assert!(context.deadline().is_none());
    assert!(!context.is_cancelled());
    assert_eq!(context.credential(), Some(&credential));
    assert!(context.check_active().is_ok());
    assert!(context.check_not_cancelled().is_ok());

    let token = CancellationToken::new();
    let live = ProviderContext::new(Duration::from_secs(30), false, None)
        .with_control(token.clone(), Instant::now() + Duration::from_secs(30));
    assert!(live.deadline().is_some());
    token.cancel();
    assert!(live.is_cancelled());
    assert!(live.check_active().is_err());
    assert!(live.check_not_cancelled().is_err());

    let events = SmokeProvider.stream(&request, &context);
    assert_eq!(events.len(), 4);
    assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
    assert!(!events[0].is_terminal());
    assert!(events[3].is_terminal());
    let ready = ProviderEvent::ToolCallReady(
        CallCandidate::new("item-1", "prov-ref-1", "host_read", r#"{"a":1}"#)
            .expect("valid candidate builds"),
    );
    assert!(!ready.is_terminal());
    assert!(ProviderEvent::Failed(safe_error()).is_terminal());

    assert_eq!(SmokeProvider.adapter_identity(), DEFAULT_ADAPTER_IDENTITY);
    assert_eq!(SmokeProvider.continuation_scope("profile-a"), "profile-a");
    assert_ne!(
        SmokeProvider.continuation_scope("profile-a"),
        SmokeProvider.continuation_scope("profile-b")
    );

    assert_eq!(
        MAX_CONVERSATION_ITEMS,
        Limits::M0_TEST_RETAINED_CONTEXT_ITEMS
    );
    assert_eq!(
        MAX_TOOL_DEFINITIONS,
        Limits::M0_TEST_TOOL_CALLS_PER_TURN as usize
    );
    assert_eq!(MAX_CONVERSATION_BYTES, 1_048_576);
    assert_eq!(MAX_TOOL_DEFINITION_BYTES, 2_097_152);
    assert_eq!(MAX_PROFILE_LEN, 128);
    assert_eq!(MAX_CREDENTIAL_REF_LEN, 128);
    assert!(!DEFAULT_ADAPTER_IDENTITY.is_empty());
}

struct SmokeStore {
    checkpoint: Option<SessionCheckpoint>,
    intents: usize,
    outcomes: usize,
}

impl SessionStore for SmokeStore {
    fn list_sessions(&self, limit: usize) -> Result<Vec<SessionMetadata>, AgentError> {
        Ok(self
            .checkpoint
            .iter()
            .take(limit)
            .map(|checkpoint| SessionMetadata {
                session: checkpoint.session().clone(),
                logical_revision: checkpoint.logical_revision(),
            })
            .collect())
    }

    fn load_session(&self, id: &SessionId) -> Result<SessionCheckpoint, AgentError> {
        match &self.checkpoint {
            Some(checkpoint) if checkpoint.session() == id => Ok(checkpoint.clone()),
            _ => Err(AgentError::new(
                ErrorCategory::StorageFailure,
                "session not found",
                RetryGuidance::DoNotRetry,
            )
            .expect("static safe diagnostic builds")),
        }
    }

    fn save_checkpoint(&mut self, checkpoint: &SessionCheckpoint) -> Result<(), AgentError> {
        self.checkpoint = Some(checkpoint.clone());
        Ok(())
    }

    fn record_intent(&mut self, _intent: &ToolIntentRecord) -> Result<(), AgentError> {
        self.intents += 1;
        Ok(())
    }

    fn record_outcome(&mut self, _outcome: &ToolOutcomeRecord) -> Result<(), AgentError> {
        self.outcomes += 1;
        Ok(())
    }
}

#[test]
fn store_surface_smoke() {
    let stored = StoredMessage {
        source: "user".to_owned(),
        text: "hello".to_owned(),
        complete: true,
    };
    let checkpoint = SessionCheckpoint::new(
        session_id(),
        STORE_FORMAT_REVISION,
        1,
        vec![stored.clone()],
        "default",
    )
    .expect("valid checkpoint builds");
    assert_eq!(checkpoint.session(), &session_id());
    assert_eq!(checkpoint.format_revision(), STORE_FORMAT_REVISION);
    assert_eq!(checkpoint.logical_revision(), 1);
    assert_eq!(checkpoint.messages(), std::slice::from_ref(&stored));
    assert_eq!(checkpoint.profile(), "default");
    assert!(check_format_revision(STORE_FORMAT_REVISION).is_ok());
    assert!(check_format_revision(STORE_FORMAT_REVISION + 1).is_err());
    check_checkpoint_bounds(&checkpoint).expect("bounded checkpoint passes the recheck");
    assert!(
        SessionCheckpoint::new(session_id(), STORE_FORMAT_REVISION, 1, Vec::new(), "").is_err()
    );

    let mut store = SmokeStore {
        checkpoint: None,
        intents: 0,
        outcomes: 0,
    };
    store.save_checkpoint(&checkpoint).expect("save succeeds");
    let listed = store.list_sessions(10).expect("list succeeds");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].session, session_id());
    assert_eq!(listed[0].logical_revision, 1);
    assert_eq!(
        store.load_session(&session_id()).expect("load succeeds"),
        checkpoint
    );
    assert!(
        store
            .load_session(&SessionId::new("other").expect("valid"))
            .is_err()
    );

    let intent = ToolIntentRecord {
        run: run_id(),
        call: call_id(),
        tool: tool_id(),
        args: normalized_args(),
        scope: ApprovedScope::new("project-read").expect("valid scope builds"),
    };
    assert_eq!(intent.run, run_id());
    assert_eq!(intent.call, call_id());
    assert_eq!(intent.tool, tool_id());
    store.record_intent(&intent).expect("intent records");
    assert_eq!(store.intents, 1);

    let outcome = ToolOutcomeRecord {
        run: run_id(),
        call: call_id(),
        status: ExecutionStatus::Succeeded,
        effect: EffectState::KnownApplied,
        evidence: Evidence::HostObserved,
        truncated: false,
    };
    assert_eq!(outcome.status, ExecutionStatus::Succeeded);
    assert_eq!(outcome.effect, EffectState::KnownApplied);
    assert_eq!(outcome.evidence, Evidence::HostObserved);
    assert!(!outcome.truncated);
    store.record_outcome(&outcome).expect("outcome records");
    assert_eq!(store.outcomes, 1);

    let metadata = SessionMetadata {
        session: session_id(),
        logical_revision: 1,
    };
    assert_eq!(metadata.session, session_id());
    assert_eq!(metadata.logical_revision, 1);

    assert_eq!(STORE_FORMAT_REVISION, M0_REVISION);
    assert_eq!(MAX_STORED_MESSAGES, Limits::M0_TEST_RETAINED_CONTEXT_ITEMS);
    assert_eq!(MAX_PROFILE_LEN, nexus_core::provider::MAX_PROFILE_LEN);
    const {
        assert!(MAX_SESSIONS > 0);
        assert!(MAX_INTENT_RECORDS > 0);
        assert!(MAX_OUTCOME_RECORDS > 0);
    }
}

struct SmokeTool;

impl ToolPort for SmokeTool {
    fn describe(&self) -> ToolSpec {
        tool_spec()
    }

    fn execute(&self, call: &ToolCall, context: &ToolContext) -> ToolOutcome {
        assert!(context.check_active().is_ok());
        assert_eq!(context.scope().as_str(), "project-read");
        succeeded_outcome(&format!("executed {}", call.call().as_str()))
    }
}

#[test]
fn tool_surface_smoke() {
    let spec = tool_spec();
    assert_eq!(spec.id(), &tool_id());
    assert_eq!(spec.description(), "read files");
    assert_eq!(spec.input_schema_json(), r#"{"type":"object"}"#);
    assert!(ToolSpec::new(tool_id(), "", "{}").is_err());
    assert!(ToolSpec::new(tool_id(), "read files", "[]").is_err());

    let scope = ApprovedScope::new("project-read").expect("valid scope builds");
    let context = ToolContext::new(1024, Duration::from_secs(30), false, scope.clone())
        .expect("valid context builds");
    assert_eq!(context.output_budget_bytes(), 1024);
    assert_eq!(context.deadline_elapsed(), Duration::from_secs(30));
    assert!(context.deadline().is_none());
    assert!(!context.is_cancelled());
    assert_eq!(context.scope(), &scope);
    assert!(context.check_active().is_ok());
    assert!(context.check_not_cancelled().is_ok());
    assert!(ToolContext::new(0, Duration::ZERO, false, scope.clone()).is_err());

    let token = CancellationToken::new();
    let live = ToolContext::new(1024, Duration::from_secs(30), false, scope)
        .expect("valid context builds")
        .with_control(token.clone(), Instant::now() + Duration::from_secs(30));
    assert!(live.deadline().is_some());
    token.cancel();
    assert!(live.is_cancelled());
    assert!(live.check_active().is_err());
    assert!(live.check_not_cancelled().is_err());

    let outcome = SmokeTool.execute(&tool_call(), &context);
    assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    assert_eq!(outcome.content(), "executed call-1");
    assert_eq!(SmokeTool.describe(), spec);

    assert_eq!(MAX_TOOL_DESCRIPTION_LEN, 1024);
    assert_eq!(MAX_SCHEMA_BYTES, 65_536);
}

#[test]
fn module_level_items_resolve_and_match_root_reexports() {
    assert_eq!(nexus_core::content::MAX_TOOL_NAME_LEN, 64);
    assert_eq!(nexus_core::ids::MAX_ID_LEN, 64);
    assert_eq!(nexus_core::approval::MAX_SCOPE_BYTES, 1024);
    assert_eq!(nexus_core::error::MAX_MESSAGE_LEN, 1024);
    assert_eq!(nexus_core::error::MAX_CORRELATION_ENTRIES, 8);
    assert_eq!(nexus_core::error::MAX_CORRELATION_KEY_LEN, 64);
    assert_eq!(nexus_core::error::MAX_CORRELATION_VALUE_LEN, 256);
    assert_eq!(nexus_core::commands::MAX_INPUT_BYTES, 65_536);
    assert_eq!(nexus_core::commands::MAX_SUMMARY_BYTES, 1024);
    assert_eq!(nexus_core::commands::MAX_TEXT_FRAGMENT_BYTES, 65_536);
    assert_eq!(nexus_core::commands::MAX_LIST_LIMIT, 1024);
    assert_eq!(
        nexus_core::store::MAX_STORED_MESSAGES,
        Limits::M0_TEST_RETAINED_CONTEXT_ITEMS
    );
    assert_eq!(
        nexus_core::store::MAX_PROFILE_LEN,
        nexus_core::provider::MAX_PROFILE_LEN
    );
    const {
        assert!(nexus_core::store::MAX_SESSIONS > 0);
        assert!(nexus_core::store::MAX_INTENT_RECORDS > 0);
        assert!(nexus_core::store::MAX_OUTCOME_RECORDS > 0);
    }
    assert_eq!(nexus_core::provider::MAX_CREDENTIAL_REF_LEN, 128);
    assert_eq!(nexus_core::provider::MAX_PROFILE_LEN, 128);
    assert_eq!(
        nexus_core::provider::MAX_CONVERSATION_ITEMS,
        Limits::M0_TEST_RETAINED_CONTEXT_ITEMS
    );
    assert_eq!(nexus_core::provider::MAX_CONVERSATION_BYTES, 1_048_576);
    assert_eq!(
        nexus_core::provider::MAX_TOOL_DEFINITIONS,
        Limits::M0_TEST_TOOL_CALLS_PER_TURN as usize
    );
    assert_eq!(nexus_core::provider::MAX_TOOL_DEFINITION_BYTES, 2_097_152);
    assert_eq!(nexus_core::tool::MAX_TOOL_DESCRIPTION_LEN, 1024);
    assert_eq!(nexus_core::tool::MAX_SCHEMA_BYTES, 65_536);

    assert_eq!(nexus_core::commands::checked_next_sequence(41), Some(42));
    assert_eq!(
        nexus_core::commands::checked_next_sequence(EventSequence::MAX),
        None
    );
    let _: EventSequence = 7;

    assert!(nexus_core::store::check_format_revision(STORE_FORMAT_REVISION).is_ok());
    let stored = StoredMessage {
        source: "user".to_owned(),
        text: "hi".to_owned(),
        complete: true,
    };
    let checkpoint = SessionCheckpoint::new(
        session_id(),
        STORE_FORMAT_REVISION,
        1,
        vec![stored],
        "profile",
    )
    .expect("valid checkpoint builds");
    nexus_core::store::check_checkpoint_bounds(&checkpoint).expect("bounded checkpoint");

    // Root re-exports must alias their defining module items.
    assert_eq!(M0_REVISION, nexus_core::ids::M0_REVISION);
    assert_eq!(
        STORE_FORMAT_REVISION,
        nexus_core::store::STORE_FORMAT_REVISION
    );
    assert_eq!(MAX_ITEM_KEY_LEN, nexus_core::content::MAX_ITEM_KEY_LEN);
    assert_eq!(
        MAX_CONTINUATION_BYTES,
        nexus_core::content::MAX_CONTINUATION_BYTES
    );
    assert_eq!(
        MAX_PROVIDER_REF_LEN,
        nexus_core::content::MAX_PROVIDER_REF_LEN
    );
    assert_eq!(
        MAX_CONVERSATION_ITEMS,
        nexus_core::provider::MAX_CONVERSATION_ITEMS
    );
    assert_eq!(
        MAX_CONVERSATION_BYTES,
        nexus_core::provider::MAX_CONVERSATION_BYTES
    );
    assert_eq!(
        MAX_TOOL_DEFINITIONS,
        nexus_core::provider::MAX_TOOL_DEFINITIONS
    );
    assert_eq!(
        MAX_TOOL_DEFINITION_BYTES,
        nexus_core::provider::MAX_TOOL_DEFINITION_BYTES
    );
    assert_eq!(
        DEFAULT_ADAPTER_IDENTITY,
        nexus_core::provider::DEFAULT_ADAPTER_IDENTITY
    );
}
