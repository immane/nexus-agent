//! Root-jailed file tools (`host_read` and `host_write` at the M0 revision).

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use nexus_core::{
    AgentError, EffectState, ErrorCategory, Evidence, ExecutionStatus, Limits, M0_REVISION,
    RetryGuidance, ToolCall, ToolContext, ToolId, ToolOutcome, ToolPort, ToolSpec,
};

/// Tool name. Matches the policy auto-read set, so scoped reads stay
/// automatic under the same authorization as the fake reader.
pub const HOST_READ_TOOL: &str = "host_read";

/// Closed input schema: exactly one `path` string, nothing else.
pub const READ_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}"#;

/// Read chunk size in bytes. Cancellation is observed between chunks.
const READ_CHUNK_BYTES: usize = 65_536;

fn tool_error(category: ErrorCategory, message: &'static str) -> AgentError {
    AgentError::new(category, message, RetryGuidance::DoNotRetry)
        .expect("static safe tool message builds")
}

/// Read-only file executor jailed to one canonical root. Constructed with
/// [`ScopedReader::with_root`]; the root defaults to the process working
/// directory via [`ScopedReader::with_current_dir`].
pub struct ScopedReader {
    root: PathBuf,
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
            "Read a file inside the tool root",
            READ_SCHEMA,
        )
        .map_err(|_| tool_error(ErrorCategory::Internal, "tool description is invalid"))?;
        Ok(Self { root, spec })
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
        context: &ToolContext,
    ) -> Result<(String, bool), AgentError> {
        if context.is_cancelled() {
            return Err(tool_error(
                ErrorCategory::Cancelled,
                "tool call was cancelled",
            ));
        }
        let file = File::open(path)
            .map_err(|_| tool_error(ErrorCategory::ToolFailure, "file cannot be read"))?;
        if !file
            .metadata()
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
        {
            return Err(tool_error(
                ErrorCategory::ToolFailure,
                "file cannot be read",
            ));
        }
        // One byte past the global cap is enough: anything longer is cut
        // with the truncation flag below, so memory stays bounded.
        let mut limited = file.take(Limits::M0_TEST_TOOL_OUTPUT_BYTES as u64 + 1);
        let mut bytes = Vec::new();
        let mut chunk = [0u8; READ_CHUNK_BYTES];
        loop {
            if context.is_cancelled() {
                return Err(tool_error(
                    ErrorCategory::Cancelled,
                    "tool call was cancelled",
                ));
            }
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
        let truncated = bytes.len() > Limits::M0_TEST_TOOL_OUTPUT_BYTES;
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(error) if truncated && error.utf8_error().error_len().is_none() => {
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
        Ok((text, truncated))
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
        let (text, truncated) = match self.read_scoped(&path, context) {
            Ok(text) => text,
            Err(error) => return Self::failed(error),
        };
        match ToolOutcome::from_bounded_content(
            ExecutionStatus::Succeeded,
            EffectState::KnownNotApplied,
            Evidence::HostObserved,
            text,
            truncated,
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
    spec: ToolSpec,
}

impl ScopedWriter {
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
            ToolId::new(HOST_WRITE_TOOL, M0_REVISION).expect("static tool identity builds"),
            "Write a file inside the tool root, creating it when missing and replacing its whole content otherwise. Parent directories must already exist. Takes exactly {\"path\": \"notes/todo.txt\", \"content\": \"...\"}.",
            WRITE_SCHEMA,
        )
        .map_err(|_| tool_error(ErrorCategory::Internal, "tool description is invalid"))?;
        Ok(Self { root, spec })
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
        let name = joined
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty() && !name.contains('\0'))
            .ok_or_else(|| tool_error(ErrorCategory::InvalidInput, "tool path is invalid"))?
            .to_owned();
        let parent = joined
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
        Ok((canonical, name, content.to_owned()))
    }

    /// Writes the content, observing cancellation before touching the
    /// filesystem. New files are created with `create_new` so a raced
    /// symlink cannot redirect the create; overwrites re-validate the
    /// resolved target stays inside the jail and refuses directories.
    /// Same documented residual as the reader: a path swapped between the
    /// final check and the open could escape, so `openat2` remains the
    /// hard-guarantee upgrade path.
    fn write_scoped(
        &self,
        parent: &Path,
        name: &str,
        content: &str,
        context: &ToolContext,
    ) -> Result<usize, AgentError> {
        if context.is_cancelled() {
            return Err(tool_error(
                ErrorCategory::Cancelled,
                "tool call was cancelled",
            ));
        }
        let target = parent.join(name);
        let exists = std::fs::symlink_metadata(&target).is_ok();
        if exists {
            let canonical = std::fs::canonicalize(&target)
                .map_err(|_| tool_error(ErrorCategory::ToolFailure, "file cannot be written"))?;
            if !canonical.starts_with(&self.root) {
                return Err(tool_error(
                    ErrorCategory::PermissionDenied,
                    "path escapes the tool root",
                ));
            }
            if !canonical.is_file() {
                return Err(tool_error(
                    ErrorCategory::ToolFailure,
                    "file cannot be written",
                ));
            }
            let mut file = OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&target)
                .map_err(|_| tool_error(ErrorCategory::ToolFailure, "file cannot be written"))?;
            file.write_all(content.as_bytes())
                .map_err(|_| tool_error(ErrorCategory::ToolFailure, "file cannot be written"))?;
        } else {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .map_err(|_| tool_error(ErrorCategory::ToolFailure, "file cannot be written"))?;
            file.write_all(content.as_bytes())
                .map_err(|_| tool_error(ErrorCategory::ToolFailure, "file cannot be written"))?;
        }
        Ok(content.len())
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
