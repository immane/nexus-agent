#![forbid(unsafe_code)]

//! Public-API rendering hardening for `nexus_tui::render`.
//!
//! Everything here drives the real renderer through [`nexus_tui::render`]
//! over a `ratatui` [`TestBackend`], so every frame is deterministic: no PTY,
//! no threads, no clock, no I/O, and no runtime, provider, or tool involved.
//!
//! Covered contract:
//! - the fixed composer, bounded viewport, and footer stay laid out and
//!   visible on every accepted frame, in every focus, at the minimum frame
//!   size, and with a pending approval card competing for rows;
//! - wrapping is measured and rendered at the same width, so no rendered row
//!   exceeds the frame and the viewport never claims rows it cannot show;
//! - both truncation indicators are explicit on screen: the presentation
//!   bound above the window, the per-entry byte bound, and retention drops;
//! - the approval card shows the exact runtime operation, scope, call, grant,
//!   deadline, and the exact arguments preview verbatim, and offers only the
//!   bounded decision set;
//! - a card whose detail cannot fit clips explicitly, drops every decision
//!   affordance, and leaves the approval gate closed until the expanded detail
//!   has been read contiguously from the first row;
//! - hostile runtime text (escapes, bidi overrides, invisible separators, C1
//!   controls, oversized titles) is sanitized, bounded, and flattened before
//!   it can move a cursor, reorder text, or spoof the card.
//!
//! Gaps documented here rather than papered over:
//! - a card larger than the frame can spare over-constrains the vertical split,
//!   so the composer/footer guarantees are asserted only at sizes where the
//!   layout is solvable; smaller accepted frames are asserted bounded and
//!   panic-free instead;
//! - `wrapped_height` counts characters, not terminal cells, so wide glyphs
//!   may still overflow a narrow frame. That is a documented limitation of the
//!   renderer, and these tests pin the character bound rather than deny it.

use std::time::Duration;

use nexus_core::{
    ApprovalId, ApprovalNotice, AssistantText, CallId, EventPayload, RequestId, RunEvent, RunId,
    SessionId, TurnId,
};
use nexus_tui::state::{
    MAX_APPROVAL_FIELD_BYTES, MAX_COMPOSER_BYTES, MAX_ENTRY_BYTES, MAX_RETAINED_BYTES,
    MAX_RETAINED_ENTRIES, MAX_TITLE_BYTES,
};
use nexus_tui::{AppState, Focus, render};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// Smallest accepted frame width; below it the renderer replaces the layout
/// with a one-line notice instead of a clipped frame.
const MIN_WIDTH: u16 = 20;
/// Smallest accepted frame height (header + footer + composer chrome + body).
const MIN_HEIGHT: u16 = 8;
/// Whole too-small notice, rendered only when it fits the frame.
const TOO_SMALL: &str = "terminal too small for nexus-tui (M0 test build)";
/// Leading words of the notice, short enough to survive the narrowest frame.
const TOO_SMALL_PREFIX: &str = "terminal too small";
/// Marker for an entry cut at its own byte bound.
const ENTRY_MARKER: &str = "[output truncated to presentation bound]";
/// Notice above the window when the presentation bound is reached.
const PRESENTATION_TRUNCATED: &str = "presentation-truncated";

/// Draws one frame through the public entry point and returns it as text rows.
fn frame(state: &mut AppState, width: u16, height: u16, focus: Focus) -> Vec<String> {
    let mut terminal =
        Terminal::new(TestBackend::new(width, height)).expect("test terminal builds");
    terminal
        .draw(|frame| render(state, frame.size(), frame.buffer_mut(), focus))
        .expect("test draw succeeds");
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer.get(x, y).symbol())
                .collect()
        })
        .collect()
}

fn joined(rows: &[String]) -> String {
    rows.join("\n")
}

fn row_holding(rows: &[String], needle: &str) -> Option<usize> {
    rows.iter().position(|row| row.contains(needle))
}

fn session() -> SessionId {
    SessionId::new("sess-1").expect("valid")
}

fn run() -> RunId {
    RunId::new("run-1").expect("valid")
}

fn started(seq: u64) -> RunEvent {
    RunEvent::new(
        session(),
        run(),
        seq,
        EventPayload::RunStarted {
            request: RequestId::new("req-1").expect("valid"),
        },
    )
}

fn text(seq: u64, turn: &str, body: &str) -> RunEvent {
    RunEvent::new(
        session(),
        run(),
        seq,
        EventPayload::AssistantTextDelta(
            AssistantText::new(TurnId::new(turn).expect("valid"), "item-0", body)
                .expect("fragment builds"),
        ),
    )
}

fn preview(seq: u64, item_key: String) -> RunEvent {
    RunEvent::new(
        session(),
        run(),
        seq,
        EventPayload::ToolCallPreview { item_key },
    )
}

/// Builds an approval notice through the validating constructor. The core
/// bounds each field at `MAX_SUMMARY_BYTES`, so summaries must stay under it.
fn approval_request(seq: u64, summary: &str, scope: &str, args: Option<&str>) -> RunEvent {
    let notice = ApprovalNotice::new(
        ApprovalId::new("a1-0").expect("valid"),
        CallId::new("c1-0").expect("valid"),
        summary,
        scope,
        Duration::from_secs(120),
    )
    .expect("notice builds");
    let notice = match args {
        Some(args) => notice.with_args_preview(args).expect("preview attaches"),
        None => notice,
    };
    RunEvent::new(
        session(),
        run(),
        seq,
        EventPayload::ApprovalRequired(notice),
    )
}

fn started_state() -> AppState {
    let mut state = AppState::new();
    assert!(state.apply_event(&started(0)));
    state
}

fn submitted(count: usize) -> AppState {
    let mut state = started_state();
    for index in 0..count {
        state.record_submitted(&format!("message {index}"));
    }
    state
}

/// A detail long enough to overflow any approved card width while staying
/// inside the core `MAX_SUMMARY_BYTES` field bound.
fn long_summary() -> String {
    "operation ".repeat(100)
}

fn long_scope() -> String {
    "scope ".repeat(100)
}

/// Every row must fit inside the frame, and the frame must be exactly as tall
/// as requested: the renderer never writes outside `area`.
fn assert_bounded(rows: &[String], width: u16, height: u16) {
    assert_eq!(rows.len(), height as usize, "every row is rendered");
    for (index, row) in rows.iter().enumerate() {
        assert!(
            row.chars().count() <= width as usize,
            "row {index} overflows the frame width: {row:?}"
        );
    }
}

#[test]
fn too_small_frames_get_one_notice_instead_of_a_clipped_layout() {
    // Wide but too short: the whole notice fits its single row.
    let mut state = submitted(40);
    let screen = joined(&frame(&mut state, 80, MIN_HEIGHT - 1, Focus::Viewport));
    assert_eq!(
        screen.lines().filter(|row| !row.trim().is_empty()).count(),
        1,
        "a rejected frame renders the notice and nothing else: {screen:?}"
    );
    assert!(screen.contains(TOO_SMALL), "{screen:?}");
    // The notice names the app, so the header is identified by the state it
    // reports rather than by the product name, which the notice shares.
    assert!(!screen.contains("composer"), "{screen:?}");
    assert!(!screen.contains("v0.1.1-alpha"), "{screen:?}");
    assert!(!screen.contains("run:"), "{screen:?}");
    assert!(!screen.contains("focus:"), "{screen:?}");

    // Too narrow, too short, or degenerate: still no layout, and still no
    // escape, panic, or out-of-frame write.
    for (width, height) in [
        (MIN_WIDTH - 1, 24),
        (19, 24),
        (12, 40),
        (1, 1),
        (0, 0),
        (MIN_WIDTH - 1, MIN_HEIGHT - 1),
    ] {
        let rows = frame(&mut state, width, height, Focus::Viewport);
        assert_bounded(&rows, width, height);
        let screen = joined(&rows);
        assert!(!screen.contains("composer"), "{width}x{height}: {screen:?}");
        assert!(
            !screen.contains("v0.1.1-alpha"),
            "{width}x{height}: {screen:?}"
        );
        assert!(!screen.contains("run:"), "{width}x{height}: {screen:?}");
        assert!(!screen.contains("focus:"), "{width}x{height}: {screen:?}");
        assert!(!screen.contains('\x1b'), "{width}x{height}: {screen:?}");
        if width as usize >= TOO_SMALL_PREFIX.len() && height >= 1 {
            assert!(
                screen.contains(TOO_SMALL_PREFIX),
                "{width}x{height}: {screen:?}"
            );
        }
    }

    // Exactly at both bounds the real layout comes back, so the threshold is
    // inclusive and not accidentally one row or one column early.
    for (width, height) in [(MIN_WIDTH, MIN_HEIGHT), (MIN_WIDTH, 24), (80, MIN_HEIGHT)] {
        let mut state = AppState::new();
        let rows = frame(&mut state, width, height, Focus::Composer);
        assert_bounded(&rows, width, height);
        let screen = joined(&rows);
        assert!(screen.contains("composer"), "{width}x{height}: {screen:?}");
        assert!(
            screen.contains("v0.1.1-alpha"),
            "{width}x{height}: {screen:?}"
        );
        assert!(
            !screen.contains(TOO_SMALL_PREFIX),
            "{width}x{height}: {screen:?}"
        );
    }
}

#[test]
fn composer_and_footer_survive_every_focus_and_the_smallest_frame() {
    for (width, height) in [
        (MIN_WIDTH, MIN_HEIGHT),
        (MIN_WIDTH, 12),
        (30, 10),
        (80, 24),
        (120, 40),
    ] {
        for focus in [Focus::Composer, Focus::Viewport, Focus::ApprovalCard] {
            let mut state = started_state();
            for char in "draft".chars() {
                state.composer_type(char);
            }
            let rows = frame(&mut state, width, height, focus);
            assert_bounded(&rows, width, height);
            let screen = joined(&rows);
            assert!(
                screen.contains("composer"),
                "{width}x{height} {focus:?}: composer missing: {screen:?}"
            );
            assert!(
                screen.contains("v0.1.1-alpha"),
                "{width}x{height} {focus:?}: footer missing: {screen:?}"
            );
            assert!(
                screen.contains("nexus-tui"),
                "{width}x{height} {focus:?}: header missing: {screen:?}"
            );
            // The draft is echoed, and the caret marks it only while the
            // composer owns the keyboard.
            assert!(
                screen.contains("draft"),
                "{width}x{height} {focus:?}: draft missing: {screen:?}"
            );
            if focus == Focus::Composer {
                assert!(
                    screen.contains('▊'),
                    "{width}x{height}: focused composer needs its caret: {screen:?}"
                );
            } else {
                assert!(
                    !screen.contains('▊'),
                    "{width}x{height} {focus:?}: caret must not follow focus: {screen:?}"
                );
            }
            // Focus state is never rendered: neither the header nor the
            // footer names it, at any frame width.
            assert!(
                !screen.contains("focus:"),
                "{width}x{height} {focus:?}: focus must not be reported: {screen:?}"
            );
        }
    }
}

#[test]
fn the_composer_body_is_clamped_and_never_pushes_the_footer_off_frame() {
    let mut state = started_state();
    state.composer_type('d');
    for _ in 0..10 {
        state.composer_newline();
    }
    // Many draft lines must not grow the composer past its clamp: the footer
    // still owns the last row and the block keeps its reserved rows.
    let rows = frame(&mut state, 80, 24, Focus::Composer);
    assert_bounded(&rows, 80, 24);
    // Match the block title: the header also names the focused pane, so a bare
    // "composer" would match the header row instead of the composer block.
    let composer_row = row_holding(&rows, "composer (fixed)").expect("composer row");
    assert_eq!(
        composer_row, 16,
        "the composer is clamped to five body rows plus its borders"
    );
    assert!(
        rows[23].contains("v0.1.1-alpha"),
        "the footer stays the last row: {rows:?}"
    );
}

#[test]
fn a_pending_approval_cannot_starve_the_composer_or_the_footer() {
    // At sizes the layout can solve, the card grows only into spare rows: the
    // fixed composer and the footer keep their rows, and the footer keeps
    // naming the inspector.
    for (width, height) in [(48, 16), (60, 20), (80, 24), (100, 30), (120, 40)] {
        let mut state = started_state();
        assert!(state.apply_event(&approval_request(
            1,
            &long_summary(),
            &long_scope(),
            Some("path=src/main.rs"),
        )));
        for focus in [Focus::Composer, Focus::ApprovalCard, Focus::Viewport] {
            let rows = frame(&mut state, width, height, focus);
            assert_bounded(&rows, width, height);
            let screen = joined(&rows);
            assert!(
                screen.contains("composer"),
                "{width}x{height} {focus:?}: card starved the composer: {screen:?}"
            );
            assert!(
                screen.contains("v0.1.1-alpha"),
                "{width}x{height} {focus:?}: card starved the footer: {screen:?}"
            );
            assert!(
                rows[height as usize - 1].contains("v0.1.1-alpha"),
                "{width}x{height} {focus:?}: footer must stay the last row: {rows:?}"
            );
            let composer_row = row_holding(&rows, "composer (fixed)").expect("composer block row");
            assert!(
                composer_row < height as usize - 1,
                "{width}x{height} {focus:?}: composer must sit above the footer"
            );
        }
    }
    // Degenerate-but-accepted frames with a pending card must stay in bounds
    // and panic-free even when the card cannot fit its own rows.
    for (width, height) in [
        (MIN_WIDTH, MIN_HEIGHT),
        (MIN_WIDTH, MIN_HEIGHT + 1),
        (21, 9),
        (1, 8),
    ] {
        let mut state = started_state();
        assert!(state.apply_event(&approval_request(1, &long_summary(), &long_scope(), None,)));
        for focus in [Focus::Composer, Focus::ApprovalCard] {
            let rows = frame(&mut state, width, height, focus);
            assert_bounded(&rows, width, height);
            let screen = joined(&rows);
            assert!(!screen.contains('\x1b'), "{width}x{height}: {screen:?}");
        }
    }
}

#[test]
fn wrapping_matches_the_measured_width_and_keeps_the_live_tail() {
    // Long unbroken tokens plus explicit spaces at a range of widths: whatever
    // the state measures is what the renderer can show.
    let long = format!("{}{}", "word ".repeat(30), "tail".repeat(10));
    for width in [MIN_WIDTH, 24, 40, 63, 80, 120] {
        let mut state = started_state();
        assert!(state.apply_event(&text(1, "t1-0", &long)));
        let rows = frame(&mut state, width, 24, Focus::Viewport);
        assert_bounded(&rows, width, 24);
        // Measurement and rendering agree: at any width the state hands out
        // lines that fit that width.
        for measure in [width as usize, width as usize - 1, width as usize + 1] {
            for line in state.visible_lines(measure, 200).lines {
                assert!(
                    line.chars().count() <= measure,
                    "line {line:?} exceeds its measured width {measure}"
                );
            }
        }
        // Wrapping is real: the long text occupies several rows instead of
        // being truncated to a single frame row, and reassembling the wrapped
        // rows minus their indent reproduces the text exactly.
        let view = state.visible_lines(width as usize, 200);
        assert!(
            view.lines.len() > 1,
            "long text must wrap at width {width}: {:?}",
            view.lines
        );
        // The run start is a system entry above the assistant entry, so the assistant
        // title is located rather than assumed to be the first row. The last
        // line is the entry's blank separator row, not body text.
        let start = row_holding(&view.lines, "assistant")
            .unwrap_or_else(|| panic!("no assistant entry at width {width}: {view:?}"));
        let body: String = view.lines[start + 1..view.lines.len() - 1]
            .iter()
            .map(|line| {
                line.strip_prefix("  ").unwrap_or_else(|| {
                    panic!("body rows keep the indent at width {width}: {line:?}")
                })
            })
            .collect();
        assert_eq!(
            body, long,
            "wrapping must not lose or reorder characters at width {width}"
        );
        // Every rendered body row but the last is exactly the indent plus a
        // full-width chunk.
        let body_width = width as usize - 2;
        for line in &view.lines[start + 1..view.lines.len() - 2] {
            assert_eq!(line.chars().count(), 2 + body_width, "{line:?}");
        }
    }
}

#[test]
fn the_viewport_bound_is_explicit_and_always_keeps_the_live_tail() {
    // Above the window: the bound notice names itself and the newest entry is
    // still on screen, so the live tail is never the row clipped away.
    //
    // The notice carries the hidden-line count, so it only fits verbatim once
    // the frame is wide enough for a three-digit count; at the narrowest frames
    // the bound row itself is cut. That is recorded here rather than hidden.
    for (width, height) in [(MIN_WIDTH, 12), (40, 12), (60, 20), (80, 24)] {
        let mut state = submitted(400);
        let rows = frame(&mut state, width, height, Focus::Viewport);
        assert_bounded(&rows, width, height);
        let screen = joined(&rows);
        if width >= 40 {
            assert!(
                screen.contains(PRESENTATION_TRUNCATED),
                "{width}x{height}: presentation bound not shown: {screen:?}"
            );
        } else {
            assert!(
                screen.contains("↑ presentation"),
                "{width}x{height}: the bound row is still present: {screen:?}"
            );
        }
        assert!(
            screen.contains("message 399"),
            "{width}x{height}: the live tail must survive the bound: {screen:?}"
        );
        let view = state.visible_lines(width as usize, 10);
        assert!(
            view.hidden_above > 0 || view.retention_truncated,
            "{width}x{height}: the bound must be reported to the renderer"
        );
    }
}

#[test]
fn the_entry_byte_bound_is_marked_instead_of_silently_dropping_characters() {
    for width in [80, 120] {
        let mut state = started_state();
        state.record_submitted(&"z".repeat(MAX_ENTRY_BYTES * 2));
        let entry = state.entry(1).expect("entry exists");
        assert!(entry.truncated, "the oversized entry is flagged");
        // The marker is the last thing the reader sees, after a bounded prefix
        // of the output rather than the whole oversized line.
        let retained: usize = entry.lines.iter().map(String::len).sum();
        assert!(
            retained > 0,
            "a bounded prefix is kept, not dropped wholesale"
        );
        assert!(retained <= MAX_ENTRY_BYTES);
        let rows = frame(&mut state, width, 24, Focus::Viewport);
        assert_bounded(&rows, width, 24);
        let screen = joined(&rows);
        assert!(
            screen.contains(ENTRY_MARKER),
            "width {width}: the entry bound must be visible: {screen:?}"
        );
    }
}

#[test]
fn retention_drops_are_bounded_counted_and_visible() {
    let mut state = started_state();
    for index in 0..(MAX_RETAINED_ENTRIES + 50) {
        state.record_submitted(&format!("entry {index}"));
    }
    assert_eq!(state.entry_count(), MAX_RETAINED_ENTRIES);
    assert!(state.dropped_entries > 0, "oldest entries are dropped");
    assert!(state.retained_bytes() <= MAX_RETAINED_BYTES);
    let rows = frame(&mut state, 80, 24, Focus::Viewport);
    assert_bounded(&rows, 80, 24);
    let screen = joined(&rows);
    assert!(
        screen.contains(PRESENTATION_TRUNCATED),
        "retention drops must be visible: {screen:?}"
    );
    assert!(
        screen.contains(&format!("entry {}", MAX_RETAINED_ENTRIES + 49)),
        "the newest retained entry stays visible: {screen:?}"
    );
    assert!(
        state.visible_lines(80, 20).retention_truncated,
        "the renderer is told older entries were dropped"
    );
}

#[test]
fn the_approval_card_shows_the_exact_operation_scope_and_arguments_preview() {
    let mut state = started_state();
    assert!(state.apply_event(&approval_request(
        1,
        "run tool host_write",
        "project scope",
        Some(r#"{"path":"src/main.rs","mode":"read"}"#),
    )));
    let rows = frame(&mut state, 120, 30, Focus::ApprovalCard);
    assert_bounded(&rows, 120, 30);
    let screen = joined(&rows);
    for expected in [
        "approval required: allow-once / deny",
        "operation: run tool host_write",
        "scope: project scope",
        r#"args: {"path":"src/main.rs","mode":"read"}"#,
        "call:c1-0 run:run-1",
        "grant:a1-0 runtime-bound; deadline 120s run-elapsed (monotonic, rechecked at dispatch)",
        "[a] allow once   [d] deny   [i] inspect   [esc] park (no decision)",
        "decisions go through the runtime only; previews never authorize",
    ] {
        assert!(
            screen.contains(expected),
            "missing {expected:?} in {screen:?}"
        );
    }
    // The preview is echoed verbatim, never reformatted or summarized, and an
    // absent preview contributes no row at all.
    assert_eq!(
        state.approval_args_preview(),
        Some(r#"{"path":"src/main.rs","mode":"read"}"#)
    );
    let mut bare = started_state();
    assert!(bare.apply_event(&approval_request(
        1,
        "run tool host_write",
        "project scope",
        None,
    )));
    assert!(bare.approval_args_preview().is_none());
    let bare_screen = joined(&frame(&mut bare, 120, 30, Focus::ApprovalCard));
    assert!(
        !bare_screen.contains("args:"),
        "an absent preview must not render an args row: {bare_screen:?}"
    );
    assert!(
        bare_screen.contains("operation: run tool host_write"),
        "{bare_screen:?}"
    );
    // Only the bounded decision set exists: no standing grant and no
    // trust-this-tool language anywhere on the card.
    let lowered = screen.to_lowercase();
    for forbidden in [
        "always",
        "trust",
        "auto-approve",
        "auto approve",
        "persist",
        "approve all",
    ] {
        assert!(
            !lowered.contains(forbidden),
            "the card must not offer {forbidden:?}: {screen:?}"
        );
    }
    let geometry = state.approval_geometry().expect("geometry recorded");
    assert_eq!(
        geometry.inner_width,
        120 - 2,
        "the card is inset by its borders"
    );
    assert!(!geometry.clipped, "the detail fits the frame");
    assert!(
        state.approval_decision_allowed(),
        "a fully shown card may be decided"
    );
    assert!(!state.approval_detail_truncated());
}

#[test]
fn a_clipped_card_drops_every_decision_and_keeps_the_gate_closed() {
    for (width, height) in [(48, 16), (60, 20), (80, 24)] {
        let mut state = started_state();
        assert!(state.apply_event(&approval_request(1, &long_summary(), &long_scope(), None,)));
        let rows = frame(&mut state, width, height, Focus::ApprovalCard);
        assert_bounded(&rows, width, height);
        let screen = joined(&rows);
        let geometry = state.approval_geometry().expect("geometry recorded");
        assert!(geometry.clipped, "{width}x{height}: the detail cannot fit");
        assert!(
            !state.approval_decision_allowed(),
            "{width}x{height}: a clipped card can never be approved"
        );
        assert!(
            screen.contains("detail clipped"),
            "{width}x{height}: the clip must be explicit: {screen:?}"
        );
        for forbidden in ["[a] allow once", "[d] deny", "[esc] park"] {
            assert!(
                !screen.contains(forbidden),
                "{width}x{height}: {forbidden:?} must not survive a clip: {screen:?}"
            );
        }
        // Opening the inspector is not a grant: allow stays locked and deny
        // stays reachable while the detail is being read.
        state.inspect_approval();
        let expanded = joined(&frame(&mut state, width, height, Focus::ApprovalCard));
        assert!(state.approval_detail_open());
        assert!(
            !state.approval_decision_allowed(),
            "{width}x{height}: inspecting is not approving"
        );
        assert!(
            expanded.contains("[d] deny"),
            "{width}x{height}: deny must stay reachable while reading: {expanded:?}"
        );
        assert!(
            !expanded.contains("[a] allow once"),
            "{width}x{height}: allow must stay locked until the detail ends: {expanded:?}"
        );
    }
}

#[test]
fn the_gate_opens_only_after_contiguous_inspection_of_the_whole_detail() {
    let mut state = started_state();
    assert!(state.apply_event(&approval_request(
        1,
        &long_summary(),
        &long_scope(),
        Some("path=src/main.rs"),
    )));
    frame(&mut state, 60, 20, Focus::ApprovalCard);
    assert!(
        state.approval_geometry().expect("geometry").clipped,
        "this card is too tall to fit, so inspection is required"
    );

    // Jumping to the last page is not inspection: rows the user never saw must
    // not count as seen, however far the scroll jumps.
    state.inspect_approval();
    frame(&mut state, 60, 20, Focus::ApprovalCard);
    let total = state.approval_detail_total_rows();
    let view_rows = state.approval_detail_view_rows();
    assert!(total > view_rows, "the detail is taller than one page");
    state.approval_detail_scroll(i32::MAX as isize);
    frame(&mut state, 60, 20, Focus::ApprovalCard);
    assert_eq!(
        state.approval_detail_scroll_position(),
        total - view_rows,
        "the scroll clamps to the last page"
    );
    assert!(
        !state.approval_detail_seen_all(),
        "skipping to the end must not count as having seen every row"
    );
    assert!(!state.approval_decision_allowed());

    // Reopening restarts at row zero; paging forward from there is the
    // deliberate, contiguous reading the gate requires.
    state.close_approval_detail();
    state.inspect_approval();
    frame(&mut state, 60, 20, Focus::ApprovalCard);
    assert_eq!(state.approval_detail_scroll_position(), 0);
    let mut steps = 0;
    while !state.approval_detail_seen_all() && steps < 256 {
        state.approval_detail_page(1);
        frame(&mut state, 60, 20, Focus::ApprovalCard);
        steps += 1;
    }
    assert!(
        state.approval_detail_seen_all(),
        "every detail row was shown"
    );
    assert!(
        state.approval_decision_allowed(),
        "deliberate full inspection unlocks the same live identity"
    );
    let inspected = joined(&frame(&mut state, 60, 20, Focus::ApprovalCard));
    assert!(
        inspected.contains("[a] allow once"),
        "allow appears only once the detail was fully read: {inspected:?}"
    );
    assert_eq!(
        state.approval_detail_total_rows(),
        total,
        "the measured detail is stable across redraws"
    );
    assert!(
        state.approval_geometry().expect("geometry").clipped,
        "reading the detail does not retroactively make the card fit"
    );
}

#[test]
fn a_field_cut_at_the_storage_bound_never_unlocks_the_gate() {
    // The core constructor caps every notice field at MAX_SUMMARY_BYTES, well
    // under the presentation bound, so the storage cut is only reachable with a
    // forged notice. The cut must fail closed: the visible prefix is shown, the
    // full detail is unavailable, and no amount of inspection unlocks the gate.
    let forged = "f".repeat(MAX_APPROVAL_FIELD_BYTES + 64);
    let forged_notice = ApprovalNotice {
        approval: ApprovalId::new("a1-0").expect("valid"),
        call: CallId::new("c1-0").expect("valid"),
        summary: forged,
        scope_summary: "project scope".to_owned(),
        args_preview: None,
        expires_at_elapsed: Duration::from_secs(120),
        session_directory: None,
    };
    let mut state = started_state();
    let event = RunEvent::new(
        session(),
        run(),
        1,
        EventPayload::ApprovalRequired(forged_notice),
    );
    assert!(state.apply_event(&event));
    assert!(
        state.approval_detail_truncated(),
        "a field above the presentation bound is cut at storage"
    );
    let card = state.pending_approval().expect("card exists");
    assert_eq!(
        card.summary.len(),
        MAX_APPROVAL_FIELD_BYTES,
        "the retained field is cut to the bound, never longer"
    );
    // Even where the compact card happens to show every retained row, the gate
    // stays shut because the full detail was never available.
    let screen = joined(&frame(&mut state, 120, 30, Focus::ApprovalCard));
    assert!(screen.contains("operation: fff"), "{screen:?}");
    assert!(
        !state.approval_decision_allowed(),
        "a cut field can never be approved"
    );
    state.inspect_approval();
    for _ in 0..64 {
        frame(&mut state, 120, 30, Focus::ApprovalCard);
        state.approval_detail_page(1);
    }
    frame(&mut state, 120, 30, Focus::ApprovalCard);
    assert!(
        state.approval_detail_seen_all(),
        "every retained row was shown"
    );
    assert!(
        !state.approval_decision_allowed(),
        "reading every retained row still cannot unlock a cut field"
    );
}

#[test]
fn hostile_runtime_text_cannot_reorder_hide_or_repaint_the_frame() {
    // A combined payload: screen clear, color change, OSC title, bidi override,
    // C1 CSI, DEL, and NUL.
    let payload = "run \x1b[2J\x1b[31mtool\x1b]0;spoofed\x07 \u{202E}banana\u{9b}31m\x7f\x00 done";
    let mut state = started_state();
    assert!(state.apply_event(&text(1, "t1-0", payload)));
    // The same tricks inside an approval field are the dangerous case: they are
    // escaped into a visible form rather than silently dropped.
    assert!(state.apply_event(&approval_request(
        2,
        "pay\u{200B}load \u{202E}rev",
        "scope\u{FEFF}text",
        Some("a\u{2060}b"),
    )));
    let rows = frame(&mut state, 100, 30, Focus::ApprovalCard);
    assert_bounded(&rows, 100, 30);
    let screen = joined(&rows);
    for forbidden in ['\x1b', '\u{9b}', '\x7f', '\x00', '\u{202E}', '\u{200B}'] {
        assert!(
            !screen.contains(forbidden),
            "{forbidden:?} must not reach the frame: {screen:?}"
        );
    }
    // Invisible characters in an approval field are escaped visibly, so a
    // reorder or split attempt is noticeable rather than silent.
    assert!(
        screen.contains("\\u{200B}"),
        "a zero-width space must be visibly escaped: {screen:?}"
    );
    assert!(
        screen.contains("\\u{202E}"),
        "a bidi override must be visibly escaped: {screen:?}"
    );
    // Sanitizing is not censoring: the ordinary words still render.
    assert!(
        screen.contains("banana"),
        "benign text survives: {screen:?}"
    );
    assert!(screen.contains("done"), "benign text survives: {screen:?}");
    assert!(screen.contains("tool"), "benign text survives: {screen:?}");
}

#[test]
fn hostile_titles_are_sanitized_flattened_and_bounded_before_rendering() {
    let mut state = started_state();
    assert!(state.apply_event(&text(1, "t1-0", "first line\nsecond line\ttabbed")));
    assert!(state.apply_event(&preview(
        2,
        format!("\x1b[2J\u{202E}{}", "t".repeat(MAX_TITLE_BYTES * 8)),
    )));
    state.notice(&format!(
        "notice \x1b[31m{}",
        "n".repeat(MAX_TITLE_BYTES * 2)
    ));
    let rows = frame(&mut state, 80, 24, Focus::Viewport);
    assert_bounded(&rows, 80, 24);
    let screen = joined(&rows);
    assert!(
        !screen.contains('\x1b'),
        "no escape reaches the frame: {screen:?}"
    );

    // Every retained title is bounded, single-line, and escape-free.
    for index in 0..state.entry_count() {
        let entry = state.entry(index).expect("entry exists");
        assert!(
            entry.title.len() <= MAX_TITLE_BYTES,
            "title {} exceeds its bound",
            entry.title.len()
        );
        assert!(!entry.title.contains('\n'), "a title is always one line");
        assert!(
            !entry.title.contains('\x1b'),
            "a title cannot inject escapes"
        );
        assert!(
            !entry.title.contains('\u{202E}'),
            "a title cannot reorder text"
        );
    }
    // The hostile preview key became a title that is escaped, and cut to the
    // title bound before any layout width was applied to it.
    let preview = state.entry(2).expect("preview entry");
    assert!(
        preview.title.starts_with("preview \\u{202E}"),
        "the bidi override in a title is escaped visibly: {:?}",
        preview.title
    );
    assert_eq!(
        preview.title.len(),
        MAX_TITLE_BYTES,
        "the title was cut to the bound, never longer"
    );
    assert!(
        screen.contains("preview \\u{202E}"),
        "the escaped prefix reaches the frame: {screen:?}"
    );
    assert!(
        screen.contains(&"t".repeat(60)),
        "only a bounded prefix of the title is rendered: {screen:?}"
    );
    // A frontend notice takes the body path, and is bounded by the entry bound
    // rather than the title bound.
    let notice_entry = state
        .entry_count()
        .checked_sub(1)
        .and_then(|index| state.entry(index))
        .expect("notice entry exists");
    assert_eq!(notice_entry.title, "system");
    assert!(
        notice_entry.lines[0].starts_with("notice "),
        "{:?}",
        notice_entry.lines[0]
    );
    assert_eq!(
        notice_entry.lines[0].len(),
        "notice ".len() + MAX_TITLE_BYTES * 2,
        "the notice text is retained verbatim within the entry bound"
    );
    // The body of a streamed entry keeps its real newlines rather than being
    // flattened into one runaway row.
    let transcript = state.transcript();
    assert!(
        transcript.iter().any(|line| line.contains("first line")),
        "{transcript:?}"
    );
    assert!(
        transcript.iter().any(|line| line.contains("second line")),
        "{transcript:?}"
    );
}

#[test]
fn composer_input_stays_bounded_and_refuses_control_characters() {
    let mut state = AppState::new();
    for char in "deploy the thing".chars() {
        state.composer_type(char);
    }
    let draft = state.composer().to_owned();
    assert_eq!(draft, "deploy the thing");
    // Control and bidi characters never reach the draft, so they can never
    // reach the composer render, which does not sanitize what it draws.
    for char in ['\n', '\r', '\x1b', '\x07', '\u{200F}', '\u{202E}'] {
        state.composer_type(char);
    }
    assert_eq!(
        state.composer(),
        draft,
        "control and bidi chars are refused"
    );
    // The rendered draft is exactly the retained draft plus the caret.
    let screen = joined(&frame(&mut state, 80, 24, Focus::Composer));
    assert!(screen.contains(&draft), "the draft is echoed verbatim");
    assert!(screen.contains('▊'), "the caret marks the insertion point");

    // Newline enters only through the explicit path, and submit hands the draft
    // back to the caller.
    state.composer_newline();
    assert!(state.composer().ends_with('\n'));
    assert!(state.composer_take().starts_with("deploy the thing"));
    assert!(state.composer().is_empty(), "submit clears the draft");
    state.composer_type('x');
    assert!(state.composer_backspace(), "backspace reports work done");
    assert!(
        !state.composer_backspace(),
        "backspace on an empty draft is a no-op"
    );

    // The draft is bounded by the command input bound, and an oversized draft
    // still renders inside the frame instead of spilling out of it.
    let mut flooded = AppState::new();
    for char in "deploy the thing".chars() {
        flooded.composer_type(char);
    }
    for _ in 0..(MAX_COMPOSER_BYTES + 16) {
        flooded.composer_type('x');
    }
    assert_eq!(flooded.composer().len(), MAX_COMPOSER_BYTES);
    let rows = frame(&mut flooded, 80, 24, Focus::Composer);
    assert_bounded(&rows, 80, 24);
    let overflow = joined(&rows);
    assert!(overflow.contains("deploy the thing"), "{overflow:?}");
    assert!(!overflow.contains('\x1b'), "{overflow:?}");
}

#[test]
fn the_same_state_renders_identical_frames_across_sizes_and_redraws() {
    let mut state = started_state();
    for index in 0..40 {
        state.record_submitted(&format!("line {index}"));
    }
    assert!(state.apply_event(&approval_request(
        1,
        &long_summary(),
        &long_scope(),
        Some("path=src/main.rs"),
    )));
    let first = frame(&mut state, 80, 24, Focus::ApprovalCard);
    assert_eq!(
        first,
        frame(&mut state, 80, 24, Focus::ApprovalCard),
        "redrawing unchanged state is idempotent"
    );
    let resized = frame(&mut state, 60, 16, Focus::ApprovalCard);
    // The measured geometry is the geometry this frame actually reserved.
    let geometry = state.approval_geometry().expect("geometry recorded");
    assert!(geometry.clipped);
    assert_eq!(
        geometry.inner_width,
        60 - 2,
        "inner width is the card width"
    );
    assert!(
        geometry.detail_rows > geometry.inner_rows,
        "the detail wanted more rows than the card could use"
    );
    assert_eq!(
        resized,
        frame(&mut state, 60, 16, Focus::ApprovalCard),
        "resizing does not introduce drift"
    );
    assert_eq!(
        frame(&mut state, 80, 24, Focus::ApprovalCard),
        first,
        "the renderer keeps no hidden layout state between frames"
    );
}
