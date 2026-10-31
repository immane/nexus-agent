#![forbid(unsafe_code)]

use std::ffi::CString;
use std::fs::{File, Permissions};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nexus_core::{
    ApprovedScope, CallId, EffectState, ExecutionStatus, M0_REVISION, NormalizedArgs, RunId,
    ToolCall, ToolContext, ToolId, ToolPort, TurnId,
};
use nexus_tools::{ScopedPatcher, ScopedWriter};
use rustix::buffer::spare_capacity;
use rustix::fs::{XattrFlags, fgetxattr, fsetxattr};

struct Root(PathBuf);

impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "nexus-linux-metadata-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).expect("create isolated fixture directory");
        Self(path)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn call(tool: &str, args: &str) -> ToolCall {
    ToolCall::new(
        RunId::new("run-1").unwrap(),
        TurnId::new("turn-1").unwrap(),
        CallId::new("call-1").unwrap(),
        ToolId::new(tool, M0_REVISION).unwrap(),
        NormalizedArgs::new(args).unwrap(),
    )
}

fn context() -> ToolContext {
    ToolContext::new(
        4096,
        Duration::ZERO,
        false,
        ApprovedScope::new("path:test").unwrap(),
    )
    .unwrap()
}

fn attribute(file: &File, name: &str) -> Vec<u8> {
    let mut value = Vec::with_capacity(4096);
    fgetxattr(file, name, spare_capacity(&mut value))
        .expect("BLOCKED: cannot read fixture xattr on this filesystem");
    value
}

#[test]
#[ignore = "Linux filesystem acceptance: requires user xattrs and copyable inode flags"]
fn linux_replacements_preserve_user_xattrs_and_metadata() {
    for patch in [false, true] {
        let root = Root::new();
        let path = root.0.join("fixture.txt");
        std::fs::write(&path, b"before OLD").unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o640)).unwrap();
        let source = File::open(&path).unwrap();
        for (name, value) in [
            ("user.nexus.binary", &b"\0value\xff"[..]),
            ("user.nexus.empty", &b""[..]),
        ] {
            fsetxattr(&source, name, value, XattrFlags::empty())
                .expect("BLOCKED: filesystem cannot set user fixture xattrs");
        }
        let before = source.metadata().unwrap();
        let binary = attribute(&source, "user.nexus.binary");
        let empty = attribute(&source, "user.nexus.empty");
        assert_eq!(binary, b"\0value\xff");
        assert!(empty.is_empty());
        let result = if patch {
            ScopedPatcher::with_root(&root.0).unwrap().execute(
                &call(
                    "host_patch",
                    r#"{"path":"fixture.txt","old_text":"OLD","new_text":"NEW"}"#,
                ),
                &context(),
            )
        } else {
            ScopedWriter::with_root(&root.0).unwrap().execute(
                &call("host_write", r#"{"path":"fixture.txt","content":"after"}"#),
                &context(),
            )
        };
        assert_eq!(
            result.status(),
            ExecutionStatus::Succeeded,
            "replacement failed or filesystem lacks required metadata support: {}",
            result.content()
        );
        assert_eq!(result.effect(), EffectState::KnownApplied);
        let replacement = File::open(&path).unwrap();
        let after = replacement.metadata().unwrap();
        assert_eq!(
            (after.uid(), after.gid(), after.mode() & 0o7777),
            (before.uid(), before.gid(), before.mode() & 0o7777)
        );
        assert_eq!(attribute(&replacement, "user.nexus.binary"), binary);
        assert_eq!(attribute(&replacement, "user.nexus.empty"), empty);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            if patch {
                &b"before NEW"[..]
            } else {
                &b"after"[..]
            }
        );
    }
}

#[test]
#[ignore = "Linux filesystem acceptance: requires user xattrs with non-UTF-8 names"]
fn linux_replacement_refuses_unrepresentable_xattr_without_mutation() {
    let root = Root::new();
    let path = root.0.join("fixture.txt");
    std::fs::write(&path, b"before").unwrap();
    let source = File::open(&path).unwrap();
    let name = CString::new(&b"user.nexus.\xff"[..]).unwrap();
    fsetxattr(&source, name.as_c_str(), b"retained", XattrFlags::empty())
        .expect("BLOCKED: filesystem cannot set non-UTF-8 user fixture xattr");
    let result = ScopedWriter::with_root(&root.0).unwrap().execute(
        &call("host_write", r#"{"path":"fixture.txt","content":"after"}"#),
        &context(),
    );
    assert_eq!(result.status(), ExecutionStatus::Failed);
    assert_eq!(result.effect(), EffectState::Unknown);
    assert_eq!(std::fs::read(&path).unwrap(), b"before");
    let replacement = File::open(&path).unwrap();
    let mut value = Vec::with_capacity(4096);
    fgetxattr(&replacement, name.as_c_str(), spare_capacity(&mut value))
        .expect("fixture xattr retained");
    assert_eq!(value, b"retained");
}
