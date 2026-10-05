#![forbid(unsafe_code)]

//! Root-jail coverage for the real file reader: exact reads, every
//! escape shape refused without disclosure, budget truncation, strict
//! text, cancellation, and malformed admission.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use nexus_core::{
    ApprovedScope, CallId, EffectState, Evidence, ExecutionStatus, Limits, M0_REVISION,
    NormalizedArgs, RunId, ToolCall, ToolContext, ToolId, ToolPort, TurnId,
};
use nexus_tools::ScopedReader;

static ROOTS: AtomicUsize = AtomicUsize::new(0);

/// Isolated jail root, removed on drop. Best-effort cleanup only.
struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nexus-tools-test-{}-{}",
            std::process::id(),
            ROOTS.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp root builds");
        Self(dir)
    }

    fn file(&self, name: &str, content: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, content).expect("fixture writes");
        path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn call(path_json: &str) -> ToolCall {
    ToolCall::new(
        RunId::new("run-1").expect("valid"),
        TurnId::new("turn-1").expect("valid"),
        CallId::new("call-1").expect("valid"),
        ToolId::new("host_read", M0_REVISION).expect("valid"),
        NormalizedArgs::new(path_json).expect("object arguments build"),
    )
}

fn context(budget: usize, cancelled: bool) -> ToolContext {
    ToolContext::new(
        budget,
        Duration::ZERO,
        cancelled,
        ApprovedScope::new("read:test").expect("valid scope"),
    )
    .expect("valid context builds")
}

fn reader_for(root: &TempRoot) -> ScopedReader {
    ScopedReader::with_root(&root.0).expect("valid root binds")
}

#[test]
fn roots_must_exist_and_be_directories() {
    let missing = PathBuf::from("/nonexistent-nexus-tools-root-9f3a");
    assert!(ScopedReader::with_root(&missing).is_err());
    let root = TempRoot::new();
    let file = root.file("plain", b"x");
    assert!(
        ScopedReader::with_root(&file).is_err(),
        "files are not roots"
    );
    let bound = reader_for(&root);
    assert_eq!(
        bound.root(),
        &std::fs::canonicalize(&root.0).expect("canonical root"),
        "the root is stored canonicalized"
    );
}

#[test]
fn describe_names_the_automatic_read_tool_at_m0() {
    let root = TempRoot::new();
    let spec = reader_for(&root).describe();
    assert_eq!(spec.id().name(), "host_read");
    assert!(
        spec.id()
            .is_compatible_with(&ToolId::new("host_read", M0_REVISION).expect("valid"))
    );
    assert!(spec.input_schema_json().contains("\"path\""));
}

#[test]
fn exact_content_reads_succeed_without_effects() {
    let root = TempRoot::new();
    root.file("hello.txt", "hello scoped world".as_bytes());
    let outcome =
        reader_for(&root).execute(&call(r#"{"path":"hello.txt"}"#), &context(4096, false));
    assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    assert_eq!(outcome.effect(), EffectState::KnownNotApplied);
    assert_eq!(outcome.evidence(), Evidence::HostObserved);
    assert_eq!(outcome.content(), "hello scoped world");
    assert!(!outcome.is_truncated());
}

#[test]
fn nested_relative_paths_stay_inside() {
    let root = TempRoot::new();
    std::fs::create_dir_all(root.0.join("sub")).expect("subdir builds");
    root.file("sub/deep.txt", b"deep");
    let outcome =
        reader_for(&root).execute(&call(r#"{"path":"sub/deep.txt"}"#), &context(4096, false));
    assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    assert_eq!(outcome.content(), "deep");
}

#[test]
fn missing_files_fail_without_disclosure() {
    let root = TempRoot::new();
    let outcome = reader_for(&root).execute(
        &call(r#"{"path":"no-such-file.txt"}"#),
        &context(4096, false),
    );
    assert_eq!(outcome.status(), ExecutionStatus::Failed);
    assert_eq!(outcome.effect(), EffectState::Unknown);
    assert_eq!(outcome.evidence(), Evidence::Uncertain);
    assert_eq!(outcome.content(), "file cannot be read");
}

#[test]
fn traversal_to_a_real_outside_file_is_denied_without_reading_it() {
    let root = TempRoot::new();
    let file_name = format!(
        "nexus-tools-secret-{}",
        ROOTS.fetch_add(1, Ordering::SeqCst)
    );
    let outside = root
        .0
        .parent()
        .expect("temp dir has a parent")
        .join(&file_name);
    std::fs::write(&outside, b"SENTINEL-OUTSIDE-JAIL").expect("outside fixture writes");
    let attempt = format!(r#"{{"path":"../{file_name}"}}"#);
    let outcome = reader_for(&root).execute(&call(&attempt), &context(4096, false));
    assert_eq!(outcome.status(), ExecutionStatus::Denied);
    assert_eq!(outcome.effect(), EffectState::NotStarted);
    assert_eq!(outcome.content(), "path escapes the tool root");
    assert!(
        !outcome.content().contains("SENTINEL"),
        "denial discloses nothing"
    );
    let _ = std::fs::remove_file(&outside);
}

#[test]
fn absolute_paths_outside_are_denied_absolute_inside_allowed() {
    let root = TempRoot::new();
    let outside =
        std::env::temp_dir().join(format!("nexus-tools-abs-outside-{}", std::process::id()));
    std::fs::write(&outside, b"SENTINEL-ABS").expect("outside fixture writes");
    let attempt = format!(
        r#"{{"path":{}}}"#,
        json_escape(outside.to_str().expect("utf8"))
    );
    let outcome = reader_for(&root).execute(&call(&attempt), &context(4096, false));
    assert_eq!(outcome.status(), ExecutionStatus::Denied);
    assert!(!outcome.content().contains("SENTINEL"));

    root.file("inside.txt", b"inside absolute");
    let canonical = std::fs::canonicalize(root.0.join("inside.txt")).expect("canonicalizes");
    let attempt = format!(
        r#"{{"path":{}}}"#,
        json_escape(canonical.to_str().expect("utf8"))
    );
    let outcome = reader_for(&root).execute(&call(&attempt), &context(4096, false));
    assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    assert_eq!(outcome.content(), "inside absolute");
    let _ = std::fs::remove_file(&outside);
}

/// Minimal JSON string escaper for absolute-path fixtures (the suite owns
/// no JSON writer beyond this test).
fn json_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len() + 2);
    escaped.push('"');
    for char in text.chars() {
        match char {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            _ => escaped.push(char),
        }
    }
    escaped.push('"');
    escaped
}

#[cfg(unix)]
#[test]
fn symlinks_resolve_before_the_prefix_check() {
    use std::os::unix::fs::symlink;
    let root = TempRoot::new();
    root.file("real.txt", b"through the link");
    symlink(root.0.join("real.txt"), root.0.join("good-link")).expect("link builds");
    let outcome =
        reader_for(&root).execute(&call(r#"{"path":"good-link"}"#), &context(4096, false));
    assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    assert_eq!(outcome.content(), "through the link");

    let outside =
        std::env::temp_dir().join(format!("nexus-tools-link-outside-{}", std::process::id()));
    std::fs::write(&outside, b"SENTINEL-LINK").expect("outside fixture writes");
    symlink(&outside, root.0.join("bad-link")).expect("link builds");
    let outcome = reader_for(&root).execute(&call(r#"{"path":"bad-link"}"#), &context(4096, false));
    assert_eq!(outcome.status(), ExecutionStatus::Denied);
    assert!(!outcome.content().contains("SENTINEL"));
    let _ = std::fs::remove_file(&outside);
}

#[cfg(unix)]
#[test]
fn symlink_loops_fail_instead_of_hanging() {
    use std::os::unix::fs::symlink;
    let root = TempRoot::new();
    symlink(root.0.join("loop"), root.0.join("loop")).expect("loop builds");
    let outcome = reader_for(&root).execute(&call(r#"{"path":"loop"}"#), &context(4096, false));
    assert_eq!(outcome.status(), ExecutionStatus::Failed);
}

#[test]
fn empty_and_nul_paths_are_refused() {
    let root = TempRoot::new();
    for arguments in [r#"{"path":""}"#, "{\"path\":\"a\\u0000b\"}"] {
        let outcome = reader_for(&root).execute(&call(arguments), &context(4096, false));
        assert_eq!(outcome.status(), ExecutionStatus::Failed);
        assert_eq!(outcome.content(), "tool input is invalid");
    }
}

#[test]
fn misshaped_arguments_are_refused() {
    let root = TempRoot::new();
    for arguments in [r#"{}"#, r#"{"path":42}"#, r#"{"path":null}"#] {
        let outcome = reader_for(&root).execute(&call(arguments), &context(4096, false));
        assert_eq!(outcome.status(), ExecutionStatus::Failed);
    }
}

#[test]
fn oversize_content_truncates_on_a_boundary_with_its_flag() {
    let root = TempRoot::new();
    root.file("big.txt", "x".repeat(300_000).as_bytes());
    let outcome = reader_for(&root).execute(&call(r#"{"path":"big.txt"}"#), &context(100, false));
    assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    assert!(outcome.is_truncated(), "the cut is flagged");
    assert_eq!(outcome.content().len(), 100);
    assert_eq!(outcome.content(), "x".repeat(100));
}

#[test]
fn non_utf8_content_fails_without_mangling() {
    let root = TempRoot::new();
    root.file("binary.bin", &[0xFF, 0xFE, 0x00, 0x61]);
    let outcome =
        reader_for(&root).execute(&call(r#"{"path":"binary.bin"}"#), &context(4096, false));
    assert_eq!(outcome.status(), ExecutionStatus::Failed);
    assert_eq!(outcome.content(), "tool input is invalid");
}

#[test]
fn bounded_read_cut_inside_utf8_preserves_text_and_truncation() {
    let root = TempRoot::new();
    let cap = Limits::M0_TEST_TOOL_OUTPUT_BYTES;
    let mut text = "x".repeat(cap - 1);
    text.push_str("中文");
    root.file("big-utf8.txt", text.as_bytes());
    let outcome =
        reader_for(&root).execute(&call(r#"{"path":"big-utf8.txt"}"#), &context(cap, false));
    assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    assert!(outcome.is_truncated());
    assert_eq!(outcome.content(), "x".repeat(cap - 1));
}

#[test]
fn directories_are_not_readable_content() {
    let root = TempRoot::new();
    std::fs::create_dir_all(root.0.join("dir")).expect("dir builds");
    let outcome = reader_for(&root).execute(&call(r#"{"path":"dir"}"#), &context(4096, false));
    assert_eq!(outcome.status(), ExecutionStatus::Failed);
}

#[test]
fn cancelled_calls_report_cancellation_honestly() {
    let root = TempRoot::new();
    root.file("hello.txt", b"hello");
    let outcome = reader_for(&root).execute(&call(r#"{"path":"hello.txt"}"#), &context(4096, true));
    assert_eq!(outcome.status(), ExecutionStatus::Cancelled);
    assert_eq!(outcome.effect(), EffectState::Unknown);
    assert_eq!(outcome.evidence(), Evidence::Uncertain);
}

#[test]
fn full_cap_budget_reads_without_truncation() {
    let root = TempRoot::new();
    root.file("hello.txt", b"hello");
    let outcome = reader_for(&root).execute(
        &call(r#"{"path":"hello.txt"}"#),
        &context(Limits::M0_TEST_TOOL_OUTPUT_BYTES, false),
    );
    assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
    assert!(!outcome.is_truncated());
}
