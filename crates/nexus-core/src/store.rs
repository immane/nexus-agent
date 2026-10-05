//! Session-store port and records.
//!
//! The store persists accepted conversation and operation records; it never
//! decides authorization or schedules work. M0 uses in-memory ephemeral
//! storage that self-identifies as non-durable (lock section 5). Stored
//! formats carry explicit revisions with exact-equality checks.
//!
//! Finite M0-test caps bound retained sessions, intent/outcome records,
//! profile metadata, and stored messages. Exhaustion is always an explicit
//! error, never a silent fallback.

use crate::approval::{ApprovedScope, NormalizedArgs};
use crate::error::{AgentError, ErrorCategory, RetryGuidance};
use crate::ids::{CallId, M0_REVISION, RunId, SessionId, ToolId};
use crate::limits::Limits;
use crate::outcomes::{EffectState, Evidence, ExecutionStatus};

/// Stored-format revision for M0. Exact equality; any mismatch is an
/// explicit failure with no migration or guessing.
pub const STORE_FORMAT_REVISION: u32 = M0_REVISION;

/// Maximum stored messages per checkpoint (M0-TEST choice aligned with the
/// retained-context budget).
pub const MAX_STORED_MESSAGES: usize = Limits::M0_TEST_RETAINED_CONTEXT_ITEMS;

/// Maximum retained session checkpoints per store (M0-TEST choice; not a
/// product default). A save of a new session beyond this bound fails
/// explicitly; existing sessions stay writable.
pub const MAX_SESSIONS: usize = 64;

/// Maximum retained tool-intent records per store (M0-TEST choice; not a
/// product default). Recording beyond this bound fails explicitly.
pub const MAX_INTENT_RECORDS: usize = 256;

/// Maximum retained tool-outcome records per store (M0-TEST choice; not a
/// product default). Recording beyond this bound fails explicitly.
pub const MAX_OUTCOME_RECORDS: usize = 256;

/// Maximum stored profile-metadata length in bytes (M0-TEST choice aligned
/// with the provider profile-name bound).
pub const MAX_PROFILE_LEN: usize = crate::provider::MAX_PROFILE_LEN;

/// Accepted message retained in a checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMessage {
    /// Speaker or source label (for example `user`, `assistant`).
    pub source: String,
    /// Accepted content text.
    pub text: String,
    /// True only for fully accepted assistant turns, never partial output.
    pub complete: bool,
}

/// Versioned session checkpoint: identity, format revision, logical
/// revision, accepted messages, and profile metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCheckpoint {
    session: SessionId,
    format_revision: u32,
    logical_revision: u64,
    messages: Vec<StoredMessage>,
    profile: String,
}

impl SessionCheckpoint {
    /// Builds a checkpoint after validating the format revision exactly and
    /// bounding retained messages and profile metadata. Partial assistant
    /// output must be labeled incomplete by the caller.
    pub fn new(
        session: SessionId,
        format_revision: u32,
        logical_revision: u64,
        messages: Vec<StoredMessage>,
        profile: impl Into<String>,
    ) -> Result<Self, AgentError> {
        check_format_revision(format_revision)?;
        let profile = profile.into();
        check_stored_parts(&messages, &profile)?;
        Ok(Self {
            session,
            format_revision,
            logical_revision,
            messages,
            profile,
        })
    }

    /// Returns the session identity.
    #[must_use]
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// Returns the stored-format revision.
    #[must_use]
    pub fn format_revision(&self) -> u32 {
        self.format_revision
    }

    /// Returns the logical revision for single-writer conflict checks.
    #[must_use]
    pub fn logical_revision(&self) -> u64 {
        self.logical_revision
    }

    /// Returns the accepted messages.
    #[must_use]
    pub fn messages(&self) -> &[StoredMessage] {
        &self.messages
    }

    /// Returns the profile metadata.
    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }
}

/// Validates a stored-format revision with exact equality.
pub fn check_format_revision(revision: u32) -> Result<(), AgentError> {
    if revision != STORE_FORMAT_REVISION {
        return Err(store_error("unsupported session format revision"));
    }
    Ok(())
}

/// Re-validates retained-message and profile-metadata bounds at a store
/// boundary. [`SessionCheckpoint::new`] already enforces these; a store calls
/// this so it never admits an out-of-bounds record.
pub fn check_checkpoint_bounds(checkpoint: &SessionCheckpoint) -> Result<(), AgentError> {
    check_stored_parts(checkpoint.messages(), checkpoint.profile())
}

fn check_stored_parts(messages: &[StoredMessage], profile: &str) -> Result<(), AgentError> {
    if messages.len() > MAX_STORED_MESSAGES {
        return Err(store_error("checkpoint exceeds retained message bound"));
    }
    for message in messages {
        if message.source.is_empty()
            || message.source.len() > 64
            || message.text.len() > Limits::M0_TEST_TOOL_OUTPUT_BYTES
        {
            return Err(store_error("checkpoint message is invalid"));
        }
    }
    if profile.is_empty() || profile.len() > MAX_PROFILE_LEN {
        return Err(store_error("checkpoint profile metadata is invalid"));
    }
    Ok(())
}

/// Durability intent for a side-effecting call, recorded before dispatch in
/// persistent mode. Arguments are validated; references stay bounded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolIntentRecord {
    /// Owning run.
    pub run: RunId,
    /// Host call identity.
    pub call: CallId,
    /// Resolved tool identity plus revision.
    pub tool: ToolId,
    /// Validated arguments or safe references.
    pub args: NormalizedArgs,
    /// Approved scope relevant to dispatch.
    pub scope: ApprovedScope,
}

/// Actual outcome linked to its intent, preserving execution, effect, and
/// evidence state without rewriting uncertainty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcomeRecord {
    /// Owning run.
    pub run: RunId,
    /// Host call identity.
    pub call: CallId,
    /// Execution status.
    pub status: ExecutionStatus,
    /// Known effect state.
    pub effect: EffectState,
    /// Evidence class.
    pub evidence: Evidence,
    /// True when the recorded result was truncated.
    pub truncated: bool,
}

/// Bounded session-metadata lookup result. Listing never loads full
/// conversation contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMetadata {
    /// Session identity.
    pub session: SessionId,
    /// Newest logical revision known for the session.
    pub logical_revision: u64,
}

/// Session-store port. Narrow by design: bounded listing, selected-session
/// load, versioned checkpoint save, and intent/outcome recording. A stale
/// save must not overwrite a newer session silently; conflict policy lives
/// in the implementation against `logical_revision`.
pub trait SessionStore {
    /// Lists up to `limit` session metadata entries without loading history.
    fn list_sessions(&self, limit: usize) -> Result<Vec<SessionMetadata>, AgentError>;

    /// Loads the selected session on demand.
    fn load_session(&self, id: &SessionId) -> Result<SessionCheckpoint, AgentError>;

    /// Saves a versioned checkpoint.
    fn save_checkpoint(&mut self, checkpoint: &SessionCheckpoint) -> Result<(), AgentError>;

    /// Records side-effecting intent before dispatch.
    fn record_intent(&mut self, intent: &ToolIntentRecord) -> Result<(), AgentError>;

    /// Records the actual outcome without rewriting the intent.
    fn record_outcome(&mut self, outcome: &ToolOutcomeRecord) -> Result<(), AgentError>;
}

fn store_error(message: &'static str) -> AgentError {
    AgentError::new(
        ErrorCategory::InvalidInput,
        message,
        RetryGuidance::DoNotRetry,
    )
    .expect("static safe store message builds")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint(revision: u32) -> Result<SessionCheckpoint, AgentError> {
        SessionCheckpoint::new(
            SessionId::new("sess-1").expect("valid"),
            revision,
            3,
            vec![StoredMessage {
                source: "user".to_owned(),
                text: "hello".to_owned(),
                complete: true,
            }],
            "test-profile",
        )
    }

    fn checkpoint_with_profile(profile: &str) -> Result<SessionCheckpoint, AgentError> {
        SessionCheckpoint::new(
            SessionId::new("sess-1").expect("valid"),
            STORE_FORMAT_REVISION,
            3,
            vec![StoredMessage {
                source: "user".to_owned(),
                text: "hello".to_owned(),
                complete: true,
            }],
            profile,
        )
    }

    #[test]
    fn incompatible_format_revision_fails_exact_equality() {
        assert!(checkpoint(STORE_FORMAT_REVISION).is_ok());
        assert_eq!(STORE_FORMAT_REVISION, 0);
        let error = checkpoint(STORE_FORMAT_REVISION + 1).unwrap_err();
        assert_eq!(error.category(), ErrorCategory::InvalidInput);
        assert!(check_format_revision(0).is_ok());
        assert!(check_format_revision(1).is_err());
    }

    #[test]
    fn checkpoint_bounds_retained_messages() {
        let messages = (0..MAX_STORED_MESSAGES + 1)
            .map(|index| StoredMessage {
                source: "user".to_owned(),
                text: format!("message {index}"),
                complete: true,
            })
            .collect();
        assert!(
            SessionCheckpoint::new(
                SessionId::new("sess-1").expect("valid"),
                STORE_FORMAT_REVISION,
                1,
                messages,
                "p",
            )
            .is_err()
        );
    }

    #[test]
    fn checkpoint_bounds_profile_metadata() {
        assert!(checkpoint_with_profile("").is_err(), "empty profile fails");
        assert!(
            checkpoint_with_profile(&"p".repeat(MAX_PROFILE_LEN)).is_ok(),
            "profile at the bound is accepted"
        );
        assert!(
            checkpoint_with_profile(&"p".repeat(MAX_PROFILE_LEN + 1)).is_err(),
            "profile beyond the bound fails"
        );
    }

    #[test]
    fn store_boundary_recheck_accepts_constructed_checkpoint() {
        let built = checkpoint(STORE_FORMAT_REVISION).expect("valid checkpoint builds");
        check_checkpoint_bounds(&built)
            .expect("constructed checkpoint passes the boundary recheck");
    }

    #[test]
    fn store_test_caps_are_finite_and_positive() {
        assert_eq!(MAX_STORED_MESSAGES, Limits::M0_TEST_RETAINED_CONTEXT_ITEMS);
        assert_eq!(MAX_PROFILE_LEN, crate::provider::MAX_PROFILE_LEN);
        const _: () = assert!(MAX_SESSIONS > 0);
        const _: () = assert!(MAX_INTENT_RECORDS > 0);
        const _: () = assert!(MAX_OUTCOME_RECORDS > 0);
    }
}

#[cfg(test)]
mod cov_store_private {
    use super::*;

    fn private_message(source: &str, text: &str) -> StoredMessage {
        StoredMessage {
            source: source.to_owned(),
            text: text.to_owned(),
            complete: true,
        }
    }

    #[test]
    fn private_stored_parts_bounds_are_byte_exact() {
        assert!(
            check_stored_parts(&[], "p").is_ok(),
            "a minimal bounded record is accepted"
        );

        // Source labels are bounded at the fixed 64-byte boundary; text at
        // the tool-output byte budget; both bounds are inclusive.
        assert!(check_stored_parts(&[private_message("", "t")], "p").is_err());
        let source_at_bound = "s".repeat(64);
        assert!(check_stored_parts(&[private_message(&source_at_bound, "t")], "p").is_ok());
        assert!(check_stored_parts(&[private_message(&"s".repeat(65), "t")], "p").is_err());

        let text_at_bound = "t".repeat(Limits::M0_TEST_TOOL_OUTPUT_BYTES);
        assert!(check_stored_parts(&[private_message("u", &text_at_bound)], "p").is_ok());
        let text_over_bound = "t".repeat(Limits::M0_TEST_TOOL_OUTPUT_BYTES + 1);
        assert!(check_stored_parts(&[private_message("u", &text_over_bound)], "p").is_err());

        // Profile metadata is non-empty and bounded, and the message count is
        // capped at exactly MAX_STORED_MESSAGES.
        assert!(check_stored_parts(&[], "").is_err());
        assert!(check_stored_parts(&[], &"p".repeat(MAX_PROFILE_LEN)).is_ok());
        assert!(check_stored_parts(&[], &"p".repeat(MAX_PROFILE_LEN + 1)).is_err());

        let at_count: Vec<StoredMessage> = (0..MAX_STORED_MESSAGES)
            .map(|_| private_message("u", "t"))
            .collect();
        assert!(check_stored_parts(&at_count, "p").is_ok());
        let over_count: Vec<StoredMessage> = (0..=MAX_STORED_MESSAGES)
            .map(|_| private_message("u", "t"))
            .collect();
        assert!(check_stored_parts(&over_count, "p").is_err());
    }

    #[test]
    fn private_boundary_recheck_rejects_construction_bypass() {
        // `SessionCheckpoint::new` validates, but the store boundary must not
        // trust construction: a checkpoint assembled through the private
        // fields with an over-count message list is rejected by the recheck.
        let bypass = SessionCheckpoint {
            session: SessionId::new("sess-bypass").expect("valid"),
            format_revision: STORE_FORMAT_REVISION,
            logical_revision: 1,
            messages: (0..=MAX_STORED_MESSAGES)
                .map(|_| private_message("u", "t"))
                .collect(),
            profile: "p".to_owned(),
        };
        assert!(check_checkpoint_bounds(&bypass).is_err());

        let wrong_format = SessionCheckpoint {
            session: SessionId::new("sess-format").expect("valid"),
            format_revision: STORE_FORMAT_REVISION + 1,
            logical_revision: 1,
            messages: vec![private_message("u", "t")],
            profile: "p".to_owned(),
        };
        assert!(
            check_checkpoint_bounds(&wrong_format).is_ok(),
            "the bounds recheck judges stored parts, not the format revision"
        );
        assert!(check_format_revision(wrong_format.format_revision()).is_err());
    }

    #[test]
    fn private_equality_covers_every_stored_field() {
        // Conflict detection at equal revisions relies on whole-record
        // equality, so every stored field must participate.
        let base = SessionCheckpoint {
            session: SessionId::new("sess-eq").expect("valid"),
            format_revision: STORE_FORMAT_REVISION,
            logical_revision: 9,
            messages: vec![private_message("user", "hello")],
            profile: "p".to_owned(),
        };
        assert_eq!(base, base.clone());

        let mut other_session = base.clone();
        other_session.session = SessionId::new("sess-other").expect("valid");
        assert_ne!(base, other_session);

        let mut other_format = base.clone();
        other_format.format_revision = STORE_FORMAT_REVISION + 1;
        assert_ne!(base, other_format);

        let mut other_revision = base.clone();
        other_revision.logical_revision = 10;
        assert_ne!(base, other_revision);

        let mut other_text = base.clone();
        other_text.messages[0].text = "changed".to_owned();
        assert_ne!(base, other_text);

        let mut other_complete = base.clone();
        other_complete.messages[0].complete = false;
        assert_ne!(base, other_complete);

        let mut other_profile = base.clone();
        other_profile.profile = "other".to_owned();
        assert_ne!(base, other_profile);
    }

    #[test]
    fn private_store_error_is_invalid_input_without_retry() {
        let error = store_error("private coverage probe");
        assert_eq!(error.category(), ErrorCategory::InvalidInput);
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        assert_eq!(error.message(), "private coverage probe");
        assert!(error.correlation().is_empty());
    }
}
