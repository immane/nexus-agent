//! Tool port: immutable descriptors and single-call execution.
//!
//! Registration is not permission to execute. The runtime controls
//! resolution, validation, approval, dispatch, and outcome recording; a
//! tool must never mutate the conversation, schedule runs, or grant itself
//! capabilities. The trait is synchronous and data-only.

use std::time::{Duration, Instant};

use crate::approval::ApprovedScope;
use crate::content::ToolCall;
use crate::error::{AgentError, ErrorCategory, RetryGuidance};
use crate::execution::CancellationToken;
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

/// Execution context: effective budgets, live cancellation, a monotonic
/// deadline, and the approved resource scope. No unrestricted runtime state
/// and no unrelated credentials cross this boundary.
///
/// [`ToolContext::new`] keeps the legacy fixture shape: a `Duration` elapsed
/// reading and a `bool` cancellation snapshot, neither cooperative.
/// [`ToolContext::with_control`] additively installs a live
/// [`CancellationToken`] and an evaluable [`Instant`] deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolContext {
    output_budget_bytes: usize,
    deadline_elapsed: Duration,
    cancelled: bool,
    scope: ApprovedScope,
    token: Option<CancellationToken>,
    deadline_at: Option<Instant>,
}

impl ToolContext {
    /// Builds the legacy context with a finite output budget (never
    /// infinity). `deadline_elapsed` is a monotonic reading and `cancelled`
    /// is a snapshot observed at dispatch.
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
            token: None,
            deadline_at: None,
        })
    }

    /// Additively installs live cancellation and monotonic deadline control,
    /// replacing any previously installed control. The legacy snapshot
    /// fields stay untouched for fixtures.
    #[must_use]
    pub fn with_control(mut self, token: CancellationToken, deadline: Instant) -> Self {
        self.token = Some(token);
        self.deadline_at = Some(deadline);
        self
    }

    /// Returns the finite output budget in bytes.
    #[must_use]
    pub fn output_budget_bytes(&self) -> usize {
        self.output_budget_bytes
    }

    /// Returns the monotonic elapsed reading from the legacy constructor.
    #[must_use]
    pub fn deadline_elapsed(&self) -> Duration {
        self.deadline_elapsed
    }

    /// Returns the live monotonic deadline, or [`None`] for legacy
    /// constructor snapshots: an elapsed `Duration` carries no evaluable
    /// instant without the run's start reading.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline_at
    }

    /// Returns cancellation observed live from the token, falling back to
    /// the dispatch-time snapshot for legacy fixtures. A `bool` snapshot is
    /// never cooperative.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.token
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
            || self.cancelled
    }

    /// Returns the approved resource scope.
    #[must_use]
    pub fn scope(&self) -> &ApprovedScope {
        &self.scope
    }

    /// Explicit combined dispatch check: cancellation first, then the live
    /// deadline. Cancellation maps to [`ErrorCategory::Cancelled`], an
    /// elapsed deadline to [`ErrorCategory::Timeout`].
    pub fn check_active(&self) -> Result<(), AgentError> {
        if self.is_cancelled() {
            return Err(tool_state_error(
                ErrorCategory::Cancelled,
                "tool execution cancelled",
            ));
        }
        if self
            .deadline_at
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(tool_state_error(
                ErrorCategory::Timeout,
                "tool deadline exceeded",
            ));
        }
        Ok(())
    }

    /// Maps cancellation to an explicit outcome for dispatch, observing the
    /// live token when control was installed.
    pub fn check_not_cancelled(&self) -> Result<(), AgentError> {
        if self.is_cancelled() {
            return Err(tool_state_error(
                ErrorCategory::Cancelled,
                "tool execution cancelled",
            ));
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

fn tool_state_error(category: ErrorCategory, message: &'static str) -> AgentError {
    AgentError::new(category, message, RetryGuidance::DoNotRetry)
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
        assert_eq!(
            context
                .check_active()
                .expect_err("snapshot cancels")
                .category(),
            ErrorCategory::Cancelled
        );
    }

    #[test]
    fn live_control_observes_cancellation_and_evaluable_deadline() {
        let scope = ApprovedScope::new("project-read").expect("valid");
        let token = CancellationToken::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        let context = ToolContext::new(1024, Duration::from_secs(300), false, scope.clone())
            .expect("valid context builds")
            .with_control(token.clone(), deadline);
        assert_eq!(context.deadline(), Some(deadline));
        assert!(!context.is_cancelled());
        assert!(context.check_active().is_ok());

        token.cancel();
        assert!(
            context.is_cancelled(),
            "token is re-read after construction"
        );
        assert_eq!(
            context.check_active().expect_err("live cancel").category(),
            ErrorCategory::Cancelled
        );
        assert!(context.check_not_cancelled().is_err());

        let past = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .expect("test clock has history");
        let expired = ToolContext::new(1024, Duration::from_secs(300), false, scope)
            .expect("valid context builds")
            .with_control(CancellationToken::new(), past);
        assert!(!expired.is_cancelled());
        assert_eq!(
            expired
                .check_active()
                .expect_err("deadline passed")
                .category(),
            ErrorCategory::Timeout
        );
    }

    #[test]
    fn legacy_contexts_have_no_evaluable_deadline() {
        let scope = ApprovedScope::new("project-read").expect("valid");
        let context = ToolContext::new(1024, Duration::from_secs(60), false, scope)
            .expect("valid context builds");
        assert_eq!(context.deadline(), None);
        assert_eq!(context.deadline_elapsed(), Duration::from_secs(60));
    }
}

#[cfg(test)]
mod cov_tool_private {
    use super::*;
    use crate::ids::M0_REVISION;

    fn tool_id(revision: u32) -> ToolId {
        ToolId::new("host_read", revision).expect("valid tool id")
    }

    fn scope() -> ApprovedScope {
        ApprovedScope::new("project-read").expect("valid scope builds")
    }

    #[test]
    fn private_error_helpers_are_typed_and_static() {
        let invalid = tool_error("tool description is invalid");
        assert_eq!(invalid.category(), ErrorCategory::InvalidInput);
        assert_eq!(invalid.retry(), RetryGuidance::DoNotRetry);
        assert_eq!(invalid.message(), "tool description is invalid");
        assert!(invalid.correlation().is_empty());

        for (category, message) in [
            (ErrorCategory::Cancelled, "tool execution cancelled"),
            (ErrorCategory::Timeout, "tool deadline exceeded"),
        ] {
            let error = tool_state_error(category, message);
            assert_eq!(error.category(), category);
            assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
            assert_eq!(error.message(), message);
            assert!(error.correlation().is_empty());
        }
    }

    #[test]
    fn spec_fields_store_exact_bytes_at_the_bounds() {
        let description = "d".repeat(MAX_TOOL_DESCRIPTION_LEN);
        let schema = format!("{{{}}}", "s".repeat(MAX_SCHEMA_BYTES - 2));
        assert_eq!(schema.len(), MAX_SCHEMA_BYTES);
        let spec = ToolSpec::new(tool_id(M0_REVISION), description.clone(), schema.clone())
            .expect("boundary spec builds");
        assert_eq!(spec.description, description);
        assert_eq!(spec.input_schema_json, schema);
        assert_eq!(spec.description.len(), MAX_TOOL_DESCRIPTION_LEN);
        assert_eq!(spec.input_schema_json.len(), MAX_SCHEMA_BYTES);
        assert_eq!(spec.id.revision(), M0_REVISION);

        assert!(
            ToolSpec::new(
                tool_id(M0_REVISION),
                "d".repeat(MAX_TOOL_DESCRIPTION_LEN + 1),
                "{}",
            )
            .is_err(),
            "one byte over the description bound is rejected"
        );
        let over_schema = format!("{{{}}}", "s".repeat(MAX_SCHEMA_BYTES - 1));
        assert_eq!(over_schema.len(), MAX_SCHEMA_BYTES + 1);
        assert!(
            ToolSpec::new(tool_id(M0_REVISION), "d", over_schema).is_err(),
            "one byte over the schema bound is rejected"
        );
    }

    #[test]
    fn with_control_replaces_both_live_controls() {
        let first_token = CancellationToken::new();
        let second_token = CancellationToken::new();
        let first_deadline = Instant::now() + Duration::from_secs(30);
        let second_deadline = Instant::now() + Duration::from_secs(60);
        let context = ToolContext::new(1024, Duration::from_secs(60), false, scope())
            .expect("valid context builds")
            .with_control(first_token.clone(), first_deadline)
            .with_control(second_token.clone(), second_deadline);
        assert_eq!(context.token.as_ref(), Some(&second_token));
        assert_eq!(context.deadline_at, Some(second_deadline));

        first_token.cancel();
        assert!(
            !context.is_cancelled(),
            "the replaced token is no longer observed"
        );
        second_token.cancel();
        assert!(context.is_cancelled());
    }

    #[test]
    fn legacy_constructor_leaves_live_control_absent() {
        let context =
            ToolContext::new(1, Duration::ZERO, false, scope()).expect("valid context builds");
        assert!(context.token.is_none());
        assert!(context.deadline_at.is_none());
        assert_eq!(context.deadline(), None);
        assert_eq!(context.deadline_elapsed(), Duration::ZERO);
        assert!(context.check_active().is_ok());
    }

    #[test]
    fn snapshot_cancellation_survives_control_install() {
        let context = ToolContext::new(1024, Duration::from_secs(60), true, scope())
            .expect("valid context builds")
            .with_control(
                CancellationToken::new(),
                Instant::now() + Duration::from_secs(60),
            );
        assert!(context.cancelled);
        assert!(
            context.is_cancelled(),
            "the dispatch snapshot is never cleared by control install"
        );
        assert!(context.check_not_cancelled().is_err());
    }
}
