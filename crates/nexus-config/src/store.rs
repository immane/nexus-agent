//! Configuration errors, document parsing, file persistence, and secrets.
//!
//! Errors are static diagnostics that never interpolate caller text, so a
//! value can never leak through a failure. Loading distinguishes a missing
//! file (caller falls back to defaults) from an invalid one (explicit
//! error). Saves are atomic (temporary file plus rename) with owner-only
//! permissions on Unix.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::model::{CONFIG_REVISION, UserConfig};

/// Maximum configuration document size in bytes (parse budget).
pub const MAX_DOCUMENT_BYTES: usize = 65_536;
/// Maximum one stored credential value accepted from an authenticated caller.
pub const MAX_CREDENTIAL_BYTES: usize = 16_384;
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
    pub fn message(&self) -> &'static str {
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

/// Per-process nonce disambiguating concurrent saves: the temporary name
/// mixes pid, wall-clock nanos, and this counter, so two saves never share
/// a sibling even within one process.
static SAVE_NONCE: AtomicU64 = AtomicU64::new(0);

/// Saves atomically: write a uniquely named temporary sibling created with
/// exclusive semantics and owner-only permissions, flush it, then rename
/// over the target. Readers never observe a partial document, concurrent
/// saves never share a temporary, and no world-readable window exists.
pub fn save(config: &UserConfig, path: &Path) -> Result<(), ConfigError> {
    write_atomic(
        path,
        config.to_json().as_bytes(),
        "configuration could not be written",
    )
}

/// Atomic owner-only replacement shared by the configuration and stored
/// credentials. A uniquely named sibling is written, flushed, restricted, and
/// renamed so readers never observe a partial document.
fn write_atomic(path: &Path, contents: &[u8], message: &'static str) -> Result<(), ConfigError> {
    let io_error = || ConfigError {
        kind: ConfigErrorKind::Io,
        message,
    };
    let parent = path.parent().ok_or_else(io_error)?;
    let pid = std::process::id();
    let mut attempts = 0;
    loop {
        let nonce = SAVE_NONCE.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let temporary = parent.join(format!(".nexus-config-{pid}-{nanos}-{nonce}.tmp"));
        let mut file = match create_temp(&temporary) {
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempts < 8 => {
                attempts += 1;
                continue;
            }
            Err(_) => return Err(io_error()),
            Ok(file) => file,
        };
        if file.write_all(contents).is_err()
            || restrict_owner_only(&temporary).is_err()
            || file.sync_all().is_err()
        {
            drop(file);
            let _ = std::fs::remove_file(&temporary);
            return Err(io_error());
        }
        drop(file);
        if std::fs::rename(&temporary, path).is_err() {
            let _ = std::fs::remove_file(&temporary);
            return Err(io_error());
        }
        return Ok(());
    }
}

/// Creates the temporary save sibling exclusively: `create_new` fails when
/// the name already exists instead of truncating a concurrent save. On Unix
/// the file is born owner-only, so no permissive window precedes the
/// post-write restriction.
#[cfg(unix)]
fn create_temp(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Creates the temporary save sibling exclusively on non-Unix platforms,
/// where owner-only permissions are a documented no-op.
#[cfg(not(unix))]
fn create_temp(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
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

/// Returns the stored-credential file beside the default configuration file.
#[must_use]
pub fn credentials_path() -> Option<PathBuf> {
    default_path().map(|path| path.with_file_name("credentials.json"))
}

/// Resolves a credential reference for the selected integration, reading
/// the value only at use time. Missing or empty credentials are safe
/// actionable failures; the value never enters an error or a log.
pub fn resolve_credential(credential: &crate::model::CredentialRef) -> Result<String, ConfigError> {
    resolve_credential_at(credential, None)
}

/// Resolves a credential with an explicit stored-credential file. Env-var
/// references still use the process environment. `None` uses the default
/// credentials path, preserving the existing API.
pub fn resolve_credential_at(
    credential: &crate::model::CredentialRef,
    stored_path: Option<&Path>,
) -> Result<String, ConfigError> {
    match credential {
        crate::model::CredentialRef::EnvVar { .. } => resolve_with(credential, |var| {
            std::env::var(var).ok().filter(|value| !value.is_empty())
        }),
        crate::model::CredentialRef::Stored { provider } => {
            let path = stored_path
                .map(Path::to_path_buf)
                .or_else(credentials_path)
                .ok_or_else(missing_credential)?;
            resolve_stored_at(provider, &path)
        }
    }
}

/// Resolves a credential through an injected lookup, so the missing/empty
/// contract is testable without touching the process environment. Stored
/// references use the credentials file instead of the injected lookup. `None`
/// and empty values both fail; the looked-up value never enters an error.
pub fn resolve_with(
    credential: &crate::model::CredentialRef,
    lookup: impl FnOnce(&str) -> Option<String>,
) -> Result<String, ConfigError> {
    let var = match credential {
        crate::model::CredentialRef::EnvVar { var } => var,
        crate::model::CredentialRef::Stored { provider } => {
            let path = default_path()
                .map(|path| path.with_file_name("credentials.json"))
                .ok_or_else(missing_credential)?;
            return resolve_stored_at(provider, &path);
        }
    };
    match lookup(var) {
        Some(value) if !value.is_empty() => Ok(value),
        _ => Err(missing_credential()),
    }
}

fn missing_credential() -> ConfigError {
    ConfigError {
        kind: ConfigErrorKind::MissingCredential,
        message: "credential is not set",
    }
}

fn read_credentials_at(
    path: &Path,
) -> Result<serde_json::Map<String, serde_json::Value>, ConfigError> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(serde_json::Map::new());
        }
        Err(_) => {
            return Err(ConfigError {
                kind: ConfigErrorKind::Io,
                message: "credentials could not be read",
            });
        }
    };
    let mut bytes = Vec::new();
    file.take(MAX_DOCUMENT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ConfigError {
            kind: ConfigErrorKind::Io,
            message: "credentials could not be read",
        })?;
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(invalid("credentials are too large"));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| invalid("credentials are invalid"))?;
    let document = nexus_validation::parse_object(text, MAX_DOCUMENT_BYTES)
        .map_err(|_| invalid("credentials are invalid"))?;
    let object = document
        .as_object()
        .ok_or(invalid("credentials are invalid"))?
        .clone();
    if !object.iter().all(|(id, value)| {
        crate::model::is_id_text(id) && value.as_str().is_some_and(|key| !key.is_empty())
    }) {
        return Err(invalid("credentials are invalid"));
    }
    Ok(object)
}

fn resolve_stored_at(provider: &str, path: &Path) -> Result<String, ConfigError> {
    read_credentials_at(path)?
        .get(provider)
        .and_then(|value| value.as_str())
        .map(str::to_owned)
        .ok_or_else(missing_credential)
}

/// Adds or replaces one stored credential in the default credentials file.
pub fn store_stored_credential(provider: &str, value: &str) -> Result<(), ConfigError> {
    let path = credentials_path().ok_or(ConfigError {
        kind: ConfigErrorKind::Io,
        message: "credentials could not be written",
    })?;
    store_stored_credential_at(&path, provider, value)
}

/// Adds or replaces one stored credential at an explicit path. The file is
/// parsed strictly first so malformed caller-owned data is never overwritten,
/// then replaced atomically with owner-only permissions. The value never
/// enters an error or a log.
pub fn store_stored_credential_at(
    path: &Path,
    provider: &str,
    value: &str,
) -> Result<(), ConfigError> {
    if !crate::model::is_id_text(provider)
        || value.is_empty()
        || value.len() > MAX_CREDENTIAL_BYTES
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(invalid("credential value is invalid"));
    }
    let mut object = read_credentials_at(path)?;
    object.insert(
        provider.to_owned(),
        serde_json::Value::String(value.to_owned()),
    );
    let text = serde_json::to_vec(&object).map_err(|_| invalid("credentials are invalid"))?;
    if text.len() > MAX_DOCUMENT_BYTES {
        return Err(invalid("credentials are too large"));
    }
    write_atomic(path, &text, "credentials could not be written")
}

/// Removes one stored credential from the default credentials file.
pub fn remove_stored_credential(provider: &str) -> Result<(), ConfigError> {
    let Some(path) = credentials_path() else {
        return Ok(());
    };
    remove_stored_credential_at(&path, provider)
}

/// Removes one stored credential at an explicit path. Missing files and
/// entries are already in the requested state. An empty document removes the
/// file instead of leaving an empty secret store behind.
pub fn remove_stored_credential_at(path: &Path, provider: &str) -> Result<(), ConfigError> {
    let mut object = read_credentials_at(path)?;
    if object.remove(provider).is_none() {
        return Ok(());
    }
    if object.is_empty() {
        return match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(ConfigError {
                kind: ConfigErrorKind::Io,
                message: "credentials could not be written",
            }),
        };
    }
    let text = serde_json::to_vec(&object).map_err(|_| invalid("credentials are invalid"))?;
    write_atomic(path, &text, "credentials could not be written")
}

/// Renders a redacted summary: identities, labels, references, and order,
/// but no secret values (the document cannot hold any).
#[must_use]
pub fn summary(config: &UserConfig) -> serde_json::Value {
    let mut result = serde_json::json!({
        "revision": CONFIG_REVISION,
        "providers": config.providers().iter().map(|profile| {
            let mut provider = serde_json::json!({
                "id": profile.id,
                "display_name": profile.display_name,
                "adapter": profile.adapter.name(),
                "protocol": profile.protocol.name(),
                "endpoint": profile.endpoint,
                "credential": profile.credential.to_json(),
                "default_model": profile.default_model,
                "context_window_tokens": profile.context_window_tokens,
            });
            if let Some(effort) = profile.task_reasoning_effort {
                provider["task_reasoning_effort"] = effort.as_str().into();
            }
            provider
        }).collect::<Vec<_>>(),
        "models": config.models().iter().map(|entry| {
            let mut model = serde_json::json!({
                "id": entry.id,
                "provider": entry.provider,
                "name": entry.name,
                "context_window_tokens": entry.context_window_tokens,
                "max_output_tokens": entry.max_output_tokens,
            });
            if let Some(effort) = entry.task_reasoning_effort {
                model["task_reasoning_effort"] = effort.as_str().into();
            }
            model
        }).collect::<Vec<_>>(),
        "favourites": config.favourites(),
        "recent": config.recent(),
    });
    if let Some(effort) = config.task_reasoning_effort() {
        result["task_reasoning_effort"] = effort.as_str().into();
    }
    result
}

#[cfg(test)]
mod credential_tests {
    use super::*;

    #[test]
    fn stored_credentials_are_strict_bounded_and_read_on_demand() {
        let path = std::env::temp_dir().join(format!(
            "nexus-credentials-test-{}-{}",
            std::process::id(),
            SAVE_NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        assert_eq!(
            resolve_stored_at("acme", &path).unwrap_err().kind(),
            ConfigErrorKind::MissingCredential
        );
        std::fs::write(&path, r#"{"acme":"secret-one"}"#).unwrap();
        assert_eq!(resolve_stored_at("acme", &path).unwrap(), "secret-one");
        std::fs::write(&path, r#"{"acme":"secret-two"}"#).unwrap();
        assert_eq!(resolve_stored_at("acme", &path).unwrap(), "secret-two");
        assert_eq!(
            resolve_stored_at("other", &path).unwrap_err().kind(),
            ConfigErrorKind::MissingCredential
        );
        for text in [
            r#"{"acme":"secret","acme":"other"}"#,
            r#"{"bad id":"secret"}"#,
            r#"{"acme":""}"#,
            r#"{"acme":3}"#,
            "[]",
            "not-json",
        ] {
            std::fs::write(&path, text).unwrap();
            let error = resolve_stored_at("acme", &path).unwrap_err();
            assert_eq!(error.kind(), ConfigErrorKind::Invalid);
            assert!(!error.to_string().contains("secret"));
        }
        std::fs::write(&path, vec![b' '; MAX_DOCUMENT_BYTES + 1]).unwrap();
        assert_eq!(
            resolve_stored_at("acme", &path).unwrap_err().kind(),
            ConfigErrorKind::Invalid
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn stored_credentials_are_updated_removed_and_never_overwrite_malformed_data() {
        let path = std::env::temp_dir().join(format!(
            "nexus-credentials-write-test-{}-{}",
            std::process::id(),
            SAVE_NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        store_stored_credential_at(&path, "acme", "secret-one").unwrap();
        assert_eq!(resolve_stored_at("acme", &path).unwrap(), "secret-one");
        store_stored_credential_at(&path, "acme", "secret-two").unwrap();
        store_stored_credential_at(&path, "other", "secret-three").unwrap();
        assert_eq!(resolve_stored_at("acme", &path).unwrap(), "secret-two");
        assert_eq!(resolve_stored_at("other", &path).unwrap(), "secret-three");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        for invalid in ["", "line\nbreak", "nul\0byte"] {
            assert_eq!(
                store_stored_credential_at(&path, "acme", invalid)
                    .unwrap_err()
                    .kind(),
                ConfigErrorKind::Invalid
            );
        }
        assert_eq!(resolve_stored_at("acme", &path).unwrap(), "secret-two");

        std::fs::write(&path, "not-json").unwrap();
        assert_eq!(
            store_stored_credential_at(&path, "acme", "replacement")
                .unwrap_err()
                .kind(),
            ConfigErrorKind::Invalid
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not-json");

        std::fs::write(&path, r#"{"acme":"one","other":"two"}"#).unwrap();
        remove_stored_credential_at(&path, "acme").unwrap();
        assert_eq!(resolve_stored_at("other", &path).unwrap(), "two");
        remove_stored_credential_at(&path, "other").unwrap();
        assert!(!path.exists(), "an empty credential store is removed");
        remove_stored_credential_at(&path, "other").unwrap();
    }
}
