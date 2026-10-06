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
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, StatefulWidget,
    Widget,
};

use crate::keys::Focus;
use crate::markdown::{MdStyle, StyledRun};
use crate::state::{AppState, ApprovalGeometry, PendingApprovalCard, wrapped_height};

/// Maps width-neutral Markdown style bits to terminal styling. Only color
/// and modifiers change; the visible text (and its measured width) is
/// untouched.
fn markdown_style(style: MdStyle) -> Style {
    let mut out = Style::default();
    if style.contains(MdStyle::BOLD) || style.contains(MdStyle::HEADING) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.contains(MdStyle::ITALIC) || style.contains(MdStyle::QUOTE) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.contains(MdStyle::STRIKE) {
        out = out.add_modifier(Modifier::CROSSED_OUT);
    }
    if style.contains(MdStyle::LINK) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    if style.contains(MdStyle::CODE) {
        out = out.fg(Color::Yellow);
    } else if style.contains(MdStyle::HEADING) {
        out = out.fg(Color::LightBlue);
    } else if style.contains(MdStyle::LINK) {
        out = out.fg(Color::Cyan);
    } else if style.contains(MdStyle::LINK_URL) || style.contains(MdStyle::QUOTE) {
        out = out.add_modifier(Modifier::DIM);
    }
    out
}

/// Builds one body row from its visible text and style runs. Runs hold
/// character offsets in line coordinates; gaps stay unstyled. An empty run
/// list takes the plain fast path with identical output.
fn styled_line(text: &str, runs: &[StyledRun]) -> Line<'static> {
    if runs.is_empty() {
        return Line::from(text.to_owned());
    }
    // Character offsets to byte offsets (runs never split UTF-8).
    let bytes: Vec<usize> = text
        .char_indices()
        .map(|(offset, _)| offset)
        .chain(std::iter::once(text.len()))
        .collect();
    let byte_at =
        |char_index: usize| -> usize { bytes.get(char_index).copied().unwrap_or(text.len()) };
    let mut spans = Vec::with_capacity(runs.len() * 2 + 1);
    let mut cursor = 0usize;
    for run in runs {
        let from = byte_at(run.start);
        let to = byte_at(run.start + run.len);
        if from > byte_at(cursor) {
            spans.push(Span::raw(text[byte_at(cursor)..from].to_owned()));
        }
        if from < to {
            spans.push(Span::styled(
                text[from..to].to_owned(),
                markdown_style(run.style),
            ));
        }
        cursor = cursor.max(run.start + run.len);
    }
    if cursor < text.chars().count() {
        spans.push(Span::raw(text[byte_at(cursor)..].to_owned()));
    }
    Line::from(spans)
}

/// Welcome hero rows: the gradient banner plus breathing room and the
/// subtitle, centered in a `width`-wide body. The caller guarantees the
/// fit, so no row exceeds `width`.
fn hero_lines() -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(EMPTY_ROWS);
    lines.push(Line::from(""));
    for (index, row) in LOGO.iter().enumerate() {
        let (red, green, blue) = LOGO_GRADIENT[index];
        lines.push(Line::from(Span::styled(
            row.to_owned(),
            Style::default()
                .fg(Color::Rgb(red, green, blue))
                .add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        EMPTY_SUBTITLE.to_owned(),
        Style::default().add_modifier(Modifier::DIM),
    )));
    lines
}

/// Small-terminal hero: the wordmark plus subtitle, left-aligned. Two rows.
fn wordmark_lines() -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            WORDMARK.to_owned(),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            EMPTY_SUBTITLE.to_owned(),
            Style::default().add_modifier(Modifier::DIM),
        )),
    ]
}

/// Banner rows (single-cell glyphs only, so character count equals cell
/// count; short rows leave trailing blanks).
const LOGO: &[&str] = &[
    "███╗   ██╗███████╗██╗  ██╗██╗   ██╗███████╗     █████╗  ██████╗ ███████╗███╗   ██╗████████╗",
    "████╗  ██║██╔════╝╚██╗██╔╝██║   ██║██╔════╝    ██╔══██╗██╔════╝ ██╔════╝████╗  ██║╚══██╔══╝",
    "██╔██╗ ██║█████╗   ╚███╔╝ ██║   ██║███████╗    ███████║██║  ███╗█████╗  ██╔██╗ ██║   ██║",
    "██║╚██╗██║██╔══╝   ██╔██╗ ██║   ██║╚════██║    ██╔══██║██║   ██║██╔══╝  ██║╚██╗██║   ██║",
    "██║ ╚████║███████╗██╔╝ ██╗╚██████╔╝███████║    ██║  ██║╚██████╔╝███████╗██║ ╚████║   ██║",
    "╚═╝  ╚═══╝╚══════╝╚═╝  ╚═╝ ╚═════╝ ╚══════╝    ╚═╝  ╚═╝ ╚═════╝ ╚══════╝╚═╝  ╚═══╝   ╚═╝",
];

/// Banner width in cells (the widest row; shorter rows leave blanks).
const LOGO_WIDTH: u16 = 91;

/// Green gradient, one entry per banner row.
const LOGO_GRADIENT: [(u8, u8, u8); 6] = [
    (74, 222, 128),
    (63, 208, 116),
    (52, 194, 105),
    (42, 180, 94),
    (32, 168, 84),
    (22, 163, 74),
];

/// Banner rows plus the leading blank, breathing room, and the subtitle.
const EMPTY_ROWS: usize = 9;

/// Small-terminal hero height: wordmark plus subtitle.
const WORDMARK_ROWS: usize = 2;

/// Empty-state subtitle under the banner or wordmark.
const EMPTY_SUBTITLE: &str = "M0-TEST demo · fast to start · small by design";

/// Small-terminal wordmark, centered when the banner does not fit.
const WORDMARK: &str = "nexus-agent";

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
    let mut spans = vec![
        Span::styled(" nexus-tui ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("M0-TEST "),
        Span::styled(format!("run:{run} "), Style::default().fg(Color::Cyan)),
        Span::raw(state.status()),
        Span::raw(format!(" focus:{focus:?}").to_lowercase()),
    ];
    // The directory trails every existing segment, so frames that never
    // set it render byte-identically and narrow frames clip the path
    // before any status marker.
    if let Some(label) = state.session_label() {
        spans.push(Span::raw(format!(" sess:{label}")));
    }
    if let Some(dir) = state.project_dir() {
        spans.push(Span::raw(format!(" dir:{}", abbreviate_home(dir))));
    }
    let header = Paragraph::new(Line::from(spans));
    header.render(area, buf);
}

/// Abbreviates a display path by folding a leading home directory to `~`.
/// Pure presentation: the stored directory is never rewritten.
fn abbreviate_home(path: &str) -> String {
    abbreviate_home_with(path, std::env::var("HOME").ok().as_deref())
}

/// Folds `home` to `~` when it is a real prefix (a full path component,
/// not a string prefix like `/home/user2` under `/home/user`).
fn abbreviate_home_with(path: &str, home: Option<&str>) -> String {
    let Some(home) = home.filter(|home| !home.is_empty()) else {
        return path.to_owned();
    };
    if path == home {
        return "~".to_owned();
    }
    match path.strip_prefix(home) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_owned(),
    }
}

fn render_body(state: &mut AppState, area: Rect, buf: &mut Buffer) {
    let (width, height) = (area.width as usize, area.height as usize);
    // A one-cell scrollbar appears only when history exceeds the window; the
    // text then wraps one cell narrower so the bar never covers content.
    let needs_bar = state.total_height(width.saturating_sub(1).max(1)) > height;
    let text_width = if needs_bar && width > 1 {
        width - 1
    } else {
        width
    };
    // Reserve one row for the indicator before materializing the window, so
    // the live tail is never the line clipped away. Heights are cached, so
    // this probe neither re-measures nor re-renders history.
    let banner = state.truncation_indicator(text_width, height);
    let content_height = if banner {
        height.saturating_sub(1).max(1)
    } else {
        height
    };
    state.set_viewport(text_width, content_height);
    let view = state.visible_lines(text_width, content_height);
    let truncated = view.hidden_above > 0 || view.retention_truncated;
    let mut lines: Vec<Line> = Vec::with_capacity(content_height + 1);
    // Welcome hero: while the whole conversation fits the window, the logo
    // owns the top of the body and content grows beneath it. Once history
    // exceeds the window the hero yields every row to content, so it scrolls
    // away naturally instead of competing for space.
    let used = view.lines.len() + usize::from(truncated);
    if !truncated && text_width >= LOGO_WIDTH as usize && used + EMPTY_ROWS <= content_height {
        lines.extend(hero_lines());
    } else if !truncated && text_width >= WORDMARK.len() && used + WORDMARK_ROWS <= content_height {
        lines.extend(wordmark_lines());
    }
    if truncated {
        lines.push(Line::from(Span::styled(
            // Short enough to survive narrow frames: the bound notice must
            // itself never be clipped away.
            format!("… {}↑ presentation-truncated", view.hidden_above),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::DIM),
        )));
    }
    for (line, runs) in view.lines.iter().zip(&view.styles) {
        lines.push(styled_line(line, runs));
    }
    let text_area = Rect {
        width: text_width as u16,
        ..area
    };
    Paragraph::new(lines).render(text_area, buf);
    if needs_bar && width > 1 {
        let total = state.total_height(text_width);
        let mut bar = ScrollbarState::new(total)
            .position(view.hidden_above)
            .viewport_content_length(content_height);
        let bar_area = Rect {
            x: area.x + area.width.saturating_sub(1),
            width: 1,
            ..area
        };
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(None)
            .thumb_symbol("▐")
            .render(bar_area, buf, &mut bar);
    }
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
    // The composer border names the active agent mode: read-only modes
    // (plan and read-only customs) render amber, full-capability modes keep
    // the focused green. Unfocused keeps the plain border as before.
    let style = if focus == Focus::Composer {
        if state.mode().read_only {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::Green)
        }
    } else {
        Style::default()
    };
    // The selected model rides in the title, after the fixed marker every
    // existing test pins; an unselected model renders the bare title. The
    // mode rides right after the marker so the switch is always labeled.
    let mut title = format!(" composer (fixed) · {} ", state.mode().id);
    if let Some(model) = state.active_model() {
        title.push_str(&format!("· {model}"));
        if let Some(provider) = state.active_provider() {
            title.push_str(&format!(" @ {provider}"));
        }
        title.push(' ');
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(title);
    let inner = block.inner(area);
    block.render(area, buf);
    // An empty focused composer shows a dimmed invitation instead of a
    // bare caret; anything typed replaces it, and other focuses render
    // the draft (or nothing) exactly as before.
    let draft = state.composer().to_owned();
    let line = if draft.is_empty() && focus == Focus::Composer {
        Line::from(vec![
            Span::styled(
                "Type a task or /help for commands",
                Style::default().add_modifier(Modifier::DIM),
            ),
            Span::raw("▊"),
        ])
    } else {
        let mut text = draft;
        if focus == Focus::Composer {
            text.push('▊');
        }
        Line::from(text)
    };
    Paragraph::new(line).render(inner, buf);
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
    let mut spans = vec![
        Span::raw(" m0-test "),
        Span::styled(cancel_hint, Style::default().fg(Color::Yellow)),
        Span::styled(approval_hint, Style::default().fg(Color::Yellow)),
        Span::raw(" tab mode · enter submit · ↕ · m model · fold · approval i/a/d · ctrl+d quit "),
        Span::raw(
            format!(
                "focus:{focus:?} stale:{} seq:{} dropped:{}",
                state.stale_rejected, state.seq_rejected, state.dropped_entries
            )
            .to_lowercase(),
        ),
    ];
    // Context usage trails every existing segment, so frames without
    // observed usage render byte-identically and narrow frames clip the
    // counters before any status marker. Unknown stays `?`, never zero.
    if let Some(usage) = state.last_usage() {
        spans.push(Span::raw(format!(
            " ctx in:{} out:{}",
            counter(usage.input_tokens()),
            counter(usage.output_tokens())
        )));
    }
    let footer = Paragraph::new(Line::from(spans));
    footer.render(area, buf);
}

/// Renders an optional usage counter: the exact number, or `?` for
/// unknown. Unknown is never fabricated as zero.
fn counter(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "?".to_owned())
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

    #[test]
    fn styled_line_splits_runs_without_changing_text() {
        let plain = styled_line("hello", &[]);
        assert_eq!(plain.spans.len(), 1);
        assert_eq!(plain.spans[0].content, "hello");

        let styled = styled_line(
            "  a bold word",
            &[StyledRun {
                start: 4,
                len: 4,
                style: MdStyle::BOLD,
            }],
        );
        let content: String = styled
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(content, "  a bold word", "gaps stay, text is unchanged");
        assert_eq!(styled.spans.len(), 3);
        assert_eq!(styled.spans[1].content, "bold");
        assert_eq!(
            styled.spans[1].style.add_modifier,
            Modifier::BOLD,
            "the bold run maps to the bold modifier"
        );
        assert_eq!(styled.spans[0].style, Style::default());
        assert_eq!(styled.spans[2].style, Style::default());
    }

    #[test]
    fn styled_line_keeps_multibyte_runs_on_char_boundaries() {
        let styled = styled_line(
            "日本語 bold",
            &[StyledRun {
                start: 4,
                len: 4,
                style: MdStyle::CODE,
            }],
        );
        let content: String = styled
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(content, "日本語 bold");
        assert_eq!(styled.spans[1].content, "bold");
        assert_eq!(styled.spans[1].style.fg, Some(Color::Yellow));
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
    fn empty_conversation_shows_the_left_aligned_banner() {
        let (mut state, mut terminal) = harness(100, 30);
        assert_eq!(state.entry_count(), 0, "a fresh view is empty");
        draw(&mut state, &mut terminal, Focus::Composer);
        let frame = screen(&terminal);
        assert!(frame.contains("███╗   ██╗"), "banner present");
        assert!(frame.contains("M0-TEST"), "subtitle present");
        let row = frame
            .lines()
            .find(|row| row.contains("███╗"))
            .expect("banner row");
        assert!(row.starts_with("█"), "banner is left-aligned: {row:?}");
    }

    #[test]
    fn empty_conversation_falls_back_to_the_wordmark_on_small_frames() {
        for (width, height) in [(40, 10), (100, 8), (90, 30)] {
            let (mut state, mut terminal) = harness(width, height);
            draw(&mut state, &mut terminal, Focus::Composer);
            let frame = screen(&terminal);
            assert!(frame.contains("nexus-agent"), "wordmark present");
            assert!(
                !frame.contains("███╗"),
                "no clipped banner at {width}x{height}"
            );
        }
    }

    #[test]
    fn the_first_messages_share_the_body_with_the_hero() {
        let (mut state, mut terminal) = harness(100, 30);
        draw(&mut state, &mut terminal, Focus::Composer);
        assert!(screen(&terminal).contains("███╗"), "banner first");
        state.record_submitted("hello");
        draw(&mut state, &mut terminal, Focus::Composer);
        let frame = screen(&terminal);
        assert!(frame.contains("███╗"), "hero stays while content fits");
        assert!(frame.contains("hello"), "the message stays");
        for index in 0..30 {
            state.record_submitted(&format!("message number {index}"));
        }
        draw(&mut state, &mut terminal, Focus::Composer);
        let crowded = screen(&terminal);
        assert!(
            !crowded.contains("███╗"),
            "the hero yields every row once history overflows"
        );
        assert!(
            crowded.contains("presentation-truncated"),
            "overflow is marked, not clipped silently"
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
        // Three rows per entry (title, body, separator) with a six-row
        // page: the window sits on entries 26-27.
        assert!(scrolled.contains("entry 26"), "older content is visible");
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

#[cfg(test)]
mod cov_render_private {
    //! Coverage for the renderer's private layout arithmetic.
    //!
    //! The public tests observe only whole frames. These tests reach the
    //! private functions directly so each invariant is pinned on its own: the
    //! reserved-chrome constants, the region split, the wrap-budget interaction
    //! between the detail fields and the clip warning, the exact detail-field
    //! text, the compact and expanded layout accounting, and the per-widget
    //! title, hint, and style choices. Everything is a plain function over
    //! fixed `Rect` and `PendingApprovalCard` values: no terminal, no clock, no
    //! threads, no I/O.

    use super::*;
    use crate::state::MAX_TITLE_BYTES;
    use nexus_core::{
        ApprovalId, ApprovalNotice, AssistantText, CallId, EventPayload, RequestId, RunEvent,
        RunId, SessionId, TurnId,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use std::time::Duration;

    fn card(summary: &str, scope: &str) -> PendingApprovalCard {
        PendingApprovalCard {
            approval: ApprovalId::new("a1-0").expect("valid"),
            call: CallId::new("c1-0").expect("valid"),
            summary: summary.to_owned(),
            scope_summary: scope.to_owned(),
            expires_at_elapsed: Duration::from_secs(120),
        }
    }

    /// Reads a buffer as text rows, one string per frame row.
    fn text_rows(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer.get(x, y).symbol())
                    .collect()
            })
            .collect()
    }

    /// Renders one region with the matching private renderer and returns the
    /// text rows, so a single widget can be inspected without a full frame.
    fn rows_of(area: Rect, draw: impl FnOnce(&mut Buffer)) -> Vec<String> {
        let mut buffer = Buffer::empty(area);
        draw(&mut buffer);
        text_rows(&buffer)
    }

    fn row_holding(rows: &[String], needle: &str) -> Option<usize> {
        rows.iter().position(|row| row.contains(needle))
    }

    fn started(seq: u64) -> RunEvent {
        RunEvent::new(
            SessionId::new("sess-1").expect("valid"),
            RunId::new("run-1").expect("valid"),
            seq,
            EventPayload::RunStarted {
                request: RequestId::new("req-1").expect("valid"),
            },
        )
    }

    fn text(seq: u64, body: &str) -> RunEvent {
        RunEvent::new(
            SessionId::new("sess-1").expect("valid"),
            RunId::new("run-1").expect("valid"),
            seq,
            EventPayload::AssistantTextDelta(
                AssistantText::new(TurnId::new("t1-0").expect("valid"), "item-0", body)
                    .expect("fragment builds"),
            ),
        )
    }

    fn approval(seq: u64, summary: &str, scope: &str) -> RunEvent {
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
            RunId::new("run-1").expect("valid"),
            seq,
            EventPayload::ApprovalRequired(notice),
        )
    }

    #[test]
    fn the_reserved_chrome_constants_are_pinned() {
        assert_eq!(MIN_WIDTH, 20);
        assert_eq!(CHROME_LINES, 4);
        assert_eq!(MAX_COMPOSER_BODY, 5);
        assert_eq!(APPROVAL_MIN_ROWS, 3);
        assert_eq!(
            APPROVAL_LOCK,
            "[!] detail clipped: press [i] to inspect before deciding"
        );
    }

    #[test]
    fn split_reserves_the_fixed_chrome_and_hands_the_rest_to_the_body() {
        for (height, composer_body, approval_rows) in [
            (24u16, 1u16, 0u16),
            (24, MAX_COMPOSER_BODY, 0),
            (24, 1, 6),
            (16, 3, 4),
            (10, MAX_COMPOSER_BODY, 0),
        ] {
            let area = Rect::new(0, 0, 40, height);
            let regions = split(area, composer_body, approval_rows);
            assert_eq!(regions.header.height, 1, "the header is one row");
            assert_eq!(regions.footer.height, 1, "the footer is one row");
            assert_eq!(
                regions.composer.height,
                composer_body + 2,
                "the composer is its body plus two border rows"
            );
            assert_eq!(
                regions.approval.map(|rect| rect.height),
                if approval_rows == 0 {
                    None
                } else {
                    Some(approval_rows)
                },
                "the card region exists exactly when rows were measured for it"
            );
            // The regions tile the frame exactly once, top to bottom, at full
            // width, with no gap and no overlap.
            let total = regions.header.height
                + regions.body.height
                + regions.approval.map_or(0, |rect| rect.height)
                + regions.composer.height
                + regions.footer.height;
            assert_eq!(total, height, "{height} rows must be fully allocated");
            assert_eq!(regions.header.y, 0);
            assert_eq!(regions.body.y, 1);
            let after_body = regions.body.y + regions.body.height;
            let after_card = after_body + regions.approval.map_or(0, |rect| rect.height);
            assert_eq!(regions.composer.y, after_card);
            assert_eq!(
                regions.footer.y,
                regions.composer.y + regions.composer.height
            );
            assert_eq!(regions.footer.y + 1, height, "the footer ends the frame");
            for rect in [
                regions.header,
                regions.body,
                regions.composer,
                regions.footer,
            ] {
                assert_eq!(rect.x, 0, "every region spans the full width");
                assert_eq!(rect.width, area.width);
            }
        }
    }

    #[test]
    fn push_wrapped_chunks_by_char_and_honours_an_absolute_row_budget() {
        let mut out = Vec::new();
        push_wrapped("abcdefgh", 3, usize::MAX, &mut out);
        assert_eq!(out, vec!["abc", "def", "gh"]);
        // A zero budget materializes nothing, not even an empty line.
        let mut out = Vec::new();
        push_wrapped("abc", 3, 0, &mut out);
        assert!(out.is_empty());
        // An empty line still occupies one row, so a blank field is not lost.
        let mut out = Vec::new();
        push_wrapped("", 3, 2, &mut out);
        assert_eq!(out, vec![String::new()]);
        // A zero width degrades to one character per row instead of dividing.
        let mut out = Vec::new();
        push_wrapped("ab", 0, usize::MAX, &mut out);
        assert_eq!(out, vec!["a", "b"]);
        // Chunks are cut by character, never by byte, so no grapheme is split.
        let mut out = Vec::new();
        push_wrapped("héllo", 2, usize::MAX, &mut out);
        assert_eq!(out, vec!["hé", "ll", "o"]);
        // The budget is absolute against the rows already present, so a later
        // call cannot push past it, and a call that would fill the budget stops
        // as soon as it is reached.
        let mut out = Vec::new();
        push_wrapped("abcdefgh", 3, 2, &mut out);
        assert_eq!(out, vec!["abc", "def"]);
        push_wrapped("zz", 3, 2, &mut out);
        assert_eq!(out, vec!["abc", "def"], "the budget is absolute");
        let mut out = vec!["kept".to_owned()];
        push_wrapped("abcdefgh", 3, 3, &mut out);
        assert_eq!(
            out,
            vec!["kept", "abc", "def"],
            "the budget counts prior rows"
        );
        // Materializing a whole line agrees with the measured height, which is
        // the invariant the layout relies on.
        for width in 1..=9usize {
            for text in ["", "x", "abcdef", "héllo wörld"] {
                let mut out = Vec::new();
                push_wrapped(text, width, usize::MAX, &mut out);
                assert_eq!(
                    out.len(),
                    wrapped_height(text, width),
                    "{text:?} at width {width}"
                );
                assert_eq!(out.concat(), text, "no character is lost or reordered");
                assert!(
                    out.iter().all(|row| row.chars().count() <= width),
                    "{text:?} at width {width} produced an over-wide row"
                );
            }
        }
    }

    #[test]
    fn push_fields_fills_its_budget_in_order_and_stops() {
        let fields: Vec<String> = vec![
            "one".to_owned(),
            "two three four".to_owned(),
            String::new(),
            "five".to_owned(),
        ];
        // A zero budget writes nothing at all.
        let mut out = Vec::new();
        push_fields(&fields, 7, 0, &mut out);
        assert!(out.is_empty());
        // Everything fits: order and content are preserved, wrapped to width.
        let mut out = Vec::new();
        push_fields(&fields, 7, usize::MAX, &mut out);
        assert_eq!(out, vec!["one", "two thr", "ee four", "", "five"]);
        // A partial budget cuts the tail, never the head or the middle.
        let mut out = Vec::new();
        push_fields(&fields, 7, 3, &mut out);
        assert_eq!(out, vec!["one", "two thr", "ee four"]);
        let mut out = Vec::new();
        push_fields(&fields, 7, 1, &mut out);
        assert_eq!(out, vec!["one"]);
        // No fields means no rows.
        let mut out = Vec::new();
        push_fields(&[], 7, 5, &mut out);
        assert!(out.is_empty());
        // Whatever the budget, the result never exceeds it.
        for budget in 0..=9usize {
            let mut out = Vec::new();
            push_fields(&fields, 5, budget, &mut out);
            assert!(out.len() <= budget, "budget {budget} exceeded");
            assert!(out.iter().all(|row| row.chars().count() <= 5));
        }
    }

    #[test]
    fn push_detail_range_tiles_the_detail_rows_without_gaps_or_repeats() {
        let fields = vec!["abcdefgh".to_owned(), String::new(), "ij".to_owned()];
        // The full materialization matches per-field wrapping and the measured
        // height, including the single row an empty field occupies.
        let mut full = Vec::new();
        push_detail_range(&fields, 3, 0, usize::MAX, &mut full);
        assert_eq!(full, vec!["abc", "def", "gh", "", "ij"]);
        let expected: usize = fields.iter().map(|field| wrapped_height(field, 3)).sum();
        assert_eq!(full.len(), expected, "layout and measurement cannot drift");
        // A zero take materializes nothing, even with rows already present.
        let mut out = vec!["kept".to_owned()];
        push_detail_range(&fields, 3, 0, 0, &mut out);
        assert_eq!(out, vec!["kept"]);
        // Every window is exactly the matching slice of the full rows, so a
        // scrolled view can never skip or repeat a detail row.
        for skip in 0..=full.len() {
            for take in 0..=4usize {
                let mut window = Vec::new();
                push_detail_range(&fields, 3, skip, take, &mut window);
                let end = (skip + take).min(full.len());
                assert_eq!(window, full[skip..end], "skip {skip} take {take}");
            }
        }
        // A zero width degrades to one character per row rather than dividing.
        let mut narrow = Vec::new();
        push_detail_range(&["ab".to_owned()], 0, 0, usize::MAX, &mut narrow);
        assert_eq!(narrow, vec!["a", "b"]);
    }

    #[test]
    fn approval_detail_fields_name_the_exact_operation_scope_and_binding() {
        let card = card("run tool host_write", "project scope");
        let fields = approval_detail_fields(&card, Some("run-1"), Some("path=src/main.rs"));
        assert_eq!(
            fields,
            vec![
                "operation: run tool host_write",
                "scope: project scope",
                "args: path=src/main.rs",
                "call:c1-0 run:run-1",
                "grant:a1-0 runtime-bound; deadline 120s run-elapsed (monotonic, rechecked at dispatch)",
            ]
        );
        // An absent preview contributes no row at all, and an unknown run is
        // reported explicitly rather than omitted.
        let bare = approval_detail_fields(&card, None, None);
        assert_eq!(
            bare,
            vec![
                "operation: run tool host_write",
                "scope: project scope",
                "call:c1-0 run:?",
                "grant:a1-0 runtime-bound; deadline 120s run-elapsed (monotonic, rechecked at dispatch)",
            ]
        );
        assert!(
            !bare.iter().any(|field| field.starts_with("args:")),
            "an absent preview must not fabricate an args row"
        );
        // The deadline is the monotonic run-elapsed reading, never a wall clock
        // and never a remaining time.
        let expired = PendingApprovalCard {
            expires_at_elapsed: Duration::from_secs(0),
            ..card.clone()
        };
        let fields = approval_detail_fields(&expired, None, None);
        assert!(
            fields[3].contains("deadline 0s run-elapsed (monotonic"),
            "{}",
            fields[3]
        );
    }

    #[test]
    fn a_fitting_compact_card_shows_every_field_and_both_decisions() {
        let card = card("run tool host_write", "project scope");
        let args = Some("path=src/main.rs");
        let fields = approval_detail_fields(&card, Some("run-1"), args);
        let area = Rect::new(0, 0, 120, 30);
        let layout = approval_layout(&card, Some("run-1"), args, area, 1, false, 0);
        let detail: usize = fields.iter().map(|field| wrapped_height(field, 118)).sum();
        assert_eq!(
            layout.inner_width, 118,
            "the inner width is the card width minus its borders"
        );
        assert_eq!(
            layout.detail_rows, detail,
            "the measured detail is the sum of the field heights"
        );
        assert!(!layout.clipped);
        assert!(!layout.expanded);
        assert_eq!(layout.scroll, 0, "the compact view never scrolls");
        let rows_cap = 30 - 6;
        assert_eq!(
            layout.view_rows,
            (rows_cap - 2) as usize,
            "the compact card may use the inner rows it was measured for"
        );
        assert!(
            layout.detail_rows <= layout.view_rows,
            "the detail fits, so the card is not clipped"
        );
        // The full detail plus both decision rows are present, in order.
        let text = layout.lines.concat();
        assert!(text.contains("operation: run tool host_write"));
        assert!(text.contains("scope: project scope"));
        assert!(text.contains("args: path=src/main.rs"));
        assert!(text.contains("call:c1-0 run:run-1"));
        assert!(text.contains("grant:a1-0 runtime-bound"));
        assert!(text.contains("[a] allow once   [d] deny"));
        assert!(text.contains("decisions go through the runtime only"));
        assert!(
            !text.contains("[!] detail clipped"),
            "a fitting card must not claim it clipped"
        );
        // The card claims exactly the rows it drew, plus its two border rows.
        assert_eq!(layout.rows, layout.lines.len() as u16 + 2);
        // The geometry reported to the gate is the geometry the card was given.
        let geometry = layout.geometry();
        assert_eq!(geometry.inner_width, layout.inner_width);
        assert_eq!(geometry.inner_rows, layout.view_rows);
        assert_eq!(geometry.detail_rows, layout.detail_rows);
        assert!(!geometry.clipped);
    }

    #[test]
    fn a_clipped_compact_card_hides_every_decision_row() {
        let card = card(&"s".repeat(900), &"c".repeat(900));
        let area = Rect::new(0, 0, 48, 16);
        let layout = approval_layout(&card, Some("run-1"), None, area, 1, false, 0);
        assert_eq!(layout.inner_width, 46);
        assert!(layout.clipped, "the detail cannot fit the measured rows");
        assert!(layout.detail_rows > layout.view_rows);
        assert!(!layout.expanded);
        // No decision affordance survives the clip: the leading detail consumes
        // the budget before a decision row would be reached.
        for forbidden in ["[a] allow once", "[d] deny", "[esc] park", "decisions go"] {
            assert!(
                !layout.lines.iter().any(|line| line.contains(forbidden)),
                "{forbidden:?} must not survive a clip: {:?}",
                layout.lines
            );
        }
        // The card stays within the rows it was measured for.
        assert!(layout.lines.len() <= layout.view_rows);
        assert!(
            layout.rows <= 16 - 6,
            "the card never exceeds the spare rows"
        );
        assert_eq!(layout.rows, layout.lines.len() as u16 + 2);
        let geometry = layout.geometry();
        assert!(geometry.clipped, "the gate is told the detail did not fit");
        assert_eq!(geometry.detail_rows, layout.detail_rows);
    }

    #[test]
    fn a_narrow_clipped_card_still_spends_every_row_on_the_lock_not_a_decision() {
        // In a card too small for the leading detail to matter, the clip
        // warning alone can consume the whole inner budget. Every row must then
        // belong to the warning: no decision may leak into a clipped card.
        let card = card(&"s".repeat(900), &"c".repeat(900));
        let area = Rect::new(0, 0, 24, 10);
        let layout = approval_layout(&card, Some("run-1"), None, area, 1, false, 0);
        assert!(layout.clipped);
        assert_eq!(layout.inner_width, 22);
        let rows_cap = 10 - 6;
        assert_eq!(
            layout.rows, rows_cap,
            "the card claims exactly its spare rows"
        );
        assert_eq!(
            layout.lines.len(),
            layout.view_rows,
            "the inner rows are fully used"
        );
        assert!(
            layout
                .lines
                .iter()
                .all(|line| line.chars().count() <= layout.inner_width),
            "{:?}",
            layout.lines
        );
        // Whatever fills the card, the rows are the warning text or detail, and
        // never a decision.
        for line in &layout.lines {
            assert!(!line.contains("[a]"), "{line:?}");
            assert!(!line.contains("[d] deny"), "{line:?}");
            assert!(!line.contains("[esc] park"), "{line:?}");
        }
        // The warning is present when the budget could fit it.
        let warning = std::iter::once(APPROVAL_LOCK)
            .flat_map(|lock| lock.as_bytes().chunks(layout.inner_width))
            .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
            .collect::<Vec<String>>();
        let expected = warning.len().min(layout.view_rows);
        let reconstructed = layout.lines[..expected].concat();
        assert_eq!(
            reconstructed,
            warning[..expected].concat(),
            "the warning text is materialized verbatim"
        );
    }

    #[test]
    fn the_expanded_layout_keeps_deny_reachable_and_reports_its_window() {
        let card = card(&"s".repeat(900), &"c".repeat(900));
        let fields = approval_detail_fields(&card, Some("run-1"), None);
        let inner_width = 46;
        let detail_rows: usize = fields
            .iter()
            .map(|field| wrapped_height(field, inner_width))
            .sum();
        let rows_cap = 16 - 6;
        let inner_rows = (rows_cap - 2) as usize;
        let view_rows = inner_rows - 1;

        let first = expanded_approval_layout(&fields, detail_rows, inner_width, rows_cap, 0);
        assert_eq!(
            first.rows, rows_cap,
            "the expanded view uses every spare row"
        );
        assert_eq!(
            first.lines.len(),
            view_rows + 1,
            "the detail rows plus exactly one hint row"
        );
        assert_eq!(first.scroll, 0);
        assert!(first.expanded);
        assert!(first.clipped, "the detail is taller than the window");
        assert_eq!(first.geometry().inner_rows, view_rows);
        assert_eq!(first.geometry().detail_rows, detail_rows);
        // The hint row is last and keeps deny reachable while allow is locked.
        let hint = first.lines.last().expect("hint row");
        assert!(hint.contains("[d] deny"), "{hint:?}");
        assert!(!hint.contains("[a] allow once"), "{hint:?}");
        assert!(
            !first
                .lines
                .iter()
                .any(|line| line.contains("[a] allow once")),
            "allow must not appear mid-detail"
        );
        // The displayed detail rows are exactly the rows at that offset.
        let mut full = Vec::new();
        push_detail_range(&fields, inner_width, 0, usize::MAX, &mut full);
        assert_eq!(full.len(), detail_rows);
        assert_eq!(first.lines[..view_rows], full[..view_rows]);

        // Scrolling clamps to the last page, which then reports the whole
        // detail as seen and unlocks allow without ever hiding deny.
        let max_scroll = detail_rows - view_rows;
        let last =
            expanded_approval_layout(&fields, detail_rows, inner_width, rows_cap, usize::MAX);
        assert_eq!(
            last.scroll, max_scroll,
            "the scroll clamps to the last page"
        );
        assert_eq!(last.lines[..view_rows], full[max_scroll..]);
        let hint = last.lines.last().expect("hint row");
        assert!(hint.contains("[d] deny"), "{hint:?}");
        assert!(hint.contains("all detail seen"), "{hint:?}");
        assert!(hint.contains("[a] allow once"), "{hint:?}");
        // An intermediate page is still locked.
        let middle =
            expanded_approval_layout(&fields, detail_rows, inner_width, rows_cap, max_scroll - 1);
        let hint = middle.lines.last().expect("hint row");
        assert!(hint.contains("allow locked until detail end"), "{hint:?}");
    }

    #[test]
    fn an_expanded_layout_that_fits_reports_every_row_and_no_scroll() {
        let card = card("run tool host_write", "project scope");
        let fields = approval_detail_fields(&card, Some("run-1"), None);
        let detail_rows: usize = fields.iter().map(|field| wrapped_height(field, 78)).sum();
        assert_eq!(detail_rows, 5, "four fields, one of which wraps");
        let layout = expanded_approval_layout(&fields, detail_rows, 78, 24, usize::MAX);
        assert_eq!(layout.scroll, 0, "a detail that fits never scrolls");
        assert!(!layout.clipped, "a detail that fits is not clipped");
        assert_eq!(
            layout.lines.len(),
            detail_rows + 1,
            "every detail row plus the hint row"
        );
        let hint = layout.lines.last().expect("hint row");
        assert!(hint.contains("all detail seen"), "{hint:?}");
        assert_eq!(layout.geometry().inner_rows, layout.view_rows);
        assert_eq!(layout.geometry().detail_rows, detail_rows);
    }

    #[test]
    fn an_expanded_card_too_small_for_a_hint_row_still_shows_detail() {
        // A card of the minimum size has a single inner row: no hint row is
        // possible, and the layout must degrade to showing detail only.
        let fields = vec!["only row".to_owned()];
        let layout = expanded_approval_layout(&fields, 1, 20, APPROVAL_MIN_ROWS, 0);
        assert_eq!(layout.rows, APPROVAL_MIN_ROWS);
        assert_eq!(layout.lines, vec!["only row"]);
        assert_eq!(layout.geometry().inner_rows, 1);
        assert!(!layout.clipped);
        assert!(layout.expanded);
    }

    #[test]
    fn header_reports_the_run_status_and_focus() {
        let mut state = AppState::new();
        let area = Rect::new(0, 0, 80, 1);
        let header = rows_of(area, |buf| {
            render_header(&state, area, buf, Focus::Viewport)
        })[0]
            .clone();
        assert!(header.contains("nexus-tui"), "{header:?}");
        assert!(header.contains("M0-TEST"), "{header:?}");
        assert!(header.contains("run:no run"), "{header:?}");
        assert!(header.contains("idle"), "{header:?}");
        assert!(header.contains("focus:viewport"), "{header:?}");

        assert!(state.apply_event(&started(0)));
        for (focus, label) in [
            (Focus::Composer, "focus:composer"),
            (Focus::Viewport, "focus:viewport"),
            (Focus::ApprovalCard, "focus:approvalcard"),
        ] {
            let header = rows_of(area, |buf| render_header(&state, area, buf, focus))[0].clone();
            assert!(header.contains("run:run-1"), "{header:?}");
            assert!(header.contains("running"), "{header:?}");
            assert!(header.contains(label), "{header:?}");
        }
    }

    #[test]
    fn footer_switches_on_cancel_approval_and_detail_state() {
        let mut state = AppState::new();
        let area = Rect::new(0, 0, 200, 1);
        let footer = rows_of(area, |buf| {
            render_footer(&state, area, buf, Focus::Viewport)
        })[0]
            .clone();
        assert!(footer.contains(" m0-test "), "{footer:?}");
        assert!(footer.contains("idle"), "{footer:?}");
        assert!(
            !footer.contains("ctrl+c cancel"),
            "an idle run offers no cancel: {footer:?}"
        );
        assert!(!footer.contains("[i]"), "{footer:?}");
        assert!(footer.contains("stale:0 seq:0 dropped:0"), "{footer:?}");

        assert!(state.apply_event(&started(0)));
        let footer = rows_of(area, |buf| {
            render_footer(&state, area, buf, Focus::Viewport)
        })[0]
            .clone();
        assert!(footer.contains("ctrl+c cancel"), "{footer:?}");

        assert!(state.apply_event(&approval(1, "run tool host_write", "project scope")));
        let footer = rows_of(area, |buf| {
            render_footer(&state, area, buf, Focus::Viewport)
        })[0]
            .clone();
        assert!(footer.contains("[i] inspect"), "{footer:?}");
        assert!(footer.contains("approval i/a/d"), "{footer:?}");

        state.inspect_approval();
        let footer = rows_of(area, |buf| {
            render_footer(&state, area, buf, Focus::Viewport)
        })[0]
            .clone();
        assert!(footer.contains("[i] close"), "{footer:?}");
        assert!(footer.contains("pgup/pgdn"), "{footer:?}");
    }

    #[test]
    fn composer_shows_the_draft_and_marks_the_caret_only_while_focused() {
        let mut state = AppState::new();
        for char in "hello".chars() {
            state.composer_type(char);
        }
        let area = Rect::new(0, 0, 40, 3);
        let mut focused = Buffer::empty(area);
        render_composer(&state, area, &mut focused, Focus::Composer);
        let text = text_rows(&focused);
        assert!(text[0].contains("composer (fixed)"), "{text:?}");
        assert!(text[1].contains("hello"), "{text:?}");
        assert!(text[1].contains('▊'), "{text:?}");
        assert_eq!(
            focused.get(0, 0).fg,
            Color::Green,
            "the focused composer border is green"
        );

        let mut blurred = Buffer::empty(area);
        render_composer(&state, area, &mut blurred, Focus::Viewport);
        let text = text_rows(&blurred);
        assert!(text[1].contains("hello"), "{text:?}");
        assert!(!text[1].contains('▊'), "{text:?}");
        assert_ne!(blurred.get(0, 0).fg, Color::Green);
    }

    #[test]
    fn body_reserves_one_row_for_the_presentation_bound_notice() {
        let mut state = AppState::new();
        for index in 0..20 {
            state.record_submitted(&format!("message {index}"));
        }
        // A body region of seven rows: one is spent on the notice, six remain
        // for the window.
        let area = Rect::new(0, 0, 40, 7);
        let rows = rows_of(area, |buf| render_body(&mut state, area, buf));
        assert!(
            rows[0].trim_end().starts_with('…'),
            "the notice owns the first body row: {rows:?}"
        );
        assert!(rows[0].contains("presentation-truncated"), "{rows:?}");
        assert_eq!(
            state.viewport_height(),
            6,
            "the notice row is reserved before the window is materialized"
        );
        let view = state.visible_lines(39, state.viewport_height());
        // The scrollbar owns the last body column, so the notice text is
        // compared without it.
        let notice_text: String = rows[0].chars().take(39).collect();
        assert_eq!(
            notice_text.trim_end(),
            format!("… {}↑ presentation-truncated", view.hidden_above),
            "the notice reports the exact hidden-line count"
        );
        // The scrollbar owns the last body column here, so content rows are
        // compared without it.
        let text_of = |row: &String| row.chars().take(39).collect::<String>();
        assert!(
            rows.iter()
                .map(text_of)
                .rev()
                .find(|row| !row.trim().is_empty())
                .expect("body row")
                .contains("message 19"),
            "the live tail is never the row clipped away: {rows:?}"
        );

        // With nothing to bound, no row is reserved and the window is taller.
        let mut quiet = AppState::new();
        quiet.record_submitted("short");
        let rows = rows_of(area, |buf| render_body(&mut quiet, area, buf));
        assert!(
            !rows
                .iter()
                .any(|row| row.contains("presentation-truncated")),
            "{rows:?}"
        );
        assert_eq!(
            quiet.viewport_height(),
            7,
            "no row is wasted on a notice that is not needed"
        );
    }

    #[test]
    fn approval_titles_track_the_state_of_the_card_they_render() {
        let short = card("run tool host_write", "project scope");

        // The compact, fully shown card names the bounded decision set.
        let compact = approval_layout(
            &short,
            Some("run-1"),
            None,
            Rect::new(0, 0, 120, 30),
            1,
            false,
            0,
        );
        assert!(!compact.clipped);
        let area = Rect::new(0, 0, 120, compact.rows);
        let rows = rows_of(area, |buf| render_approval(&compact, area, buf));
        let title = rows[0].clone();
        assert!(
            title.contains("approval required: allow-once / deny"),
            "{title:?}"
        );

        // A clipped compact card names the clip and the inspector instead. The
        // detail must be long enough to overflow this frame: the same short
        // card that fits above still fits here, so the long card is what makes
        // the title branch observable.
        let long = card(&"s".repeat(900), &"c".repeat(900));
        let clipped = approval_layout(
            &long,
            Some("run-1"),
            None,
            Rect::new(0, 0, 48, 16),
            1,
            false,
            0,
        );
        assert!(clipped.clipped);
        // Rendered wide enough for the whole title: at the clipping width the
        // border cuts the title itself, which says nothing about its text.
        let area = Rect::new(0, 0, 120, clipped.rows);
        let rows = rows_of(area, |buf| render_approval(&clipped, area, buf));
        let title = rows[0].clone();
        assert!(
            title.contains("approval required (detail clipped · [i] inspect)"),
            "{title:?}"
        );

        // The expanded view names its own scroll position and the keys.
        let expanded = expanded_approval_layout(
            &approval_detail_fields(&short, Some("run-1"), None),
            9,
            46,
            10,
            0,
        );
        let area = Rect::new(0, 0, 120, expanded.rows);
        let rows = rows_of(area, |buf| render_approval(&expanded, area, buf));
        let title = rows[0].clone();
        assert!(title.contains("approval detail 1-7/9"), "{title:?}");
        assert!(title.contains("PgUp/PgDn"), "{title:?}");
        assert!(title.contains("i/esc close"), "{title:?}");
        assert!(!title.contains("approval required"), "{title:?}");
    }

    #[test]
    fn approval_styles_separate_the_decision_rows_from_the_detail_rows() {
        let card = card("run tool host_write", "project scope");
        let layout = approval_layout(
            &card,
            Some("run-1"),
            None,
            Rect::new(0, 0, 120, 30),
            1,
            false,
            0,
        );
        let area = Rect::new(0, 0, 120, layout.rows);
        let mut buffer = Buffer::empty(area);
        render_approval(&layout, area, &mut buffer);
        let rows = text_rows(&buffer);

        // The allow row is green wherever it appears, and the advisory row is
        // dimmed so it reads as a note rather than a control.
        let allow = row_holding(&rows, "[a] allow once").expect("allow row") as u16;
        assert_eq!(buffer.get(1, allow).fg, Color::Green);
        let note = row_holding(&rows, "decisions go").expect("advisory row") as u16;
        assert_eq!(buffer.get(1, note).fg, Color::Reset);
        assert!(buffer.get(1, note).modifier.contains(Modifier::DIM));
        // A plain detail row carries no decision styling.
        let detail = row_holding(&rows, "operation:").expect("detail row") as u16;
        assert_eq!(buffer.get(1, detail).fg, Color::Reset);
        assert_eq!(buffer.get(1, detail).modifier, Modifier::empty());
        // The border is always the loud, red framing the card is meant to have.
        assert_eq!(buffer.get(0, 0).fg, Color::Red);
        assert!(buffer.get(0, 0).modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn rendering_is_immediate_mode_and_agrees_with_a_terminal_draw() {
        let build = || {
            let mut state = AppState::new();
            assert!(state.apply_event(&started(0)));
            assert!(state.apply_event(&text(1, "hello world")));
            state.record_submitted("a typed draft");
            state
        };
        // Rendering into a buffer by hand and rendering through a Terminal must
        // produce identical cells: the renderer keeps no hidden frame state.
        let mut state = build();
        let area = Rect::new(0, 0, 80, 24);
        let direct: Vec<String> = rows_of(area, |buf| {
            render(&mut state, area, buf, Focus::Composer);
        });

        let mut state = build();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal builds");
        terminal
            .draw(|frame| {
                render(
                    &mut state,
                    frame.size(),
                    frame.buffer_mut(),
                    Focus::Composer,
                );
            })
            .expect("test draw succeeds");
        assert_eq!(text_rows(terminal.backend().buffer()), direct);
    }

    #[test]
    fn hostile_entry_titles_are_bounded_and_escape_free_at_insertion() {
        // A hostile tool-preview key is sanitized and bounded when it is
        // inserted, before any layout, width, or card ever sees it.
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        let event = RunEvent::new(
            SessionId::new("sess-1").expect("valid"),
            RunId::new("run-1").expect("valid"),
            1,
            EventPayload::ToolCallPreview {
                item_key: format!("\u{202E}{}", "t".repeat(MAX_TITLE_BYTES * 8)),
            },
        );
        assert!(state.apply_event(&event));
        assert_eq!(
            state.entry_count(),
            2,
            "both the system and preview entries exist"
        );
        for index in 0..state.entry_count() {
            let entry = state.entry(index).expect("entry exists");
            assert!(entry.title.len() <= MAX_TITLE_BYTES, "{:?}", entry.title);
            assert!(!entry.title.contains('\x1b'), "{:?}", entry.title);
            assert!(!entry.title.contains('\u{202E}'), "{:?}", entry.title);
        }
        let preview = state.entry(1).expect("preview entry");
        assert!(
            preview.title.starts_with("preview \\u{202E}"),
            "the bidi override is escaped visibly: {:?}",
            preview.title
        );
        assert_eq!(
            preview.title.len(),
            MAX_TITLE_BYTES,
            "the bounded title is exactly the bound"
        );
    }

    #[test]
    fn header_hides_the_session_label_until_one_is_recorded() {
        let state = AppState::new();
        let area = Rect::new(0, 0, 80, 1);
        let header = rows_of(area, |buf| {
            render_header(&state, area, buf, Focus::Viewport)
        })[0]
            .clone();
        assert!(!header.contains("sess:"), "{header:?}");
        assert!(header.contains("focus:viewport"), "{header:?}");
    }

    #[test]
    fn header_hides_the_directory_until_one_is_recorded() {
        let state = AppState::new();
        let area = Rect::new(0, 0, 80, 1);
        let header = rows_of(area, |buf| {
            render_header(&state, area, buf, Focus::Viewport)
        })[0]
            .clone();
        assert!(!header.contains("dir:"), "{header:?}");
        assert!(header.contains("focus:viewport"), "{header:?}");
    }

    #[test]
    fn header_shows_the_session_label_before_the_directory() {
        let mut state = AppState::new();
        state.set_session_label("s2");
        state.set_project_dir("/Volumes/work/proj");
        let area = Rect::new(0, 0, 120, 1);
        let header = rows_of(area, |buf| {
            render_header(&state, area, buf, Focus::Viewport)
        })[0]
            .clone();
        assert!(header.contains("sess:s2"), "{header:?}");
        assert!(
            header.find("sess:").expect("session") < header.find("dir:").expect("dir"),
            "the session slot precedes the directory: {header:?}"
        );
    }

    #[test]
    fn header_shows_the_recorded_directory_after_the_status_markers() {
        let mut state = AppState::new();
        state.set_project_dir("/Volumes/work/proj");
        let area = Rect::new(0, 0, 120, 1);
        let header = rows_of(area, |buf| {
            render_header(&state, area, buf, Focus::Viewport)
        })[0]
            .clone();
        assert!(header.contains("focus:viewport"), "{header:?}");
        assert!(header.contains("dir:/Volumes/work/proj"), "{header:?}");
        assert!(
            header.find("focus:viewport").expect("marker") < header.find("dir:").expect("dir"),
            "narrow frames clip the path before any status marker: {header:?}"
        );
    }

    #[test]
    fn home_prefix_folds_to_a_tilde_on_component_boundaries() {
        assert_eq!(
            abbreviate_home_with("/home/user/proj", Some("/home/user")),
            "~/proj"
        );
        assert_eq!(abbreviate_home_with("/home/user", Some("/home/user")), "~");
        assert_eq!(
            abbreviate_home_with("/home/user2/proj", Some("/home/user")),
            "/home/user2/proj",
            "string prefixes that are not components never fold"
        );
        assert_eq!(
            abbreviate_home_with("/home/user/proj", None),
            "/home/user/proj"
        );
        assert_eq!(
            abbreviate_home_with("/home/user/proj", Some("")),
            "/home/user/proj"
        );
    }

    #[test]
    fn composer_title_names_the_selected_model_and_provider() {
        let mut state = AppState::new();
        let area = Rect::new(0, 0, 80, 3);
        let mut bare = Buffer::empty(area);
        render_composer(&state, area, &mut bare, Focus::Composer);
        let rows = text_rows(&bare);
        assert!(rows[0].contains("composer (fixed)"), "{rows:?}");
        assert!(rows[0].contains("· build"), "the default mode is labeled");

        state.set_active_model(Some("demo".to_owned()), Some("acme".to_owned()));
        let mut named = Buffer::empty(area);
        render_composer(&state, area, &mut named, Focus::Composer);
        let rows = text_rows(&named);
        assert!(rows[0].contains("composer (fixed)"), "{rows:?}");
        assert!(rows[0].contains("demo"), "{rows:?}");
        assert!(rows[0].contains("acme"), "{rows:?}");

        state.set_active_model(Some("demo".to_owned()), None);
        let mut model_only = Buffer::empty(area);
        render_composer(&state, area, &mut model_only, Focus::Composer);
        let rows = text_rows(&model_only);
        assert!(rows[0].contains("demo"), "{rows:?}");
    }

    #[test]
    fn composer_border_names_the_mode_and_marks_read_only() {
        use ratatui::style::Color;

        let mut state = AppState::new();
        let area = Rect::new(0, 0, 40, 3);
        // Build is the default: green border, labeled title.
        let mut build = Buffer::empty(area);
        render_composer(&state, area, &mut build, Focus::Composer);
        assert_eq!(build.get(0, 0).fg, Color::Green);
        assert!(text_rows(&build)[0].contains("· build"));

        // Plan is read-only: amber border, labeled title.
        assert_eq!(state.cycle_mode(), "plan");
        let mut plan = Buffer::empty(area);
        render_composer(&state, area, &mut plan, Focus::Composer);
        assert_eq!(plan.get(0, 0).fg, Color::Yellow);
        assert!(text_rows(&plan)[0].contains("· plan"));

        // Unfocused keeps the plain border whatever the mode.
        let mut parked = Buffer::empty(area);
        render_composer(&state, area, &mut parked, Focus::Viewport);
        assert_eq!(parked.get(0, 0).fg, Color::Reset);
    }

    #[test]
    fn footer_shows_observed_usage_and_never_zeroes_unknown() {
        use nexus_core::{EventPayload, RunEvent, RunId, SessionId, Usage, UsageFinality};
        let mut state = AppState::new();
        let area = Rect::new(0, 0, 200, 1);
        let plain = rows_of(area, |buf| {
            render_footer(&state, area, buf, Focus::Viewport)
        })[0]
            .clone();
        assert!(!plain.contains("ctx"), "{plain:?}");

        assert!(state.apply_event(&started(0)));
        let usage = |seq: u64, input: Option<u64>, output: Option<u64>| {
            RunEvent::new(
                SessionId::new("sess-1").expect("valid"),
                RunId::new("run-1").expect("valid"),
                seq,
                EventPayload::UsageUpdated(Usage::new(input, output, UsageFinality::Final)),
            )
        };
        assert!(state.apply_event(&usage(1, Some(10), None)));
        let shown = rows_of(area, |buf| {
            render_footer(&state, area, buf, Focus::Viewport)
        })[0]
            .clone();
        assert!(shown.contains("ctx in:10 out:?"), "{shown:?}");
        assert!(state.apply_event(&usage(2, Some(10), Some(3))));
        let shown = rows_of(area, |buf| {
            render_footer(&state, area, buf, Focus::Viewport)
        })[0]
            .clone();
        assert!(shown.contains("ctx in:10 out:3"), "{shown:?}");
    }
}
