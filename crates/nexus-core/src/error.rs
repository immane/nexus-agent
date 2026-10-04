//! Typed agent errors with a safe public boundary.
//!
//! [`AgentError`] carries a typed category, a safe message, bounded
//! correlation data, and advisory retry guidance. Diagnostics containing
//! secrets or unrestricted payloads must never cross this boundary: callers
//! MUST supply pre-redacted message and correlation text, and SHOULD pass
//! static diagnostics that never interpolate credentials, raw external
//! payloads, or user content.
//!
//! The constructors enforce documented bounds and apply a best-effort marker
//! net for known secret shapes. The net reduces accidental leaks; it does
//! not prove that text is secret-free, so it never replaces caller
//! redaction.

/// Maximum safe-message length in bytes.
pub const MAX_MESSAGE_LEN: usize = 1024;
/// Maximum correlation entries per error.
pub const MAX_CORRELATION_ENTRIES: usize = 8;
/// Maximum correlation key length in bytes.
pub const MAX_CORRELATION_KEY_LEN: usize = 64;
/// Maximum correlation value length in bytes.
pub const MAX_CORRELATION_VALUE_LEN: usize = 256;

/// Substrings that suggest secret material. Best-effort boundary net, not a
/// secret detector; matched case-insensitively against every message and
/// correlation value before an [`AgentError`] may exist. A match rejects the
/// diagnostic, but a non-match does not prove the text is secret-free:
/// callers must still pre-redact.
const SECRET_MARKERS: &[&str] = &[
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

fn contains_secret_marker(text: &str) -> bool {
    let lowered = text.to_lowercase();
    SECRET_MARKERS.iter().any(|marker| lowered.contains(marker))
}

/// Best-effort safe-text check shared by trust-boundary validators: the text
/// must be non-empty, within `max_bytes`, and free of known secret markers.
/// Passing does not prove the text is secret-free; validators using this
/// helper must also document that callers pre-redact.
pub(crate) fn is_bounded_safe_text(text: &str, max_bytes: usize) -> bool {
    !text.is_empty() && text.len() <= max_bytes && !contains_secret_marker(text)
}

/// Typed failure category covering the contract outcome vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    /// Malformed or out-of-bounds input at a trust boundary.
    InvalidInput,
    /// Requested capability the selected adapter or tool does not offer.
    UnsupportedCapability,
    /// Authentication failed or credentials are missing.
    Authentication,
    /// Authorization, approval, or scope check refused execution.
    PermissionDenied,
    /// Rate limit signaled by the provider or host policy.
    RateLimited,
    /// Transport, framing, or protocol violation.
    Protocol,
    /// Deadline exceeded while the effect state may be uncertain.
    Timeout,
    /// Work was cancelled; recorded effects are preserved, never rewritten.
    Cancelled,
    /// A finite budget was exhausted; never silently bypassed.
    ResourceLimit,
    /// The tool executed and reported failure.
    ToolFailure,
    /// Session persistence failed or is unavailable.
    StorageFailure,
    /// Effects happened (or not) but evidence is inconclusive.
    UncertainOutcome,
    /// Unexpected internal failure.
    Internal,
}

impl ErrorCategory {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid-input",
            Self::UnsupportedCapability => "unsupported-capability",
            Self::Authentication => "authentication",
            Self::PermissionDenied => "permission-denied",
            Self::RateLimited => "rate-limited",
            Self::Protocol => "protocol",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::ResourceLimit => "resource-limit",
            Self::ToolFailure => "tool-failure",
            Self::StorageFailure => "storage-failure",
            Self::UncertainOutcome => "uncertain-outcome",
            Self::Internal => "internal",
        }
    }
}

/// Advisory retry guidance. Host policy and known effect state still govern
/// retries; unknown effects forbid blind replay regardless of this hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryGuidance {
    /// Retrying cannot help (budgets, denials, validation failures).
    DoNotRetry,
    /// A retry may succeed after backoff (rate limits, transient transport).
    RetryAfterBackoff,
    /// The operation had no effects, so a retry is safe.
    SafeToRetry,
}

/// Bounded key/value correlation data. Values are bounded and checked
/// against the best-effort marker net; callers must supply pre-redacted
/// values because the net is not a secret detector.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CorrelationData(Vec<(String, String)>);

impl CorrelationData {
    /// Creates empty correlation data.
    #[must_use]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Appends one entry after bounds and best-effort marker checks.
    /// The caller must pass a pre-redacted value; a passing check does not
    /// prove the value is secret-free.
    pub fn push(
        &mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<(), ErrorBuildError> {
        let key = key.into();
        let value = value.into();
        if self.0.len() >= MAX_CORRELATION_ENTRIES {
            return Err(ErrorBuildError::TooLong);
        }
        if key.is_empty()
            || key.len() > MAX_CORRELATION_KEY_LEN
            || value.len() > MAX_CORRELATION_VALUE_LEN
        {
            return Err(ErrorBuildError::TooLong);
        }
        if contains_secret_marker(&key) || contains_secret_marker(&value) {
            return Err(ErrorBuildError::SuspectedSecret);
        }
        self.0.push((key, value));
        Ok(())
    }

    /// Iterates over entries.
    pub fn iter(&self) -> std::slice::Iter<'_, (String, String)> {
        self.0.iter()
    }

    /// Returns the entry count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns true when no entries are present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Failure to build an [`AgentError`]: the proposed diagnostic itself was
/// invalid or unsafe to carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorBuildError {
    /// Empty message or empty correlation key.
    Empty,
    /// Exceeds a documented bound.
    TooLong,
    /// Suspected secret material must be redacted before crossing the boundary.
    SuspectedSecret,
}

impl std::fmt::Display for ErrorBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "error message is empty"),
            Self::TooLong => write!(f, "error diagnostic exceeds its bound"),
            Self::SuspectedSecret => {
                write!(f, "error diagnostic may contain secret material")
            }
        }
    }
}

impl std::error::Error for ErrorBuildError {}

/// Public error type. Diagnostics here are bounded and pre-redacted by their
/// caller; the constructor's marker net is best-effort only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentError {
    category: ErrorCategory,
    message: String,
    correlation: CorrelationData,
    retry: RetryGuidance,
}

impl AgentError {
    /// Builds an error without correlation data. `message` must be
    /// pre-redacted display text; prefer a static diagnostic that does not
    /// interpolate credentials, raw payloads, or user content.
    pub fn new(
        category: ErrorCategory,
        message: impl Into<String>,
        retry: RetryGuidance,
    ) -> Result<Self, ErrorBuildError> {
        Self::with_correlation(category, message, CorrelationData::new(), retry)
    }

    /// Builds an error with bounded correlation data. `message` and the
    /// correlation values must be pre-redacted; the marker net is
    /// best-effort and never proves safety.
    pub fn with_correlation(
        category: ErrorCategory,
        message: impl Into<String>,
        correlation: CorrelationData,
        retry: RetryGuidance,
    ) -> Result<Self, ErrorBuildError> {
        let message = message.into();
        if message.is_empty() {
            return Err(ErrorBuildError::Empty);
        }
        if message.len() > MAX_MESSAGE_LEN {
            return Err(ErrorBuildError::TooLong);
        }
        if correlation.len() > MAX_CORRELATION_ENTRIES {
            return Err(ErrorBuildError::TooLong);
        }
        if contains_secret_marker(&message) {
            return Err(ErrorBuildError::SuspectedSecret);
        }
        Ok(Self {
            category,
            message,
            correlation,
            retry,
        })
    }

    /// Returns the typed failure category.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        self.category
    }

    /// Returns the safe message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the bounded correlation data.
    #[must_use]
    pub fn correlation(&self) -> &CorrelationData {
        &self.correlation
    }

    /// Returns the advisory retry guidance.
    #[must_use]
    pub fn retry(&self) -> RetryGuidance {
        self.retry
    }
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.category.as_str(), self.message)
    }
}

impl std::error::Error for AgentError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn safe_error() -> AgentError {
        AgentError::new(
            ErrorCategory::Timeout,
            "tool deadline exceeded",
            RetryGuidance::DoNotRetry,
        )
        .expect("static safe message builds")
    }

    #[test]
    fn valid_error_exposes_category_message_and_retry() {
        let error = safe_error();
        assert_eq!(error.category(), ErrorCategory::Timeout);
        assert_eq!(error.message(), "tool deadline exceeded");
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        assert!(error.correlation().is_empty());
        assert_eq!(error.to_string(), "[timeout] tool deadline exceeded");
    }

    #[test]
    fn secret_bearing_messages_never_become_errors() {
        for secret in [
            "login failed, password=hunter2",
            "upstream said Bearer abc123",
            "key dump: sk-live-0000",
            "-----BEGIN PRIVATE KEY-----",
            "rejected api_key=AAAA",
            "AKIAIOSFODNN7EXAMPLE denied",
        ] {
            assert_eq!(
                AgentError::new(
                    ErrorCategory::Authentication,
                    secret,
                    RetryGuidance::DoNotRetry
                ),
                Err(ErrorBuildError::SuspectedSecret),
                "message {secret:?} must not cross the boundary"
            );
        }
    }

    #[test]
    fn secret_bearing_correlation_never_crosses() {
        let mut correlation = CorrelationData::new();
        assert_eq!(
            correlation.push("token", "Bearer abc123"),
            Err(ErrorBuildError::SuspectedSecret)
        );
        assert!(correlation.is_empty());
        let mut correlation = CorrelationData::new();
        correlation
            .push("call", "call-1")
            .expect("safe entry builds");
        assert_eq!(correlation.len(), 1);
    }

    #[test]
    fn bounds_are_enforced() {
        assert_eq!(
            AgentError::new(ErrorCategory::Internal, "", RetryGuidance::DoNotRetry),
            Err(ErrorBuildError::Empty)
        );
        assert_eq!(
            AgentError::new(
                ErrorCategory::Internal,
                "x".repeat(MAX_MESSAGE_LEN + 1),
                RetryGuidance::DoNotRetry
            ),
            Err(ErrorBuildError::TooLong)
        );
        let mut correlation = CorrelationData::new();
        for index in 0..MAX_CORRELATION_ENTRIES {
            correlation
                .push(format!("k{index}"), "v".to_owned())
                .expect("entries within bound build");
        }
        assert_eq!(
            correlation.push("extra", "v"),
            Err(ErrorBuildError::TooLong)
        );
    }

    #[test]
    fn marker_net_is_best_effort_not_a_secret_detector() {
        // The net matches known marker substrings only. Text carrying a
        // secret in another form passes, which is why callers must
        // pre-redact; this test pins the documented limitation.
        assert!(
            AgentError::new(
                ErrorCategory::Authentication,
                "login rejected for hunter2",
                RetryGuidance::DoNotRetry
            )
            .is_ok(),
            "the net does not prove the absence of secrets"
        );
    }

    #[test]
    fn bounded_safe_text_enforces_bounds_and_markers() {
        assert!(is_bounded_safe_text("delete directory", MAX_MESSAGE_LEN));
        assert!(!is_bounded_safe_text("", MAX_MESSAGE_LEN));
        assert!(!is_bounded_safe_text("x", 0));
        assert!(!is_bounded_safe_text("password=hunter2", MAX_MESSAGE_LEN));
        assert!(!is_bounded_safe_text(
            &"x".repeat(MAX_MESSAGE_LEN + 1),
            MAX_MESSAGE_LEN
        ));
    }
}
