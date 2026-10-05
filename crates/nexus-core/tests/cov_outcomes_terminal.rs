#![forbid(unsafe_code)]

//! Terminal outcome coverage for `TurnFinished`, `RunFinished`, and `Usage`.
//!
//! Public-API integration checks pinning the documented terminal invariants:
//! a consumed turn rejects provisional usage, missing counters stay unknown
//! and are never fabricated as zero, typed execution errors survive terminal
//! records, and a persistence failure is reported beside the lifecycle
//! outcome without replacing it. Deterministic: no clocks, I/O, or threads.

use nexus_core::{
    AgentError, ContinuationData, ErrorCategory, FinishReason, InvocationOutcome, PersistenceState,
    RetryGuidance, RunFinished, RunOutcome, TurnFinished, Usage, UsageFinality,
};

fn typed_error(category: ErrorCategory, retry: RetryGuidance, message: &'static str) -> AgentError {
    AgentError::new(category, message, retry).expect("static safe diagnostic builds")
}

fn timeout_error() -> AgentError {
    typed_error(
        ErrorCategory::Timeout,
        RetryGuidance::DoNotRetry,
        "tool deadline exceeded",
    )
}

#[test]
fn provisional_terminal_usage_is_rejected_for_every_finish_reason() {
    for reason in [
        FinishReason::Stop,
        FinishReason::ToolCalls,
        FinishReason::OutputLimit,
        FinishReason::Refusal,
        FinishReason::Incomplete,
    ] {
        let provisional = Usage::new(Some(10), Some(5), UsageFinality::Provisional);
        let error = TurnFinished::try_new(reason, provisional, None)
            .expect_err("a consumed turn must reject provisional usage");
        assert_eq!(error.category(), ErrorCategory::InvalidInput);
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        assert_eq!(error.message(), "finished turn requires final usage");

        // The unchecked compatibility constructor still exposes the same
        // violation to `validate`, so host boundaries can detect it.
        let compatibility = TurnFinished::new(reason, provisional, None);
        let error = compatibility
            .validate()
            .expect_err("legacy construction stays detectable");
        assert_eq!(error.category(), ErrorCategory::InvalidInput);
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        assert_eq!(error.message(), "finished turn requires final usage");
        assert_eq!(compatibility.reason(), reason);
        assert_eq!(compatibility.usage(), provisional);
        assert_eq!(compatibility.usage().finality(), UsageFinality::Provisional);
    }
}

#[test]
fn final_usage_is_accepted_and_continuation_survives_validation() {
    let usage = Usage::new(Some(10), Some(8), UsageFinality::Final);
    let continuation =
        ContinuationData::new("acme-adapter", "model-x", vec![1, 2, 3]).expect("valid builds");
    let finished = TurnFinished::try_new(FinishReason::Stop, usage, Some(continuation.clone()))
        .expect("final usage builds");
    assert!(finished.validate().is_ok());
    assert_eq!(finished.reason(), FinishReason::Stop);
    assert_eq!(finished.usage(), usage);
    assert_eq!(finished.continuation(), Some(&continuation));
    assert_eq!(
        InvocationOutcome::TurnFinished(finished).finish_reason(),
        Some(FinishReason::Stop)
    );

    let compatibility = TurnFinished::new(FinishReason::ToolCalls, usage, None);
    assert!(compatibility.validate().is_ok());
}

#[test]
fn missing_usage_stays_unknown_never_zero() {
    let unknown = Usage::new(None, None, UsageFinality::Final);
    assert_eq!(unknown.input_tokens(), None);
    assert_eq!(unknown.output_tokens(), None);
    assert_eq!(unknown.finality(), UsageFinality::Final);

    let input_only = Usage::new(Some(12), None, UsageFinality::Final);
    assert_eq!(input_only.input_tokens(), Some(12));
    assert_eq!(
        input_only.output_tokens(),
        None,
        "a missing output counter is unknown, not zero"
    );
    assert_ne!(input_only.output_tokens(), Some(0));

    let output_only = Usage::new(None, Some(7), UsageFinality::Final);
    assert_eq!(
        output_only.input_tokens(),
        None,
        "a missing input counter is unknown, not zero"
    );
    assert_ne!(output_only.input_tokens(), Some(0));
    assert_eq!(output_only.output_tokens(), Some(7));

    // An explicitly reported zero is a known value and stays distinct from
    // an unknown counter.
    let reported_zero = Usage::new(Some(0), Some(0), UsageFinality::Final);
    assert_eq!(reported_zero.input_tokens(), Some(0));
    assert_eq!(reported_zero.output_tokens(), Some(0));
    assert_ne!(unknown, reported_zero);
    assert_ne!(unknown.input_tokens(), reported_zero.input_tokens());

    // Unknown counters survive the terminal turn unchanged.
    let finished = TurnFinished::try_new(FinishReason::Stop, unknown, None)
        .expect("final finality with unknown counters builds");
    assert_eq!(finished.usage().input_tokens(), None);
    assert_eq!(finished.usage().output_tokens(), None);
}

#[test]
fn failed_invocation_outcome_exposes_no_finish_reason_and_keeps_its_error() {
    let typed = timeout_error();
    let outcome = InvocationOutcome::Failed(typed.clone());
    assert_eq!(outcome.finish_reason(), None);
    assert_eq!(outcome, InvocationOutcome::Failed(typed));
}

#[test]
fn run_finished_retains_typed_execution_error() {
    let typed = typed_error(
        ErrorCategory::ToolFailure,
        RetryGuidance::SafeToRetry,
        "tool executed and reported failure",
    );
    let finished = RunFinished::new(RunOutcome::Failed, PersistenceState::Ephemeral, None)
        .expect("failed record builds")
        .with_error(typed.clone());
    assert_eq!(finished.outcome(), RunOutcome::Failed);
    assert_eq!(finished.persistence(), PersistenceState::Ephemeral);
    assert_eq!(finished.error(), Some(&typed));
    assert_eq!(
        finished.error().map(AgentError::category),
        Some(ErrorCategory::ToolFailure)
    );
    assert_eq!(
        finished.error().map(AgentError::retry),
        Some(RetryGuidance::SafeToRetry)
    );
    assert_eq!(
        finished.error().map(AgentError::message),
        Some("tool executed and reported failure")
    );
    assert!(finished.persistence_error().is_none());

    let cloned = finished.clone();
    assert_eq!(cloned, finished, "the typed error survives a clone");
    assert_eq!(cloned.error(), Some(&typed));

    // A different typed error is never conflated with the retained one.
    let other = timeout_error();
    assert_ne!(
        finished,
        RunFinished::new(RunOutcome::Failed, PersistenceState::Ephemeral, None)
            .expect("failed record builds")
            .with_error(other)
    );

    // No execution error is fabricated for a normal completion.
    let completed = RunFinished::new(RunOutcome::Completed, PersistenceState::Ephemeral, None)
        .expect("completed record builds");
    assert!(completed.error().is_none());
    assert!(completed.persistence_error().is_none());
}

#[test]
fn non_completed_run_outcomes_retain_their_execution_error() {
    let typed = typed_error(
        ErrorCategory::Cancelled,
        RetryGuidance::DoNotRetry,
        "run cancelled",
    );
    for outcome in [
        RunOutcome::Failed,
        RunOutcome::Refused,
        RunOutcome::Cancelled,
        RunOutcome::LimitReached,
    ] {
        let record = RunFinished::new(outcome, PersistenceState::Ephemeral, None)
            .expect("record builds")
            .with_error(typed.clone());
        assert_eq!(record.outcome(), outcome);
        assert_eq!(record.error(), Some(&typed));
    }
}

#[test]
fn save_failure_is_reported_beside_the_outcome_without_replacing_it() {
    let storage = typed_error(
        ErrorCategory::StorageFailure,
        RetryGuidance::DoNotRetry,
        "session store unavailable",
    );
    let finished = RunFinished::new(
        RunOutcome::Failed,
        PersistenceState::SaveFailed,
        Some(storage.clone()),
    )
    .expect("save failure with its error builds");
    assert_eq!(
        finished.outcome(),
        RunOutcome::Failed,
        "a save failure never rewrites the lifecycle outcome"
    );
    assert_eq!(finished.persistence(), PersistenceState::SaveFailed);
    assert_eq!(finished.persistence_error(), Some(&storage));
    assert!(
        finished.error().is_none(),
        "the persistence failure is not the execution failure"
    );

    // The execution error attaches additively; both remain readable.
    let execution = typed_error(
        ErrorCategory::ToolFailure,
        RetryGuidance::SafeToRetry,
        "tool executed and reported failure",
    );
    let both = finished.clone().with_error(execution.clone());
    assert_eq!(both.outcome(), RunOutcome::Failed);
    assert_eq!(both.persistence(), PersistenceState::SaveFailed);
    assert_eq!(both.persistence_error(), Some(&storage));
    assert_eq!(both.error(), Some(&execution));
    assert_ne!(both.persistence_error(), both.error());

    // A save failure reported for a completed lifecycle keeps that outcome.
    let completed_unsaved = RunFinished::new(
        RunOutcome::Completed,
        PersistenceState::SaveFailed,
        Some(storage.clone()),
    )
    .expect("completed record with a failed save builds");
    assert_eq!(completed_unsaved.outcome(), RunOutcome::Completed);
    assert_eq!(
        completed_unsaved.persistence(),
        PersistenceState::SaveFailed
    );
    assert_eq!(completed_unsaved.persistence_error(), Some(&storage));

    // Ephemeral and saved records carry no persistence failure.
    for persistence in [PersistenceState::Ephemeral, PersistenceState::Saved] {
        let record = RunFinished::new(RunOutcome::Completed, persistence, None)
            .expect("record without a persistence error builds");
        assert_eq!(record.persistence(), persistence);
        assert!(record.persistence_error().is_none());
    }
}

#[test]
fn persistence_error_and_save_failure_must_agree() {
    let storage = typed_error(
        ErrorCategory::StorageFailure,
        RetryGuidance::DoNotRetry,
        "session store unavailable",
    );

    let missing = RunFinished::new(RunOutcome::Completed, PersistenceState::SaveFailed, None)
        .expect_err("a save failure without its error is rejected");
    assert_eq!(missing.category(), ErrorCategory::InvalidInput);
    assert_eq!(missing.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(missing.message(), "save failure requires its error");

    for persistence in [PersistenceState::Ephemeral, PersistenceState::Saved] {
        let stray = RunFinished::new(RunOutcome::Completed, persistence, Some(storage.clone()))
            .expect_err("a persistence error without a save failure is rejected");
        assert_eq!(stray.category(), ErrorCategory::InvalidInput);
        assert_eq!(stray.retry(), RetryGuidance::DoNotRetry);
        assert_eq!(stray.message(), "persistence error without save failure");
    }
}

#[test]
fn terminal_record_equality_distinguishes_outcome_persistence_and_errors() {
    let base = || {
        RunFinished::new(RunOutcome::Completed, PersistenceState::Saved, None)
            .expect("completed saved record builds")
    };
    assert_eq!(base(), base());

    let refused = RunFinished::new(RunOutcome::Refused, PersistenceState::Saved, None)
        .expect("refused saved record builds");
    assert_ne!(base(), refused);

    let ephemeral = RunFinished::new(RunOutcome::Completed, PersistenceState::Ephemeral, None)
        .expect("completed ephemeral record builds");
    assert_ne!(base(), ephemeral);

    let with_error = base().with_error(timeout_error());
    assert_ne!(base(), with_error);

    let unsaved = RunFinished::new(
        RunOutcome::Completed,
        PersistenceState::SaveFailed,
        Some(timeout_error()),
    )
    .expect("completed save-failed record builds");
    assert_ne!(base(), unsaved);
    assert_ne!(with_error, unsaved);
}
