#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::time::Duration;

use nexus_core::{
    ApprovedScope, CallId, ExecutionStatus, M0_REVISION, NormalizedArgs, RunId, ToolCall,
    ToolContext, ToolId, ToolPort, TurnId,
};
use nexus_tools::{ScopedLister, ScopedSearcher};

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "nexus-fs-read-{}-{}",
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
fn call(tool: &str, args: &str) -> ToolCall {
    ToolCall::new(
        RunId::new("run-1").unwrap(),
        TurnId::new("turn-1").unwrap(),
        CallId::new("call-1").unwrap(),
        ToolId::new(tool, M0_REVISION).unwrap(),
        NormalizedArgs::new(args).unwrap(),
    )
}
fn context(budget: usize) -> ToolContext {
    ToolContext::new(
        budget,
        Duration::ZERO,
        false,
        ApprovedScope::new("read:test").unwrap(),
    )
    .unwrap()
}

#[test]
fn list_shows_sorted_direct_children_and_marks_budget_cut() {
    let root = Root::new();
    std::fs::create_dir(root.0.join("dir")).unwrap();
    std::fs::write(root.0.join("b.txt"), b"b").unwrap();
    std::fs::write(root.0.join("a.txt"), b"a").unwrap();
    let tool = ScopedLister::with_root(&root.0).unwrap();
    let result = tool.execute(&call("host_list", r#"{"path":"."}"#), &context(100));
    assert_eq!(result.status(), ExecutionStatus::Succeeded);
    assert_eq!(result.content(), "a.txt\nb.txt\ndir/\n");
    let cut = tool.execute(&call("host_list", r#"{"path":"."}"#), &context(2));
    assert!(cut.is_truncated());
}

#[test]
fn search_returns_relative_path_line_and_skips_binary_content() {
    let root = Root::new();
    std::fs::create_dir(root.0.join("sub")).unwrap();
    std::fs::write(root.0.join("sub/file.txt"), b"first\nneedle here\nlast").unwrap();
    std::fs::write(root.0.join("binary"), [0xff, 0x00]).unwrap();
    let tool = ScopedSearcher::with_root(&root.0).unwrap();
    let result = tool.execute(
        &call("host_search", r#"{"path":".","query":"needle"}"#),
        &context(100),
    );
    assert_eq!(result.status(), ExecutionStatus::Succeeded);
    assert_eq!(result.content(), "sub/file.txt:2:needle here\n");
}

#[test]
fn list_and_search_refuse_paths_outside_the_root() {
    let root = Root::new();
    for result in [
        ScopedLister::with_root(&root.0)
            .unwrap()
            .execute(&call("host_list", r#"{"path":".."}"#), &context(100)),
        ScopedSearcher::with_root(&root.0).unwrap().execute(
            &call("host_search", r#"{"path":"..","query":"x"}"#),
            &context(100),
        ),
    ] {
        assert_eq!(result.status(), ExecutionStatus::Denied);
    }
}
