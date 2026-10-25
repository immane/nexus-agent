//! Session state and recovery boundary, currently providing local persistence.
//!
//! The file store preserves checkpoints, operation records, and original model
//! context items in a versioned, bounded JSON snapshot. Writes use a private
//! temporary sibling, file sync, atomic rename, and directory sync on Unix.
//! Runtime integration is opt-in through an injected store. The bounded recovery
//! API validates selected history without interpreting it as executable work.
//! Model calls, tool dispatch, approvals, and cancellation belong to runtime;
//! loading session data must never trigger execution or grant access.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use nexus_core::store::StoredMessage;
use nexus_core::{
    AgentError, ApprovedScope, CallId, EffectState, ErrorCategory, Evidence, ExecutionStatus,
    ModelContextItem, NormalizedArgs, RetryGuidance, RunId, SessionCheckpoint, SessionId,
    SessionMetadata, SessionStore, SourceRecord, ToolId, ToolIntentRecord, ToolOutcome,
    ToolOutcomeRecord,
};
use serde_json::{Value, json};

/// Current on-disk format revision. Revision 2 remains readable; all writes use
/// this revision, and other mismatches fail closed without guessing.
pub const FILE_FORMAT_REVISION: u64 = 3;
const PREVIOUS_FILE_FORMAT_REVISION: u64 = 2;
/// Maximum encoded store size.
pub const MAX_STORE_BYTES: usize = nexus_validation::MAX_DOCUMENT_BYTES;
/// Maximum number of accepted source records retained in one store.
pub const MAX_SOURCE_RECORDS: usize = 16_384;
const SOURCE_PAGE_SIZE: usize = 256;

/// Returns the user-local application data directory for automatic session
/// storage, when a supported Unix home/state location is available.
#[must_use]
pub fn default_data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
            .map(|home| home.join("Library/Application Support/nexus-agent/sessions"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let base = std::env::var_os("XDG_STATE_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|value| !value.is_empty())
                    .map(|home| PathBuf::from(home).join(".local/state"))
            })?;
        base.is_absolute()
            .then(|| base.join("nexus-agent/sessions"))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

static WRITE_NONCE: AtomicU64 = AtomicU64::new(0);

type SnapshotRecords = (
    Vec<SessionCheckpoint>,
    Vec<ToolIntentRecord>,
    Vec<ToolOutcomeRecord>,
    Vec<SourceRecord>,
);

/// Validated data loaded for an explicit session restore. This is history and
/// metadata only; callers must apply current policy and start a fresh run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredSession {
    checkpoint: SessionCheckpoint,
    source_records: Vec<SourceRecord>,
}

impl RecoveredSession {
    /// Returns the saved checkpoint metadata and active-view messages.
    #[must_use]
    pub fn checkpoint(&self) -> &SessionCheckpoint {
        &self.checkpoint
    }

    /// Returns original records in their validated session sequence.
    #[must_use]
    pub fn source_records(&self) -> &[SourceRecord] {
        &self.source_records
    }

    /// Returns original model-context items without interpreting them as
    /// executable instructions or restoring authorization.
    pub fn items(&self) -> impl Iterator<Item = &ModelContextItem> {
        self.source_records.iter().map(SourceRecord::item)
    }
}

/// Loads one explicitly selected session using bounded pages and validates
/// its source sequence. It never scans other sessions or executes stored data.
pub fn recover_session(
    store: &dyn SessionStore,
    session: &SessionId,
) -> Result<RecoveredSession, AgentError> {
    let checkpoint = store.load_session(session)?;
    if checkpoint.session() != session {
        return Err(store_error(
            "loaded session identity does not match request",
        ));
    }

    let mut source_records = Vec::new();
    let mut after_sequence = 0;
    loop {
        let page = store.load_source_records(session, after_sequence, SOURCE_PAGE_SIZE)?;
        if page.is_empty() {
            break;
        }
        if source_records.len().saturating_add(page.len()) > MAX_SOURCE_RECORDS {
            return Err(store_error("recovered source history exceeds its bound"));
        }
        for record in &page {
            let expected = after_sequence
                .checked_add(1)
                .ok_or_else(|| store_error("source record sequence is exhausted"))?;
            if record.session() != session || record.sequence() != expected {
                return Err(store_error("source history sequence is invalid"));
            }
            after_sequence = record.sequence();
        }
        let page_is_full = page.len() == SOURCE_PAGE_SIZE;
        source_records.extend(page);
        if !page_is_full {
            break;
        }
    }

    Ok(RecoveredSession {
        checkpoint,
        source_records,
    })
}

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

/// Directory-backed store that keeps each conversation in its own bounded
/// snapshot. Opening it reads only a small metadata index; session bodies are
/// loaded only for the selected conversation or one being updated.
pub struct DirectorySessionStore {
    sessions_directory: PathBuf,
    index_path: PathBuf,
    sessions: Vec<SessionMetadata>,
    write_uncertain: bool,
    _lock: File,
}

impl DirectorySessionStore {
    /// Opens a private store whose index contains metadata only. Session files
    /// are decoded only when that session is loaded or changed.
    pub fn open(directory: impl Into<PathBuf>) -> Result<Self, AgentError> {
        let directory = directory.into();
        prepare_directory(&directory)?;
        let lock = acquire_lock(&directory)?;
        let sessions_directory = directory.join("sessions");
        prepare_directory(&sessions_directory)?;
        let index_path = directory.join("session-index.json");
        let sessions = read_session_index(&index_path)?;
        let mut store = Self {
            sessions_directory,
            index_path,
            sessions,
            write_uncertain: false,
            _lock: lock,
        };
        store.recover_transaction()?;
        // No journal now references staging. Discard a pre-intent interrupted
        // update so explicit deletion cannot leave another copy of its body.
        store.discard_staging()?;
        Ok(store)
    }

    fn ensure_available(&self) -> Result<(), AgentError> {
        if self.write_uncertain {
            return Err(store_error(
                "session store publication is uncertain; reopen required",
            ));
        }
        Ok(())
    }

    fn session_path(&self, session: &SessionId) -> PathBuf {
        // Encode bytes to avoid case-folding collisions on macOS filesystems.
        let name: String = session
            .as_str()
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        self.sessions_directory.join(format!("{name}.json"))
    }

    fn staging_path(&self) -> PathBuf {
        self.sessions_directory.join("pending-body.json")
    }

    fn journal_path(&self) -> PathBuf {
        self.index_path.with_file_name("session-pending.json")
    }

    fn discard_staging(&self) -> Result<(), AgentError> {
        match fs::remove_file(self.staging_path()) {
            Ok(()) => sync_directory(&self.sessions_directory),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(store_error("unpublished staging data could not be removed")),
        }
    }

    fn publish(&mut self, session: &SessionId, revision: Option<u64>) -> Result<(), AgentError> {
        self.ensure_available()?;
        if revision.is_none() {
            self.discard_staging()?;
        }
        let bytes = serde_json::to_vec(&json!({
            "session": session.as_str(), "revision": revision,
        }))
        .map_err(|_| store_error("session transaction could not be encoded"))?;
        atomic_write(
            &self.journal_path(),
            &bytes,
            &mut self.write_uncertain,
            sync_directory,
        )?;
        // Once the journal is durable, failures require reopening and replaying
        // this metadata-only transaction before any further store access.
        let result = self.recover_transaction();
        if result.is_err() {
            self.write_uncertain = true;
        }
        result
    }

    fn recover_transaction(&mut self) -> Result<(), AgentError> {
        let Some(bytes) = read_snapshot(&self.journal_path())? else {
            return Ok(());
        };
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| store_error("session transaction is invalid"))?;
        let root = nexus_validation::parse_document_object(text, 1024)
            .map_err(|_| store_error("session transaction is invalid"))?;
        check_keys(&root, &["session", "revision"])?;
        let session = SessionId::new(string(field(&root, "session")?)?)
            .map_err(|_| store_error("session transaction identity is invalid"))?;
        let value = field(&root, "revision")?;
        let revision = if value.is_null() {
            None
        } else {
            Some(number(value)?)
        };
        if revision.is_some() {
            if fs::symlink_metadata(self.staging_path()).is_ok() {
                fs::rename(self.staging_path(), self.session_path(&session))
                    .map_err(|_| store_error("session body could not be published"))?;
            } else if read_snapshot(&self.session_path(&session))?.is_none() {
                return Err(store_error("pending session body is missing"));
            }
            sync_directory(&self.sessions_directory)?;
        }
        let mut entries = self.sessions.clone();
        entries.retain(|entry| entry.session != session);
        if let Some(logical_revision) = revision {
            entries.push(SessionMetadata {
                session: session.clone(),
                logical_revision,
            });
        }
        entries.sort_by(|left, right| left.session.as_str().cmp(right.session.as_str()));
        self.write_index(entries)?;
        if revision.is_none() {
            match fs::remove_file(self.session_path(&session)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(store_error("session data could not be removed")),
            }
            sync_directory(&self.sessions_directory)?;
        }
        fs::remove_file(self.journal_path())
            .map_err(|_| store_error("session transaction could not be completed"))?;
        sync_directory(self.index_path.parent().expect("store directory"))
    }

    fn update<T>(
        &mut self,
        session: &SessionId,
        change: impl FnOnce(&mut FileSessionStore) -> Result<T, AgentError>,
    ) -> Result<T, AgentError> {
        let mut store = self.load_file_store(session)?;
        store.path = self.staging_path();
        match fs::remove_file(&store.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(store_error("unpublished staging data could not be removed")),
        }
        let result = change(&mut store);
        self.write_uncertain |= store.write_uncertain;
        let value = result?;
        // Even an idempotent operation must stage a complete snapshot; an old
        // staging file must never be mistaken for the current transaction.
        if !store.path.exists() {
            let staged = store.commit(
                store.sessions.clone(),
                store.intents.clone(),
                store.outcomes.clone(),
                store.source_records.clone(),
            );
            self.write_uncertain |= store.write_uncertain;
            staged?;
        }
        self.save_loaded(store)?;
        Ok(value)
    }

    fn load_file_store(&self, session: &SessionId) -> Result<FileSessionStore, AgentError> {
        self.ensure_available()?;
        if !self.sessions.iter().any(|entry| &entry.session == session) {
            return Err(store_error("session was not found"));
        }
        let path = self.session_path(session);
        let (sessions, intents, outcomes, source_records) = match read_snapshot(&path)? {
            Some(bytes) => decode(&bytes)?,
            None => return Err(store_error("session data is missing")),
        };
        if sessions.len() != 1 || sessions[0].session() != session {
            return Err(store_error("session data identity is invalid"));
        }
        Ok(FileSessionStore {
            path,
            sessions,
            intents,
            outcomes,
            source_records,
            write_uncertain: false,
            _lock: self
                ._lock
                .try_clone()
                .map_err(|_| store_error("session store is unavailable"))?,
        })
    }

    fn write_index(&mut self, entries: Vec<SessionMetadata>) -> Result<(), AgentError> {
        self.ensure_available()?;
        let value = json!({
            "revision": 1,
            "sessions": entries.iter().map(|entry| json!({
                "session": entry.session.as_str(),
                "logical_revision": entry.logical_revision,
            })).collect::<Vec<_>>(),
        });
        let bytes = serde_json::to_vec(&value)
            .map_err(|_| store_error("session index could not be encoded"))?;
        if bytes.len() > 16 * 1024 {
            return Err(store_error("session index size limit is exceeded"));
        }
        atomic_write(
            &self.index_path,
            &bytes,
            &mut self.write_uncertain,
            sync_directory,
        )?;
        self.sessions = entries;
        Ok(())
    }

    fn save_loaded(&mut self, store: FileSessionStore) -> Result<(), AgentError> {
        self.write_uncertain |= store.write_uncertain;
        store.ensure_available()?;
        let checkpoint = store
            .sessions
            .first()
            .ok_or_else(|| store_error("session data is missing"))?;
        let session = checkpoint.session().clone();
        let revision = checkpoint.logical_revision();
        self.publish(&session, Some(revision))
    }
}

fn read_session_index(path: &Path) -> Result<Vec<SessionMetadata>, AgentError> {
    let Some(bytes) = read_snapshot(path)? else {
        return Ok(Vec::new());
    };
    if bytes.len() > 16 * 1024 {
        return Err(store_error("session index size limit is exceeded"));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| store_error("session index is invalid"))?;
    let root = nexus_validation::parse_document_object(text, 16 * 1024)
        .map_err(|_| store_error("session index is invalid"))?;
    check_keys(&root, &["revision", "sessions"])?;
    if number(field(&root, "revision")?)? != 1 {
        return Err(store_error("session index format revision is unsupported"));
    }
    let values = bounded_array(field(&root, "sessions")?, nexus_core::store::MAX_SESSIONS)?;
    let entries = values
        .iter()
        .map(|value| {
            check_keys(value, &["session", "logical_revision"])?;
            let session = SessionId::new(string(field(value, "session")?)?.to_owned())
                .map_err(|_| store_error("session index identity is invalid"))?;
            let logical_revision = number(field(value, "logical_revision")?)?;
            Ok(SessionMetadata {
                session,
                logical_revision,
            })
        })
        .collect::<Result<Vec<_>, AgentError>>()?;
    let mut seen = HashSet::new();
    if entries
        .iter()
        .any(|entry| !seen.insert(entry.session.as_str()))
    {
        return Err(store_error("session index contains duplicate sessions"));
    }
    Ok(entries)
}

/// Defers reading the bounded snapshot until the first store operation.
/// Applications can construct a runtime and open a fresh conversation without
/// materializing prior session history during startup.
pub struct LazyFileSessionStore {
    directory: PathBuf,
    store: Mutex<Option<FileSessionStore>>,
}

impl LazyFileSessionStore {
    /// Creates a store handle without opening or scanning its directory.
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            store: Mutex::new(None),
        }
    }

    fn lock_store(&self) -> Result<MutexGuard<'_, Option<FileSessionStore>>, AgentError> {
        let mut guard = self
            .store
            .lock()
            .map_err(|_| store_error("session store is unavailable"))?;
        if guard.is_none() {
            *guard = Some(FileSessionStore::open(self.directory.clone())?);
        }
        Ok(guard)
    }
}

impl SessionStore for LazyFileSessionStore {
    fn list_sessions(&self, limit: usize) -> Result<Vec<SessionMetadata>, AgentError> {
        self.lock_store()?.as_ref().unwrap().list_sessions(limit)
    }

    fn load_session(&self, id: &SessionId) -> Result<SessionCheckpoint, AgentError> {
        self.lock_store()?.as_ref().unwrap().load_session(id)
    }

    fn save_checkpoint(&mut self, checkpoint: &SessionCheckpoint) -> Result<(), AgentError> {
        self.lock_store()?
            .as_mut()
            .unwrap()
            .save_checkpoint(checkpoint)
    }

    fn initialize_session(&mut self, checkpoint: &SessionCheckpoint) -> Result<bool, AgentError> {
        self.lock_store()?
            .as_mut()
            .unwrap()
            .initialize_session(checkpoint)
    }

    fn record_intent(&mut self, intent: &ToolIntentRecord) -> Result<(), AgentError> {
        self.lock_store()?.as_mut().unwrap().record_intent(intent)
    }

    fn record_outcome(&mut self, outcome: &ToolOutcomeRecord) -> Result<(), AgentError> {
        self.lock_store()?.as_mut().unwrap().record_outcome(outcome)
    }

    fn append_source_record(&mut self, record: &SourceRecord) -> Result<(), AgentError> {
        self.lock_store()?
            .as_mut()
            .unwrap()
            .append_source_record(record)
    }

    fn load_source_records(
        &self,
        session: &SessionId,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<SourceRecord>, AgentError> {
        self.lock_store()?
            .as_ref()
            .unwrap()
            .load_source_records(session, after_sequence, limit)
    }

    fn last_source_sequence(&self, session: &SessionId) -> Result<u64, AgentError> {
        self.lock_store()?
            .as_ref()
            .unwrap()
            .last_source_sequence(session)
    }

    fn append_source_items(
        &mut self,
        session: &SessionId,
        run: &RunId,
        items: &[ModelContextItem],
    ) -> Result<u64, AgentError> {
        self.lock_store()?
            .as_mut()
            .unwrap()
            .append_source_items(session, run, items)
    }
}

impl SessionStore for DirectorySessionStore {
    fn import_legacy_sessions(&mut self) -> Result<usize, AgentError> {
        self.ensure_available()?;
        let legacy = self.index_path.with_file_name("session-store.json");
        let Some(bytes) = read_snapshot(&legacy)? else {
            return Err(store_error("legacy session store was not found"));
        };
        let (checkpoints, intents, outcomes, records) = decode(&bytes)?;
        // Tool records predate session ownership. Infer it only from original
        // source records, never by guessing which conversation owns a run.
        let mut owners = std::collections::HashMap::new();
        for record in &records {
            if !checkpoints
                .iter()
                .any(|checkpoint| checkpoint.session() == record.session())
            {
                return Err(store_error("legacy source has no owning session"));
            }
            if let Some(owner) = owners.insert(record.run().clone(), record.session().clone())
                && owner != *record.session()
            {
                return Err(store_error("legacy run belongs to multiple sessions"));
            }
        }
        if intents
            .iter()
            .any(|record| !owners.contains_key(&record.run))
            || outcomes
                .iter()
                .any(|record| !owners.contains_key(&record.run))
        {
            return Err(store_error(
                "legacy tool ownership is unknown; original file preserved",
            ));
        }
        let missing = checkpoints
            .iter()
            .filter(|checkpoint| {
                !self
                    .sessions
                    .iter()
                    .any(|entry| &entry.session == checkpoint.session())
            })
            .count();
        if self.sessions.len() + missing > nexus_core::store::MAX_SESSIONS {
            return Err(store_error(
                "import exceeds session capacity; delete old saved sessions first",
            ));
        }
        let mut snapshots = Vec::new();
        for checkpoint in checkpoints {
            let session = checkpoint.session();
            let selected_intents = intents
                .iter()
                .filter(|record| owners.get(&record.run) == Some(session))
                .cloned()
                .collect::<Vec<_>>();
            let selected_outcomes = outcomes
                .iter()
                .filter(|record| owners.get(&record.run) == Some(session))
                .cloned()
                .collect::<Vec<_>>();
            let selected_records = records
                .iter()
                .filter(|record| record.session() == session)
                .cloned()
                .collect::<Vec<_>>();
            let bytes = encode(
                std::slice::from_ref(&checkpoint),
                &selected_intents,
                &selected_outcomes,
                &selected_records,
            )?;
            decode(&bytes)?;
            if self.sessions.iter().any(|entry| &entry.session == session) {
                let existing = self.load_file_store(session)?;
                if encode(
                    &existing.sessions,
                    &existing.intents,
                    &existing.outcomes,
                    &existing.source_records,
                )? != bytes
                {
                    return Err(store_error(
                        "legacy session conflicts with existing data; import refused",
                    ));
                }
                continue;
            }
            if fs::symlink_metadata(self.session_path(session)).is_ok() {
                return Err(store_error(
                    "orphaned session data exists; refusing to overwrite it",
                ));
            }
            snapshots.push((session.clone(), checkpoint.logical_revision(), bytes));
        }
        let count = snapshots.len();
        for (session, revision, bytes) in snapshots {
            atomic_write(
                &self.staging_path(),
                &bytes,
                &mut self.write_uncertain,
                sync_directory,
            )?;
            self.publish(&session, Some(revision))?;
        }
        // The legacy file remains an untouched backup. Repeating an interrupted
        // import skips identical sessions and refuses conflicts.
        Ok(count)
    }

    fn list_sessions(&self, limit: usize) -> Result<Vec<SessionMetadata>, AgentError> {
        self.ensure_available()?;
        let mut entries = self.sessions.clone();
        entries.sort_by(|left, right| left.session.as_str().cmp(right.session.as_str()));
        entries.truncate(limit);
        Ok(entries)
    }

    fn load_session(&self, id: &SessionId) -> Result<SessionCheckpoint, AgentError> {
        self.load_file_store(id)?
            .sessions
            .into_iter()
            .next()
            .ok_or_else(|| store_error("session was not found"))
    }

    fn save_checkpoint(&mut self, checkpoint: &SessionCheckpoint) -> Result<(), AgentError> {
        nexus_core::store::check_format_revision(checkpoint.format_revision())?;
        nexus_core::store::check_checkpoint_bounds(checkpoint)?;
        if !self
            .sessions
            .iter()
            .any(|entry| entry.session == *checkpoint.session())
        {
            return Err(store_error("session was not initialized"));
        }
        self.update(checkpoint.session(), |store| {
            store.save_checkpoint(checkpoint)
        })
    }

    fn initialize_session(&mut self, checkpoint: &SessionCheckpoint) -> Result<bool, AgentError> {
        self.ensure_available()?;
        nexus_core::store::check_format_revision(checkpoint.format_revision())?;
        nexus_core::store::check_checkpoint_bounds(checkpoint)?;
        if self
            .sessions
            .iter()
            .any(|entry| entry.session == *checkpoint.session())
        {
            return Ok(false);
        }
        if self.sessions.len() >= nexus_core::store::MAX_SESSIONS {
            return Err(store_error(
                "session capacity reached; delete an old session with :session delete <id>",
            ));
        }
        let path = self.session_path(checkpoint.session());
        if fs::symlink_metadata(&path).is_ok() {
            return Err(store_error(
                "orphaned session data exists; refusing to overwrite it",
            ));
        }
        let mut store = FileSessionStore {
            path: self.staging_path(),
            sessions: Vec::new(),
            intents: Vec::new(),
            outcomes: Vec::new(),
            source_records: Vec::new(),
            write_uncertain: false,
            _lock: self
                ._lock
                .try_clone()
                .map_err(|_| store_error("session store is unavailable"))?,
        };
        let result = store.save_checkpoint(checkpoint);
        self.write_uncertain |= store.write_uncertain;
        result?;
        self.save_loaded(store)?;
        Ok(true)
    }

    fn record_intent(&mut self, _intent: &ToolIntentRecord) -> Result<(), AgentError> {
        Err(store_error(
            "session identity is required for operation records",
        ))
    }

    fn record_session_intent(
        &mut self,
        session: &SessionId,
        intent: &ToolIntentRecord,
    ) -> Result<(), AgentError> {
        self.update(session, |store| store.record_intent(intent))
    }

    fn record_outcome(&mut self, _outcome: &ToolOutcomeRecord) -> Result<(), AgentError> {
        Err(store_error(
            "session identity is required for operation records",
        ))
    }

    fn record_session_outcome(
        &mut self,
        session: &SessionId,
        outcome: &ToolOutcomeRecord,
    ) -> Result<(), AgentError> {
        self.update(session, |store| store.record_outcome(outcome))
    }

    fn delete_session(&mut self, session: &SessionId) -> Result<(), AgentError> {
        self.ensure_available()?;
        if !self.sessions.iter().any(|entry| &entry.session == session) {
            return Err(store_error("session was not found"));
        }
        self.publish(session, None)
    }

    fn append_source_record(&mut self, record: &SourceRecord) -> Result<(), AgentError> {
        self.update(record.session(), |store| store.append_source_record(record))
    }

    fn load_source_records(
        &self,
        session: &SessionId,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<SourceRecord>, AgentError> {
        self.load_file_store(session)?
            .load_source_records(session, after_sequence, limit)
    }

    fn last_source_sequence(&self, session: &SessionId) -> Result<u64, AgentError> {
        self.load_file_store(session)?.last_source_sequence(session)
    }

    fn append_source_items(
        &mut self,
        session: &SessionId,
        run: &RunId,
        items: &[ModelContextItem],
    ) -> Result<u64, AgentError> {
        self.update(session, |store| {
            store.append_source_items(session, run, items)
        })
    }
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
        // Every published snapshot must satisfy the same bounds as reopening it.
        decode(&bytes)?;
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
                return Err(store_error(
                    "session operation history is full; restart in a new session, then delete an old saved session with :session delete <id>",
                ));
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

    fn initialize_session(&mut self, checkpoint: &SessionCheckpoint) -> Result<bool, AgentError> {
        self.ensure_available()?;
        nexus_core::store::check_format_revision(checkpoint.format_revision())?;
        nexus_core::store::check_checkpoint_bounds(checkpoint)?;
        if self
            .sessions
            .iter()
            .any(|current| current.session() == checkpoint.session())
        {
            return Ok(false);
        }
        self.save_checkpoint(checkpoint)?;
        Ok(true)
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
            return Err(store_error(
                "session operation history is full; restart in a new session, then delete an old saved session with :session delete <id>",
            ));
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
            return Err(store_error(
                "session operation history is full; restart in a new session, then delete an old saved session with :session delete <id>",
            ));
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
        if !self
            .sessions
            .iter()
            .any(|checkpoint| checkpoint.session() == record.session())
        {
            return Err(store_error("source history session is not initialized"));
        }
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
            return Err(store_error(
                "session source history is full; restart in a new session, then delete an old saved session with :session delete <id>",
            ));
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

    fn last_source_sequence(&self, session: &SessionId) -> Result<u64, AgentError> {
        self.ensure_available()?;
        Ok(self
            .source_records
            .iter()
            .filter(|record| record.session() == session)
            .map(SourceRecord::sequence)
            .max()
            .unwrap_or(0))
    }

    fn append_source_items(
        &mut self,
        session: &SessionId,
        run: &RunId,
        items: &[ModelContextItem],
    ) -> Result<u64, AgentError> {
        self.ensure_available()?;
        if !self
            .sessions
            .iter()
            .any(|checkpoint| checkpoint.session() == session)
        {
            return Err(store_error("source history session is not initialized"));
        }
        let mut sequence = self.last_source_sequence(session)?;
        if self.source_records.len().saturating_add(items.len()) > MAX_SOURCE_RECORDS {
            return Err(store_error(
                "session source history is full; restart in a new session, then delete an old saved session with :session delete <id>",
            ));
        }
        let mut records = self.source_records.clone();
        for item in items {
            sequence = sequence
                .checked_add(1)
                .ok_or_else(|| store_error("source record sequence is exhausted"))?;
            records.push(SourceRecord::new(
                session.clone(),
                sequence,
                run.clone(),
                item.clone(),
            )?);
        }
        if !items.is_empty() {
            self.commit(
                self.sessions.clone(),
                self.intents.clone(),
                self.outcomes.clone(),
                records,
            )?;
        }
        Ok(sequence)
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
    if revision != FILE_FORMAT_REVISION && revision != PREVIOUS_FILE_FORMAT_REVISION {
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
        ExecutionStatus::Interrupted => "interrupted",
    }
}

fn parse_status(value: String) -> Result<ExecutionStatus, AgentError> {
    match value.as_str() {
        "succeeded" => Ok(ExecutionStatus::Succeeded),
        "failed" => Ok(ExecutionStatus::Failed),
        "denied" => Ok(ExecutionStatus::Denied),
        "cancelled" => Ok(ExecutionStatus::Cancelled),
        "timed_out" => Ok(ExecutionStatus::TimedOut),
        "interrupted" => Ok(ExecutionStatus::Interrupted),
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

    fn checkpoint_for(id: &str, revision: u64, text: &str) -> SessionCheckpoint {
        SessionCheckpoint::new(
            SessionId::new(id).expect("valid session"),
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
    fn directory_transactions_recover_create_update_and_delete_without_loading_bodies() {
        for boundary in 0..3 {
            let directory = directory("transaction-recovery");
            let mut store = DirectorySessionStore::open(&directory).unwrap();
            let checkpoint = checkpoint_for("pending", 1, "retained");
            let bytes = encode(std::slice::from_ref(&checkpoint), &[], &[], &[]).unwrap();
            atomic_write(&store.staging_path(), &bytes, &mut false, sync_directory).unwrap();
            let journal = json!({"session": "pending", "revision": 1});
            atomic_write(
                &store.journal_path(),
                &serde_json::to_vec(&journal).unwrap(),
                &mut false,
                sync_directory,
            )
            .unwrap();
            if boundary >= 1 {
                fs::rename(
                    store.staging_path(),
                    store.session_path(checkpoint.session()),
                )
                .unwrap();
                sync_directory(&store.sessions_directory).unwrap();
            }
            if boundary >= 2 {
                store
                    .write_index(vec![SessionMetadata {
                        session: checkpoint.session().clone(),
                        logical_revision: 1,
                    }])
                    .unwrap();
            }
            drop(store);
            let store = DirectorySessionStore::open(&directory).unwrap();
            assert_eq!(
                store.load_session(checkpoint.session()).unwrap(),
                checkpoint
            );
            assert!(!store.journal_path().exists());

            // Update recovery reads only pending metadata, not even the body's
            // JSON. A damaged body remains selected-session corruption.
            let journal = json!({"session": "pending", "revision": 2});
            atomic_write(
                &store.staging_path(),
                b"damaged",
                &mut false,
                sync_directory,
            )
            .unwrap();
            atomic_write(
                &store.journal_path(),
                &serde_json::to_vec(&journal).unwrap(),
                &mut false,
                sync_directory,
            )
            .unwrap();
            drop(store);
            let store = DirectorySessionStore::open(&directory).unwrap();
            assert_eq!(store.list_sessions(1).unwrap()[0].logical_revision, 2);
            assert!(store.load_session(checkpoint.session()).is_err());

            let journal = json!({"session": "pending", "revision": null});
            atomic_write(
                &store.journal_path(),
                &serde_json::to_vec(&journal).unwrap(),
                &mut false,
                sync_directory,
            )
            .unwrap();
            drop(store);
            let store = DirectorySessionStore::open(&directory).unwrap();
            assert!(store.list_sessions(1).unwrap().is_empty());
            assert!(!store.session_path(checkpoint.session()).exists());
            drop(store);
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn directory_child_sync_failure_blocks_outer_store_until_reopened() {
        let directory = directory("child-sync-failure");
        let mut store = DirectorySessionStore::open(&directory).unwrap();
        let checkpoint = checkpoint(1, "original");
        store.initialize_session(&checkpoint).unwrap();
        let result = store.update(checkpoint.session(), |child| {
            child.commit_with_sync(child.sessions.clone(), vec![], vec![], vec![], |_| {
                Err(store_error("injected directory sync failure"))
            })
        });
        assert!(result.is_err());
        assert!(store.list_sessions(1).is_err());
        assert!(store.load_session(checkpoint.session()).is_err());
        drop(store);
        let reopened = DirectorySessionStore::open(&directory).unwrap();
        assert_eq!(
            reopened.load_session(checkpoint.session()).unwrap(),
            checkpoint
        );
        drop(reopened);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn directory_failed_index_publication_preserves_memory_and_replays_on_reopen() {
        let directory = directory("index-failure");
        let mut store = DirectorySessionStore::open(&directory).unwrap();
        let original = checkpoint(1, "original");
        store.initialize_session(&original).unwrap();
        let index_bytes = fs::read(&store.index_path).unwrap();
        fs::remove_file(&store.index_path).unwrap();
        fs::create_dir(&store.index_path).unwrap();
        assert!(store.save_checkpoint(&checkpoint(2, "updated")).is_err());
        assert_eq!(store.sessions[0].logical_revision, 1);
        assert!(store.list_sessions(1).is_err());
        assert!(store.journal_path().exists());
        fs::remove_dir(&store.index_path).unwrap();
        atomic_write(&store.index_path, &index_bytes, &mut false, sync_directory).unwrap();
        drop(store);
        let reopened = DirectorySessionStore::open(&directory).unwrap();
        assert_eq!(
            reopened.load_session(original.session()).unwrap(),
            checkpoint(2, "updated")
        );
        assert_eq!(reopened.list_sessions(1).unwrap()[0].logical_revision, 2);
        drop(reopened);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn directory_capacity_can_be_released_and_case_distinct_ids_do_not_collide() {
        let directory = directory("index-capacity");
        let mut store = DirectorySessionStore::open(&directory).unwrap();
        for index in 0..nexus_core::store::MAX_SESSIONS {
            store
                .initialize_session(&checkpoint_for(&format!("session-{index}"), 1, "saved"))
                .unwrap();
        }
        assert!(
            store
                .initialize_session(&checkpoint_for("extra", 1, "saved"))
                .is_err()
        );
        store
            .delete_session(&SessionId::new("session-0").unwrap())
            .unwrap();
        store
            .initialize_session(&checkpoint_for("Session-1", 1, "distinct"))
            .unwrap();
        assert_eq!(
            store
                .load_session(&SessionId::new("session-1").unwrap())
                .unwrap()
                .messages()[0]
                .text,
            "saved"
        );
        assert_eq!(
            store
                .load_session(&SessionId::new("Session-1").unwrap())
                .unwrap()
                .messages()[0]
                .text,
            "distinct"
        );
        drop(store);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_import_is_explicit_idempotent_and_preserves_backup_and_ownership() {
        let directory = directory("legacy-import");
        let mut legacy = FileSessionStore::open(&directory).unwrap();
        let checkpoint = checkpoint(1, "legacy text");
        legacy.save_checkpoint(&checkpoint).unwrap();
        let run = RunId::new("legacy-run").unwrap();
        legacy
            .append_source_items(
                checkpoint.session(),
                &run,
                &[ModelContextItem::user_text("legacy input").unwrap()],
            )
            .unwrap();
        let intent = ToolIntentRecord {
            run,
            call: CallId::new("call").unwrap(),
            tool: ToolId::new("read", M0_REVISION).unwrap(),
            args: NormalizedArgs::new("{}").unwrap(),
            scope: ApprovedScope::new("read").unwrap(),
        };
        legacy.record_intent(&intent).unwrap();
        drop(legacy);
        let backup = fs::read(directory.join("session-store.json")).unwrap();
        let mut store = DirectorySessionStore::open(&directory).unwrap();
        assert!(store.list_sessions(10).unwrap().is_empty());
        assert_eq!(store.import_legacy_sessions().unwrap(), 1);
        assert_eq!(store.import_legacy_sessions().unwrap(), 0);
        assert_eq!(
            store.load_session(checkpoint.session()).unwrap(),
            checkpoint
        );
        assert_eq!(
            store.load_file_store(checkpoint.session()).unwrap().intents,
            vec![intent]
        );
        assert_eq!(
            fs::read(directory.join("session-store.json")).unwrap(),
            backup
        );
        store
            .save_checkpoint(&checkpoint_for("session-a", 2, "changed"))
            .unwrap();
        assert!(store.import_legacy_sessions().is_err());
        drop(store);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_unknown_tool_ownership_fails_before_importing_any_session() {
        let directory = directory("legacy-unknown-owner");
        let mut legacy = FileSessionStore::open(&directory).unwrap();
        legacy.save_checkpoint(&checkpoint(1, "legacy")).unwrap();
        legacy
            .record_intent(&ToolIntentRecord {
                run: RunId::new("unknown-run").unwrap(),
                call: CallId::new("call").unwrap(),
                tool: ToolId::new("read", M0_REVISION).unwrap(),
                args: NormalizedArgs::new("{}").unwrap(),
                scope: ApprovedScope::new("read").unwrap(),
            })
            .unwrap();
        drop(legacy);
        let backup = fs::read(directory.join("session-store.json")).unwrap();
        let mut store = DirectorySessionStore::open(&directory).unwrap();
        assert!(store.import_legacy_sessions().is_err());
        assert!(store.list_sessions(64).unwrap().is_empty());
        assert_eq!(
            fs::read(directory.join("session-store.json")).unwrap(),
            backup
        );
        drop(store);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_delete_keeps_a_recoverable_journal_and_blocks_further_writes() {
        let directory = directory("delete-failure");
        let mut store = DirectorySessionStore::open(&directory).unwrap();
        let checkpoint = checkpoint(1, "original");
        store.initialize_session(&checkpoint).unwrap();
        let path = store.session_path(checkpoint.session());
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(store.delete_session(checkpoint.session()).is_err());
        assert!(store.journal_path().exists());
        assert!(
            store
                .initialize_session(&checkpoint_for("new", 1, "blocked"))
                .is_err()
        );
        fs::remove_dir(path).unwrap();
        drop(store);
        let reopened = DirectorySessionStore::open(&directory).unwrap();
        assert!(reopened.list_sessions(64).unwrap().is_empty());
        assert!(!reopened.journal_path().exists());
        drop(reopened);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reopen_discards_pre_intent_staging_before_explicit_delete() {
        let directory = directory("abandoned-stage");
        let mut store = DirectorySessionStore::open(&directory).unwrap();
        let checkpoint = checkpoint(1, "private history");
        store.initialize_session(&checkpoint).unwrap();
        let bytes = fs::read(store.session_path(checkpoint.session())).unwrap();
        atomic_write(&store.staging_path(), &bytes, &mut false, sync_directory).unwrap();
        assert!(!store.journal_path().exists());
        drop(store);
        let mut store = DirectorySessionStore::open(&directory).unwrap();
        assert!(!store.staging_path().exists());
        store.delete_session(checkpoint.session()).unwrap();
        assert_eq!(fs::read_dir(&store.sessions_directory).unwrap().count(), 0);
        drop(store);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn directory_store_lists_metadata_without_decoding_other_sessions_and_deletes_explicitly() {
        let directory = directory("partitioned");
        let mut store = DirectorySessionStore::open(&directory).unwrap();
        let first = checkpoint_for("session-a", 1, "original");
        let second = checkpoint_for("session-b", 1, "new session");
        assert!(store.initialize_session(&first).unwrap());

        // A malformed body in an unselected session must not block metadata
        // listing or creation of another independent conversation.
        fs::write(store.session_path(first.session()), b"not json").unwrap();
        assert_eq!(store.list_sessions(10).unwrap().len(), 1);
        assert!(store.initialize_session(&second).unwrap());
        assert_eq!(store.load_session(second.session()).unwrap(), second);
        assert!(store.load_session(first.session()).is_err());

        store.delete_session(second.session()).unwrap();
        let remaining = store.list_sessions(10).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].session, *first.session());
        assert!(!store.session_path(second.session()).exists());
        drop(store);
        let reopened = DirectorySessionStore::open(&directory).unwrap();
        assert_eq!(reopened.list_sessions(10).unwrap().len(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn directory_store_keeps_operation_capacity_per_session() {
        let directory = directory("per-session-capacity");
        let mut store = DirectorySessionStore::open(&directory).unwrap();
        let checkpoint_a = checkpoint_for("session-a", 1, "a");
        let checkpoint_b = checkpoint_for("session-b", 1, "b");
        store.initialize_session(&checkpoint_a).unwrap();
        store.initialize_session(&checkpoint_b).unwrap();
        let session = checkpoint_a.session();
        for index in 0..nexus_core::store::MAX_INTENT_RECORDS {
            let intent = ToolIntentRecord {
                run: RunId::new(format!("run-{index}")).unwrap(),
                call: CallId::new("call").unwrap(),
                tool: ToolId::new("read", M0_REVISION).unwrap(),
                args: NormalizedArgs::new("{}").unwrap(),
                scope: ApprovedScope::new("read").unwrap(),
            };
            store.record_session_intent(session, &intent).unwrap();
        }
        let other_intent = ToolIntentRecord {
            run: RunId::new("run-other").unwrap(),
            call: CallId::new("call").unwrap(),
            tool: ToolId::new("read", M0_REVISION).unwrap(),
            args: NormalizedArgs::new("{}").unwrap(),
            scope: ApprovedScope::new("read").unwrap(),
        };
        store
            .record_session_intent(checkpoint_b.session(), &other_intent)
            .unwrap();
        store.delete_session(session).unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn explicit_recovery_returns_validated_data_without_executing_records() {
        let directory = directory("recover");
        let mut store = FileSessionStore::open(&directory).unwrap();
        let checkpoint = checkpoint(1, "active view");
        store.save_checkpoint(&checkpoint).unwrap();
        let session = checkpoint.session().clone();
        let run = RunId::new("run-recovered").unwrap();
        let source = vec![
            ModelContextItem::user_text("archived input").unwrap(),
            ModelContextItem::assistant_text_with_reasoning(
                "answer-item",
                "archived answer",
                Some("retained reasoning".to_owned()),
            )
            .unwrap(),
        ];
        for (index, item) in source.iter().cloned().enumerate() {
            store
                .append_source_record(
                    &SourceRecord::new(session.clone(), index as u64 + 1, run.clone(), item)
                        .unwrap(),
                )
                .unwrap();
        }

        let recovered = recover_session(&store, &session).unwrap();
        assert_eq!(recovered.checkpoint(), &checkpoint);
        assert_eq!(recovered.source_records().len(), 2);
        assert_eq!(recovered.items().cloned().collect::<Vec<_>>(), source);
        drop(store);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn lazy_store_does_not_touch_history_until_first_operation() {
        let directory = directory("lazy-open");
        let store = LazyFileSessionStore::new(&directory);
        assert!(!directory.exists());
        assert!(store.list_sessions(8).unwrap().is_empty());
        assert!(directory.exists());
        assert!(!directory.join("session-store.json").exists());
        drop(store);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn initializes_once_and_appends_source_batches_atomically_in_order() {
        let directory = directory("source-batch");
        let mut store = FileSessionStore::open(&directory).unwrap();
        let checkpoint = checkpoint(1, "active view");
        assert!(store.initialize_session(&checkpoint).unwrap());
        assert!(!store.initialize_session(&checkpoint).unwrap());

        let session = checkpoint.session().clone();
        let run = RunId::new("run-batch").unwrap();
        let items = vec![
            ModelContextItem::user_text("first").unwrap(),
            ModelContextItem::assistant_text("answer", "second").unwrap(),
        ];
        assert_eq!(
            store.append_source_items(&session, &run, &items).unwrap(),
            2
        );
        assert_eq!(store.last_source_sequence(&session).unwrap(), 2);
        let records = store.load_source_records(&session, 0, 10).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].sequence(), 1);
        assert_eq!(records[1].sequence(), 2);
        assert_eq!(
            records
                .iter()
                .map(SourceRecord::item)
                .cloned()
                .collect::<Vec<_>>(),
            items
        );

        // The whole batch is rejected before committing when it would exceed
        // capacity; no prefix is visible afterward.
        let remaining = MAX_SOURCE_RECORDS - store.source_records.len();
        let too_many = vec![items[0].clone(); remaining + 1];
        assert!(
            store
                .append_source_items(&session, &run, &too_many)
                .is_err()
        );
        assert_eq!(store.last_source_sequence(&session).unwrap(), 2);

        drop(store);
        let reopened = FileSessionStore::open(&directory).unwrap();
        assert_eq!(reopened.last_source_sequence(&session).unwrap(), 2);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn source_batch_exceeding_parser_nodes_preserves_previous_snapshot() {
        let directory = directory("source-node-bound");
        let mut store = FileSessionStore::open(&directory).unwrap();
        let checkpoint = checkpoint(1, "active view");
        store.initialize_session(&checkpoint).unwrap();
        let session = checkpoint.session().clone();
        let run = RunId::new("run-batch").unwrap();
        let seed = ModelContextItem::user_text("retained source").unwrap();
        store.append_source_items(&session, &run, &[seed]).unwrap();
        let original = fs::read(&store.path).unwrap();
        let original_records = store.load_source_records(&session, 0, 10).unwrap();

        let item = ModelContextItem::tool_result(
            CallId::new("call-a").unwrap(),
            "item-result",
            "provider-call-a",
            ToolId::new("host_read", M0_REVISION).unwrap(),
            ToolOutcome::new(
                ExecutionStatus::Succeeded,
                EffectState::Unknown,
                Evidence::Uncertain,
                "result",
                false,
            )
            .unwrap(),
        )
        .unwrap();
        let items = vec![item; 13_000];
        let mut records = original_records.clone();
        for (index, item) in items.iter().enumerate() {
            records.push(
                SourceRecord::new(session.clone(), index as u64 + 2, run.clone(), item.clone())
                    .unwrap(),
            );
        }
        assert!(records.len() < MAX_SOURCE_RECORDS);
        let candidate = encode(&store.sessions, &store.intents, &store.outcomes, &records).unwrap();
        assert!(candidate.len() < MAX_STORE_BYTES);
        assert!(decode(&candidate).is_err());

        assert!(store.append_source_items(&session, &run, &items).is_err());
        assert_eq!(fs::read(&store.path).unwrap(), original);
        assert_eq!(store.load_session(&session).unwrap(), checkpoint);
        assert_eq!(store.last_source_sequence(&session).unwrap(), 1);
        assert_eq!(store.source_records, original_records);
        drop(store);

        let reopened = FileSessionStore::open(&directory).unwrap();
        assert_eq!(reopened.load_session(&session).unwrap(), checkpoint);
        assert_eq!(reopened.source_records, original_records);
        drop(reopened);
        fs::remove_dir_all(directory).unwrap();
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
    fn revision_two_snapshots_remain_readable_and_new_status_round_trips() {
        let directory = directory("revision-two");
        let session = SessionId::new("session-a").expect("valid session");
        let run = RunId::new("run-a").expect("valid run");
        let mut store = FileSessionStore::open(&directory).expect("store opens");
        store
            .initialize_session(&checkpoint(0, "seed"))
            .expect("checkpoint initializes");
        drop(store);

        let path = directory.join("session-store.json");
        let v3 = fs::read_to_string(&path).expect("snapshot reads");
        let v2 = v3.replacen("\"revision\":3", "\"revision\":2", 1);
        fs::write(&path, v2).expect("legacy revision snapshot written");
        let mut store = FileSessionStore::open(&directory).expect("revision 2 remains readable");
        let interrupted = ToolOutcome::new(
            ExecutionStatus::Interrupted,
            EffectState::Unknown,
            Evidence::Uncertain,
            "record interrupted",
            false,
        )
        .expect("interrupted outcome builds");
        let item = ModelContextItem::tool_result(
            CallId::new("call-a").expect("valid call"),
            "item-a",
            "ref-a",
            ToolId::new("host_read", M0_REVISION).expect("valid tool"),
            interrupted,
        )
        .expect("tool result builds");
        store
            .append_source_items(&session, &run, std::slice::from_ref(&item))
            .expect("revision 3 write succeeds");
        drop(store);

        let reopened = FileSessionStore::open(&directory).expect("revision 3 reopens");
        assert_eq!(
            reopened.load_source_records(&session, 0, 1).unwrap()[0].item(),
            &item
        );
        let saved = fs::read_to_string(&path).expect("new snapshot reads");
        assert!(saved.contains("\"revision\":3"));
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
        store
            .initialize_session(
                &SessionCheckpoint::new(
                    session.clone(),
                    nexus_core::STORE_FORMAT_REVISION,
                    1,
                    Vec::new(),
                    "profile-a",
                )
                .unwrap(),
            )
            .unwrap();
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
