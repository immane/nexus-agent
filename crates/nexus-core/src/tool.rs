//! Tool port: immutable descriptors and single-call execution.
//!
//! Registration is not permission to execute. The runtime controls
//! resolution, validation, approval, dispatch, and outcome recording; a
//! tool must never mutate the conversation, schedule runs, or grant itself
//! capabilities. The trait is synchronous and data-only.

use std::time::Duration;

use crate::approval::ApprovedScope;
use crate::content::ToolCall;
use crate::error::{AgentError, ErrorCategory, RetryGuidance};
use crate::ids::ToolId;
use crate::limits::Limits;
use crate::outcomes::ToolOutcome;

/// Maximum tool description length in bytes (M0-TEST choice).
pub const MAX_TOOL_DESCRIPTION_LEN: usize = 1024;
/// Maximum input-schema text in bytes (M0-TEST choice; the lock sets no
/// table value for schema size).
pub const MAX_SCHEMA_BYTES: usize = 65_536;

/// Immutable registered descriptor for the current implementation revision.
/// Schemas are object-root JSON; unsupported validation features fail
/// registration explicitly in the runtime (lock section 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    id: ToolId,
    description: String,
    input_schema_json: String,
}

impl ToolSpec {
    /// Validates description and object-root schema bounds at registration.
    pub fn new(
        id: ToolId,
        description: impl Into<String>,
        input_schema_json: impl Into<String>,
    ) -> Result<Self, AgentError> {
        let description = description.into();
        let input_schema_json = input_schema_json.into();
        if description.is_empty() || description.len() > MAX_TOOL_DESCRIPTION_LEN {
            return Err(tool_error("tool description is invalid"));
        }
        if input_schema_json.is_empty() || input_schema_json.len() > MAX_SCHEMA_BYTES {
            return Err(tool_error("tool input schema is invalid"));
        }
        let trimmed = input_schema_json.trim();
        if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
            return Err(tool_error("tool input schema must be an object-root value"));
        }
        Ok(Self {
            id,
            description,
            input_schema_json,
        })
    }

    /// Returns the namespaced tool identity plus revision.
    #[must_use]
    pub fn id(&self) -> &ToolId {
        &self.id
    }

    /// Returns the description.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Returns the object-root input schema text.
    #[must_use]
    pub fn input_schema_json(&self) -> &str {
        &self.input_schema_json
    }
}

/// Execution context: effective budgets, monotonic deadline, cancellation,
/// and the approved resource scope. No unrestricted runtime state and no
/// unrelated credentials cross this boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolContext {
    output_budget_bytes: usize,
    deadline_elapsed: Duration,
    cancelled: bool,
    scope: ApprovedScope,
}

impl ToolContext {
    /// Builds the context with a finite output budget (never infinity).
    pub fn new(
        output_budget_bytes: usize,
        deadline_elapsed: Duration,
        cancelled: bool,
        scope: ApprovedScope,
    ) -> Result<Self, AgentError> {
        if output_budget_bytes == 0 || output_budget_bytes > Limits::M0_TEST_TOOL_OUTPUT_BYTES {
            return Err(tool_error("tool output budget is invalid"));
        }
        Ok(Self {
            output_budget_bytes,
            deadline_elapsed,
            cancelled,
            scope,
        })
    }

    /// Returns the finite output budget in bytes.
    #[must_use]
    pub fn output_budget_bytes(&self) -> usize {
        self.output_budget_bytes
    }

    /// Returns the monotonic deadline.
    #[must_use]
    pub fn deadline_elapsed(&self) -> Duration {
        self.deadline_elapsed
    }

    /// Returns the cancellation flag observed at dispatch.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Returns the approved resource scope.
    #[must_use]
    pub fn scope(&self) -> &ApprovedScope {
        &self.scope
    }

    /// Maps a set cancellation flag to an explicit outcome for dispatch.
    pub fn check_not_cancelled(&self) -> Result<(), AgentError> {
        if self.cancelled {
            return Err(tool_error("tool execution cancelled"));
        }
        Ok(())
    }
}

/// Tool port. Narrow by design: describe the current revision, then execute
/// one admitted call.
pub trait ToolPort {
    /// Returns the immutable registered descriptor for this revision.
    fn describe(&self) -> ToolSpec;

    /// Executes one admitted call and returns its actual outcome,
    /// optionally after emitting bounded progress through events.
    fn execute(&self, call: &ToolCall, context: &ToolContext) -> ToolOutcome;
}

fn tool_error(message: &'static str) -> AgentError {
    AgentError::new(
        ErrorCategory::InvalidInput,
        message,
        RetryGuidance::DoNotRetry,
    )
    .expect("static safe tool message builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::M0_REVISION;

    fn spec() -> ToolSpec {
        ToolSpec::new(
            ToolId::new("host_read", M0_REVISION).expect("valid"),
            "read files",
            r#"{"type":"object"}"#,
        )
        .expect("valid spec builds")
    }

    #[test]
    fn spec_rejects_non_object_schema() {
        let id = ToolId::new("host_read", M0_REVISION).expect("valid");
        assert!(ToolSpec::new(id.clone(), "read", r#"["array"]"#).is_err());
        assert!(ToolSpec::new(id.clone(), "", r#"{"type":"object"}"#).is_err());
        assert_eq!(spec().id().revision(), M0_REVISION);
    }

    #[test]
    fn context_budget_is_finite_and_cancellation_explicit() {
        let scope = ApprovedScope::new("project-read").expect("valid");
        assert!(ToolContext::new(0, Duration::from_secs(60), false, scope.clone()).is_err());
        let context = ToolContext::new(1024, Duration::from_secs(60), true, scope)
            .expect("valid context builds");
        assert!(context.is_cancelled());
        assert!(context.check_not_cancelled().is_err());
    }
}
