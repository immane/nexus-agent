#![forbid(unsafe_code)]

//! M0 headless composition root: fake-wired, test-only.
//!
//! The binary owns the M0 composition root for headless mode: it builds the
//! runtime with a scripted [`FakeProvider`](nexus_fakes::FakeProvider) plus
//! [`FakeTool`](nexus_fakes::FakeTool) set, submits one task taken from
//! argv, drains both event channels to the terminal [`RunOutcome`], and
//! exits. No approval handler is configured, so confirmation-required calls
//! exercise the denial path (denied, never executed, never hung).
//!
//! Output contract (unstable test-only revision `m0-test-0`, see
//! [`OUTPUT_REV`]): stdout carries ONLY machine-readable event/result lines,
//! one record per line, each starting with `rev=m0-test-0 `. Fields are
//! space-separated `key=value` pairs; values never contain spaces, `=`,
//! newlines, or escape codes (see [`sanitize`]). No `Debug` formatting is
//! used. Event lines are sorted by per-run sequence before printing.
//! Diagnostics and [`FAKE_BANNER`] go to stderr only.
//!
//! Task routing (test-only stand-in, not a product default): the task string
//! is submitted verbatim as the run input; the fake script is picked by
//! prefix: `deny: ...` proposes a `host_write` call (denial path), `refuse:
//! ...` refuses, anything else answers with a `host_read` call plus text
//! (completed path).
//!
//! Exit-code mapping: 0 = completed with no denied calls; 3 = completed run
//! containing denied calls (headless denial); 1 = failed or refused; 4 =
//! cancelled; 5 = limit reached; 2 = usage error (missing/empty/oversize
//! task, rejected submit). A denied tool call surfaces as `Completed` at
//! run level with a `Denied`/`NotStarted` tool outcome, hence the distinct
//! code 3.

use std::sync::Arc;
use std::time::Duration;

use nexus_core::{
    CommandReply, EventPayload, Limits, RequestId, RunEvent, RunFinished, RunOutcome, SessionId,
    SubmitCommand,
};
use nexus_fakes::{FakeProvider, FakeTool, candidate, stop_turn, tool_turn};
use nexus_runtime::{Policy, Runtime, RuntimeConfig};

/// Unstable test-only output revision. Parsers must exact-match this.
pub const OUTPUT_REV: &str = "m0-test-0";

/// Self-identifying banner, printed to stderr (never stdout). A missing real
/// configuration must never look like a real successful task.
pub const FAKE_BANNER: &str = "nexus-headless: FAKE TEST-ONLY wiring rev m0-test-0 (scripted FakeProvider + FakeTool, ephemeral, NO approval handler; confirmation-required calls are denied). This is never a real task run.";

/// Usage hint for stderr diagnostics.
pub const USAGE: &str = "usage: nexus-headless <task>; prefix with 'deny:' to exercise headless denial, 'refuse:' for refusal";

/// Terminal report for one headless task.
pub struct HeadlessReport {
    /// Host-issued run id, for stderr diagnostics.
    pub run: String,
    /// Stdout lines: sequenced event lines plus one trailing result line.
    pub lines: Vec<String>,
    /// Terminal run outcome.
    pub outcome: RunOutcome,
    /// Tool-finished outcomes with `Denied` status.
    pub denied_calls: usize,
    /// `ToolStarted` events observed (must be 0 on the denial path).
    pub tool_started: usize,
    /// `ApprovalRequired` events observed (must be 0 on the denial path).
    pub approval_required: usize,
    /// `ToolFinished` events observed.
    pub tool_finished: usize,
    /// Process exit code per the mapping documented above.
    pub exit_code: i32,
}

/// Maps a terminal outcome (plus denial count) to a process exit code.
#[must_use]
pub fn exit_code_for(outcome: RunOutcome, denied_calls: usize) -> i32 {
    match outcome {
        RunOutcome::Completed if denied_calls == 0 => 0,
        RunOutcome::Completed => 3,
        RunOutcome::Failed | RunOutcome::Refused => 1,
        RunOutcome::Cancelled => 4,
        RunOutcome::LimitReached => 5,
    }
}

/// Lowercase wire name for a run outcome (no `Debug` formatting).
#[must_use]
pub fn outcome_name(outcome: RunOutcome) -> &'static str {
    match outcome {
        RunOutcome::Completed => "completed",
        RunOutcome::Refused => "refused",
        RunOutcome::Failed => "failed",
        RunOutcome::Cancelled => "cancelled",
        RunOutcome::LimitReached => "limit-reached",
    }
}

/// Makes free text safe for `key=value` stdout lines: the output alphabet
/// excludes whitespace, `=`, escape codes, and all other control characters,
/// so stdout can never carry terminal escapes.
#[must_use]
pub fn sanitize(raw: &str) -> String {
    raw.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | ',' | '-' | '_' | '+' | '/' | ':' => c,
            ' ' => '_',
            _ => '?',
        })
        .collect()
}

/// Submits `task`, drains both event channels to the terminal outcome, and
/// builds the stdout lines plus exit code. Returns `Err` for invalid input
/// or a rejected submit (caller maps to exit code 2).
pub fn run_task(task: &str) -> Result<HeadlessReport, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("test runtime failed: {error}"))?;
    runtime.block_on(run_task_async(task))
}

async fn run_task_async(task: &str) -> Result<HeadlessReport, String> {
    let provider = Arc::new(FakeProvider::new(script_for(task)));
    let tools: Vec<Arc<dyn nexus_core::ToolPort + Send + Sync>> = vec![
        Arc::new(FakeTool::read_only()),
        Arc::new(FakeTool::mutation()),
        Arc::new(FakeTool::command()),
    ];
    let (runtime, mut streams) = Runtime::new(
        RuntimeConfig {
            limits: Limits::m0_test(),
            policy: Policy::m0_test(),
            has_approval_handler: false,
        },
        provider,
        tools,
    );
    let submit = SubmitCommand::new(
        RequestId::new("req-headless-1").map_err(|_| "request id invalid".to_owned())?,
        SessionId::new("sess-headless-1").map_err(|_| "session id invalid".to_owned())?,
        task,
        "m0-test",
    )
    .map_err(|_| "task input is empty or exceeds the input budget".to_owned())?;
    let response = runtime.submit(submit).await;
    if response.reply() != CommandReply::Accepted {
        return Err("run submit was not accepted".to_owned());
    }
    let run = response
        .run()
        .cloned()
        .expect("accepted submit issues a run");

    let mut events: Vec<RunEvent> = Vec::new();
    let terminal: RunFinished = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            tokio::select! {
                event = streams.data.recv() => {
                    if let Some(event) = event {
                        events.push(event);
                    }
                }
                event = streams.control.recv() => {
                    match event {
                        None => return None,
                        Some(event) => {
                            let done = matches!(event.payload(), EventPayload::RunFinished(_));
                            let finished = if done {
                                match event.payload() {
                                    EventPayload::RunFinished(finished) => Some(finished.clone()),
                                    _ => None,
                                }
                            } else {
                                None
                            };
                            events.push(event);
                            if let Some(finished) = finished {
                                return Some(finished);
                            }
                        }
                    }
                }
            }
        }
    })
    .await
    .map_err(|_| "run did not reach a terminal outcome in time".to_owned())?
    .ok_or_else(|| "control channel closed before terminal outcome".to_owned())?;
    while let Ok(event) = streams.data.try_recv() {
        events.push(event);
    }
    events.sort_by_key(RunEvent::seq);

    let mut tool_started = 0;
    let mut approval_required = 0;
    let mut tool_finished = 0;
    let mut denied_calls = 0;
    let mut lines: Vec<String> = Vec::with_capacity(events.len() + 1);
    for event in &events {
        match event.payload() {
            EventPayload::ToolStarted(_) => tool_started += 1,
            EventPayload::ApprovalRequired(_) => approval_required += 1,
            EventPayload::ToolFinished(info) => {
                tool_finished += 1;
                if info.outcome.status() == nexus_core::ExecutionStatus::Denied {
                    denied_calls += 1;
                }
            }
            _ => {}
        }
        lines.push(format_event(event));
    }
    let outcome = terminal.outcome();
    lines.push(format!(
        "rev={OUTPUT_REV} type=result outcome={} denied={denied_calls} tool-started={tool_started} tool-finished={tool_finished} run={}",
        outcome_name(outcome),
        run.as_str(),
    ));
    let exit_code = exit_code_for(outcome, denied_calls);
    Ok(HeadlessReport {
        run: run.as_str().to_owned(),
        lines,
        outcome,
        denied_calls,
        tool_started,
        approval_required,
        tool_finished,
        exit_code,
    })
}

fn script_for(task: &str) -> Vec<Vec<nexus_core::ProviderEvent>> {
    if let Some(_rest) = task.strip_prefix("deny:") {
        vec![
            tool_turn(vec![candidate(
                "item-1",
                "prov-ref-1",
                "host_write",
                r#"{"path":"dst"}"#,
            )]),
            stop_turn("done-after-denial"),
        ]
    } else if task.strip_prefix("refuse:").is_some() {
        vec![vec![nexus_core::ProviderEvent::TurnFinished(
            nexus_core::TurnFinished::new(
                nexus_core::FinishReason::Refusal,
                nexus_core::Usage::new(None, None, nexus_core::UsageFinality::Final),
                None,
            ),
        )]]
    } else {
        vec![
            tool_turn(vec![candidate(
                "item-1",
                "prov-ref-1",
                "host_read",
                r#"{"path":"src"}"#,
            )]),
            stop_turn("fake-answer"),
        ]
    }
}

fn format_event(event: &RunEvent) -> String {
    let head = format!(
        "rev={OUTPUT_REV} type=event seq={} run={} kind=",
        event.seq(),
        event.run().as_str(),
    );
    match event.payload() {
        EventPayload::RunStarted { request } => {
            format!("{head}run-started request={}", request.as_str())
        }
        EventPayload::AssistantTextDelta(text) => format!(
            "{}text item={} text-len={} text={}",
            head,
            sanitize(&text.item_key),
            text.text.len(),
            sanitize(&text.text),
        ),
        EventPayload::ToolCallPreview { item_key } => {
            format!("{head}preview item={}", sanitize(item_key))
        }
        EventPayload::ApprovalRequired(notice) => format!(
            "{head}approval-required approval={} call={} scope={}",
            notice.approval.as_str(),
            notice.call.as_str(),
            sanitize(&notice.scope_summary),
        ),
        EventPayload::ToolStarted(info) => {
            format!("{head}tool-started call={}", info.call.as_str())
        }
        EventPayload::ToolOutput(progress) => format!(
            "{head}tool-output call={} truncated={} preview-len={}",
            progress.call.as_str(),
            progress.truncated,
            progress.preview.len(),
        ),
        EventPayload::ToolFinished(info) => {
            let outcome = &info.outcome;
            format!(
                "{head}tool-finished call={} status={} effect={} evidence={} truncated={} content-len={} content={}",
                info.call.as_str(),
                execution_name(outcome.status()),
                effect_name(outcome.effect()),
                evidence_name(outcome.evidence()),
                outcome.is_truncated(),
                outcome.content().len(),
                sanitize(outcome.content()),
            )
        }
        EventPayload::UsageUpdated(usage) => format!(
            "{head}usage input={} output={} finality={}",
            usage_tokens(usage.input_tokens()),
            usage_tokens(usage.output_tokens()),
            match usage.finality() {
                nexus_core::UsageFinality::Provisional => "provisional",
                nexus_core::UsageFinality::Final => "final",
            },
        ),
        EventPayload::RunFinished(finished) => format!(
            "{head}run-finished outcome={} persistence={}",
            outcome_name(finished.outcome()),
            match finished.persistence() {
                nexus_core::PersistenceState::Ephemeral => "ephemeral",
                nexus_core::PersistenceState::Saved => "saved",
                nexus_core::PersistenceState::SaveFailed => "save-failed",
            },
        ),
    }
}

fn execution_name(status: nexus_core::ExecutionStatus) -> &'static str {
    match status {
        nexus_core::ExecutionStatus::Succeeded => "succeeded",
        nexus_core::ExecutionStatus::Failed => "failed",
        nexus_core::ExecutionStatus::Denied => "denied",
        nexus_core::ExecutionStatus::Cancelled => "cancelled",
        nexus_core::ExecutionStatus::TimedOut => "timed-out",
    }
}

fn effect_name(effect: nexus_core::EffectState) -> &'static str {
    match effect {
        nexus_core::EffectState::NotStarted => "not-started",
        nexus_core::EffectState::KnownNotApplied => "known-not-applied",
        nexus_core::EffectState::KnownApplied => "known-applied",
        nexus_core::EffectState::Unknown => "unknown",
    }
}

fn evidence_name(evidence: nexus_core::Evidence) -> &'static str {
    match evidence {
        nexus_core::Evidence::HostObserved => "host-observed",
        nexus_core::Evidence::PluginReported => "plugin-reported",
        nexus_core::Evidence::Uncertain => "uncertain",
    }
}

fn usage_tokens(tokens: Option<u64>) -> String {
    tokens.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_stdout_hygiene(lines: &[String]) {
        assert!(!lines.is_empty());
        for line in lines {
            assert!(
                line.starts_with("rev=m0-test-0 "),
                "every stdout line carries the rev: {line}"
            );
            assert!(!line.contains('\x1b'), "no escape codes: {line}");
            assert!(
                !line.contains('\n') && !line.contains('\r'),
                "single line each: {line}"
            );
            assert!(!line.contains("FAKE"), "no banner leakage: {line}");
            for field in line.split(' ').skip(1) {
                assert!(field.contains('='), "key=value fields: {line}");
            }
        }
    }

    #[test]
    fn completed_path_executes_read_tool() {
        let report = run_task("hello").expect("completed script runs");
        assert_eq!(report.outcome, RunOutcome::Completed);
        assert_eq!(report.denied_calls, 0);
        assert_eq!(report.tool_started, 1);
        assert_eq!(report.tool_finished, 1);
        assert_eq!(report.exit_code, 0);
        assert_stdout_hygiene(&report.lines);
        assert!(
            report
                .lines
                .last()
                .expect("result")
                .contains("outcome=completed")
        );
    }

    #[test]
    fn denial_path_has_no_tool_started_or_approval() {
        let report = run_task("deny: write it").expect("denial script runs");
        assert_eq!(report.outcome, RunOutcome::Completed);
        assert_eq!(report.tool_started, 0, "denied calls never start");
        assert_eq!(report.approval_required, 0, "no handler consumes approvals");
        assert_eq!(report.denied_calls, 1);
        assert_eq!(report.tool_finished, 1);
        let finished = report
            .lines
            .iter()
            .find(|line| line.contains("kind=tool-finished"))
            .expect("denial recorded");
        assert!(finished.contains("status=denied"), "{finished}");
        assert!(finished.contains("effect=not-started"), "{finished}");
        assert_eq!(report.exit_code, 3);
        assert_stdout_hygiene(&report.lines);
    }

    #[test]
    fn refusal_path_maps_to_exit_one() {
        let report = run_task("refuse: no").expect("refusal script runs");
        assert_eq!(report.outcome, RunOutcome::Refused);
        assert_eq!(report.exit_code, 1);
        assert_stdout_hygiene(&report.lines);
    }

    #[test]
    fn empty_and_oversize_tasks_are_rejected() {
        assert!(run_task("").is_err());
        assert!(run_task(&"x".repeat(nexus_core::commands::MAX_INPUT_BYTES + 1)).is_err());
    }

    #[test]
    fn exit_code_mapping_covers_all_outcomes() {
        assert_eq!(exit_code_for(RunOutcome::Completed, 0), 0);
        assert_eq!(exit_code_for(RunOutcome::Completed, 2), 3);
        assert_eq!(exit_code_for(RunOutcome::Failed, 0), 1);
        assert_eq!(exit_code_for(RunOutcome::Refused, 0), 1);
        assert_eq!(exit_code_for(RunOutcome::Cancelled, 0), 4);
        assert_eq!(exit_code_for(RunOutcome::LimitReached, 0), 5);
    }

    #[test]
    fn sanitize_strips_escapes_newlines_and_separators() {
        let dirty = "a\x1bb\nc\rd=e f\"g";
        let clean = sanitize(dirty);
        assert!(!clean.contains('\x1b'));
        assert!(!clean.contains(['\n', '\r', ' ', '=']));
        assert!(clean.contains('?'));
        assert_eq!(sanitize("run-1"), "run-1");
    }

    #[test]
    fn banner_self_identifies_fake_wiring() {
        assert!(FAKE_BANNER.contains("FAKE"));
        assert!(FAKE_BANNER.contains("TEST-ONLY"));
        assert!(FAKE_BANNER.contains("NO approval handler"));
    }
}
