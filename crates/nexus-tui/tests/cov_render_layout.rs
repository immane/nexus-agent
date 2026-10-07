#![forbid(unsafe_code)]

//! Public-API integration coverage for the TUI layout.
//!
//! These tests drive [`nexus_tui::render`] the way the frontend does — one
//! [`AppState`], one frame area, one buffer — and read the frame back through
//! `ratatui`'s `TestBackend`, so no PTY, clock, or randomness is involved and
//! every assertion is on exact rendered rows or on the geometry the renderer
//! publishes for the decision gate:
//!
//! - the compact, clipped, and expanded cards each occupy a bounded region
//!   whose inner rows match the geometry the renderer reported;
//! - the expanded detail names the keys it actually binds (`PgUp`/`PgDn`,
//!   `i`/`esc`), shows its own row range, and keeps deny reachable while rows
//!   are still hidden;
//! - the footer names `[i] inspect` exactly while an approval is pending and
//!   switches to close/page hints while the detail is open;
//! - page positions (viewport and expanded detail) survive repeated redraws
//!   and new output;
//! - wrapped heights agree with the rows drawn: the body region holds exactly
//!   the measured window across a sweep of frame widths, off the tail and with
//!   a folded entry;
//! - a long operation detail pages through every wrapped row, and only a
//!   contiguous walk unlocks allow-once — jumping to the last row does not;
//! - frames at the size limits stay honest: below the documented minimum one
//!   notice replaces the layout, and the smallest frame that also holds a card
//!   still bounds it, keeps the composer and footer, and keeps the gate locked.

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nexus_core::{
    ApprovalId, ApprovalNotice, AssistantText, CallId, EventPayload, RequestId, RunEvent, RunId,
    SessionId, TurnId,
};
use nexus_tui::{Action, AppState, Focus, map_key, render};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// An exact-arguments preview long enough to be its own wrapped row.
const ARGS_PREVIEW: &str = "path=src/main.rs module=deploy target=production-eu";

fn harness(width: u16, height: u16) -> (AppState, Terminal<TestBackend>) {
    let backend = TestBackend::new(width, height);
    let terminal = Terminal::new(backend).expect("test terminal builds");
    (AppState::new(), terminal)
}

fn draw(state: &mut AppState, terminal: &mut Terminal<TestBackend>, focus: Focus) {
    terminal
        .draw(|frame| render(state, frame.size(), frame.buffer_mut(), focus))
        .expect("test draw succeeds");
}

/// One row per terminal row, exactly as the frame was written.
fn frame_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer.get(x, y).symbol())
                .collect()
        })
        .collect()
}

/// The footer is always the last row of the frame.
fn footer_row(rows: &[String]) -> &str {
    rows.last().map_or("", String::as_str)
}

/// The composer always carries its title on its top border row, so the body
/// region is exactly the rows between the header and that border.
fn body_region(rows: &[String]) -> Vec<String> {
    let composer_top = rows
        .iter()
        .position(|row| row.contains("composer (fixed)"))
        .expect("the composer border row is always rendered");
    assert!(
        composer_top > 1,
        "the body sits between the header and the composer"
    );
    rows[1..composer_top].to_vec()
}

/// Inner rows of the bordered approval card, top and bottom borders removed,
/// with trailing padding stripped. Rows the card left blank are empty strings.
///
/// The card is drawn inside a `ratatui` `Block` with all borders, so every
/// row the card occupies — title row, detail rows, hint row, and the padding
/// rows below them — carries a `│` in the first and last column of the card
/// area. Those columns belong to the block, not to the card's text, so they
/// are stripped here and the rows compared below are exactly the content
/// columns the renderer materialized (at most `geometry.inner_width` of them).
fn card_inner(rows: &[String]) -> Vec<String> {
    let top = rows
        .iter()
        .position(|row| row.contains("approval detail ") || row.contains("approval required"))
        .expect("the approval card border row is rendered");
    let bottom = rows[top + 1..]
        .iter()
        .position(|row| row.starts_with('\u{2514}'))
        .map_or(rows.len(), |offset| top + 1 + offset);
    rows[top + 1..bottom]
        .iter()
        .map(|row| {
            let content = row.strip_prefix('\u{2502}').unwrap_or(row);
            content
                .strip_suffix('\u{2502}')
                .unwrap_or(content)
                .trim_end()
                .to_owned()
        })
        .collect()
}

/// The expanded view's final inner row is a fixed key hint, never detail.
fn is_hint(row: &str) -> bool {
    row.starts_with("[a] allow once") || row.starts_with("[d] deny")
}

/// Detail rows the card actually displayed, excluding hint and padding rows.
fn shown_detail(card: &[String]) -> Vec<String> {
    card.iter()
        .filter(|row| !is_hint(row.as_str()) && !row.is_empty())
        .cloned()
        .collect()
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::empty())
}

fn started(state: &mut AppState, seq: u64) {
    let event = RunEvent::new(
        SessionId::new("sess-1").expect("valid"),
        RunId::new("run-1").expect("valid"),
        seq,
        EventPayload::RunStarted {
            request: RequestId::new("req-1").expect("valid"),
        },
    );
    assert!(state.apply_event(&event), "the run adopts");
}

fn text_event(seq: u64, text: &str) -> RunEvent {
    RunEvent::new(
        SessionId::new("sess-1").expect("valid"),
        RunId::new("run-1").expect("valid"),
        seq,
        EventPayload::AssistantTextDelta(
            AssistantText::new(TurnId::new("t1-0").expect("valid"), "item-0", text)
                .expect("fragment builds"),
        ),
    )
}

fn approval_notice(summary: &str, scope: &str, args: Option<&str>) -> ApprovalNotice {
    let notice = ApprovalNotice::new(
        ApprovalId::new("a1-0").expect("valid"),
        CallId::new("c1-0").expect("valid"),
        summary,
        scope,
        Duration::from_secs(120),
    )
    .expect("notice builds");
    match args {
        Some(args) => notice.with_args_preview(args).expect("preview builds"),
        None => notice,
    }
}

fn approval_event(seq: u64, summary: &str, scope: &str, args: Option<&str>) -> RunEvent {
    RunEvent::new(
        SessionId::new("sess-1").expect("valid"),
        RunId::new("run-1").expect("valid"),
        seq,
        EventPayload::ApprovalRequired(approval_notice(summary, scope, args)),
    )
}

/// A summary whose wrapped rows are far more than any single card view can
/// show: the exact operation cannot be read without scrolling the detail.
fn long_summary() -> String {
    let mut text = String::from("run tool host_write");
    while text.chars().count() <= 900 {
        text.push_str(" deploy-to-production-segment");
    }
    text
}

/// The hard-wrapped rows of one logical line at `width` columns: the
/// independent model the renderer's reported row counts are compared against,
/// so measurement and drawing cannot silently drift apart.
fn chunks_at(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if line.is_empty() {
        return vec![String::new()];
    }
    line.chars()
        .collect::<Vec<char>>()
        .chunks(width)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

/// The full operation detail the approval card wraps, in display order, built
/// from the public card surface so the model follows the card data.
fn detail_fields(state: &AppState) -> Vec<String> {
    let card = state
        .pending_approval()
        .expect("a live approval card is pending");
    let mut fields = vec![
        format!("operation: {}", card.summary),
        format!("scope: {}", card.scope_summary),
    ];
    if let Some(args) = state.approval_args_preview() {
        fields.push(format!("args: {args}"));
    }
    let run = state
        .active_run()
        .map_or_else(|| "?".to_owned(), |run| run.as_str().to_owned());
    fields.push(format!("call:{} run:{run}", card.call.as_str()));
    fields.push(format!(
        "grant:{} runtime-bound; deadline {}s run-elapsed (monotonic, rechecked at dispatch)",
        card.approval.as_str(),
        card.expires_at_elapsed.as_secs(),
    ));
    fields
}

/// Every wrapped detail row, in order, at the reported inner card width.
fn detail_rows(state: &AppState, inner_width: usize) -> Vec<String> {
    detail_fields(state)
        .iter()
        .flat_map(|field| chunks_at(field, inner_width))
        .collect()
}

/// True when a drawn row carries exactly `model`'s text: the renderer wraps
/// by character count, but a terminal gives a double-width glyph two cells and
/// `ratatui` writes its symbol in the first cell and blanks the second, so the
/// drawn row is the model row plus those filler cells. Anything else — a
/// dropped, reordered, or truncated glyph — fails the match.
fn drawn_as(drawn: &str, model: &str) -> bool {
    let mut cells = drawn.chars();
    for want in model.chars() {
        match cells.next() {
            Some(got) if got == want => {}
            // Not the glyph the model expects: this is the filler cell behind
            // the previous double-width glyph.
            Some(_) => {
                let mut filler = cells.clone();
                if filler.next() == Some(want) {
                    cells = filler;
                } else {
                    return false;
                }
            }
            None => return false,
        }
    }
    cells.next().is_none()
}

/// The body region must hold exactly the window the renderer measured: an
/// optional bound notice, then one screen row per presented line, and nothing
/// else. This is the agreement between wrapped heights and drawn rows.
///
/// When history exceeds the window the renderer shows a one-cell scrollbar
/// and wraps the text one cell narrower, so the model is built at that same
/// text width and the bar column is excluded from every comparison.
fn assert_body_matches_measurement(state: &AppState, rows: &[String], width: u16) {
    let width = (width as usize).max(1);
    let body = body_region(rows);
    let content_height = state.viewport_height();
    assert!(
        content_height <= body.len(),
        "measured viewport {content_height} fits the {} body rows",
        body.len()
    );
    let bar = width > 1 && state.total_height(width - 1) > body.len();
    let text_width = if bar { width - 1 } else { width };
    let view = state.visible_lines(text_width, content_height);
    let text_of = |row: &String| {
        if bar {
            row.chars().take(text_width).collect::<String>()
        } else {
            row.clone()
        }
    };
    // Blank rows are entry separators and appear in the model itself; the
    // row-by-row equality below pins each one exactly, so padding is still
    // caught by the trailing-blank check.
    let notice = usize::from(view.hidden_above > 0 || view.retention_truncated);
    assert!(
        notice + view.lines.len() <= body.len(),
        "the window fits the body region"
    );
    // Non-blank counts agree (blank separator rows are validated by the
    // row-by-row equality below, not by omission).
    let drawn_nonblank = body[notice..notice + view.lines.len()]
        .iter()
        .map(text_of)
        .filter(|row| !row.trim().is_empty())
        .count();
    let model_nonblank = view
        .lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .count();
    assert_eq!(
        drawn_nonblank, model_nonblank,
        "every measured line is drawn exactly once"
    );
    assert!(
        body[notice..notice + view.lines.len()]
            .iter()
            .map(text_of)
            .zip(&view.lines)
            .all(|(row, line)| row.trim_end() == line.trim_end()),
        "drawn rows match the presented lines"
    );
    assert!(
        body[notice + view.lines.len()..]
            .iter()
            .map(text_of)
            .all(|row| row.trim().is_empty()),
        "no rows are drawn beyond the measured window"
    );
    assert!(
        view.lines.len() <= content_height,
        "the window never exceeds the body"
    );
    assert!(
        view.lines.iter().all(|line| line.chars().count() <= width),
        "no presented line exceeds the frame width"
    );
    assert!(
        body.iter().all(|row| row.chars().count() <= width),
        "no rendered body row exceeds the frame width"
    );
    let expected = content_height.min(state.total_height(width).saturating_sub(state.scrollback()));
    assert_eq!(
        view.lines.len(),
        expected,
        "window rows match the measured total height"
    );
}

#[test]
fn expanded_approval_detail_is_readable_and_names_the_page_keys() {
    let (mut state, mut terminal) = harness(60, 18);
    started(&mut state, 0);
    assert!(state.apply_event(&approval_event(
        1,
        &long_summary(),
        "project scope spanning the whole workspace",
        Some(ARGS_PREVIEW),
    )));
    state.inspect_approval();
    draw(&mut state, &mut terminal, Focus::ApprovalCard);

    assert!(state.approval_detail_open(), "the detail view is shown");
    assert!(
        !state.approval_detail_truncated(),
        "no field was cut, so inspection is the only gate"
    );
    let geometry = state
        .approval_geometry()
        .expect("the renderer reported geometry");
    let view_rows = state.approval_detail_view_rows();
    let expected = detail_rows(&state, geometry.inner_width);
    assert_eq!(
        geometry.detail_rows,
        expected.len(),
        "measured detail rows match the wrapped model"
    );
    assert_eq!(
        state.approval_detail_total_rows(),
        expected.len(),
        "the detail view reports the same row count"
    );
    assert_eq!(
        geometry.inner_rows, view_rows,
        "geometry and the view agree on the visible rows"
    );
    assert!(
        expected.len() > view_rows,
        "the detail needs more than one page: {} rows over {view_rows}",
        expected.len()
    );
    assert!(geometry.clipped, "the detail does not fit one view");

    let rows = frame_rows(&terminal);
    let title = rows
        .iter()
        .find(|row| row.contains("approval detail "))
        .expect("the expanded card is titled");
    assert_eq!(
        shown_detail(&card_inner(&rows)),
        expected[..view_rows],
        "the first page shows exactly the leading detail rows"
    );
    assert!(
        title.contains(&format!("approval detail 1-{view_rows}/{}", expected.len())),
        "the title names the visible row range: {title}"
    );
    assert!(
        title.contains("PgUp/PgDn"),
        "the title names the keys: {title}"
    );
    assert!(
        title.contains("i/esc close"),
        "the title names the close keys: {title}"
    );
    // The keys the card names are the keys the keymap actually binds.
    assert_eq!(
        map_key(Focus::ApprovalCard, key(KeyCode::PageDown)),
        Some(Action::PageDown),
        "PgDn pages the expanded detail"
    );
    assert_eq!(
        map_key(Focus::ApprovalCard, key(KeyCode::PageUp)),
        Some(Action::PageUp),
        "PgUp pages the expanded detail back"
    );
    assert_eq!(
        map_key(Focus::ApprovalCard, key(KeyCode::Char('i'))),
        Some(Action::InspectApproval),
        "i opens and closes the detail"
    );

    let card = card_inner(&rows);
    assert_eq!(
        card.len(),
        view_rows + 1,
        "the fixed hint row closes the card below the detail rows"
    );
    assert_eq!(
        card[view_rows], "[d] deny  i/esc close \u{b7} allow locked until detail end",
        "deny stays reachable and allow is locked before the last row"
    );
    assert!(
        shown_detail(&card)
            .iter()
            .all(|row| !row.starts_with("[a] allow once")),
        "no allow affordance while detail rows are hidden"
    );
    assert!(
        !state.approval_decision_allowed(),
        "the gate stays locked until the whole detail was displayed"
    );
}

#[test]
fn footer_names_the_inspect_key_while_an_approval_is_pending() {
    let (mut state, mut terminal) = harness(160, 24);
    started(&mut state, 0);
    draw(&mut state, &mut terminal, Focus::ApprovalCard);
    let footer = footer_row(&frame_rows(&terminal)).to_owned();
    assert!(
        !footer.contains("[i] inspect") && !footer.contains("[i] close"),
        "no card, no inspect hint: {footer}"
    );
    assert!(
        !footer.contains("pgup/pgdn"),
        "no card, no page hint: {footer}"
    );
    assert_eq!(
        footer.trim_start(),
        format!("{} running", concat!("v", env!("CARGO_PKG_VERSION"))),
        "{footer}"
    );

    assert!(state.apply_event(&approval_event(
        1,
        "run tool host_write",
        "project scope",
        None,
    )));
    draw(&mut state, &mut terminal, Focus::ApprovalCard);
    let footer = footer_row(&frame_rows(&terminal)).to_owned();
    assert_eq!(
        footer.trim_start(),
        format!(
            "{} awaiting approval",
            concat!("v", env!("CARGO_PKG_VERSION"))
        ),
        "{footer}"
    );
    assert!(
        state.approval_decision_allowed(),
        "a fully visible card may still be decided"
    );

    state.inspect_approval();
    draw(&mut state, &mut terminal, Focus::ApprovalCard);
    let footer = footer_row(&frame_rows(&terminal)).to_owned();
    assert_eq!(
        footer.trim_start(),
        format!(
            "{} awaiting approval",
            concat!("v", env!("CARGO_PKG_VERSION"))
        ),
        "{footer}"
    );

    state.close_approval_detail();
    draw(&mut state, &mut terminal, Focus::ApprovalCard);
    assert_eq!(
        footer_row(&frame_rows(&terminal)).trim_start(),
        format!(
            "{} awaiting approval",
            concat!("v", env!("CARGO_PKG_VERSION"))
        )
    );

    state.resolve_approval();
    draw(&mut state, &mut terminal, Focus::ApprovalCard);
    assert!(
        !frame_rows(&terminal)
            .iter()
            .any(|row| row.contains("approval required")),
        "the card is gone from the frame"
    );
}

#[test]
fn viewport_page_position_survives_repeated_redraws() {
    let (mut state, mut terminal) = harness(48, 14);
    started(&mut state, 0);
    for index in 0..40 {
        state.record_submitted(&format!("entry {index:02}"));
    }
    draw(&mut state, &mut terminal, Focus::Viewport);
    assert!(
        body_region(&frame_rows(&terminal))
            .join("\n")
            .contains("entry 39"),
        "the live tail is visible first"
    );

    assert_eq!(
        map_key(Focus::Viewport, key(KeyCode::PageUp)),
        Some(Action::PageUp),
        "PgUp pages the viewport"
    );
    let page = state.viewport_height();
    assert!(page > 0, "the body has rows to page over");
    state.scroll_up(page);
    let offset = state.scrollback();
    assert_eq!(offset, page, "one PageUp moves exactly one page");

    draw(&mut state, &mut terminal, Focus::Viewport);
    let paged = frame_rows(&terminal);
    assert_body_matches_measurement(&state, &paged, 48);
    let body = body_region(&paged);
    assert!(
        body[0].contains("presentation-truncated"),
        "the bound notice occupies the first body row: {body:?}"
    );
    let visible = visible_entries(&visible_body_text(&body));
    assert!(
        !visible.is_empty() && visible.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "one contiguous page of older entries is visible: {visible:?}"
    );
    assert!(
        visible.iter().all(|index| *index < 39),
        "no live-tail entry is left on screen: {visible:?}"
    );
    for _ in 0..3 {
        draw(&mut state, &mut terminal, Focus::Viewport);
        assert_eq!(frame_rows(&terminal), paged, "redraws do not drift");
        assert_eq!(
            state.scrollback(),
            offset,
            "the page position survives the redraw"
        );
    }

    // New output while scrolled must not yank the reader back to the tail.
    assert!(state.apply_event(&text_event(1, "later output")));
    draw(&mut state, &mut terminal, Focus::Viewport);
    assert_eq!(
        state.scrollback(),
        offset,
        "new output does not reset the reader's page position"
    );
    assert_body_matches_measurement(&state, &frame_rows(&terminal), 48);
    let after_text = visible_body_text(&body_region(&frame_rows(&terminal)));
    assert!(
        !after_text.contains("later output"),
        "the new fragment arrived below the fold, not in the reader's page"
    );
    assert!(
        !after_text.contains("entry 39"),
        "the tail is still off screen after new output"
    );
    assert!(
        visible_entries(&after_text).len() > 1,
        "older entries are still shown after new output"
    );
}

/// Submitted entry numbers whose text is visible in a body region.
fn visible_entries(body: &str) -> Vec<usize> {
    (0..40)
        .filter(|index| body.contains(&format!("entry {index:02}")))
        .collect()
}

/// The body rows the reader actually sees, with the presentation bound notice
/// excluded so it is never mistaken for presented content.
fn visible_body_text(body: &[String]) -> String {
    body.iter()
        .filter(|row| !row.contains("presentation-truncated"))
        .cloned()
        .collect::<Vec<String>>()
        .join("\n")
}

#[test]
fn detail_page_position_survives_repeated_redraws() {
    let (mut state, mut terminal) = harness(60, 18);
    started(&mut state, 0);
    assert!(state.apply_event(&approval_event(
        1,
        &long_summary(),
        "project scope spanning the whole workspace",
        Some(ARGS_PREVIEW),
    )));
    state.inspect_approval();
    draw(&mut state, &mut terminal, Focus::ApprovalCard);

    let geometry = state.approval_geometry().expect("geometry recorded");
    let expected = detail_rows(&state, geometry.inner_width);
    let page = state.approval_detail_view_rows();
    assert!(page > 1, "the view spans more than one row");

    state.approval_detail_page(1);
    draw(&mut state, &mut terminal, Focus::ApprovalCard);
    let offset = state.approval_detail_scroll_position();
    assert_eq!(offset, page, "one page down from the first row");
    let paged = frame_rows(&terminal);
    assert_eq!(
        shown_detail(&card_inner(&paged))[0],
        expected[offset],
        "the top row is the row at the new offset"
    );
    for _ in 0..3 {
        draw(&mut state, &mut terminal, Focus::ApprovalCard);
        assert_eq!(frame_rows(&terminal), paged, "redraws do not drift");
        assert_eq!(
            state.approval_detail_scroll_position(),
            offset,
            "the detail page position survives the redraw"
        );
    }

    // New output does not move the detail either.
    assert!(state.apply_event(&text_event(2, "later output")));
    draw(&mut state, &mut terminal, Focus::ApprovalCard);
    assert_eq!(
        state.approval_detail_scroll_position(),
        offset,
        "new output does not reset the detail position"
    );

    state.approval_detail_page(-1);
    draw(&mut state, &mut terminal, Focus::ApprovalCard);
    assert_eq!(
        state.approval_detail_scroll_position(),
        0,
        "one page up returns to the first row"
    );
    let card = card_inner(&frame_rows(&terminal));
    assert!(
        card[0].starts_with("operation: run tool host_write"),
        "the first row is the operation head: {}",
        card[0]
    );
    assert_eq!(
        card,
        card_inner(&frame_rows(&terminal)),
        "the detail frame is stable after paging back"
    );
}

#[test]
fn wrapped_heights_match_the_rendered_rows_across_frame_widths() {
    for width in [20u16, 24, 33, 48, 80] {
        let (mut state, mut terminal) = harness(width, 16);
        started(&mut state, 0);
        state.record_submitted("first note");
        state.record_submitted("alpha bravo charlie delta");
        state.record_submitted("line one\nline two\nline three");
        // Beyond the per-entry byte bound, so the entry carries its truncation
        // marker and the window reports hidden rows above it.
        state.record_submitted(&"z ".repeat(6000));
        // `started` records the run-started system entry, so the oversized
        // submission is the last retained entry, not the fourth one.
        let oversized = state.entry_count() - 1;
        draw(&mut state, &mut terminal, Focus::Viewport);
        assert_eq!(state.dropped_entries, 0, "width {width}: retention held");
        assert!(
            state.entry(oversized).expect("the last entry").truncated,
            "width {width}: the oversized entry is marked truncated"
        );
        assert_body_matches_measurement(&state, &frame_rows(&terminal), width);

        // Off the live tail: the same agreement must hold.
        let page = state.viewport_height();
        state.scroll_up(page);
        assert!(
            state.scrollback() > 0,
            "width {width}: scrolled off the tail"
        );
        draw(&mut state, &mut terminal, Focus::Viewport);
        assert_body_matches_measurement(&state, &frame_rows(&terminal), width);

        // Back to the live tail, then fold the oversized tail entry: the collapse
        // is observable on screen because its marker is the last row.
        let back = state.scrollback();
        state.scroll_down(back);
        draw(&mut state, &mut terminal, Focus::Viewport);
        assert_eq!(state.scrollback(), 0, "width {width}: back at the tail");
        let open = state.total_height(width as usize);
        assert!(state.toggle_fold(oversized), "width {width}: fold toggles");
        draw(&mut state, &mut terminal, Focus::Viewport);
        assert!(
            state.total_height(width as usize) < open,
            "width {width}: folding removes rows"
        );
        assert_body_matches_measurement(&state, &frame_rows(&terminal), width);
        assert!(
            body_region(&frame_rows(&terminal))
                .join("\n")
                .contains("folded"),
            "width {width}: the fold marker is drawn"
        );
        assert!(
            state.toggle_fold(oversized),
            "width {width}: unfold toggles"
        );
        state.unfold_all();
        draw(&mut state, &mut terminal, Focus::Viewport);
        assert_eq!(
            state.total_height(width as usize),
            open,
            "width {width}: unfolding restores the measured height"
        );
        assert_body_matches_measurement(&state, &frame_rows(&terminal), width);
    }
}

#[test]
fn long_operation_detail_pages_through_every_row_before_a_decision() {
    let (mut state, mut terminal) = harness(60, 18);
    started(&mut state, 0);
    assert!(state.apply_event(&approval_event(
        1,
        &long_summary(),
        "project scope spanning the whole workspace",
        Some(ARGS_PREVIEW),
    )));
    state.inspect_approval();
    draw(&mut state, &mut terminal, Focus::ApprovalCard);

    let geometry = state.approval_geometry().expect("geometry recorded");
    let view_rows = state.approval_detail_view_rows();
    let expected = detail_rows(&state, geometry.inner_width);
    assert_eq!(
        state.approval_detail_total_rows(),
        expected.len(),
        "the reported row count matches the wrapped model"
    );
    assert!(
        expected.len() > view_rows,
        "the detail needs more than one page: {} rows over {view_rows}",
        expected.len()
    );
    let max_offset = expected.len() - view_rows;

    // Jumping to the last row shows the end without the hidden rows, which is
    // not an inspection: the gate must stay locked.
    state.approval_detail_scroll(max_offset as isize);
    draw(&mut state, &mut terminal, Focus::ApprovalCard);
    assert_eq!(
        state.approval_detail_scroll_position(),
        max_offset,
        "the scroll clamps to the last reachable offset"
    );
    let jumped = frame_rows(&terminal);
    assert!(
        jumped
            .iter()
            .find(|row| row.contains("approval detail "))
            .is_some_and(|row| row.contains(&format!(
                "approval detail {}-{}/{}",
                max_offset + 1,
                expected.len(),
                expected.len()
            ))),
        "the title names the last page"
    );
    assert_eq!(
        shown_detail(&card_inner(&jumped)),
        expected[max_offset..],
        "the last page shows the tail of the detail"
    );
    assert!(
        !state.approval_detail_seen_all(),
        "a jump over hidden rows is not a complete inspection"
    );
    assert!(
        !state.approval_decision_allowed(),
        "allow-once stays locked after a jump"
    );

    // Walk the whole detail back to its first row, one page row at a time.
    state.approval_detail_scroll(-(max_offset as isize));
    draw(&mut state, &mut terminal, Focus::ApprovalCard);
    assert_eq!(state.approval_detail_scroll_position(), 0);

    // Pages overlap: the walk advances one row at a time, so this records the row
    // shown at each row index (the last page to cover it writes the same value
    // the per-offset assertions above already proved) instead of concatenating
    // the pages, which would repeat the shared rows.
    let mut displayed: Vec<String> = vec![String::new(); expected.len()];
    for offset in 0..=max_offset {
        assert_eq!(
            state.approval_detail_scroll_position(),
            offset,
            "offset {offset} is reached in contiguous steps"
        );
        draw(&mut state, &mut terminal, Focus::ApprovalCard);
        let rows = frame_rows(&terminal);
        let end = (offset + view_rows).min(expected.len());
        let card = card_inner(&rows);
        assert_eq!(
            shown_detail(&card),
            expected[offset..end],
            "offset {offset} shows exactly its detail rows"
        );
        assert_eq!(
            card.len(),
            view_rows + 1,
            "offset {offset} keeps the fixed hint row"
        );
        let hint = if end >= expected.len() {
            "[a] allow once  [d] deny  i/esc close (all detail seen)"
        } else {
            "[d] deny  i/esc close \u{b7} allow locked until detail end"
        };
        assert_eq!(card[end - offset], hint, "offset {offset} hint");
        assert!(
            rows.iter()
                .find(|row| row.contains("approval detail "))
                .is_some_and(|row| row.contains(&format!(
                    "approval detail {}-{end}/{}",
                    offset + 1,
                    expected.len()
                ))),
            "offset {offset} names its row range"
        );
        displayed[offset..end].clone_from_slice(&expected[offset..end]);
        if end >= expected.len() {
            assert!(
                state.approval_detail_seen_all(),
                "reaching the last row contiguously completes the inspection"
            );
            assert!(
                state.approval_decision_allowed(),
                "deliberate full inspection unlocks the live identity"
            );
        } else {
            assert!(
                !state.approval_decision_allowed(),
                "offset {offset} has not shown the detail end yet"
            );
        }
        if offset < max_offset {
            state.approval_detail_scroll(1);
        }
    }
    assert_eq!(
        displayed, expected,
        "every wrapped detail row was displayed in order"
    );
    assert!(
        state.approval_geometry().expect("geometry").clipped,
        "the compact card still does not fit, so the unlock came from the walk"
    );
}

#[test]
fn frames_at_the_size_limits_render_one_notice_or_a_bounded_card() {
    for (width, height) in [(19u16, 24u16), (80, 7), (20, 7), (19, 8)] {
        let (mut state, mut terminal) = harness(width, height);
        started(&mut state, 0);
        assert!(state.apply_event(&approval_event(1, &long_summary(), "project scope", None,)));
        draw(&mut state, &mut terminal, Focus::ApprovalCard);
        let frame = frame_rows(&terminal).join("\n");
        assert!(
            frame.contains("terminal too small"),
            "{width}x{height}: an honest notice replaces the layout"
        );
        assert!(
            !frame.contains("composer (fixed)"),
            "{width}x{height}: no clipped composer"
        );
        assert!(
            !frame.contains("[a] allow once") && !frame.contains("[d] deny"),
            "{width}x{height}: no decision affordance to press"
        );
        assert!(
            state.approval_geometry().is_none(),
            "{width}x{height}: an unrendered card reports no geometry"
        );
        assert!(
            !state.approval_decision_allowed(),
            "{width}x{height}: an undisplayed card can never be decided"
        );
    }

    // The smallest frame that also holds the smallest card (two borders plus
    // one detail row) still bounds it, and the fixed composer and footer stay.
    let (mut state, mut terminal) = harness(20, 9);
    started(&mut state, 0);
    assert!(state.apply_event(&approval_event(1, &long_summary(), "project scope", None,)));
    state.inspect_approval();
    draw(&mut state, &mut terminal, Focus::ApprovalCard);
    let rows = frame_rows(&terminal);
    let frame = rows.join("\n");
    assert!(
        rows.iter().all(|row| row.chars().count() <= 20),
        "no row exceeds the frame: {rows:?}"
    );
    assert!(
        frame.contains("approval detail"),
        "the card is still titled"
    );
    assert!(
        frame.contains("composer (fixed)"),
        "the fixed composer still fits beside the card"
    );
    assert!(
        !footer_row(&rows).trim().is_empty(),
        "the footer still renders beside the card: {rows:?}"
    );
    assert!(
        state.approval_detail_view_rows() >= 1,
        "the detail view keeps at least one row"
    );
    assert!(
        state.approval_detail_total_rows() > state.approval_detail_view_rows(),
        "the detail still does not fit"
    );
    assert!(
        state.approval_geometry().expect("geometry").clipped,
        "a tiny card stays clipped"
    );
    assert!(
        !state.approval_decision_allowed(),
        "a tiny card never unlocks the gate"
    );
    let first = state.approval_detail_scroll_position();
    state.approval_detail_scroll(1);
    draw(&mut state, &mut terminal, Focus::ApprovalCard);
    assert!(
        state.approval_detail_scroll_position() > first,
        "the tiny detail still scrolls row by row"
    );
    assert!(
        !state.approval_decision_allowed(),
        "scrolling one row of a tiny card is not an inspection"
    );
}

#[test]
fn detail_rows_are_never_wider_than_the_card() {
    // Wrapping is character-counted, so a multi-byte field must still produce
    // rows that fit the inner card width on screen.
    let (mut state, mut terminal) = harness(44, 20);
    started(&mut state, 0);
    let scope = "répertoire \u{1d173}\u{200b} étendu \u{65e5}\u{672c}\u{8a9e}";
    assert!(state.apply_event(&approval_event(
        1,
        "run tool host_write",
        scope,
        Some(ARGS_PREVIEW),
    )));
    state.inspect_approval();
    draw(&mut state, &mut terminal, Focus::ApprovalCard);

    let geometry = state.approval_geometry().expect("geometry recorded");
    let expected = detail_rows(&state, geometry.inner_width);
    let view_rows = state.approval_detail_view_rows();
    assert_eq!(
        state.approval_detail_total_rows(),
        expected.len(),
        "invisible formatting is escaped before the card wraps it"
    );
    let rows = frame_rows(&terminal);
    let card = card_inner(&rows);
    // At this size the whole escaped detail fits the expanded view, so the
    // visible window is the full measured model rather than a page of it.
    assert!(
        expected.len() <= view_rows,
        "the escaped detail fits the view: {} rows over {view_rows}",
        expected.len()
    );
    let drawn = shown_detail(&card);
    assert_eq!(
        drawn.len(),
        expected.len(),
        "every measured detail row is drawn once: {card:?}"
    );
    assert!(
        drawn
            .iter()
            .zip(&expected)
            .all(|(row, model)| drawn_as(row, model)),
        "escaped text wraps exactly as measured: {drawn:?}"
    );
    assert!(
        card.iter()
            .all(|row| row.chars().count() <= geometry.inner_width),
        "no rendered card row is wider than the inner width: {card:?}"
    );
    assert!(
        rows.iter().all(|row| row.chars().count() <= 44),
        "no rendered row exceeds the frame width"
    );
    assert!(
        !shown_detail(&card)
            .iter()
            .any(|row| row.contains('\u{200b}')),
        "an invisible separator is escaped, never rendered raw"
    );
}
