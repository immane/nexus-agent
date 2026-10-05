#![forbid(unsafe_code)]

//! Root-jail coverage for the real file writer: create and overwrite,
//! every escape shape refused without disclosure, missing parents and
//! directories failed, closed schema (path-only calls rejected), and
//! cancellation.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use nexus_core::{
    ApprovedScope, CallId, EffectState, Evidence, ExecutionStatus, M0_REVISION, NormalizedArgs,
    RunId, ToolCall, ToolContext, ToolId, ToolPort, TurnId,
};
use nexus_tools::ScopedWriter;

static ROOTS: AtomicUsize = AtomicUsize::new(0);

/// Isolated jail root, removed on drop. Best-effort cleanup only.
struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nexus-tools-write-test-{}-{}",
            std::process::id(),
            ROOTS.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp root builds");
        Self(dir)
    }

    fn read(&self, name: &str) -> Option<Vec<u8>> {
        std::fs::read(self.0.join(name)).ok()
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn call(args_json: &str) -> ToolCall {
    ToolCall::new(
        RunId::new("run-1").expect("valid"),
        TurnId::new("turn-1").expect("valid"),
        CallId::new("call-1").expect("valid"),
        ToolId::new("host_write", M0_REVISION).expect("valid"),
        NormalizedArgs::new(args_json).expect("object arguments build"),
    )
}

fn context(budget: usize, cancelled: bool) -> ToolContext {
    ToolContext::new(
        budget,
        Duration::ZERO,
        cancelled,
        ApprovedScope::new("path:test").expect("valid scope"),
    )
    .expect("valid context builds")
}

fn writer_for(root: &TempRoot) -> ScopedWriter {
    ScopedWriter::with_root(&root.0).expect("valid root binds")
}

#[test]
fn roots_must_exist_and_be_directories() {
    let missing = PathBuf::from("/nonexistent-nexus-tools-write-root-9f3a");
    assert!(ScopedWriter::with_root(&missing).is_err());
    let root = TempRoot::new();
    let file = root.0.join("plain");
    std::fs::write(&file, b"x").expect("fixture writes");
    assert!(
        ScopedWriter::with_root(&file).is_err(),
        "files are not roots"
    );
    let bound = writer_for(&root);
    assert_eq!(
        bound.root(),
        &std::fs::canonicalize(&root.0).expect("canonical root"),
        "the root is stored canonicalized"
    );
}

#[test]
fn describe_names_the_gated_write_tool_at_m0() {
    let root = TempRoot::new();
    let spec = writer_for(&root).describe();
    assert_eq!(spec.id().name(), "host_write");
    assert!(
        spec.id()
            .is_compatible_with(&ToolId::new("host_write", M0_REVISION).expect("valid"))
    );
    assert!(spec.input_schema_json().contains("\"path\""));
    assert!(spec.input_schema_json().contains("\"content\""));
}

#[test]
fn create_and_overwrite_replace_the_whole_content() {
    let root = TempRoot::new();
    let writer = writer_for(&root);
    let outcome = writer.execute(
        &call(r#"{"path":"note.txt","content":"hello"}"#),
        &context(4096, false),
    );
    assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    assert_eq!(outcome.effect(), EffectState::KnownApplied);
    assert_eq!(outcome.evidence(), Evidence::HostObserved);
    assert_eq!(root.read("note.txt").expect("file created"), b"hello");

    let outcome = writer.execute(
        &call(r#"{"path":"note.txt","content":"hello, world"}"#),
        &context(4096, false),
    );
    assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    assert_eq!(outcome.effect(), EffectState::KnownApplied);
    assert_eq!(
        root.read("note.txt").expect("file overwritten"),
        b"hello, world"
    );
    let outcome = writer.execute(
        &call(r#"{"path":"note.txt","content":"短"}"#),
        &context(4096, false),
    );
    assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    assert_eq!(
        root.read("note.txt").expect("short replacement"),
        "短".as_bytes()
    );
}

#[cfg(unix)]
#[test]
fn symlinks_outside_the_root_are_denied_and_targets_unchanged() {
    let root = TempRoot::new();
    let outside = TempRoot::new();
    std::fs::write(outside.0.join("original"), b"unchanged").expect("fixture writes");
    std::os::unix::fs::symlink(outside.0.join("original"), root.0.join("link"))
        .expect("symlink builds");
    let outcome = writer_for(&root).execute(
        &call(r#"{"path":"link","content":"replacement"}"#),
        &context(4096, false),
    );
    assert_eq!(outcome.status(), ExecutionStatus::Denied);
    assert_eq!(outcome.effect(), EffectState::NotStarted);
    assert_eq!(
        outside.read("original").expect("original survives"),
        b"unchanged"
    );
}

#[cfg(unix)]
#[test]
fn special_file_targets_are_refused_before_opening() {
    let root = TempRoot::new();
    // A socket exercises non-regular targets without a blocking FIFO open.
    let _socket = std::os::unix::net::UnixListener::bind(root.0.join("socket"))
        .expect("fixture socket binds");
    let outcome = writer_for(&root).execute(
        &call(r#"{"path":"socket","content":"x"}"#),
        &context(4096, false),
    );
    assert_eq!(outcome.status(), ExecutionStatus::Failed);
}

#[test]
fn missing_parents_are_failed_never_created() {
    let root = TempRoot::new();
    let outcome = writer_for(&root).execute(
        &call(r#"{"path":"no/such/dir.txt","content":"x"}"#),
        &context(4096, false),
    );
    assert_eq!(outcome.status(), ExecutionStatus::Failed);
    assert!(root.read("no/such/dir.txt").is_none());
    assert!(!root.0.join("no").exists(), "no directories are created");
}

#[test]
fn escapes_are_denied_without_disclosure_or_effect() {
    let root = TempRoot::new();
    // Present so traversal shapes reach the jail prefix check instead of
    // failing earlier on a missing parent (also refused, as Failed).
    std::fs::create_dir(root.0.join("sub")).expect("fixture dir builds");
    let writer = writer_for(&root);
    for args in [
        r#"{"path":"../outside.txt","content":"x"}"#,
        r#"{"path":"sub/../../outside.txt","content":"x"}"#,
        r#"{"path":"/etc/nexus-write-probe","content":"x"}"#,
    ] {
        let outcome = writer.execute(&call(args), &context(4096, false));
        assert_eq!(outcome.status(), ExecutionStatus::Denied, "{args}");
        assert_eq!(outcome.effect(), EffectState::NotStarted, "{args}");
        assert_eq!(outcome.evidence(), Evidence::HostObserved, "{args}");
    }
    assert!(!root.0.join("outside.txt").exists());
}

#[test]
fn directories_are_never_overwritten() {
    let root = TempRoot::new();
    std::fs::create_dir(root.0.join("sub")).expect("fixture dir builds");
    let outcome = writer_for(&root).execute(
        &call(r#"{"path":"sub","content":"x"}"#),
        &context(4096, false),
    );
    assert_eq!(outcome.status(), ExecutionStatus::Failed);
    assert!(root.0.join("sub").is_dir(), "the directory survives");
}

#[test]
fn path_only_calls_are_rejected_without_writing() {
    // The read-shaped mistake the model otherwise makes must not produce
    // an empty file: content is required.
    let root = TempRoot::new();
    for args in [r#"{"path":"note.txt"}"#, r#"{"content":"x"}"#, r#"{}"#] {
        let outcome = writer_for(&root).execute(&call(args), &context(4096, false));
        assert_eq!(outcome.status(), ExecutionStatus::Failed, "{args}");
    }
    assert!(root.read("note.txt").is_none(), "no empty file is created");
}

#[test]
fn cancelled_calls_write_nothing() {
    let root = TempRoot::new();
    let outcome = writer_for(&root).execute(
        &call(r#"{"path":"note.txt","content":"x"}"#),
        &context(4096, true),
    );
    assert_eq!(outcome.status(), ExecutionStatus::Cancelled);
    assert!(root.read("note.txt").is_none());
}

#[test]
fn nested_paths_work_inside_existing_directories() {
    let root = TempRoot::new();
    std::fs::create_dir(root.0.join("sub")).expect("fixture dir builds");
    let outcome = writer_for(&root).execute(
        &call(r#"{"path":"sub/note.txt","content":"nested"}"#),
        &context(4096, false),
    );
    assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    assert_eq!(root.read("sub/note.txt").expect("nested file"), b"nested");
}
