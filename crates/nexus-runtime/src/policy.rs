//! Approval policy for the M0 single-run loop.
//!
//! Only host-authorized scoped reads/searches proceed automatically; every
//! model-directed mutation and every command execution requires an explicit
//! grant. Automatic execution is fail-closed: [`Policy::authorize`] accepts
//! only a canonical logical project-relative `path` for the exact M0
//! revision, and an invalid policy (wrong revision, or an auto-approved set
//! beyond `host_read`/`host_search`) is rejected by [`Policy::validate`]
//! before it can authorize or scope anything.
//!
//! Argument text is parsed with the strict workspace validator
//! ([`nexus_validation::parse_object`]), so duplicate keys and depth, node,
//! or byte budget exhaustion are rejected even though [`nexus_core::NormalizedArgs`]
//! itself only checks object-root shape. Ambiguous JSON never reaches a
//! policy decision.
//!
//! Scope and preview text is bounded, control-escaped, and conservatively
//! redacted. An action whose complete redacted preview or resource label
//! exceeds its bound is refused with a static diagnostic: truncated text is
//! never offered for approval, because a hidden suffix could change the
//! target or command and make two distinct operations indistinguishable.
//! A refused action is denied before dispatch (not started), never silently
//! auto-executed. Redaction and marker checks are best-effort only; callers
//! must still pre-redact at the source, and a returned preview is not a
//! secret-free guarantee.
//!
//! This module makes logical-path decisions only: it performs no filesystem
//! access and claims no symlink, nonexistent-target, or check/use-race
//! protection; concrete adapters must re-validate real paths before touching
//! a filesystem.

use nexus_core::approval::MAX_SCOPE_BYTES;
use nexus_core::commands::MAX_SUMMARY_BYTES;
use nexus_core::{
    AgentError, ApprovedScope, ErrorCategory, Limits, M0_REVISION, RetryGuidance, ToolCall, ToolId,
};
use nexus_validation::parse_object;
use nexus_validation::serde_json::{self, Map, Value};

/// The only tool names the M0 policy may auto-approve.
const AUTO_READ_TOOLS: &[&str] = &["host_read", "host_search"];

/// Placeholder replacing a redacted JSON value.
const REDACTED_VALUE: &str = "[redacted]";
/// Placeholder replacing a redacted JSON object key.
const REDACTED_KEY: &str = "[redacted-key]";

/// Case-insensitive key substrings treated as secret-bearing. Redaction is
/// deliberately conservative: over-redacting a harmless field is acceptable,
/// under-redacting a credential is not.
const SECRET_KEY_MARKERS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "credential",
    "auth",
    "bearer",
    "cookie",
    "private",
    "apikey",
    "api_key",
    "session",
    "key",
];

/// Best-effort value markers mirroring the core error net. A match rejects
/// the preview instead of displaying it; a non-match does not prove the text
/// is secret-free.
const SECRET_VALUE_MARKERS: &[&str] = &[
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

/// Policy boundary: which tools need an explicit approval grant.
#[derive(Debug, Clone)]
pub struct Policy {
    auto_tools: Vec<String>,
    revision: u32,
}

impl Policy {
    /// M0-test policy: `host_read`/`host_search` are automatic, everything
    /// else requires confirmation. Revision is [`M0_REVISION`].
    #[must_use]
    pub fn m0_test() -> Self {
        Self::new(
            AUTO_READ_TOOLS
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            M0_REVISION,
        )
    }

    /// Builds a policy with an explicit auto-approved tool-name set.
    ///
    /// Compatibility constructor: it performs no validation. Use
    /// [`Self::try_new`] or call [`Self::validate`] before relying on the
    /// result; [`Self::authorize`] and [`Self::approval_scope`] themselves
    /// fail closed on an invalid policy.
    #[must_use]
    pub fn new(auto_tools: Vec<String>, revision: u32) -> Self {
        Self {
            auto_tools,
            revision,
        }
    }

    /// Builds a validated policy, rejecting anything but the exact M0
    /// revision and the automatic read/search set.
    pub fn try_new(auto_tools: Vec<String>, revision: u32) -> Result<Self, AgentError> {
        let policy = Self::new(auto_tools, revision);
        policy.validate()?;
        Ok(policy)
    }

    /// Validates the policy boundary: exact [`M0_REVISION`], and only
    /// `host_read`/`host_search` may be automatic. An invalid policy fails
    /// closed in [`Self::authorize`] and [`Self::approval_scope`], so a
    /// custom set that names `host_write`/`host_exec` can never widen
    /// automatic execution. Diagnostics are static and never interpolate
    /// tool names or arguments.
    pub fn validate(&self) -> Result<(), AgentError> {
        if self.revision != M0_REVISION {
            return Err(policy_error(
                ErrorCategory::InvalidInput,
                "policy revision is not the M0 revision",
            ));
        }
        if self.auto_tools.iter().any(|name| !is_auto_read_tool(name)) {
            return Err(policy_error(
                ErrorCategory::InvalidInput,
                "policy auto-approved tool set is invalid",
            ));
        }
        Ok(())
    }

    /// Returns true when the tool requires an approval grant before dispatch.
    ///
    /// This is the admission-time classification only; it is not the
    /// authorization boundary. Automatic calls must still pass
    /// [`Self::authorize`] immediately before dispatch.
    #[must_use]
    pub fn requires_approval(&self, tool: &ToolId) -> bool {
        !self.auto_tools.iter().any(|name| name == tool.name())
    }

    /// Returns the policy revision bound into approval grants.
    #[must_use]
    pub fn revision(&self) -> u32 {
        self.revision
    }

    /// Authorizes one automatic scoped read/search and returns its exact
    /// approved resource scope.
    ///
    /// Fail-closed sequence:
    /// 1. the policy itself must validate (exact M0 revision, only
    ///    `host_read`/`host_search` automatic);
    /// 2. the call's tool revision must equal the policy revision;
    /// 3. the tool must be automatic;
    /// 4. arguments must pass the strict JSON parse (duplicate keys and
    ///    depth/node/byte budgets rejected) and be exactly
    ///    `{"path": <string>}` with a canonical logical project-relative
    ///    path.
    ///
    /// A refusal returns a static diagnostic and never interpolates the
    /// rejected arguments. The returned scope is `read:<path>` or
    /// `search:<path>`. No filesystem or symlink claim is made.
    pub fn authorize(&self, call: &ToolCall) -> Result<ApprovedScope, AgentError> {
        self.validate()?;
        if call.tool().revision() != self.revision {
            return Err(policy_error(
                ErrorCategory::PermissionDenied,
                "tool revision does not match the policy revision",
            ));
        }
        let name = call.tool().name();
        if self.requires_approval(call.tool()) || !is_auto_read_tool(name) {
            return Err(policy_error(
                ErrorCategory::PermissionDenied,
                "tool is not authorized for automatic execution",
            ));
        }
        let args = parse_object_args(call)?;
        if args.len() != 1 {
            return Err(policy_error(
                ErrorCategory::PermissionDenied,
                "automatic tool arguments are not a canonical path",
            ));
        }
        let Some(Value::String(raw_path)) = args.get("path") else {
            return Err(policy_error(
                ErrorCategory::PermissionDenied,
                "automatic tool arguments are not a canonical path",
            ));
        };
        let path = normalize_project_path(raw_path)?;
        let prefix = if name == "host_search" {
            "search:"
        } else {
            "read:"
        };
        bounded_scope(&format!("{prefix}{path}"))
    }

    /// Resolves the exact bounded action resource for a call that requires
    /// confirmation:
    /// - `path:<canonical path>` when the arguments carry a logical
    ///   project-relative `path` string;
    /// - `command:<escaped command text>` when they carry a non-empty
    ///   `command` string or `argv` string/string-array;
    /// - `tool:<name>` otherwise.
    ///
    /// Commands always require confirmation; this only describes the
    /// resource. The returned scope never authorizes execution by itself,
    /// and no filesystem or symlink claim is made. A command label that
    /// cannot fit the scope bound whole is refused, never truncated: an
    /// approval prompt must not make two different commands look identical.
    pub fn approval_scope(&self, call: &ToolCall) -> Result<ApprovedScope, AgentError> {
        self.validate()?;
        let args = parse_object_args(call)?;
        if let Some(value) = args.get("path") {
            let Value::String(raw_path) = value else {
                return Err(policy_error(
                    ErrorCategory::PermissionDenied,
                    "tool path argument is invalid",
                ));
            };
            let path = normalize_project_path(raw_path)?;
            return bounded_scope(&format!("path:{path}"));
        }
        if let Some(command) = command_argument(&args)? {
            let escaped = escape_controls(&command);
            reject_secret_text(&escaped)?;
            return bounded_scope(&format!("command:{escaped}"));
        }
        bounded_scope(&format!("tool:{}", call.tool().name()))
    }

    /// Builds the complete, bounded, control-escaped, conservatively
    /// redacted preview of the exact immutable arguments for an approval
    /// notice: the tool name followed by a compact JSON object.
    ///
    /// Values under secret-suspect keys are replaced with `[redacted]` (and
    /// the key name with `[redacted-key]`); any remaining value matching the
    /// best-effort secret marker net is rejected instead of displayed. If
    /// the complete preview cannot fit [`MAX_SUMMARY_BYTES`] it is refused
    /// with a static diagnostic, never truncated. Redaction and the marker
    /// net are best-effort only: callers must pre-redact at the source, and
    /// a returned preview is not a secret-free guarantee. A preview never
    /// authorizes execution.
    pub fn approval_preview(&self, call: &ToolCall) -> Result<String, AgentError> {
        self.validate()?;
        let args = parse_object_args(call)?;
        let json = serde_json::to_string(&Value::Object(redact_object(&args))).map_err(|_| {
            policy_error(
                ErrorCategory::Internal,
                "tool arguments could not be rendered",
            )
        })?;
        let escaped = escape_controls(&json);
        reject_secret_text(&escaped)?;
        let name = call.tool().name();
        let preview = format!("{name} {escaped}");
        if preview.len() > MAX_SUMMARY_BYTES {
            return Err(policy_error(
                ErrorCategory::ResourceLimit,
                "approval preview exceeds its bound",
            ));
        }
        Ok(preview)
    }
}

/// Parses the immutable argument text as a JSON object with the strict
/// workspace validator, which rejects duplicate keys and enforces depth,
/// node, and byte budgets. Diagnostics are static; the rejected text is
/// never carried.
fn parse_object_args(call: &ToolCall) -> Result<Map<String, Value>, AgentError> {
    let value = parse_object(call.args().as_str(), Limits::M0_TEST_ARG_ASSEMBLY_BYTES)?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(policy_error(
            ErrorCategory::InvalidInput,
            "tool arguments are not a JSON object",
        )),
    }
}

/// Validates and returns a canonical logical project-relative path.
///
/// Logical scope only: no filesystem access, no symlink resolution, and no
/// check/use-race guarantee. Rejects empty paths, absolute paths, `~`
/// shorthand, parent traversal and empty/`.` components, backslash
/// separators or drives, Windows-invalid characters, and control
/// characters.
fn normalize_project_path(raw: &str) -> Result<String, AgentError> {
    if raw.is_empty() {
        return Err(policy_error(
            ErrorCategory::PermissionDenied,
            "tool path is empty",
        ));
    }
    if raw.starts_with('/') {
        return Err(policy_error(
            ErrorCategory::PermissionDenied,
            "tool path is absolute",
        ));
    }
    if raw.starts_with('~') {
        return Err(policy_error(
            ErrorCategory::PermissionDenied,
            "tool path is outside the project scope",
        ));
    }
    if raw.contains('\\') {
        return Err(policy_error(
            ErrorCategory::PermissionDenied,
            "tool path uses backslash separators",
        ));
    }
    for ch in raw.chars() {
        if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
            return Err(policy_error(
                ErrorCategory::PermissionDenied,
                "tool path contains invalid characters",
            ));
        }
    }
    for component in raw.split('/') {
        match component {
            "" => {
                return Err(policy_error(
                    ErrorCategory::PermissionDenied,
                    "tool path contains an empty component",
                ));
            }
            "." | ".." => {
                return Err(policy_error(
                    ErrorCategory::PermissionDenied,
                    "tool path contains a parent traversal",
                ));
            }
            _ => {}
        }
    }
    Ok(raw.to_owned())
}

/// Returns the command text from `command`/`argv`, or `None` when neither is
/// present. A present but malformed or empty value is an explicit refusal.
fn command_argument(args: &Map<String, Value>) -> Result<Option<String>, AgentError> {
    for key in ["command", "argv"] {
        let Some(value) = args.get(key) else {
            continue;
        };
        return match value {
            Value::String(text) if !text.is_empty() => Ok(Some(text.clone())),
            Value::Array(items) if !items.is_empty() => {
                let mut parts = Vec::with_capacity(items.len());
                for item in items {
                    let Value::String(part) = item else {
                        return Err(policy_error(
                            ErrorCategory::PermissionDenied,
                            "tool command argument is invalid",
                        ));
                    };
                    parts.push(part.as_str());
                }
                Ok(Some(parts.join(" ")))
            }
            _ => Err(policy_error(
                ErrorCategory::PermissionDenied,
                "tool command argument is invalid",
            )),
        };
    }
    Ok(None)
}

/// Redacts secret-suspect object members and recurses into everything else.
fn redact_object(args: &Map<String, Value>) -> Map<String, Value> {
    args.iter()
        .map(|(key, value)| {
            if is_secret_key(key) {
                (
                    REDACTED_KEY.to_owned(),
                    Value::String(REDACTED_VALUE.to_owned()),
                )
            } else {
                (key.clone(), redact_value(value))
            }
        })
        .collect()
}

/// Recursively redacts secret-suspect object members inside a value. Arrays
/// carry no key context and recurse element-wise.
fn redact_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(redact_object(map)),
        Value::Array(items) => Value::Array(items.iter().map(redact_value).collect()),
        other => other.clone(),
    }
}

/// Conservative key-name check; any substring match marks the member
/// secret-bearing.
fn is_secret_key(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    SECRET_KEY_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
}

/// Rejects text carrying best-effort secret markers. Callers must not treat a
/// pass as proof of secret-freedom.
fn reject_secret_text(text: &str) -> Result<(), AgentError> {
    let lowered = text.to_ascii_lowercase();
    if SECRET_VALUE_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
    {
        return Err(policy_error(
            ErrorCategory::PermissionDenied,
            "tool arguments may contain secret material",
        ));
    }
    Ok(())
}

/// Escapes every control character (including `DEL` and C1 controls that
/// `serde_json` leaves raw) as `\uXXXX`, so returned text never contains raw
/// control bytes.
fn escape_controls(text: &str) -> String {
    if !text.chars().any(char::is_control) {
        return text.to_owned();
    }
    use std::fmt::Write as _;
    let mut escaped = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        if ch.is_control() {
            let _ = write!(escaped, "\\u{:04x}", ch as u32);
        } else {
            escaped.push(ch);
        }
    }
    escaped
}

/// Builds a bounded scope. An over-long resource is refused with a static
/// diagnostic; the rejected text is never truncated or echoed.
fn bounded_scope(text: &str) -> Result<ApprovedScope, AgentError> {
    if text.len() > MAX_SCOPE_BYTES {
        return Err(policy_error(
            ErrorCategory::ResourceLimit,
            "tool resource scope exceeds its bound",
        ));
    }
    ApprovedScope::new(text).map_err(|_| {
        policy_error(
            ErrorCategory::Internal,
            "tool resource scope could not be built",
        )
    })
}

/// Returns true only for the two M0 automatic read/search tools.
fn is_auto_read_tool(name: &str) -> bool {
    AUTO_READ_TOOLS.contains(&name)
}

/// Builds a static, marker-safe policy diagnostic.
fn policy_error(category: ErrorCategory, message: &'static str) -> AgentError {
    AgentError::new(category, message, RetryGuidance::DoNotRetry)
        .expect("static safe policy message builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::{CallId, NormalizedArgs, RunId, TurnId};
    use nexus_validation::{MAX_ARGS_DEPTH, MAX_ARGS_NODES};

    fn call(tool: &str, revision: u32, args: &str) -> ToolCall {
        ToolCall::new(
            RunId::new("run-1").expect("valid"),
            TurnId::new("turn-1").expect("valid"),
            CallId::new("call-1").expect("valid"),
            ToolId::new(tool, revision).expect("valid"),
            NormalizedArgs::new(args).expect("valid args build"),
        )
    }

    #[test]
    fn scoped_reads_are_automatic_mutations_require_confirmation() {
        let policy = Policy::m0_test();
        let read = ToolId::new("host_read", M0_REVISION).expect("valid");
        let search = ToolId::new("host_search", M0_REVISION).expect("valid");
        let write = ToolId::new("host_write", M0_REVISION).expect("valid");
        let exec = ToolId::new("host_exec", M0_REVISION).expect("valid");
        assert!(!policy.requires_approval(&read));
        assert!(!policy.requires_approval(&search));
        assert!(policy.requires_approval(&write));
        assert!(policy.requires_approval(&exec));
        assert_eq!(policy.revision(), M0_REVISION);
    }

    #[test]
    fn policy_validation_rejects_wrong_revision_and_forbidden_auto_tools() {
        assert!(Policy::m0_test().validate().is_ok());
        assert!(
            Policy::try_new(
                vec!["host_read".to_owned(), "host_search".to_owned()],
                M0_REVISION
            )
            .is_ok()
        );
        assert!(
            Policy::try_new(Vec::new(), M0_REVISION).is_ok(),
            "an empty automatic set is conservative, not invalid"
        );
        assert!(Policy::try_new(vec!["host_write".to_owned()], M0_REVISION).is_err());
        assert!(Policy::try_new(vec!["host_exec".to_owned()], M0_REVISION).is_err());
        assert!(
            Policy::try_new(
                vec!["host_read".to_owned(), "host_exec".to_owned()],
                M0_REVISION
            )
            .is_err()
        );
        assert!(Policy::try_new(vec!["host_read".to_owned()], M0_REVISION + 1).is_err());
    }

    #[test]
    fn authorize_accepts_canonical_project_reads_and_searches() {
        let policy = Policy::m0_test();
        let read = call("host_read", M0_REVISION, r#"{"path":"src/lib.rs"}"#);
        assert_eq!(
            policy
                .authorize(&read)
                .expect("scoped read authorizes")
                .as_str(),
            "read:src/lib.rs"
        );
        let search = call(
            "host_search",
            M0_REVISION,
            r#"{"path":"crates/nexus-core/src"}"#,
        );
        assert_eq!(
            policy
                .authorize(&search)
                .expect("scoped search authorizes")
                .as_str(),
            "search:crates/nexus-core/src"
        );
    }

    #[test]
    fn authorize_rejects_non_project_paths_and_ambiguous_arguments() {
        let policy = Policy::m0_test();
        for args in [
            r#"{"path":""}"#,
            r#"{"path":"/etc/passwd"}"#,
            r#"{"path":".."}"#,
            r#"{"path":"src/../etc"}"#,
            r#"{"path":"..\\windows"}"#,
            r#"{"path":"C:\\Users"}"#,
            r#"{"path":"\\\\server\\share"}"#,
            r#"{"path":"src\\lib.rs"}"#,
            r#"{"path":"C:/Users"}"#,
            r#"{"path":"~/secrets"}"#,
            r#"{"path":"src//lib.rs"}"#,
            r#"{"path":"./src"}"#,
            r#"{"path":"src/"}"#,
            r#"{"path":"src/lib.rs:stream"}"#,
            r#"{"path":"src/\u0001lib.rs"}"#,
            r#"{"path":5}"#,
            r#"{"path":"src","recursive":true}"#,
            r#"{"path":"/etc/passwd","path":"src"}"#,
            r#"{"path":"src","path":"/etc/passwd"}"#,
            r#"{}"#,
            r#"{oops}"#,
        ] {
            let denied = call("host_read", M0_REVISION, args);
            assert!(
                policy.authorize(&denied).is_err(),
                "must refuse automatic execution for {args}"
            );
        }
    }

    #[test]
    fn duplicate_path_keys_are_rejected_never_last_wins() {
        let policy = Policy::m0_test();
        let out_of_scope_first = call(
            "host_read",
            M0_REVISION,
            r#"{"path":"/etc/passwd","path":"src"}"#,
        );
        let safe_first = call(
            "host_read",
            M0_REVISION,
            r#"{"path":"src","path":"/etc/passwd"}"#,
        );
        for duplicate in [out_of_scope_first, safe_first] {
            let error = policy
                .authorize(&duplicate)
                .expect_err("duplicate path keys are ambiguous");
            assert_eq!(error.category(), ErrorCategory::InvalidInput);
            assert!(
                error.message().contains("duplicate"),
                "the strict parser must reject the duplicate, not apply last-key-wins"
            );
        }

        let scope_duplicate = call(
            "host_write",
            M0_REVISION,
            r#"{"path":"/etc/passwd","path":"src"}"#,
        );
        assert!(policy.approval_scope(&scope_duplicate).is_err());
        let preview_duplicate = call("host_write", M0_REVISION, r#"{"path":"src","path":"src"}"#);
        assert!(policy.approval_preview(&preview_duplicate).is_err());
    }

    #[test]
    fn ambiguous_or_oversized_json_is_rejected_before_policy_decisions() {
        let policy = Policy::m0_test();

        let mut nested = String::new();
        for _ in 0..=MAX_ARGS_DEPTH {
            nested.push('[');
        }
        for _ in 0..=MAX_ARGS_DEPTH {
            nested.push(']');
        }
        let deep = call("host_read", M0_REVISION, &format!(r#"{{"a":{nested}}}"#));
        assert!(policy.authorize(&deep).is_err());

        let items = vec!["0"; MAX_ARGS_NODES].join(",");
        let wide = call("host_read", M0_REVISION, &format!(r#"{{"a":[{items}]}}"#));
        assert!(policy.authorize(&wide).is_err());
    }

    #[test]
    fn authorize_requires_exact_revision_and_an_explicit_auto_set() {
        let policy = Policy::m0_test();
        let wrong_revision = call("host_read", M0_REVISION + 1, r#"{"path":"src"}"#);
        assert!(policy.authorize(&wrong_revision).is_err());

        let closed = Policy::try_new(Vec::new(), M0_REVISION).expect("conservative policy builds");
        let read = call("host_read", M0_REVISION, r#"{"path":"src"}"#);
        assert!(closed.authorize(&read).is_err());
    }

    #[test]
    fn invalid_custom_auto_sets_cannot_bypass_authorization() {
        let write = call("host_write", M0_REVISION, r#"{"path":"dst"}"#);
        let exec = call("host_exec", M0_REVISION, r#"{"argv":"run"}"#);
        let read = call("host_read", M0_REVISION, r#"{"path":"src"}"#);

        let custom_write = Policy::new(vec!["host_write".to_owned()], M0_REVISION);
        // Compatibility view stays name-based; authorize is the boundary.
        assert!(!custom_write.requires_approval(write.tool()));
        assert!(custom_write.authorize(&write).is_err());

        let custom_exec = Policy::new(vec!["host_exec".to_owned()], M0_REVISION);
        assert!(custom_exec.authorize(&exec).is_err());

        let wrong_revision = Policy::new(vec!["host_read".to_owned()], M0_REVISION + 1);
        assert!(wrong_revision.authorize(&read).is_err());

        assert!(Policy::m0_test().authorize(&write).is_err());
        assert!(Policy::m0_test().authorize(&exec).is_err());
    }

    #[test]
    fn approval_scope_resolves_paths_commands_and_tool_fallback() {
        let policy = Policy::m0_test();

        let write = call(
            "host_write",
            M0_REVISION,
            r#"{"path":"dst/file.txt","content":"hi"}"#,
        );
        assert_eq!(
            policy
                .approval_scope(&write)
                .expect("path scope resolves")
                .as_str(),
            "path:dst/file.txt"
        );

        let exec = call("host_exec", M0_REVISION, r#"{"argv":"run --flag"}"#);
        let scope = policy
            .approval_scope(&exec)
            .expect("command scope resolves");
        assert!(scope.as_str().starts_with("command:"));
        assert!(scope.as_str().contains("run --flag"));

        let array_exec = call("host_exec", M0_REVISION, r#"{"argv":["git","status"]}"#);
        assert_eq!(
            policy
                .approval_scope(&array_exec)
                .expect("argv scope resolves")
                .as_str(),
            "command:git status"
        );

        let other = call("host_write", M0_REVISION, r#"{"content":"hi"}"#);
        assert_eq!(
            policy
                .approval_scope(&other)
                .expect("fallback scope resolves")
                .as_str(),
            "tool:host_write"
        );

        let traversal = call("host_write", M0_REVISION, r#"{"path":"../outside"}"#);
        assert!(
            policy.approval_scope(&traversal).is_err(),
            "a traversal path cannot be scoped exactly"
        );

        let malformed = call("host_exec", M0_REVISION, r#"{"argv":[]}"#);
        assert!(policy.approval_scope(&malformed).is_err());
    }

    #[test]
    fn approval_scope_rejects_secret_command_text() {
        let policy = Policy::m0_test();
        let secret = call("host_exec", M0_REVISION, r#"{"argv":"echo sk-live-0000"}"#);
        assert!(
            policy.approval_scope(&secret).is_err(),
            "secret-bearing command text is refused, never displayed"
        );
    }

    #[test]
    fn action_bounds_are_exact_and_refusals_are_static() {
        let policy = Policy::m0_test();

        let prefix = "command:";
        let exact = "q".repeat(MAX_SCOPE_BYTES - prefix.len());
        let accepted = call(
            "host_exec",
            M0_REVISION,
            &format!(r#"{{"argv":"{exact}"}}"#),
        );
        let scope = policy
            .approval_scope(&accepted)
            .expect("an exactly bound scope is accepted");
        assert_eq!(scope.as_str().len(), MAX_SCOPE_BYTES);
        assert!(scope.as_str().starts_with(prefix));

        let over = "q".repeat(MAX_SCOPE_BYTES - prefix.len() + 1);
        let refused = call("host_exec", M0_REVISION, &format!(r#"{{"argv":"{over}"}}"#));
        let error = policy
            .approval_scope(&refused)
            .expect_err("an over-bound scope is refused");
        assert_eq!(error.message(), "tool resource scope exceeds its bound");
        assert!(!error.message().contains('q'), "refusals never echo input");

        // Preview boundary: "host_exec " (10) plus `{"argv":"<n>"}` (n + 11)
        // equals 1024 at n = 1003.
        let fixed = "host_exec ".len() + r#"{"argv":""}"#.len();
        let exact_n = MAX_SUMMARY_BYTES - fixed;
        let exact_args = format!(r#"{{"argv":"{}"}}"#, "q".repeat(exact_n));
        let accepted = call("host_exec", M0_REVISION, &exact_args);
        let preview = policy
            .approval_preview(&accepted)
            .expect("an exactly bound preview is accepted");
        assert_eq!(preview.len(), MAX_SUMMARY_BYTES);

        let over_args = format!(r#"{{"argv":"{}"}}"#, "q".repeat(exact_n + 1));
        let refused = call("host_exec", M0_REVISION, &over_args);
        let error = policy
            .approval_preview(&refused)
            .expect_err("an over-bound preview is refused");
        assert_eq!(error.message(), "approval preview exceeds its bound");
        assert!(!error.message().contains('q'), "refusals never echo input");
    }

    #[test]
    fn oversized_actions_are_refused_not_truncated_into_ambiguous_labels() {
        let policy = Policy::m0_test();

        // Two commands share their whole visible prefix and differ only past
        // the scope bound. Both must be refused; a truncated label would
        // have made them indistinguishable and approvable.
        let shared = "q".repeat(MAX_SCOPE_BYTES);
        let first = call(
            "host_exec",
            M0_REVISION,
            &format!(r#"{{"argv":"{shared}A"}}"#),
        );
        let second = call(
            "host_exec",
            M0_REVISION,
            &format!(r#"{{"argv":"{shared}B"}}"#),
        );
        let first_error = policy
            .approval_scope(&first)
            .expect_err("oversized command is refused");
        let second_error = policy
            .approval_scope(&second)
            .expect_err("oversized command is refused");
        assert_eq!(first_error.message(), second_error.message());
        assert!(!first_error.message().contains('q'));

        // Same for argument previews that differ only past the preview bound.
        let shared = "q".repeat(MAX_SUMMARY_BYTES);
        let first = call(
            "host_write",
            M0_REVISION,
            &format!(r#"{{"content":"{shared}A"}}"#),
        );
        let second = call(
            "host_write",
            M0_REVISION,
            &format!(r#"{{"content":"{shared}B"}}"#),
        );
        let first_error = policy
            .approval_preview(&first)
            .expect_err("oversized preview is refused");
        let second_error = policy
            .approval_preview(&second)
            .expect_err("oversized preview is refused");
        assert_eq!(first_error.message(), second_error.message());
        assert!(!first_error.message().contains('q'));
    }

    #[test]
    fn approval_preview_is_escaped_and_redacted() {
        let policy = Policy::m0_test();

        let write = call(
            "host_write",
            M0_REVISION,
            r#"{"path":"dst","password":"hunter2","token":"abc","mode":"w"}"#,
        );
        let preview = policy.approval_preview(&write).expect("preview builds");
        assert!(preview.starts_with("host_write "));
        assert!(preview.contains(REDACTED_VALUE));
        assert!(preview.contains(REDACTED_KEY));
        assert!(!preview.contains("hunter2"));
        assert!(!preview.contains("abc"));
        assert!(preview.contains("dst"));
        assert!(preview.contains("mode"));
        assert!(preview.len() <= MAX_SUMMARY_BYTES);
        assert!(!preview.chars().any(char::is_control));

        let nested = call(
            "host_write",
            M0_REVISION,
            r#"{"headers":{"authorization":"Bearer abc123"},"path":"dst"}"#,
        );
        let preview = policy
            .approval_preview(&nested)
            .expect("nested redaction builds");
        assert!(preview.contains(REDACTED_VALUE));
        assert!(!preview.contains("abc123"));

        let control = call("host_read", M0_REVISION, r#"{"path":"src/\u0001lib.rs"}"#);
        let preview = policy.approval_preview(&control).expect("preview builds");
        assert!(preview.contains("\\u0001"));
        assert!(!preview.chars().any(char::is_control));

        let leak = call("host_read", M0_REVISION, r#"{"note":"sk-live-0000"}"#);
        assert!(
            policy.approval_preview(&leak).is_err(),
            "secret-looking values are refused, not displayed"
        );
    }

    #[test]
    fn approval_helpers_fail_closed_on_invalid_policy_and_arguments() {
        let policy = Policy::m0_test();
        let malformed = call("host_write", M0_REVISION, r#"{oops}"#);
        assert!(policy.approval_scope(&malformed).is_err());
        assert!(policy.approval_preview(&malformed).is_err());

        let invalid = Policy::new(vec!["host_write".to_owned()], M0_REVISION);
        let write = call("host_write", M0_REVISION, r#"{"path":"dst"}"#);
        assert!(invalid.approval_scope(&write).is_err());
        assert!(invalid.approval_preview(&write).is_err());
    }
}
