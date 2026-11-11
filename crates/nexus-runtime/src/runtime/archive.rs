//! Read-only retrieval of original tool output captured in the active
//! session's source history. Retrieval never dispatches a tool and never
//! accepts an arbitrary filesystem path.

use std::sync::{Arc, Mutex as StdMutex};

use nexus_core::{
    AgentError, CallId, EffectState, ErrorCategory, Evidence, ExecutionStatus, M0_REVISION,
    ModelContextItem, RetryGuidance, SessionStore, ToolCall, ToolContext, ToolId, ToolOutcome,
    ToolPort, ToolSpec,
};

pub(super) const HOST_ARCHIVE_TOOL: &str = "host_archive";
const DEFAULT_PAGE_BYTES: usize = 8192;
const MAX_PAGE_BYTES: usize = 16_384;
const MAX_QUERY_BYTES: usize = 512;
const MAX_SEARCH_MATCHES: usize = 32;
const MAX_SEARCH_LINE_BYTES: usize = 512;
const ARCHIVE_SCHEMA: &str = r#"{"type":"object","properties":{"output_ref":{"type":"string","minLength":1,"maxLength":64},"query":{"type":"string","minLength":1,"maxLength":512},"offset":{"type":"number","minimum":0,"maximum":9223372036854775807},"limit":{"type":"number","minimum":4,"maximum":16384}},"required":["output_ref"],"additionalProperties":false}"#;

type Store = Arc<StdMutex<Box<dyn SessionStore + Send>>>;

pub(super) struct ArchivePort {
    store: Store,
    spec: ToolSpec,
}

impl ArchivePort {
    pub(super) fn new(store: Store) -> Result<Self, AgentError> {
        let spec = ToolSpec::new(
            ToolId::new(HOST_ARCHIVE_TOOL, M0_REVISION).expect("static archive tool identity"),
            "Read or search the original captured output of an earlier tool call in this session. Use output_ref from a bounded tool-output projection. offset and limit are bytes; query searches captured lines. This never re-executes the tool or reads a host path.",
            ARCHIVE_SCHEMA,
        )?;
        Ok(Self { store, spec })
    }

    fn execute_archive(&self, call: &ToolCall, context: &ToolContext) -> ToolOutcome {
        if context.is_cancelled() {
            return failure(
                ExecutionStatus::Cancelled,
                EffectState::Unknown,
                Evidence::Uncertain,
                "archive retrieval was cancelled",
            );
        }
        let (session, requested) = match (context.session(), parse_arguments(call.args().as_str()))
        {
            (None, _) | (_, None) => {
                return failure(
                    ExecutionStatus::Denied,
                    EffectState::NotStarted,
                    Evidence::HostObserved,
                    "archive retrieval is not authorized for this call",
                );
            }
            (Some(session), Some(requested)) => (session, requested),
        };
        if context.run().is_none() {
            return failure(
                ExecutionStatus::Denied,
                EffectState::NotStarted,
                Evidence::HostObserved,
                "archive retrieval is not authorized for this call",
            );
        }
        let Ok(call_id) = CallId::new(requested.output_ref.clone()) else {
            return failure(
                ExecutionStatus::Denied,
                EffectState::NotStarted,
                Evidence::HostObserved,
                "archive retrieval is not authorized for this call",
            );
        };
        let record = match self
            .store
            .lock()
            .map_err(|_| archive_error(ErrorCategory::StorageFailure))
            .and_then(|store| {
                context.check_active()?;
                store.find_source_record(session, &call_id)
            }) {
            Ok(Some(record)) => record,
            Ok(None) => {
                return failure(
                    ExecutionStatus::Failed,
                    EffectState::KnownNotApplied,
                    Evidence::HostObserved,
                    "archived output was not found in this session",
                );
            }
            Err(error) if error.category() == ErrorCategory::Cancelled => {
                return failure(
                    ExecutionStatus::Cancelled,
                    EffectState::Unknown,
                    Evidence::Uncertain,
                    "archive retrieval was cancelled",
                );
            }
            Err(error) if error.category() == ErrorCategory::Timeout => {
                return failure(
                    ExecutionStatus::Failed,
                    EffectState::KnownNotApplied,
                    Evidence::Uncertain,
                    "archive retrieval timed out",
                );
            }
            Err(_) => {
                return failure(
                    ExecutionStatus::Failed,
                    EffectState::KnownNotApplied,
                    Evidence::Uncertain,
                    "archive storage is unavailable",
                );
            }
        };
        let ModelContextItem::ToolResult { outcome, .. } = record.item() else {
            unreachable!("the record filter selected only tool results");
        };
        let content = outcome.content();
        let rendered = match requested.mode {
            Mode::Range { offset, limit } => render_range(
                outcome,
                content,
                offset,
                limit,
                context.output_budget_bytes(),
            ),
            Mode::Search { query } => render_search(outcome, content, &query),
        };
        let rendered = match rendered {
            Some(rendered) => rendered,
            None => {
                return failure(
                    ExecutionStatus::Failed,
                    EffectState::KnownNotApplied,
                    Evidence::HostObserved,
                    "archive retrieval arguments are invalid or output budget is too small",
                );
            }
        };
        let truncated = outcome.is_truncated() || rendered.truncated;
        match ToolOutcome::from_bounded_content(
            ExecutionStatus::Succeeded,
            EffectState::KnownNotApplied,
            Evidence::HostObserved,
            rendered.text,
            truncated,
        ) {
            Ok(outcome) => outcome
                .enforce_budget(context.output_budget_bytes())
                .unwrap_or_else(|_| {
                    failure(
                        ExecutionStatus::Failed,
                        EffectState::KnownNotApplied,
                        Evidence::Uncertain,
                        "archive output could not be bounded",
                    )
                }),
            Err(_) => failure(
                ExecutionStatus::Failed,
                EffectState::KnownNotApplied,
                Evidence::Uncertain,
                "archive output could not be bounded",
            ),
        }
    }
}

impl ToolPort for ArchivePort {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn execute(&self, call: &ToolCall, context: &ToolContext) -> ToolOutcome {
        self.execute_archive(call, context)
    }
}

struct Requested {
    output_ref: String,
    mode: Mode,
}

enum Mode {
    Range { offset: usize, limit: usize },
    Search { query: String },
}

fn parse_arguments(raw: &str) -> Option<Requested> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let object = value.as_object()?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "output_ref" | "query" | "offset" | "limit"))
    {
        return None;
    }
    let output_ref = object.get("output_ref")?.as_str()?;
    if output_ref.is_empty()
        || output_ref.len() > 64
        || !output_ref
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return None;
    }
    if let Some(query) = object.get("query") {
        if object.contains_key("offset") || object.contains_key("limit") {
            return None;
        }
        let query = query.as_str()?;
        if query.is_empty() || query.len() > MAX_QUERY_BYTES || query.contains('\0') {
            return None;
        }
        return Some(Requested {
            output_ref: output_ref.to_owned(),
            mode: Mode::Search {
                query: query.to_owned(),
            },
        });
    }
    let offset = match object.get("offset") {
        Some(value) => usize::try_from(value.as_u64()?).ok()?,
        None => 0,
    };
    let limit = match object.get("limit") {
        Some(value) => usize::try_from(value.as_u64()?).ok()?,
        None => DEFAULT_PAGE_BYTES,
    };
    (offset <= i64::MAX as usize && (4..=MAX_PAGE_BYTES).contains(&limit)).then_some(Requested {
        output_ref: output_ref.to_owned(),
        mode: Mode::Range { offset, limit },
    })
}

struct Rendered {
    text: String,
    truncated: bool,
}

fn render_range(
    outcome: &ToolOutcome,
    content: &str,
    offset: usize,
    limit: usize,
    budget: usize,
) -> Option<Rendered> {
    if offset > content.len() || !content.is_char_boundary(offset) {
        return None;
    }
    let mut text = header(outcome, content.len(), "range");
    // Reserve the largest possible cursor before selecting bytes, so the
    // returned cursor always describes the content actually delivered.
    let footer = format!(
        "\n[archived output continues; next_offset={}]",
        content.len()
    );
    let remaining = content.len().saturating_sub(offset);
    let available = budget.checked_sub(text.len())?;
    let capacity = if remaining <= limit && remaining <= available {
        remaining
    } else {
        limit
            .min(remaining)
            .min(available.checked_sub(footer.len())?)
    };
    let mut end = offset.saturating_add(capacity);
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    let more = end < content.len();
    if more && end == offset {
        return None;
    }
    text.push_str(&content[offset..end]);
    if more {
        text.push_str(&format!("\n[archived output continues; next_offset={end}]"));
    }
    Some(Rendered {
        text,
        truncated: more,
    })
}

fn render_search(outcome: &ToolOutcome, content: &str, query: &str) -> Option<Rendered> {
    let mut text = header(outcome, content.len(), "search");
    text.push_str(&format!("\nquery={query:?}\n"));
    let mut matched = 0;
    let mut shown = 0;
    let mut line_truncated = false;
    let mut line_offset = 0usize;
    for (index, raw_line) in content.split_inclusive('\n').enumerate() {
        let line = raw_line.trim_end_matches('\n').trim_end_matches('\r');
        let start_offset = line_offset;
        line_offset += raw_line.len();
        let Some(found) = line.find(query) else {
            continue;
        };
        matched += 1;
        if shown >= MAX_SEARCH_MATCHES {
            continue;
        }
        let context = (MAX_SEARCH_LINE_BYTES - query.len()) / 2;
        let mut start = found.saturating_sub(context);
        while !line.is_char_boundary(start) {
            start -= 1;
        }
        let (snippet, truncated) = truncate_utf8(&line[start..], MAX_SEARCH_LINE_BYTES);
        let truncated = truncated || start > 0;
        line_truncated |= truncated;
        text.push_str(&format!(
            "line {} byte_offset={}: {snippet}\n",
            index + 1,
            start_offset + start
        ));
        shown += 1;
    }
    text.push_str(&format!(
        "matches={matched} shown={shown}{}\n",
        if matched > shown {
            "; narrower query or archived range may be needed"
        } else {
            ""
        }
    ));
    Some(Rendered {
        text,
        truncated: matched > shown || line_truncated,
    })
}

fn header(outcome: &ToolOutcome, captured_bytes: usize, mode: &str) -> String {
    format!(
        "[archived tool output; not a new execution. mode={mode} status={:?} effect={:?} evidence={:?} original_output_truncated={} captured_bytes={captured_bytes}]\n",
        outcome.status(),
        outcome.effect(),
        outcome.evidence(),
        outcome.is_truncated(),
    )
}

fn truncate_utf8(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_owned(), false);
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}... [line truncated]", &text[..end]), true)
}

fn failure(
    status: ExecutionStatus,
    effect: EffectState,
    evidence: Evidence,
    content: &'static str,
) -> ToolOutcome {
    ToolOutcome::new(status, effect, evidence, content, false)
        .expect("static archive failure builds")
}

fn archive_error(category: ErrorCategory) -> AgentError {
    AgentError::new(
        category,
        "archive storage is unavailable",
        RetryGuidance::DoNotRetry,
    )
    .expect("static archive error builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::{
        ApprovedScope, Limits, RunId, SessionCheckpoint, SessionId, SourceRecord, ToolCall,
        ToolIntentRecord, ToolOutcomeRecord,
    };
    use std::time::Duration;

    struct MemoryStore {
        records: Vec<SourceRecord>,
    }

    impl SessionStore for MemoryStore {
        fn list_sessions(&self, _: usize) -> Result<Vec<nexus_core::SessionMetadata>, AgentError> {
            Ok(Vec::new())
        }
        fn load_session(&self, _: &SessionId) -> Result<SessionCheckpoint, AgentError> {
            Err(archive_error(ErrorCategory::StorageFailure))
        }
        fn save_checkpoint(&mut self, _: &SessionCheckpoint) -> Result<(), AgentError> {
            Ok(())
        }
        fn record_intent(&mut self, _: &ToolIntentRecord) -> Result<(), AgentError> {
            Ok(())
        }
        fn record_outcome(&mut self, _: &ToolOutcomeRecord) -> Result<(), AgentError> {
            Ok(())
        }
        fn load_source_records(
            &self,
            session: &SessionId,
            after_sequence: u64,
            limit: usize,
        ) -> Result<Vec<SourceRecord>, AgentError> {
            Ok(self
                .records
                .iter()
                .filter(|record| record.session() == session && record.sequence() > after_sequence)
                .take(limit)
                .cloned()
                .collect())
        }
    }

    fn session() -> SessionId {
        SessionId::new("archive-session").unwrap()
    }

    fn run() -> RunId {
        RunId::new("archive-run").unwrap()
    }

    fn call() -> ToolCall {
        ToolCall::new(
            run(),
            nexus_core::TurnId::new("turn-1").unwrap(),
            nexus_core::CallId::new("call-1").unwrap(),
            ToolId::new("host_exec", M0_REVISION).unwrap(),
            nexus_core::NormalizedArgs::new(r#"{"argv":["x"]}"#).unwrap(),
        )
    }

    fn context() -> ToolContext {
        ToolContext::new(
            Limits::M0_TEST_TOOL_OUTPUT_BYTES,
            Duration::from_secs(30),
            false,
            ApprovedScope::new("archive:call-1").unwrap(),
        )
        .unwrap()
        .with_source_scope(session(), run())
    }

    fn port(content: &str) -> ArchivePort {
        let outcome = ToolOutcome::new(
            ExecutionStatus::Failed,
            EffectState::Unknown,
            Evidence::HostObserved,
            content,
            false,
        )
        .unwrap();
        let item = ModelContextItem::tool_result(
            nexus_core::CallId::new("call-1").unwrap(),
            "item-1",
            "ref-1",
            call().tool().clone(),
            outcome,
        )
        .unwrap();
        let record = SourceRecord::new(session(), 1, run(), item).unwrap();
        ArchivePort::new(Arc::new(StdMutex::new(Box::new(MemoryStore {
            records: vec![record],
        }))))
        .unwrap()
    }

    #[test]
    fn range_and_search_read_the_original_capture_without_execution() {
        let original = format!(
            "head\n{}\nfailure: missing symbol\ntail",
            "x".repeat(20_000)
        );
        let port = port(&original);
        let range = port.execute(
            &ToolCall::new(
                run(),
                nexus_core::TurnId::new("turn-2").unwrap(),
                nexus_core::CallId::new("read-1").unwrap(),
                ToolId::new(HOST_ARCHIVE_TOOL, M0_REVISION).unwrap(),
                nexus_core::NormalizedArgs::new(r#"{"output_ref":"call-1","limit":8192}"#).unwrap(),
            ),
            &context(),
        );
        assert_eq!(range.status(), ExecutionStatus::Succeeded);
        assert!(range.content().contains("mode=range"));
        assert!(range.content().contains("head"));
        assert!(range.content().contains("next_offset="));
        assert!(range.is_truncated());
        let search = port.execute(
            &ToolCall::new(
                run(),
                nexus_core::TurnId::new("turn-3").unwrap(),
                nexus_core::CallId::new("read-2").unwrap(),
                ToolId::new(HOST_ARCHIVE_TOOL, M0_REVISION).unwrap(),
                nexus_core::NormalizedArgs::new(
                    r#"{"output_ref":"call-1","query":"missing symbol"}"#,
                )
                .unwrap(),
            ),
            &context(),
        );
        assert!(search.content().contains("matches=1 shown=1"));
        assert!(search.content().contains("failure: missing symbol"));
    }

    #[test]
    fn retrieval_survives_a_store_reopen_without_replaying_the_tool() {
        let directory = std::env::temp_dir().join(format!(
            "nexus-archive-durable-{}-{}",
            std::process::id(),
            super::super::next_incarnation()
        ));
        let original = "durable head\nmiddle-marker\ndurable tail";
        let item = ModelContextItem::tool_result(
            nexus_core::CallId::new("call-1").unwrap(),
            "item-1",
            "ref-1",
            call().tool().clone(),
            ToolOutcome::new(
                ExecutionStatus::Succeeded,
                EffectState::KnownNotApplied,
                Evidence::HostObserved,
                original,
                false,
            )
            .unwrap(),
        )
        .unwrap();
        {
            let mut store = nexus_session::FileSessionStore::open(&directory).unwrap();
            store
                .initialize_session(
                    &SessionCheckpoint::new(
                        session(),
                        nexus_core::STORE_FORMAT_REVISION,
                        0,
                        Vec::new(),
                        "profile",
                    )
                    .unwrap(),
                )
                .unwrap();
            store
                .append_source_items(&session(), &run(), &[item])
                .unwrap();
        }
        let reopened = nexus_session::FileSessionStore::open(&directory).unwrap();
        let port = ArchivePort::new(Arc::new(StdMutex::new(
            Box::new(reopened) as Box<dyn SessionStore + Send>
        )))
        .unwrap();
        let found = port.execute(
            &ToolCall::new(
                run(),
                nexus_core::TurnId::new("turn-durable").unwrap(),
                nexus_core::CallId::new("read-durable").unwrap(),
                ToolId::new(HOST_ARCHIVE_TOOL, M0_REVISION).unwrap(),
                nexus_core::NormalizedArgs::new(
                    r#"{"output_ref":"call-1","query":"middle-marker"}"#,
                )
                .unwrap(),
            ),
            &context(),
        );
        assert_eq!(found.status(), ExecutionStatus::Succeeded);
        assert!(found.content().contains("middle-marker"));
        assert!(!found.content().contains("durable head"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn search_reports_line_truncation_even_when_the_match_count_fits() {
        let port = port(&format!("prefix {} suffix", "x".repeat(2000)));
        let search = port.execute(
            &ToolCall::new(
                run(),
                nexus_core::TurnId::new("turn-long-line").unwrap(),
                nexus_core::CallId::new("read-long-line").unwrap(),
                ToolId::new(HOST_ARCHIVE_TOOL, M0_REVISION).unwrap(),
                nexus_core::NormalizedArgs::new(r#"{"output_ref":"call-1","query":"prefix"}"#)
                    .unwrap(),
            ),
            &context(),
        );
        assert_eq!(search.status(), ExecutionStatus::Succeeded);
        assert!(search.content().contains("[line truncated]"));
        assert!(search.is_truncated());
    }

    #[test]
    fn range_cursor_tracks_bytes_delivered_with_a_small_output_budget() {
        let original = "x".repeat(20_000);
        let port = port(&original);
        let small_context = ToolContext::new(
            4096,
            Duration::from_secs(30),
            false,
            ApprovedScope::new("archive:call-1").unwrap(),
        )
        .unwrap()
        .with_source_scope(session(), run());
        let request = |offset| {
            ToolCall::new(
                run(),
                nexus_core::TurnId::new("turn-page").unwrap(),
                nexus_core::CallId::new("read-page").unwrap(),
                ToolId::new(HOST_ARCHIVE_TOOL, M0_REVISION).unwrap(),
                nexus_core::NormalizedArgs::new(format!(
                    r#"{{"output_ref":"call-1","offset":{offset}}}"#
                ))
                .unwrap(),
            )
        };
        let first = port.execute(&request(0), &small_context);
        assert_eq!(first.status(), ExecutionStatus::Succeeded);
        let (body, cursor) = first
            .content()
            .split_once('\n')
            .unwrap()
            .1
            .split_once("\n[archived output continues; next_offset=")
            .unwrap();
        let next: usize = cursor.trim_end_matches(']').parse().unwrap();
        assert_eq!(body.len(), next);
        let second = port.execute(&request(next), &small_context);
        assert_eq!(second.status(), ExecutionStatus::Succeeded);
        assert!(second.content().contains("next_offset="));
    }

    #[test]
    fn search_shows_a_match_near_the_end_of_a_long_line() {
        let original = format!("first\n{} UNIQUE_FAILURE", "x".repeat(2000));
        let outcome = ToolOutcome::new(
            ExecutionStatus::Failed,
            EffectState::KnownNotApplied,
            Evidence::HostObserved,
            original.clone(),
            false,
        )
        .unwrap();
        let result = render_search(&outcome, &original, "UNIQUE_FAILURE").unwrap();
        assert!(result.truncated);
        assert!(result.text.contains("UNIQUE_FAILURE"));
        let offset: usize = result
            .text
            .split_once("byte_offset=")
            .unwrap()
            .1
            .split_once(':')
            .unwrap()
            .0
            .parse()
            .unwrap();
        assert!(offset > "first\n".len());
        assert!(original[offset..].contains("UNIQUE_FAILURE"));
    }

    #[test]
    fn a_call_reference_cannot_read_another_sessions_archive() {
        let other_session = SessionId::new("other-session").unwrap();
        let item = ModelContextItem::tool_result(
            nexus_core::CallId::new("call-1").unwrap(),
            "item-1",
            "ref-1",
            call().tool().clone(),
            ToolOutcome::new(
                ExecutionStatus::Succeeded,
                EffectState::KnownNotApplied,
                Evidence::HostObserved,
                "other-session output",
                false,
            )
            .unwrap(),
        )
        .unwrap();
        let record = SourceRecord::new(other_session, 1, run(), item).unwrap();
        let port = ArchivePort::new(Arc::new(StdMutex::new(Box::new(MemoryStore {
            records: vec![record],
        }))))
        .unwrap();
        let found = port.execute(
            &ToolCall::new(
                run(),
                nexus_core::TurnId::new("turn-cross-session").unwrap(),
                nexus_core::CallId::new("read-cross-session").unwrap(),
                ToolId::new(HOST_ARCHIVE_TOOL, M0_REVISION).unwrap(),
                nexus_core::NormalizedArgs::new(r#"{"output_ref":"call-1"}"#).unwrap(),
            ),
            &context(),
        );
        assert_eq!(found.status(), ExecutionStatus::Failed);
        assert!(found.content().contains("not found"));
        assert!(!found.content().contains("other-session output"));
    }

    #[test]
    fn missing_scope_or_record_is_explicit_and_never_denies_unknown_effects() {
        let port = port("output");
        let no_scope = ToolContext::new(
            1024,
            Duration::from_secs(30),
            false,
            ApprovedScope::new("archive:call-1").unwrap(),
        )
        .unwrap();
        let denied = port.execute(
            &ToolCall::new(
                run(),
                nexus_core::TurnId::new("turn-4").unwrap(),
                nexus_core::CallId::new("read-3").unwrap(),
                ToolId::new(HOST_ARCHIVE_TOOL, M0_REVISION).unwrap(),
                nexus_core::NormalizedArgs::new(r#"{"output_ref":"call-1"}"#).unwrap(),
            ),
            &no_scope,
        );
        assert_eq!(denied.status(), ExecutionStatus::Denied);
        assert_eq!(denied.effect(), EffectState::NotStarted);
        let missing = port.execute(
            &ToolCall::new(
                run(),
                nexus_core::TurnId::new("turn-5").unwrap(),
                nexus_core::CallId::new("read-4").unwrap(),
                ToolId::new(HOST_ARCHIVE_TOOL, M0_REVISION).unwrap(),
                nexus_core::NormalizedArgs::new(r#"{"output_ref":"call-999"}"#).unwrap(),
            ),
            &context(),
        );
        assert_eq!(missing.status(), ExecutionStatus::Failed);
        assert_eq!(missing.effect(), EffectState::KnownNotApplied);
        assert!(missing.content().contains("not found"));
    }

    #[test]
    fn invalid_arguments_do_not_become_path_or_scope_access() {
        let port = port("output");
        for args in [
            r#"{"output_ref":"call-1","query":"x","offset":4}"#,
            r#"{"output_ref":"../call-1"}"#,
            r#"{"output_ref":""}"#,
            r#"{"output_ref":"call-1","path":"/etc/hosts"}"#,
        ] {
            let call = ToolCall::new(
                run(),
                nexus_core::TurnId::new("turn-6").unwrap(),
                nexus_core::CallId::new("read-5").unwrap(),
                ToolId::new(HOST_ARCHIVE_TOOL, M0_REVISION).unwrap(),
                nexus_core::NormalizedArgs::new(args).unwrap(),
            );
            let denied = port.execute(&call, &context());
            assert_eq!(denied.status(), ExecutionStatus::Denied, "{args}");
            assert_eq!(denied.effect(), EffectState::NotStarted);
        }
    }
}
