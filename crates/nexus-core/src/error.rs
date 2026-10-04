//! Typed agent errors with a safe public boundary.
//!
//! [`AgentError`] carries a typed category, a safe message, bounded
//! correlation data, and advisory retry guidance. Diagnostics containing
//! secrets or unrestricted payloads must never cross this boundary: the
//! constructors reject suspected secret material instead of carrying it.

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
/// correlation value before an [`AgentError`] may exist.
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

/// Bounded key/value correlation data. Values are non-secret by
/// construction: the constructor rejects suspected secret material.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CorrelationData(Vec<(String, String)>);

impl CorrelationData {
    /// Creates empty correlation data.
    #[must_use]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Appends one entry after bounds and secret checks.
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

/// Public error type. Only safe, bounded, secret-free diagnostics exist here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentError {
    category: ErrorCategory,
    message: String,
    correlation: CorrelationData,
    retry: RetryGuidance,
}

impl AgentError {
    /// Builds an error without correlation data.
    pub fn new(
        category: ErrorCategory,
        message: impl Into<String>,
        retry: RetryGuidance,
    ) -> Result<Self, ErrorBuildError> {
        Self::with_correlation(category, message, CorrelationData::new(), retry)
    }

    /// Builds an error with bounded correlation data.
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
}
