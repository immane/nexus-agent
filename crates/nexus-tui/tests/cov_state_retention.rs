//! Coverage hardening for `AppState` presentation retention.
//!
//! Integration-level companion to the unit tests inside `nexus_tui::state`:
//! it drives the public API only (`record_submitted`, `notice`,
//! `apply_event`, the fold controls, and the read-only accessors) so the
//! retention guarantees hold for the entry-producing paths a caller actually
//! uses, not only for the crate-private push helpers.
//!
//! The contract under test:
//! - retained entries are capped by [`MAX_RETAINED_ENTRIES`] and every drop
//!   is counted in `AppState::dropped_entries` and surfaced to the viewport;
//! - retention drops the OLDEST entries first and keeps retained order, and
//!   the aggregate budget drops before the entry-count cap binds;
//! - retained bytes always equal the per-entry accounting (title plus per-line
//!   bookkeeping plus line bytes) and never exceed either budget;
//! - folding collapses an entry to exactly its title marker without changing
//!   retained content, byte accounting, or retention;
//! - text that overflows an entry keeps the longest safe prefix, sets the
//!   truncation flag, and is never charged past the entry bound;
//! - truncation never splits a UTF-8 character, on the whole-line path or
//!   while appending a streamed fragment.
//!
//! Determinism: no clock, randomness, threads, or external I/O; every input
//! is built from the exported budget constants.

#![forbid(unsafe_code)]

use std::mem::size_of;

use nexus_core::{
    AssistantText, CallId, EventPayload, RequestId, RunEvent, RunId, SessionId, ToolProgress,
    TurnId,
};
use nexus_tui::state::{Entry, MAX_ENTRY_BYTES, MAX_RETAINED_BYTES, MAX_RETAINED_ENTRIES};
use nexus_tui::{AppState, EntryKind};

/// Mirrors the module-private marker appended after cut entry text.
const TRUNCATION_MARK: &str = "[output truncated to presentation bound]";

/// Per-line `String` bookkeeping the module charges against the entry bound
/// (its `LINE_OVERHEAD`). The constant is private, so the byte-accounting
/// boundary tests pin the same value the module computes.
const LINE_OVERHEAD: usize = size_of::<String>();

/// Title of an entry created by [`AppState::record_submitted`].
const USER_TITLE: &str = "you";

/// Title of an entry created by an assistant text fragment.
const ASSISTANT_TITLE: &str = "assistant";

/// Largest body line that still fits one entry with `title`.
fn max_line_bytes(title: &str) -> usize {
    MAX_ENTRY_BYTES - title.len() - LINE_OVERHEAD
}

/// Retained bytes of one entry as the module accounts them: the title plus
/// per-line bookkeeping plus the line bytes.
fn entry_bytes(entry: &Entry) -> usize {
    entry.title.len()
        + entry.lines.len() * LINE_OVERHEAD
        + entry.lines.iter().map(String::len).sum::<usize>()
}

/// Asserts the documented accounting identity and both budgets after every
/// retention-changing operation.
fn assert_accounted(state: &AppState) {
    let mut expected = 0usize;
    for index in 0..state.entry_count() {
        let bytes = entry_bytes(state.entry(index).expect("retained entry exists"));
        assert!(
            bytes <= MAX_ENTRY_BYTES,
            "entry {index} stays within the per-entry bound"
        );
        expected += bytes;
    }
    assert_eq!(
        state.retained_bytes(),
        expected,
        "aggregate retained bytes match the per-entry accounting"
    );
    assert!(
        state.retained_bytes() <= MAX_RETAINED_BYTES,
        "aggregate retained bytes stay within the retention bound"
    );
    assert!(
        state.entry_count() <= MAX_RETAINED_ENTRIES,
        "retained entry count stays within the retention cap"
    );
}

fn last_entry(state: &AppState) -> &Entry {
    state.entry(state.entry_count() - 1).expect("entry exists")
}

/// Unique, sortable body text for one recorded submission.
fn marker(index: usize) -> String {
    format!("kept {index:03}")
}

/// Notice text long enough to fill one entry exactly, tagged with its index at
/// the front so the retained prefix still identifies the entry.
fn notice_text(index: usize) -> String {
    format!("{}{}", notice_head(index), "x".repeat(MAX_ENTRY_BYTES))
}

/// Leading marker of [`notice_text`], which always survives truncation.
fn notice_head(index: usize) -> String {
    format!("note {index:02}:")
}

fn ids() -> (SessionId, RunId) {
    (
        SessionId::new("sess-1").expect("valid session id"),
        RunId::new("run-1").expect("valid run id"),
    )
}

fn started(seq: u64) -> RunEvent {
    let (session, run) = ids();
    RunEvent::new(
        session,
        run,
        seq,
        EventPayload::RunStarted {
            request: RequestId::new("req-1").expect("valid request id"),
        },
    )
}

fn assistant_fragment(seq: u64, text: &str) -> RunEvent {
    let (session, run) = ids();
    RunEvent::new(
        session,
        run,
        seq,
        EventPayload::AssistantTextDelta(
            AssistantText::new(TurnId::new("t1-0").expect("valid turn id"), "item-0", text)
                .expect("valid fragment"),
        ),
    )
}

fn tool_output(seq: u64, call: &str, preview: &str, truncated: bool) -> RunEvent {
    let (session, run) = ids();
    RunEvent::new(
        session,
        run,
        seq,
        EventPayload::ToolOutput(
            ToolProgress::new(
                CallId::new(call).expect("valid call id"),
                preview,
                truncated,
            )
            .expect("valid progress"),
        ),
    )
}

#[test]
fn retained_entries_are_capped_with_visible_oldest_first_drops() {
    let mut state = AppState::new();
    let submitted = MAX_RETAINED_ENTRIES + 10;
    for index in 0..submitted {
        state.record_submitted(&marker(index));
        assert!(
            state.entry_count() <= MAX_RETAINED_ENTRIES,
            "the entry cap holds after every insertion"
        );
    }
    assert_eq!(state.entry_count(), MAX_RETAINED_ENTRIES);
    assert_eq!(
        state.dropped_entries,
        submitted - MAX_RETAINED_ENTRIES,
        "every eviction is counted as a presentation drop"
    );
    assert_accounted(&state);

    // The oldest submissions are the dropped ones; the rest keep their order.
    for index in 0..state.entry_count() {
        let entry = state.entry(index).expect("retained entry exists");
        assert_eq!(entry.kind, EntryKind::User);
        assert_eq!(
            entry.lines,
            vec![marker(index + 10)],
            "retention drops the oldest entries and preserves order"
        );
    }

    let view = state.visible_lines(80, 20);
    assert!(
        view.retention_truncated,
        "drops are visible to the viewport"
    );
    assert!(state.truncation_indicator(80, 20));
    assert!(view.hidden_above > 0);
    assert_eq!(view.lines.len(), 20, "the window never exceeds its height");

    let transcript = state.transcript();
    assert!(
        !transcript.contains(&marker(0)),
        "a dropped submission is gone from the transcript"
    );
    assert!(
        transcript.contains(&marker(submitted - 1)),
        "the newest submission is retained"
    );
}

#[test]
fn retention_drop_keeps_the_selected_entry_selected() {
    let mut state = AppState::new();
    state.set_viewport(80, 10);
    for index in 0..MAX_RETAINED_ENTRIES {
        state.record_submitted(&marker(index));
    }
    assert_eq!(state.entry_count(), MAX_RETAINED_ENTRIES);

    state.move_selection(-1);
    let back = -(isize::try_from(MAX_RETAINED_ENTRIES - 2).expect("cap fits isize"));
    state.move_selection(back);
    assert_eq!(state.selected(), Some(1));
    assert!(
        state.scrollback() > 0,
        "scrolled up, so the selection is not reset by the next insertion"
    );

    // One more entry forces one oldest-first drop.
    state.notice("newest notice");
    assert_eq!(state.dropped_entries, 1);
    assert_eq!(state.entry_count(), MAX_RETAINED_ENTRIES);
    assert_eq!(
        state.selected(),
        Some(0),
        "the selection index follows its entry across the drop"
    );
    let entry = state.entry(0).expect("retained entry exists");
    assert_eq!(entry.kind, EntryKind::User);
    assert_eq!(
        entry.lines,
        vec![marker(1)],
        "the selected entry survived the drop"
    );
    assert_accounted(&state);
}

#[test]
fn aggregate_byte_budget_drops_oldest_before_the_entry_cap() {
    let mut state = AppState::new();
    // Each notice fills exactly one entry to the per-entry bound, so the
    // aggregate budget binds after a known number of entries.
    let per_entry = MAX_RETAINED_BYTES / MAX_ENTRY_BYTES;
    for index in 0..(per_entry + 1) {
        state.notice(&notice_text(index));
        assert!(
            state.retained_bytes() <= MAX_RETAINED_BYTES,
            "the aggregate bound holds after every insertion"
        );
    }
    assert_eq!(
        state.entry_count(),
        per_entry,
        "one entry is dropped to restore the aggregate bound"
    );
    assert!(
        state.entry_count() < MAX_RETAINED_ENTRIES,
        "the byte budget binds before the entry-count cap"
    );
    assert_eq!(state.dropped_entries, 1);
    assert_eq!(state.retained_bytes(), per_entry * MAX_ENTRY_BYTES);
    assert_accounted(&state);

    // The oldest notice is the one that was dropped.
    for index in 0..state.entry_count() {
        let entry = state.entry(index).expect("retained entry exists");
        assert_eq!(entry.title, "system");
        assert_eq!(entry_bytes(entry), MAX_ENTRY_BYTES);
        assert!(entry.truncated);
        assert!(
            entry.lines[0].starts_with(&notice_head(index + 1)),
            "the oldest notice was dropped, later ones retained in order"
        );
    }
    assert!(
        !state
            .transcript()
            .iter()
            .any(|line| line.starts_with(&notice_head(0))),
        "the dropped notice is gone from the transcript"
    );
}

#[test]
fn folding_collapses_an_entry_to_its_title_marker() {
    let mut state = AppState::new();
    state.record_submitted("line one\nline two");
    let index = state.entry_count() - 1;
    let unfolded = state.visible_lines(80, 50).lines;
    let transcript = state.transcript();
    let bytes = state.retained_bytes();
    let unfolded_height = state.total_height(80);
    assert_eq!(unfolded, vec!["you", "  line one", "  line two"]);

    assert!(state.toggle_fold(index));
    assert!(
        !state.toggle_fold(state.entry_count() + 5),
        "an out-of-range fold is refused"
    );
    let entry = state.entry(index).expect("retained entry exists");
    assert!(entry.folded);
    assert_eq!(
        entry.lines,
        vec!["line one".to_owned(), "line two".to_owned()],
        "folding hides body text without dropping it"
    );
    assert_eq!(
        state.visible_lines(80, 50).lines,
        vec!["you  [folded, 2 lines]"],
        "a folded entry renders exactly its title marker"
    );
    assert_eq!(state.total_height(80), 1);
    assert!(state.total_height(80) < unfolded_height);
    assert_eq!(state.retained_bytes(), bytes, "folding changes no bytes");
    assert_eq!(state.transcript(), transcript, "folding changes no content");

    assert!(state.toggle_fold(index));
    assert!(!state.entry(index).expect("retained entry exists").folded);
    assert_eq!(
        state.visible_lines(80, 50).lines,
        unfolded,
        "unfolding restores the exact rendering"
    );
    assert_eq!(state.total_height(80), unfolded_height);
    assert_accounted(&state);
}

#[test]
fn folding_a_stream_entry_starts_a_fresh_entry_for_the_next_fragment() {
    let mut state = AppState::new();
    assert!(state.apply_event(&started(0)));
    assert!(state.apply_event(&assistant_fragment(1, "first")));
    let folded = state.entry_count() - 1;
    assert!(state.toggle_fold(folded));

    assert!(state.apply_event(&assistant_fragment(2, "second")));
    assert_eq!(
        state.entry_count(),
        3,
        "a folded entry no longer absorbs streamed appends"
    );
    let tail = last_entry(&state);
    assert_eq!(tail.kind, EntryKind::Assistant);
    assert_eq!(tail.title, ASSISTANT_TITLE);
    assert_eq!(tail.lines, vec!["second".to_owned()]);
    assert!(!tail.folded);
    assert!(state.entry(folded).expect("retained entry exists").folded);
    assert_accounted(&state);
}

#[test]
fn fold_all_and_unfold_all_cover_retained_entries_without_touching_retention() {
    let mut state = AppState::new();
    for index in 0..(MAX_RETAINED_ENTRIES + 1) {
        state.record_submitted(&marker(index));
    }
    assert_eq!(state.dropped_entries, 1);
    let retained = state.retained_bytes();
    // A truncated tail entry proves folding also hides the truncation marker.
    state.record_submitted(&"z".repeat(MAX_ENTRY_BYTES));
    assert!(last_entry(&state).truncated);
    assert_eq!(state.dropped_entries, 2);
    let bytes = state.retained_bytes();
    assert!(
        bytes > retained,
        "a truncated entry charges the entry bound"
    );
    assert_accounted(&state);

    state.fold_all();
    for index in 0..state.entry_count() {
        assert!(state.entry(index).expect("retained entry exists").folded);
    }
    assert_eq!(state.entry_count(), MAX_RETAINED_ENTRIES);
    assert_eq!(state.dropped_entries, 2);
    assert_eq!(state.retained_bytes(), bytes, "folding changes no bytes");

    assert_eq!(
        state.total_height(80),
        state.entry_count(),
        "each folded entry collapses to one marker line"
    );
    let folded = state.visible_lines(80, state.entry_count()).lines;
    assert_eq!(folded.len(), state.entry_count());
    assert!(folded.iter().all(|line| line.contains("[folded,")));
    assert!(
        !folded.iter().any(|line| line.contains("[output truncated")),
        "a folded entry hides its truncation marker too"
    );
    assert!(!folded.iter().any(|line| line.contains("kept ")));
    assert_eq!(
        folded.last().map(String::as_str),
        Some("you  [folded, 1 lines]"),
        "the folded window stays anchored to the newest retained entry"
    );

    state.unfold_all();
    for index in 0..state.entry_count() {
        assert!(!state.entry(index).expect("retained entry exists").folded);
    }
    assert_eq!(state.dropped_entries, 2);
    assert_eq!(state.retained_bytes(), bytes);
    let unfolded = state.visible_lines(80, state.entry_count() * 3).lines;
    assert!(
        unfolded
            .iter()
            .any(|line| line.contains(&marker(MAX_RETAINED_ENTRIES))),
        "unfolding restores retained body text"
    );
    assert!(
        unfolded
            .iter()
            .any(|line| line.trim_start() == TRUNCATION_MARK),
        "unfolding restores the truncation marker"
    );
    assert_accounted(&state);
}

#[test]
fn entry_byte_bound_is_exact_at_the_capacity_edge() {
    let capacity = max_line_bytes(USER_TITLE);
    assert!(capacity > 1, "the per-entry bound admits a body line");
    for (label, offered, expect_truncated) in [
        ("one under the bound", capacity - 1, false),
        ("exactly the bound", capacity, false),
        ("one byte over the bound", capacity + 1, true),
    ] {
        let mut state = AppState::new();
        state.record_submitted(&"x".repeat(offered));
        let entry = state.entry(0).expect("retained entry exists");
        assert_eq!(entry.title, USER_TITLE);
        assert_eq!(entry.truncated, expect_truncated, "{label}");
        let kept = offered.min(capacity);
        assert_eq!(
            entry.lines,
            vec!["x".repeat(kept)],
            "{label}: exactly the safe prefix is retained"
        );
        assert_eq!(entry_bytes(entry), USER_TITLE.len() + LINE_OVERHEAD + kept);
        assert!(entry_bytes(entry) <= MAX_ENTRY_BYTES);
        assert_eq!(entry_bytes(entry), state.retained_bytes(), "{label}");
        assert_eq!(
            state
                .transcript()
                .iter()
                .any(|line| line == TRUNCATION_MARK),
            expect_truncated,
            "{label}: the marker appears only when text was cut"
        );
        assert_accounted(&state);
    }
}

#[test]
fn oversized_entry_sets_the_truncation_flag_and_keeps_a_safe_prefix() {
    let capacity = max_line_bytes(USER_TITLE);
    let oversize = format!("{}{}", "x".repeat(capacity + 1), "tail that cannot fit");
    let mut state = AppState::new();
    state.record_submitted(&oversize);

    let entry = state.entry(0).expect("retained entry exists");
    assert!(entry.truncated, "cut text sets the truncation flag");
    assert_eq!(
        entry.lines,
        vec![oversize[..capacity].to_owned()],
        "exactly the safe prefix is kept, never dropped wholesale"
    );
    assert_eq!(state.retained_bytes(), MAX_ENTRY_BYTES);
    assert_eq!(state.dropped_entries, 0, "an entry is trimmed, not dropped");
    assert_eq!(
        state.transcript().last().map(String::as_str),
        Some(TRUNCATION_MARK)
    );
    assert!(
        !state
            .transcript()
            .iter()
            .any(|line| line.contains("tail that cannot fit"))
    );

    let view = state.visible_lines(200, 100);
    assert_eq!(view.hidden_above, 0);
    assert_eq!(
        view.lines.len(),
        state.total_height(200),
        "the wrapped height matches the rendered window"
    );
    assert!(
        view.lines
            .iter()
            .any(|line| line.trim_start() == TRUNCATION_MARK),
        "the truncation marker is rendered"
    );
    assert_accounted(&state);
}

#[test]
fn lines_beyond_the_entry_bound_are_dropped_not_exceeded() {
    let capacity = max_line_bytes(USER_TITLE);
    let first = "a".repeat(capacity + 1);
    let mut state = AppState::new();
    state.record_submitted(&format!("{first}\n{}", "Z".repeat(MAX_ENTRY_BYTES)));

    let entry = state.entry(0).expect("retained entry exists");
    assert_eq!(
        entry.lines,
        vec![first[..capacity].to_owned()],
        "the exhausted bound drops the next line entirely"
    );
    assert!(entry.truncated);
    assert_eq!(state.retained_bytes(), MAX_ENTRY_BYTES);
    assert_eq!(state.dropped_entries, 0);
    assert!(
        !state.transcript().iter().any(|line| line.contains('Z')),
        "the line past the bound is never retained"
    );
    assert_accounted(&state);

    // A trailing newline after the exhausted bound cannot add even an empty line.
    let mut trailing = AppState::new();
    trailing.record_submitted(&format!("{first}\n"));
    let entry = trailing.entry(0).expect("retained entry exists");
    assert_eq!(entry.lines, vec![first[..capacity].to_owned()]);
    assert!(entry.truncated);
    assert_accounted(&trailing);
}

#[test]
fn tool_progress_truncation_sets_the_flag_without_adding_a_line() {
    let mut state = AppState::new();
    assert!(state.apply_event(&started(0)));

    assert!(state.apply_event(&tool_output(1, "call-a", "", true)));
    let empty = last_entry(&state);
    assert_eq!(empty.kind, EntryKind::Tool);
    assert_eq!(empty.title, "tool call-a output");
    assert!(
        empty.lines.is_empty(),
        "a truncated preview with no text adds no line"
    );
    assert!(empty.truncated);

    assert!(state.apply_event(&tool_output(2, "call-b", "partial output", true)));
    let partial = last_entry(&state);
    assert_eq!(partial.lines, vec!["partial output".to_owned()]);
    assert!(
        partial.truncated,
        "a caller truncation flag is never cleared"
    );

    assert!(state.apply_event(&tool_output(3, "call-c", "complete output", false)));
    let complete = last_entry(&state);
    assert_eq!(complete.lines, vec!["complete output".to_owned()]);
    assert!(!complete.truncated, "fitting output is not flagged");

    // An oversized preview is cut to the entry bound even without a flag.
    assert!(state.apply_event(&tool_output(
        4,
        "call-d",
        &"z".repeat(MAX_ENTRY_BYTES + 1),
        false
    )));
    let oversized = last_entry(&state);
    assert!(oversized.truncated);
    assert_eq!(entry_bytes(oversized), MAX_ENTRY_BYTES);
    assert_eq!(oversized.lines.len(), 1);
    assert_eq!(
        oversized.lines[0].len(),
        max_line_bytes(oversized.title.as_str()),
        "the longest line that fits the entry bound is kept"
    );
    assert!(oversized.lines[0].bytes().all(|byte| byte == b'z'));
    assert!(
        state
            .transcript()
            .iter()
            .any(|line| line == TRUNCATION_MARK),
        "the truncation marker is retained alongside cut entries"
    );
    assert_accounted(&state);
}

#[test]
fn multi_byte_truncation_keeps_only_whole_characters() {
    let capacity = max_line_bytes(USER_TITLE);
    for (glyph, encoded) in [("é", 2usize), ("€", 3), ("😀", 4)] {
        let oversize = glyph.repeat(MAX_ENTRY_BYTES / encoded + 1);
        let mut state = AppState::new();
        state.record_submitted(&oversize);

        let entry = state.entry(0).expect("retained entry exists");
        assert!(entry.truncated, "{glyph} overflows the entry bound");
        let stored = entry.lines.first().expect("a safe prefix is retained");
        assert!(!stored.is_empty(), "{glyph}: a prefix is kept");
        assert!(stored.len() <= capacity, "{glyph}: prefix fits the bound");
        assert_eq!(
            stored.len() % encoded,
            0,
            "{glyph}: no partial character is retained"
        );
        assert_eq!(stored.chars().count(), stored.len() / encoded);
        assert!(
            stored.chars().all(|char| glyph.contains(char)),
            "{glyph}: only whole characters of the input survive"
        );
        assert!(
            oversize.starts_with(stored),
            "{glyph}: stored text is a byte prefix of the input"
        );
        assert_eq!(
            stored,
            &glyph.repeat(capacity / encoded),
            "{glyph}: the longest whole-character prefix that fits is kept"
        );
        assert_eq!(entry_bytes(entry), state.retained_bytes());
        assert_accounted(&state);
    }
}

#[test]
fn streamed_fragment_with_one_byte_left_drops_the_whole_character() {
    // Fill the assistant entry so exactly one byte of room is left.
    let filler = "a".repeat(MAX_ENTRY_BYTES - ASSISTANT_TITLE.len() - LINE_OVERHEAD - 1);
    let mut state = AppState::new();
    assert!(state.apply_event(&started(0)));
    assert!(state.apply_event(&assistant_fragment(1, &filler)));

    let entry = last_entry(&state);
    assert_eq!(entry.title, ASSISTANT_TITLE);
    assert_eq!(entry.lines, vec![filler.clone()]);
    assert!(!entry.truncated, "the first fragment fits the entry");
    assert_eq!(entry_bytes(entry), MAX_ENTRY_BYTES - 1);

    assert!(state.apply_event(&assistant_fragment(2, "€abc")));
    let cut = last_entry(&state);
    assert!(cut.truncated, "a fragment that cannot fit sets the flag");
    assert_eq!(
        cut.lines,
        vec![filler],
        "the whole character is dropped, never split or half-appended"
    );
    assert_eq!(entry_bytes(cut), MAX_ENTRY_BYTES - 1);
    assert!(
        !cut.lines[0].contains('€'),
        "no partial multi-byte character is retained"
    );
    let view = state.visible_lines(200, 100);
    assert!(
        view.lines
            .iter()
            .any(|line| line.trim_start() == TRUNCATION_MARK),
        "the truncation marker is rendered after a streamed cut"
    );
    assert_accounted(&state);
}

#[test]
fn truncated_multibyte_entry_renders_only_valid_utf8() {
    let width = 60;
    let mut state = AppState::new();
    state.record_submitted(&"日本語".repeat(MAX_ENTRY_BYTES));

    let entry = state.entry(0).expect("retained entry exists");
    assert!(entry.truncated);
    let stored = entry.lines.first().expect("a safe prefix is retained");
    assert!(
        stored.chars().all(|char| "日本語".contains(char)),
        "only whole characters are retained"
    );

    let height = state.total_height(width);
    let view = state.visible_lines(width, height);
    assert_eq!(view.hidden_above, 0);
    assert_eq!(
        view.lines.len(),
        height,
        "the wrapped height matches the rendered window"
    );
    assert_eq!(view.lines.first().map(String::as_str), Some(USER_TITLE));
    assert_eq!(
        view.lines.last().map(|line| line.trim_start()),
        Some(TRUNCATION_MARK)
    );
    assert!(
        view.lines.iter().all(|line| line.chars().count() <= width),
        "no rendered line exceeds the wrap width in characters"
    );
    assert!(
        view.lines.iter().all(|line| !line.contains('\u{FFFD}')),
        "no replacement character appears at a cut point"
    );
    let body: String = view.lines[1..view.lines.len() - 1]
        .iter()
        .map(|line| line.strip_prefix("  ").expect("body lines are indented"))
        .collect();
    assert_eq!(
        body,
        stored.as_str(),
        "re-wrapping the retained prefix reproduces it byte for byte"
    );
    assert_accounted(&state);
}
