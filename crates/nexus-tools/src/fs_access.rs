//! Descriptor-bound access to a canonical tool root.
//!
//! Callers resolve the requested path (including existing symlinks) and check
//! its scope before entering this module. This owner pins the configured root
//! and walks the canonical relative path with no-follow directory opens, so a
//! later symlink swap cannot redirect the operation. The descriptor walk is
//! available on both supported Unix platforms and fails closed on a changed
//! component.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::{Mode, OFlags, RenameFlags, open, openat, renameat_with, unlinkat};

#[cfg(target_os = "linux")]
const MAX_XATTR_BYTES: usize = 1_048_576;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

pub(crate) struct BoundRoot {
    path: PathBuf,
    directory: File,
    replacement_metadata: ReplacementMetadata,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplacementMetadata {
    RequireAll,
    AllowUnsupported,
}

impl BoundRoot {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        Self::open_with_replacement_metadata(path, ReplacementMetadata::RequireAll)
    }

    pub(crate) fn open_container_workspace(path: &Path) -> io::Result<Self> {
        Self::open_with_replacement_metadata(path, ReplacementMetadata::AllowUnsupported)
    }

    fn open_with_replacement_metadata(
        path: &Path,
        replacement_metadata: ReplacementMetadata,
    ) -> io::Result<Self> {
        let canonical = std::fs::canonicalize(path)?;
        if !canonical.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "tool root is not a directory",
            ));
        }
        let expected = std::fs::metadata(&canonical)?;
        let directory = open_absolute_directory(&canonical)?;
        let actual = directory.metadata()?;
        use std::os::unix::fs::MetadataExt;
        if expected.dev() != actual.dev() || expected.ino() != actual.ino() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "tool root changed while opening",
            ));
        }
        Ok(Self {
            path: canonical,
            directory,
            replacement_metadata,
        })
    }

    pub(crate) fn replacement_metadata(&self) -> ReplacementMetadata {
        self.replacement_metadata
    }

    pub(crate) fn open_file(&self, path: &Path) -> io::Result<File> {
        self.open_relative(path, OFlags::RDONLY | OFlags::NONBLOCK)
    }

    pub(crate) fn open_directory(&self, path: &Path) -> io::Result<File> {
        self.open_relative(path, OFlags::RDONLY | OFlags::DIRECTORY)
    }

    fn open_relative(&self, path: &Path, flags: OFlags) -> io::Result<File> {
        let relative = path.strip_prefix(&self.path).map_err(|_| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "path escapes the tool root",
            )
        })?;
        let components = relative
            .components()
            .map(|component| match component {
                Component::Normal(name) => Ok(name),
                _ => Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "path is not canonical",
                )),
            })
            .collect::<io::Result<Vec<_>>>()?;
        if components.is_empty() {
            if flags.contains(OFlags::DIRECTORY) {
                return self.directory.try_clone();
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a file path is required",
            ));
        }

        let mut parent = self.directory.try_clone()?;
        for component in &components[..components.len() - 1] {
            let fd = openat(
                &parent,
                *component,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map_err(io::Error::from)?;
            parent = File::from(fd);
        }
        let fd = openat(
            &parent,
            components[components.len() - 1],
            flags | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        let file = File::from(fd);
        let metadata = file.metadata()?;
        if flags.contains(OFlags::DIRECTORY) {
            if !metadata.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "target is not a directory",
                ));
            }
        } else if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "target is not a regular file",
            ));
        }
        Ok(file)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileStamp {
    device: u64,
    inode: u64,
    links: u64,
    size: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl FileStamp {
    pub(crate) fn read(file: &File) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            links: metadata.nlink(),
            size: metadata.len(),
            mode: metadata.mode(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            mtime: (metadata.mtime(), metadata.mtime_nsec()),
            ctime: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }

    fn validate_replaceable(self) -> io::Result<Self> {
        if self.links != 1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "files with multiple hard links cannot be replaced",
            ));
        }
        Ok(self)
    }
}

pub(crate) fn create_new(
    parent: &File,
    name: &str,
    content: &[u8],
    cancelled: impl Fn() -> bool,
) -> io::Result<()> {
    let cleanup_parent = parent.try_clone()?;
    let temporary = create_temporary(parent)?;
    let mut cleanup = TempCleanup::new(cleanup_parent, temporary.name.clone());
    let mut file = temporary.file;
    file.write_all(content)?;
    if cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "operation cancelled",
        ));
    }
    renameat_with(
        parent,
        cleanup.name.as_str(),
        parent,
        name,
        RenameFlags::NOREPLACE,
    )
    .map_err(io::Error::from)?;
    cleanup.committed = true;
    Ok(())
}

pub(crate) fn replace_existing(
    parent: &File,
    name: &str,
    source: &File,
    expected: FileStamp,
    content: &[u8],
    replacement_metadata: ReplacementMetadata,
    cancelled: impl Fn() -> bool,
) -> io::Result<()> {
    let expected = expected.validate_replaceable()?;
    if FileStamp::read(source)? != expected {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "file changed while preparing replacement",
        ));
    }
    let cleanup_parent = parent.try_clone()?;
    let temporary = create_replacement_temporary(parent, source)?;
    let mut cleanup = TempCleanup::new(cleanup_parent, temporary.name.clone());
    let mut replacement = temporary.file;
    replacement.set_len(0)?;
    replacement.write_all(content)?;
    preserve_metadata(source, &replacement, expected, replacement_metadata)?;
    if FileStamp::read(source)? != expected {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "file changed while preparing replacement",
        ));
    }
    if cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "operation cancelled",
        ));
    }
    let current = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let current = File::from(current);
    if FileStamp::read(&current)? != expected {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "file changed before replacement",
        ));
    }
    renameat_with(
        parent,
        cleanup.name.as_str(),
        parent,
        name,
        RenameFlags::empty(),
    )
    .map_err(io::Error::from)?;
    cleanup.committed = true;
    Ok(())
}

struct Temporary {
    name: String,
    file: File,
}

fn create_temporary(parent: &File) -> io::Result<Temporary> {
    for _ in 0..32 {
        let name = format!(
            ".nexus-write-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        );
        match openat(
            parent,
            name.as_str(),
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o666),
        ) {
            Ok(file) => {
                return Ok(Temporary {
                    name,
                    file: File::from(file),
                });
            }
            Err(rustix::io::Errno::EXIST) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "temporary file name space exhausted",
    ))
}

#[cfg(target_os = "macos")]
fn create_replacement_temporary(parent: &File, source: &File) -> io::Result<Temporary> {
    for _ in 0..32 {
        let name = format!(
            ".nexus-write-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        );
        match rustix::fs::fclonefileat(
            source,
            parent,
            name.as_str(),
            rustix::fs::CloneFlags::empty(),
        ) {
            Ok(()) => {
                let fd = match openat(
                    parent,
                    name.as_str(),
                    OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                    Mode::empty(),
                ) {
                    Ok(fd) => fd,
                    Err(error) => {
                        let _ = unlinkat(parent, name.as_str(), rustix::fs::AtFlags::empty());
                        return Err(error.into());
                    }
                };
                return Ok(Temporary {
                    name,
                    file: File::from(fd),
                });
            }
            Err(rustix::io::Errno::EXIST) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "temporary file name space exhausted",
    ))
}

#[cfg(not(target_os = "macos"))]
fn create_replacement_temporary(parent: &File, _source: &File) -> io::Result<Temporary> {
    create_temporary(parent)
}

struct TempCleanup {
    parent: File,
    name: String,
    committed: bool,
}

impl TempCleanup {
    fn new(parent: File, name: String) -> Self {
        Self {
            parent,
            name,
            committed: false,
        }
    }
}

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if !self.committed {
            let _ = unlinkat(
                &self.parent,
                self.name.as_str(),
                rustix::fs::AtFlags::empty(),
            );
        }
    }
}

#[cfg(target_os = "macos")]
fn preserve_metadata(
    _source: &File,
    _replacement: &File,
    _stamp: FileStamp,
    _metadata_policy: ReplacementMetadata,
) -> io::Result<()> {
    // `fclonefileat` creates the staged copy with the source file's metadata,
    // ACLs, extended attributes, and platform flags. Unsupported filesystems
    // fail the clone and therefore fail closed.
    Ok(())
}

#[cfg(target_os = "linux")]
fn preserve_metadata(
    source: &File,
    replacement: &File,
    stamp: FileStamp,
    metadata_policy: ReplacementMetadata,
) -> io::Result<()> {
    use rustix::buffer::spare_capacity;
    use rustix::fs::{
        Gid, IFlags, Uid, XattrFlags, fchmod, fchown, fgetxattr, flistxattr, fsetxattr,
        ioctl_getflags, ioctl_setflags,
    };

    fchown(
        replacement,
        Some(Uid::from_raw(stamp.uid)),
        Some(Gid::from_raw(stamp.gid)),
    )
    .map_err(io::Error::from)?;

    let mut names = Vec::with_capacity(MAX_XATTR_BYTES);
    let names_len = match flistxattr(source, spare_capacity(&mut names)) {
        Ok(length) => Some(length),
        Err(error) if unsupported_metadata_error(error) => {
            if metadata_policy == ReplacementMetadata::AllowUnsupported {
                None
            } else {
                return Err(error.into());
            }
        }
        Err(error) => return Err(error.into()),
    };
    if names_len.is_some_and(|length| length > MAX_XATTR_BYTES) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file metadata exceeds its bound",
        ));
    }
    if names_len.is_some() {
        for raw_name in names
            .as_slice()
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
        {
            let name = std::str::from_utf8(raw_name).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "unsupported attribute name")
            })?;
            if name == "security.capability" {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "capability-bearing files cannot be replaced",
                ));
            }
            let mut value = Vec::with_capacity(MAX_XATTR_BYTES);
            let value_len =
                fgetxattr(source, name, spare_capacity(&mut value)).map_err(io::Error::from)?;
            if value_len > MAX_XATTR_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file metadata exceeds its bound",
                ));
            }
            fsetxattr(replacement, name, &value, XattrFlags::empty()).map_err(io::Error::from)?;
        }
    }
    fchmod(replacement, Mode::from_raw_mode(stamp.mode & 0o7777)).map_err(io::Error::from)?;
    let Some(flags) = copyable_file_flags(ioctl_getflags(source), metadata_policy)? else {
        // When an explicitly configured container workspace uses a filesystem
        // that exposes no Linux inode flags, retain atomic replacement and let
        // that filesystem enforce its own rename rules. Host tools stay strict.
        return Ok(());
    };
    if flags.intersects(IFlags::IMMUTABLE | IFlags::APPEND) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "immutable or append-only files cannot be replaced",
        ));
    }
    ioctl_setflags(replacement, flags).map_err(io::Error::from)
}

#[cfg(target_os = "linux")]
fn copyable_file_flags(
    result: Result<rustix::fs::IFlags, rustix::io::Errno>,
    policy: ReplacementMetadata,
) -> io::Result<Option<rustix::fs::IFlags>> {
    match result {
        Ok(flags) => Ok(Some(flags)),
        Err(error) if unsupported_metadata_error(error) => {
            if policy == ReplacementMetadata::AllowUnsupported {
                Ok(None)
            } else {
                Err(error.into())
            }
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(target_os = "linux")]
fn unsupported_metadata_error(error: rustix::io::Errno) -> bool {
    matches!(
        error,
        rustix::io::Errno::NOTTY | rustix::io::Errno::OPNOTSUPP
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn preserve_metadata(
    _source: &File,
    _replacement: &File,
    _stamp: FileStamp,
    _metadata_policy: ReplacementMetadata,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "file replacement metadata is unsupported on this platform",
    ))
}

fn open_absolute_directory(path: &Path) -> io::Result<File> {
    let root_fd = open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let mut directory = File::from(root_fd);
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let fd = openat(
            &directory,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        directory = File::from(fd);
    }
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::{BoundRoot, FileStamp, ReplacementMetadata, create_new, replace_existing};
    use std::io;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "nexus-fs-access-{}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).expect("test root creates");
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn cancelled_new_file_publish_cleans_the_stage() {
        let root = TempRoot::new();
        let bound = BoundRoot::open(&root.0).expect("root binds");
        let result = create_new(&bound.directory, "new.txt", b"content", || true);
        assert_eq!(
            result.expect_err("cancelled before publish").kind(),
            io::ErrorKind::Interrupted
        );
        assert!(!root.0.join("new.txt").exists());
        assert_eq!(
            std::fs::read_dir(&root.0).expect("directory reads").count(),
            0
        );
    }

    #[test]
    fn container_roots_allow_only_explicitly_unsupported_replacement_metadata() {
        let root = TempRoot::new();
        assert_eq!(
            BoundRoot::open(&root.0)
                .expect("strict root binds")
                .replacement_metadata(),
            ReplacementMetadata::RequireAll
        );
        assert_eq!(
            BoundRoot::open_container_workspace(&root.0)
                .expect("container root binds")
                .replacement_metadata(),
            ReplacementMetadata::AllowUnsupported
        );
    }

    #[test]
    fn deadline_expiring_after_new_file_staging_prevents_publish() {
        let root = TempRoot::new();
        let bound = BoundRoot::open(&root.0).expect("root binds");
        let result = create_new(&bound.directory, "new.txt", b"staged content", || {
            // The callback runs after the temporary file has been written and
            // immediately before publication. Simulate a deadline expiring
            // during staging, without relying on wall-clock timing.
            assert!(
                std::fs::read_dir(&root.0)
                    .expect("directory reads")
                    .next()
                    .is_some(),
                "staging has completed before the activity check"
            );
            true
        });
        assert_eq!(
            result
                .expect_err("expired operation must not publish")
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert!(!root.0.join("new.txt").exists());
        assert_eq!(
            std::fs::read_dir(&root.0).expect("directory reads").count(),
            0,
            "the staged temporary is cleaned up"
        );
    }

    #[test]
    fn new_file_publish_never_clobbers_a_racing_creator() {
        let root = TempRoot::new();
        let bound = BoundRoot::open(&root.0).expect("root binds");
        let target = bound.path.join("new.txt");
        let result = create_new(&bound.directory, "new.txt", b"staged", || {
            std::fs::write(&target, b"concurrent creator").expect("concurrent create succeeds");
            false
        });
        assert_eq!(
            result
                .expect_err("no-clobber publish refuses the existing name")
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            std::fs::read(&target).expect("concurrent file remains"),
            b"concurrent creator"
        );
        assert_eq!(
            std::fs::read_dir(&root.0).expect("directory reads").count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn swapped_parent_symlink_cannot_escape_the_bound_root() {
        let root = TempRoot::new();
        let outside = TempRoot::new();
        std::fs::create_dir(root.0.join("sub")).expect("child directory creates");
        std::fs::write(root.0.join("sub/secret.txt"), b"inside").expect("inside file writes");
        std::fs::write(outside.0.join("secret.txt"), b"outside").expect("outside file writes");
        let bound = BoundRoot::open(&root.0).expect("root binds");

        std::fs::rename(root.0.join("sub"), root.0.join("old-sub"))
            .expect("original directory moves");
        std::os::unix::fs::symlink(&outside.0, root.0.join("sub"))
            .expect("attacker replaces parent with symlink");

        assert!(bound.open_file(&bound.path.join("sub/secret.txt")).is_err());
        assert_eq!(
            std::fs::read(outside.0.join("secret.txt")).expect("outside survives"),
            b"outside"
        );
    }

    #[test]
    fn changed_source_is_not_overwritten_after_staging() {
        let root = TempRoot::new();
        let bound = BoundRoot::open(&root.0).expect("root binds");
        let path = bound.path.join("file.txt");
        std::fs::write(&path, b"before").expect("fixture writes");
        let source = bound.open_file(&path).expect("source opens");
        let stamp = FileStamp::read(&source).expect("source stamps");
        let result = replace_existing(
            &bound.directory,
            "file.txt",
            &source,
            stamp,
            b"our replacement",
            ReplacementMetadata::RequireAll,
            || {
                std::fs::write(&path, b"external update").expect("concurrent update writes");
                false
            },
        );
        assert_eq!(
            result.expect_err("concurrent update is detected").kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            std::fs::read(path).expect("concurrent update remains"),
            b"external update"
        );
        assert_eq!(
            std::fs::read_dir(&root.0).expect("directory reads").count(),
            1
        );
    }

    #[test]
    fn deadline_expiring_after_replacement_staging_preserves_original() {
        let root = TempRoot::new();
        let bound = BoundRoot::open(&root.0).expect("root binds");
        let path = bound.path.join("file.txt");
        std::fs::write(&path, b"before").expect("fixture writes");
        let source = bound.open_file(&path).expect("source opens");
        let stamp = FileStamp::read(&source).expect("source stamps");
        let result = replace_existing(
            &bound.directory,
            "file.txt",
            &source,
            stamp,
            b"replacement",
            ReplacementMetadata::RequireAll,
            || {
                assert!(
                    std::fs::read_dir(&root.0).expect("directory reads").count() > 1,
                    "replacement staging exists before the activity check"
                );
                true
            },
        );
        assert_eq!(
            result
                .expect_err("expired operation must not publish")
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(std::fs::read(&path).expect("original remains"), b"before");
        assert_eq!(
            std::fs::read_dir(&root.0).expect("directory reads").count(),
            1,
            "the staged temporary is cleaned up"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unsupported_inode_flag_ioctl_is_optional_but_other_errors_fail_closed() {
        use super::copyable_file_flags;
        use rustix::fs::IFlags;
        use rustix::io::Errno;

        assert_eq!(
            copyable_file_flags(Ok(IFlags::empty())).unwrap(),
            Some(IFlags::empty())
        );
        assert!(copyable_file_flags(Err(Errno::NOTTY), ReplacementMetadata::RequireAll).is_err());
        assert!(
            copyable_file_flags(Err(Errno::NOTTY), ReplacementMetadata::AllowUnsupported)
                .unwrap()
                .is_none()
        );
        assert!(
            copyable_file_flags(Err(Errno::OPNOTSUPP), ReplacementMetadata::AllowUnsupported)
                .unwrap()
                .is_none()
        );
        assert!(
            copyable_file_flags(Err(Errno::IO), ReplacementMetadata::AllowUnsupported).is_err()
        );
    }
}
