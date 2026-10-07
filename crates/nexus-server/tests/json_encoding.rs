#![forbid(unsafe_code)]

//! Coverage for every `nexus_server::json` encoder.
//!
//! Each JSON shape is built from `nexus_core` values directly, so these
//! tests are pure and deterministic: no runtime, socket, clock, or thread is
//! involved, and repeated encodes of the same input are identical. Encoded
//! enum names are asserted against literals because the wire names are the
//! Rust `Debug` forms, which the module documents as unstable. Whole values
//! are compared so an added, renamed, or dropped field fails the test.

use std::time::Duration;

use nexus_core::{
    AgentError, ApprovalId, ApprovalNotice, AssistantText, CallId, CommandReply, CorrelationData,
    EffectState, ErrorCategory, EventPayload, Evidence, ExecutionStatus, Limits, OutcomeSummary,
    PersistenceState, RequestId, RetryGuidance, RunEvent, RunFinished, RunId, RunLifecycle,
    RunOutcome, SessionId, Snapshot, ToolFinishedInfo, ToolOutcome, ToolProgress, ToolStartedInfo,
    TurnId, Usage, UsageFinality,
};
use nexus_server::json::{
    error_body, error_json, event_json, finished_json, health_json, notice_json, outcome_json,
    outcome_name, outcome_summary_json, reply_name, reply_status, snapshot_json, usage_json,
};
use serde_json::{Value, json};

/// Number of object fields an encoded value carries; an unexpected extra or
/// missing field is an observable wire change.
fn field_count(value: &Value) -> usize {
    value
        .as_object()
        .expect("encoder emits a JSON object")
        .len()
}

fn field_names(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .expect("encoder emits a JSON object")
        .keys()
        .map(String::as_str)
        .collect()
}

fn session() -> SessionId {
    SessionId::new("sess-1").expect("valid session id")
}

fn run() -> RunId {
    RunId::new("run-1").expect("valid run id")
}

fn call(name: &str) -> CallId {
    CallId::new(name).expect("valid call id")
}

fn approval(name: &str) -> ApprovalId {
    ApprovalId::new(name).expect("valid approval id")
}

fn request() -> RequestId {
    RequestId::new("req-1").expect("valid request id")
}

fn turn() -> TurnId {
    TurnId::new("turn-1").expect("valid turn id")
}

/// Wraps a payload in a run envelope owned by [`run`].
fn event(seq: u64, payload: EventPayload) -> RunEvent {
    RunEvent::new(session(), run(), seq, payload)
}

fn typed_error(category: ErrorCategory, message: &str, retry: RetryGuidance) -> AgentError {
    AgentError::new(category, message, retry).expect("static safe message builds")
}

/// Terminal record with ephemeral persistence and no execution error.
fn terminal(outcome: RunOutcome) -> RunFinished {
    RunFinished::new(outcome, PersistenceState::Ephemeral, None).expect("terminal record builds")
}

fn notice() -> ApprovalNotice {
    ApprovalNotice::new(
        approval("appr-1"),
        call("call-1"),
        "run tool host_write",
        "project scope",
        Duration::from_secs(120),
    )
    .expect("caller-redacted notice builds")
}

fn tool_outcome(
    status: ExecutionStatus,
    effect: EffectState,
    evidence: Evidence,
    content: &str,
    truncated: bool,
) -> ToolOutcome {
    ToolOutcome::new(status, effect, evidence, content, truncated).expect("outcome builds")
}

/// Every run outcome with its lowercase wire name.
fn run_outcomes() -> [(RunOutcome, &'static str); 5] {
    [
        (RunOutcome::Completed, "completed"),
        (RunOutcome::Refused, "refused"),
        (RunOutcome::Failed, "failed"),
        (RunOutcome::Cancelled, "cancelled"),
        (RunOutcome::LimitReached, "limit-reached"),
    ]
}

/// Every command reply with its wire name and HTTP status.
fn replies() -> [(CommandReply, &'static str, u16); 5] {
    [
        (CommandReply::Accepted, "accepted", 200),
        (CommandReply::Rejected, "rejected", 400),
        (CommandReply::AlreadyFinalized, "already-finalized", 409),
        (
            CommandReply::StaleOrUnknownTarget,
            "stale-or-unknown-target",
            404,
        ),
        (CommandReply::Busy, "busy", 409),
    ]
}

/// Every execution status with its `Debug` wire name.
fn statuses() -> [(ExecutionStatus, &'static str); 5] {
    [
        (ExecutionStatus::Succeeded, "Succeeded"),
        (ExecutionStatus::Failed, "Failed"),
        (ExecutionStatus::Denied, "Denied"),
        (ExecutionStatus::Cancelled, "Cancelled"),
        (ExecutionStatus::TimedOut, "TimedOut"),
    ]
}

/// Every effect state with its `Debug` wire name.
fn effects() -> [(EffectState, &'static str); 4] {
    [
        (EffectState::NotStarted, "NotStarted"),
        (EffectState::KnownNotApplied, "KnownNotApplied"),
        (EffectState::KnownApplied, "KnownApplied"),
        (EffectState::Unknown, "Unknown"),
    ]
}

/// Every evidence class with its `Debug` wire name.
fn evidence_classes() -> [(Evidence, &'static str); 3] {
    [
        (Evidence::HostObserved, "HostObserved"),
        (Evidence::PluginReported, "PluginReported"),
        (Evidence::Uncertain, "Uncertain"),
    ]
}

/// Every persistence state with its `Debug` wire name.
fn persistence_states() -> [(PersistenceState, &'static str); 3] {
    [
        (PersistenceState::Ephemeral, "Ephemeral"),
        (PersistenceState::Saved, "Saved"),
        (PersistenceState::SaveFailed, "SaveFailed"),
    ]
}

/// Every error category with its `Debug` wire name.
fn categories() -> [(ErrorCategory, &'static str); 13] {
    [
        (ErrorCategory::InvalidInput, "InvalidInput"),
        (
            ErrorCategory::UnsupportedCapability,
            "UnsupportedCapability",
        ),
        (ErrorCategory::Authentication, "Authentication"),
        (ErrorCategory::PermissionDenied, "PermissionDenied"),
        (ErrorCategory::RateLimited, "RateLimited"),
        (ErrorCategory::Protocol, "Protocol"),
        (ErrorCategory::Timeout, "Timeout"),
        (ErrorCategory::Cancelled, "Cancelled"),
        (ErrorCategory::ResourceLimit, "ResourceLimit"),
        (ErrorCategory::ToolFailure, "ToolFailure"),
        (ErrorCategory::StorageFailure, "StorageFailure"),
        (ErrorCategory::UncertainOutcome, "UncertainOutcome"),
        (ErrorCategory::Internal, "Internal"),
    ]
}

/// Every retry guidance with its `Debug` wire name.
fn retries() -> [(RetryGuidance, &'static str); 3] {
    [
        (RetryGuidance::DoNotRetry, "DoNotRetry"),
        (RetryGuidance::RetryAfterBackoff, "RetryAfterBackoff"),
        (RetryGuidance::SafeToRetry, "SafeToRetry"),
    ]
}

/// The `false` entries a snapshot summary never carries: a denial claims no
/// effects, so the core constructor rejects any other effect state.
fn is_admissible(status: ExecutionStatus, effect: EffectState) -> bool {
    status != ExecutionStatus::Denied || effect == EffectState::NotStarted
}

#[test]
fn health_json_reports_ok_with_the_test_only_scope() {
    assert_eq!(
        health_json(),
        json!({ "status": "ok", "scope": "loopback test-only demo" })
    );
    assert_eq!(field_count(&health_json()), 2);
}

#[test]
fn outcome_name_maps_every_run_outcome_to_a_distinct_lowercase_wire_name() {
    let mut names: Vec<&'static str> = Vec::new();
    for (outcome, name) in run_outcomes() {
        assert_eq!(outcome_name(outcome), name, "outcome {outcome:?}");
        assert!(
            name.chars().all(|ch| ch.is_ascii_lowercase() || ch == '-'),
            "wire name {name:?} stays lowercase"
        );
        assert!(
            !names.contains(&name),
            "{name:?} must not be reused by another outcome"
        );
        names.push(name);
    }
    assert_eq!(names.len(), 5, "all five outcomes are named");
}

#[test]
fn reply_names_and_statuses_cover_every_reply() {
    let mut names: Vec<&'static str> = Vec::new();
    let mut statuses: Vec<u16> = Vec::new();
    for (reply, name, status) in replies() {
        assert_eq!(reply_name(reply), name, "reply {reply:?}");
        assert_eq!(reply_status(reply), status, "reply {reply:?}");
        assert!(
            !names.contains(&name),
            "{name:?} must not be reused by another reply"
        );
        names.push(name);
        statuses.push(status);
    }
    assert_eq!(names.len(), 5, "all five replies are named");
    statuses.sort_unstable();
    assert_eq!(
        statuses,
        vec![200, 400, 404, 409, 409],
        "accepted, rejected, unknown target, then the two conflict replies"
    );
}

#[test]
fn reply_statuses_agree_with_the_documented_reply_classes() {
    assert_eq!(reply_status(CommandReply::Accepted), 200);
    assert_eq!(reply_status(CommandReply::Rejected), 400);
    assert_eq!(reply_status(CommandReply::StaleOrUnknownTarget), 404);
    assert_eq!(
        reply_status(CommandReply::Busy),
        reply_status(CommandReply::AlreadyFinalized),
        "busy and already-finalized are both conflicts"
    );
}

#[test]
fn error_json_encodes_category_retry_and_message_for_every_typed_error() {
    for (category, category_name) in categories() {
        for (retry, retry_name) in retries() {
            let error = typed_error(category, "static diagnostic", retry);
            assert_eq!(
                error_json(&error),
                json!({
                    "category": category_name,
                    "retry": retry_name,
                    "message": "static diagnostic",
                }),
                "category {category:?} with retry {retry:?}"
            );
        }
    }
}

#[test]
fn error_json_uses_debug_names_not_the_kebab_display_forms() {
    // The module documents the wire names as the Rust `Debug` forms; the
    // kebab `as_str` spellings are a separate core vocabulary and must not
    // silently replace them on the wire.
    let error = typed_error(
        ErrorCategory::Timeout,
        "tool deadline exceeded",
        RetryGuidance::DoNotRetry,
    );
    let encoded = error_json(&error);
    assert_eq!(encoded["category"], "Timeout");
    assert_ne!(encoded["category"], "timeout");
    assert_eq!(encoded["retry"], "DoNotRetry");
    assert_ne!(encoded["retry"], "do-not-retry");
    assert_eq!(encoded["message"], "tool deadline exceeded");
    assert_eq!(field_count(&encoded), 3, "exactly category, retry, message");
}

#[test]
fn error_json_carries_the_message_and_omits_correlation_data() {
    let mut correlation = CorrelationData::new();
    correlation
        .push("run", "run-1")
        .expect("bounded correlation entry builds");
    let error = AgentError::with_correlation(
        ErrorCategory::UncertainOutcome,
        "effects are inconclusive",
        correlation,
        RetryGuidance::DoNotRetry,
    )
    .expect("static safe message builds");
    let encoded = error_json(&error);
    assert_eq!(
        encoded,
        json!({
            "category": "UncertainOutcome",
            "retry": "DoNotRetry",
            "message": "effects are inconclusive",
        })
    );
    assert_eq!(field_count(&encoded), 3, "correlation stays host-local");
    assert!(
        encoded.get("correlation").is_none(),
        "bounded correlation data never reaches the wire"
    );
}

#[test]
fn usage_json_keeps_unknown_counters_null_never_zero() {
    let counter_pairs = [
        (None, None),
        (Some(0), None),
        (None, Some(0)),
        (Some(7), Some(9)),
        (Some(u64::MAX), Some(u64::MAX)),
    ];
    let finalities = [
        (UsageFinality::Provisional, "Provisional"),
        (UsageFinality::Final, "Final"),
    ];
    let mut encoded_count = 0usize;
    for (input, output) in counter_pairs {
        for (finality, finality_name) in finalities {
            let encoded = usage_json(&Usage::new(input, output, finality));
            assert_eq!(
                encoded,
                json!({ "input": input, "output": output, "finality": finality_name }),
                "input {input:?} output {output:?} finality {finality_name}"
            );
            if input.is_none() {
                assert!(
                    encoded["input"].is_null(),
                    "unknown input tokens stay null, never 0"
                );
            } else {
                assert_eq!(encoded["input"].as_u64(), input, "known input is exact");
            }
            if output.is_none() {
                assert!(
                    encoded["output"].is_null(),
                    "unknown output tokens stay null, never 0"
                );
            } else {
                assert_eq!(encoded["output"].as_u64(), output, "known output is exact");
            }
            assert_eq!(field_count(&encoded), 3, "exactly input, output, finality");
            encoded_count += 1;
        }
    }
    assert_eq!(encoded_count, 10, "every counter combination is encoded");
}

#[test]
fn usage_json_distinguishes_a_reported_zero_from_an_unknown_counter() {
    let reported_zero = usage_json(&Usage::new(Some(0), Some(0), UsageFinality::Final));
    assert_eq!(
        reported_zero["input"], 0,
        "a reported zero is a real counter"
    );
    assert_eq!(
        reported_zero["output"], 0,
        "a reported zero is a real counter"
    );
    let unknown = usage_json(&Usage::new(None, None, UsageFinality::Final));
    assert!(
        unknown["input"].is_null() && unknown["output"].is_null(),
        "unknown counters are never fabricated as zero"
    );
    assert_ne!(reported_zero, unknown);
}

#[test]
fn outcome_json_encodes_every_admissible_status_effect_and_evidence_combination() {
    let mut encoded_count = 0usize;
    for (status, status_name) in statuses() {
        for (effect, effect_name) in effects() {
            for (evidence, evidence_name) in evidence_classes() {
                if !is_admissible(status, effect) {
                    assert!(
                        ToolOutcome::new(status, effect, evidence, "content", false).is_err(),
                        "a denial must never claim applied effects: {status:?}/{effect:?}"
                    );
                    continue;
                }
                let truncated = encoded_count.is_multiple_of(2);
                let content = format!("outcome-{encoded_count}");
                let outcome = tool_outcome(status, effect, evidence, &content, truncated);
                assert_eq!(
                    outcome_json(&outcome),
                    json!({
                        "status": status_name,
                        "effect": effect_name,
                        "evidence": evidence_name,
                        "content": content,
                        "truncated": truncated,
                    }),
                    "status {status:?} effect {effect:?} evidence {evidence:?}"
                );
                encoded_count += 1;
            }
        }
    }
    // 5 statuses x 4 effects x 3 evidence classes = 60, less the 9 denied
    // combinations that the core constructor rejects outright.
    assert_eq!(encoded_count, 51, "every admissible combination is encoded");
}

#[test]
fn outcome_json_preserves_the_truncation_flag_in_both_states() {
    for truncated in [false, true] {
        let outcome = tool_outcome(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            "full content",
            truncated,
        );
        let encoded = outcome_json(&outcome);
        assert_eq!(encoded["truncated"], truncated, "flag is passed through");
        assert_eq!(encoded["content"], "full content", "content is verbatim");
        assert_eq!(field_count(&encoded), 5);
    }
}

#[test]
fn outcome_json_keeps_the_flag_set_when_content_was_actually_cut() {
    let over_budget = "x".repeat(Limits::M0_TEST_TOOL_OUTPUT_BYTES + 1);
    let cut = ToolOutcome::from_bounded_content(
        ExecutionStatus::Succeeded,
        EffectState::KnownApplied,
        Evidence::HostObserved,
        over_budget,
        false,
    )
    .expect("bounded construction cuts instead of failing");
    let encoded = outcome_json(&cut);
    assert_eq!(encoded["truncated"], true, "an actual cut sets the flag");
    assert_eq!(
        encoded["content"].as_str().map(str::len),
        Some(Limits::M0_TEST_TOOL_OUTPUT_BYTES),
        "encoded content stays within the shared budget"
    );
}

#[test]
fn outcome_json_escapes_untrusted_tool_content() {
    let raw = "said \"done\" <ok> \\ backslash";
    let outcome = tool_outcome(
        ExecutionStatus::Failed,
        EffectState::Unknown,
        Evidence::Uncertain,
        raw,
        false,
    );
    let encoded = outcome_json(&outcome);
    assert_eq!(encoded["content"], raw, "the decoded value round-trips");
    let text = serde_json::to_string(&encoded).expect("value re-encodes");
    assert!(
        text.contains(r#"\"done\""#) && text.contains(r"\\"),
        "quotes and backslashes are escaped on the wire: {text}"
    );
}

#[test]
fn finished_json_encodes_every_outcome_with_its_persistence_state() {
    for (outcome, outcome_wire) in run_outcomes() {
        for (persistence, persistence_wire) in persistence_states() {
            let persistence_error = (persistence == PersistenceState::SaveFailed).then(|| {
                typed_error(
                    ErrorCategory::StorageFailure,
                    "session record could not be saved",
                    RetryGuidance::SafeToRetry,
                )
            });
            let finished = RunFinished::new(outcome, persistence, persistence_error)
                .expect("consistent persistence record builds");
            assert_eq!(
                finished_json(&finished),
                json!({
                    "outcome": outcome_wire,
                    "persistence": persistence_wire,
                    "error": null,
                }),
                "outcome {outcome:?} with persistence {persistence:?}"
            );
        }
    }
}

#[test]
fn finished_json_encodes_a_typed_execution_error_additively() {
    let finished = terminal(RunOutcome::Failed).with_error(typed_error(
        ErrorCategory::Timeout,
        "tool deadline exceeded",
        RetryGuidance::DoNotRetry,
    ));
    let encoded = finished_json(&finished);
    assert_eq!(
        encoded,
        json!({
            "outcome": "failed",
            "persistence": "Ephemeral",
            "error": {
                "category": "Timeout",
                "retry": "DoNotRetry",
                "message": "tool deadline exceeded",
            },
        })
    );
    assert_eq!(field_count(&encoded), 3, "the error nests, never flattens");
}

#[test]
fn finished_json_reports_lifecycle_and_error_independently() {
    // The lifecycle outcome is not inferred from the presence of a typed
    // error, and no error is fabricated when one was never attached.
    let both = terminal(RunOutcome::Completed).with_error(typed_error(
        ErrorCategory::Internal,
        "unexpected internal failure",
        RetryGuidance::DoNotRetry,
    ));
    assert_eq!(finished_json(&both)["outcome"], "completed");
    assert_eq!(finished_json(&both)["error"]["category"], "Internal");
    assert!(
        finished_json(&terminal(RunOutcome::Completed))["error"].is_null(),
        "no execution error is fabricated"
    );
}

#[test]
fn notice_json_carries_the_exact_decision_identity_without_a_preview() {
    let plain = notice();
    assert_eq!(plain.args_preview(), None);
    let encoded = notice_json(&plain);
    assert_eq!(
        encoded,
        json!({
            "approval": "appr-1",
            "call": "call-1",
            "summary": "run tool host_write",
            "scope": "project scope",
            "args_preview": null,
        })
    );
    assert_eq!(field_count(&encoded), 5);
}

#[test]
fn notice_json_carries_an_attached_args_preview_verbatim() {
    let preview = r#"{"path":"src","mode":"read"}"#;
    let with_preview = notice()
        .with_args_preview(preview)
        .expect("caller-redacted preview attaches");
    let encoded = notice_json(&with_preview);
    assert_eq!(encoded["args_preview"], preview, "preview text is exact");
    assert_eq!(encoded["approval"], "appr-1", "identity is unchanged");
    assert_eq!(encoded["call"], "call-1", "identity is unchanged");
    assert_eq!(encoded["summary"], "run tool host_write");
    assert_eq!(encoded["scope"], "project scope");
    assert_eq!(field_count(&encoded), 5);
}

#[test]
fn event_json_run_started_carries_the_originating_request() {
    let encoded = event_json(&event(0, EventPayload::RunStarted { request: request() }));
    assert_eq!(
        encoded,
        json!({
            "seq": 0,
            "run": "run-1",
            "kind": "run-started",
            "terminal": false,
            "detail": { "request": "req-1" },
        })
    );
}

#[test]
fn event_json_assistant_text_delta_carries_turn_item_and_text() {
    let fragment =
        AssistantText::new(turn(), "item-0", "hello there").expect("bounded fragment builds");
    let encoded = event_json(&event(1, EventPayload::AssistantTextDelta(fragment)));
    assert_eq!(encoded["kind"], "assistant-text");
    assert_eq!(encoded["terminal"], false);
    assert_eq!(
        encoded["detail"],
        json!({ "turn": "turn-1", "item": "item-0", "text": "hello there" })
    );
}

#[test]
fn event_json_tool_call_preview_carries_only_the_item_key() {
    let encoded = event_json(&event(
        2,
        EventPayload::ToolCallPreview {
            item_key: "item-1".to_owned(),
        },
    ));
    assert_eq!(encoded["kind"], "tool-call-preview");
    assert_eq!(encoded["terminal"], false);
    assert_eq!(encoded["detail"], json!({ "item": "item-1" }));
    assert_eq!(
        field_count(&encoded["detail"]),
        1,
        "a preview authorizes nothing"
    );
}

#[test]
fn event_json_approval_required_delegates_to_the_notice_encoder() {
    let attached = notice()
        .with_args_preview(r#"{"path":"src"}"#)
        .expect("caller-redacted preview attaches");
    let plain = event_json(&event(3, EventPayload::ApprovalRequired(attached.clone())));
    assert_eq!(plain["kind"], "approval-required");
    assert_eq!(plain["terminal"], false);
    assert_eq!(plain["detail"], notice_json(&attached));
    assert_eq!(plain["detail"]["args_preview"], r#"{"path":"src"}"#);

    let without = event_json(&event(4, EventPayload::ApprovalRequired(notice())));
    assert_eq!(without["detail"], notice_json(&notice()));
    assert!(
        without["detail"]["args_preview"].is_null(),
        "an absent preview stays null"
    );
}

#[test]
fn event_json_tool_started_carries_the_call_identity() {
    let encoded = event_json(&event(
        5,
        EventPayload::ToolStarted(ToolStartedInfo {
            call: call("call-7"),
            tool: nexus_core::ToolId::new("host_read", nexus_core::M0_REVISION).unwrap(),
            args_preview: Some(r#"host_read {"path":"src"}"#.to_owned()),
        }),
    ));
    assert_eq!(encoded["kind"], "tool-started");
    assert_eq!(encoded["terminal"], false);
    assert_eq!(
        encoded["detail"],
        json!({ "call": "call-7", "tool": "host_read", "revision": nexus_core::M0_REVISION, "args_preview": r#"host_read {"path":"src"}"# })
    );
}

#[test]
fn event_json_tool_output_preserves_the_truncation_flag() {
    for truncated in [false, true] {
        let progress = ToolProgress::new(call("call-7"), "partial output", truncated)
            .expect("bounded progress builds");
        let encoded = event_json(&event(6, EventPayload::ToolOutput(progress)));
        assert_eq!(encoded["kind"], "tool-output");
        assert_eq!(encoded["terminal"], false);
        assert_eq!(
            encoded["detail"],
            json!({
                "call": "call-7",
                "preview": "partial output",
                "truncated": truncated,
            })
        );
    }
}

#[test]
fn event_json_tool_finished_carries_the_typed_outcome() {
    let outcome = tool_outcome(
        ExecutionStatus::Denied,
        EffectState::NotStarted,
        Evidence::HostObserved,
        "operator refused",
        false,
    );
    let expected = json!({ "call": "call-7", "outcome": outcome_json(&outcome) });
    let encoded = event_json(&event(
        7,
        EventPayload::ToolFinished(ToolFinishedInfo {
            call: call("call-7"),
            outcome,
        }),
    ));
    assert_eq!(encoded["kind"], "tool-finished");
    assert_eq!(encoded["terminal"], false);
    assert_eq!(encoded["detail"], expected);
    assert_eq!(encoded["detail"]["outcome"]["effect"], "NotStarted");
    assert_eq!(encoded["detail"]["outcome"]["truncated"], false);
}

#[test]
fn event_json_usage_keeps_unknown_counters_null_never_zero() {
    let unknown = event_json(&event(
        8,
        EventPayload::UsageUpdated(Usage::new(None, None, UsageFinality::Final)),
    ));
    assert_eq!(unknown["kind"], "usage");
    assert_eq!(unknown["terminal"], false);
    assert_eq!(
        unknown["detail"],
        json!({ "input": null, "output": null, "finality": "Final" })
    );
    assert!(unknown["detail"]["input"].is_null());
    assert!(unknown["detail"]["output"].is_null());

    let provisional = event_json(&event(
        9,
        EventPayload::UsageUpdated(Usage::new(Some(11), None, UsageFinality::Provisional)),
    ));
    assert_eq!(
        provisional["detail"],
        json!({ "input": 11, "output": null, "finality": "Provisional" })
    );
    assert_eq!(
        usage_json(&Usage::new(Some(11), None, UsageFinality::Provisional)),
        provisional["detail"],
        "the usage event detail is exactly the usage encoding"
    );
}

#[test]
fn event_json_run_finished_carries_the_terminal_record() {
    let finished = terminal(RunOutcome::LimitReached).with_error(typed_error(
        ErrorCategory::ResourceLimit,
        "turn budget exhausted",
        RetryGuidance::DoNotRetry,
    ));
    let expected = finished_json(&finished);
    let encoded = event_json(&event(10, EventPayload::RunFinished(finished)));
    assert_eq!(encoded["kind"], "run-finished");
    assert_eq!(encoded["terminal"], true);
    assert_eq!(
        encoded["detail"],
        json!({
            "outcome": "limit-reached",
            "persistence": "Ephemeral",
            "error": {
                "category": "ResourceLimit",
                "retry": "DoNotRetry",
                "message": "turn budget exhausted",
            },
        })
    );
    assert_eq!(
        encoded["detail"], expected,
        "the run-finished detail is exactly the terminal encoding"
    );
}

#[test]
fn every_payload_kind_has_a_distinct_name_and_only_run_finished_is_terminal() {
    let payloads = [
        (
            EventPayload::RunStarted { request: request() },
            "run-started",
            false,
        ),
        (
            EventPayload::AssistantTextDelta(
                AssistantText::new(turn(), "item-0", "hi").expect("bounded fragment builds"),
            ),
            "assistant-text",
            false,
        ),
        (
            EventPayload::ToolCallPreview {
                item_key: "item-1".to_owned(),
            },
            "tool-call-preview",
            false,
        ),
        (
            EventPayload::ApprovalRequired(notice()),
            "approval-required",
            false,
        ),
        (
            EventPayload::ToolStarted(ToolStartedInfo {
                call: call("call-7"),
                tool: nexus_core::ToolId::new("host_read", nexus_core::M0_REVISION).unwrap(),
                args_preview: None,
            }),
            "tool-started",
            false,
        ),
        (
            EventPayload::ToolOutput(
                ToolProgress::new(call("call-7"), "partial", true)
                    .expect("bounded progress builds"),
            ),
            "tool-output",
            false,
        ),
        (
            EventPayload::ToolFinished(ToolFinishedInfo {
                call: call("call-7"),
                outcome: tool_outcome(
                    ExecutionStatus::TimedOut,
                    EffectState::Unknown,
                    Evidence::Uncertain,
                    "deadline hit",
                    true,
                ),
            }),
            "tool-finished",
            false,
        ),
        (
            EventPayload::UsageUpdated(Usage::new(Some(1), Some(2), UsageFinality::Provisional)),
            "usage",
            false,
        ),
        (
            EventPayload::RunFinished(terminal(RunOutcome::Completed)),
            "run-finished",
            true,
        ),
    ];
    let mut names: Vec<&str> = Vec::new();
    for (index, (payload, kind, terminal)) in payloads.into_iter().enumerate() {
        let seq = index as u64 + 1;
        let encoded = event_json(&event(seq, payload));
        assert_eq!(encoded["kind"], kind, "payload {index}");
        assert_eq!(
            encoded["terminal"], terminal,
            "payload {index} ({kind}) terminal flag"
        );
        assert_eq!(encoded["seq"], seq, "the envelope sequence is preserved");
        assert_eq!(encoded["run"], "run-1", "the envelope run is preserved");
        assert_eq!(
            field_count(&encoded),
            5,
            "exactly seq, run, kind, terminal, detail"
        );
        assert!(!names.contains(&kind), "{kind:?} must be unique");
        names.push(kind);
    }
    assert_eq!(
        names,
        vec![
            "run-started",
            "assistant-text",
            "tool-call-preview",
            "approval-required",
            "tool-started",
            "tool-output",
            "tool-finished",
            "usage",
            "run-finished",
        ],
        "all nine payload kinds are covered"
    );
}

#[test]
fn snapshot_json_reports_an_active_run_with_a_null_outcome() {
    let snapshot = Snapshot::new(
        session(),
        run(),
        Some(7),
        RunLifecycle::Active,
        vec![approval("appr-1"), approval("appr-2")],
        vec![OutcomeSummary {
            call: call("call-7"),
            status: ExecutionStatus::Succeeded,
            effect: EffectState::KnownApplied,
            evidence: Evidence::HostObserved,
        }],
        false,
    )
    .expect("bounded snapshot builds");
    let encoded = snapshot_json(&snapshot);
    assert_eq!(
        encoded,
        json!({
            "run": "run-1",
            "lifecycle": "active",
            "outcome": null,
            "last_sequence": 7,
            "pending_approvals": ["appr-1", "appr-2"],
            "known_outcomes": [{
                "call": "call-7",
                "status": "Succeeded",
                "effect": "KnownApplied",
                "evidence": "HostObserved",
            }],
            "content_truncated": false,
        })
    );
    assert_eq!(
        field_count(&encoded),
        7,
        "the owning session is not part of the encoded snapshot"
    );
    assert!(encoded["outcome"].is_null(), "an active run has no outcome");
}

#[test]
fn snapshot_json_reports_every_finalized_outcome() {
    for (outcome, outcome_wire) in run_outcomes() {
        let snapshot = Snapshot::new(
            session(),
            run(),
            Some(9),
            RunLifecycle::Finalized(outcome),
            Vec::new(),
            Vec::new(),
            true,
        )
        .expect("bounded snapshot builds");
        let encoded = snapshot_json(&snapshot);
        assert_eq!(encoded["lifecycle"], "finalized", "outcome {outcome:?}");
        assert_eq!(encoded["outcome"], outcome_wire, "outcome {outcome:?}");
        assert_eq!(encoded["last_sequence"], 9);
        assert_eq!(encoded["content_truncated"], true);
        assert_eq!(
            encoded["pending_approvals"],
            json!([]),
            "an empty pending list stays an array"
        );
        assert_eq!(
            encoded["known_outcomes"],
            json!([]),
            "an empty known-outcome list stays an array"
        );
    }
}

#[test]
fn snapshot_json_keeps_an_unpublished_sequence_null() {
    let snapshot = Snapshot::new(
        session(),
        run(),
        None,
        RunLifecycle::Active,
        Vec::new(),
        Vec::new(),
        false,
    )
    .expect("bounded snapshot builds");
    let encoded = snapshot_json(&snapshot);
    assert!(
        encoded["last_sequence"].is_null(),
        "an unpublished sequence is null, never 0"
    );
    assert_ne!(encoded["last_sequence"], 0);
}

#[test]
fn snapshot_json_encodes_pending_approvals_in_order_at_the_bound() {
    let pending: Vec<ApprovalId> = (0..Limits::M0_TEST_MAX_CONCURRENT_OPS)
        .map(|index| approval(&format!("appr-{index}")))
        .collect();
    let snapshot = Snapshot::new(
        session(),
        run(),
        Some(8),
        RunLifecycle::Active,
        pending,
        Vec::new(),
        false,
    )
    .expect("a snapshot exactly at its pending bound builds");
    let expected: Vec<Value> = (0..Limits::M0_TEST_MAX_CONCURRENT_OPS)
        .map(|index| json!(format!("appr-{index}")))
        .collect();
    let encoded = snapshot_json(&snapshot);
    assert_eq!(encoded["pending_approvals"], Value::Array(expected));
    assert_eq!(
        encoded["last_sequence"],
        Limits::M0_TEST_MAX_CONCURRENT_OPS as u64
    );

    let over_bound: Vec<ApprovalId> = (0..=Limits::M0_TEST_MAX_CONCURRENT_OPS)
        .map(|index| approval(&format!("appr-{index}")))
        .collect();
    assert!(
        Snapshot::new(
            session(),
            run(),
            Some(0),
            RunLifecycle::Active,
            over_bound,
            Vec::new(),
            false,
        )
        .is_err(),
        "snapshots beyond the pending bound never reach the encoder"
    );
}

#[test]
fn snapshot_json_encodes_known_outcomes_in_order_within_the_bound() {
    // A snapshot is bounded by the per-run call budget, so the exhaustive
    // status/effect/evidence matrix is asserted per summary
    // (`outcome_summary_json_encodes_only_the_typed_summary_fields`) and a
    // bound-sized run of them is asserted here, in order.
    let mut summaries = Vec::new();
    let mut expected = Vec::new();
    for (status, _) in statuses() {
        for (effect, _) in effects() {
            for (evidence, _) in evidence_classes() {
                if !is_admissible(status, effect) {
                    continue;
                }
                let summary = OutcomeSummary {
                    call: call("call-7"),
                    status,
                    effect,
                    evidence,
                };
                expected.push(outcome_summary_json(&summary));
                summaries.push(summary);
            }
        }
    }
    assert_eq!(summaries.len(), 51, "every admissible combination is built");
    let bound = Limits::M0_TEST_TOOL_CALLS_PER_RUN as usize;
    // Cycle the matrix to the bound: the bound exceeds the admissible
    // combinations, and repetition keeps construction order checkable.
    let summaries: Vec<OutcomeSummary> = (0..bound)
        .map(|index| summaries[index % summaries.len()].clone())
        .collect();
    let expected: Vec<Value> = (0..bound)
        .map(|index| expected[index % expected.len()].clone())
        .collect();
    let snapshot = Snapshot::new(
        session(),
        run(),
        Some(3),
        RunLifecycle::Active,
        Vec::new(),
        summaries,
        false,
    )
    .expect("a snapshot at the known-outcome bound builds");
    let encoded = snapshot_json(&snapshot);
    assert_eq!(
        encoded["known_outcomes"],
        Value::Array(expected),
        "known outcomes keep their construction order"
    );
}

#[test]
fn snapshot_summaries_carry_no_tool_content() {
    let summary = OutcomeSummary {
        call: call("call-7"),
        status: ExecutionStatus::Succeeded,
        effect: EffectState::KnownApplied,
        evidence: Evidence::HostObserved,
    };
    let snapshot = Snapshot::new(
        session(),
        run(),
        Some(2),
        RunLifecycle::Active,
        Vec::new(),
        vec![summary.clone()],
        false,
    )
    .expect("bounded snapshot builds");
    let encoded = snapshot_json(&snapshot);
    let entry = &encoded["known_outcomes"][0];
    assert_eq!(
        field_count(entry),
        4,
        "a summary is identity plus typed fields"
    );
    assert_eq!(*entry, outcome_summary_json(&summary));
    let names = field_names(&encoded);
    assert!(
        names.contains(&"content_truncated") && !names.contains(&"content"),
        "a snapshot is never a replay log: {names:?}"
    );
}

#[test]
fn snapshot_json_carries_the_content_truncation_flag() {
    for truncated in [false, true] {
        let snapshot = Snapshot::new(
            session(),
            run(),
            Some(1),
            RunLifecycle::Active,
            Vec::new(),
            Vec::new(),
            truncated,
        )
        .expect("bounded snapshot builds");
        assert_eq!(
            snapshot_json(&snapshot)["content_truncated"],
            truncated,
            "retained-content truncation is reported verbatim"
        );
    }
}

#[test]
fn snapshot_json_accepts_the_exact_known_outcome_bound() {
    let known_outcomes: Vec<OutcomeSummary> = (0..Limits::M0_TEST_TOOL_CALLS_PER_RUN as usize)
        .map(|index| OutcomeSummary {
            call: call(&format!("call-{index}")),
            status: ExecutionStatus::Succeeded,
            effect: EffectState::KnownApplied,
            evidence: Evidence::HostObserved,
        })
        .collect();
    let snapshot = Snapshot::new(
        session(),
        run(),
        Some(16),
        RunLifecycle::Active,
        vec![approval("appr-1")],
        known_outcomes,
        false,
    )
    .expect("a snapshot exactly at its known-outcome bound builds");
    let encoded = snapshot_json(&snapshot);
    let list = encoded["known_outcomes"]
        .as_array()
        .expect("outcome summaries are an array");
    assert_eq!(list.len(), Limits::M0_TEST_TOOL_CALLS_PER_RUN as usize);
    assert_eq!(list[15]["call"], "call-15", "ordering is preserved");

    let over_bound: Vec<OutcomeSummary> = (0..=Limits::M0_TEST_TOOL_CALLS_PER_RUN as usize)
        .map(|index| OutcomeSummary {
            call: call(&format!("call-{index}")),
            status: ExecutionStatus::Succeeded,
            effect: EffectState::KnownApplied,
            evidence: Evidence::HostObserved,
        })
        .collect();
    assert!(
        Snapshot::new(
            session(),
            run(),
            Some(0),
            RunLifecycle::Active,
            Vec::new(),
            over_bound,
            false,
        )
        .is_err(),
        "snapshots beyond the known-outcome bound never reach the encoder"
    );
}

#[test]
fn outcome_summary_json_encodes_only_the_typed_summary_fields() {
    for (status, status_name) in statuses() {
        for (effect, effect_name) in effects() {
            for (evidence, evidence_name) in evidence_classes() {
                let encoded = outcome_summary_json(&OutcomeSummary {
                    call: call("call-7"),
                    status,
                    effect,
                    evidence,
                });
                assert_eq!(
                    encoded,
                    json!({
                        "call": "call-7",
                        "status": status_name,
                        "effect": effect_name,
                        "evidence": evidence_name,
                    }),
                    "status {status:?} effect {effect:?} evidence {evidence:?}"
                );
                assert_eq!(field_count(&encoded), 4);
            }
        }
    }
}

#[test]
fn error_body_is_a_single_static_field_and_escapes_quotes() {
    assert_eq!(
        error_body("snapshot failed"),
        br#"{"error":"snapshot failed"}"#.to_vec()
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&error_body("malformed run id"))
            .expect("error body is valid JSON"),
        json!({ "error": "malformed run id" })
    );
    let quoted = error_body(r#"route said "nope""#);
    assert_eq!(
        quoted,
        br#"{"error":"route said \"nope\""}"#.to_vec(),
        "quotes cannot break out of the static body"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&quoted).expect("escaped body is valid JSON"),
        json!({ "error": r#"route said "nope""# })
    );
}

#[test]
fn encoders_are_pure_and_repeatable() {
    let payloads = [
        EventPayload::RunStarted { request: request() },
        EventPayload::ApprovalRequired(notice()),
        EventPayload::ToolFinished(ToolFinishedInfo {
            call: call("call-7"),
            outcome: tool_outcome(
                ExecutionStatus::Succeeded,
                EffectState::KnownApplied,
                Evidence::HostObserved,
                "content",
                false,
            ),
        }),
        EventPayload::RunFinished(terminal(RunOutcome::Cancelled)),
    ];
    for (index, payload) in payloads.into_iter().enumerate() {
        let encoded = event(1, payload);
        let first = event_json(&encoded);
        let second = event_json(&encoded);
        assert_eq!(first, second, "payload {index} encodes identically");
        assert_eq!(
            serde_json::to_string(&first).expect("event encodes"),
            serde_json::to_string(&second).expect("event encodes"),
            "payload {index} is byte-stable"
        );
    }

    let snapshot = Snapshot::new(
        session(),
        run(),
        Some(1),
        RunLifecycle::Active,
        vec![approval("appr-1")],
        vec![OutcomeSummary {
            call: call("call-7"),
            status: ExecutionStatus::Succeeded,
            effect: EffectState::KnownApplied,
            evidence: Evidence::HostObserved,
        }],
        true,
    )
    .expect("bounded snapshot builds");
    assert_eq!(snapshot_json(&snapshot), snapshot_json(&snapshot));
    assert_eq!(
        serde_json::to_string(&snapshot_json(&snapshot)).expect("snapshot encodes"),
        serde_json::to_string(&snapshot_json(&snapshot)).expect("snapshot encodes")
    );
}
