//! JSON encoding for the loopback API.
//!
//! Enum names use the Rust `Debug` forms and are NOT a stable contract.
//! All diagnostics are static; rejected or untrusted text is never echoed,
//! so free-form model/tool content crosses only inside the event stream
//! fields the runtime already bounded and the policy already admitted.

use nexus_core::{
    AgentError, ApprovalNotice, CommandReply, EventPayload, OutcomeSummary, RunEvent, RunFinished,
    RunLifecycle, Snapshot, ToolOutcome, Usage,
};
use serde_json::{Value, json};

/// Lowercase wire name for a terminal execution outcome.
#[must_use]
pub fn health_json() -> Value {
    serde_json::json!({ "status": "ok", "scope": "loopback test-only demo" })
}

/// Lowercase wire name for a terminal execution outcome.
#[must_use]
pub fn outcome_name(outcome: nexus_core::RunOutcome) -> &'static str {
    match outcome {
        nexus_core::RunOutcome::Completed => "completed",
        nexus_core::RunOutcome::Failed => "failed",
        nexus_core::RunOutcome::Refused => "refused",
        nexus_core::RunOutcome::Cancelled => "cancelled",
        nexus_core::RunOutcome::LimitReached => "limit-reached",
    }
}

/// HTTP status for a runtime reply: acceptance succeeds, `Busy` and settled
/// targets conflict, unknown targets are missing, rejections are bad input.
#[must_use]
pub fn reply_status(reply: CommandReply) -> u16 {
    match reply {
        CommandReply::Accepted => 200,
        CommandReply::Busy | CommandReply::AlreadyFinalized => 409,
        CommandReply::Rejected => 400,
        CommandReply::StaleOrUnknownTarget => 404,
    }
}

/// Lowercase wire name for a reply, for machine-readable bodies.
#[must_use]
pub fn reply_name(reply: CommandReply) -> &'static str {
    match reply {
        CommandReply::Accepted => "accepted",
        CommandReply::Busy => "busy",
        CommandReply::Rejected => "rejected",
        CommandReply::StaleOrUnknownTarget => "stale-or-unknown-target",
        CommandReply::AlreadyFinalized => "already-finalized",
    }
}

/// Encodes a typed execution error as category plus retry guidance. The
/// message is static by construction (the core marker net rejects anything
/// else), so it is safe to carry.
#[must_use]
pub fn error_json(error: &AgentError) -> Value {
    json!({
        "category": format!("{:?}", error.category()),
        "retry": format!("{:?}", error.retry()),
        "message": error.message(),
    })
}

/// Encodes usage counters; unknown stays `null`, never zero.
#[must_use]
pub fn usage_json(usage: &Usage) -> Value {
    json!({
        "input": usage.input_tokens(),
        "output": usage.output_tokens(),
        "finality": format!("{:?}", usage.finality()),
    })
}

/// Encodes a recorded tool outcome with its truncation flag.
#[must_use]
pub fn outcome_json(outcome: &ToolOutcome) -> Value {
    json!({
        "status": format!("{:?}", outcome.status()),
        "effect": format!("{:?}", outcome.effect()),
        "evidence": format!("{:?}", outcome.evidence()),
        "content": outcome.content(),
        "truncated": outcome.is_truncated(),
    })
}

/// Encodes a terminal record with its typed error, if any.
#[must_use]
pub fn finished_json(finished: &RunFinished) -> Value {
    json!({
        "outcome": outcome_name(finished.outcome()),
        "persistence": format!("{:?}", finished.persistence()),
        "error": finished.error().map(error_json),
    })
}

/// Encodes an approval notice: the exact identity a decision must bind,
// plus the pre-redacted safe text the runtime published.
#[must_use]
pub fn notice_json(notice: &ApprovalNotice) -> Value {
    json!({
        "approval": notice.approval.as_str(),
        "call": notice.call.as_str(),
        "summary": notice.summary,
        "scope": notice.scope_summary,
        "args_preview": notice.args_preview,
        "session_directory": notice.session_directory,
    })
}

/// Encodes one run event. `terminal` is true only for the run's terminal
/// outcome; clients close their stream on it.
#[must_use]
pub fn event_json(event: &RunEvent) -> Value {
    let payload = event.payload();
    let (kind, detail) = match payload {
        EventPayload::RunStarted { request } => {
            ("run-started", json!({ "request": request.as_str() }))
        }
        EventPayload::AssistantTextDelta(fragment) => (
            "assistant-text",
            json!({ "turn": fragment.turn.as_str(), "item": fragment.item_key, "text": fragment.text }),
        ),
        EventPayload::ToolCallPreview { item_key } => {
            ("tool-call-preview", json!({ "item": item_key }))
        }
        EventPayload::ApprovalRequired(notice) => ("approval-required", notice_json(notice)),
        EventPayload::ToolStarted(info) => (
            "tool-started",
            json!({
                "call": info.call.as_str(),
                "tool": info.tool.name(),
                "revision": info.tool.revision(),
                "args_preview": info.args_preview,
            }),
        ),
        EventPayload::ToolOutput(progress) => (
            "tool-output",
            json!({ "call": progress.call.as_str(), "preview": progress.preview, "truncated": progress.truncated }),
        ),
        EventPayload::ToolFinished(info) => (
            "tool-finished",
            json!({ "call": info.call.as_str(), "outcome": outcome_json(&info.outcome) }),
        ),
        EventPayload::UsageUpdated(usage) => ("usage", usage_json(usage)),
        EventPayload::RunFinished(finished) => ("run-finished", finished_json(finished)),
    };
    json!({
        "seq": event.seq(),
        "run": event.run().as_str(),
        "kind": kind,
        "terminal": event.is_terminal(),
        "detail": detail,
    })
}

/// Encodes a bounded snapshot: lifecycle, last sequence, pending approval
/// identities, and known outcome summaries. Never a replay log.
#[must_use]
pub fn snapshot_json(snapshot: &Snapshot) -> Value {
    let (lifecycle, outcome) = match snapshot.lifecycle() {
        RunLifecycle::Active => ("active", Value::Null),
        RunLifecycle::Finalized(finished) => (
            "finalized",
            Value::String(outcome_name(finished).to_owned()),
        ),
    };
    json!({
        "run": snapshot.run().as_str(),
        "lifecycle": lifecycle,
        "outcome": outcome,
        "last_sequence": snapshot.last_sequence(),
        "pending_approvals": snapshot.pending_approvals().iter().map(|id| id.as_str()).collect::<Vec<_>>(),
        "known_outcomes": snapshot.known_outcomes().iter().map(outcome_summary_json).collect::<Vec<_>>(),
        "content_truncated": snapshot.is_content_truncated(),
    })
}

/// Encodes one known outcome summary from a snapshot.
#[must_use]
pub fn outcome_summary_json(summary: &OutcomeSummary) -> Value {
    json!({
        "call": summary.call.as_str(),
        "status": format!("{:?}", summary.status),
        "effect": format!("{:?}", summary.effect),
        "evidence": format!("{:?}", summary.evidence),
    })
}

/// Builds a static client-error body.
#[must_use]
pub fn error_body(message: &'static str) -> Vec<u8> {
    serde_json::to_vec(&json!({ "error": message })).expect("static error body encodes")
}
