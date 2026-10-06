#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::time::Duration;

use nexus_core::{
    ApprovedScope, CallId, EffectState, ExecutionStatus, M0_REVISION, NormalizedArgs, RunId,
    ToolCall, ToolContext, ToolId, ToolPort, TurnId,
};
use nexus_tools::ScopedPatcher;

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "nexus-fs-patch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn call(args: &str) -> ToolCall {
    ToolCall::new(
        RunId::new("run-1").unwrap(),
        TurnId::new("turn-1").unwrap(),
        CallId::new("call-1").unwrap(),
        ToolId::new("host_patch", M0_REVISION).unwrap(),
        NormalizedArgs::new(args).unwrap(),
    )
}
fn context(cancelled: bool) -> ToolContext {
    ToolContext::new(
        4096,
        Duration::ZERO,
        cancelled,
        ApprovedScope::new("path:test").unwrap(),
    )
    .unwrap()
}

#[test]
fn unique_exact_match_is_replaced_and_effect_is_applied() {
    let root = Root::new();
    std::fs::write(root.0.join("note.txt"), "before OLD after\n").unwrap();
    let patcher = ScopedPatcher::with_root(&root.0).unwrap();
    let result = patcher.execute(
        &call(r#"{"path":"note.txt","old_text":"OLD","new_text":"NEW"}"#),
        &context(false),
    );
    assert_eq!(result.status(), ExecutionStatus::Succeeded);
    assert_eq!(result.effect(), EffectState::KnownApplied);
    assert_eq!(
        std::fs::read_to_string(root.0.join("note.txt")).unwrap(),
        "before NEW after\n"
    );
}

#[test]
fn absent_or_ambiguous_match_fails_without_modifying_file() {
    let root = Root::new();
    let original = "OLD and OLD";
    std::fs::write(root.0.join("note.txt"), original).unwrap();
    let patcher = ScopedPatcher::with_root(&root.0).unwrap();
    for args in [
        r#"{"path":"note.txt","old_text":"MISSING","new_text":"NEW"}"#,
        r#"{"path":"note.txt","old_text":"OLD","new_text":"NEW"}"#,
    ] {
        let result = patcher.execute(&call(args), &context(false));
        assert_eq!(result.status(), ExecutionStatus::Failed);
        assert_eq!(result.effect(), EffectState::KnownNotApplied);
        assert_eq!(result.content(), "patch did not match exactly once");
        assert_eq!(
            std::fs::read_to_string(root.0.join("note.txt")).unwrap(),
            original
        );
    }
}

#[test]
fn patch_never_creates_files_or_changes_files_outside_the_root() {
    let root = Root::new();
    let outside = root.0.parent().unwrap().join(format!(
        "{}-outside.txt",
        root.0.file_name().unwrap().to_string_lossy()
    ));
    std::fs::write(&outside, "OUTSIDE").unwrap();
    let patcher = ScopedPatcher::with_root(&root.0).unwrap();
    let missing = patcher.execute(
        &call(r#"{"path":"new.txt","old_text":"x","new_text":"y"}"#),
        &context(false),
    );
    assert_eq!(missing.status(), ExecutionStatus::Failed);
    assert!(!root.0.join("new.txt").exists());
    let args = format!(
        r#"{{"path":{},"old_text":"OUTSIDE","new_text":"CHANGED"}}"#,
        serde_json::to_string(&outside.to_string_lossy().as_ref()).unwrap()
    );
    let escape = patcher.execute(&call(&args), &context(false));
    assert_eq!(escape.status(), ExecutionStatus::Denied);
    assert_eq!(escape.effect(), EffectState::NotStarted);
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "OUTSIDE");
    std::fs::remove_file(outside).unwrap();
}

#[test]
fn cancelled_patch_does_not_change_the_file() {
    let root = Root::new();
    std::fs::write(root.0.join("note.txt"), "OLD").unwrap();
    let result = ScopedPatcher::with_root(&root.0).unwrap().execute(
        &call(r#"{"path":"note.txt","old_text":"OLD","new_text":"NEW"}"#),
        &context(true),
    );
    assert_eq!(result.status(), ExecutionStatus::Cancelled);
    assert_eq!(
        std::fs::read_to_string(root.0.join("note.txt")).unwrap(),
        "OLD"
    );
}
