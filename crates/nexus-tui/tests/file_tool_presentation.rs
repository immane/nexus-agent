//! File-reading tools retain paths and typed status, never result bodies.

use nexus_core::provider::ModelContextItem;
use nexus_core::{
    CallId, EffectState, EventPayload, Evidence, ExecutionStatus, M0_REVISION, NormalizedArgs,
    PersistenceState, RequestId, RunEvent, RunFinished, RunId, RunOutcome, SessionId, ToolCall,
    ToolFinishedInfo, ToolId, ToolOutcome, ToolProgress, ToolStartedInfo, TurnId,
};
use nexus_tui::state::{AppState, EntryKind, MAX_RETAINED_BYTES, MAX_RETAINED_ENTRIES};

fn tool(name: &str) -> ToolId {
    ToolId::new(name, M0_REVISION).unwrap()
}

fn apply(state: &mut AppState, run: &str, seq: u64, payload: EventPayload) {
    assert!(state.apply_event(&RunEvent::new(
        SessionId::new("session").unwrap(),
        RunId::new(run).unwrap(),
        seq,
        payload,
    )));
}

fn start_run(state: &mut AppState, run: &str) {
    apply(
        state,
        run,
        0,
        EventPayload::RunStarted {
            request: RequestId::new("request").unwrap(),
        },
    );
}

fn start(call: &str, name: &str, args: Option<&str>) -> EventPayload {
    EventPayload::ToolStarted(ToolStartedInfo {
        call: CallId::new(call).unwrap(),
        tool: tool(name),
        args_preview: args.map(str::to_owned),
    })
}

fn output(call: &str, content: &str) -> EventPayload {
    EventPayload::ToolOutput(ToolProgress::new(CallId::new(call).unwrap(), content, true).unwrap())
}

fn outcome(status: ExecutionStatus, content: &str) -> ToolOutcome {
    ToolOutcome::new(
        status,
        EffectState::KnownNotApplied,
        Evidence::HostObserved,
        content,
        true,
    )
    .unwrap()
}

fn finish(call: &str, status: ExecutionStatus, content: &str) -> EventPayload {
    EventPayload::ToolFinished(ToolFinishedInfo {
        call: CallId::new(call).unwrap(),
        outcome: outcome(status, content),
    })
}

fn assert_hidden_even_expanded(state: &mut AppState, secret: &str) {
    for index in 0..state.entry_count() {
        assert!(
            !state
                .entry(index)
                .unwrap()
                .lines
                .join("\n")
                .contains(secret)
        );
        state.toggle_fold(index);
    }
    assert!(!state.transcript().join("\n").contains(secret));
    assert!(
        !state
            .visible_lines(160, 1000)
            .lines
            .join("\n")
            .contains(secret)
    );
}

#[test]
fn live_file_tools_hide_stream_and_final_bodies_but_keep_paths_and_status() {
    for (name, status) in [
        ("host_read", ExecutionStatus::Succeeded),
        ("host_list", ExecutionStatus::Failed),
    ] {
        let mut state = AppState::new();
        start_run(&mut state, "run");
        let args = format!(r#"{name} {{"path":"src/日本語.rs","extra":"SECRET"}}"#);
        apply(&mut state, "run", 1, start("call", name, Some(&args)));
        assert!(
            state
                .visible_lines(160, 30)
                .lines
                .join("\n")
                .contains("src/日本語.rs")
        );
        let before = state.retained_bytes();
        for seq in 2..20 {
            apply(
                &mut state,
                "run",
                seq,
                output("call", "SECRET streamed content\nentry"),
            );
            assert_eq!(
                state.retained_bytes(),
                before,
                "hidden streams retain no payload"
            );
        }
        apply(
            &mut state,
            "run",
            20,
            finish("call", status, "SECRET final content\nentry"),
        );
        assert_eq!(state.status(), "running");
        let transcript = state.transcript().join("\n");
        assert!(transcript.contains("src/日本語.rs"));
        assert!(transcript.contains(&format!("{status:?}")));
        assert!(transcript.contains("truncated"));
        assert_hidden_even_expanded(&mut state, "SECRET");
    }
}

#[test]
fn unrelated_tool_streams_and_errors_are_unchanged_when_interleaved() {
    let mut state = AppState::new();
    start_run(&mut state, "run");
    apply(&mut state, "run", 1, start("read", "host_read", None));
    apply(
        &mut state,
        "run",
        2,
        start("exec", "host_exec", Some(r#"host_exec {"argv":["false"]}"#)),
    );
    apply(&mut state, "run", 3, output("read", "SECRET"));
    apply(
        &mut state,
        "run",
        4,
        output("exec", "stderr: useful detail"),
    );
    apply(
        &mut state,
        "run",
        5,
        finish("exec", ExecutionStatus::Failed, "exit status 1"),
    );
    apply(
        &mut state,
        "run",
        6,
        finish("read", ExecutionStatus::Succeeded, "SECRET"),
    );
    let transcript = state.transcript().join("\n");
    for expected in [
        "host_exec",
        "Failed",
        "stderr: useful detail",
        "exit status 1",
        "path: [unavailable]",
    ] {
        assert!(transcript.contains(expected), "missing {expected}");
    }
    assert!(state.entry(4).unwrap().folded);
    assert!(
        state
            .visible_lines(160, 1000)
            .lines
            .join("\n")
            .contains("click to expand")
    );
    assert!(state.toggle_fold(4), "other tool outputs remain expandable");
    assert!(!state.entry(4).unwrap().folded);
    assert!(
        state
            .visible_lines(160, 1000)
            .lines
            .join("\n")
            .contains("click to collapse")
    );
    assert_hidden_even_expanded(&mut state, "SECRET");
}

#[test]
fn file_path_cards_have_no_fold_chrome_or_mouse_and_keyboard_fold_actions() {
    let mut live = AppState::new();
    start_run(&mut live, "run");
    apply(
        &mut live,
        "run",
        1,
        start("read", "host_read", Some(r#"host_read {"path":"file.rs"}"#)),
    );
    apply(
        &mut live,
        "run",
        2,
        start(
            "list",
            "host_list",
            Some(r#"host_list {"path":"directory"}"#),
        ),
    );
    apply(&mut live, "run", 3, output("read", "SECRET"));
    apply(
        &mut live,
        "run",
        4,
        finish("read", ExecutionStatus::Succeeded, "SECRET"),
    );
    apply(
        &mut live,
        "run",
        5,
        finish("list", ExecutionStatus::Failed, "SECRET"),
    );
    let mut saved = AppState::new();
    saved.record_restored_history(&[
        saved_call("read", "host_read", r#"{"path":"file.rs"}"#, "read-key"),
        saved_result("read", "host_read", "read-key"),
        saved_call("list", "host_list", r#"{"path":"directory"}"#, "list-key"),
        saved_result("list", "host_list", "list-key"),
    ]);
    for state in [&mut live, &mut saved] {
        state.set_viewport(160, 1000);
        for index in 0..state.entry_count() {
            if state.entry(index).unwrap().kind == EntryKind::Tool {
                assert!(!state.entry(index).unwrap().folded);
                assert!(!state.toggle_fold(index));
            }
        }
        state.fold_all();
        for index in 0..state.entry_count() {
            if state.entry(index).unwrap().kind == EntryKind::Tool {
                assert!(!state.entry(index).unwrap().folded);
            }
        }
        for row in 0..1000 {
            assert_eq!(state.tool_header_at(row), None);
            assert_eq!(state.tool_entry_at(row), None);
            assert_eq!(state.tool_identity_at(row), None);
            assert!(!state.toggle_tool_at_row(row));
        }
        let visible = state.visible_lines(160, 1000).lines.join("\n");
        assert!(visible.contains("file.rs"));
        assert!(visible.contains("directory"));
        assert!(visible.contains("Failed"));
        for chrome in [
            "▸ preview",
            "▾ expanded",
            "click to expand",
            "click to collapse",
        ] {
            assert!(
                !visible.contains(chrome),
                "unexpected fold chrome: {chrome}"
            );
        }
        assert_hidden_even_expanded(state, "SECRET");
    }
}

#[test]
fn eviction_does_not_forget_file_classification_and_new_runs_do_not_inherit_it() {
    let mut state = AppState::new();
    start_run(&mut state, "old");
    apply(
        &mut state,
        "old",
        1,
        start(
            "reused",
            "host_list",
            Some(r#"host_list {"path":"safe-dir"}"#),
        ),
    );
    for _ in 0..MAX_RETAINED_ENTRIES {
        state.record_submitted("evict old presentation entries");
    }
    assert!(state.dropped_entries > 0);
    apply(&mut state, "old", 2, output("reused", "SECRET entries"));
    apply(
        &mut state,
        "old",
        3,
        finish("reused", ExecutionStatus::TimedOut, "SECRET contents"),
    );
    assert!(state.transcript().join("\n").contains("safe-dir"));
    assert!(state.transcript().join("\n").contains("TimedOut"));
    assert_hidden_even_expanded(&mut state, "SECRET");
    apply(
        &mut state,
        "old",
        4,
        EventPayload::RunFinished(
            RunFinished::new(RunOutcome::Completed, PersistenceState::Ephemeral, None).unwrap(),
        ),
    );
    start_run(&mut state, "new");
    apply(&mut state, "new", 1, start("reused", "host_exec", None));
    apply(&mut state, "new", 2, output("reused", "new visible stream"));
    apply(
        &mut state,
        "new",
        3,
        finish("reused", ExecutionStatus::Failed, "new visible failure"),
    );
    assert!(state.transcript().join("\n").contains("new visible stream"));
    assert!(
        state
            .transcript()
            .join("\n")
            .contains("new visible failure")
    );
    assert!(state.retained_bytes() <= MAX_RETAINED_BYTES);
}

#[test]
fn classification_overflow_remains_bounded_and_fails_closed_after_eviction() {
    let mut state = AppState::new();
    start_run(&mut state, "run");
    for index in 0..=MAX_RETAINED_ENTRIES {
        apply(
            &mut state,
            "run",
            index as u64 + 1,
            start(&format!("call-{index}"), "host_read", None),
        );
    }
    for _ in 0..MAX_RETAINED_ENTRIES {
        state.record_submitted("evict all classified entries");
    }
    let call = format!("call-{MAX_RETAINED_ENTRIES}");
    apply(&mut state, "run", 1000, output(&call, "SECRET"));
    apply(
        &mut state,
        "run",
        1001,
        finish(&call, ExecutionStatus::Failed, "SECRET"),
    );
    assert!(state.entry_count() <= MAX_RETAINED_ENTRIES);
    assert!(state.retained_bytes() <= MAX_RETAINED_BYTES);
    assert_hidden_even_expanded(&mut state, "SECRET");
}

fn saved_call(call: &str, name: &str, args: &str, key: &str) -> ModelContextItem {
    ModelContextItem::assistant_call(
        key,
        key,
        ToolCall::new(
            RunId::new("saved-run").unwrap(),
            TurnId::new("turn").unwrap(),
            CallId::new(call).unwrap(),
            tool(name),
            NormalizedArgs::new(args).unwrap(),
        ),
    )
    .unwrap()
}

fn saved_result(call: &str, name: &str, key: &str) -> ModelContextItem {
    ModelContextItem::tool_result(
        CallId::new(call).unwrap(),
        key,
        key,
        tool(name),
        outcome(ExecutionStatus::Failed, "SECRET saved body"),
    )
    .unwrap()
}

#[test]
fn restored_paths_come_from_correlated_calls_without_changing_saved_results() {
    let items = vec![
        saved_call(
            "read",
            "host_read",
            r#"{"path":"saved-file","extra":"SECRET"}"#,
            "read-key",
        ),
        saved_call("list", "host_list", r#"{"path":"saved-dir"}"#, "list-key"),
        saved_result("read", "host_read", "read-key"),
        saved_result("list", "host_list", "list-key"),
        saved_call("exec", "host_exec", r#"{"argv":["false"]}"#, "exec-key"),
        ModelContextItem::tool_result(
            CallId::new("exec").unwrap(),
            "exec-key",
            "exec-key",
            tool("host_exec"),
            outcome(ExecutionStatus::Failed, "visible saved error"),
        )
        .unwrap(),
    ];
    let original = items.clone();
    let mut state = AppState::new();
    state.record_restored_history(&items);
    assert_eq!(items, original);
    let transcript = state.transcript().join("\n");
    for expected in ["saved-file", "saved-dir", "Failed", "visible saved error"] {
        assert!(transcript.contains(expected), "missing {expected}");
    }
    assert_hidden_even_expanded(&mut state, "SECRET");
}

#[test]
fn restored_paths_do_not_match_wrong_keys_tools_or_future_calls() {
    let mut state = AppState::new();
    state.record_restored_history(&[
        saved_call(
            "same",
            "host_read",
            r#"{"path":"WRONG-old-path"}"#,
            "old-key",
        ),
        saved_call(
            "same",
            "host_list",
            r#"{"path":"WRONG-new-path"}"#,
            "new-key",
        ),
        saved_result("same", "host_read", "old-key"),
        saved_result("future", "host_list", "future-key"),
        saved_call(
            "future",
            "host_list",
            r#"{"path":"WRONG-future-path"}"#,
            "future-key",
        ),
    ]);
    let transcript = state.transcript().join("\n");
    assert_eq!(transcript.matches("path: [unavailable]").count(), 2);
    assert_hidden_even_expanded(&mut state, "WRONG");
    assert_hidden_even_expanded(&mut state, "SECRET");

    let mut state = AppState::new();
    state.record_restored_history(&[
        saved_call(
            "reused",
            "host_read",
            r#"{"path":"WRONG-prior-run"}"#,
            "key",
        ),
        ModelContextItem::user_text("new run without its call record").unwrap(),
        saved_result("reused", "host_read", "key"),
    ]);
    assert!(
        state
            .transcript()
            .join("\n")
            .contains("path: [unavailable]")
    );
    assert_hidden_even_expanded(&mut state, "WRONG");
    assert_hidden_even_expanded(&mut state, "SECRET");
}

#[test]
fn missing_invalid_and_hostile_paths_are_safe_in_live_and_saved_views() {
    for args in [
        None,
        Some("not JSON SECRET"),
        Some(r#"{"path":null,"extra":"SECRET"}"#),
        Some(r#"{"path":""}"#),
    ] {
        let mut state = AppState::new();
        start_run(&mut state, "run");
        apply(&mut state, "run", 1, start("read", "host_read", args));
        assert!(
            state
                .transcript()
                .join("\n")
                .contains("path: [unavailable]")
        );
        assert_hidden_even_expanded(&mut state, "SECRET");
    }
    for args in ["{invalid SECRET}", r#"{"path":[],"extra":"SECRET"}"#, "{}"] {
        let mut state = AppState::new();
        state.record_restored_history(&[
            saved_call("read", "host_read", args, "key"),
            saved_result("read", "host_read", "key"),
        ]);
        assert!(
            state
                .transcript()
                .join("\n")
                .contains("path: [unavailable]")
        );
        assert_hidden_even_expanded(&mut state, "SECRET");
    }
    let path = "日本語\n\t\u{1b}[2J\u{202e}\u{200b}".to_owned() + &"界".repeat(4000);
    let args = serde_json::json!({"path": path}).to_string();
    let mut live = AppState::new();
    start_run(&mut live, "run");
    apply(&mut live, "run", 1, start("read", "host_read", Some(&args)));
    let mut saved = AppState::new();
    saved.record_restored_history(&[
        saved_call("read", "host_read", &args, "key"),
        saved_result("read", "host_read", "key"),
    ]);
    for state in [&mut live, &mut saved] {
        let transcript = state.transcript().join("\n");
        assert!(transcript.contains("日本語\\n\\t"));
        assert!(transcript.contains("\\u{202E}\\u{200B}"));
        assert!(transcript.contains("[path truncated]"));
        for unsafe_char in ['\u{1b}', '\u{202e}', '\u{200b}'] {
            assert!(!transcript.contains(unsafe_char));
        }
        assert!(state.retained_bytes() < 2048);
        assert_hidden_even_expanded(state, "SECRET");
    }
}
