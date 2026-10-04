//! Immediate-mode renderer: header, bounded viewport, approval card,
//! fixed composer, footer.
//!
//! Rendering is event-driven through [`crate::RefreshGate`] (the caller
//! redraws only when the gate is ready), and only the visible window from
//! [`AppState::visible_lines`] is laid out — history outside the window is
//! counted, never reprocessed.
//!
//! The approval card wraps its full detail to the inner width and grows up
//! to the space the layout can spare. When even that cannot show every line,
//! the card clips explicitly, hides the decision affordances, and records
//! [`crate::state::ApprovalGeometry`]. The `[i]` key opens a bounded
//! scrollable expanded view of the full detail; the renderer reports which
//! rows it actually displayed, and [`AppState::approval_decision_allowed`]
//! stays false until the whole detail was visible (or scrolled through from
//! the first row). The footer and card titles name the key.

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::keys::Focus;
use crate::state::{AppState, ApprovalGeometry, PendingApprovalCard, wrapped_height};

/// Minimum usable frame; smaller frames get a one-line notice instead of a
/// clipped layout.
const MIN_WIDTH: u16 = 20;
/// Reserved lines: header (1) + footer (1) + composer chrome (2).
const CHROME_LINES: u16 = 4;
/// Composer body lines, clamped so history keeps the frame.
const MAX_COMPOSER_BODY: u16 = 5;
/// Smallest approval card: both borders plus one detail row.
const APPROVAL_MIN_ROWS: u16 = 3;
/// Shown in place of the decision row when the card detail does not fit.
const APPROVAL_LOCK: &str = "[!] detail clipped: press [i] to inspect before deciding";

/// Frame regions plus the viewport geometry handed back to presentation
/// state for selection math.
struct Regions {
    header: Rect,
    body: Rect,
    approval: Option<Rect>,
    composer: Rect,
    footer: Rect,
}

/// One frame's approval card: bounded, already wrapped detail lines plus the
/// geometry the decision gate needs. `expanded` selects the scrollable full
/// detail view over the compact summary card.
struct ApprovalLayout {
    lines: Vec<String>,
    rows: u16,
    inner_width: usize,
    detail_rows: usize,
    view_rows: usize,
    scroll: usize,
    clipped: bool,
    expanded: bool,
}

impl ApprovalLayout {
    fn geometry(&self) -> ApprovalGeometry {
        ApprovalGeometry {
            inner_width: self.inner_width,
            inner_rows: self.view_rows,
            detail_rows: self.detail_rows,
            clipped: self.clipped,
        }
    }
}

fn split(area: Rect, composer_body: u16, approval_rows: u16) -> Regions {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(approval_rows),
            Constraint::Length(composer_body + 2),
            Constraint::Length(1),
        ])
        .split(area);
    Regions {
        header: chunks[0],
        body: chunks[1],
        approval: (approval_rows > 0).then_some(chunks[2]),
        composer: chunks[3],
        footer: chunks[4],
    }
}

/// Renders one full frame: fixed composer, bounded viewport, approval card
/// on demand, compact header/footer. Pure presentation: reads [`AppState`],
/// writes cells, executes nothing.
pub fn render(state: &mut AppState, area: Rect, buf: &mut Buffer, focus: Focus) {
    if area.width < MIN_WIDTH || area.height < CHROME_LINES + 4 {
        let note = Paragraph::new("terminal too small for nexus-tui (M0 test build)");
        note.render(area, buf);
        return;
    }
    let composer_body = (state.composer().lines().count().max(1) as u16).min(MAX_COMPOSER_BODY);
    let approval = match state.pending_approval().cloned() {
        Some(card) => {
            let run = state.active_run().map(|run| run.as_str().to_owned());
            let args = state.approval_args_preview().map(str::to_owned);
            Some(approval_layout(
                &card,
                run.as_deref(),
                args.as_deref(),
                area,
                composer_body,
                state.approval_detail_open(),
                state.approval_detail_scroll_position(),
            ))
        }
        None => None,
    };
    if let Some(layout) = &approval {
        // The renderer owns measurement: key handling reads this back before
        // it may issue an approve command. The expanded view additionally
        // reports which rows it actually displayed, so the gate can require
        // deliberate, contiguous inspection of the full detail.
        state.set_approval_geometry(layout.geometry());
        if layout.expanded {
            state.record_approval_detail_view(layout.scroll, layout.view_rows, layout.detail_rows);
        }
    }
    let approval_rows = approval.as_ref().map_or(0, |layout| layout.rows);
    let regions = split(area, composer_body, approval_rows);
    render_header(state, regions.header, buf, focus);
    render_body(state, regions.body, buf);
    if let (Some(layout), Some(card_area)) = (approval.as_ref(), regions.approval) {
        render_approval(layout, card_area, buf);
    }
    render_composer(state, regions.composer, buf, focus);
    render_footer(state, regions.footer, buf, focus);
}

fn render_header(state: &AppState, area: Rect, buf: &mut Buffer, focus: Focus) {
    let run = state
        .active_run()
        .map(|run| run.as_str())
        .unwrap_or("no run");
    let header = Paragraph::new(Line::from(vec![
        Span::styled(" nexus-tui ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("M0-TEST "),
        Span::styled(format!("run:{run} "), Style::default().fg(Color::Cyan)),
        Span::raw(state.status()),
        Span::raw(format!(" focus:{focus:?}").to_lowercase()),
    ]));
    header.render(area, buf);
}

fn render_body(state: &mut AppState, area: Rect, buf: &mut Buffer) {
    let (width, height) = (area.width as usize, area.height as usize);
    // Reserve one row for the indicator before materializing the window, so
    // the live tail is never the line clipped away. Heights are cached, so
    // this probe neither re-measures nor re-renders history.
    let banner = state.truncation_indicator(width, height);
    let content_height = if banner {
        height.saturating_sub(1).max(1)
    } else {
        height
    };
    state.set_viewport(width, content_height);
    let view = state.visible_lines(width, content_height);
    let mut lines: Vec<Line> = Vec::with_capacity(content_height + 1);
    if view.hidden_above > 0 || view.retention_truncated {
        lines.push(Line::from(Span::styled(
            // Short enough to survive narrow frames: the bound notice must
            // itself never be clipped away.
            format!("… {}↑ presentation-truncated", view.hidden_above),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::DIM),
        )));
    }
    for line in view.lines {
        lines.push(Line::from(line));
    }
    Paragraph::new(lines).render(area, buf);
}

/// Builds the bounded, wrapped approval card. The compact card shows all
/// fields when they fit, otherwise the leading detail plus an explicit lock
/// row and no decision affordance. The expanded view shows the full
/// (untruncated) operation detail in a bounded scrollable panel with an
/// explicit scroll indicator and a hint row that keeps deny reachable.
fn approval_layout(
    card: &PendingApprovalCard,
    run: Option<&str>,
    args: Option<&str>,
    area: Rect,
    composer_body: u16,
    detail_open: bool,
    detail_scroll: usize,
) -> ApprovalLayout {
    let inner_width = area.width.saturating_sub(2).max(1) as usize;
    // Reserve header, footer, composer chrome, and one body row first; the
    // card may grow into whatever remains, never beyond it.
    let available = area.height.saturating_sub(1 + 1 + (composer_body + 2) + 1);
    let rows_cap = available.max(APPROVAL_MIN_ROWS);
    let detail_fields = approval_detail_fields(card, run, args);
    let detail_rows: usize = detail_fields
        .iter()
        .map(|field| wrapped_height(field, inner_width))
        .sum();
    if detail_open {
        return expanded_approval_layout(
            &detail_fields,
            detail_rows,
            inner_width,
            rows_cap,
            detail_scroll,
        );
    }
    let inner_cap = rows_cap.saturating_sub(2).max(1) as usize;
    let mut fields = detail_fields;
    fields.push("[a] allow once   [d] deny   [i] inspect   [esc] park (no decision)".to_owned());
    fields.push("decisions go through the runtime only; previews never authorize".to_owned());
    let clipped = detail_rows > inner_cap;
    let mut lines = Vec::new();
    if clipped {
        // The lock row replaces the decision affordance; it must be visible.
        let warning_rows = wrapped_height(APPROVAL_LOCK, inner_width).min(inner_cap);
        let budget = inner_cap.saturating_sub(warning_rows);
        push_fields(&fields, inner_width, budget, &mut lines);
        push_wrapped(APPROVAL_LOCK, inner_width, warning_rows, &mut lines);
    } else {
        push_fields(&fields, inner_width, inner_cap, &mut lines);
    }
    let rows = (lines.len() as u16 + 2).max(APPROVAL_MIN_ROWS);
    ApprovalLayout {
        lines,
        rows,
        inner_width,
        detail_rows,
        view_rows: inner_cap,
        scroll: 0,
        clipped,
        expanded: false,
    }
}

/// The expanded view uses every row the frame can spare and materializes
/// only the visible slice of the full detail. The last inner row is the hint
/// row, so deny stays reachable even mid-scroll.
fn expanded_approval_layout(
    detail_fields: &[String],
    detail_rows: usize,
    inner_width: usize,
    rows_cap: u16,
    detail_scroll: usize,
) -> ApprovalLayout {
    let rows = rows_cap;
    let inner_rows = rows.saturating_sub(2) as usize;
    let hint_rows = usize::from(inner_rows >= 2);
    let view_rows = inner_rows.saturating_sub(hint_rows).max(1);
    let max_scroll = detail_rows.saturating_sub(view_rows);
    let scroll = detail_scroll.min(max_scroll);
    let mut lines = Vec::new();
    push_detail_range(detail_fields, inner_width, scroll, view_rows, &mut lines);
    if hint_rows == 1 {
        lines.push(
            if detail_rows > 0 && scroll.saturating_add(view_rows) >= detail_rows {
                "[a] allow once  [d] deny  i/esc close (all detail seen)".to_owned()
            } else {
                "[d] deny  i/esc close · allow locked until detail end".to_owned()
            },
        );
    }
    ApprovalLayout {
        lines,
        rows,
        inner_width,
        detail_rows,
        view_rows,
        scroll,
        clipped: detail_rows > view_rows,
        expanded: true,
    }
}

/// Full operation detail shared by the compact and expanded views. The
/// deadline is a monotonic run-elapsed reading from the runtime notice, not
/// a wall-clock or remaining time.
fn approval_detail_fields(
    card: &PendingApprovalCard,
    run: Option<&str>,
    args: Option<&str>,
) -> Vec<String> {
    let mut fields = vec![
        format!("operation: {}", card.summary),
        format!("scope: {}", card.scope_summary),
    ];
    if let Some(args) = args {
        fields.push(format!("args: {args}"));
    }
    fields.extend([
        format!("call:{} run:{}", card.call.as_str(), run.unwrap_or("?")),
        format!(
            "grant:{} runtime-bound; deadline {}s run-elapsed (monotonic, rechecked at dispatch)",
            card.approval.as_str(),
            card.expires_at_elapsed.as_secs(),
        ),
    ]);
    fields
}

/// Materializes fields up to `budget` rows, never more.
fn push_fields(fields: &[String], width: usize, budget: usize, out: &mut Vec<String>) {
    for field in fields {
        if out.len() >= budget {
            return;
        }
        push_wrapped(field, width, budget, out);
    }
}

/// Materializes only wrapped rows `skip..skip + take` of the full detail.
/// Chunk counts match [`wrapped_height`] so layout and measurement cannot
/// drift, and skipped rows are never allocated.
fn push_detail_range(
    fields: &[String],
    width: usize,
    skip: usize,
    take: usize,
    out: &mut Vec<String>,
) {
    if take == 0 {
        return;
    }
    let width = width.max(1);
    let limit = skip.saturating_add(take);
    let mut index = 0usize;
    for field in fields {
        if index >= limit {
            break;
        }
        if field.is_empty() {
            if index >= skip {
                out.push(String::new());
            }
            index += 1;
            continue;
        }
        let chars: Vec<char> = field.chars().collect();
        for chunk in chars.chunks(width) {
            if index >= limit {
                break;
            }
            if index >= skip {
                out.push(chunk.iter().collect());
            }
            index += 1;
        }
    }
}

/// Hard-wrapped chunks of one logical line, stopped at `budget` rows. Chunk
/// counts match [`wrapped_height`] so layout and measurement cannot drift.
fn push_wrapped(text: &str, width: usize, budget: usize, out: &mut Vec<String>) {
    if budget == 0 {
        return;
    }
    if text.is_empty() {
        out.push(String::new());
        return;
    }
    let width = width.max(1);
    let chars: Vec<char> = text.chars().collect();
    for chunk in chars.chunks(width) {
        if out.len() >= budget {
            break;
        }
        out.push(chunk.iter().collect());
    }
}

fn render_approval(layout: &ApprovalLayout, area: Rect, buf: &mut Buffer) {
    let title = if layout.expanded {
        let end = layout
            .scroll
            .saturating_add(layout.view_rows)
            .min(layout.detail_rows);
        format!(
            " approval detail {}-{}/{} · PgUp/PgDn · i/esc close ",
            layout.scroll.saturating_add(1),
            end,
            layout.detail_rows,
        )
    } else if layout.clipped {
        " approval required (detail clipped · [i] inspect) ".to_owned()
    } else {
        " approval required: allow-once / deny ".to_owned()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
        .title(Line::from(title));
    let inner = block.inner(area);
    block.render(area, buf);
    let last = layout.lines.len().saturating_sub(1);
    let lines: Vec<Line> = layout
        .lines
        .iter()
        .enumerate()
        .map(|(index, text)| {
            let hint_row = index == last && (layout.expanded || layout.clipped);
            let style = if hint_row && text.starts_with("[a] allow once") {
                Style::default().fg(Color::Green)
            } else if hint_row {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else if text.starts_with("[a] allow once") {
                Style::default().fg(Color::Green)
            } else if text.starts_with("decisions go") {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };
            Line::from(Span::styled(text.clone(), style))
        })
        .collect();
    Paragraph::new(lines).render(inner, buf);
}

fn render_composer(state: &AppState, area: Rect, buf: &mut Buffer, focus: Focus) {
    let style = if focus == Focus::Composer {
        Style::default().fg(Color::Green)
    } else {
        Style::default()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(" composer (fixed) ");
    let inner = block.inner(area);
    block.render(area, buf);
    let mut text = state.composer().to_owned();
    if focus == Focus::Composer {
        text.push('▊');
    }
    Paragraph::new(text).render(inner, buf);
}

fn render_footer(state: &AppState, area: Rect, buf: &mut Buffer, focus: Focus) {
    let cancel_hint = if state.can_cancel() {
        "ctrl+c cancel"
    } else {
        "idle"
    };
    let approval_hint = if state.approval_detail_open() {
        " [i] close · pgup/pgdn "
    } else if state.pending_approval().is_some() {
        " [i] inspect "
    } else {
        ""
    };
    let footer = Paragraph::new(Line::from(vec![
        Span::raw(" m0-test "),
        Span::styled(cancel_hint, Style::default().fg(Color::Yellow)),
        Span::styled(approval_hint, Style::default().fg(Color::Yellow)),
        Span::raw(" tab focus · enter submit · fold ←/→ · approval i/a/d · ctrl+d quit "),
        Span::raw(
            format!(
                "focus:{focus:?} stale:{} seq:{} dropped:{}",
                state.stale_rejected, state.seq_rejected, state.dropped_entries
            )
            .to_lowercase(),
        ),
    ]));
    footer.render(area, buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::{
        ApprovalId, ApprovalNotice, CallId, EventPayload, RequestId, RunEvent, SessionId,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Duration;

    fn harness(width: u16, height: u16) -> (AppState, Terminal<TestBackend>) {
        let backend = TestBackend::new(width, height);
        let terminal = Terminal::new(backend).expect("test terminal builds");
        (AppState::new(), terminal)
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let mut rows = Vec::new();
        for y in 0..buffer.area.height {
            let mut row = String::new();
            for x in 0..buffer.area.width {
                row.push_str(buffer.get(x, y).symbol());
            }
            rows.push(row);
        }
        rows.join("\n")
    }

    fn draw(state: &mut AppState, terminal: &mut Terminal<TestBackend>, focus: Focus) {
        terminal
            .draw(|frame| render(state, frame.size(), frame.buffer_mut(), focus))
            .expect("test draw succeeds");
    }

    fn started(state: &mut AppState, seq: u64) {
        let event = RunEvent::new(
            SessionId::new("sess-1").expect("valid"),
            nexus_core::RunId::new("run-1").expect("valid"),
            seq,
            EventPayload::RunStarted {
                request: RequestId::new("req-1").expect("valid"),
            },
        );
        assert!(state.apply_event(&event));
    }

    fn text_event(seq: u64, text: &str) -> RunEvent {
        use nexus_core::{AssistantText, TurnId};
        RunEvent::new(
            SessionId::new("sess-1").expect("valid"),
            nexus_core::RunId::new("run-1").expect("valid"),
            seq,
            EventPayload::AssistantTextDelta(
                AssistantText::new(TurnId::new("t1-0").expect("valid"), "item-0", text)
                    .expect("fragment builds"),
            ),
        )
    }

    fn approval_event(seq: u64, summary: &str, scope: &str) -> RunEvent {
        let notice = ApprovalNotice::new(
            ApprovalId::new("a1-0").expect("valid"),
            CallId::new("c1-0").expect("valid"),
            summary,
            scope,
            Duration::from_secs(120),
        )
        .expect("notice builds");
        RunEvent::new(
            SessionId::new("sess-1").expect("valid"),
            nexus_core::RunId::new("run-1").expect("valid"),
            seq,
            EventPayload::ApprovalRequired(notice),
        )
    }

    #[test]
    fn initial_frame_keeps_composer_and_footer_visible() {
        let (mut state, mut terminal) = harness(80, 24);
        draw(&mut state, &mut terminal, Focus::Composer);
        let frame = screen(&terminal);
        assert!(frame.contains("nexus-tui"), "header present");
        assert!(frame.contains("composer (fixed)"), "fixed composer present");
        assert!(frame.contains("m0-test"), "footer present");
        assert!(frame.contains("no run"), "accurate not-ready state");
    }

    #[test]
    fn approval_card_shows_exact_operation_and_choices() {
        let (mut state, mut terminal) = harness(80, 24);
        started(&mut state, 0);
        assert!(state.apply_event(&approval_event(1, "run tool host_write", "project scope")));
        draw(&mut state, &mut terminal, Focus::ApprovalCard);
        let frame = screen(&terminal);
        assert!(frame.contains("approval required"), "card is shown");
        assert!(
            frame.contains("run tool host_write"),
            "exact operation shown"
        );
        assert!(frame.contains("project scope"), "affected scope shown");
        assert!(frame.contains("c1-0"), "bound call shown");
        assert!(frame.contains("[a] allow once"), "allow-once offered");
        assert!(frame.contains("[d] deny"), "deny offered");
        assert!(
            !frame.to_lowercase().contains("always"),
            "no always-approve row"
        );
        let geometry = state.approval_geometry().expect("geometry recorded");
        assert!(!geometry.clipped, "card fits the frame");
        assert!(
            state.approval_decision_allowed(),
            "fully visible card may be decided"
        );
        assert!(
            frame.contains("[i] inspect"),
            "footer names the inspect key while an approval is pending"
        );
    }

    #[test]
    fn footer_and_titles_name_the_inspect_key_for_a_pending_approval() {
        let (mut state, mut terminal) = harness(120, 24);
        started(&mut state, 0);
        draw(&mut state, &mut terminal, Focus::ApprovalCard);
        assert!(
            !screen(&terminal).contains("[i] inspect"),
            "no inspect hint without a live approval"
        );
        assert!(state.apply_event(&approval_event(1, "run tool host_write", "project scope")));
        draw(&mut state, &mut terminal, Focus::ApprovalCard);
        let compact = screen(&terminal);
        assert!(
            compact.contains("[i] inspect"),
            "pending approval names inspect"
        );
        assert!(compact.contains("approval i/a/d"), "footer lists i/a/d");
        state.inspect_approval();
        draw(&mut state, &mut terminal, Focus::ApprovalCard);
        let expanded = screen(&terminal);
        assert!(expanded.contains("[i] close"), "open detail names close");
        assert!(
            expanded.contains("i/esc close"),
            "expanded title names the close keys"
        );
    }

    #[test]
    fn approval_card_shows_the_exact_arguments_preview_when_attached() {
        let (mut state, mut terminal) = harness(80, 24);
        started(&mut state, 0);
        let notice = ApprovalNotice::new(
            ApprovalId::new("a1-0").expect("valid"),
            CallId::new("c1-0").expect("valid"),
            "run tool host_write",
            "project scope",
            Duration::from_secs(120),
        )
        .expect("notice builds")
        .with_args_preview("path=src/main.rs")
        .expect("preview builds");
        let event = RunEvent::new(
            SessionId::new("sess-1").expect("valid"),
            nexus_core::RunId::new("run-1").expect("valid"),
            1,
            EventPayload::ApprovalRequired(notice),
        );
        assert!(state.apply_event(&event));
        draw(&mut state, &mut terminal, Focus::ApprovalCard);
        let frame = screen(&terminal);
        assert!(frame.contains("args: path=src/main.rs"), "preview shown");
        assert!(!state.approval_detail_truncated());
        assert!(state.approval_decision_allowed());
    }

    #[test]
    fn clipped_approval_card_hides_decisions_and_locks_the_gate() {
        let (mut state, mut terminal) = harness(48, 16);
        started(&mut state, 0);
        let summary = "s".repeat(900);
        let scope = "c".repeat(900);
        assert!(state.apply_event(&approval_event(1, &summary, &scope)));
        draw(&mut state, &mut terminal, Focus::ApprovalCard);
        let frame = screen(&terminal);
        let geometry = state.approval_geometry().expect("geometry recorded");
        assert!(geometry.clipped, "long detail does not fit");
        assert!(
            !state.approval_decision_allowed(),
            "clipped card cannot be approved"
        );
        assert!(frame.contains("detail clipped"), "clip is explicit");
        assert!(
            frame.contains("[i] inspect"),
            "clipped card names the inspect key"
        );
        assert!(
            !frame.contains("[a] allow once"),
            "no decision affordance on a clipped card"
        );
        // Opening the expanded detail is not a grant: the hidden rows must
        // actually be rendered, from the first row to the last.
        state.inspect_approval();
        draw(&mut state, &mut terminal, Focus::ApprovalCard);
        assert!(state.approval_detail_open(), "expanded detail is shown");
        assert!(
            !state.approval_decision_allowed(),
            "opening the detail view does not unlock the gate"
        );
        let expanded = screen(&terminal);
        assert!(
            expanded.contains("approval detail"),
            "detail view is titled"
        );
        assert!(
            expanded.contains("[d] deny"),
            "deny stays reachable while inspecting"
        );
        let mut steps = 0;
        while !state.approval_detail_seen_all() && steps < 256 {
            state.approval_detail_page(1);
            draw(&mut state, &mut terminal, Focus::ApprovalCard);
            steps += 1;
        }
        assert!(state.approval_detail_seen_all(), "every row was rendered");
        assert!(
            state.approval_decision_allowed(),
            "deliberate full inspection unlocks the same live identity"
        );
        let inspected = screen(&terminal);
        assert!(
            inspected.contains("[a] allow once"),
            "allow-once appears only after the full detail was seen"
        );
    }

    #[test]
    fn long_operation_detail_is_readable_in_the_expanded_view() {
        let (mut state, mut terminal) = harness(60, 18);
        started(&mut state, 0);
        let summary = format!("deploy {} to production", "very-long-segment-".repeat(40));
        assert!(state.apply_event(&approval_event(1, &summary, "project scope")));
        state.inspect_approval();
        draw(&mut state, &mut terminal, Focus::ApprovalCard);
        let frame = screen(&terminal);
        assert!(frame.contains("approval detail"), "expanded view rendered");
        assert!(
            frame.contains("operation: deploy"),
            "operation detail stays readable"
        );
        assert!(
            frame.contains("PgUp/PgDn"),
            "scroll indicator names the keys"
        );
        assert!(frame.contains("i/esc close"), "expanded title names close");
        assert!(!state.approval_decision_allowed());
    }

    #[test]
    fn truncation_indicator_marks_the_presentation_bound() {
        let (mut state, mut terminal) = harness(40, 12);
        started(&mut state, 0);
        for index in 0..20 {
            state.record_submitted(&format!("message number {index}"));
        }
        draw(&mut state, &mut terminal, Focus::Viewport);
        let frame = screen(&terminal);
        assert!(frame.contains("presentation-truncated"), "truncation shown");
        assert!(frame.contains("message number 19"), "tail stays visible");
    }

    #[test]
    fn wrapping_respects_frame_width_and_indentation() {
        let (mut state, mut terminal) = harness(30, 14);
        started(&mut state, 0);
        let long = "word ".repeat(40);
        assert!(state.apply_event(&text_event(1, &long)));
        draw(&mut state, &mut terminal, Focus::Viewport);
        let frame = screen(&terminal);
        for row in frame.lines() {
            assert!(
                row.chars().count() <= 30,
                "no row exceeds the frame width: {row:?}"
            );
        }
        let view = state.visible_lines(30, 20);
        assert!(
            view.lines.iter().all(|line| line.chars().count() <= 30),
            "presentation lines fit the available width"
        );
        assert!(
            view.lines.iter().any(|line| line.starts_with("  word")),
            "body continuation keeps the two-space indent"
        );
    }

    #[test]
    fn page_scroll_persists_across_redraws() {
        let (mut state, mut terminal) = harness(40, 12);
        started(&mut state, 0);
        for index in 0..30 {
            state.record_submitted(&format!("entry {index}"));
        }
        draw(&mut state, &mut terminal, Focus::Viewport);
        assert!(screen(&terminal).contains("entry 29"), "tail first");
        state.scroll_up(state.viewport_height());
        draw(&mut state, &mut terminal, Focus::Viewport);
        assert!(state.scrollback() > 0, "page position survives the redraw");
        let scrolled = screen(&terminal);
        assert!(scrolled.contains("entry 24"), "older content is visible");
        assert!(!scrolled.contains("entry 29"), "tail moved out of view");
        draw(&mut state, &mut terminal, Focus::Viewport);
        assert_eq!(scrolled, screen(&terminal), "no drift between frames");
    }

    #[test]
    fn untrusted_output_cannot_inject_control_sequences() {
        let (mut state, mut terminal) = harness(80, 24);
        started(&mut state, 0);
        assert!(state.apply_event(&text_event(1, "x\x1b[2Jy")));
        draw(&mut state, &mut terminal, Focus::Viewport);
        let frame = screen(&terminal);
        assert!(!frame.contains('\x1b'), "no raw escape reaches the frame");
    }

    #[test]
    fn hostile_titles_stay_sanitized_and_bounded_on_screen() {
        use crate::state::MAX_TITLE_BYTES;
        let (mut state, mut terminal) = harness(80, 24);
        started(&mut state, 0);
        let event = RunEvent::new(
            SessionId::new("sess-1").expect("valid"),
            nexus_core::RunId::new("run-1").expect("valid"),
            1,
            EventPayload::ToolCallPreview {
                item_key: format!("\x1b[2J{}", "t".repeat(MAX_TITLE_BYTES * 8)),
            },
        );
        assert!(state.apply_event(&event));
        draw(&mut state, &mut terminal, Focus::Viewport);
        let frame = screen(&terminal);
        assert!(!frame.contains('\x1b'), "title cannot inject escapes");
    }
}
