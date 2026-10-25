//! Session state and recovery boundary, currently providing local persistence.
//!
//! The file store preserves checkpoints, operation records, and original model
//! context items in a versioned, bounded JSON snapshot. Writes use a private
//! temporary sibling, file sync, atomic rename, and directory sync on Unix.
//! Runtime integration, active-view coordination, and recovery assembly remain
//! unimplemented. Model calls, tool dispatch, approvals, and cancellation belong
//! to runtime; loading session data must never trigger execution or grant access.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nexus_core::store::StoredMessage;
use nexus_core::{
    AgentError, ApprovedScope, CallId, EffectState, ErrorCategory, Evidence, ExecutionStatus,
    ModelContextItem, NormalizedArgs, RetryGuidance, RunId, SessionCheckpoint, SessionId,
    SessionMetadata, SessionStore, SourceRecord, ToolId, ToolIntentRecord, ToolOutcome,
    ToolOutcomeRecord,
};
use serde_json::{Value, json};

/// Current on-disk format revision. Mismatches fail closed; no guessing or migration.
pub const FILE_FORMAT_REVISION: u64 = 2;
/// Maximum encoded store size.
pub const MAX_STORE_BYTES: usize = nexus_validation::MAX_DOCUMENT_BYTES;
/// Maximum number of accepted source records retained in one store.
pub const MAX_SOURCE_RECORDS: usize = 16_384;

static WRITE_NONCE: AtomicU64 = AtomicU64::new(0);

type SnapshotRecords = (
    Vec<SessionCheckpoint>,
    Vec<ToolIntentRecord>,
    Vec<ToolOutcomeRecord>,
    Vec<SourceRecord>,
);

/// Persistent session store backed by `session-store.json` in a dedicated directory.
pub struct FileSessionStore {
    path: PathBuf,
    sessions: Vec<SessionCheckpoint>,
    intents: Vec<ToolIntentRecord>,
    outcomes: Vec<ToolOutcomeRecord>,
    source_records: Vec<SourceRecord>,
    write_uncertain: bool,
    _lock: File,
}

impl FileSessionStore {
    /// Opens a store directory, creating it privately if absent, and loads its
    /// versioned snapshot if present. The directory must be dedicated to this
    /// store; it is never the caller's general config or project directory.
    pub fn open(directory: impl Into<PathBuf>) -> Result<Self, AgentError> {
        let directory = directory.into();
        prepare_directory(&directory)?;
        let lock = acquire_lock(&directory)?;
        let path = directory.join("session-store.json");
        let (sessions, intents, outcomes, source_records) = match read_snapshot(&path)? {
            Some(bytes) => decode(&bytes)?,
            None => (Vec::new(), Vec::new(), Vec::new(), Vec::new()),
        };
        Ok(Self {
            path,
            sessions,
            intents,
            outcomes,
            source_records,
            write_uncertain: false,
            _lock: lock,
        })
    }

    fn commit(
        &mut self,
        sessions: Vec<SessionCheckpoint>,
        intents: Vec<ToolIntentRecord>,
        outcomes: Vec<ToolOutcomeRecord>,
        source_records: Vec<SourceRecord>,
    ) -> Result<(), AgentError> {
        self.commit_with_sync(sessions, intents, outcomes, source_records, sync_directory)
    }

    fn commit_with_sync(
        &mut self,
        sessions: Vec<SessionCheckpoint>,
        intents: Vec<ToolIntentRecord>,
        outcomes: Vec<ToolOutcomeRecord>,
        source_records: Vec<SourceRecord>,
        sync: impl FnOnce(&Path) -> Result<(), AgentError>,
    ) -> Result<(), AgentError> {
        self.ensure_available()?;
        let bytes = encode(&sessions, &intents, &outcomes, &source_records)?;
        atomic_write(&self.path, &bytes, &mut self.write_uncertain, sync)?;
        self.sessions = sessions;
        self.intents = intents;
        self.outcomes = outcomes;
        self.source_records = source_records;
        Ok(())
    }

    fn ensure_available(&self) -> Result<(), AgentError> {
        if self.write_uncertain {
            return Err(store_error(
                "session store publication is uncertain; reopen required",
            ));
        }
        Ok(())
    }
}

impl SessionStore for FileSessionStore {
    fn list_sessions(&self, limit: usize) -> Result<Vec<SessionMetadata>, AgentError> {
        self.ensure_available()?;
        let mut sessions = self
            .sessions
            .iter()
            .map(|checkpoint| SessionMetadata {
                session: checkpoint.session().clone(),
                logical_revision: checkpoint.logical_revision(),
            })
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| left.session.as_str().cmp(right.session.as_str()));
        sessions.truncate(limit);
        Ok(sessions)
    }

    fn load_session(&self, id: &SessionId) -> Result<SessionCheckpoint, AgentError> {
        self.ensure_available()?;
        self.sessions
            .iter()
            .find(|checkpoint| checkpoint.session() == id)
            .cloned()
            .ok_or_else(|| store_error("session was not found"))
    }

    fn save_checkpoint(&mut self, checkpoint: &SessionCheckpoint) -> Result<(), AgentError> {
        self.ensure_available()?;
        nexus_core::store::check_format_revision(checkpoint.format_revision())?;
        nexus_core::store::check_checkpoint_bounds(checkpoint)?;
        let mut sessions = self.sessions.clone();
        if let Some(current) = sessions
            .iter_mut()
            .find(|current| current.session() == checkpoint.session())
        {
            if checkpoint.logical_revision() < current.logical_revision()
                || (checkpoint.logical_revision() == current.logical_revision()
                    && checkpoint != current)
            {
                return Err(store_error("session revision is stale"));
            }
            if checkpoint.logical_revision() == current.logical_revision() {
                return Ok(());
            }
            *current = checkpoint.clone();
        } else {
            if sessions.len() >= nexus_core::store::MAX_SESSIONS {
                return Err(store_error("session store capacity is exhausted"));
            }
            sessions.push(checkpoint.clone());
        }
        self.commit(
            sessions,
            self.intents.clone(),
            self.outcomes.clone(),
            self.source_records.clone(),
        )
    }

    fn record_intent(&mut self, intent: &ToolIntentRecord) -> Result<(), AgentError> {
        self.ensure_available()?;
        if let Some(current) = self
            .intents
            .iter()
            .find(|current| current.run == intent.run && current.call == intent.call)
        {
            return if current == intent {
                Ok(())
            } else {
                Err(store_error(
                    "session store intent conflicts with existing record",
                ))
            };
        }
        if self.intents.len() >= nexus_core::store::MAX_INTENT_RECORDS {
            return Err(store_error("session store capacity is exhausted"));
        }
        let mut intents = self.intents.clone();
        intents.push(intent.clone());
        self.commit(
            self.sessions.clone(),
            intents,
            self.outcomes.clone(),
            self.source_records.clone(),
        )
    }

    fn record_outcome(&mut self, outcome: &ToolOutcomeRecord) -> Result<(), AgentError> {
        self.ensure_available()?;
        check_outcome(outcome.status, outcome.effect)?;
        if let Some(current) = self
            .outcomes
            .iter()
            .find(|current| current.run == outcome.run && current.call == outcome.call)
        {
            return if current == outcome {
                Ok(())
            } else {
                Err(store_error(
                    "session store outcome conflicts with existing record",
                ))
            };
        }
        if self.outcomes.len() >= nexus_core::store::MAX_OUTCOME_RECORDS {
            return Err(store_error("session store capacity is exhausted"));
        }
        let mut outcomes = self.outcomes.clone();
        outcomes.push(outcome.clone());
        self.commit(
            self.sessions.clone(),
            self.intents.clone(),
            outcomes,
            self.source_records.clone(),
        )
    }

    fn append_source_record(&mut self, record: &SourceRecord) -> Result<(), AgentError> {
        self.ensure_available()?;
        if let Some(existing) = self.source_records.iter().find(|existing| {
            existing.session() == record.session() && existing.sequence() == record.sequence()
        }) {
            return if existing == record {
                Ok(())
            } else {
                Err(store_error(
                    "source record conflicts with existing sequence",
                ))
            };
        }
        let last_sequence = self
            .source_records
            .iter()
            .filter(|existing| existing.session() == record.session())
            .map(SourceRecord::sequence)
            .max()
            .unwrap_or(0);
        if record.sequence() != last_sequence.saturating_add(1) {
            return Err(store_error("source record sequence is not contiguous"));
        }
        if self.source_records.len() >= MAX_SOURCE_RECORDS {
            return Err(store_error("source history capacity is exhausted"));
        }
        let mut records = self.source_records.clone();
        records.push(record.clone());
        self.commit(
            self.sessions.clone(),
            self.intents.clone(),
            self.outcomes.clone(),
            records,
        )
    }

    fn load_source_records(
        &self,
        session: &SessionId,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<SourceRecord>, AgentError> {
        self.ensure_available()?;
        Ok(self
            .source_records
            .iter()
            .filter(|record| record.session() == session && record.sequence() > after_sequence)
            .take(limit)
            .cloned()
            .collect())
    }
}

fn encode(
    sessions: &[SessionCheckpoint],
    intents: &[ToolIntentRecord],
    outcomes: &[ToolOutcomeRecord],
    source_records: &[SourceRecord],
) -> Result<Vec<u8>, AgentError> {
    let value = json!({
        "revision": FILE_FORMAT_REVISION,
        "sessions": sessions.iter().map(checkpoint_value).collect::<Vec<_>>(),
        "intents": intents.iter().map(intent_value).collect::<Vec<_>>(),
        "outcomes": outcomes.iter().map(outcome_value).collect::<Vec<_>>(),
        "source_records": source_records.iter().map(source_record_value).collect::<Vec<_>>(),
    });
    let bytes = serde_json::to_vec(&value)
        .map_err(|_| store_error("session store could not be encoded"))?;
    if bytes.len() > MAX_STORE_BYTES {
        return Err(store_error("session store size limit is exceeded"));
    }
    Ok(bytes)
}

fn decode(bytes: &[u8]) -> Result<SnapshotRecords, AgentError> {
    if bytes.len() > MAX_STORE_BYTES {
        return Err(store_error("session store size limit is exceeded"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| store_error("session store is invalid"))?;
    let root = nexus_validation::parse_document_object(text, MAX_STORE_BYTES)
        .map_err(|_| store_error("session store is invalid"))?;
    check_keys(
        &root,
        &[
            "revision",
            "sessions",
            "intents",
            "outcomes",
            "source_records",
        ],
    )?;
    let revision = number(field(&root, "revision")?)?;
    if revision != FILE_FORMAT_REVISION {
        return Err(store_error("session store format revision is unsupported"));
    }
    let sessions = bounded_array(field(&root, "sessions")?, nexus_core::store::MAX_SESSIONS)?
        .iter()
        .map(parse_checkpoint)
        .collect::<Result<Vec<_>, _>>()?;
    let intents = bounded_array(
        field(&root, "intents")?,
        nexus_core::store::MAX_INTENT_RECORDS,
    )?
    .iter()
    .map(parse_intent)
    .collect::<Result<Vec<_>, _>>()?;
    let outcomes = bounded_array(
        field(&root, "outcomes")?,
        nexus_core::store::MAX_OUTCOME_RECORDS,
    )?
    .iter()
    .map(parse_outcome)
    .collect::<Result<Vec<_>, _>>()?;
    let source_records = bounded_array(field(&root, "source_records")?, MAX_SOURCE_RECORDS)?
        .iter()
        .map(parse_source_record)
        .collect::<Result<Vec<_>, _>>()?;
    if sessions.len() > nexus_core::store::MAX_SESSIONS
        || intents.len() > nexus_core::store::MAX_INTENT_RECORDS
        || outcomes.len() > nexus_core::store::MAX_OUTCOME_RECORDS
        || source_records.len() > MAX_SOURCE_RECORDS
    {
        return Err(store_error("session store record limit is exceeded"));
    }
    let mut session_ids = HashSet::with_capacity(sessions.len());
    if sessions
        .iter()
        .any(|session| !session_ids.insert(session.session().as_str()))
    {
        return Err(store_error("session store contains duplicate sessions"));
    }
    let mut intent_ids = HashSet::with_capacity(intents.len());
    if intents
        .iter()
        .any(|intent| !intent_ids.insert((intent.run.as_str(), intent.call.as_str())))
    {
        return Err(store_error("session store contains duplicate intents"));
    }
    let mut outcome_ids = HashSet::with_capacity(outcomes.len());
    if outcomes
        .iter()
        .any(|outcome| !outcome_ids.insert((outcome.run.as_str(), outcome.call.as_str())))
    {
        return Err(store_error("session store contains duplicate outcomes"));
    }
    let mut last_sequence = HashSet::with_capacity(source_records.len());
    for record in &source_records {
        let key = record.session().as_str();
        if !last_sequence.insert((key, record.sequence())) {
            return Err(store_error(
                "session store contains duplicate source sequence",
            ));
        }
    }
    let mut sequence_by_session = std::collections::HashMap::new();
    for record in &source_records {
        let expected = sequence_by_session
            .get(record.session().as_str())
            .copied()
            .unwrap_or(0u64)
            .saturating_add(1);
        if record.sequence() != expected {
            return Err(store_error("session store source sequence is invalid"));
        }
        sequence_by_session.insert(record.session().as_str(), expected);
    }
    Ok((sessions, intents, outcomes, source_records))
}

fn checkpoint_value(checkpoint: &SessionCheckpoint) -> Value {
    json!({
        "session": checkpoint.session().as_str(),
        "revision": checkpoint.format_revision(),
        "logical_revision": checkpoint.logical_revision(),
        "profile": checkpoint.profile(),
        "messages": checkpoint.messages().iter().map(|message| json!({
            "source": message.source,
            "text": message.text,
            "complete": message.complete,
        })).collect::<Vec<_>>(),
    })
}

fn intent_value(intent: &ToolIntentRecord) -> Value {
    json!({
        "run": intent.run.as_str(),
        "call": intent.call.as_str(),
        "tool": intent.tool.name(),
        "tool_revision": intent.tool.revision(),
        "args": intent.args.as_str(),
        "scope": intent.scope.as_str(),
    })
}

fn outcome_value(outcome: &ToolOutcomeRecord) -> Value {
    json!({
        "run": outcome.run.as_str(),
        "call": outcome.call.as_str(),
        "status": status_name(outcome.status),
        "effect": effect_name(outcome.effect),
        "evidence": evidence_name(outcome.evidence),
        "truncated": outcome.truncated,
    })
}

fn source_record_value(record: &SourceRecord) -> Value {
    json!({
        "session": record.session().as_str(),
        "sequence": record.sequence(),
        "run": record.run().as_str(),
        "item": context_item_value(record.item()),
    })
}

fn context_item_value(item: &ModelContextItem) -> Value {
    match item {
        ModelContextItem::UserText(text) => json!({ "kind": "user_text", "text": text.as_str() }),
        ModelContextItem::AssistantText {
            item_key,
            text,
            reasoning,
        } => json!({
            "kind": "assistant_text",
            "item_key": item_key.as_str(),
            "text": text.as_str(),
            "reasoning": reasoning,
        }),
        ModelContextItem::AssistantCall {
            item_key,
            provider_ref,
            call,
            reasoning,
        } => json!({
            "kind": "assistant_call",
            "item_key": item_key.as_str(),
            "provider_ref": provider_ref.as_str(),
            "run": call.run().as_str(),
            "turn": call.turn().as_str(),
            "call": call.call().as_str(),
            "tool": call.tool().name(),
            "tool_revision": call.tool().revision(),
            "args": call.args().as_str(),
            "reasoning": reasoning,
        }),
        ModelContextItem::AssistantDeniedCall {
            call,
            candidate,
            reasoning,
        } => json!({
            "kind": "assistant_denied_call",
            "call": call.as_str(),
            "item_key": candidate.item_key(),
            "provider_ref": candidate.provider_ref(),
            "tool_name": candidate.tool_name(),
            "arguments_json": candidate.arguments_json(),
            "reasoning": reasoning,
        }),
        ModelContextItem::AssistantReasoning { reasoning } => json!({
            "kind": "assistant_reasoning",
            "reasoning": reasoning,
        }),
        ModelContextItem::ToolResult {
            call,
            item_key,
            provider_ref,
            tool,
            outcome,
        } => json!({
            "kind": "tool_result",
            "call": call.as_str(),
            "item_key": item_key.as_str(),
            "provider_ref": provider_ref.as_str(),
            "tool": tool.name(),
            "tool_revision": tool.revision(),
            "status": status_name(outcome.status()),
            "effect": effect_name(outcome.effect()),
            "evidence": evidence_name(outcome.evidence()),
            "content": outcome.content(),
            "truncated": outcome.is_truncated(),
        }),
    }
}

fn parse_source_record(value: &Value) -> Result<SourceRecord, AgentError> {
    check_keys(value, &["session", "sequence", "run", "item"])?;
    SourceRecord::new(
        SessionId::new(string(field(value, "session")?)?)
            .map_err(|_| store_error("session store identity is invalid"))?,
        number(field(value, "sequence")?)?,
        RunId::new(string(field(value, "run")?)?)
            .map_err(|_| store_error("session store identity is invalid"))?,
        parse_context_item(field(value, "item")?)?,
    )
}

fn parse_context_item(value: &Value) -> Result<ModelContextItem, AgentError> {
    let kind = string(field(value, "kind")?)?;
    match kind.as_str() {
        "user_text" => {
            check_keys(value, &["kind", "text"])?;
            ModelContextItem::user_text(string(field(value, "text")?)?)
        }
        "assistant_text" => {
            check_keys(value, &["kind", "item_key", "text", "reasoning"])?;
            ModelContextItem::assistant_text_with_reasoning(
                string(field(value, "item_key")?)?,
                string(field(value, "text")?)?,
                optional_string(field(value, "reasoning")?)?,
            )
        }
        "assistant_call" => {
            check_keys(
                value,
                &[
                    "kind",
                    "item_key",
                    "provider_ref",
                    "run",
                    "turn",
                    "call",
                    "tool",
                    "tool_revision",
                    "args",
                    "reasoning",
                ],
            )?;
            let tool = ToolId::new(
                string(field(value, "tool")?)?,
                u32::try_from(number(field(value, "tool_revision")?)?)
                    .map_err(|_| store_error("session store tool identity is invalid"))?,
            )
            .map_err(|_| store_error("session store tool identity is invalid"))?;
            let admitted = nexus_core::ToolCall::new(
                RunId::new(string(field(value, "run")?)?)
                    .map_err(|_| store_error("session store identity is invalid"))?,
                nexus_core::TurnId::new(string(field(value, "turn")?)?)
                    .map_err(|_| store_error("session store identity is invalid"))?,
                CallId::new(string(field(value, "call")?)?)
                    .map_err(|_| store_error("session store identity is invalid"))?,
                tool,
                NormalizedArgs::new(string(field(value, "args")?)?)?,
            );
            ModelContextItem::assistant_call_with_reasoning(
                string(field(value, "item_key")?)?,
                string(field(value, "provider_ref")?)?,
                admitted,
                optional_string(field(value, "reasoning")?)?,
            )
        }
        "assistant_denied_call" => {
            check_keys(
                value,
                &[
                    "kind",
                    "call",
                    "item_key",
                    "provider_ref",
                    "tool_name",
                    "arguments_json",
                    "reasoning",
                ],
            )?;
            let candidate = nexus_core::CallCandidate::new(
                string(field(value, "item_key")?)?,
                string(field(value, "provider_ref")?)?,
                string(field(value, "tool_name")?)?,
                string(field(value, "arguments_json")?)?,
            )?;
            ModelContextItem::assistant_denied_call(
                CallId::new(string(field(value, "call")?)?)
                    .map_err(|_| store_error("session store identity is invalid"))?,
                candidate,
                optional_string(field(value, "reasoning")?)?,
            )
        }
        "assistant_reasoning" => {
            check_keys(value, &["kind", "reasoning"])?;
            ModelContextItem::assistant_reasoning(string(field(value, "reasoning")?)?)
        }
        "tool_result" => {
            check_keys(
                value,
                &[
                    "kind",
                    "call",
                    "item_key",
                    "provider_ref",
                    "tool",
                    "tool_revision",
                    "status",
                    "effect",
                    "evidence",
                    "content",
                    "truncated",
                ],
            )?;
            let status = parse_status(string(field(value, "status")?)?)?;
            let effect = parse_effect(string(field(value, "effect")?)?)?;
            check_outcome(status, effect)?;
            let outcome = ToolOutcome::new(
                status,
                effect,
                parse_evidence(string(field(value, "evidence")?)?)?,
                string(field(value, "content")?)?,
                boolean(field(value, "truncated")?)?,
            )?;
            ModelContextItem::tool_result(
                CallId::new(string(field(value, "call")?)?)
                    .map_err(|_| store_error("session store identity is invalid"))?,
                string(field(value, "item_key")?)?,
                string(field(value, "provider_ref")?)?,
                ToolId::new(
                    string(field(value, "tool")?)?,
                    u32::try_from(number(field(value, "tool_revision")?)?)
                        .map_err(|_| store_error("session store tool identity is invalid"))?,
                )
                .map_err(|_| store_error("session store tool identity is invalid"))?,
                outcome,
            )
        }
        _ => Err(store_error("session store context item kind is invalid")),
    }
}

fn optional_string(value: &Value) -> Result<Option<String>, AgentError> {
    match value {
        Value::Null => Ok(None),
        Value::String(value) => Ok(Some(value.clone())),
        _ => Err(store_error("session store is invalid")),
    }
}

fn parse_checkpoint(value: &Value) -> Result<SessionCheckpoint, AgentError> {
    check_keys(
        value,
        &[
            "session",
            "revision",
            "logical_revision",
            "profile",
            "messages",
        ],
    )?;
    let session = SessionId::new(string(field(value, "session")?)?)
        .map_err(|_| store_error("session store identity is invalid"))?;
    let revision = u32::try_from(number(field(value, "revision")?)?)
        .map_err(|_| store_error("session store revision is invalid"))?;
    let logical_revision = number(field(value, "logical_revision")?)?;
    let profile = string(field(value, "profile")?)?;
    let messages = bounded_array(
        field(value, "messages")?,
        nexus_core::store::MAX_STORED_MESSAGES,
    )?
    .iter()
    .map(|message| {
        check_keys(message, &["source", "text", "complete"])?;
        Ok(StoredMessage {
            source: string(field(message, "source")?)?,
            text: string(field(message, "text")?)?,
            complete: boolean(field(message, "complete")?)?,
        })
    })
    .collect::<Result<Vec<_>, AgentError>>()?;
    SessionCheckpoint::new(session, revision, logical_revision, messages, profile)
}

fn parse_intent(value: &Value) -> Result<ToolIntentRecord, AgentError> {
    check_keys(
        value,
        &["run", "call", "tool", "tool_revision", "args", "scope"],
    )?;
    let run = RunId::new(string(field(value, "run")?)?)
        .map_err(|_| store_error("session store identity is invalid"))?;
    let call = CallId::new(string(field(value, "call")?)?)
        .map_err(|_| store_error("session store identity is invalid"))?;
    let tool_revision = u32::try_from(number(field(value, "tool_revision")?)?)
        .map_err(|_| store_error("session store tool identity is invalid"))?;
    let tool = ToolId::new(string(field(value, "tool")?)?, tool_revision)
        .map_err(|_| store_error("session store tool identity is invalid"))?;
    let args = NormalizedArgs::new(string(field(value, "args")?)?)?;
    let scope = ApprovedScope::new(string(field(value, "scope")?)?)?;
    Ok(ToolIntentRecord {
        run,
        call,
        tool,
        args,
        scope,
    })
}

fn parse_outcome(value: &Value) -> Result<ToolOutcomeRecord, AgentError> {
    check_keys(
        value,
        &["run", "call", "status", "effect", "evidence", "truncated"],
    )?;
    let status = parse_status(string(field(value, "status")?)?)?;
    let effect = parse_effect(string(field(value, "effect")?)?)?;
    check_outcome(status, effect)?;
    Ok(ToolOutcomeRecord {
        run: RunId::new(string(field(value, "run")?)?)
            .map_err(|_| store_error("session store identity is invalid"))?,
        call: CallId::new(string(field(value, "call")?)?)
            .map_err(|_| store_error("session store identity is invalid"))?,
        status,
        effect,
        evidence: parse_evidence(string(field(value, "evidence")?)?)?,
        truncated: boolean(field(value, "truncated")?)?,
    })
}

fn check_outcome(status: ExecutionStatus, effect: EffectState) -> Result<(), AgentError> {
    if status == ExecutionStatus::Denied && effect != EffectState::NotStarted {
        return Err(store_error("session store outcome is inconsistent"));
    }
    Ok(())
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a Value, AgentError> {
    value
        .as_object()
        .and_then(|map| map.get(key))
        .ok_or_else(|| store_error("session store is invalid"))
}

fn check_keys(value: &Value, expected: &[&str]) -> Result<(), AgentError> {
    let object = value
        .as_object()
        .ok_or_else(|| store_error("session store is invalid"))?;
    if object.len() != expected.len() || object.keys().any(|key| !expected.contains(&key.as_str()))
    {
        return Err(store_error("session store fields are invalid"));
    }
    Ok(())
}

fn string(value: &Value) -> Result<String, AgentError> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| store_error("session store is invalid"))
}

fn number(value: &Value) -> Result<u64, AgentError> {
    value
        .as_u64()
        .ok_or_else(|| store_error("session store is invalid"))
}

fn boolean(value: &Value) -> Result<bool, AgentError> {
    value
        .as_bool()
        .ok_or_else(|| store_error("session store is invalid"))
}

fn bounded_array(value: &Value, limit: usize) -> Result<&Vec<Value>, AgentError> {
    let items = value
        .as_array()
        .ok_or_else(|| store_error("session store is invalid"))?;
    if items.len() > limit {
        return Err(store_error("session store record limit is exceeded"));
    }
    Ok(items)
}

fn status_name(status: ExecutionStatus) -> &'static str {
    match status {
        ExecutionStatus::Succeeded => "succeeded",
        ExecutionStatus::Failed => "failed",
        ExecutionStatus::Denied => "denied",
        ExecutionStatus::Cancelled => "cancelled",
        ExecutionStatus::TimedOut => "timed_out",
    }
}

fn parse_status(value: String) -> Result<ExecutionStatus, AgentError> {
    match value.as_str() {
        "succeeded" => Ok(ExecutionStatus::Succeeded),
        "failed" => Ok(ExecutionStatus::Failed),
        "denied" => Ok(ExecutionStatus::Denied),
        "cancelled" => Ok(ExecutionStatus::Cancelled),
        "timed_out" => Ok(ExecutionStatus::TimedOut),
        _ => Err(store_error("session store outcome is invalid")),
    }
}

fn effect_name(effect: EffectState) -> &'static str {
    match effect {
        EffectState::NotStarted => "not_started",
        EffectState::KnownNotApplied => "known_not_applied",
        EffectState::KnownApplied => "known_applied",
        EffectState::Unknown => "unknown",
    }
}

fn parse_effect(value: String) -> Result<EffectState, AgentError> {
    match value.as_str() {
        "not_started" => Ok(EffectState::NotStarted),
        "known_not_applied" => Ok(EffectState::KnownNotApplied),
        "known_applied" => Ok(EffectState::KnownApplied),
        "unknown" => Ok(EffectState::Unknown),
        _ => Err(store_error("session store outcome is invalid")),
    }
}

fn evidence_name(evidence: Evidence) -> &'static str {
    match evidence {
        Evidence::HostObserved => "host_observed",
        Evidence::PluginReported => "plugin_reported",
        Evidence::Uncertain => "uncertain",
    }
}

fn parse_evidence(value: String) -> Result<Evidence, AgentError> {
    match value.as_str() {
        "host_observed" => Ok(Evidence::HostObserved),
        "plugin_reported" => Ok(Evidence::PluginReported),
        "uncertain" => Ok(Evidence::Uncertain),
        _ => Err(store_error("session store outcome is invalid")),
    }
}

fn atomic_write(
    path: &Path,
    bytes: &[u8],
    write_uncertain: &mut bool,
    sync: impl FnOnce(&Path) -> Result<(), AgentError>,
) -> Result<(), AgentError> {
    let parent = path
        .parent()
        .ok_or_else(|| store_error("session store path is invalid"))?;
    let nonce = WRITE_NONCE.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temporary = parent.join(format!(
        ".session-store-{}-{nanos}-{nonce}.tmp",
        std::process::id()
    ));
    let mut file = create_private(&temporary)
        .map_err(|_| store_error("session store could not be written"))?;
    if file.write_all(bytes).is_err() || file.sync_all().is_err() {
        drop(file);
        let _ = fs::remove_file(&temporary);
        return Err(store_error("session store could not be written"));
    }
    drop(file);
    if fs::rename(&temporary, path).is_err() {
        let _ = fs::remove_file(&temporary);
        return Err(store_error("session store could not be written"));
    }
    // Publication has occurred. Do not use the old in-memory snapshot if
    // confirming durability fails; reopening must reconcile it with disk.
    *write_uncertain = true;
    sync(parent)?;
    *write_uncertain = false;
    Ok(())
}

fn read_snapshot(path: &Path) -> Result<Option<Vec<u8>>, AgentError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(store_error("session store could not be read")),
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_STORE_BYTES as u64 {
        return Err(store_error("session store file is invalid"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(store_error("session store file is not private"));
        }
    }
    fs::read(path)
        .map(Some)
        .map_err(|_| store_error("session store could not be read"))
}

#[cfg(unix)]
fn acquire_lock(directory: &Path) -> Result<File, AgentError> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let path = directory.join("session-store.lock");
    if let Ok(metadata) = fs::symlink_metadata(&path)
        && (!metadata.file_type().is_file() || metadata.permissions().mode() & 0o077 != 0)
    {
        return Err(store_error("session store lock file is not private"));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|_| store_error("session store lock could not be opened"))?;
    file.try_lock()
        .map_err(|_| store_error("session store already has a writer"))?;
    Ok(file)
}

#[cfg(not(unix))]
fn acquire_lock(_: &Path) -> Result<File, AgentError> {
    Err(store_error(
        "single-writer session storage is unsupported on this platform",
    ))
}

#[cfg(unix)]
fn create_private(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(unix)]
fn prepare_directory(path: &Path) -> Result<(), AgentError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    match builder.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(store_error("session store directory could not be created")),
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| store_error("session store directory is invalid"))?;
    if !metadata.file_type().is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(store_error("session store directory is not private"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn prepare_directory(path: &Path) -> Result<(), AgentError> {
    let _ = path;
    Err(store_error(
        "private session storage is unsupported on this platform",
    ))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), AgentError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| store_error("session store durability could not be confirmed"))
}

#[cfg(not(unix))]
fn sync_directory(_: &Path) -> Result<(), AgentError> {
    Ok(())
}

fn store_error(message: &'static str) -> AgentError {
    AgentError::new(
        ErrorCategory::StorageFailure,
        message,
        RetryGuidance::DoNotRetry,
    )
    .expect("static session-store message builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::{M0_REVISION, NormalizedArgs, ToolCall, TurnId};

    fn directory(label: &str) -> PathBuf {
        let unique = WRITE_NONCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "nexus-session-{label}-{}-{unique}",
            std::process::id()
        ))
    }

    fn checkpoint(revision: u64, text: &str) -> SessionCheckpoint {
        SessionCheckpoint::new(
            SessionId::new("session-a").expect("valid session"),
            nexus_core::STORE_FORMAT_REVISION,
            revision,
            vec![StoredMessage {
                source: "user".to_owned(),
                text: text.to_owned(),
                complete: true,
            }],
            "profile-a",
        )
        .expect("valid checkpoint")
    }

    #[test]
    fn duplicate_operations_are_idempotent_at_capacity_and_conflicts_preserve_snapshot() {
        let directory = directory("duplicates");
        let mut store = FileSessionStore::open(&directory).unwrap();
        let intent = ToolIntentRecord {
            run: RunId::new("run-a").unwrap(),
            call: CallId::new("call-a").unwrap(),
            tool: ToolId::new("host_write", M0_REVISION).unwrap(),
            args: NormalizedArgs::new("{}").unwrap(),
            scope: ApprovedScope::new("project-write").unwrap(),
        };
        let outcome = ToolOutcomeRecord {
            run: intent.run.clone(),
            call: intent.call.clone(),
            status: ExecutionStatus::Succeeded,
            effect: EffectState::Unknown,
            evidence: Evidence::Uncertain,
            truncated: false,
        };
        let intents = (0..nexus_core::store::MAX_INTENT_RECORDS)
            .map(|index| ToolIntentRecord {
                call: CallId::new(format!("call-{index}")).unwrap(),
                ..intent.clone()
            })
            .collect::<Vec<_>>();
        let outcomes = (0..nexus_core::store::MAX_OUTCOME_RECORDS)
            .map(|index| ToolOutcomeRecord {
                call: CallId::new(format!("call-{index}")).unwrap(),
                ..outcome.clone()
            })
            .collect::<Vec<_>>();
        let intent = intents[0].clone();
        let outcome = outcomes[0].clone();
        store
            .commit(Vec::new(), intents, outcomes, Vec::new())
            .unwrap();
        let original = fs::read(&store.path).unwrap();
        assert!(store.record_intent(&intent).is_ok());
        assert!(store.record_outcome(&outcome).is_ok());
        let conflicting_intent = ToolIntentRecord {
            args: NormalizedArgs::new(r#"{"changed":true}"#).unwrap(),
            ..intent
        };
        let conflicting_outcome = ToolOutcomeRecord {
            effect: EffectState::KnownApplied,
            ..outcome
        };
        assert!(store.record_intent(&conflicting_intent).is_err());
        assert!(store.record_outcome(&conflicting_outcome).is_err());
        assert_eq!(fs::read(&store.path).unwrap(), original);
        drop(store);
        let reopened = FileSessionStore::open(&directory).unwrap();
        assert_eq!(
            reopened.intents.len(),
            nexus_core::store::MAX_INTENT_RECORDS
        );
        assert_eq!(
            reopened.outcomes.len(),
            nexus_core::store::MAX_OUTCOME_RECORDS
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn inconsistent_outcomes_are_rejected_without_changing_snapshot() {
        let directory = directory("invalid-outcome");
        let mut store = FileSessionStore::open(&directory).unwrap();
        store.save_checkpoint(&checkpoint(1, "seed")).unwrap();
        let original = fs::read(&store.path).unwrap();
        for effect in [
            EffectState::KnownApplied,
            EffectState::KnownNotApplied,
            EffectState::Unknown,
        ] {
            let outcome = ToolOutcomeRecord {
                run: RunId::new("run-a").unwrap(),
                call: CallId::new("call-a").unwrap(),
                status: ExecutionStatus::Denied,
                effect,
                evidence: Evidence::HostObserved,
                truncated: false,
            };
            assert!(store.record_outcome(&outcome).is_err());
            assert!(parse_outcome(&outcome_value(&outcome)).is_err());
            assert_eq!(fs::read(&store.path).unwrap(), original);
        }
        assert!(store.outcomes.is_empty());
        let outcome = ToolOutcomeRecord {
            run: RunId::new("run-a").unwrap(),
            call: CallId::new("call-a").unwrap(),
            status: ExecutionStatus::Denied,
            effect: EffectState::NotStarted,
            evidence: Evidence::HostObserved,
            truncated: false,
        };
        store.record_outcome(&outcome).unwrap();
        drop(store);
        let reopened = FileSessionStore::open(&directory).unwrap();
        assert_eq!(reopened.outcomes, vec![outcome]);
        assert_eq!(reopened.sessions, vec![checkpoint(1, "seed")]);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn directory_sync_failure_requires_reopen_before_access() {
        let directory = directory("sync-failure");
        let mut store = FileSessionStore::open(&directory).unwrap();
        store.save_checkpoint(&checkpoint(1, "old")).unwrap();
        let published = checkpoint(2, "published");
        assert!(
            store
                .commit_with_sync(
                    vec![published.clone()],
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    |_| Err(store_error("injected directory sync failure")),
                )
                .is_err()
        );
        let bytes = fs::read(&store.path).unwrap();
        assert!(store.list_sessions(1).is_err());
        assert!(store.load_session(published.session()).is_err());
        assert!(store.save_checkpoint(&checkpoint(1, "old")).is_err());
        assert!(store.save_checkpoint(&checkpoint(3, "next")).is_err());
        let outcome = ToolOutcomeRecord {
            run: RunId::new("run-a").unwrap(),
            call: CallId::new("call-a").unwrap(),
            status: ExecutionStatus::Succeeded,
            effect: EffectState::Unknown,
            evidence: Evidence::Uncertain,
            truncated: false,
        };
        let intent = ToolIntentRecord {
            run: outcome.run.clone(),
            call: outcome.call.clone(),
            tool: ToolId::new("host_write", M0_REVISION).unwrap(),
            args: NormalizedArgs::new("{}").unwrap(),
            scope: ApprovedScope::new("project-write").unwrap(),
        };
        assert!(store.record_intent(&intent).is_err());
        assert!(store.record_outcome(&outcome).is_err());
        assert_eq!(fs::read(&store.path).unwrap(), bytes);
        drop(store);
        let mut reopened = FileSessionStore::open(&directory).unwrap();
        assert_eq!(
            reopened.load_session(published.session()).unwrap(),
            published
        );
        reopened.save_checkpoint(&checkpoint(3, "next")).unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn checkpoint_survives_reopen_and_has_private_file_permissions() {
        let directory = directory("reopen");
        let mut store = FileSessionStore::open(&directory).expect("store opens");
        store
            .save_checkpoint(&checkpoint(1, "accepted input"))
            .expect("save succeeds");
        drop(store);

        let store = FileSessionStore::open(&directory).expect("stored snapshot reopens");
        let loaded = store
            .load_session(&SessionId::new("session-a").expect("valid session"))
            .expect("checkpoint restores");
        assert_eq!(loaded.messages()[0].text, "accepted input");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(directory.join("session-store.json"))
                .expect("store file exists")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        fs::remove_dir_all(directory).expect("test directory cleanup");
    }

    #[test]
    fn larger_than_tool_argument_documents_round_trip_within_record_limits() {
        let directory = directory("large-record");
        let mut store = FileSessionStore::open(&directory).expect("store opens");
        let large_text = "x".repeat(70_000);
        store
            .save_checkpoint(&checkpoint(1, &large_text))
            .expect("bounded session record persists");
        drop(store);
        let reopened = FileSessionStore::open(&directory).expect("large snapshot reopens");
        let loaded = reopened
            .load_session(&SessionId::new("session-a").expect("valid session"))
            .expect("checkpoint restores");
        assert_eq!(loaded.messages()[0].text.len(), 70_000);
        fs::remove_dir_all(directory).expect("test directory cleanup");
    }

    #[test]
    fn failed_stale_write_does_not_replace_checkpoint() {
        let directory = directory("stale");
        let mut store = FileSessionStore::open(&directory).expect("store opens");
        store
            .save_checkpoint(&checkpoint(2, "newer"))
            .expect("save succeeds");
        assert!(store.save_checkpoint(&checkpoint(1, "older")).is_err());
        assert_eq!(
            store
                .load_session(&SessionId::new("session-a").expect("valid"))
                .unwrap()
                .messages()[0]
                .text,
            "newer"
        );
        fs::remove_dir_all(directory).expect("test directory cleanup");
    }

    #[test]
    fn invalid_and_unsupported_snapshots_fail_closed() {
        let directory = directory("invalid");
        let mut store =
            FileSessionStore::open(&directory).expect("private store directory created");
        store
            .save_checkpoint(&checkpoint(1, "seed"))
            .expect("initial snapshot written");
        drop(store);
        let path = directory.join("session-store.json");
        fs::write(&path, br#"{"revision":1,"revision":1}"#).expect("invalid snapshot written");
        assert!(FileSessionStore::open(&directory).is_err());
        fs::write(
            &path,
            br#"{"revision":99,"sessions":[],"intents":[],"outcomes":[]}"#,
        )
        .expect("unsupported snapshot written");
        assert!(FileSessionStore::open(&directory).is_err());
        fs::remove_dir_all(directory).expect("test directory cleanup");
    }

    #[test]
    fn operation_records_round_trip_without_rewriting_uncertainty() {
        let directory = directory("operation");
        let mut store = FileSessionStore::open(&directory).expect("store opens");
        store
            .record_intent(&ToolIntentRecord {
                run: RunId::new("run-a").expect("valid run"),
                call: CallId::new("call-a").expect("valid call"),
                tool: ToolId::new("host_write", M0_REVISION).expect("valid tool"),
                args: NormalizedArgs::new(r#"{"path":"file"}"#).expect("valid args"),
                scope: ApprovedScope::new("project-write").expect("valid scope"),
            })
            .expect("intent persists");
        store
            .record_outcome(&ToolOutcomeRecord {
                run: RunId::new("run-a").expect("valid run"),
                call: CallId::new("call-a").expect("valid call"),
                status: ExecutionStatus::Succeeded,
                effect: EffectState::Unknown,
                evidence: Evidence::Uncertain,
                truncated: false,
            })
            .expect("outcome persists");
        drop(store);

        let reopened = FileSessionStore::open(&directory).expect("operation store reopens");
        assert_eq!(reopened.intents.len(), 1);
        assert_eq!(reopened.outcomes[0].effect, EffectState::Unknown);
        assert_eq!(reopened.outcomes[0].evidence, Evidence::Uncertain);
        fs::remove_dir_all(directory).expect("test directory cleanup");
    }

    #[test]
    fn source_items_round_trip_in_order_and_store_rejects_second_writer() {
        let directory = directory("source-history");
        let mut store = FileSessionStore::open(&directory).expect("store opens");
        assert!(FileSessionStore::open(&directory).is_err());

        let session = SessionId::new("session-a").unwrap();
        let run = RunId::new("run-a").unwrap();
        let call_id = CallId::new("call-a").unwrap();
        let tool_id = ToolId::new("host_read", M0_REVISION).unwrap();
        let admitted_call = ToolCall::new(
            run.clone(),
            TurnId::new("turn-a").unwrap(),
            call_id.clone(),
            tool_id.clone(),
            NormalizedArgs::new(r#"{"path":"src/lib.rs"}"#).unwrap(),
        );
        let items = vec![
            ModelContextItem::user_text("question").unwrap(),
            ModelContextItem::assistant_text_with_reasoning(
                "item-text",
                "thinking answer",
                Some("reasoning trace".to_owned()),
            )
            .unwrap(),
            ModelContextItem::assistant_call_with_reasoning(
                "item-call",
                "provider-call-a",
                admitted_call,
                Some("call reasoning".to_owned()),
            )
            .unwrap(),
            ModelContextItem::assistant_denied_call(
                CallId::new("call-denied").unwrap(),
                nexus_core::CallCandidate::new(
                    "item-denied",
                    "provider-call-denied",
                    "unknown_tool",
                    "{bad json retained verbatim",
                )
                .unwrap(),
                Some("denial reasoning".to_owned()),
            )
            .unwrap(),
            ModelContextItem::assistant_reasoning("reasoning only".to_owned()).unwrap(),
            ModelContextItem::tool_result(
                call_id,
                "item-result",
                "provider-call-a",
                tool_id,
                ToolOutcome::new(
                    ExecutionStatus::Succeeded,
                    EffectState::Unknown,
                    Evidence::Uncertain,
                    "result observed, effect uncertain",
                    false,
                )
                .unwrap(),
            )
            .unwrap(),
        ];
        for (index, item) in items.into_iter().enumerate() {
            let record =
                SourceRecord::new(session.clone(), index as u64 + 1, run.clone(), item).unwrap();
            store.append_source_record(&record).unwrap();
            store.append_source_record(&record).unwrap();
        }
        let page = store.load_source_records(&session, 2, 2).unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].sequence(), 3);
        assert_eq!(page[1].sequence(), 4);
        drop(store);

        let reopened = FileSessionStore::open(&directory).unwrap();
        let restored = reopened.load_source_records(&session, 0, 20).unwrap();
        assert_eq!(restored.len(), 6);
        assert_eq!(
            restored[0].item(),
            &ModelContextItem::user_text("question").unwrap()
        );
        assert_eq!(
            restored[5].item(),
            &ModelContextItem::tool_result(
                CallId::new("call-a").unwrap(),
                "item-result",
                "provider-call-a",
                ToolId::new("host_read", M0_REVISION).unwrap(),
                ToolOutcome::new(
                    ExecutionStatus::Succeeded,
                    EffectState::Unknown,
                    Evidence::Uncertain,
                    "result observed, effect uncertain",
                    false,
                )
                .unwrap(),
            )
            .unwrap()
        );
        drop(reopened);
        fs::remove_dir_all(directory).unwrap();
    }
}
