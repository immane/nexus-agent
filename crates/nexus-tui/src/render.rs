//! Immediate-mode renderer: header, bounded viewport, approval card,
//! fixed composer, footer.
//!
//! Rendering is event-driven through [`crate::RefreshGate`] (the caller
//! redraws only when the gate is ready), and only the visible window from
//! [`AppState::visible_lines`] is laid out — history outside the window is
//! counted, never reprocessed.

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::keys::Focus;
use crate::state::AppState;

/// Minimum usable frame; smaller frames get a one-line notice instead of a
/// clipped layout.
const MIN_WIDTH: u16 = 20;
/// Reserved lines: header (1) + footer (1) + composer chrome (2).
const CHROME_LINES: u16 = 4;
/// Composer body lines, clamped so history keeps the frame.
const MAX_COMPOSER_BODY: u16 = 5;
/// Approval card body lines.
const APPROVAL_BODY: u16 = 5;

/// Frame regions plus the viewport geometry handed back to presentation
/// state for selection math.
struct Regions {
    header: Rect,
    body: Rect,
    approval: Option<Rect>,
    composer: Rect,
    footer: Rect,
}

fn split(area: Rect, composer_body: u16, approval: bool) -> Regions {
    let approval_rows = if approval { APPROVAL_BODY + 2 } else { 0 };
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
        approval: approval.then_some(chunks[2]),
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
    let regions = split(area, composer_body, state.pending_approval().is_some());
    render_header(state, regions.header, buf, focus);
    render_body(state, regions.body, buf);
    if let Some(area) = regions.approval {
        render_approval(state, area, buf);
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
    state.set_viewport(width, height);
    // The truncation banner occupies one body row when shown; shrink the
    // window first so the live tail is never the line clipped away.
    let probe = state.visible_lines(width, height);
    let content_height = if probe.hidden_above > 0 || probe.retention_truncated {
        height.saturating_sub(1).max(1)
    } else {
        height
    };
    let view = if content_height == height {
        probe
    } else {
        state.visible_lines(width, content_height)
    };
    let mut lines: Vec<Line> = Vec::with_capacity(height);
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

fn render_approval(state: &AppState, area: Rect, buf: &mut Buffer) {
    let Some(card) = state.pending_approval() else {
        return;
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
        .title(" approval required: allow-once / deny ");
    let inner = block.inner(area);
    block.render(area, buf);
    let lines = vec![
        Line::from(vec![
            Span::styled("operation: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(card.summary.clone()),
        ]),
        Line::from(vec![
            Span::styled("scope: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(card.scope_summary.clone()),
        ]),
        Line::from(format!(
            "call:{} run:{}",
            card.call.as_str(),
            state.active_run().map(|run| run.as_str()).unwrap_or("?"),
        )),
        Line::from(format!(
            "grant:{} expires in {}s (runtime binding, args immutable)",
            card.approval.as_str(),
            card.expires_at_elapsed.as_secs(),
        )),
        Line::from(Span::styled(
            "[a] allow once   [d] deny   [esc] park (no decision)",
            Style::default().fg(Color::Green),
        )),
        Line::from(Span::styled(
            "decisions go through the runtime only; previews never authorize",
            Style::default().add_modifier(Modifier::DIM),
        )),
    ];
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
    let footer = Paragraph::new(Line::from(vec![
        Span::raw(" m0-test "),
        Span::styled(cancel_hint, Style::default().fg(Color::Yellow)),
        Span::raw(" tab focus · enter submit · fold ←/→ · approval a/d · ctrl+d quit "),
        Span::raw(
            format!(
                "focus:{focus:?} stale:{} dropped:{}",
                state.stale_rejected, state.dropped_entries
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
        let notice = ApprovalNotice::new(
            ApprovalId::new("a1-0").expect("valid"),
            CallId::new("c1-0").expect("valid"),
            "run tool host_write",
            "project scope",
            Duration::from_secs(120),
        )
        .expect("notice builds");
        let event = RunEvent::new(
            SessionId::new("sess-1").expect("valid"),
            nexus_core::RunId::new("run-1").expect("valid"),
            1,
            EventPayload::ApprovalRequired(notice),
        );
        assert!(state.apply_event(&event));
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
    fn untrusted_output_cannot_inject_control_sequences() {
        use nexus_core::{AssistantText, TurnId};
        let (mut state, mut terminal) = harness(80, 24);
        started(&mut state, 0);
        let event = RunEvent::new(
            SessionId::new("sess-1").expect("valid"),
            nexus_core::RunId::new("run-1").expect("valid"),
            1,
            EventPayload::AssistantTextDelta(
                AssistantText::new(TurnId::new("t1-0").expect("valid"), "item-0", "x\x1b[2Jy")
                    .expect("fragment builds"),
            ),
        );
        assert!(state.apply_event(&event));
        draw(&mut state, &mut terminal, Focus::Viewport);
        let frame = screen(&terminal);
        assert!(!frame.contains('\x1b'), "no raw escape reaches the frame");
    }
}
