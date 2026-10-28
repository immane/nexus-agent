//! Root-jailed file tools (`host_read` and `host_write` at the M0 revision).

use std::io::{Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use nexus_core::{
    AgentError, EffectState, ErrorCategory, Evidence, ExecutionStatus, M0_REVISION, RetryGuidance,
    ToolCall, ToolContext, ToolId, ToolOutcome, ToolPort, ToolSpec,
};

use crate::fs_access::BoundRoot;

/// Tool name. Matches the policy auto-read set, so scoped reads stay
/// automatic under the same authorization as the fake reader.
pub const HOST_READ_TOOL: &str = "host_read";
/// Tool name for listing direct children of a jailed directory.
pub const HOST_LIST_TOOL: &str = "host_list";
/// Tool name for searching text files under a jailed directory.
pub const HOST_SEARCH_TOOL: &str = "host_search";
/// Tool name for approval-gated exact text replacement.
pub const HOST_PATCH_TOOL: &str = "host_patch";

/// Closed input schema for bounded, zero-based byte pages (not line offsets).
/// The runtime schema subset has no `integer` type; `read_page` enforces it.
pub const READ_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"number","minimum":0,"maximum":9223372036854775807},"limit":{"type":"number","minimum":4,"maximum":16384}},"required":["path"],"additionalProperties":false}"#;

/// Shared closed validation for host adapters and the container wire protocol.
pub fn valid_read_arguments(value: &serde_json::Value) -> bool {
    read_page(value).is_some()
}

fn nonnegative_integer(value: &serde_json::Value) -> Option<u64> {
    if let Some(integer) = value.as_u64() {
        return Some(integer);
    }
    let number = value.as_f64()?;
    // i64::MAX rounds up to 2^63 as f64; an exclusive bound avoids
    // accepting an out-of-range float through a saturating cast.
    ((0.0..9_223_372_036_854_775_808.0).contains(&number) && number.fract() == 0.0)
        .then_some(number as u64)
}

fn read_page(value: &serde_json::Value) -> Option<(u64, usize)> {
    let object = value.as_object()?;
    if !object.get("path")?.is_string()
        || object
            .keys()
            .any(|key| !matches!(key.as_str(), "path" | "offset" | "limit"))
    {
        return None;
    }
    let offset = match object.get("offset") {
        Some(value) => nonnegative_integer(value)?,
        None => 0,
    };
    let limit = match object.get("limit") {
        Some(value) => usize::try_from(nonnegative_integer(value)?).ok()?,
        None => 8192,
    };
    (offset <= i64::MAX as u64 && (4..=16384).contains(&limit)).then_some((offset, limit))
}

/// Read chunk size in bytes. Cancellation is observed between chunks.
const READ_CHUNK_BYTES: usize = 65_536;

fn tool_error(category: ErrorCategory, message: &'static str) -> AgentError {
    AgentError::new(category, message, RetryGuidance::DoNotRetry)
        .expect("static safe tool message builds")
}

fn active_error(context: &ToolContext) -> AgentError {
    let (category, message) = if context.is_cancelled() {
        (ErrorCategory::Cancelled, "tool call was cancelled")
    } else {
        (ErrorCategory::Timeout, "tool deadline exceeded")
    };
    tool_error(category, message)
}

/// Read-only file executor jailed to one canonical root. Constructed with
/// [`ScopedReader::with_root`]; the root defaults to the process working
/// directory via [`ScopedReader::with_current_dir`].
pub struct ScopedReader {
    root: PathBuf,
    access: BoundRoot,
    spec: ToolSpec,
}

impl ScopedReader {
    /// Binds the jail to `root`, canonicalized once. Fails when the root
    /// cannot be canonicalized or is not a directory.
    pub fn with_root(root: &Path) -> Result<Self, AgentError> {
        let root = std::fs::canonicalize(root)
            .map_err(|_| tool_error(ErrorCategory::InvalidInput, "tool root is invalid"))?;
        if !root.is_dir() {
            return Err(tool_error(
                ErrorCategory::InvalidInput,
                "tool root is invalid",
            ));
        }
        let spec = ToolSpec::new(
            ToolId::new(HOST_READ_TOOL, M0_REVISION).expect("static tool identity builds"),
            "Read a bounded UTF-8 byte page inside the tool root. Defaults: offset=0, limit=8192. Both must be integers; limit is 4..16384. On truncation follow next_offset; offsets are zero-based bytes, not lines. No total size is inferred.",
            READ_SCHEMA,
        )
        .map_err(|_| tool_error(ErrorCategory::Internal, "tool description is invalid"))?;
        let access = BoundRoot::open(&root)
            .map_err(|_| tool_error(ErrorCategory::InvalidInput, "tool root is invalid"))?;
        Ok(Self { root, access, spec })
    }

    /// Binds the jail to the process working directory.
    pub fn with_current_dir() -> Result<Self, AgentError> {
        Self::with_root(Path::new("."))
    }

    /// Returns the canonical jail root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves admitted argument text to a jailed absolute path. Empty,
    /// null-byte, and escaping paths are refused with static diagnostics;
    /// an absolute path is accepted only when it canonicalizes inside the
    /// root.
    fn resolve(&self, arguments: &str) -> Result<PathBuf, AgentError> {
        let value: serde_json::Value = serde_json::from_str(arguments)
            .map_err(|_| tool_error(ErrorCategory::InvalidInput, "tool arguments are invalid"))?;
        let path = value
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| tool_error(ErrorCategory::InvalidInput, "tool arguments are invalid"))?;
        if path.is_empty() || path.contains('\0') {
            return Err(tool_error(
                ErrorCategory::InvalidInput,
                "tool path is invalid",
            ));
        }
        let joined = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.root.join(path)
        };
        let canonical = std::fs::canonicalize(&joined)
            .map_err(|_| tool_error(ErrorCategory::ToolFailure, "file cannot be read"))?;
        if !canonical.starts_with(&self.root) {
            return Err(tool_error(
                ErrorCategory::PermissionDenied,
                "path escapes the tool root",
            ));
        }
        Ok(canonical)
    }

    /// Reads a jailed file into bounded text, observing cancellation
    /// between chunks. Non-regular files and non-UTF-8 content fail
    /// without disclosure.
    fn read_scoped(
        &self,
        path: &Path,
        offset: u64,
        limit: usize,
        context: &ToolContext,
    ) -> Result<(String, bool), AgentError> {
        context.check_active()?;
        let mut file = self
            .access
            .open_file(path)
            .map_err(|_| tool_error(ErrorCategory::ToolFailure, "file cannot be read"))?;
        // Reserve the largest possible continuation header before reading so
        // later budget enforcement cannot remove source bytes behind a cursor.
        let header_bytes = format!("[more; next_offset={}]\n", offset + limit as u64).len();
        let capacity = limit.min(context.output_budget_bytes().saturating_sub(header_bytes));
        if capacity < 4 {
            return Err(tool_error(
                ErrorCategory::ResourceLimit,
                "read output budget is too small",
            ));
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|_| tool_error(ErrorCategory::ToolFailure, "file cannot be read"))?;
        // One probe byte establishes more; the source prefix is validated
        // separately so the probe cannot introduce a spurious UTF-8 error.
        let mut limited = file.take(capacity as u64 + 1);
        let mut bytes = Vec::new();
        let mut chunk = [0u8; READ_CHUNK_BYTES];
        loop {
            context.check_active()?;
            match limited.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => bytes.extend_from_slice(&chunk[..read]),
                Err(_) => {
                    return Err(tool_error(
                        ErrorCategory::ToolFailure,
                        "file cannot be read",
                    ));
                }
            }
        }
        let more = bytes.len() > capacity;
        bytes.truncate(capacity);
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(error) if more && error.utf8_error().error_len().is_none() => {
                // A bounded read can end inside the last UTF-8 character.
                // Reject actual invalid bytes, but retain the valid prefix
                // when the only invalidity is our deliberate cut.
                let end = error.utf8_error().valid_up_to();
                let mut bytes = error.into_bytes();
                bytes.truncate(end);
                String::from_utf8(bytes).expect("validated UTF-8 prefix")
            }
            Err(_) => {
                return Err(tool_error(
                    ErrorCategory::InvalidInput,
                    "file is not valid text",
                ));
            }
        };
        context.check_active()?;
        if more {
            let next = offset + text.len() as u64;
            Ok((format!("[more; next_offset={next}]\n{text}"), true))
        } else {
            Ok((text, false))
        }
    }

    pub(crate) fn execute_resolved(
        &self,
        path: &Path,
        args: &serde_json::Value,
        context: &ToolContext,
    ) -> ToolOutcome {
        let outcome = (|| {
            let Some((offset, limit)) = read_page(args) else {
                return Self::failed(tool_error(
                    ErrorCategory::InvalidInput,
                    "tool arguments are invalid",
                ));
            };
            if context.check_active().is_err() {
                return Self::failed(tool_error(
                    if context.is_cancelled() {
                        ErrorCategory::Cancelled
                    } else {
                        ErrorCategory::Timeout
                    },
                    "filesystem call is no longer active",
                ));
            }
            let (text, truncated) = match self.read_scoped(path, offset, limit, context) {
                Ok(result) => result,
                Err(error) => return Self::failed(error),
            };
            match ToolOutcome::from_bounded_content(
                ExecutionStatus::Succeeded,
                EffectState::KnownNotApplied,
                Evidence::HostObserved,
                text,
                truncated,
            ) {
                Ok(outcome) => outcome,
                Err(_) => Self::failed(tool_error(
                    ErrorCategory::Internal,
                    "tool output could not be bounded",
                )),
            }
        })();
        outcome
            .enforce_budget(context.output_budget_bytes())
            .expect("valid read output budget")
    }

    /// Maps an internal failure to an honest outcome: unknown effects,
    /// uncertain evidence, static diagnostics, never fabricated content.
    fn failed(error: AgentError) -> ToolOutcome {
        let (status, effect) = match error.category() {
            ErrorCategory::Cancelled => (ExecutionStatus::Cancelled, EffectState::Unknown),
            _ => (ExecutionStatus::Failed, EffectState::Unknown),
        };
        ToolOutcome::new(
            status,
            effect,
            Evidence::Uncertain,
            match error.category() {
                ErrorCategory::PermissionDenied => "path escapes the tool root",
                ErrorCategory::Cancelled => "tool call was cancelled",
                ErrorCategory::InvalidInput => "tool input is invalid",
                ErrorCategory::ResourceLimit => "read output budget is too small",
                _ => "file cannot be read",
            },
            false,
        )
        .expect("static safe failure builds")
    }

    /// Maps a jail refusal to a denial: never executed, never started.
    fn denied() -> ToolOutcome {
        ToolOutcome::new(
            ExecutionStatus::Denied,
            EffectState::NotStarted,
            Evidence::HostObserved,
            "path escapes the tool root",
            false,
        )
        .expect("static safe denial builds")
    }
}

impl ToolPort for ScopedReader {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn execute(&self, call: &ToolCall, context: &ToolContext) -> ToolOutcome {
        let outcome = (|| {
            if context.is_cancelled() {
                return Self::failed(tool_error(
                    ErrorCategory::Cancelled,
                    "tool call was cancelled",
                ));
            }
            let path = match self.resolve(call.args().as_str()) {
                Ok(path) => path,
                Err(error) if error.category() == ErrorCategory::PermissionDenied => {
                    return Self::denied();
                }
                Err(error) => return Self::failed(error),
            };
            let args =
                serde_json::from_str(call.args().as_str()).expect("resolve parsed arguments");
            self.execute_resolved(&path, &args, context)
        })();
        outcome
            .enforce_budget(context.output_budget_bytes())
            .expect("valid read output budget")
    }
}

/// Closed input schema for directory listing.
pub const LIST_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}"#;
/// Closed input schema for recursive text search.
pub const SEARCH_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string"},"query":{"type":"string"}},"required":["path","query"],"additionalProperties":false}"#;
const SEARCH_FILE_BYTES: u64 = 1_048_576;
const LIST_ENTRY_LIMIT: usize = 10_000;

/// Root-jailed directory listing tool. Lists direct children only.
pub struct ScopedLister {
    root: PathBuf,
    access: BoundRoot,
    spec: ToolSpec,
    protected: Option<nexus_permissions::DirectoryPolicy>,
    entry_limit: usize,
}

impl ScopedLister {
    /// Bind the tool to an existing directory root.
    pub fn with_root(root: &Path) -> Result<Self, AgentError> {
        let root = canonical_root(root)?;
        let access = BoundRoot::open(&root)
            .map_err(|_| tool_error(ErrorCategory::InvalidInput, "tool root is invalid"))?;
        let spec = ToolSpec::new(
            ToolId::new(HOST_LIST_TOOL, M0_REVISION).expect("static tool identity builds"),
            "List direct children of a directory inside the tool root",
            LIST_SCHEMA,
        )
        .map_err(|_| tool_error(ErrorCategory::Internal, "tool description is invalid"))?;
        Ok(Self {
            root,
            access,
            spec,
            protected: None,
            entry_limit: LIST_ENTRY_LIMIT,
        })
    }
    /// Applies validated operational ceilings without changing the jail or policy.
    pub fn with_settings(
        mut self,
        settings: &nexus_config::ToolsSettings,
    ) -> Result<Self, AgentError> {
        settings
            .validate()
            .map_err(|_| tool_error(ErrorCategory::InvalidInput, "tools settings are invalid"))?;
        self.entry_limit = settings.list_entry_limit;
        Ok(self)
    }
    pub(crate) fn with_protected_paths(
        mut self,
        policy: nexus_permissions::DirectoryPolicy,
    ) -> Self {
        self.protected = Some(policy);
        self
    }
    /// Bind to the process working directory.
    pub fn with_current_dir() -> Result<Self, AgentError> {
        Self::with_root(Path::new("."))
    }
    fn execute_inner(
        &self,
        call: &ToolCall,
        context: &ToolContext,
    ) -> Result<(String, bool), AgentError> {
        let path = resolve_existing(&self.root, call.args().as_str())?;
        self.execute_directory(&path, context)
    }

    fn execute_directory(
        &self,
        path: &Path,
        context: &ToolContext,
    ) -> Result<(String, bool), AgentError> {
        if !path.is_dir() {
            return Err(tool_error(
                ErrorCategory::ToolFailure,
                "directory cannot be listed",
            ));
        }
        let mut entries = Vec::new();
        let directory = self
            .access
            .open_directory(path)
            .map_err(|_| tool_error(ErrorCategory::ToolFailure, "directory cannot be listed"))?;
        let mut iterator = rustix::fs::Dir::read_from(&directory)
            .map_err(|_| tool_error(ErrorCategory::ToolFailure, "directory cannot be listed"))?;
        while entries.len() < self.entry_limit {
            match iterator.read() {
                Some(Ok(entry)) => {
                    if entry.file_name().to_bytes() != b"." && entry.file_name().to_bytes() != b".."
                    {
                        entries.push(entry);
                    }
                }
                Some(Err(_)) => {
                    return Err(tool_error(
                        ErrorCategory::ToolFailure,
                        "directory cannot be listed",
                    ));
                }
                None => break,
            }
        }
        let mut truncated = entries.len() == self.entry_limit && iterator.read().is_some();
        entries.sort_by_key(|entry| entry.file_name().to_owned());
        let mut output = String::new();
        for entry in entries {
            context.check_active()?;
            let raw_name = std::ffi::OsStr::from_bytes(entry.file_name().to_bytes());
            let entry_path = path.join(raw_name);
            let name = entry_path
                .file_name()
                .expect("joined directory entry has a name")
                .to_string_lossy();
            if self.protected.as_ref().is_some_and(|policy| {
                policy.is_protected(&entry_path)
                    || entry_path
                        .canonicalize()
                        .is_ok_and(|path| policy.is_protected(&path))
            }) {
                continue;
            }
            let suffix = if entry.file_type() == rustix::fs::FileType::Directory {
                "/"
            } else {
                ""
            };
            if output
                .len()
                .saturating_add(name.len())
                .saturating_add(suffix.len())
                .saturating_add(1)
                > context.output_budget_bytes()
            {
                truncated = true;
                break;
            }
            output.push_str(&name);
            output.push_str(suffix);
            output.push('\n');
        }
        Ok((output, truncated))
    }

    pub(crate) fn execute_resolved(&self, path: &Path, context: &ToolContext) -> ToolOutcome {
        if context.check_active().is_err() {
            return ScopedReader::failed(tool_error(
                if context.is_cancelled() {
                    ErrorCategory::Cancelled
                } else {
                    ErrorCategory::Timeout
                },
                "filesystem call is no longer active",
            ));
        }
        match self.execute_directory(path, context) {
            Err(error) if error.category() == ErrorCategory::PermissionDenied => {
                ScopedReader::denied()
            }
            result => read_result(result, context, "directory cannot be listed"),
        }
    }
}

/// Root-jailed recursive UTF-8 text search. Results contain relative path, line number and matching line.
pub struct ScopedSearcher {
    root: PathBuf,
    access: BoundRoot,
    spec: ToolSpec,
    protected: Option<nexus_permissions::DirectoryPolicy>,
    file_bytes: u64,
}

impl ScopedSearcher {
    /// Bind the tool to an existing directory root.
    pub fn with_root(root: &Path) -> Result<Self, AgentError> {
        let root = canonical_root(root)?;
        let access = BoundRoot::open(&root)
            .map_err(|_| tool_error(ErrorCategory::InvalidInput, "tool root is invalid"))?;
        let spec = ToolSpec::new(
            ToolId::new(HOST_SEARCH_TOOL, M0_REVISION).expect("static tool identity builds"),
            "Search UTF-8 text files beneath a directory inside the tool root",
            SEARCH_SCHEMA,
        )
        .map_err(|_| tool_error(ErrorCategory::Internal, "tool description is invalid"))?;
        Ok(Self {
            root,
            access,
            spec,
            protected: None,
            file_bytes: SEARCH_FILE_BYTES,
        })
    }
    /// Applies validated operational ceilings without changing the jail or policy.
    pub fn with_settings(
        mut self,
        settings: &nexus_config::ToolsSettings,
    ) -> Result<Self, AgentError> {
        settings
            .validate()
            .map_err(|_| tool_error(ErrorCategory::InvalidInput, "tools settings are invalid"))?;
        self.file_bytes = settings.search_file_bytes;
        Ok(self)
    }
    pub(crate) fn with_protected_paths(
        mut self,
        policy: nexus_permissions::DirectoryPolicy,
    ) -> Self {
        self.protected = Some(policy);
        self
    }
    /// Bind to the process working directory.
    pub fn with_current_dir() -> Result<Self, AgentError> {
        Self::with_root(Path::new("."))
    }
    fn execute_inner(
        &self,
        call: &ToolCall,
        context: &ToolContext,
    ) -> Result<(String, bool), AgentError> {
        let (directory, query) = parse_path_query(call.args().as_str())?;
        let base = resolve_path(&self.root, &directory)?;
        self.execute_search(&base, &query, context)
    }

    fn execute_search(
        &self,
        base: &Path,
        query: &str,
        context: &ToolContext,
    ) -> Result<(String, bool), AgentError> {
        if !base.is_dir() || query.is_empty() {
            return Err(tool_error(
                ErrorCategory::InvalidInput,
                "tool input is invalid",
            ));
        }
        let mut stack = vec![base.to_path_buf()];
        let mut output = String::new();
        let mut truncated = false;
        while let Some(dir) = stack.pop() {
            context.check_active()?;
            let directory = self.access.open_directory(&dir).map_err(|_| {
                tool_error(ErrorCategory::ToolFailure, "search cannot be completed")
            })?;
            let entries = rustix::fs::Dir::read_from(&directory).map_err(|_| {
                tool_error(ErrorCategory::ToolFailure, "search cannot be completed")
            })?;
            for entry in entries {
                context.check_active()?;
                let entry = entry.map_err(|_| {
                    tool_error(ErrorCategory::ToolFailure, "search cannot be completed")
                })?;
                if entry.file_name().to_bytes() == b"." || entry.file_name().to_bytes() == b".." {
                    continue;
                }
                let name = std::ffi::OsStr::from_bytes(entry.file_name().to_bytes());
                let path = dir.join(name);
                if self
                    .protected
                    .as_ref()
                    .is_some_and(|policy| policy.is_protected(&path))
                {
                    continue;
                }
                let metadata = match std::fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(_) => continue,
                };
                if metadata.file_type().is_symlink() {
                    continue;
                }
                if metadata.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !metadata.is_file() {
                    continue;
                }
                if self
                    .protected
                    .as_ref()
                    .is_some_and(|policy| policy.is_protected(&path))
                {
                    continue;
                }
                let Ok(file) = self.access.open_file(&path) else {
                    continue;
                };
                let mut limited = file.take(self.file_bytes + 1);
                let mut bytes = Vec::new();
                if limited.read_to_end(&mut bytes).is_err() {
                    continue;
                }
                let file_truncated = bytes.len() as u64 > self.file_bytes;
                if file_truncated {
                    truncated = true;
                    bytes.truncate(self.file_bytes as usize);
                }
                let text = match std::str::from_utf8(&bytes) {
                    Ok(text) => text,
                    Err(error) if file_truncated && error.error_len().is_none() => {
                        std::str::from_utf8(&bytes[..error.valid_up_to()])
                            .expect("validated UTF-8 prefix")
                    }
                    Err(_) => continue,
                };
                for (index, line) in text.lines().enumerate() {
                    if line.contains(query) {
                        let relative = path.strip_prefix(base).unwrap_or(&path).to_string_lossy();
                        let record = format!("{relative}:{}:{line}\n", index + 1);
                        if output.len().saturating_add(record.len()) > context.output_budget_bytes()
                        {
                            truncated = true;
                            return Ok((output, truncated));
                        }
                        output.push_str(&record);
                    }
                }
            }
        }
        Ok((output, truncated))
    }

    pub(crate) fn execute_resolved(
        &self,
        base: &Path,
        query: &str,
        context: &ToolContext,
    ) -> ToolOutcome {
        if context.check_active().is_err() {
            return ScopedReader::failed(tool_error(
                if context.is_cancelled() {
                    ErrorCategory::Cancelled
                } else {
                    ErrorCategory::Timeout
                },
                "filesystem call is no longer active",
            ));
        }
        match self.execute_search(base, query, context) {
            Err(error) if error.category() == ErrorCategory::PermissionDenied => {
                ScopedReader::denied()
            }
            result => read_result(result, context, "search cannot be completed"),
        }
    }
}

fn canonical_root(root: &Path) -> Result<PathBuf, AgentError> {
    let root = std::fs::canonicalize(root)
        .map_err(|_| tool_error(ErrorCategory::InvalidInput, "tool root is invalid"))?;
    if !root.is_dir() {
        return Err(tool_error(
            ErrorCategory::InvalidInput,
            "tool root is invalid",
        ));
    }
    Ok(root)
}
fn parse_path_query(args: &str) -> Result<(String, String), AgentError> {
    let value: serde_json::Value = serde_json::from_str(args)
        .map_err(|_| tool_error(ErrorCategory::InvalidInput, "tool arguments are invalid"))?;
    let path = value
        .get("path")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty() && !s.contains('\0'));
    let query = value
        .get("query")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty());
    match (path, query) {
        (Some(path), Some(query)) => Ok((path.to_owned(), query.to_owned())),
        _ => Err(tool_error(
            ErrorCategory::InvalidInput,
            "tool arguments are invalid",
        )),
    }
}
fn resolve_path(root: &Path, path: &str) -> Result<PathBuf, AgentError> {
    let joined = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        root.join(path)
    };
    let canonical = std::fs::canonicalize(joined)
        .map_err(|_| tool_error(ErrorCategory::ToolFailure, "file cannot be read"))?;
    if !canonical.starts_with(root) {
        return Err(tool_error(
            ErrorCategory::PermissionDenied,
            "path escapes the tool root",
        ));
    }
    Ok(canonical)
}
fn resolve_existing(root: &Path, args: &str) -> Result<PathBuf, AgentError> {
    let value: serde_json::Value = serde_json::from_str(args)
        .map_err(|_| tool_error(ErrorCategory::InvalidInput, "tool arguments are invalid"))?;
    let path = value
        .get("path")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty() && !s.contains('\0'))
        .ok_or_else(|| tool_error(ErrorCategory::InvalidInput, "tool arguments are invalid"))?;
    resolve_path(root, path)
}
fn read_result(
    result: Result<(String, bool), AgentError>,
    context: &ToolContext,
    failure: &'static str,
) -> ToolOutcome {
    let (text, truncated) = match result {
        Ok(value) => value,
        Err(error) => return ScopedReader::failed(error),
    };
    match ToolOutcome::from_bounded_content(
        ExecutionStatus::Succeeded,
        EffectState::KnownNotApplied,
        Evidence::HostObserved,
        text,
        truncated,
    )
    .and_then(|outcome| outcome.enforce_budget(context.output_budget_bytes()))
    {
        Ok(outcome) => outcome,
        Err(_) => ToolOutcome::new(
            ExecutionStatus::Failed,
            EffectState::Unknown,
            Evidence::Uncertain,
            failure,
            false,
        )
        .expect("static failure builds"),
    }
}
impl ToolPort for ScopedLister {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }
    fn execute(&self, call: &ToolCall, context: &ToolContext) -> ToolOutcome {
        if context.is_cancelled() {
            return ScopedReader::failed(tool_error(
                ErrorCategory::Cancelled,
                "tool call was cancelled",
            ));
        }
        match self.execute_inner(call, context) {
            Err(error) if error.category() == ErrorCategory::PermissionDenied => {
                ScopedReader::denied()
            }
            result => read_result(result, context, "directory cannot be listed"),
        }
    }
}
impl ToolPort for ScopedSearcher {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }
    fn execute(&self, call: &ToolCall, context: &ToolContext) -> ToolOutcome {
        if context.is_cancelled() {
            return ScopedReader::failed(tool_error(
                ErrorCategory::Cancelled,
                "tool call was cancelled",
            ));
        }
        match self.execute_inner(call, context) {
            Err(error) if error.category() == ErrorCategory::PermissionDenied => {
                ScopedReader::denied()
            }
            result => read_result(result, context, "search cannot be completed"),
        }
    }
}

/// Closed input schema for a unique exact-text patch.
pub const PATCH_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string"},"old_text":{"type":"string"},"new_text":{"type":"string"}},"required":["path","old_text","new_text"],"additionalProperties":false}"#;
const PATCH_FILE_BYTES: u64 = 8 * 1_048_576;

/// Approval-gated exact text patcher jailed to one canonical root.
/// Requires exactly one occurrence of `old_text`; it never creates files.
pub struct ScopedPatcher {
    root: PathBuf,
    access: BoundRoot,
    spec: ToolSpec,
}

impl ScopedPatcher {
    /// Bind the tool to an existing directory root.
    pub fn with_root(root: &Path) -> Result<Self, AgentError> {
        Self::with_root_policy(root, false)
    }

    /// Shared workspaces may report some Linux metadata features as
    /// unsupported. Preserve all metadata the filesystem exposes, but allow
    /// those explicitly unsupported features.
    pub fn with_shared_workspace_root(root: &Path) -> Result<Self, AgentError> {
        Self::with_root_policy(root, true)
    }

    fn with_root_policy(root: &Path, container_workspace: bool) -> Result<Self, AgentError> {
        let root = canonical_root(root)?;
        let access = (if container_workspace {
            BoundRoot::open_container_workspace(&root)
        } else {
            BoundRoot::open(&root)
        })
        .map_err(|_| tool_error(ErrorCategory::InvalidInput, "tool root is invalid"))?;
        let spec = ToolSpec::new(
            ToolId::new(HOST_PATCH_TOOL, M0_REVISION).expect("static tool identity builds"),
            "Replace one unique exact text occurrence in an existing UTF-8 file inside the tool root. Requires approval.",
            PATCH_SCHEMA,
        )
        .map_err(|_| tool_error(ErrorCategory::Internal, "tool description is invalid"))?;
        Ok(Self { root, access, spec })
    }

    /// Bind to the process working directory.
    pub fn with_current_dir() -> Result<Self, AgentError> {
        Self::with_root(Path::new("."))
    }

    fn apply(&self, call: &ToolCall, context: &ToolContext) -> Result<usize, AgentError> {
        let value: serde_json::Value = serde_json::from_str(call.args().as_str())
            .map_err(|_| tool_error(ErrorCategory::InvalidInput, "tool arguments are invalid"))?;
        let get = |key| {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    tool_error(ErrorCategory::InvalidInput, "tool arguments are invalid")
                })
        };
        let path = get("path")?;
        let old_text = get("old_text")?;
        let new_text = get("new_text")?;
        if path.is_empty() || path.contains('\0') || old_text.is_empty() {
            return Err(tool_error(
                ErrorCategory::InvalidInput,
                "tool arguments are invalid",
            ));
        }
        let target = resolve_path(&self.root, path)?;
        self.apply_resolved(&target, old_text, new_text, context)
    }

    fn apply_resolved(
        &self,
        target: &Path,
        old_text: &str,
        new_text: &str,
        context: &ToolContext,
    ) -> Result<usize, AgentError> {
        if old_text.is_empty() {
            return Err(tool_error(
                ErrorCategory::InvalidInput,
                "tool arguments are invalid",
            ));
        }
        if !target.is_file() {
            return Err(tool_error(
                ErrorCategory::ToolFailure,
                "file cannot be patched",
            ));
        }
        let source = self
            .access
            .open_file(target)
            .map_err(|_| tool_error(ErrorCategory::ToolFailure, "file cannot be patched"))?;
        let stamp = crate::fs_access::FileStamp::read(&source)
            .map_err(|_| tool_error(ErrorCategory::ToolFailure, "file cannot be patched"))?;
        let mut limited = (&source).take(PATCH_FILE_BYTES + 1);
        let mut bytes = Vec::new();
        let mut chunk = [0u8; READ_CHUNK_BYTES];
        loop {
            context.check_active()?;
            let read = limited
                .read(&mut chunk)
                .map_err(|_| tool_error(ErrorCategory::ToolFailure, "file cannot be patched"))?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
            if bytes.len() as u64 > PATCH_FILE_BYTES {
                return Err(tool_error(
                    ErrorCategory::ResourceLimit,
                    "file exceeds patch limit",
                ));
            }
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| tool_error(ErrorCategory::InvalidInput, "file is not valid text"))?;
        let mut matches = text.match_indices(old_text);
        let Some((index, _)) = matches.next() else {
            return Err(tool_error(
                ErrorCategory::InvalidInput,
                "patch did not match exactly once",
            ));
        };
        if matches.next().is_some() {
            return Err(tool_error(
                ErrorCategory::InvalidInput,
                "patch did not match exactly once",
            ));
        }
        context.check_active()?;
        let mut replacement = String::with_capacity(text.len() - old_text.len() + new_text.len());
        replacement.push_str(&text[..index]);
        replacement.push_str(new_text);
        replacement.push_str(&text[index + old_text.len()..]);

        let canonical = std::fs::canonicalize(target)
            .map_err(|_| tool_error(ErrorCategory::ToolFailure, "file cannot be patched"))?;
        if !canonical.starts_with(&self.root) {
            return Err(tool_error(
                ErrorCategory::PermissionDenied,
                "path escapes the tool root",
            ));
        }
        if !canonical.is_file() {
            return Err(tool_error(
                ErrorCategory::ToolFailure,
                "file cannot be patched",
            ));
        }
        let parent = canonical
            .parent()
            .ok_or_else(|| tool_error(ErrorCategory::ToolFailure, "file cannot be patched"))?;
        let parent_fd = self
            .access
            .open_directory(parent)
            .map_err(|_| tool_error(ErrorCategory::ToolFailure, "file cannot be patched"))?;
        let name = canonical
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| tool_error(ErrorCategory::ToolFailure, "file cannot be patched"))?;
        crate::fs_access::replace_existing(
            &parent_fd,
            name,
            &source,
            stamp,
            replacement.as_bytes(),
            self.access.replacement_metadata(),
            || context.check_active().is_err(),
        )
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::Interrupted {
                active_error(context)
            } else {
                tool_error(ErrorCategory::ToolFailure, "file cannot be patched")
            }
        })?;
        Ok(replacement.len())
    }

    pub(crate) fn execute_development(
        &self,
        target: &Path,
        old_text: &str,
        new_text: &str,
        context: &ToolContext,
    ) -> ToolOutcome {
        if context.check_active().is_err() {
            return ScopedWriter::failed(tool_error(
                if context.is_cancelled() {
                    ErrorCategory::Cancelled
                } else {
                    ErrorCategory::Timeout
                },
                "filesystem call is no longer active",
            ));
        }
        match self.apply_resolved(target, old_text, new_text, context) {
            Ok(bytes) => ToolOutcome::from_bounded_content(
                ExecutionStatus::Succeeded,
                EffectState::KnownApplied,
                Evidence::HostObserved,
                format!("patched file ({bytes} bytes after patch)"),
                false,
            )
            .and_then(|outcome| outcome.enforce_budget(context.output_budget_bytes()))
            .unwrap_or_else(|_| {
                ToolOutcome::new(
                    ExecutionStatus::Failed,
                    EffectState::Unknown,
                    Evidence::Uncertain,
                    "patch result could not be bounded",
                    false,
                )
                .expect("static failure builds")
            }),
            Err(error) if error.category() == ErrorCategory::PermissionDenied => {
                ScopedWriter::denied()
            }
            Err(error)
                if error.category() == ErrorCategory::InvalidInput
                    && error.message() == "patch did not match exactly once" =>
            {
                ToolOutcome::new(
                    ExecutionStatus::Failed,
                    EffectState::KnownNotApplied,
                    Evidence::HostObserved,
                    "patch did not match exactly once",
                    false,
                )
                .expect("static patch refusal builds")
            }
            Err(error) if error.category() == ErrorCategory::ResourceLimit => ToolOutcome::new(
                ExecutionStatus::Failed,
                EffectState::KnownNotApplied,
                Evidence::HostObserved,
                "file exceeds patch limit",
                false,
            )
            .expect("static patch limit builds"),
            Err(error) => ScopedWriter::failed(error),
        }
    }
}

impl ToolPort for ScopedPatcher {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }
    fn execute(&self, call: &ToolCall, context: &ToolContext) -> ToolOutcome {
        if context.is_cancelled() {
            return ScopedWriter::failed(tool_error(
                ErrorCategory::Cancelled,
                "tool call was cancelled",
            ));
        }
        match self.apply(call, context) {
            Ok(bytes) => ToolOutcome::from_bounded_content(
                ExecutionStatus::Succeeded,
                EffectState::KnownApplied,
                Evidence::HostObserved,
                format!("patched file ({bytes} bytes after patch)"),
                false,
            )
            .and_then(|outcome| outcome.enforce_budget(context.output_budget_bytes()))
            .unwrap_or_else(|_| {
                ToolOutcome::new(
                    ExecutionStatus::Failed,
                    EffectState::Unknown,
                    Evidence::Uncertain,
                    "patch result could not be bounded",
                    false,
                )
                .expect("static failure builds")
            }),
            Err(error) if error.category() == ErrorCategory::PermissionDenied => {
                ScopedWriter::denied()
            }
            Err(error)
                if error.category() == ErrorCategory::InvalidInput
                    && error.message() == "patch did not match exactly once" =>
            {
                ToolOutcome::new(
                    ExecutionStatus::Failed,
                    EffectState::KnownNotApplied,
                    Evidence::HostObserved,
                    "patch did not match exactly once",
                    false,
                )
                .expect("static patch refusal builds")
            }
            Err(error) if error.category() == ErrorCategory::ResourceLimit => ToolOutcome::new(
                ExecutionStatus::Failed,
                EffectState::KnownNotApplied,
                Evidence::HostObserved,
                "file exceeds patch limit",
                false,
            )
            .expect("static patch limit builds"),
            Err(error) => ScopedWriter::failed(error),
        }
    }
}

/// Tool name. Deliberately outside the policy auto-read set: every write
/// needs an approval grant, exactly like the fake mutation it replaces in
/// real wiring.
pub const HOST_WRITE_TOOL: &str = "host_write";

/// Closed input schema: a `path` plus the full replacement `content`.
/// Both keys are required so a path-only call (a read-shaped mistake the
/// model otherwise makes) is rejected at admission with a clean denial
/// instead of writing an empty file.
pub const WRITE_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"],"additionalProperties":false}"#;

/// Real file writer jailed to one canonical root. Constructed with
/// [`ScopedWriter::with_root`]. Creates the file when missing (the parent
/// directory must already exist; no directories are created) and replaces
/// the whole content otherwise. Directories, special files, and
/// jail escapes are refused before any write.
pub struct ScopedWriter {
    root: PathBuf,
    access: BoundRoot,
    spec: ToolSpec,
}

impl ScopedWriter {
    /// Binds the jail to `root`, canonicalized once. Fails when the root
    /// cannot be canonicalized or is not a directory.
    pub fn with_root(root: &Path) -> Result<Self, AgentError> {
        Self::with_root_policy(root, false)
    }

    /// Shared workspaces may report some Linux metadata features as
    /// unsupported. Preserve all metadata the filesystem exposes, but allow
    /// those explicitly unsupported features.
    pub fn with_shared_workspace_root(root: &Path) -> Result<Self, AgentError> {
        Self::with_root_policy(root, true)
    }

    fn with_root_policy(root: &Path, container_workspace: bool) -> Result<Self, AgentError> {
        let root = std::fs::canonicalize(root)
            .map_err(|_| tool_error(ErrorCategory::InvalidInput, "tool root is invalid"))?;
        if !root.is_dir() {
            return Err(tool_error(
                ErrorCategory::InvalidInput,
                "tool root is invalid",
            ));
        }
        let access = (if container_workspace {
            BoundRoot::open_container_workspace(&root)
        } else {
            BoundRoot::open(&root)
        })
        .map_err(|_| tool_error(ErrorCategory::InvalidInput, "tool root is invalid"))?;
        let spec = ToolSpec::new(
            ToolId::new(HOST_WRITE_TOOL, M0_REVISION).expect("static tool identity builds"),
            "Write a file inside the tool root, creating it when missing and replacing its whole content otherwise. Parent directories must already exist. Takes exactly {\"path\": \"notes/todo.txt\", \"content\": \"...\"}.",
            WRITE_SCHEMA,
        )
        .map_err(|_| tool_error(ErrorCategory::Internal, "tool description is invalid"))?;
        Ok(Self { root, access, spec })
    }

    /// Returns the canonical jail root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves admitted argument text to a jailed parent directory, a
    /// single file name, and the replacement content. The parent must
    /// already exist so no directories are ever created; the file name is
    /// one component by construction, so joining it onto the canonical
    /// parent cannot escape even before the target exists.
    fn resolve(&self, arguments: &str) -> Result<(PathBuf, String, String), AgentError> {
        let invalid = || tool_error(ErrorCategory::InvalidInput, "tool arguments are invalid");
        let value: serde_json::Value = serde_json::from_str(arguments).map_err(|_| invalid())?;
        let path = value
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(invalid)?;
        let content = value
            .get("content")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(invalid)?;
        if path.is_empty() || path.contains('\0') {
            return Err(tool_error(
                ErrorCategory::InvalidInput,
                "tool path is invalid",
            ));
        }
        let joined = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.root.join(path)
        };
        let candidate_name = joined
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty() && !name.contains('\0'))
            .ok_or_else(|| tool_error(ErrorCategory::InvalidInput, "tool path is invalid"))?
            .to_owned();
        let candidate = joined;
        let resolved_target =
            match std::fs::symlink_metadata(&candidate) {
                Ok(_) => Some(std::fs::canonicalize(&candidate).map_err(|_| {
                    tool_error(ErrorCategory::ToolFailure, "file cannot be written")
                })?),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => {
                    return Err(tool_error(
                        ErrorCategory::ToolFailure,
                        "file cannot be written",
                    ));
                }
            };
        let (parent, name) = if let Some(target) = resolved_target {
            if !target.starts_with(&self.root) {
                return Err(tool_error(
                    ErrorCategory::PermissionDenied,
                    "path escapes the tool root",
                ));
            }
            if !target.is_file() {
                return Err(tool_error(
                    ErrorCategory::ToolFailure,
                    "file cannot be written",
                ));
            }
            (
                target.parent().ok_or_else(invalid)?.to_path_buf(),
                target
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(invalid)?
                    .to_owned(),
            )
        } else {
            let parent = candidate
                .parent()
                .ok_or_else(|| tool_error(ErrorCategory::InvalidInput, "tool path is invalid"))?;
            let canonical = std::fs::canonicalize(parent)
                .map_err(|_| tool_error(ErrorCategory::ToolFailure, "file cannot be written"))?;
            if !canonical.starts_with(&self.root) {
                return Err(tool_error(
                    ErrorCategory::PermissionDenied,
                    "path escapes the tool root",
                ));
            }
            (canonical, candidate_name)
        };
        Ok((parent, name, content.to_owned()))
    }

    /// Stages content beside its destination and publishes it with a
    /// descriptor-relative rename after revalidating the source object.
    fn write_scoped(
        &self,
        parent: &Path,
        name: &str,
        content: &str,
        context: &ToolContext,
    ) -> Result<usize, AgentError> {
        context.check_active()?;
        let parent_fd = self
            .access
            .open_directory(parent)
            .map_err(|_| tool_error(ErrorCategory::ToolFailure, "file cannot be written"))?;
        let target = parent.join(name);
        match self.access.open_file(&target) {
            Ok(source) => {
                let stamp = crate::fs_access::FileStamp::read(&source).map_err(|_| {
                    tool_error(ErrorCategory::ToolFailure, "file cannot be written")
                })?;
                crate::fs_access::replace_existing(
                    &parent_fd,
                    name,
                    &source,
                    stamp,
                    content.as_bytes(),
                    self.access.replacement_metadata(),
                    || context.check_active().is_err(),
                )
                .map_err(|error| {
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        active_error(context)
                    } else {
                        tool_error(ErrorCategory::ToolFailure, "file cannot be written")
                    }
                })?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                crate::fs_access::create_new(&parent_fd, name, content.as_bytes(), || {
                    context.check_active().is_err()
                })
                .map_err(|error| {
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        active_error(context)
                    } else {
                        tool_error(ErrorCategory::ToolFailure, "file cannot be written")
                    }
                })?;
            }
            Err(_) => {
                return Err(tool_error(
                    ErrorCategory::ToolFailure,
                    "file cannot be written",
                ));
            }
        }
        Ok(content.len())
    }

    pub(crate) fn execute_development(
        &self,
        target: &Path,
        content: &str,
        context: &ToolContext,
    ) -> ToolOutcome {
        if context.check_active().is_err() {
            return Self::failed(tool_error(
                if context.is_cancelled() {
                    ErrorCategory::Cancelled
                } else {
                    ErrorCategory::Timeout
                },
                "filesystem call is no longer active",
            ));
        }
        let parent = match target.parent() {
            Some(parent) => parent,
            None => {
                return Self::failed(tool_error(
                    ErrorCategory::InvalidInput,
                    "tool path is invalid",
                ));
            }
        };
        let name = match target.file_name().and_then(|name| name.to_str()) {
            Some(name) if !name.is_empty() => name,
            _ => {
                return Self::failed(tool_error(
                    ErrorCategory::InvalidInput,
                    "tool path is invalid",
                ));
            }
        };
        let written = match self.write_scoped(parent, name, content, context) {
            Ok(written) => written,
            Err(error) if error.category() == ErrorCategory::PermissionDenied => {
                return Self::denied();
            }
            Err(error) => return Self::failed(error),
        };
        ToolOutcome::from_bounded_content(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            format!("wrote {written} bytes"),
            false,
        )
        .and_then(|outcome| outcome.enforce_budget(context.output_budget_bytes()))
        .unwrap_or_else(|_| {
            Self::failed(tool_error(
                ErrorCategory::Internal,
                "tool output could not be bounded",
            ))
        })
    }

    /// Maps an internal failure to an honest outcome: unknown effects,
    /// uncertain evidence, static diagnostics, never fabricated content.
    fn failed(error: AgentError) -> ToolOutcome {
        let (status, effect) = match error.category() {
            ErrorCategory::Cancelled => (ExecutionStatus::Cancelled, EffectState::Unknown),
            _ => (ExecutionStatus::Failed, EffectState::Unknown),
        };
        ToolOutcome::new(
            status,
            effect,
            Evidence::Uncertain,
            match error.category() {
                ErrorCategory::PermissionDenied => "path escapes the tool root",
                ErrorCategory::Cancelled => "tool call was cancelled",
                ErrorCategory::InvalidInput => "tool input is invalid",
                _ => "file cannot be written",
            },
            false,
        )
        .expect("static safe failure builds")
    }

    /// Maps a jail refusal to a denial: never executed, never started.
    fn denied() -> ToolOutcome {
        ToolOutcome::new(
            ExecutionStatus::Denied,
            EffectState::NotStarted,
            Evidence::HostObserved,
            "path escapes the tool root",
            false,
        )
        .expect("static safe denial builds")
    }
}

impl ToolPort for ScopedWriter {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn execute(&self, call: &ToolCall, context: &ToolContext) -> ToolOutcome {
        if context.is_cancelled() {
            return Self::failed(tool_error(
                ErrorCategory::Cancelled,
                "tool call was cancelled",
            ));
        }
        let (parent, name, content) = match self.resolve(call.args().as_str()) {
            Ok(resolved) => resolved,
            Err(error) if error.category() == ErrorCategory::PermissionDenied => {
                return Self::denied();
            }
            Err(error) => return Self::failed(error),
        };
        let written = match self.write_scoped(&parent, &name, &content, context) {
            Ok(written) => written,
            Err(error) if error.category() == ErrorCategory::PermissionDenied => {
                return Self::denied();
            }
            Err(error) => return Self::failed(error),
        };
        match ToolOutcome::from_bounded_content(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            format!("wrote {written} bytes"),
            false,
        ) {
            Ok(outcome) => match outcome.enforce_budget(context.output_budget_bytes()) {
                Ok(bounded) => bounded,
                Err(_) => Self::failed(tool_error(
                    ErrorCategory::Internal,
                    "tool output could not be bounded",
                )),
            },
            Err(_) => Self::failed(tool_error(
                ErrorCategory::Internal,
                "tool output could not be bounded",
            )),
        }
    }
}
