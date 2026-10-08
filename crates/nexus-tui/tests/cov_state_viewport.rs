#![forbid(unsafe_code)]

//! External coverage hardening for the bounded viewport, the composer
//! draft, and the approval exact-arguments preview of
//! [`nexus_tui::state::AppState`].
//!
//! The unit suite inside `state.rs` can reach crate-private entry builders
//! (`push_line`, `push_tool`). This suite deliberately does not: it drives
//! the state only through the published surface of the `nexus_tui` library
//! (runtime [`nexus_core::RunEvent`] envelopes plus the documented public
//! methods), so it also pins that this surface is sufficient to exercise
//! every viewport, composer, and approval-preview rule.
//!
//! Layout facts the assertions depend on, all measured at [`WIDTH`]:
//! `AppState::notice` records one entry whose title is the 6-char `system`
//! and whose single body line is short, so each notice entry is exactly two
//! wrapped lines and entry `i` occupies lines `2i..2i+2`. Presentation
//! bounds are read from the crate's own public constants rather than
//! hardcoded, so a deliberate bound change surfaces here as a failing
//! assertion instead of a silently stale literal.
//!
//! Deterministic: literal `RunId`/`CallId`/`TurnId` strings, literal
//! durations, no clock, no randomness, no I/O, no terminal, no runtime.

use std::time::Duration;

use nexus_core::{
    ApprovalId, ApprovalNotice, AssistantText, CallId, EffectState, EventPayload, Evidence,
    ExecutionStatus, PersistenceState, RequestId, RunEvent, RunFinished, RunId, RunOutcome,
    SessionId, ToolFinishedInfo, ToolOutcome, TurnId,
};
use nexus_tui::state::{
    AppState, ApprovalGeometry, MAX_APPROVAL_CARD_BYTES, MAX_APPROVAL_FIELD_BYTES,
    MAX_COMPOSER_BYTES, MAX_RETAINED_ENTRIES,
};

const EXPIRY: Duration = Duration::from_secs(120);
const WIDTH: usize = 80;
const HEIGHT: usize = 10;

/// Wrapped lines contributed by one `notice` entry at [`WIDTH`].
const NOTICE_LINES: usize = 3;

fn session() -> SessionId {
    SessionId::new("sess-1").expect("valid session id")
}

fn run() -> RunId {
    RunId::new("run-1").expect("valid run id")
}

fn call(id: &str) -> CallId {
    CallId::new(id).expect("valid call id")
}

fn turn() -> TurnId {
    TurnId::new("t1-0").expect("valid turn id")
}

fn run_event(seq: u64, payload: EventPayload) -> RunEvent {
    RunEvent::new(session(), run(), seq, payload)
}

/// `RunStarted`, adopting the run so later events have a live sequence.
fn started(seq: u64) -> RunEvent {
    run_event(
        seq,
        EventPayload::RunStarted {
            request: RequestId::new("req-1").expect("valid request id"),
        },
    )
}

fn fragment(seq: u64, item: &str, text: &str) -> RunEvent {
    run_event(
        seq,
        EventPayload::AssistantTextDelta(
            AssistantText::new(turn(), item, text).expect("valid text fragment"),
        ),
    )
}

fn tool_finished(seq: u64, call_id: &str, content: &str) -> RunEvent {
    run_event(
        seq,
        EventPayload::ToolFinished(ToolFinishedInfo {
            call: call(call_id),
            outcome: ToolOutcome::new(
                ExecutionStatus::Succeeded,
                EffectState::KnownApplied,
                Evidence::HostObserved,
                content,
                false,
            )
            .expect("valid tool outcome"),
        }),
    )
}

/// A conversation of `count` two-line system entries.
fn state_with_notices(count: usize) -> AppState {
    let mut state = AppState::new();
    for index in 0..count {
        state.notice(&format!("n{index}"));
    }
    state
}

/// The whole conversation rendered at full height, used as the reference
/// for "the live window is the tail of the transcript" assertions.
fn whole_transcript(state: &AppState) -> Vec<String> {
    state.visible_lines(WIDTH, state.total_height(WIDTH)).lines
}

/// Asserts entry `index` is both positioned inside the rendered window and
/// actually rendered, for the two-line notice layout.
fn assert_entry_visible(state: &AppState, index: usize, height: usize) {
    let view = state.visible_lines(WIDTH, height);
    assert_eq!(view.lines.len(), height, "the window is full");
    let window_start = state.total_height(WIDTH) - height - state.scrollback();
    let entry_start = index * NOTICE_LINES;
    assert!(
        (window_start..window_start + height).contains(&entry_start),
        "entry {index} starts at line {entry_start}, outside the window \
         {window_start}..{}: {:?}",
        window_start + height,
        view.lines
    );
    let expected = format!("  n{index}");
    assert!(
        view.lines.contains(&expected),
        "entry {index} is rendered in the window: {:?}",
        view.lines
    );
}

// -------------------------------------------------------------------------
// Viewport: bottom-anchored window and the hidden-line indicator
// -------------------------------------------------------------------------

#[test]
fn visible_window_is_the_exact_tail_of_the_conversation() {
    let state = state_with_notices(20);
    assert_eq!(
        state.total_height(WIDTH),
        60,
        "20 three-line entries at {WIDTH} columns"
    );

    let window = state.visible_lines(WIDTH, HEIGHT).lines;
    assert_eq!(window.len(), HEIGHT, "the window is filled");
    assert_eq!(
        window,
        whole_transcript(&state)[50..],
        "bottom anchoring shows the newest wrapped lines"
    );
    assert_eq!(
        window[HEIGHT - 2].as_str(),
        "  n19",
        "the newest body line sits above the trailing separator"
    );
    assert_eq!(
        window.last().map(String::as_str),
        Some(""),
        "every entry ends with a blank separator row"
    );

    let shrunk = state.visible_lines(WIDTH, 4).lines;
    assert_eq!(shrunk, whole_transcript(&state)[56..]);
    assert_eq!(shrunk[2].as_str(), "  n19");
}

#[test]
fn hidden_above_counts_exactly_the_lines_above_the_window() {
    let state = state_with_notices(20);
    let total = state.total_height(WIDTH);

    let view = state.visible_lines(WIDTH, HEIGHT);
    assert_eq!(view.lines.len(), HEIGHT);
    assert_eq!(
        view.hidden_above,
        total - HEIGHT,
        "everything above the window is counted, not rendered"
    );
    assert!(
        !view.retention_truncated,
        "nothing was dropped from retention"
    );

    // Two rows short of the content: only two lines are hidden.
    let almost = state.visible_lines(WIDTH, total - 2);
    assert_eq!(almost.hidden_above, 2);
    assert_eq!(almost.lines.len(), total - 2);
    assert_eq!(almost.lines[almost.lines.len() - 2].as_str(), "  n19");

    // Exactly the content height: nothing is hidden.
    let exact = state.visible_lines(WIDTH, total);
    assert_eq!(exact.hidden_above, 0);
    assert_eq!(exact.lines.len(), total);
    assert_eq!(exact.lines, whole_transcript(&state));
}

#[test]
fn a_frame_taller_than_the_content_hides_nothing() {
    let state = state_with_notices(3);
    let total = state.total_height(WIDTH);
    assert_eq!(total, 9);

    // At or above the content height everything is shown, with no rows
    // wasted on the hidden-lines indicator.
    for height in total..=total + 3 {
        let view = state.visible_lines(WIDTH, height);
        assert_eq!(
            view.hidden_above, 0,
            "content fits at height {height}, so nothing is hidden"
        );
        assert_eq!(
            view.lines.len(),
            total,
            "the frame is never padded with empty rows at height {height}"
        );
        assert_eq!(view.lines, whole_transcript(&state));
        assert!(
            !state.truncation_indicator(WIDTH, height),
            "a frame with spare rows needs no indicator at height {height}"
        );
    }

    // Below the content height the window is bottom-anchored and the
    // shortfall is reported exactly.
    for height in 1..total {
        let view = state.visible_lines(WIDTH, height);
        assert_eq!(
            view.hidden_above,
            total - height,
            "the shortfall is counted at height {height}"
        );
        assert_eq!(
            view.lines.len(),
            height,
            "the frame is filled to capacity at height {height}"
        );
        if height == 1 {
            assert_eq!(
                view.lines,
                vec![String::new()],
                "a one-row frame shows the trailing separator"
            );
        } else {
            assert_eq!(
                view.lines[view.lines.len() - 2].as_str(),
                "  n2",
                "the newest line stays anchored at height {height}"
            );
        }
        assert!(state.truncation_indicator(WIDTH, height));
    }
    assert_eq!(state.visible_lines(WIDTH, 1).lines.len(), 1);
    assert_eq!(
        state.visible_lines(WIDTH, 1).lines,
        vec![String::new()],
        "a one-row frame shows the newest separator row, not the oldest"
    );
}

#[test]
fn degenerate_geometry_is_clamped_instead_of_panicking() {
    let state = state_with_notices(4);

    // A zero width/height must behave as the smallest usable frame rather
    // than divide by zero or index out of bounds.
    let zero = state.visible_lines(0, 0);
    assert_eq!(zero.lines.len(), 1, "a 0x0 request renders exactly one row");
    assert!(zero.hidden_above > 0, "the rest is reported as hidden");
    assert_eq!(
        zero.lines,
        state.visible_lines(1, 1).lines,
        "0 and 1 are the same clamped geometry"
    );

    let empty = AppState::new();
    assert_eq!(empty.total_height(WIDTH), 0);
    assert_eq!(
        empty.total_height(0),
        0,
        "a zero width still measures sanely"
    );
    let blank = empty.visible_lines(WIDTH, HEIGHT);
    assert!(
        blank.lines.is_empty(),
        "an empty conversation renders nothing"
    );
    assert_eq!(blank.hidden_above, 0);
    assert!(empty.visible_lines(WIDTH, 0).lines.is_empty());
}

#[test]
fn truncation_indicator_is_reserved_only_when_content_is_hidden() {
    let state = state_with_notices(20);
    let total = state.total_height(WIDTH);
    assert_eq!(total, 60);

    assert!(
        state.truncation_indicator(WIDTH, HEIGHT),
        "content taller than the frame needs an indicator row"
    );
    assert!(
        state.truncation_indicator(WIDTH, total - 1),
        "one row short of the content still needs an indicator"
    );
    assert!(
        !state.truncation_indicator(WIDTH, total),
        "an exactly-fitting frame needs no indicator"
    );
    assert!(
        !state.truncation_indicator(WIDTH, total + 5),
        "a frame with spare rows needs no indicator"
    );
    assert!(
        !AppState::new().truncation_indicator(WIDTH, HEIGHT),
        "an empty conversation needs no indicator"
    );

    // A zero height is treated as one row, so a non-empty conversation is
    // always over-full.
    assert!(state.truncation_indicator(WIDTH, 0));
    assert!(state_with_notices(1).truncation_indicator(WIDTH, 0));
}

#[test]
fn dropped_entries_keep_the_indicator_on_even_a_spacious_frame() {
    let mut state = AppState::new();
    for index in 0..(MAX_RETAINED_ENTRIES + 1) {
        state.notice(&format!("n{index}"));
    }
    assert_eq!(state.entry_count(), MAX_RETAINED_ENTRIES);
    assert_eq!(state.dropped_entries, 1, "the oldest entry was dropped");

    // A frame tall enough for everything still retained would not need an
    // indicator for hidden lines; the drop alone keeps it on.
    let tall = state.visible_lines(WIDTH, state.total_height(WIDTH));
    assert_eq!(tall.hidden_above, 0, "nothing is hidden above the window");
    assert!(tall.retention_truncated, "the view reports the drop");
    assert!(
        state.truncation_indicator(WIDTH, state.total_height(WIDTH)),
        "a retention drop is itself a visible condition"
    );

    let short = state.visible_lines(WIDTH, HEIGHT);
    assert!(short.retention_truncated);
    assert!(short.hidden_above > 0);
    assert_eq!(short.lines.len(), HEIGHT);
    // `n0` is the entry that was dropped, so the newest retained one is the
    // last of the `MAX_RETAINED_ENTRIES` entries pushed after it.
    let newest = format!("  n{MAX_RETAINED_ENTRIES}");
    assert_eq!(short.lines[short.lines.len() - 2].as_str(), newest.as_str());
}

#[test]
fn every_rendered_line_is_sanitized_and_width_bounded() {
    let mut state = AppState::new();
    assert!(state.apply_event(&started(0)));
    assert!(state.apply_event(&fragment(1, "item-0", "he")));
    assert!(state.apply_event(&fragment(
        2,
        "item-0",
        "llo\u{202E}gnp.exe\u{1b}[2Jcleared\n\u{200B}zero"
    )));
    state.notice("notice \u{1b}]0;title\u{7} done");
    state.record_submitted("user \u{1b}[31mred\u{202E}");

    let view = state.visible_lines(WIDTH, 20);
    assert!(
        view.lines.iter().all(|line| !line.contains('\x1b')),
        "no rendered line carries a terminal escape: {:?}",
        view.lines
    );
    assert!(
        view.lines.iter().all(|line| line.chars().count() <= WIDTH),
        "every rendered line fits the frame width: {:?}",
        view.lines
    );
    assert!(
        view.lines.iter().any(|line| line.contains("202E")),
        "a reorder attempt stays visible as an escape: {:?}",
        view.lines
    );
    assert!(
        view.lines.iter().any(|line| line.contains("cleared")),
        "text after a stripped escape sequence survives: {:?}",
        view.lines
    );
    assert!(
        view.lines.iter().any(|line| line.contains("done")),
        "text after a stripped OSC survives: {:?}",
        view.lines
    );
    assert!(
        view.lines.iter().any(|line| line.contains("zero")),
        "a new line in one fragment becomes its own entry line: {:?}",
        view.lines
    );
}

#[test]
fn folding_shrinks_the_layout_without_dropping_history() {
    let mut state = state_with_notices(6);
    let unfolded = state.total_height(WIDTH);
    assert_eq!(unfolded, 18);

    // One body line per entry: folding collapses each entry to its marker.
    state.fold_all();
    let folded = state.total_height(WIDTH);
    assert!(
        folded < unfolded,
        "folding removes body lines from the layout ({folded} vs {unfolded})"
    );
    let marker = state.visible_lines(WIDTH, HEIGHT);
    assert!(
        marker
            .lines
            .iter()
            .any(|line| line.contains("[folded, 1 lines]")),
        "each folded entry renders its marker: {:?}",
        marker.lines
    );

    state.unfold_all();
    assert_eq!(
        state.total_height(WIDTH),
        unfolded,
        "unfolding restores the measured height"
    );
    assert_eq!(
        state.visible_lines(WIDTH, HEIGHT).lines,
        whole_transcript(&state)[unfolded - HEIGHT..]
    );
    assert_eq!(
        state.entry(0).expect("entry is retained").lines,
        vec!["n0".to_owned()],
        "folding never drops retained body text"
    );
}

// -------------------------------------------------------------------------
// Viewport: scrollback clamping
// -------------------------------------------------------------------------

#[test]
fn scrollback_clamps_to_the_available_history() {
    let mut state = state_with_notices(20);
    state.set_viewport(WIDTH, HEIGHT);
    assert_eq!(
        state.scrollback(),
        0,
        "a fresh view starts at the live tail"
    );

    state.scroll_up(usize::MAX);
    assert_eq!(
        state.scrollback(),
        state.total_height(WIDTH) - HEIGHT,
        "an absurd scroll stops at the oldest reachable line"
    );
    state.scroll_up(usize::MAX);
    assert_eq!(
        state.scrollback(),
        state.total_height(WIDTH) - HEIGHT,
        "scrolling further at the top is a no-op"
    );
    state.scroll_up(0);
    assert_eq!(
        state.scrollback(),
        state.total_height(WIDTH) - HEIGHT,
        "a zero-line scroll changes nothing"
    );

    let view = state.visible_lines(WIDTH, HEIGHT);
    assert_eq!(
        view.hidden_above, 0,
        "the top of the conversation is visible"
    );
    assert_eq!(view.lines.len(), HEIGHT);
    assert_eq!(
        view.lines[0], "system",
        "the oldest title is the first line"
    );
}

#[test]
fn scrollback_is_zero_when_the_whole_conversation_fits() {
    let mut state = state_with_notices(3);
    state.set_viewport(WIDTH, 20);
    state.scroll_up(usize::MAX);
    assert_eq!(state.scrollback(), 0, "nothing to scroll when content fits");
    assert_eq!(state.visible_lines(WIDTH, 20).hidden_above, 0);

    // A frame exactly the content height is still "fits".
    let mut exact = state_with_notices(3);
    let content = exact.total_height(WIDTH);
    exact.set_viewport(WIDTH, content);
    exact.scroll_up(usize::MAX);
    assert_eq!(
        exact.scrollback(),
        0,
        "an exactly-fitting frame has no history above it"
    );
}

#[test]
fn scroll_down_saturates_at_the_tail_and_releases_the_selection() {
    let mut state = state_with_notices(20);
    state.set_viewport(WIDTH, HEIGHT);
    state.move_selection(-8);
    assert_eq!(state.selected(), Some(12));
    let pinned = state.scrollback();
    assert_eq!(pinned, 14, "the window scrolled to keep the selection");

    state.scroll_down(2);
    assert_eq!(state.scrollback(), pinned - 2);
    state.scroll_down(usize::MAX);
    assert_eq!(state.scrollback(), 0, "scrolling down saturates at zero");
    assert_eq!(
        state.selected(),
        None,
        "reaching the tail releases the selection so live output is followed"
    );
    let view = state.visible_lines(WIDTH, HEIGHT);
    assert_eq!(view.hidden_above, state.total_height(WIDTH) - HEIGHT);
    assert_eq!(view.lines[view.lines.len() - 2].as_str(), "  n19");
}

#[test]
fn growing_the_frame_re_clamps_an_impossible_scroll_position() {
    let mut state = state_with_notices(20);
    state.set_viewport(WIDTH, 4);
    state.scroll_up(usize::MAX);
    assert_eq!(state.scrollback(), 56);
    assert_eq!(state.visible_lines(WIDTH, 4).hidden_above, 0);

    // A taller frame cannot keep a scroll position that no longer exists.
    state.set_viewport(WIDTH, HEIGHT);
    assert_eq!(
        state.scrollback(),
        state.total_height(WIDTH) - HEIGHT,
        "the scroll position is re-clamped to the new frame"
    );
    // A maximal scroll position anchors the window at the top of the
    // conversation, so the visible rows are the oldest ones and the newest
    // entry sits below the fold: nothing is hidden above the window.
    let view = state.visible_lines(WIDTH, HEIGHT);
    assert_eq!(view.hidden_above, 0, "the window starts at the oldest line");
    assert_eq!(view.lines.len(), HEIGHT);
    assert_eq!(view.lines.first().map(String::as_str), Some("system"));
    assert_eq!(
        view.lines.last().map(String::as_str),
        Some("system"),
        "HEIGHT rows of three-line entries end on the fourth notice title"
    );
}

#[test]
fn shrinking_the_frame_keeps_a_valid_scroll_position() {
    let mut state = state_with_notices(20);
    state.set_viewport(WIDTH, 20);
    state.scroll_up(5);
    assert_eq!(state.scrollback(), 5);

    state.set_viewport(WIDTH, HEIGHT);
    assert_eq!(
        state.scrollback(),
        5,
        "a smaller frame still admits this scroll position"
    );

    let view = state.visible_lines(WIDTH, HEIGHT);
    assert_eq!(
        view.hidden_above,
        state.total_height(WIDTH) - HEIGHT - 5,
        "the window start moved up by exactly the scroll"
    );
    assert_eq!(view.lines.len(), HEIGHT);
    assert!(
        view.lines.iter().any(|line| *line == "  n15"),
        "the first body line five rows above the tail is visible: {:?}",
        view.lines
    );
    assert!(
        !view.lines.iter().any(|line| line.contains("n19")),
        "the newest entry is below the fold: {:?}",
        view.lines
    );
}

// -------------------------------------------------------------------------
// Viewport: page scroll survives viewport recalculation
// -------------------------------------------------------------------------

#[test]
fn page_scroll_survives_every_frame_recalculation() {
    let mut state = state_with_notices(20);
    state.set_viewport(WIDTH, HEIGHT);
    state.scroll_up(HEIGHT);
    let page = state.scrollback();
    assert_eq!(page, HEIGHT);

    // Repeated recalculation with the same geometry is idempotent, and
    // reading the window never mutates the scroll position.
    for _ in 0..5 {
        state.set_viewport(WIDTH, HEIGHT);
        assert_eq!(state.scrollback(), page, "a redraw keeps the page");
        let before = state.visible_lines(WIDTH, HEIGHT).lines;
        assert_eq!(state.scrollback(), page, "rendering is read-only");
        assert_eq!(state.visible_lines(WIDTH, HEIGHT).lines, before);
        assert!(state.truncation_indicator(WIDTH, HEIGHT));
    }

    // One page down returns exactly to the live tail.
    state.scroll_down(HEIGHT);
    assert_eq!(state.scrollback(), 0);
    let tail = state.visible_lines(WIDTH, HEIGHT).lines;
    assert_eq!(
        tail[tail.len() - 2].as_str(),
        "  n19",
        "the newest body line sits above the trailing separator"
    );

    // Scrolling back up lands on exactly the same page.
    state.scroll_up(HEIGHT);
    assert_eq!(state.scrollback(), page);
    let total = state.total_height(WIDTH);
    assert_eq!(
        state.visible_lines(WIDTH, HEIGHT).lines,
        whole_transcript(&state)[total - 2 * HEIGHT..][..HEIGHT]
    );
}

#[test]
fn a_scrolled_view_is_not_stolen_by_streaming_output() {
    let mut state = state_with_notices(20);
    state.set_viewport(WIDTH, HEIGHT);
    state.scroll_up(6);
    assert_eq!(state.scrollback(), 6);
    let before = state.visible_lines(WIDTH, HEIGHT).lines;
    assert_eq!(before.len(), HEIGHT);
    assert!(
        !before.iter().any(|line| line.contains("n19")),
        "the tail is not in view while scrolled: {before:?}"
    );

    // New output arrives while the user is reading history.
    state.notice("live tail");
    state.notice("and a question");

    assert_eq!(
        state.scrollback(),
        6,
        "new output never resets an explicit scroll position"
    );
    let view = state.visible_lines(WIDTH, HEIGHT);
    assert_eq!(view.lines.len(), HEIGHT);
    assert_eq!(
        view.hidden_above,
        state.total_height(WIDTH) - HEIGHT - 6,
        "the window is still anchored the same distance above the tail"
    );
    assert!(
        !view.lines.iter().any(|line| line.contains("live tail")),
        "the new notice is below the fold: {:?}",
        view.lines
    );
    assert!(
        !view
            .lines
            .iter()
            .any(|line| line.contains("and a question")),
        "the second new notice is below the fold: {:?}",
        view.lines
    );

    // Scrolling back to the tail re-follows the newest output.
    state.scroll_down(usize::MAX);
    assert_eq!(state.scrollback(), 0);
    let tail = state.visible_lines(WIDTH, HEIGHT).lines;
    assert_eq!(
        tail[tail.len() - 2].as_str(),
        "  and a question",
        "the newest body line sits above the trailing separator"
    );
}

#[test]
fn submitting_resets_the_scroll_position_and_the_selection() {
    let mut state = state_with_notices(20);
    state.set_viewport(WIDTH, HEIGHT);
    state.scroll_up(HEIGHT);
    state.move_selection(-3);
    assert!(state.scrollback() > 0, "the selection pinned the window");
    assert!(state.selected().is_some());

    state.record_submitted("new turn");

    assert_eq!(state.scrollback(), 0, "a submission re-follows the tail");
    assert_eq!(state.selected(), None, "the selection is cleared");
    let view = state.visible_lines(WIDTH, HEIGHT);
    assert_eq!(view.hidden_above, state.total_height(WIDTH) - HEIGHT);
    assert_eq!(view.lines[view.lines.len() - 2].as_str(), "  new turn");
}

#[test]
fn selection_movement_is_bounded_at_both_ends() {
    let mut state = state_with_notices(20);
    state.set_viewport(WIDTH, 4);
    let last = state.entry_count() - 1;

    assert_eq!(state.selected(), None, "a fresh view follows the live tail");
    state.move_selection(-1);
    assert_eq!(
        state.selected(),
        Some(last),
        "one step up selects the newest"
    );
    assert_eq!(state.scrollback(), 0, "the newest entry needs no scroll");
    state.move_selection(1);
    assert_eq!(
        state.selected(),
        Some(last),
        "the newest entry is the ceiling"
    );
    state.move_selection(0);
    assert_eq!(state.selected(), Some(last));

    state.move_selection(isize::MIN);
    assert_eq!(state.selected(), Some(0), "movement clamps at the oldest");
    assert_eq!(state.scrollback(), 56, "the window scrolled to the oldest");
    state.move_selection(-1);
    assert_eq!(state.selected(), Some(0));

    state.move_selection(isize::MAX);
    assert_eq!(
        state.selected(),
        Some(last),
        "movement clamps at the newest"
    );
    assert_eq!(
        state.scrollback(),
        0,
        "the tail is followed again, so the window is bottom-anchored"
    );

    // An empty conversation has nothing to select.
    let mut empty = AppState::new();
    empty.move_selection(-1);
    empty.move_selection(1);
    assert_eq!(empty.selected(), None);
    assert_eq!(empty.scrollback(), 0);
}

#[test]
fn a_selected_entry_stays_inside_the_window_in_both_directions() {
    let mut state = state_with_notices(20);
    state.set_viewport(WIDTH, HEIGHT);
    let count = state.entry_count();

    // Walking down from the oldest entry: every selected entry is visible.
    state.move_selection(-(count as isize));
    for index in 0..count {
        assert_eq!(state.selected(), Some(index), "walked to entry {index}");
        assert_entry_visible(&state, index, HEIGHT);
        state.move_selection(1);
    }
    assert_eq!(
        state.selected(),
        Some(count - 1),
        "the walk stops at the end"
    );

    // Walking back up: every intermediate entry is visible too.
    for expected in (0..count - 1).rev() {
        state.move_selection(-1);
        assert_eq!(
            state.selected(),
            Some(expected),
            "walked back to {expected}"
        );
        assert_entry_visible(&state, expected, HEIGHT);
    }
    assert_eq!(state.scrollback(), state.total_height(WIDTH) - HEIGHT);
}

#[test]
fn viewport_geometry_is_reported_and_folds_are_exactly_addressed() {
    let mut state = state_with_notices(4);
    state.set_viewport(WIDTH, 7);
    assert_eq!(state.viewport_height(), 7);
    assert!(state.entry(usize::MAX).is_none());

    // Out-of-range folds are refused; in-range folds are exact and reversible.
    assert!(!state.toggle_fold(state.entry_count()));
    assert!(!state.toggle_fold(usize::MAX));
    assert!(state.toggle_fold(0));
    assert!(state.entry(0).expect("entry is retained").folded);
    assert!(state.toggle_fold(0));
    assert!(!state.entry(0).expect("entry is retained").folded);
    assert!(state.toggle_fold(3));
    assert!(state.entry(3).expect("entry is retained").folded);
    assert!(!state.entry(0).expect("entry is retained").folded);
}

// -------------------------------------------------------------------------
// Composer: bounded, multiline editing
// -------------------------------------------------------------------------

#[test]
fn composer_accepts_text_and_deletes_one_char_at_a_time() {
    let mut state = AppState::new();
    assert_eq!(state.composer(), "");
    for char in "ship it".chars() {
        state.composer_type(char);
    }
    assert_eq!(state.composer(), "ship it");

    assert!(state.composer_backspace());
    assert_eq!(state.composer(), "ship i");
    assert!(state.composer_backspace());
    assert_eq!(state.composer(), "ship ");
    assert!(state.composer_backspace());
    assert_eq!(state.composer(), "ship");

    // Backspacing a multi-byte character removes the whole character.
    state.composer_type('é');
    state.composer_type('漢');
    assert_eq!(state.composer(), "shipé漢");
    assert!(state.composer_backspace());
    assert_eq!(state.composer(), "shipé", "a whole char is deleted");
    assert!(
        !state.composer().contains('\u{FFFD}'),
        "no replacement char was left behind"
    );
}

#[test]
fn composer_caret_byte_offset_tracks_utf8_edits_and_movement() {
    let mut state = AppState::new();
    for char in "aé漢b".chars() {
        state.composer_type(char);
    }

    state.caret_left();
    state.caret_left();
    state.composer_type('🙂');
    assert_eq!(state.composer(), "aé🙂漢b");
    assert!(state.composer_backspace());
    assert_eq!(state.composer(), "aé漢b");

    state.caret_left();
    state.composer_newline();
    assert_eq!(state.composer(), "a\né漢b");
    state.caret_right();
    state.composer_type('!');
    assert_eq!(state.composer(), "a\né!漢b");
}

#[test]
fn composer_never_exceeds_the_input_limit() {
    let mut state = AppState::new();
    for _ in 0..MAX_COMPOSER_BYTES {
        state.composer_type('x');
    }
    assert_eq!(state.composer().len(), MAX_COMPOSER_BYTES);
    state.composer_type('x');
    assert_eq!(
        state.composer().len(),
        MAX_COMPOSER_BYTES,
        "typing past the bound is dropped, never truncated"
    );

    // Two bytes short: a 2-byte char lands exactly on the bound.
    let mut partial = AppState::new();
    for _ in 0..(MAX_COMPOSER_BYTES - 2) {
        partial.composer_type('x');
    }
    assert_eq!(partial.composer().len(), MAX_COMPOSER_BYTES - 2);
    partial.composer_type('é');
    assert_eq!(partial.composer().len(), MAX_COMPOSER_BYTES);
    assert!(
        partial
            .composer()
            .is_char_boundary(partial.composer().len()),
        "the draft stays valid UTF-8"
    );
    partial.composer_type('é');
    assert_eq!(
        partial.composer().len(),
        MAX_COMPOSER_BYTES,
        "the bound still holds for multi-byte input"
    );

    // Backspace removes a whole char, so deleting the `é` frees both of its
    // bytes and the draft lands two bytes short of the bound.
    assert!(partial.composer_backspace());
    assert_eq!(
        partial.composer().len(),
        MAX_COMPOSER_BYTES - 2,
        "backspace removes the whole multi-byte char"
    );
    partial.composer_type('é');
    assert_eq!(
        partial.composer().len(),
        MAX_COMPOSER_BYTES,
        "the freed pair of bytes fits one more multi-byte char exactly"
    );

    // One byte short: a 2-byte char is rejected whole rather than split,
    // and the one remaining byte is still usable by a 1-byte char.
    let mut one_short = AppState::new();
    for _ in 0..MAX_COMPOSER_BYTES {
        one_short.composer_type('x');
    }
    assert!(one_short.composer_backspace());
    assert_eq!(one_short.composer().len(), MAX_COMPOSER_BYTES - 1);
    one_short.composer_type('é');
    assert_eq!(
        one_short.composer().len(),
        MAX_COMPOSER_BYTES - 1,
        "a char that would cross the bound is rejected whole"
    );
    assert!(
        one_short
            .composer()
            .is_char_boundary(one_short.composer().len()),
        "a rejected char leaves no partial sequence"
    );
    assert!(
        one_short.composer().ends_with('x'),
        "the rejected 2-byte char was not partially written"
    );
    one_short.composer_type('x');
    assert_eq!(one_short.composer().len(), MAX_COMPOSER_BYTES);
    one_short.composer_type('x');
    assert_eq!(
        one_short.composer().len(),
        MAX_COMPOSER_BYTES,
        "the bound is never exceeded after rejections"
    );

    // Newlines share the same budget and are refused once it is spent.
    let mut lines = AppState::new();
    for _ in 0..MAX_COMPOSER_BYTES {
        lines.composer_newline();
    }
    assert_eq!(lines.composer().len(), MAX_COMPOSER_BYTES);
    assert_eq!(lines.composer().matches('\n').count(), MAX_COMPOSER_BYTES);
    lines.composer_newline();
    assert_eq!(lines.composer().len(), MAX_COMPOSER_BYTES);
    assert!(lines.composer_backspace());
    lines.composer_newline();
    assert_eq!(lines.composer().len(), MAX_COMPOSER_BYTES);
}

#[test]
fn composer_is_multiline_through_the_explicit_newline_only() {
    let mut state = AppState::new();
    state.composer_type('a');
    state.composer_newline();
    state.composer_type('b');
    state.composer_newline();
    state.composer_newline();
    state.composer_type('c');
    assert_eq!(state.composer(), "a\nb\n\nc", "a blank line is intentional");
    assert_eq!(state.composer().lines().count(), 4);

    // Newlines never arrive implicitly.
    state.composer_type('\n');
    assert_eq!(state.composer(), "a\nb\n\nc");
    state.composer_type('\r');
    assert_eq!(state.composer(), "a\nb\n\nc", "no CRLF is introduced");
    state.composer_type('\t');
    assert_eq!(state.composer(), "a\nb\n\nc", "no tab is introduced");

    // A leading newline is a legal draft: the user pressed Enter first.
    let mut leading = AppState::new();
    leading.composer_newline();
    leading.composer_type('x');
    assert_eq!(leading.composer(), "\nx");

    // Backspace deletes exactly one char, including newlines; deleting the
    // newline that separates two lines merges them back together.
    assert!(state.composer_backspace());
    assert_eq!(state.composer(), "a\nb\n\n");
    assert!(state.composer_backspace());
    assert_eq!(state.composer(), "a\nb\n");
    assert!(state.composer_backspace());
    assert_eq!(state.composer(), "a\nb");
    assert!(state.composer_backspace());
    assert_eq!(state.composer(), "a\n");
    assert!(state.composer_backspace());
    assert_eq!(state.composer(), "a", "the joining newline was deleted");
    assert!(state.composer_backspace());
    assert_eq!(state.composer(), "");
    assert!(
        !state.composer_backspace(),
        "backspace on an empty draft is refused"
    );
}

#[test]
fn composer_take_returns_the_draft_and_clears_it() {
    let mut state = AppState::new();
    for char in "ru".chars() {
        state.composer_type(char);
    }
    state.composer_newline();
    state.composer_type('n');

    let draft = state.composer_take();
    assert_eq!(draft, "ru\nn");
    assert_eq!(state.composer(), "", "taking empties the draft");
    assert_eq!(
        state.composer_take(),
        "",
        "taking an empty draft yields nothing"
    );

    // The cleared draft accepts a completely new submission.
    for char in "ok".chars() {
        state.composer_type(char);
    }
    assert_eq!(state.composer(), "ok");
    assert_eq!(state.composer_take(), "ok");
    assert_eq!(state.composer(), "");
}

#[test]
fn a_submitted_draft_is_recorded_as_multiline_user_text() {
    let mut state = state_with_notices(2);
    let before = state.entry_count();
    state.record_submitted("first\nsecond\n\nfourth");

    assert_eq!(state.entry_count(), before + 1);
    let entry = state.entry(before).expect("the submission is retained");
    assert_eq!(entry.title, "you");
    assert_eq!(
        entry.lines,
        vec![
            "first".to_owned(),
            "second".to_owned(),
            String::new(),
            "fourth".to_owned()
        ],
        "one entry, one line per draft line"
    );
    assert!(!entry.truncated, "a short submission is not cut");
    assert_eq!(state.composer(), "", "submission does not itself type text");
}

// -------------------------------------------------------------------------
// Composer: control and bidi formatting rejection
// -------------------------------------------------------------------------

#[test]
fn composer_rejects_every_control_and_bidi_code_point() {
    let mut state = AppState::new();

    // C0 controls and DEL.
    for code in 0u32..=0x1f {
        state.composer_type(char::from_u32(code).expect("C0 code point"));
    }
    state.composer_type('\u{7f}');
    // C1 controls.
    for code in 0x80u32..=0x9f {
        state.composer_type(char::from_u32(code).expect("C1 code point"));
    }
    assert_eq!(state.composer(), "", "no C0/C1 control enters the draft");

    // Bidi formatting: marks, embeddings/overrides, isolates, and the
    // deprecated formatting block.
    for (start, end) in [
        (0x061Cu32, 0x061Cu32),
        (0x200Eu32, 0x200Fu32),
        (0x202Au32, 0x202Eu32),
        (0x2066u32, 0x2069u32),
        (0x206Au32, 0x206Fu32),
    ] {
        for code in start..=end {
            state.composer_type(char::from_u32(code).expect("bidi code point"));
        }
    }
    assert_eq!(state.composer(), "", "no bidi formatting control enters");

    // Ordinary multilingual text still types, including non-Latin scripts.
    for char in ['é', '漢', 'ا', 'ل', 'ع', 'Ω', '🌍', 'ß'].iter().copied() {
        state.composer_type(char);
    }
    assert_eq!(state.composer(), "é漢العΩ🌍ß");

    // Rejection is total, not first-char-only: a hostile char in the middle
    // of a draft cannot corrupt it.
    state.composer_type('a');
    state.composer_type('\u{202E}');
    state.composer_type('b');
    state.composer_type('\u{1b}');
    state.composer_type('\t');
    state.composer_type('c');
    assert_eq!(
        state.composer(),
        "é漢العΩ🌍ßabc",
        "only the explicit newline binding inserts a line break"
    );
    assert!(!state.composer().contains('\t'));
    assert!(
        state
            .composer()
            .chars()
            .all(|char| !char.is_control() && char != '\n'),
        "the draft holds nothing the renderer would have to hide: {:?}",
        state.composer()
    );
}

#[test]
fn composer_rejects_hostile_characters_at_the_end_of_a_draft() {
    let mut state = AppState::new();
    for char in "safe".chars() {
        state.composer_type(char);
    }
    state.composer_type('\u{202E}');
    assert_eq!(state.composer(), "safe");
    assert!(
        !state.composer().ends_with('\u{202E}'),
        "a trailing reorder override is not stored"
    );

    // A rejected char must not consume a keystroke position either: the
    // next accepted char lands immediately after the previous one.
    state.composer_type('!');
    assert_eq!(state.composer(), "safe!");

    // And the bound check still applies after rejections.
    for _ in 0..MAX_COMPOSER_BYTES {
        state.composer_type('y');
    }
    assert_eq!(state.composer().len(), MAX_COMPOSER_BYTES);
    state.composer_type('\u{202E}');
    state.composer_type('y');
    assert_eq!(
        state.composer().len(),
        MAX_COMPOSER_BYTES,
        "rejections at the bound leave the draft at the bound"
    );
}

// -------------------------------------------------------------------------
// Approval: exact-arguments preview is sanitized, bounded, and cleared
// -------------------------------------------------------------------------

/// A notice built through the struct literal rather than the validating
/// constructor, so a field can exceed the bounds the runtime publishes. The
/// presentation boundary must not trust the producer.
fn forged_notice(seq: u64, summary: String, scope: String, args: Option<String>) -> RunEvent {
    run_event(
        seq,
        EventPayload::ApprovalRequired(ApprovalNotice {
            approval: ApprovalId::new("a1-0").expect("valid approval id"),
            call: call("c1-0"),
            summary,
            scope_summary: scope,
            args_preview: args,
            session_directory: None,
            expires_at_elapsed: EXPIRY,
        }),
    )
}

fn state_with_notice(notice: &RunEvent) -> AppState {
    let mut state = AppState::new();
    assert!(state.apply_event(&started(0)));
    assert!(state.apply_event(notice));
    state
}

fn measured(clipped: bool) -> ApprovalGeometry {
    ApprovalGeometry {
        inner_width: 78,
        inner_rows: 20,
        detail_rows: 4,
        clipped,
    }
}

#[test]
fn approval_args_preview_is_sanitized_and_flattened() {
    let hostile = "path\u{1b}[2J\u{202E}\u{200B}src\n--force\u{7f}".to_owned();
    let mut state = state_with_notice(&forged_notice(
        1,
        "run tool host_write".to_owned(),
        "project scope".to_owned(),
        Some(hostile),
    ));

    let card = state.pending_approval().expect("a card is pending");
    assert_eq!(card.summary, "run tool host_write");
    assert_eq!(card.scope_summary, "project scope");

    let args = state
        .approval_args_preview()
        .expect("the preview is retained");
    assert!(
        !args.contains('\x1b'),
        "escape sequences are stripped: {args:?}"
    );
    assert!(!args.contains('\u{7f}'), "DEL is stripped: {args:?}");
    assert!(!args.contains('\n'), "newlines are flattened: {args:?}");
    assert!(!args.contains('\u{200B}'), "invisible chars are escaped");
    assert!(
        args.contains("path") && args.contains("--force"),
        "the visible text survives: {args:?}"
    );
    assert!(
        args.contains("202E"),
        "a reorder attempt is visible as an escape: {args:?}"
    );
    assert!(
        !state.approval_detail_truncated(),
        "a short preview is not cut"
    );

    state.set_approval_geometry(measured(false));
    assert!(
        state.approval_decision_allowed(),
        "a fully visible, uncut card can be decided"
    );
}

#[test]
fn approval_args_preview_is_bounded_at_the_field_cap() {
    // A preview exactly at the cap is retained whole.
    let at_cap = "a".repeat(MAX_APPROVAL_FIELD_BYTES);
    let exact = state_with_notice(&forged_notice(
        1,
        "summary".to_owned(),
        "scope".to_owned(),
        Some(at_cap.clone()),
    ));
    assert_eq!(
        exact.approval_args_preview(),
        Some(at_cap.as_str()),
        "a preview exactly at the cap is not cut"
    );
    assert!(!exact.approval_detail_truncated());

    // One byte more is cut at the cap and reported as lost detail.
    let mut over = state_with_notice(&forged_notice(
        1,
        "summary".to_owned(),
        "scope".to_owned(),
        Some("a".repeat(MAX_APPROVAL_FIELD_BYTES + 1)),
    ));
    let preview = over
        .approval_args_preview()
        .expect("the preview is retained");
    assert_eq!(preview.len(), MAX_APPROVAL_FIELD_BYTES, "cut at the cap");
    assert!(over.approval_detail_truncated(), "the cut is reported");

    // A cut preview locks the decision permanently, however it is measured.
    over.set_approval_geometry(measured(false));
    assert!(!over.approval_decision_allowed());
    over.inspect_approval();
    over.record_approval_detail_view(0, 64, 4);
    assert!(over.approval_detail_seen_all());
    assert!(
        !over.approval_decision_allowed(),
        "full inspection cannot unlock a field lost at storage"
    );
}

#[test]
fn approval_args_preview_never_pushes_the_card_past_its_display_budget() {
    let state = state_with_notice(&forged_notice(
        1,
        "s".repeat(MAX_APPROVAL_FIELD_BYTES * 2),
        "p".repeat(MAX_APPROVAL_FIELD_BYTES * 2),
        Some("a".repeat(MAX_APPROVAL_FIELD_BYTES * 4)),
    ));
    let card = state.pending_approval().expect("a card is pending");
    let args = state
        .approval_args_preview()
        .expect("the preview is retained");

    assert!(card.summary.len() <= MAX_APPROVAL_FIELD_BYTES);
    assert!(card.scope_summary.len() <= MAX_APPROVAL_FIELD_BYTES);
    assert!(args.len() <= MAX_APPROVAL_FIELD_BYTES);
    let total = card.summary.len() + card.scope_summary.len() + args.len();
    assert!(
        total <= MAX_APPROVAL_CARD_BYTES,
        "the retained card is {total} bytes, over the {MAX_APPROVAL_CARD_BYTES}-byte budget"
    );
    assert!(
        state.approval_detail_truncated(),
        "an over-budget notice reports lost detail"
    );
}

#[test]
fn approval_args_preview_is_cleared_by_every_resolve_path() {
    // A plain preview is retained verbatim.
    let mut explicit = state_with_notice(&forged_notice(
        1,
        "run tool host_write".to_owned(),
        "project scope".to_owned(),
        Some("{\"path\":\"src\"}".to_owned()),
    ));
    assert_eq!(explicit.approval_args_preview(), Some("{\"path\":\"src\"}"));

    explicit.resolve_approval();
    assert!(explicit.approval_args_preview().is_none());
    assert!(explicit.pending_approval().is_none());
    assert!(!explicit.approval_detail_truncated());
    assert!(!explicit.approval_detail_open());
    assert!(!explicit.approval_decision_allowed());

    // A notice published without a preview leaves nothing to show.
    let absent = state_with_notice(&forged_notice(
        1,
        "run tool host_write".to_owned(),
        "project scope".to_owned(),
        None,
    ));
    assert!(absent.approval_args_preview().is_none());

    // The matching tool outcome resolves the card and drops the preview.
    let mut finished = state_with_notice(&forged_notice(
        1,
        "run tool host_write".to_owned(),
        "project scope".to_owned(),
        Some("{\"path\":\"src\"}".to_owned()),
    ));
    assert!(finished.apply_event(&tool_finished(2, "c1-0", "wrote src")));
    assert!(
        finished.approval_args_preview().is_none(),
        "the finished call cleared the preview"
    );
    assert!(finished.pending_approval().is_none());

    // The terminal run outcome resolves it too.
    let mut terminal = state_with_notice(&forged_notice(
        1,
        "run tool host_write".to_owned(),
        "project scope".to_owned(),
        Some("{\"path\":\"src\"}".to_owned()),
    ));
    assert!(
        terminal.apply_event(&run_event(
            2,
            EventPayload::RunFinished(
                RunFinished::new(RunOutcome::Completed, PersistenceState::Ephemeral, None)
                    .expect("valid terminal record"),
            )
        ))
    );
    assert!(terminal.approval_args_preview().is_none());
    assert!(terminal.pending_approval().is_none());
}

#[test]
fn an_unrelated_tool_outcome_does_not_resolve_a_pending_card() {
    let mut state = state_with_notice(&forged_notice(
        1,
        "run tool host_write".to_owned(),
        "project scope".to_owned(),
        Some("{\"path\":\"src\"}".to_owned()),
    ));

    assert!(state.apply_event(&tool_finished(2, "c9-9", "unrelated")));
    assert!(
        state.pending_approval().is_some(),
        "another call's outcome cannot clear this card"
    );
    assert_eq!(state.approval_args_preview(), Some("{\"path\":\"src\"}"));

    assert!(state.apply_event(&tool_finished(3, "c1-0", "wrote src")));
    assert!(state.pending_approval().is_none());
    assert!(state.approval_args_preview().is_none());
}

#[test]
fn a_replacement_notice_replaces_the_preview_and_resets_measurement() {
    let mut state = state_with_notice(&forged_notice(
        1,
        "run tool host_write".to_owned(),
        "project scope".to_owned(),
        Some("{\"path\":\"first\"}".to_owned()),
    ));
    state.set_approval_geometry(measured(false));
    state.inspect_approval();
    state.record_approval_detail_view(0, 8, 4);
    assert!(state.approval_detail_seen_all());
    assert!(state.approval_decision_allowed());

    // A second notice on the same run replaces the card wholesale.
    assert!(state.apply_event(&forged_notice(
        2,
        "run tool host_read".to_owned(),
        "project scope".to_owned(),
        Some("{\"path\":\"second\"}".to_owned()),
    )));
    assert_eq!(
        state.approval_args_preview(),
        Some("{\"path\":\"second\"}"),
        "the stale preview is replaced, never appended"
    );
    assert!(
        state.approval_geometry().is_none(),
        "the old measurement is dropped"
    );
    assert!(!state.approval_detail_open(), "the detail view is closed");
    assert!(!state.approval_detail_seen_all());
    assert!(
        !state.approval_decision_allowed(),
        "the new card must be measured again before it can be decided"
    );

    state.set_approval_geometry(measured(false));
    assert!(state.approval_decision_allowed());
}

#[test]
fn a_notice_without_a_preview_clears_a_previous_preview() {
    let mut state = state_with_notice(&forged_notice(
        1,
        "run tool host_write".to_owned(),
        "project scope".to_owned(),
        Some("{\"path\":\"src\"}".to_owned()),
    ));
    assert!(state.approval_args_preview().is_some());

    assert!(state.apply_event(&forged_notice(
        2,
        "run tool host_write".to_owned(),
        "project scope".to_owned(),
        None,
    )));
    assert!(
        state.approval_args_preview().is_none(),
        "an absent preview does not leave the previous one on screen"
    );
}
