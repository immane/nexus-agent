//! Approval binding (lock section 10).
//!
//! An approval binds the exact tuple of run, call, tool identity plus
//! revision, normalized immutable arguments, approved scope, expiry, and
//! policy revision, and is checked immediately before dispatch. Stream
//! fragments and previews never authorize; mutable authorization,
//! cancellation, and deadlines are rechecked at dispatch by the runtime (P3),
//! which this struct carries everything needed for.

use std::time::Duration;

use crate::error::{AgentError, ErrorCategory, RetryGuidance};
use crate::ids::{ApprovalId, CallId, RunId, ToolId};
use crate::limits::Limits;

/// Maximum approved-scope description length in bytes (M0-TEST
/// representation choice, not a product default).
pub const MAX_SCOPE_BYTES: usize = 1024;

/// Immutable object-shaped tool-argument text.
///
/// Lightweight object-root check (trimmed text starts with `{` and ends
/// with `}`) within the assembly budget. The closed minimal-validator
/// keyword subset (lock section 4), JSON parsing, and canonicalization are
/// enforced by `nexus-validation` at the runtime admission boundary, not by
/// this carrier constructor. Constructing this value alone never authorizes
/// execution or establishes schema validity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedArgs(String);

impl NormalizedArgs {
    /// Validates object-root shape and the assembly bound.
    pub fn new(raw: impl Into<String>) -> Result<Self, AgentError> {
        let value = raw.into();
        if value.is_empty() {
            return Err(input_error("tool arguments are empty"));
        }
        if value.len() > Limits::M0_TEST_ARG_ASSEMBLY_BYTES {
            return Err(input_error("tool arguments exceed assembly budget"));
        }
        let trimmed = value.trim();
        if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
            return Err(input_error("tool arguments must be an object-root value"));
        }
        Ok(Self(value))
    }

    /// Returns the carried argument text; admission supplies canonical JSON.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Approved resource scope carried into dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedScope(String);

impl ApprovedScope {
    /// Validates the scope bound. Empty means no resource beyond the call itself.
    pub fn new(raw: impl Into<String>) -> Result<Self, AgentError> {
        let value = raw.into();
        if value.len() > MAX_SCOPE_BYTES {
            return Err(input_error("approved scope exceeds its bound"));
        }
        Ok(Self(value))
    }

    /// Returns the scope text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Exact-tuple approval grant. Immutable after construction; the runtime
/// rechecks mutable authorization, cancellation, and deadlines at dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalBinding {
    approval: ApprovalId,
    run: RunId,
    call: CallId,
    tool: ToolId,
    args: NormalizedArgs,
    scope: ApprovedScope,
    expires_at_elapsed: Duration,
    policy_revision: u32,
}

impl ApprovalBinding {
    /// Binds the exact tuple. `expires_at_elapsed` is a monotonic-clock
    /// reading, not wall-clock time.
    /// Eight parameters are intentional: the lock mandates the exact tuple.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        approval: ApprovalId,
        run: RunId,
        call: CallId,
        tool: ToolId,
        args: NormalizedArgs,
        scope: ApprovedScope,
        expires_at_elapsed: Duration,
        policy_revision: u32,
    ) -> Self {
        Self {
            approval,
            run,
            call,
            tool,
            args,
            scope,
            expires_at_elapsed,
            policy_revision,
        }
    }

    /// Returns the grant identity.
    #[must_use]
    pub fn approval(&self) -> &ApprovalId {
        &self.approval
    }

    /// Returns the bound run.
    #[must_use]
    pub fn run(&self) -> &RunId {
        &self.run
    }

    /// Returns the bound call.
    #[must_use]
    pub fn call(&self) -> &CallId {
        &self.call
    }

    /// Returns the bound tool identity plus revision.
    #[must_use]
    pub fn tool(&self) -> &ToolId {
        &self.tool
    }

    /// Returns the bound immutable arguments.
    #[must_use]
    pub fn args(&self) -> &NormalizedArgs {
        &self.args
    }

    /// Returns the approved scope.
    #[must_use]
    pub fn scope(&self) -> &ApprovedScope {
        &self.scope
    }

    /// Returns the policy revision the grant was issued under.
    #[must_use]
    pub fn policy_revision(&self) -> u32 {
        self.policy_revision
    }

    /// Returns the monotonic run-elapsed expiry used by dispatch and notices.
    #[must_use]
    pub fn expires_at_elapsed(&self) -> Duration {
        self.expires_at_elapsed
    }

    /// Returns true once the monotonic `now_elapsed` reaches expiry.
    #[must_use]
    pub fn is_expired(&self, now_elapsed: Duration) -> bool {
        now_elapsed >= self.expires_at_elapsed
    }

    /// Checks the exact tuple, revision equality, and expiry immediately
    /// before dispatch. Any deviation denies execution explicitly.
    pub fn check_valid_for_dispatch(
        &self,
        run: &RunId,
        call: &CallId,
        tool: &ToolId,
        args: &NormalizedArgs,
        policy_revision: u32,
        now_elapsed: Duration,
    ) -> Result<(), AgentError> {
        if self.run != *run {
            return Err(dispatch_denied("approval run mismatch"));
        }
        if self.call != *call {
            return Err(dispatch_denied("approval call mismatch"));
        }
        if self.tool != *tool {
            return Err(dispatch_denied(
                "approval tool identity or revision mismatch",
            ));
        }
        if self.args != *args {
            return Err(dispatch_denied("approval arguments changed"));
        }
        if self.policy_revision != policy_revision {
            return Err(dispatch_denied("approval policy revision mismatch"));
        }
        if self.is_expired(now_elapsed) {
            return Err(dispatch_denied("approval expired"));
        }
        Ok(())
    }
}

fn input_error(message: &'static str) -> AgentError {
    AgentError::new(
        ErrorCategory::InvalidInput,
        message,
        RetryGuidance::DoNotRetry,
    )
    .expect("static safe approval message builds")
}

fn dispatch_denied(message: &'static str) -> AgentError {
    AgentError::new(
        ErrorCategory::PermissionDenied,
        message,
        RetryGuidance::DoNotRetry,
    )
    .expect("static safe approval message builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::M0_REVISION;

    fn binding() -> (ApprovalBinding, NormalizedArgs) {
        let args = NormalizedArgs::new(r#"{"path":"src"}"#).expect("valid args build");
        let grant = ApprovalBinding::new(
            ApprovalId::new("appr-1").expect("valid"),
            RunId::new("run-1").expect("valid"),
            CallId::new("call-1").expect("valid"),
            ToolId::new("host_read", M0_REVISION).expect("valid"),
            args.clone(),
            ApprovedScope::new("project-read").expect("valid"),
            Duration::from_secs(120),
            M0_REVISION,
        );
        (grant, args)
    }

    fn tool(revision: u32) -> ToolId {
        ToolId::new("host_read", revision).expect("valid")
    }

    #[test]
    fn exact_tuple_passes_dispatch_check() {
        let (grant, args) = binding();
        grant
            .check_valid_for_dispatch(
                &RunId::new("run-1").expect("valid"),
                &CallId::new("call-1").expect("valid"),
                &tool(M0_REVISION),
                &args,
                M0_REVISION,
                Duration::from_secs(60),
            )
            .expect("exact tuple authorizes");
    }

    #[test]
    fn changed_arguments_invalidate_approval() {
        let (grant, _) = binding();
        let changed = NormalizedArgs::new(r#"{"path":"other"}"#).expect("valid args build");
        let error = grant
            .check_valid_for_dispatch(
                &RunId::new("run-1").expect("valid"),
                &CallId::new("call-1").expect("valid"),
                &tool(M0_REVISION),
                &changed,
                M0_REVISION,
                Duration::ZERO,
            )
            .unwrap_err();
        assert_eq!(error.category(), ErrorCategory::PermissionDenied);
    }

    #[test]
    fn tool_revision_mismatch_fails_exact_equality() {
        let (grant, args) = binding();
        let error = grant
            .check_valid_for_dispatch(
                &RunId::new("run-1").expect("valid"),
                &CallId::new("call-1").expect("valid"),
                &tool(M0_REVISION + 1),
                &args,
                M0_REVISION,
                Duration::ZERO,
            )
            .unwrap_err();
        assert_eq!(error.category(), ErrorCategory::PermissionDenied);
    }

    #[test]
    fn stale_run_call_policy_or_expiry_are_denied() {
        let (grant, args) = binding();
        let run = RunId::new("run-1").expect("valid");
        let call = CallId::new("call-1").expect("valid");
        assert!(
            grant
                .check_valid_for_dispatch(
                    &RunId::new("run-2").expect("valid"),
                    &call,
                    &tool(M0_REVISION),
                    &args,
                    M0_REVISION,
                    Duration::ZERO,
                )
                .is_err()
        );
        assert!(
            grant
                .check_valid_for_dispatch(
                    &run,
                    &CallId::new("call-2").expect("valid"),
                    &tool(M0_REVISION),
                    &args,
                    M0_REVISION,
                    Duration::ZERO,
                )
                .is_err()
        );
        assert!(
            grant
                .check_valid_for_dispatch(
                    &run,
                    &call,
                    &tool(M0_REVISION),
                    &args,
                    M0_REVISION + 1,
                    Duration::ZERO,
                )
                .is_err()
        );
        assert!(
            grant
                .check_valid_for_dispatch(
                    &run,
                    &call,
                    &tool(M0_REVISION),
                    &args,
                    M0_REVISION,
                    Duration::from_secs(120),
                )
                .is_err()
        );
    }

    #[test]
    fn normalized_args_reject_non_object_and_oversize() {
        assert!(NormalizedArgs::new(r#"[1,2]"#).is_err());
        assert!(NormalizedArgs::new("").is_err());
        assert!(NormalizedArgs::new("x".repeat(Limits::M0_TEST_ARG_ASSEMBLY_BYTES + 1)).is_err());
    }
}
