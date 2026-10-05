//! Configuration errors, document parsing, file persistence, and secrets.
//!
//! Errors are static diagnostics that never interpolate caller text, so a
//! value can never leak through a failure. Loading distinguishes a missing
//! file (caller falls back to defaults) from an invalid one (explicit
//! error). Saves are atomic (temporary file plus rename) with owner-only
//! permissions on Unix.

use std::path::Path;

use crate::model::{CONFIG_REVISION, UserConfig};

/// Maximum configuration document size in bytes (parse budget).
pub const MAX_DOCUMENT_BYTES: usize = 65_536;
/// Environment variable overriding the configuration file path.
pub const CONFIG_ENV_VAR: &str = "NEXUS_CONFIG";
/// Configuration file name under the platform config directory.
pub const CONFIG_FILE_NAME: &str = "config.json";

/// Error kind for configuration failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigErrorKind {
    /// The document or value is malformed, unknown, or out of bounds.
    Invalid,
    /// A filesystem operation failed.
    Io,
    /// A referenced credential is missing or empty.
    MissingCredential,
    /// The document revision is not supported.
    UnsupportedRevision,
}

/// Configuration failure with a static diagnostic. Values are never
/// interpolated, so secrets cannot cross into logs or reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    kind: ConfigErrorKind,
    message: &'static str,
}

impl ConfigError {
    /// Returns the error kind.
    #[must_use]
    pub fn kind(&self) -> ConfigErrorKind {
        self.kind
    }

    /// Returns the static diagnostic.
    #[must_use]
    pub fn message(&self) -> &str {
        self.message
    }

    /// Builds the unsupported-revision error.
    #[must_use]
    pub fn unsupported() -> Self {
        Self {
            kind: ConfigErrorKind::UnsupportedRevision,
            message: "configuration revision is not supported",
        }
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "[config] {}", self.message)
    }
}

impl std::error::Error for ConfigError {}

/// Builds an invalid-configuration error with a static message.
pub(crate) fn invalid(message: &'static str) -> ConfigError {
    ConfigError {
        kind: ConfigErrorKind::Invalid,
        message,
    }
}

/// Strict-parses a configuration document: duplicate keys never last-win
/// and byte/depth/node budgets bound the text before validation.
pub(crate) fn parse_document(text: &str) -> Result<serde_json::Value, ConfigError> {
    if text.len() > MAX_DOCUMENT_BYTES {
        return Err(invalid("configuration is too large"));
    }
    nexus_validation::parse_object(text, MAX_DOCUMENT_BYTES)
        .map_err(|_| invalid("configuration is invalid"))
}

/// Resolves the configuration file path with the documented precedence:
/// explicit CLI path, then the `NEXUS_CONFIG` environment variable, then
/// the platform default. Returns `None` when no source names a path.
pub fn resolve_path(cli: Option<&str>) -> Option<std::path::PathBuf> {
    resolve_path_with(cli, |var| std::env::var(var).ok())
}

/// Resolves the path with an injected environment lookup, so precedence
/// is testable without touching the process environment.
pub fn resolve_path_with(
    cli: Option<&str>,
    env: impl FnOnce(&str) -> Option<String>,
) -> Option<std::path::PathBuf> {
    if let Some(path) = cli.filter(|path| !path.is_empty()) {
        return Some(std::path::PathBuf::from(path));
    }
    if let Some(path) = env(CONFIG_ENV_VAR).filter(|path| !path.is_empty()) {
        return Some(std::path::PathBuf::from(path));
    }
    default_path()
}

/// Returns the platform default configuration file path, if the platform
/// exposes a config directory.
#[must_use]
pub fn default_path() -> Option<std::path::PathBuf> {
    default_path_with(
        std::env::var("XDG_CONFIG_HOME").ok(),
        std::env::var("HOME").ok(),
    )
}

/// Computes the default path from injected locations: `XDG_CONFIG_HOME`
/// wins, otherwise `$HOME/.config`, each joined with the file name.
/// Pure and fully testable; [`default_path`] reads the environment.
#[must_use]
pub fn default_path_with(
    xdg_config_home: Option<String>,
    home: Option<String>,
) -> Option<std::path::PathBuf> {
    if let Some(base) = xdg_config_home.filter(|base| !base.is_empty()) {
        return Some(
            std::path::PathBuf::from(base)
                .join("nexus-agent")
                .join(CONFIG_FILE_NAME),
        );
    }
    home.filter(|base| !base.is_empty()).map(|base| {
        std::path::PathBuf::from(base)
            .join(".config")
            .join("nexus-agent")
            .join(CONFIG_FILE_NAME)
    })
}

/// Loads the document at `path`, or `None` when the file is absent (the
/// caller falls back to defaults). Present but invalid files are explicit
/// errors, never silent defaults.
pub fn load(path: &Path) -> Result<Option<UserConfig>, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(ConfigError {
                kind: ConfigErrorKind::Io,
                message: "configuration could not be read",
            });
        }
    };
    UserConfig::from_json(&text).map(Some)
}

/// Saves atomically: write a temporary sibling, restrict it to the owner,
/// flush it, then rename over the target. Readers never observe a partial
/// document.
pub fn save(config: &UserConfig, path: &Path) -> Result<(), ConfigError> {
    let io_error = || ConfigError {
        kind: ConfigErrorKind::Io,
        message: "configuration could not be written",
    };
    let parent = path.parent().ok_or_else(io_error)?;
    let temporary = parent.join(format!(".nexus-config-{}.tmp", std::process::id()));
    std::fs::write(&temporary, config.to_json()).map_err(|_| io_error())?;
    restrict_owner_only(&temporary).map_err(|_| io_error())?;
    std::fs::rename(&temporary, path).map_err(|_| {
        let _ = std::fs::remove_file(&temporary);
        io_error()
    })?;
    Ok(())
}

/// Restricts a file to owner-only access on Unix. Elsewhere this is a
/// documented no-op: the format still carries no secret values, so there
/// is nothing to disclose beyond names and labels.
#[cfg(unix)]
fn restrict_owner_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_owner_only(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Resolves a credential reference for the selected integration, reading
/// the value only at use time. Missing or empty variables are safe
/// actionable failures; the value never enters an error or a log.
pub fn resolve_credential(credential: &crate::model::CredentialRef) -> Result<String, ConfigError> {
    resolve_with(credential, |var| {
        std::env::var(var).ok().filter(|value| !value.is_empty())
    })
}

/// Resolves a credential through an injected lookup, so the missing/empty
/// contract is testable without touching the process environment. `None`
/// and empty values both fail; the looked-up value never enters an error.
pub fn resolve_with(
    credential: &crate::model::CredentialRef,
    lookup: impl FnOnce(&str) -> Option<String>,
) -> Result<String, ConfigError> {
    match lookup(credential.var_name()) {
        Some(value) if !value.is_empty() => Ok(value),
        _ => Err(ConfigError {
            kind: ConfigErrorKind::MissingCredential,
            message: "credential is not set",
        }),
    }
}

/// Renders a redacted summary: identities, labels, references, and order,
/// but no secret values (the document cannot hold any).
#[must_use]
pub fn summary(config: &UserConfig) -> serde_json::Value {
    serde_json::json!({
        "revision": CONFIG_REVISION,
        "providers": config.providers().iter().map(|profile| {
            serde_json::json!({
                "id": profile.id,
                "display_name": profile.display_name,
                "adapter": profile.adapter.name(),
                "endpoint": profile.endpoint,
                "credential": { "env": profile.credential.var_name() },
                "default_model": profile.default_model,
            })
        }).collect::<Vec<_>>(),
        "models": config.models().iter().map(|entry| {
            serde_json::json!({
                "id": entry.id,
                "provider": entry.provider,
                "name": entry.name,
            })
        }).collect::<Vec<_>>(),
        "favourites": config.favourites(),
        "recent": config.recent(),
    })
}
