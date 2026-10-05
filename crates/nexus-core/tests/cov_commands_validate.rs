//! Boundary-hardening tests for command and payload validation.
//!
//! Every assertion goes through the public `nexus-core` API: constructors,
//! public struct fields, `validate` methods, and read accessors. Inputs are
//! fixed literals, so the suite is deterministic and touches no runtime,
//! provider, tool, or clock. Internal helpers (`command_error`,
//! `is_bounded_safe_text`) are covered by `commands::cov_commands_private`.

#![forbid(unsafe_code)]

use std::time::Duration;

use nexus_core::commands::{
    MAX_INPUT_BYTES, MAX_LIST_LIMIT, MAX_SUMMARY_BYTES, MAX_TEXT_FRAGMENT_BYTES,
    checked_next_sequence,
};
use nexus_core::content::MAX_ITEM_KEY_LEN;
use nexus_core::{
    AgentError, ApprovalId, ApprovalNotice, ApproveCommand, AssistantText, CallId, CancelCommand,
    Command, CommandReply, CommandResponse, DenyCommand, EffectState, ErrorCategory, EventPayload,
    Evidence, ExecutionStatus, GetSnapshotCommand, Limits, ListSessionsCommand, OutcomeSummary,
    PersistenceState, RequestId, RestoreSessionCommand, RetryGuidance, RunEvent, RunFinished,
    RunId, RunLifecycle, RunOutcome, SessionId, Snapshot, SubmitCommand, ToolFinishedInfo,
    ToolOutcome, ToolProgress, ToolStartedInfo, TurnId, Usage, UsageFinality,
};

/// Secret-marker substrings that the best-effort boundary net rejects.
const SECRET_MARKERS: [&str; 12] = [
    "-----begin",
    "bearer ",
    "sk-",
    "akia",
    "ghp_",
    "xoxb-",
    "password=",
    "passwd=",
    "secret=",
    "api_key=",
    "apikey=",
    "client_secret",
];

fn request() -> RequestId {
    RequestId::new("req-1").expect("valid request id")
}

fn session() -> SessionId {
    SessionId::new("sess-1").expect("valid session id")
}

fn run() -> RunId {
    RunId::new("run-1").expect("valid run id")
}

fn turn() -> TurnId {
    TurnId::new("turn-1").expect("valid turn id")
}

fn call() -> CallId {
    CallId::new("call-1").expect("valid call id")
}

fn approval() -> ApprovalId {
    ApprovalId::new("appr-1").expect("valid approval id")
}

fn assert_invalid(error: AgentError, expected: &str) {
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(error.message(), expected);
}

fn submit(input: &str, profile: &str) -> SubmitCommand {
    SubmitCommand {
        request: request(),
        session: session(),
        input: input.to_owned(),
        profile: profile.to_owned(),
    }
}

fn notice(summary: &str, scope: &str) -> ApprovalNotice {
    ApprovalNotice::new(approval(), call(), summary, scope, Duration::from_secs(120))
        .expect("valid notice builds")
}

fn safe_error() -> AgentError {
    AgentError::new(
        ErrorCategory::Timeout,
        "tool deadline exceeded",
        RetryGuidance::DoNotRetry,
    )
    .expect("static safe message builds")
}

fn valid_usage() -> Usage {
    Usage::new(Some(10), Some(5), UsageFinality::Final)
}

fn valid_outcome() -> ToolOutcome {
    ToolOutcome::new(
        ExecutionStatus::Succeeded,
        EffectState::KnownApplied,
        Evidence::HostObserved,
        "ok",
        false,
    )
    .expect("valid tool outcome builds")
}

fn valid_terminal() -> RunFinished {
    RunFinished::new(RunOutcome::Completed, PersistenceState::Ephemeral, None)
        .expect("valid terminal record builds")
}

fn outcome_summary(index: usize) -> OutcomeSummary {
    OutcomeSummary {
        call: CallId::new(format!("call-{index}")).expect("valid call id"),
        status: ExecutionStatus::Succeeded,
        effect: EffectState::KnownApplied,
        evidence: Evidence::HostObserved,
    }
}

#[test]
fn documented_m0_bounds_stay_pinned() {
    assert_eq!(MAX_INPUT_BYTES, 65_536);
    assert_eq!(MAX_SUMMARY_BYTES, 1_024);
    assert_eq!(MAX_TEXT_FRAGMENT_BYTES, 65_536);
    assert_eq!(MAX_LIST_LIMIT, 1_024);
    assert_eq!(MAX_ITEM_KEY_LEN, 128);
}

#[test]
fn submit_constructor_enforces_input_and_profile_bounds() {
    let exact_input = "x".repeat(MAX_INPUT_BYTES);
    let command = SubmitCommand::new(request(), session(), exact_input.clone(), "p")
        .expect("input at the exact bound is accepted");
    assert_eq!(command.input, exact_input);
    assert_eq!(command.profile, "p");
    assert_invalid(
        SubmitCommand::new(request(), session(), format!("{exact_input}x"), "p")
            .expect_err("input one byte over the bound is rejected"),
        "submit input is invalid",
    );
    assert_invalid(
        SubmitCommand::new(request(), session(), "", "p").expect_err("empty input is rejected"),
        "submit input is invalid",
    );

    let exact_profile = "p".repeat(MAX_SUMMARY_BYTES);
    SubmitCommand::new(request(), session(), "go", exact_profile.clone())
        .expect("profile at the exact bound is accepted");
    assert_invalid(
        SubmitCommand::new(request(), session(), "go", format!("{exact_profile}p"))
            .expect_err("profile one byte over the bound is rejected"),
        "submit profile is invalid",
    );
    assert_invalid(
        SubmitCommand::new(request(), session(), "go", "").expect_err("empty profile is rejected"),
        "submit profile is invalid",
    );
}

#[test]
fn submit_profile_bound_is_smaller_than_the_input_bound() {
    let profile = "p".repeat(MAX_SUMMARY_BYTES + 1);
    assert!(
        profile.len() < MAX_INPUT_BYTES,
        "the rejected profile sits between the two documented bounds"
    );
    assert_invalid(
        SubmitCommand::new(request(), session(), "go", profile)
            .expect_err("the smaller profile bound governs"),
        "submit profile is invalid",
    );
}

#[test]
fn submit_bounds_count_bytes_not_characters() {
    let exact = "é".repeat(MAX_INPUT_BYTES / 2);
    assert_eq!(exact.len(), MAX_INPUT_BYTES);
    SubmitCommand::new(request(), session(), exact, "p")
        .expect("two-byte characters filling the byte bound are accepted");
    let over = "é".repeat(MAX_INPUT_BYTES / 2 + 1);
    assert!(over.chars().count() < MAX_INPUT_BYTES);
    assert_invalid(
        SubmitCommand::new(request(), session(), over, "p")
            .expect_err("a character count below the bound does not bypass the byte bound"),
        "submit input is invalid",
    );
}

#[test]
fn submit_accepts_nonempty_whitespace_input() {
    SubmitCommand::new(request(), session(), " ", "p")
        .expect("whitespace-only input is non-empty and accepted");
}

#[test]
fn submit_public_mutation_is_revalidated() {
    let mut command = submit("go", "p");
    command.validate().expect("baseline validates");

    command.input.clear();
    assert_invalid(
        command
            .validate()
            .expect_err("empty mutated input is rejected"),
        "submit input is invalid",
    );
    command.input = "x".repeat(MAX_INPUT_BYTES + 1);
    assert_invalid(
        command
            .validate()
            .expect_err("oversized mutated input is rejected"),
        "submit input is invalid",
    );

    command.input = "go".to_owned();
    command.profile.clear();
    assert_invalid(
        command
            .validate()
            .expect_err("empty mutated profile is rejected"),
        "submit profile is invalid",
    );
    command.profile = "p".repeat(MAX_SUMMARY_BYTES + 1);
    assert_invalid(
        command
            .validate()
            .expect_err("oversized mutated profile is rejected"),
        "submit profile is invalid",
    );

    command.profile = "p".to_owned();
    command.validate().expect("restored command validates");
}

#[test]
fn list_limit_accepts_exact_max_and_rejects_zero_and_overflow() {
    let command =
        ListSessionsCommand::new(request(), MAX_LIST_LIMIT).expect("exact maximum accepted");
    assert_eq!(command.limit, MAX_LIST_LIMIT);
    ListSessionsCommand::new(request(), 1).expect("minimum accepted");
    assert_invalid(
        ListSessionsCommand::new(request(), 0).expect_err("zero is not finite-and-nonzero"),
        "list limit must be finite and nonzero",
    );
    assert_invalid(
        ListSessionsCommand::new(request(), MAX_LIST_LIMIT + 1)
            .expect_err("one over the maximum is rejected"),
        "list limit must be finite and nonzero",
    );
}

#[test]
fn list_limit_public_mutation_is_revalidated() {
    let mut command = ListSessionsCommand::new(request(), 1).expect("valid list builds");
    command.limit = 0;
    assert_invalid(
        command.validate().expect_err("mutated zero is rejected"),
        "list limit must be finite and nonzero",
    );
    command.limit = usize::MAX;
    assert_invalid(
        command
            .validate()
            .expect_err("an effectively unbounded limit is rejected"),
        "list limit must be finite and nonzero",
    );
    command.limit = MAX_LIST_LIMIT;
    command.validate().expect("restored limit validates");
}

#[test]
fn assistant_text_enforces_item_key_and_text_bounds() {
    let exact_key = "k".repeat(MAX_ITEM_KEY_LEN);
    AssistantText::new(turn(), exact_key.clone(), "")
        .expect("exact item key with empty text is accepted");
    assert_invalid(
        AssistantText::new(turn(), "", "hi").expect_err("empty item key is rejected"),
        "assistant text fragment is invalid",
    );
    assert_invalid(
        AssistantText::new(turn(), format!("{exact_key}k"), "hi")
            .expect_err("over-long item key is rejected"),
        "assistant text fragment is invalid",
    );
    AssistantText::new(turn(), "item-0", "x".repeat(MAX_TEXT_FRAGMENT_BYTES))
        .expect("text at the exact bound is accepted");
    assert_invalid(
        AssistantText::new(turn(), "item-0", "x".repeat(MAX_TEXT_FRAGMENT_BYTES + 1))
            .expect_err("text one byte over the bound is rejected"),
        "assistant text fragment is invalid",
    );
}

#[test]
fn assistant_text_item_key_bound_counts_bytes_not_characters() {
    let exact = "é".repeat(MAX_ITEM_KEY_LEN / 2);
    assert_eq!(exact.len(), MAX_ITEM_KEY_LEN);
    AssistantText::new(turn(), exact, "hi")
        .expect("two-byte characters filling the byte bound are accepted");
    let over = "é".repeat(MAX_ITEM_KEY_LEN / 2 + 1);
    assert!(over.chars().count() < MAX_ITEM_KEY_LEN);
    assert_invalid(
        AssistantText::new(turn(), over, "hi")
            .expect_err("a character count below the bound does not bypass the byte bound"),
        "assistant text fragment is invalid",
    );
}

#[test]
fn assistant_text_public_mutation_is_revalidated() {
    let mut fragment = AssistantText::new(turn(), "item-0", "hi").expect("valid fragment builds");
    fragment.item_key.clear();
    assert_invalid(
        fragment.validate().expect_err("empty mutated item key"),
        "assistant text fragment is invalid",
    );
    fragment.item_key = "k".repeat(MAX_ITEM_KEY_LEN + 1);
    assert_invalid(
        fragment.validate().expect_err("over-long mutated item key"),
        "assistant text fragment is invalid",
    );
    fragment.item_key = "item-0".to_owned();
    fragment.text = "x".repeat(MAX_TEXT_FRAGMENT_BYTES + 1);
    assert_invalid(
        fragment.validate().expect_err("over-long mutated text"),
        "assistant text fragment is invalid",
    );
    fragment.text.clear();
    fragment
        .validate()
        .expect("empty text with a restored item key validates");
}

#[test]
fn tool_progress_bounds_preview_against_the_output_budget() {
    let budget = Limits::M0_TEST_TOOL_OUTPUT_BYTES;
    ToolProgress::new(call(), "", false).expect("empty preview is accepted");
    ToolProgress::new(call(), "x".repeat(budget), false).expect("exact budget is accepted");
    assert_invalid(
        ToolProgress::new(call(), "x".repeat(budget + 1), false)
            .expect_err("one byte over the budget is rejected"),
        "tool progress exceeds output budget",
    );
}

#[test]
fn tool_progress_public_mutation_is_revalidated() {
    let budget = Limits::M0_TEST_TOOL_OUTPUT_BYTES;
    let mut progress = ToolProgress::new(call(), "ok", false).expect("valid progress builds");
    progress.preview = "x".repeat(budget + 1);
    assert_invalid(
        progress
            .validate()
            .expect_err("over-budget mutated preview is rejected"),
        "tool progress exceeds output budget",
    );
    progress.preview.clear();
    progress.validate().expect("empty preview validates");
    progress.truncated = true;
    progress
        .validate()
        .expect("the truncation flag is data, not a validation input");
}

#[test]
fn approval_notice_accepts_exact_summary_and_scope_bounds() {
    let exact_summary = "s".repeat(MAX_SUMMARY_BYTES);
    let exact_scope = "c".repeat(MAX_SUMMARY_BYTES);
    let built = notice(&exact_summary, &exact_scope);
    assert_eq!(built.summary.len(), MAX_SUMMARY_BYTES);
    assert_eq!(built.scope_summary.len(), MAX_SUMMARY_BYTES);
    assert_eq!(built.args_preview(), None);
    assert_eq!(built.expires_at_elapsed, Duration::from_secs(120));
}

#[test]
fn approval_notice_rejects_empty_and_over_long_summary_and_scope() {
    let expires = Duration::from_secs(120);
    assert_invalid(
        ApprovalNotice::new(approval(), call(), "", "project scope", expires)
            .expect_err("empty summary is rejected"),
        "approval summary is invalid",
    );
    assert_invalid(
        ApprovalNotice::new(approval(), call(), "run tool", "", expires)
            .expect_err("empty scope is rejected"),
        "approval scope is invalid",
    );
    assert_invalid(
        ApprovalNotice::new(
            approval(),
            call(),
            "s".repeat(MAX_SUMMARY_BYTES + 1),
            "project scope",
            expires,
        )
        .expect_err("over-long summary is rejected"),
        "approval summary is invalid",
    );
    assert_invalid(
        ApprovalNotice::new(
            approval(),
            call(),
            "run tool",
            "s".repeat(MAX_SUMMARY_BYTES + 1),
            expires,
        )
        .expect_err("over-long scope is rejected"),
        "approval scope is invalid",
    );
}

#[test]
fn approval_notice_rejects_every_secret_marker_in_every_text_field() {
    let expires = Duration::from_secs(120);
    for marker in SECRET_MARKERS {
        let summary = format!("call {marker}value");
        assert_invalid(
            ApprovalNotice::new(approval(), call(), summary, "project scope", expires)
                .expect_err("secret-bearing summary never builds"),
            "approval summary is invalid",
        );

        let scope = format!("scope {marker}value");
        assert_invalid(
            ApprovalNotice::new(approval(), call(), "run tool", scope, expires)
                .expect_err("secret-bearing scope never builds"),
            "approval scope is invalid",
        );

        let preview = format!("args {marker}value");
        assert_invalid(
            notice("run tool", "project scope")
                .with_args_preview(preview)
                .expect_err("secret-bearing preview never attaches"),
            "approval args preview is invalid",
        );
    }
}

#[test]
fn approval_notice_marker_net_is_case_insensitive() {
    let expires = Duration::from_secs(120);
    for marker in [
        "PASSWORD=",
        "Bearer ",
        "API_KEY=",
        "Client_Secret",
        "-----BEGIN",
        "AKIAIOSFODNN7EXAMPLE",
    ] {
        assert_invalid(
            ApprovalNotice::new(approval(), call(), marker, "project scope", expires)
                .expect_err("uppercase markers are still rejected"),
            "approval summary is invalid",
        );
    }
}

#[test]
fn approval_notice_diagnostics_never_interpolate_rejected_content() {
    let error = ApprovalNotice::new(
        approval(),
        call(),
        "api_key=SUPERSECRET",
        "project scope",
        Duration::from_secs(120),
    )
    .expect_err("secret-bearing summary is rejected");
    assert_eq!(error.message(), "approval summary is invalid");
    assert!(!error.message().contains("api_key"));
    assert!(!error.message().contains("SUPERSECRET"));
}

#[test]
fn approval_notice_args_preview_is_additive_and_bounded() {
    let base = notice("run tool", "project scope");
    assert_eq!(
        base.args_preview(),
        None,
        "the base constructor adds no preview"
    );

    let attached = notice("run tool", "project scope")
        .with_args_preview(r#"{"path":"src","mode":"read"}"#)
        .expect("safe preview attaches");
    assert_eq!(
        attached.args_preview(),
        Some(r#"{"path":"src","mode":"read"}"#)
    );
    assert_eq!(
        attached.summary, "run tool",
        "the builder keeps the summary"
    );
    assert_eq!(attached.scope_summary, "project scope");

    let exact = "p".repeat(MAX_SUMMARY_BYTES);
    notice("run tool", "project scope")
        .with_args_preview(exact)
        .expect("exact preview bound is accepted");
    assert_invalid(
        notice("run tool", "project scope")
            .with_args_preview("p".repeat(MAX_SUMMARY_BYTES + 1))
            .expect_err("one byte over the preview bound is rejected"),
        "approval args preview is invalid",
    );
    assert_invalid(
        notice("run tool", "project scope")
            .with_args_preview("")
            .expect_err("empty preview is rejected"),
        "approval args preview is invalid",
    );
}

#[test]
fn approval_notice_public_mutation_is_revalidated() {
    let mut notice = notice("run tool", "project scope");
    notice.summary.clear();
    assert_invalid(
        notice.validate().expect_err("empty mutated summary"),
        "approval summary is invalid",
    );
    notice.summary = "s".repeat(MAX_SUMMARY_BYTES + 1);
    assert_invalid(
        notice.validate().expect_err("over-long mutated summary"),
        "approval summary is invalid",
    );
    notice.summary = "run tool".to_owned();

    notice.scope_summary = "secret=AAAA".to_owned();
    assert_invalid(
        notice.validate().expect_err("secret-bearing mutated scope"),
        "approval scope is invalid",
    );
    notice.scope_summary = "project scope".to_owned();

    notice.args_preview = Some(String::new());
    assert_invalid(
        notice.validate().expect_err("empty mutated preview"),
        "approval args preview is invalid",
    );
    notice.args_preview = Some("p".repeat(MAX_SUMMARY_BYTES + 1));
    assert_invalid(
        notice.validate().expect_err("over-long mutated preview"),
        "approval args preview is invalid",
    );
    notice.args_preview = Some("args".to_owned());
    notice.validate().expect("restored preview validates");
    notice.args_preview = None;
    notice.validate().expect("absent preview validates");
}

#[test]
fn event_payload_validates_text_variants_and_accepts_opaque_variants() {
    let invalid_delta = EventPayload::AssistantTextDelta(AssistantText {
        turn: turn(),
        item_key: String::new(),
        text: "hi".to_owned(),
    });
    assert_invalid(
        invalid_delta
            .validate()
            .expect_err("empty item key delta is rejected"),
        "assistant text fragment is invalid",
    );

    assert_invalid(
        EventPayload::ToolCallPreview {
            item_key: String::new(),
        }
        .validate()
        .expect_err("empty preview item key is rejected"),
        "tool call preview is invalid",
    );
    assert_invalid(
        EventPayload::ToolCallPreview {
            item_key: "k".repeat(MAX_ITEM_KEY_LEN + 1),
        }
        .validate()
        .expect_err("over-long preview item key is rejected"),
        "tool call preview is invalid",
    );
    EventPayload::ToolCallPreview {
        item_key: "k".repeat(MAX_ITEM_KEY_LEN),
    }
    .validate()
    .expect("exact preview item key is accepted");

    let mut secret_notice = notice("run tool", "project scope");
    secret_notice.summary = "api_key=AAAA".to_owned();
    assert_invalid(
        EventPayload::ApprovalRequired(secret_notice)
            .validate()
            .expect_err("secret-bearing notice is rejected"),
        "approval summary is invalid",
    );

    let mut over_progress = ToolProgress::new(call(), "ok", false).expect("valid progress builds");
    over_progress.preview = "x".repeat(Limits::M0_TEST_TOOL_OUTPUT_BYTES + 1);
    assert_invalid(
        EventPayload::ToolOutput(over_progress)
            .validate()
            .expect_err("over-budget progress is rejected"),
        "tool progress exceeds output budget",
    );

    let opaque = vec![
        EventPayload::RunStarted { request: request() },
        EventPayload::AssistantTextDelta(
            AssistantText::new(turn(), "item-0", "hi").expect("valid delta builds"),
        ),
        EventPayload::ToolStarted(ToolStartedInfo { call: call() }),
        EventPayload::ToolFinished(ToolFinishedInfo {
            call: call(),
            outcome: valid_outcome(),
        }),
        EventPayload::UsageUpdated(valid_usage()),
        EventPayload::UsageUpdated(Usage::new(None, None, UsageFinality::Provisional)),
        EventPayload::RunFinished(valid_terminal()),
        EventPayload::RunFinished(
            RunFinished::new(
                RunOutcome::Failed,
                PersistenceState::SaveFailed,
                Some(safe_error()),
            )
            .expect("save-failure terminal builds")
            .with_error(safe_error()),
        ),
    ];
    for payload in opaque {
        payload.validate().expect("constructed payload validates");
    }
}

#[test]
fn run_event_validate_delegates_to_the_payload() {
    let invalid = RunEvent::new(
        session(),
        run(),
        0,
        EventPayload::ToolCallPreview {
            item_key: String::new(),
        },
    );
    assert_invalid(
        invalid.validate().expect_err("invalid payload is rejected"),
        "tool call preview is invalid",
    );

    let event = RunEvent::new(
        session(),
        run(),
        42,
        EventPayload::AssistantTextDelta(
            AssistantText::new(turn(), "item-0", "hi").expect("valid delta builds"),
        ),
    );
    assert_eq!(event.session(), &session());
    assert_eq!(event.run(), &run());
    assert_eq!(event.seq(), 42);
    event.validate().expect("valid envelope validates");
    assert!(!event.is_terminal());
}

#[test]
fn run_event_terminal_detection_covers_every_nonterminal_payload() {
    let payloads = vec![
        EventPayload::RunStarted { request: request() },
        EventPayload::AssistantTextDelta(
            AssistantText::new(turn(), "item-0", "hi").expect("valid delta builds"),
        ),
        EventPayload::ToolCallPreview {
            item_key: "item-0".to_owned(),
        },
        EventPayload::ApprovalRequired(notice("run tool", "project scope")),
        EventPayload::ToolStarted(ToolStartedInfo { call: call() }),
        EventPayload::ToolOutput(ToolProgress::new(call(), "ok", false).expect("valid progress")),
        EventPayload::ToolFinished(ToolFinishedInfo {
            call: call(),
            outcome: valid_outcome(),
        }),
        EventPayload::UsageUpdated(valid_usage()),
    ];
    for payload in payloads {
        let event = RunEvent::new(session(), run(), 0, payload);
        assert!(!event.is_terminal(), "only RunFinished is terminal");
    }

    let finished = RunEvent::new(
        session(),
        run(),
        1,
        EventPayload::RunFinished(valid_terminal()),
    );
    assert!(finished.is_terminal());
}

#[test]
fn every_command_variant_reports_its_correlation_request() {
    let request = request();
    let commands = vec![
        Command::Submit(
            SubmitCommand::new(request.clone(), session(), "go", "p").expect("valid submit"),
        ),
        Command::Cancel(CancelCommand {
            request: request.clone(),
            run: run(),
        }),
        Command::Approve(ApproveCommand {
            request: request.clone(),
            approval: approval(),
            run: run(),
            call: call(),
        }),
        Command::Deny(DenyCommand {
            request: request.clone(),
            approval: approval(),
            run: run(),
            call: call(),
        }),
        Command::GetSnapshot(GetSnapshotCommand {
            request: request.clone(),
            run: run(),
        }),
        Command::ListSessions(ListSessionsCommand::new(request.clone(), 10).expect("valid list")),
        Command::RestoreSession(RestoreSessionCommand {
            request: request.clone(),
            session: session(),
        }),
    ];
    for command in &commands {
        assert_eq!(command.request(), &request);
    }
}

#[test]
fn command_response_accessors_cover_every_reply_and_absent_run() {
    let request = request();
    for reply in [
        CommandReply::Accepted,
        CommandReply::Rejected,
        CommandReply::AlreadyFinalized,
        CommandReply::StaleOrUnknownTarget,
        CommandReply::Busy,
    ] {
        let response = CommandResponse::new(request.clone(), reply, None);
        assert_eq!(response.request(), &request);
        assert_eq!(response.reply(), reply);
        assert!(
            response.run().is_none(),
            "no run is fabricated for {reply:?}"
        );
    }

    let response = CommandResponse::new(request, CommandReply::Accepted, Some(run()));
    assert_eq!(response.run(), Some(&run()));
}

#[test]
fn snapshot_accepts_exact_bounds_and_rejects_one_over() {
    let pending: Vec<ApprovalId> = (0..Limits::M0_TEST_MAX_CONCURRENT_OPS)
        .map(|index| ApprovalId::new(format!("appr-{index}")).expect("valid approval id"))
        .collect();
    let outcomes: Vec<OutcomeSummary> = (0..Limits::M0_TEST_TOOL_CALLS_PER_RUN as usize)
        .map(outcome_summary)
        .collect();

    let snapshot = Snapshot::new(
        session(),
        run(),
        Some(7),
        RunLifecycle::Finalized(RunOutcome::Completed),
        pending.clone(),
        outcomes.clone(),
        true,
    )
    .expect("exact bounds are accepted");
    assert_eq!(snapshot.session(), &session());
    assert_eq!(snapshot.run(), &run());
    assert_eq!(snapshot.last_sequence(), Some(7));
    assert_eq!(
        snapshot.lifecycle(),
        RunLifecycle::Finalized(RunOutcome::Completed)
    );
    assert_eq!(
        snapshot.pending_approvals().len(),
        Limits::M0_TEST_MAX_CONCURRENT_OPS
    );
    assert_eq!(
        snapshot.known_outcomes().len(),
        Limits::M0_TEST_TOOL_CALLS_PER_RUN as usize
    );
    assert!(snapshot.is_content_truncated());

    let empty = Snapshot::new(
        session(),
        run(),
        None,
        RunLifecycle::Active,
        vec![],
        vec![],
        false,
    )
    .expect("empty snapshot is accepted");
    assert_eq!(empty.last_sequence(), None);
    assert!(empty.pending_approvals().is_empty());
    assert!(empty.known_outcomes().is_empty());
    assert!(!empty.is_content_truncated());

    let mut too_many_pending = pending;
    too_many_pending.push(ApprovalId::new("appr-extra").expect("valid approval id"));
    assert_invalid(
        Snapshot::new(
            session(),
            run(),
            None,
            RunLifecycle::Active,
            too_many_pending,
            outcomes.clone(),
            false,
        )
        .expect_err("pending approvals one over the bound are rejected"),
        "snapshot exceeds pending approval bound",
    );

    let mut too_many_outcomes = outcomes;
    too_many_outcomes.push(outcome_summary(usize::MAX));
    assert_invalid(
        Snapshot::new(
            session(),
            run(),
            None,
            RunLifecycle::Active,
            vec![],
            too_many_outcomes,
            false,
        )
        .expect_err("known outcomes one over the bound are rejected"),
        "snapshot exceeds known outcome bound",
    );
}

#[test]
fn checked_next_sequence_reports_overflow_instead_of_wrapping() {
    assert_eq!(checked_next_sequence(0), Some(1));
    assert_eq!(checked_next_sequence(u64::MAX - 1), Some(u64::MAX));
    assert_eq!(checked_next_sequence(u64::MAX), None);
}
