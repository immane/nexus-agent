//! Bounded, descriptor-relative reads for the Web project's image previews.

use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

use nexus_permissions::DirectoryPolicy;

use crate::fs_access::BoundRoot;

const MAX_IMAGE_BYTES: u64 = 4 * 1024 * 1024;

/// Reads only supported image files from one pinned project root.
pub struct ProjectImageReader {
    root: BoundRoot,
    policy: DirectoryPolicy,
}

impl ProjectImageReader {
    /// Binds an already admitted canonical directory without accepting a new target.
    pub fn bind(root: &Path) -> Option<Self> {
        let policy = DirectoryPolicy::new(root).ok()?;
        let bound = BoundRoot::open(root).ok()?;
        if policy.root() != root || bound.path() != root {
            return None;
        }
        Some(Self {
            root: bound,
            policy,
        })
    }

    /// Returns bounded bytes for a regular image in this project, or no preview.
    /// SVG bytes are untrusted and must only be decoded in an image context.
    pub fn read(&self, raw: &str) -> Option<Vec<u8>> {
        if raw.is_empty() || raw.len() > 4096 || raw.chars().any(char::is_control) {
            return None;
        }
        let path = Path::new(raw);
        let relative = if path.is_absolute() {
            path.strip_prefix(self.policy.root()).ok()?
        } else {
            path.strip_prefix(".").unwrap_or(path)
        };
        if !relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        {
            return None;
        }
        let extension = relative.extension()?.to_str()?.to_ascii_lowercase();
        if !matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "webp" | "svg") {
            return None;
        }
        let path = self.policy.root().join(relative);
        if self.policy.is_protected(&path) {
            return None;
        }
        let file = self.root.open_file(&path).ok()?;
        let metadata = file.metadata().ok()?;
        if metadata.nlink() != 1 || metadata.len() > MAX_IMAGE_BYTES {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(MAX_IMAGE_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.is_empty() || bytes.len() as u64 > MAX_IMAGE_BYTES {
            return None;
        }
        let matches_image = match extension.as_str() {
            "png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            "jpg" | "jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
            "webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
            "svg" => std::str::from_utf8(&bytes).is_ok_and(|text| {
                let text = text.trim_start_matches('\u{feff}').trim_start();
                text.starts_with('<') && text.contains("<svg")
            }),
            _ => false,
        };
        matches_image.then_some(bytes)
    }
}
