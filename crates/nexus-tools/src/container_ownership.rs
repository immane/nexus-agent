//! Owner-only persistence for container cleanup metadata.
//!
//! Records contain resource identities and trusted Docker paths only; they
//! never contain workspace content, requests, or credentials. A stale lock is
//! deliberately fail-closed and is not removed or taken over automatically.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

const RECORD_VERSION: u64 = 1;
const MAX_RECORD_BYTES: u64 = 16 * 1024;
const MAX_RECORDS: usize = 64;
static TEMP_NONCE: AtomicU64 = AtomicU64::new(0);
static PROCESS_STORE: OnceLock<Mutex<Weak<OwnershipStore>>> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OwnershipRecord {
    pub(crate) nonce: String,
    pub(crate) gateway_name: String,
    pub(crate) gateway_id: Option<String>,
    pub(crate) workload_name: Option<String>,
    pub(crate) workload_id: Option<String>,
    pub(crate) resolver_file: Option<PathBuf>,
    pub(crate) docker: PathBuf,
    pub(crate) docker_config: Option<PathBuf>,
}

pub(crate) struct OwnershipStore {
    root: PathBuf,
    lock_path: PathBuf,
    _lock: File,
    operations: Mutex<()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OwnershipError {
    StorageUnavailable,
    UnresolvedOwnership,
}

pub(crate) struct OwnedRecord {
    store: Arc<OwnershipStore>,
    record: Mutex<OwnershipRecord>,
}

impl OwnershipStore {
    pub(crate) fn process_default() -> Result<Arc<Self>, OwnershipError> {
        let process_store = PROCESS_STORE.get_or_init(|| Mutex::new(Weak::new()));
        let mut current = process_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(store) = current.upgrade() {
            return Ok(store);
        }
        let root = default_root().ok_or(OwnershipError::StorageUnavailable)?;
        let store = match Self::open(&root) {
            Ok(store) => Arc::new(store),
            Err(()) if root.join("owner.lock").exists() => {
                return Err(OwnershipError::UnresolvedOwnership);
            }
            Err(()) => return Err(OwnershipError::StorageUnavailable),
        };
        match store.read_all() {
            Ok(records) if records.is_empty() => {}
            Ok(_) | Err(()) => return Err(OwnershipError::UnresolvedOwnership),
        }
        *current = Arc::downgrade(&store);
        Ok(store)
    }

    #[cfg(test)]
    fn open_for_admission(root: &Path) -> Result<Arc<Self>, ()> {
        let store = Arc::new(Self::open(root)?);
        if !store.read_all()?.is_empty() {
            return Err(());
        }
        Ok(store)
    }

    #[cfg(unix)]
    fn open(root: &Path) -> Result<Self, ()> {
        if root.exists() {
            let metadata = fs::symlink_metadata(root).map_err(|_| ())?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(());
            }
        } else {
            fs::create_dir_all(root).map_err(|_| ())?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root, fs::Permissions::from_mode(0o700)).map_err(|_| ())?;
            if fs::metadata(root).map_err(|_| ())?.permissions().mode() & 0o077 != 0 {
                return Err(());
            }
        }
        let root = fs::canonicalize(root).map_err(|_| ())?;
        let lock_path = root.join("owner.lock");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut lock = options.open(&lock_path).map_err(|_| ())?;
        writeln!(lock, "{}", std::process::id()).map_err(|_| ())?;
        lock.sync_all().map_err(|_| ())?;
        Ok(Self {
            root,
            lock_path,
            _lock: lock,
            operations: Mutex::new(()),
        })
    }

    #[cfg(not(unix))]
    fn open(_root: &Path) -> Result<Self, ()> {
        Err(())
    }

    pub(crate) fn create_record(
        self: &Arc<Self>,
        record: OwnershipRecord,
    ) -> Result<Arc<OwnedRecord>, ()> {
        validate_record(&record)?;
        let _guard = self
            .operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.root.join(format!("{}.json", record.nonce)).exists() {
            return Err(());
        }
        self.write_locked(&record)?;
        Ok(Arc::new(OwnedRecord {
            store: Arc::clone(self),
            record: Mutex::new(record),
        }))
    }

    fn write(&self, record: &OwnershipRecord) -> Result<(), ()> {
        validate_record(record)?;
        let _guard = self
            .operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.write_locked(record)
    }

    fn write_locked(&self, record: &OwnershipRecord) -> Result<(), ()> {
        let target = self.root.join(format!("{}.json", record.nonce));
        let nonce = TEMP_NONCE.fetch_add(1, Ordering::Relaxed);
        let temporary = self.root.join(format!(
            ".{}.{}.{}.tmp",
            record.nonce,
            std::process::id(),
            nonce
        ));
        let text = encode(record)?;
        if text.len() as u64 > MAX_RECORD_BYTES {
            return Err(());
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).map_err(|_| ())?;
        if file.write_all(text.as_bytes()).is_err() || file.sync_all().is_err() {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(());
        }
        drop(file);
        if fs::rename(&temporary, &target).is_err() {
            let _ = fs::remove_file(&temporary);
            return Err(());
        }
        sync_directory(&self.root)
    }

    fn remove(&self, nonce: &str) -> Result<(), ()> {
        if !valid_nonce(nonce) {
            return Err(());
        }
        let _guard = self
            .operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match fs::remove_file(self.root.join(format!("{nonce}.json"))) {
            Ok(()) => sync_directory(&self.root),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(()),
        }
    }

    fn read_all(&self) -> Result<Vec<OwnershipRecord>, ()> {
        let _guard = self
            .operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut records = Vec::new();
        for entry in fs::read_dir(&self.root).map_err(|_| ())? {
            let entry = entry.map_err(|_| ())?;
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            if records.len() >= MAX_RECORDS {
                return Err(());
            }
            let metadata = fs::symlink_metadata(&path).map_err(|_| ())?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.len() > MAX_RECORD_BYTES
            {
                return Err(());
            }
            let file = File::open(path).map_err(|_| ())?;
            let mut text = String::new();
            file.take(MAX_RECORD_BYTES + 1)
                .read_to_string(&mut text)
                .map_err(|_| ())?;
            let record = decode(&text)?;
            let expected_name = format!("{}.json", record.nonce);
            if entry.file_name() != std::ffi::OsStr::new(&expected_name) {
                return Err(());
            }
            records.push(record);
        }
        Ok(records)
    }
}

impl Drop for OwnershipStore {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.lock_path);
    }
}

impl OwnedRecord {
    pub(crate) fn snapshot(&self) -> OwnershipRecord {
        self.record
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn update(&self, update: impl FnOnce(&mut OwnershipRecord)) -> Result<(), ()> {
        let mut current = self
            .record
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut next = current.clone();
        update(&mut next);
        self.store.write(&next)?;
        *current = next;
        Ok(())
    }

    pub(crate) fn remove(&self) -> Result<(), ()> {
        self.store.remove(&self.snapshot().nonce)
    }
}

fn default_root() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    base.is_absolute()
        .then(|| base.join("nexus-agent/container"))
}

fn valid_nonce(nonce: &str) -> bool {
    (16..=64).contains(&nonce.len()) && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_record_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_record(record: &OwnershipRecord) -> Result<(), ()> {
    let has_workload = record.workload_name.is_some();
    if !valid_nonce(&record.nonce)
        || record.gateway_name != format!("nexus-gw-{}", record.nonce)
        || record
            .gateway_id
            .as_deref()
            .is_some_and(|id| !valid_record_id(id))
        || record
            .workload_id
            .as_deref()
            .is_some_and(|id| !valid_record_id(id))
        || record
            .workload_name
            .as_deref()
            .is_some_and(|name| name != format!("nexus-workload-{}", record.nonce))
        || (has_workload && (record.gateway_id.is_none() || record.resolver_file.is_none()))
        || (record.workload_id.is_some() && !has_workload)
        || (!has_workload && record.resolver_file.is_some())
        || !record.docker.is_absolute()
        || record
            .docker
            .to_string_lossy()
            .chars()
            .any(char::is_control)
        || record.docker_config.as_ref().is_some_and(|path| {
            !path.is_absolute() || path.to_string_lossy().chars().any(char::is_control)
        })
        || record.resolver_file.as_ref().is_some_and(|path| {
            !path.is_absolute() || path.to_string_lossy().chars().any(char::is_control)
        })
    {
        return Err(());
    }
    Ok(())
}

fn encode(record: &OwnershipRecord) -> Result<String, ()> {
    serde_json::to_string(&serde_json::json!({
        "version": RECORD_VERSION,
        "nonce": record.nonce,
        "gateway_name": record.gateway_name,
        "gateway_id": record.gateway_id,
        "workload_name": record.workload_name,
        "workload_id": record.workload_id,
        "resolver_file": record.resolver_file.as_ref().map(|path| path.to_string_lossy()),
        "docker": record.docker.to_string_lossy(),
        "docker_config": record.docker_config.as_ref().map(|path| path.to_string_lossy()),
    }))
    .map_err(|_| ())
}

fn decode(text: &str) -> Result<OwnershipRecord, ()> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|_| ())?;
    let object = value.as_object().ok_or(())?;
    if object.len() != 9 || object.get("version").and_then(|v| v.as_u64()) != Some(RECORD_VERSION) {
        return Err(());
    }
    let string = |key| object.get(key).and_then(|value| value.as_str()).ok_or(());
    let optional_string = |key| match object.get(key) {
        Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value.as_str().map(|s| Some(s.to_owned())).ok_or(()),
        None => Err(()),
    };
    let record = OwnershipRecord {
        nonce: string("nonce")?.to_owned(),
        gateway_name: string("gateway_name")?.to_owned(),
        gateway_id: optional_string("gateway_id")?,
        workload_name: optional_string("workload_name")?,
        workload_id: optional_string("workload_id")?,
        resolver_file: optional_string("resolver_file")?.map(PathBuf::from),
        docker: PathBuf::from(string("docker")?),
        docker_config: optional_string("docker_config")?.map(PathBuf::from),
    };
    validate_record(&record)?;
    Ok(record)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), ()> {
    File::open(path)
        .and_then(|dir| dir.sync_all())
        .map_err(|_| ())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), ()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "nexus-owner-record-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn record() -> OwnershipRecord {
        let nonce = "0123456789abcdef".to_owned();
        OwnershipRecord {
            gateway_name: format!("nexus-gw-{nonce}"),
            nonce,
            gateway_id: Some("a".repeat(64)),
            workload_name: None,
            workload_id: None,
            resolver_file: None,
            docker: PathBuf::from("/usr/local/bin/docker"),
            docker_config: None,
        }
    }

    #[test]
    fn owner_record_is_private_atomic_and_round_trips() {
        let root = TempRoot::new();
        let store = Arc::new(OwnershipStore::open(&root.0).unwrap());
        let owner = store.create_record(record()).unwrap();
        owner
            .update(|record| {
                record.workload_name = Some(format!("nexus-workload-{}", record.nonce));
                record.workload_id = Some("b".repeat(64));
                record.resolver_file = Some(PathBuf::from("/tmp/nexus-resolver"));
            })
            .unwrap();
        let records = store.read_all().unwrap();
        assert_eq!(records, [owner.snapshot()]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&root.0).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(root.0.join("0123456789abcdef.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        owner
            .update(|record| {
                record.workload_name = None;
                record.workload_id = None;
                record.resolver_file = None;
            })
            .unwrap();
        owner.remove().unwrap();
        assert!(store.read_all().unwrap().is_empty());
    }

    #[test]
    fn existing_lock_and_symlinked_storage_fail_closed() {
        let root = TempRoot::new();
        let store = OwnershipStore::open(&root.0).unwrap();
        assert!(OwnershipStore::open_for_admission(&root.0).is_err());
        drop(store);

        let target = TempRoot::new();
        let link = root.0.with_extension("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target.0, &link).unwrap();
        #[cfg(unix)]
        assert!(OwnershipStore::open(&link).is_err());
        #[cfg(unix)]
        let _ = fs::remove_file(link);
    }

    #[test]
    fn leftover_owner_records_block_new_container_admission() {
        let root = TempRoot::new();
        {
            let store = Arc::new(OwnershipStore::open(&root.0).unwrap());
            store.create_record(record()).unwrap();
        }
        assert!(OwnershipStore::open_for_admission(&root.0).is_err());
        assert!(root.0.join("0123456789abcdef.json").is_file());
    }

    #[test]
    fn records_reject_untrusted_identity_and_unknown_fields() {
        let mut invalid = record();
        invalid.gateway_name = "unrelated-container".into();
        assert!(validate_record(&invalid).is_err());
        let mut value =
            serde_json::from_str::<serde_json::Value>(&encode(&record()).unwrap()).unwrap();
        value["unexpected"] = serde_json::json!(true);
        assert!(decode(&value.to_string()).is_err());
    }
}
