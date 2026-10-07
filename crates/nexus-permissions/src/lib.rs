//! Shared filesystem boundaries for runtime authorization and real adapters.
//! Canonicalization is not a race-free filesystem capability.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

/// Immutable host-configured development boundary. Session grants belong to
/// the runtime, not this object or a process-global permission store.
#[derive(Debug, Clone)]
pub struct DirectoryPolicy {
    root: PathBuf,
    temporary: Vec<PathBuf>,
    protected: Vec<PathBuf>,
}

impl DirectoryPolicy {
    pub fn new(root: &Path) -> Result<Self, &'static str> {
        let root = root
            .canonicalize()
            .map_err(|_| "permissions root is invalid")?;
        if !root.is_dir() || root.to_str().is_none() {
            return Err("permissions root is invalid");
        }
        let mut temporary = Vec::new();
        for path in [PathBuf::from("/tmp"), std::env::temp_dir()] {
            if let Ok(path) = path.canonicalize()
                && !temporary.contains(&path)
            {
                temporary.push(path);
            }
        }
        let mut protected = Vec::new();
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("permissions home is unavailable")?;
        if !home.is_absolute() || !home.is_dir() {
            return Err("permissions home is invalid");
        }
        let canonical_home = home
            .canonicalize()
            .map_err(|_| "permissions home is invalid")?;
        for home in [&home, &canonical_home] {
            for name in [
                ".ssh",
                ".aws",
                ".gnupg",
                ".azure",
                ".config/gcloud",
                ".config/gh",
                ".kube",
                ".netrc",
                ".npmrc",
                ".pypirc",
            ] {
                let path = home.join(name);
                protected.push(path.clone());
                protected.push(path.canonicalize().unwrap_or(path));
            }
        }
        for base in [&root, &home, &canonical_home] {
            for name in [
                ".env",
                ".env.local",
                ".env.production",
                ".env.development",
                ".env.test",
            ] {
                let path = base.join(name);
                protected.push(path.clone());
                if let Ok(target) = path.canonicalize() {
                    protected.push(target);
                }
            }
        }
        protected.sort();
        protected.dedup();
        if protected
            .iter()
            .chain(&temporary)
            .any(|path| path.to_str().is_none())
        {
            return Err("permissions paths must be UTF-8");
        }
        if protected.iter().any(|path| root.starts_with(path)) {
            return Err("permissions root is protected");
        }
        Ok(Self {
            root,
            temporary,
            protected,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn temporary(&self) -> &[PathBuf] {
        &self.temporary
    }
    pub fn protected(&self) -> &[PathBuf] {
        &self.protected
    }

    /// Existing targets resolve through symlinks; new files resolve through
    /// an existing canonical parent, matching the writer's parent requirement.
    pub fn resolve(&self, raw: &str) -> Result<PathBuf, &'static str> {
        if raw.is_empty() || raw.contains('\0') {
            return Err("tool path is invalid");
        }
        let raw = Path::new(raw);
        let joined = if raw.is_absolute() {
            raw.to_owned()
        } else {
            self.root.join(raw)
        };
        let path = match joined.canonicalize() {
            Ok(path) => path,
            Err(_) => {
                if joined.symlink_metadata().is_ok() {
                    return Err("tool path cannot be resolved");
                }
                let name = joined.file_name().ok_or("tool path cannot be resolved")?;
                let parent = joined
                    .parent()
                    .ok_or("tool path cannot be resolved")?
                    .canonicalize()
                    .map_err(|_| "tool path cannot be resolved")?;
                parent.join(name)
            }
        };
        if path.to_str().is_none() {
            return Err("tool path must be UTF-8");
        }
        if self.is_protected(&joined) || self.is_protected(&path) {
            return Err("sensitive path is not available to tools");
        }
        Ok(path)
    }

    pub fn is_protected(&self, path: &Path) -> bool {
        self.protected.iter().any(|denied| path.starts_with(denied))
    }

    pub fn is_automatic(&self, path: &Path) -> bool {
        !self.is_protected(path)
            && (path.starts_with(&self.root)
                || self.temporary.iter().any(|root| path.starts_with(root)))
    }

    pub fn directory(&self, path: &Path) -> Result<PathBuf, &'static str> {
        let directory = if path.is_dir() {
            path
        } else {
            path.parent().ok_or("permission directory is invalid")?
        };
        if directory.parent().is_none() || self.is_protected(directory) {
            return Err("permission directory is invalid");
        }
        Ok(directory.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn canonical_paths_distinguish_project_temp_external_and_protected_targets() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let policy = DirectoryPolicy::new(root).unwrap();
        assert!(policy.is_automatic(&policy.resolve("src/lib.rs").unwrap()));
        assert!(policy.is_automatic(&policy.resolve("new-file.txt").unwrap()));
        let outside = policy.resolve("/etc/hosts").unwrap();
        assert!(!outside.starts_with(policy.root()));
        assert!(!policy.is_automatic(&outside));
        assert_eq!(
            policy.directory(&outside).unwrap(),
            outside.parent().unwrap()
        );
        assert!(
            policy
                .temporary()
                .iter()
                .all(|path| policy.is_automatic(path))
        );
        assert!(policy.resolve(".env").is_err());
        assert!(policy.resolve("missing-parent/new-file").is_err());
        assert!(policy.resolve("\0").is_err());
        assert!(policy.directory(Path::new("/")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_targets_are_canonical_and_dangling_links_are_rejected() {
        let root =
            std::env::temp_dir().join(format!("nexus-permissions-links-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let outside = Path::new("/etc/hosts").canonicalize().unwrap();
        std::os::unix::fs::symlink(&outside, root.join("outside")).unwrap();
        std::os::unix::fs::symlink(root.join("missing"), root.join("dangling")).unwrap();
        let policy = DirectoryPolicy::new(&root).unwrap();
        assert_eq!(policy.resolve("outside").unwrap(), outside);
        assert!(!policy.is_automatic(&outside));
        assert!(policy.resolve("dangling").is_err());
    }
}
