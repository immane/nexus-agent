//! Opaque identifier newtypes (lock section 11).
//!
//! Every identifier is an opaque validated string: at most 64 characters of
//! `[A-Za-z0-9_-]`. Identifiers are never filesystem paths and never imply
//! permissions. Each event and grant carries its owning run; provider item
//! keys and call references stay separate (see [`crate::content`]).

/// Maximum identifier length in bytes (charset is ASCII, so bytes == chars).
pub const MAX_ID_LEN: usize = 64;

/// M0 revision for every exact-equality revision check (lock section 6).
/// Any mismatch is an explicit failure; no migration, no guessing.
pub const M0_REVISION: u32 = 0;

/// Identifier validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdError {
    /// Empty identifiers carry no identity.
    Empty,
    /// Longer than [`MAX_ID_LEN`] bytes.
    TooLong,
    /// Contains a byte outside `[A-Za-z0-9_-]`.
    IllegalChar,
}

impl std::fmt::Display for IdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "identifier is empty"),
            Self::TooLong => write!(f, "identifier exceeds 64 characters"),
            Self::IllegalChar => write!(f, "identifier contains illegal characters"),
        }
    }
}

impl std::error::Error for IdError {}

const fn is_id_byte(byte: u8) -> bool {
    matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_')
}

fn validate_id(raw: &str) -> Result<(), IdError> {
    if raw.is_empty() {
        return Err(IdError::Empty);
    }
    if raw.len() > MAX_ID_LEN {
        return Err(IdError::TooLong);
    }
    if !raw.bytes().all(is_id_byte) {
        return Err(IdError::IllegalChar);
    }
    Ok(())
}

/// Host-issued conversation identity, unique in the configured store.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    /// Validates the raw value at the trust boundary.
    pub fn new(raw: impl Into<String>) -> Result<Self, IdError> {
        let value = raw.into();
        validate_id(&value)?;
        Ok(Self(value))
    }

    /// Returns the opaque value. Never use as a filesystem path.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Runtime-issued execution identity, distinct across runs and records.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunId(String);

impl RunId {
    /// Validates the raw value at the trust boundary.
    pub fn new(raw: impl Into<String>) -> Result<Self, IdError> {
        let value = raw.into();
        validate_id(&value)?;
        Ok(Self(value))
    }

    /// Returns the opaque value. Never use as a filesystem path.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for RunId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Runtime-issued model invocation identity within a run.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TurnId(String);

impl TurnId {
    /// Validates the raw value at the trust boundary.
    pub fn new(raw: impl Into<String>) -> Result<Self, IdError> {
        let value = raw.into();
        validate_id(&value)?;
        Ok(Self(value))
    }

    /// Returns the opaque value. Never use as a filesystem path.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TurnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for TurnId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Runtime-issued identity for an admitted tool call.
/// Assigned only after a complete turn is accepted; stream fragments,
/// unknown tools, and invalid arguments never receive one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CallId(String);

impl CallId {
    /// Validates the raw value at the trust boundary.
    pub fn new(raw: impl Into<String>) -> Result<Self, IdError> {
        let value = raw.into();
        validate_id(&value)?;
        Ok(Self(value))
    }

    /// Returns the opaque value. Never use as a filesystem path.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CallId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for CallId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Frontend correlation identity; not an automatic idempotency guarantee.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RequestId(String);

impl RequestId {
    /// Validates the raw value at the trust boundary.
    pub fn new(raw: impl Into<String>) -> Result<Self, IdError> {
        let value = raw.into();
        validate_id(&value)?;
        Ok(Self(value))
    }

    /// Returns the opaque value. Never use as a filesystem path.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for RequestId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Runtime-issued grant request bound to one specific call.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ApprovalId(String);

impl ApprovalId {
    /// Validates the raw value at the trust boundary.
    pub fn new(raw: impl Into<String>) -> Result<Self, IdError> {
        let value = raw.into();
        validate_id(&value)?;
        Ok(Self(value))
    }

    /// Returns the opaque value. Never use as a filesystem path.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ApprovalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for ApprovalId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Namespaced registered tool identity with an implementation revision.
///
/// The lock charset `[A-Za-z0-9_-]` carries no hierarchy separator, so the
/// namespace is encoded with `-` or `_` by convention (for example
/// `host_read`). Revision comparison is exact equality against
/// [`M0_REVISION`]; any mismatch is an explicit failure.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolId {
    name: String,
    revision: u32,
}

impl ToolId {
    /// Validates the name at the trust boundary and attaches the revision.
    pub fn new(name: impl Into<String>, revision: u32) -> Result<Self, IdError> {
        let name = name.into();
        validate_id(&name)?;
        Ok(Self { name, revision })
    }

    /// Returns the namespaced tool name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the implementation revision.
    #[must_use]
    pub fn revision(&self) -> u32 {
        self.revision
    }

    /// Exact-equality compatibility: same name and same revision.
    #[must_use]
    pub fn is_compatible_with(&self, other: &ToolId) -> bool {
        self == other
    }
}

impl std::fmt::Display for ToolId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.name, self.revision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_accepts_max_length_boundary() {
        let boundary = "a".repeat(MAX_ID_LEN);
        assert_eq!(
            SessionId::new(boundary.clone())
                .expect("64 chars accepted")
                .as_str(),
            boundary
        );
        assert_eq!(
            SessionId::new("a".repeat(MAX_ID_LEN + 1)),
            Err(IdError::TooLong)
        );
    }

    #[test]
    fn every_id_type_rejects_overlong_and_illegal() {
        let overlong = "x".repeat(MAX_ID_LEN + 1);
        assert_eq!(RunId::new(&overlong), Err(IdError::TooLong));
        assert_eq!(TurnId::new(&overlong), Err(IdError::TooLong));
        assert_eq!(CallId::new(&overlong), Err(IdError::TooLong));
        assert_eq!(RequestId::new(&overlong), Err(IdError::TooLong));
        assert_eq!(ApprovalId::new(&overlong), Err(IdError::TooLong));
        assert_eq!(ToolId::new(overlong, M0_REVISION), Err(IdError::TooLong));
        for illegal in ["", "a/b", "a b", "a.b", "ünïcode", "a:b"] {
            assert_eq!(
                SessionId::new(illegal),
                if illegal.is_empty() {
                    Err(IdError::Empty)
                } else {
                    Err(IdError::IllegalChar)
                },
                "input {illegal:?}"
            );
            assert!(RunId::new(illegal).is_err(), "input {illegal:?}");
            assert!(TurnId::new(illegal).is_err(), "input {illegal:?}");
            assert!(CallId::new(illegal).is_err(), "input {illegal:?}");
            assert!(RequestId::new(illegal).is_err(), "input {illegal:?}");
            assert!(ApprovalId::new(illegal).is_err(), "input {illegal:?}");
            assert!(
                ToolId::new(illegal, M0_REVISION).is_err(),
                "input {illegal:?}"
            );
        }
    }

    #[test]
    fn id_types_are_scoped_and_do_not_alias() {
        let session = SessionId::new("shared").expect("valid");
        let run = RunId::new("shared").expect("valid");
        assert_eq!(session.as_str(), run.as_str());
        assert_eq!(session.to_string(), "shared");
        assert_eq!(run.as_ref() as &str, "shared");
        // Distinct types: no cross-type equality exists to misuse.
        fn takes_session(_: SessionId) {}
        takes_session(session);
    }

    #[test]
    fn tool_id_carries_exact_revision() {
        let current = ToolId::new("host_read", M0_REVISION).expect("valid");
        let newer = ToolId::new("host_read", M0_REVISION + 1).expect("valid");
        let renamed = ToolId::new("host_write", M0_REVISION).expect("valid");
        assert!(current.is_compatible_with(&current));
        assert!(!current.is_compatible_with(&newer));
        assert!(!current.is_compatible_with(&renamed));
        assert_eq!(current.revision(), M0_REVISION);
    }

    #[test]
    fn m0_revision_is_zero() {
        assert_eq!(M0_REVISION, 0);
    }
}
