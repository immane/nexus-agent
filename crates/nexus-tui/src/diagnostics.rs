//! Private, bounded metadata logs; never a transcript or provider wire log.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const MAX_FILE_BYTES: u64 = 1_048_576;
const MAX_RECORD_BYTES: usize = 4096;

#[derive(Clone, Debug)]
pub(crate) struct DiagnosticLog(Arc<Mutex<LogFiles>>, Arc<AtomicBool>);

#[derive(Debug)]
struct LogFiles {
    directory: PathBuf,
    lock: File,
    max_bytes: u64,
}

impl DiagnosticLog {
    pub(crate) fn open_default() -> io::Result<Self> {
        let sessions = nexus_session::default_data_dir()
            .ok_or_else(|| io::Error::other("local diagnostics directory is unavailable"))?;
        Self::open(&sessions.with_file_name("logs"), MAX_FILE_BYTES)
    }

    fn open(directory: &Path, max_bytes: u64) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(directory)?;
            let metadata = fs::symlink_metadata(directory)?;
            if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
                return Err(io::Error::other(
                    "local diagnostics directory must be private",
                ));
            }
            let lock = private_file(&directory.join("tui-runtime.lock"))?;
            check_file(&directory.join("tui-runtime.log.1"))?;
            private_file(&directory.join("tui-runtime.log"))?;
            Ok(Self(
                Arc::new(Mutex::new(LogFiles {
                    directory: directory.to_owned(),
                    lock,
                    max_bytes,
                })),
                Arc::new(AtomicBool::new(false)),
            ))
        }
        #[cfg(not(unix))]
        {
            let _ = (directory, max_bytes);
            Err(io::Error::other(
                "private local diagnostics are unsupported",
            ))
        }
    }
    pub(crate) fn take_failure(&self) -> bool {
        self.1.swap(false, Ordering::Relaxed)
    }
}

fn check_file(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                if !metadata.is_file()
                    || metadata.permissions().mode() & 0o077 != 0
                    || metadata.nlink() != 1
                {
                    return Err(io::Error::other(
                        "local diagnostic file must be private and regular",
                    ));
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn private_file(path: &Path) -> io::Result<File> {
    check_file(path)?;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    check_file(path)?;
    Ok(file)
}

impl DiagnosticLog {
    fn write_record(&self, bytes: &[u8]) -> io::Result<usize> {
        let files = self
            .0
            .lock()
            .map_err(|_| io::Error::other("diagnostic log lock poisoned"))?;
        // Reject, rather than truncate, an unexpectedly large record.
        if bytes.len() > MAX_RECORD_BYTES || bytes.len() as u64 > files.max_bytes {
            return Err(io::Error::other("diagnostic record exceeds its bound"));
        }
        // The stable lock file serializes rotation across TUI processes too.
        files.lock.lock()?;
        let result = (|| {
            let current = files.directory.join("tui-runtime.log");
            let previous = files.directory.join("tui-runtime.log.1");
            check_file(&previous)?;
            let mut file = private_file(&current)?;
            if file.metadata()?.len().saturating_add(bytes.len() as u64) > files.max_bytes {
                drop(file);
                fs::rename(&current, &previous)?;
                file = private_file(&current)?;
            }
            file.write_all(bytes)?;
            file.flush()?;
            Ok(bytes.len())
        })();
        let unlock = files.lock.unlock();
        result.and_then(|written| unlock.map(|()| written))
    }
}

impl Write for DiagnosticLog {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let result = self.write_record(bytes);
        if result.is_err() {
            self.1.store(true, Ordering::Relaxed);
        }
        result
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn directory() -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "nexus-diagnostics-{}-{}",
            std::process::id(),
            crate::SESSION_NONCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    #[test]
    fn runtime_metadata_is_persisted_and_survives_reopening() {
        let directory = directory();
        let log = DiagnosticLog::open(&directory, MAX_FILE_BYTES).unwrap();
        let (runtime, _streams) = crate::build_runtime().unwrap();
        let runtime = runtime.with_diagnostic_writer(log.clone());
        let text = fs::read_to_string(directory.join("tui-runtime.log")).unwrap();
        assert!(text.contains("timestamp_unix_s="));
        assert!(
            text.contains("nexus-runtime: context budget configured_items=512 effective_items=")
        );
        drop((runtime, log));
        let reopened = DiagnosticLog::open(&directory, MAX_FILE_BYTES).unwrap();
        assert_eq!(
            fs::read_to_string(directory.join("tui-runtime.log")).unwrap(),
            text
        );
        drop(reopened);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn log_rotates_with_bounded_retention_and_private_permissions() {
        let directory = directory();
        let mut log = DiagnosticLog::open(&directory, 16).unwrap();
        let observer = log.clone();
        let mut other = DiagnosticLog::open(&directory, 16).unwrap();
        log.write_all(b"first record\n").unwrap();
        other.write_all(b"second record\n").unwrap();
        log.write_all(b"third record\n").unwrap();
        assert_eq!(
            fs::read(directory.join("tui-runtime.log")).unwrap(),
            b"third record\n"
        );
        assert_eq!(
            fs::read(directory.join("tui-runtime.log.1")).unwrap(),
            b"second record\n"
        );
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 3);
        for name in ["tui-runtime.log", "tui-runtime.log.1", "tui-runtime.lock"] {
            let metadata = fs::metadata(directory.join(name)).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o077, 0);
            assert!(metadata.len() <= 16);
        }
        assert!(log.write_all(&[b'x'; MAX_RECORD_BYTES + 1]).is_err());
        assert!(observer.take_failure());
        assert!(!log.take_failure());
        drop((log, other, observer));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn log_refuses_symlinks_and_public_directories() {
        let directory = directory();
        let target = directory.join("target");
        fs::write(&target, b"untouched").unwrap();
        symlink(&target, directory.join("tui-runtime.log")).unwrap();
        assert!(DiagnosticLog::open(&directory, 16).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(DiagnosticLog::open(&directory, 16).is_err());
        fs::remove_dir_all(directory).unwrap();
    }
}
