#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::time::Duration;

use nexus_config::ToolsSettings;
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

#[test]
fn configured_list_cap_is_exact_and_reports_only_overflow_as_truncated() {
    let root = Root::new();
    let settings = ToolsSettings {
        list_entry_limit: 2,
        ..ToolsSettings::default()
    };
    let tool = ScopedLister::with_root(&root.0)
        .unwrap()
        .with_settings(&settings)
        .unwrap();
    let empty = tool.execute(&call("host_list", r#"{"path":"."}"#), &context(100));
    assert_eq!(empty.status(), ExecutionStatus::Succeeded);
    assert_eq!(empty.content(), "");
    assert!(!empty.is_truncated());
    for (name, expected) in [("a", "a\n"), ("b", "a\nb\n")] {
        std::fs::write(root.0.join(name), "").unwrap();
        let result = tool.execute(&call("host_list", r#"{"path":"."}"#), &context(100));
        assert_eq!(result.status(), ExecutionStatus::Succeeded);
        assert_eq!(result.content(), expected);
        assert!(!result.is_truncated());
    }
    std::fs::write(root.0.join("c"), "").unwrap();
    let over = tool.execute(&call("host_list", r#"{"path":"."}"#), &context(100));
    assert_eq!(over.status(), ExecutionStatus::Succeeded);
    assert_eq!(over.content().lines().count(), 2);
    assert!(over.is_truncated());
}

#[test]
fn configured_list_cap_counts_hidden_files_and_directories_as_children() {
    let root = Root::new();
    std::fs::write(root.0.join(".hidden"), "").unwrap();
    std::fs::create_dir(root.0.join("sub")).unwrap();
    let settings = ToolsSettings {
        list_entry_limit: 2,
        ..ToolsSettings::default()
    };
    let tool = ScopedLister::with_root(&root.0)
        .unwrap()
        .with_settings(&settings)
        .unwrap();
    let exact = tool.execute(&call("host_list", r#"{"path":"."}"#), &context(100));
    assert_eq!(exact.status(), ExecutionStatus::Succeeded);
    assert_eq!(exact.content(), ".hidden\nsub/\n");
    assert!(!exact.is_truncated());
}

#[test]
fn configured_search_scans_only_prefix_and_preserves_multibyte_boundary() {
    let root = Root::new();
    let settings = ToolsSettings {
        search_file_bytes: 8,
        ..ToolsSettings::default()
    };
    let tool = ScopedSearcher::with_root(&root.0)
        .unwrap()
        .with_settings(&settings)
        .unwrap();
    for (bytes, expected, truncated) in [
        (b"needle!!".as_slice(), "file:1:needle!!\n", false),
        (b"needle!!outside".as_slice(), "file:1:needle!!\n", true),
        ("needle\n雪tail".as_bytes(), "file:1:needle\n", true),
        (b"needle\xfftail".as_slice(), "", true),
        (b"needle\xe9".as_slice(), "", false),
    ] {
        std::fs::write(root.0.join("file"), bytes).unwrap();
        let result = tool.execute(
            &call("host_search", r#"{"path":".","query":"needle"}"#),
            &context(100),
        );
        assert_eq!(result.status(), ExecutionStatus::Succeeded);
        assert_eq!(result.content(), expected);
        assert_eq!(result.is_truncated(), truncated);
    }
    std::fs::write(root.0.join("file"), "12345678needle").unwrap();
    let result = tool.execute(
        &call("host_search", r#"{"path":".","query":"needle"}"#),
        &context(100),
    );
    assert_eq!(result.content(), "");
    assert!(result.is_truncated());
    std::fs::write(root.0.join("file"), "needle\n雪tail").unwrap();
    for (budget, expected) in [(8, ""), (9, ""), (10, "file:2:雪\n")] {
        let settings = ToolsSettings {
            search_file_bytes: budget,
            ..ToolsSettings::default()
        };
        let tool = ScopedSearcher::with_root(&root.0)
            .unwrap()
            .with_settings(&settings)
            .unwrap();
        let result = tool.execute(
            &call("host_search", r#"{"path":".","query":"雪"}"#),
            &context(100),
        );
        assert!(result.is_truncated());
        assert_eq!(result.content(), expected);
    }
}

#[test]
fn adapters_and_factories_reject_invalid_operational_settings() {
    let root = Root::new();
    for settings in [
        ToolsSettings {
            list_entry_limit: 0,
            ..ToolsSettings::default()
        },
        ToolsSettings {
            list_entry_limit: 10_001,
            ..ToolsSettings::default()
        },
        ToolsSettings {
            search_file_bytes: 0,
            ..ToolsSettings::default()
        },
        ToolsSettings {
            search_file_bytes: 1_048_577,
            ..ToolsSettings::default()
        },
    ] {
        assert!(
            ScopedLister::with_root(&root.0)
                .unwrap()
                .with_settings(&settings)
                .is_err()
        );
        assert!(
            ScopedSearcher::with_root(&root.0)
                .unwrap()
                .with_settings(&settings)
                .is_err()
        );
        assert!(nexus_tools::strict_tools_with_settings(&root.0, &settings).is_err());
        assert!(nexus_tools::development_tools_with_settings(&root.0, &settings).is_err());
        assert!(nexus_tools::development_file_tools_with_settings(&root.0, &settings).is_err());
    }
}

#[test]
fn configured_factories_apply_both_limits_and_defaults_preserve_old_caps() {
    let root = Root::new();
    let canonical = root.0.canonicalize().unwrap();
    std::fs::write(root.0.join("a"), "needle\n雪tail").unwrap();
    std::fs::write(root.0.join("b"), "needle\n雪tail").unwrap();
    let settings = ToolsSettings {
        list_entry_limit: 1,
        search_file_bytes: 8,
    };
    for tools in [
        nexus_tools::strict_tools_with_settings(&root.0, &settings).unwrap(),
        nexus_tools::development_tools_with_settings(&root.0, &settings).unwrap(),
        nexus_tools::development_file_tools_with_settings(&root.0, &settings).unwrap(),
    ] {
        let context = ToolContext::new(
            1024,
            Duration::ZERO,
            false,
            ApprovedScope::new(format!("path:{}", canonical.display())).unwrap(),
        )
        .unwrap();
        for tool in ["host_list", "host_search"] {
            let args = serde_json::json!({"path":".","query":"needle"});
            let args = if tool == "host_list" {
                serde_json::json!({"path":"."})
            } else {
                args
            };
            let result = tools
                .iter()
                .find(|registration| registration.port().describe().id().name() == tool)
                .unwrap()
                .port()
                .execute(&call(tool, &args.to_string()), &context);
            assert_eq!(
                result.status(),
                ExecutionStatus::Succeeded,
                "{}",
                result.content()
            );
            assert!(result.is_truncated());
            if tool == "host_list" {
                assert_eq!(result.content().lines().count(), 1);
            } else {
                assert_eq!(result.content().lines().count(), 2);
                assert!(result.content().contains("a:1:needle\n"));
                assert!(result.content().contains("b:1:needle\n"));
                assert!(!result.content().contains("雪"));
            }
        }
    }
    for tools in [
        nexus_tools::strict_tools(&root.0).unwrap(),
        nexus_tools::development_tools(&root.0).unwrap(),
        nexus_tools::development_file_tools(&root.0).unwrap(),
    ] {
        let context = ToolContext::new(
            1024,
            Duration::ZERO,
            false,
            ApprovedScope::new(format!("path:{}", canonical.display())).unwrap(),
        )
        .unwrap();
        let result = tools
            .iter()
            .find(|registration| registration.port().describe().id().name() == "host_list")
            .unwrap()
            .port()
            .execute(&call("host_list", r#"{"path":"."}"#), &context);
        assert_eq!(result.content(), "a\nb\n");
        assert!(!result.is_truncated());
        let result = tools
            .iter()
            .find(|registration| registration.port().describe().id().name() == "host_search")
            .unwrap()
            .port()
            .execute(
                &call("host_search", r#"{"path":".","query":"雪tail"}"#),
                &context,
            );
        assert!(result.content().contains("a:2:雪tail\n"));
        assert!(result.content().contains("b:2:雪tail\n"));
        assert!(!result.is_truncated());
    }
}
