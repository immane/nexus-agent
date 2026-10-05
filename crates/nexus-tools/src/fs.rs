//! Root-jailed read-only file tool (`host_read` at the M0 revision).

use std::fs::File;
use std::io::Read;
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
    fn read_scoped(&self, path: &Path, context: &ToolContext) -> Result<String, AgentError> {
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
        String::from_utf8(bytes)
            .map_err(|_| tool_error(ErrorCategory::InvalidInput, "file is not valid text"))
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
        let text = match self.read_scoped(&path, context) {
            Ok(text) => text,
            Err(error) => return Self::failed(error),
        };
        match ToolOutcome::from_bounded_content(
            ExecutionStatus::Succeeded,
            EffectState::KnownNotApplied,
            Evidence::HostObserved,
            text,
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
