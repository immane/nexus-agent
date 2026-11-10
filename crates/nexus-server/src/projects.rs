//! Operator-managed project roots selectable by web-created sessions.
//!
//! A project is an existing canonical directory that local file and command
//! tools may use as their project root. The web UI can add and remove entries
//! with the server bearer token, so every path is validated before admission:
//! the filesystem root, the user's home or any ancestor of it, and known
//! credential directories are refused. Canonicalization is not secret
//! detection; a project may still contain sensitive files.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value};

/// File stored next to the user configuration.
pub const PROJECTS_FILE_NAME: &str = "projects.json";
/// Maximum projects retained in one catalog.
pub const MAX_PROJECTS: usize = 32;
/// Maximum display-name length in bytes.
pub const MAX_PROJECT_NAME_BYTES: usize = 128;
/// Maximum serialized path length in bytes.
pub const MAX_PROJECT_PATH_BYTES: usize = 4096;
/// Parse budget for the catalog document.
pub const MAX_DOCUMENT_BYTES: usize = 32_768;
/// Reserved id for the startup `--tools-root` workspace.
pub const DEFAULT_PROJECT_ID: &str = "default";

static SAVE_NONCE: AtomicU64 = AtomicU64::new(0);

/// A selectable project root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// Stable server-generated identity.
    pub id: String,
    /// Operator-facing label.
    pub name: String,
    /// Canonical existing directory.
    pub path: PathBuf,
}

impl Project {
    /// Returns the path as bounded UTF-8. Canonical paths admitted by this
    /// module are always UTF-8; deserialized entries are revalidated.
    #[must_use]
    pub fn path_text(&self) -> &str {
        self.path.to_str().unwrap_or("")
    }
}

/// Static project-catalog failure. Messages never echo caller paths or names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectErrorKind {
    /// Invalid name, path, or document.
    Invalid,
    /// Duplicate path, capacity, or still-referenced entry.
    Conflict,
    /// Filesystem read/write failure.
    Io,
}

/// Project error with a static diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectError {
    kind: ProjectErrorKind,
    message: &'static str,
}

impl ProjectError {
    /// Returns the failure class.
    #[must_use]
    pub fn kind(&self) -> ProjectErrorKind {
        self.kind
    }

    /// Returns the static diagnostic.
    #[must_use]
    pub fn message(&self) -> &'static str {
        self.message
    }
}

impl std::fmt::Display for ProjectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ProjectError {}

fn error(kind: ProjectErrorKind, message: &'static str) -> ProjectError {
    ProjectError { kind, message }
}

fn invalid(message: &'static str) -> ProjectError {
    error(ProjectErrorKind::Invalid, message)
}

/// Persisted project list. The startup workspace is synthesized by the
/// server and never stored here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCatalog {
    projects: Vec<Project>,
    next: u64,
}

impl Default for ProjectCatalog {
    fn default() -> Self {
        Self {
            projects: Vec::new(),
            next: 1,
        }
    }
}

impl ProjectCatalog {
    /// Returns admitted projects in declaration order.
    #[must_use]
    pub fn projects(&self) -> &[Project] {
        &self.projects
    }

    /// Finds one project by id.
    #[must_use]
    pub fn find(&self, id: &str) -> Option<&Project> {
        self.projects.iter().find(|project| project.id == id)
    }

    /// Adds a canonical existing directory. Duplicate canonical paths are
    /// refused so two labels cannot accidentally describe the same root.
    pub fn add(&mut self, name: &str, path: &Path) -> Result<Project, ProjectError> {
        let name = name.trim();
        if name.is_empty()
            || name.len() > MAX_PROJECT_NAME_BYTES
            || name.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(invalid("project name is invalid"));
        }
        let path = validate_project_root(path)?;
        if self.projects.iter().any(|project| project.path == path) {
            return Err(error(
                ProjectErrorKind::Conflict,
                "project path is already registered",
            ));
        }
        if self.projects.len() >= MAX_PROJECTS {
            return Err(error(ProjectErrorKind::Conflict, "project list is full"));
        }
        let id = self
            .mint_id()
            .ok_or_else(|| error(ProjectErrorKind::Conflict, "project list is full"))?;
        let project = Project {
            id,
            name: name.to_owned(),
            path,
        };
        self.projects.push(project.clone());
        Ok(project)
    }

    /// Removes a project by id. Existing sessions are checked by the server,
    /// which owns session lifetime.
    pub fn remove(&mut self, id: &str) -> Result<(), ProjectError> {
        let before = self.projects.len();
        self.projects.retain(|project| project.id != id);
        if self.projects.len() == before {
            return Err(invalid("project is unknown"));
        }
        Ok(())
    }

    fn mint_id(&mut self) -> Option<String> {
        loop {
            if self.next == u64::MAX {
                return None;
            }
            let id = format!("project-{}", self.next);
            self.next += 1;
            if !self.projects.iter().any(|project| project.id == id) {
                return Some(id);
            }
        }
    }

    /// Serializes the catalog document. No caller text is ever interpolated
    /// into an error; values are escaped by the JSON encoder.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut document = Map::new();
        document.insert("revision".to_owned(), Value::from(1));
        document.insert(
            "projects".to_owned(),
            Value::Array(
                self.projects
                    .iter()
                    .map(|project| {
                        let mut object = Map::new();
                        object.insert("id".to_owned(), Value::String(project.id.clone()));
                        object.insert("name".to_owned(), Value::String(project.name.clone()));
                        object.insert(
                            "path".to_owned(),
                            Value::String(project.path_text().to_owned()),
                        );
                        Value::Object(object)
                    })
                    .collect(),
            ),
        );
        Value::Object(document).to_string()
    }

    /// Parses and revalidates every stored project root. Unknown fields,
    /// duplicate ids, invalid ids, and unavailable roots are explicit errors.
    pub fn from_json(text: &str) -> Result<Self, ProjectError> {
        if text.len() > MAX_DOCUMENT_BYTES {
            return Err(invalid("project document is too large"));
        }
        let value: Value =
            serde_json::from_str(text).map_err(|_| invalid("project document is invalid"))?;
        let document = value
            .as_object()
            .ok_or_else(|| invalid("project document is invalid"))?;
        if document
            .keys()
            .any(|key| !matches!(key.as_str(), "revision" | "projects"))
        {
            return Err(invalid("project document is invalid"));
        }
        if document.get("revision").and_then(Value::as_u64) != Some(1) {
            return Err(invalid("project document revision is unsupported"));
        }
        let entries = document
            .get("projects")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("project document is invalid"))?;
        if entries.len() > MAX_PROJECTS {
            return Err(invalid("project document is invalid"));
        }
        let mut catalog = Self::default();
        let mut max_sequence = 0u64;
        for entry in entries {
            let object = entry
                .as_object()
                .ok_or_else(|| invalid("project document is invalid"))?;
            if object
                .keys()
                .any(|key| !matches!(key.as_str(), "id" | "name" | "path"))
            {
                return Err(invalid("project document is invalid"));
            }
            let id = object
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("project document is invalid"))?;
            let sequence = id
                .strip_prefix("project-")
                .and_then(|digits| {
                    (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
                        .then(|| digits.parse::<u64>().ok())
                        .flatten()
                })
                .ok_or_else(|| invalid("project document is invalid"))?;
            if sequence == 0 || catalog.projects.iter().any(|project| project.id == id) {
                return Err(invalid("project document is invalid"));
            }
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("project document is invalid"))?;
            let name = name.trim();
            if name.is_empty()
                || name.len() > MAX_PROJECT_NAME_BYTES
                || name.bytes().any(|byte| byte.is_ascii_control())
            {
                return Err(invalid("project document is invalid"));
            }
            let path = object
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("project document is invalid"))?;
            if path.len() > MAX_PROJECT_PATH_BYTES {
                return Err(invalid("project document is invalid"));
            }
            let path = validate_project_root(Path::new(path))?;
            if catalog.projects.iter().any(|project| project.path == path) {
                return Err(invalid("project document is invalid"));
            }
            catalog.projects.push(Project {
                id: id.to_owned(),
                name: name.to_owned(),
                path,
            });
            max_sequence = max_sequence.max(sequence);
        }
        catalog.next = max_sequence.saturating_add(1).max(1);
        Ok(catalog)
    }

    /// Loads a catalog, treating a missing file as an empty catalog.
    pub fn load(path: &Path) -> Result<Option<Self>, ProjectError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                return Err(error(
                    ProjectErrorKind::Io,
                    "project list could not be read",
                ));
            }
        };
        Self::from_json(&text).map(Some)
    }

    /// Atomically writes the catalog with owner-only permissions on Unix.
    pub fn save(&self, path: &Path) -> Result<(), ProjectError> {
        let io_error = || error(ProjectErrorKind::Io, "project list could not be written");
        let parent = path.parent().ok_or_else(io_error)?;
        let pid = std::process::id();
        let mut attempts = 0;
        loop {
            let nonce = SAVE_NONCE.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0);
            let temporary = parent.join(format!(".nexus-projects-{pid}-{nanos}-{nonce}.tmp"));
            let mut file = match create_temp(&temporary) {
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempts < 8 => {
                    attempts += 1;
                    continue;
                }
                Err(_) => return Err(io_error()),
                Ok(file) => file,
            };
            if file.write_all(self.to_json().as_bytes()).is_err()
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
}

/// Returns the catalog path next to a configuration document.
#[must_use]
pub fn path_near_config(config_path: &Path) -> PathBuf {
    config_path.with_file_name(PROJECTS_FILE_NAME)
}

/// Canonicalizes and validates a project root. The home directory, any
/// ancestor of it, the filesystem root, and known credential directories are
/// refused. This is a bounded access decision, not secret detection.
pub fn validate_project_root(path: &Path) -> Result<PathBuf, ProjectError> {
    if !path.is_absolute() {
        return Err(invalid("project path must be absolute"));
    }
    let canonical =
        std::fs::canonicalize(path).map_err(|_| invalid("project path is unavailable"))?;
    let metadata =
        std::fs::metadata(&canonical).map_err(|_| invalid("project path is unavailable"))?;
    if !metadata.is_dir() {
        return Err(invalid("project path must be a directory"));
    }
    if canonical.to_str().is_none()
        || canonical
            .to_str()
            .is_some_and(|text| text.len() > MAX_PROJECT_PATH_BYTES)
    {
        return Err(invalid("project path is invalid"));
    }
    if canonical.as_os_str() == "/" {
        return Err(invalid("project path is too broad"));
    }
    for broad in [
        "/etc",
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
        "/boot",
        "/dev",
        "/proc",
        "/sys",
        "/var",
        "/System",
        "/Library",
        "/Applications",
    ] {
        if std::fs::canonicalize(broad).is_ok_and(|broad| canonical == broad) {
            return Err(invalid("project path is too broad"));
        }
    }
    if let Some(home) = std::env::var_os("HOME").and_then(|home| std::fs::canonicalize(home).ok()) {
        if canonical == home || home.starts_with(&canonical) {
            return Err(invalid("project path is too broad"));
        }
        for relative in [
            ".ssh",
            ".aws",
            ".gnupg",
            ".azure",
            ".kube",
            ".netrc",
            ".npmrc",
            ".pypirc",
            ".config/gcloud",
            ".config/gh",
        ] {
            let protected = home.join(relative);
            if let Ok(protected) = std::fs::canonicalize(&protected)
                && (canonical == protected || protected.starts_with(&canonical))
            {
                return Err(invalid("project path contains protected credentials"));
            }
        }
    }
    Ok(canonical)
}

#[cfg(unix)]
fn create_temp(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_temp(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(unix)]
fn restrict_owner_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_owner_only(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_directory(label: &str) -> PathBuf {
        let nonce = SAVE_NONCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nexus-projects-{label}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn paths_are_canonical_bounded_and_not_broad_roots() {
        let root = temp_directory("validate");
        let project = root.join("workspace");
        std::fs::create_dir_all(&project).unwrap();
        let canonical = std::fs::canonicalize(&project).unwrap();
        assert_eq!(validate_project_root(&project).unwrap(), canonical);
        assert!(validate_project_root(Path::new("/")).is_err());
        if Path::new("/etc").is_dir() {
            assert!(validate_project_root(Path::new("/etc")).is_err());
        }
        assert!(validate_project_root(Path::new("relative")).is_err());
        assert!(validate_project_root(&root.join("missing")).is_err());
        assert!(validate_project_root(&project.join("file")).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn catalog_adds_removes_and_round_trips() {
        let root = temp_directory("catalog");
        let one = root.join("one");
        let two = root.join("two");
        std::fs::create_dir_all(&one).unwrap();
        std::fs::create_dir_all(&two).unwrap();
        let mut catalog = ProjectCatalog::default();
        let first = catalog.add("First", &one).unwrap();
        assert_eq!(first.id, "project-1");
        assert_eq!(first.name, "First");
        assert_eq!(
            catalog.add("Again", &one).unwrap_err().kind(),
            ProjectErrorKind::Conflict
        );
        let second = catalog.add("Second", &two).unwrap();
        assert_eq!(second.id, "project-2");
        assert_eq!(catalog.projects().len(), 2);

        let encoded = catalog.to_json();
        let decoded = ProjectCatalog::from_json(&encoded).unwrap();
        assert_eq!(decoded, catalog);

        let path = root.join("projects.json");
        catalog.save(&path).unwrap();
        assert_eq!(ProjectCatalog::load(&path).unwrap(), Some(catalog.clone()));
        catalog.remove(&second.id).unwrap();
        assert!(catalog.find(&second.id).is_none());
        assert!(catalog.remove(&second.id).is_err());
        assert_eq!(
            ProjectCatalog::load(&root.join("missing.json")).unwrap(),
            None
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_documents_are_rejected() {
        for text in [
            "not-json",
            "[]",
            r#"{"revision":2,"projects":[]}"#,
            r#"{"revision":1,"projects":[{"id":"bad","name":"x","path":"/"}]}"#,
            r#"{"revision":1,"projects":[{"id":"project-1","name":"","path":"/"}]}"#,
            r#"{"revision":1,"projects":[],"extra":true}"#,
        ] {
            assert!(ProjectCatalog::from_json(text).is_err(), "{text}");
        }
    }
}
