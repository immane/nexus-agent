//! Markdown rendering for assistant bodies (presentation only).
//!
//! Assistant text arrives as sanitized plain lines and is parsed here into
//! width-neutral styled rows: every [`RichLine`] carries its visible text
//! plus style runs, so wrapping and height measurement keep counting plain
//! characters exactly as before. Styles never add width and never alter the
//! stored transcript; folding, retention, and scrolling are unaffected.
//!
//! Deliberate subset, not full CommonMark:
//! - Inline: emphasis, strong, strikethrough, inline code, links (the URL is
//!   appended dimmed so no information is lost), images (alt text plus URL),
//!   task-list markers, footnote references (`[^1]`, definitions are
//!   collected and appended after the body), inline/display math (shown
//!   literally, code-styled).
//! - Blocks: headings (markers stripped, styled instead; `{#id}` attributes
//!   are consumed, never shown), fenced and indented code blocks,
//!   bulleted/ordered/nested lists, definition lists, block quotes (styled,
//!   no prefix; GFM admonitions like `[!NOTE]` keep a bold label),
//!   thematic breaks (full-width rule), hard breaks, and pipe tables with
//!   alignment, padding, and cell wrapping.
//! - Not enabled, on purpose: smart punctuation (rewrites the author's
//!   text), metadata blocks (would change `---` handling), superscript and
//!   subscript (no terminal representation; the markers stay readable
//!   literally), and wikilinks (need a URL-resolver design first).
//! - Tables are laid out, not faked: column widths derive from cell
//!   content, columns shrink widest-first under the available width, and
//!   long cells wrap with alignment preserved.
//! - Raw HTML has no terminal representation and is shown literally
//!   (code-styled) rather than dropped, so model output is never silently
//!   lost. The sanitizer has already neutralized control sequences.
//! - An unclosed fence renders as plain text: while streaming, the closing
//!   fence has not arrived yet, and a lone opener must not restyle the tail
//!   of the message. Once the fence closes the block renders as code.

use std::borrow::Cow;

use pulldown_cmark::{Alignment, BlockQuoteKind, Event, Options, Parser, Tag, TagEnd};

/// Width-neutral style bits for one styled run. Styles ride alongside the
/// visible text and must never affect wrapping or height measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MdStyle(u16);

impl MdStyle {
    /// No styling; runs with this style are never materialized.
    pub const PLAIN: Self = Self(0);
    /// `**strong**`.
    pub const BOLD: Self = Self(1 << 0);
    /// `*emphasis*`.
    pub const ITALIC: Self = Self(1 << 1);
    /// `~~struck~~`.
    pub const STRIKE: Self = Self(1 << 2);
    /// `` `code` `` and fenced/indented code blocks.
    pub const CODE: Self = Self(1 << 3);
    /// `# Heading` (markers stripped, style applied instead).
    pub const HEADING: Self = Self(1 << 4);
    /// `> quote` (styled, no prefix added).
    pub const QUOTE: Self = Self(1 << 5);
    /// `[text](url)` link text.
    pub const LINK: Self = Self(1 << 6);
    /// The appended ` <url>` or ` (image: url)` suffix.
    pub const LINK_URL: Self = Self(1 << 7);
    /// Drag-selected text. A width-neutral overlay bit: renderers union it
    /// onto existing runs and map it to reversed video.
    pub const SELECTED: Self = Self(1 << 8);

    /// True when no style bit is set.
    #[must_use]
    pub fn is_plain(self) -> bool {
        self.0 == 0
    }

    /// True when `flag` is set.
    #[must_use]
    pub fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 != 0
    }

    /// Union of two styles (nested constructs combine).
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// One styled run inside a rendered line. Offsets are character (not byte)
/// offsets in line coordinates, so slicing never splits UTF-8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StyledRun {
    /// First character covered by the run.
    pub start: usize,
    /// Character count covered by the run.
    pub len: usize,
    /// Style applied to the run.
    pub style: MdStyle,
}

/// One span of text sharing a single style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RichSpan {
    /// Visible text (no style escapes embedded).
    pub text: String,
    /// Style for the whole span.
    pub style: MdStyle,
}

/// One logical (pre-wrap) line: visible text plus its style runs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RichLine {
    /// Spans in order; concatenation is the visible text.
    pub spans: Vec<RichSpan>,
}

impl RichLine {
    /// Visible text of the line (styles contribute no characters).
    #[must_use]
    pub fn visible_text(&self) -> String {
        let len: usize = self.spans.iter().map(|span| span.text.len()).sum();
        let mut out = String::with_capacity(len);
        for span in &self.spans {
            out.push_str(&span.text);
        }
        out
    }

    /// Visible character count (the only width that matters).
    #[must_use]
    pub fn visible_len(&self) -> usize {
        self.spans
            .iter()
            .map(|span| span.text.chars().count())
            .sum()
    }

    /// Style runs in line coordinates, skipping unstyled gaps. Runs are
    /// non-overlapping and ordered.
    #[must_use]
    pub fn runs(&self) -> Vec<StyledRun> {
        let mut runs = Vec::new();
        let mut start = 0usize;
        for span in &self.spans {
            let len = span.text.chars().count();
            if !span.style.is_plain() && len > 0 {
                runs.push(StyledRun {
                    start,
                    len,
                    style: span.style,
                });
            }
            start += len;
        }
        runs
    }
}

/// Clips style runs to the character window `[start, end)` of one logical
/// line and rebases them to window coordinates, for slicing one wrapped
/// chunk out of a longer line. Runs outside the window are dropped.
#[must_use]
pub fn slice_runs(runs: &[StyledRun], start: usize, end: usize) -> Vec<StyledRun> {
    runs.iter()
        .filter_map(|run| {
            let from = run.start.max(start);
            let to = (run.start + run.len).min(end);
            if from < to {
                Some(StyledRun {
                    start: from - start,
                    len: to - from,
                    style: run.style,
                })
            } else {
                None
            }
        })
        .collect()
}

/// Nesting context for one open list.
struct ListContext {
    ordered: bool,
    next: u64,
    depth: usize,
}

/// One table under construction: alignments from the opening tag, finished
/// rows split into header and body, and the cell currently collecting
/// inline content.
struct TableBuilder {
    alignments: Vec<Alignment>,
    head_rows: Vec<Vec<Vec<RichSpan>>>,
    body_rows: Vec<Vec<Vec<RichSpan>>>,
    in_head: bool,
    current_row: Vec<Vec<RichSpan>>,
    current_cell: Vec<RichSpan>,
    cell_open: bool,
}

impl TableBuilder {
    fn new(alignments: Vec<Alignment>) -> Self {
        Self {
            alignments,
            head_rows: Vec::new(),
            body_rows: Vec::new(),
            in_head: false,
            current_row: Vec::new(),
            current_cell: Vec::new(),
            cell_open: false,
        }
    }

    /// Closes the open cell, if any, into the current row.
    fn close_cell(&mut self) {
        if self.cell_open {
            self.cell_open = false;
            self.current_row
                .push(std::mem::take(&mut self.current_cell));
        }
    }

    /// Closes the current row into the header or body section.
    fn close_row(&mut self) {
        self.close_cell();
        if self.current_row.iter().any(|cell| !cell.is_empty()) || !self.current_row.is_empty() {
            let row = std::mem::take(&mut self.current_row);
            if self.in_head {
                self.head_rows.push(row);
            } else {
                self.body_rows.push(row);
            }
        }
    }
}

/// Visible cell width of one cell (CJK glyphs need two cells).
fn cell_len(cell: &[RichSpan]) -> usize {
    cell.iter()
        .map(|span| crate::state::display_width(&span.text))
        .sum()
}

/// Splits text into at most `width`-cell slices on character boundaries,
/// mirroring the wrapping measurement so layout math agrees with rendering.
/// Wide glyphs are never split; a lone glyph wider than `width` still
/// advances one glyph.
fn char_chunks(text: &str, width: usize) -> Vec<&str> {
    let width = width.max(1);
    let char_cells =
        |char: char| -> usize { unicode_width::UnicodeWidthChar::width(char).unwrap_or(1) };
    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut used = 0usize;
    for (offset, char) in text.char_indices() {
        let wide = char_cells(char);
        if used > 0 && used + wide > width {
            chunks.push(&text[start..offset]);
            start = offset;
            used = 0;
        }
        used += wide;
    }
    if start < text.len() || text.is_empty() {
        chunks.push(&text[start..]);
    }
    chunks
}

/// Pads text to exactly `width` terminal cells per the column alignment.
fn pad_cell(text: &str, width: usize, align: &Alignment) -> String {
    let len = crate::state::display_width(text);
    if len >= width {
        return text.to_owned();
    }
    let pad = width - len;
    match align {
        Alignment::Right => format!("{}{text}", " ".repeat(pad)),
        Alignment::Center => {
            let left = pad / 2;
            format!("{}{text}{}", " ".repeat(left), " ".repeat(pad - left))
        }
        Alignment::Left | Alignment::None => format!("{text}{}", " ".repeat(pad)),
    }
}

/// Rebuilds spans from one wrapped chunk and its sliced runs (character
/// offsets in chunk coordinates).
fn spans_from_chunk(chunk: &str, runs: &[StyledRun]) -> Vec<RichSpan> {
    let bytes: Vec<usize> = chunk
        .char_indices()
        .map(|(offset, _)| offset)
        .chain(std::iter::once(chunk.len()))
        .collect();
    let byte_at = |index: usize| bytes.get(index).copied().unwrap_or(chunk.len());
    let mut spans = Vec::new();
    let mut cursor = 0usize;
    for run in runs {
        let from = byte_at(run.start);
        let to = byte_at(run.start + run.len);
        if from > byte_at(cursor) {
            spans.push(RichSpan {
                text: chunk[byte_at(cursor)..from].to_owned(),
                style: MdStyle::PLAIN,
            });
        }
        if from < to {
            spans.push(RichSpan {
                text: chunk[from..to].to_owned(),
                style: run.style,
            });
        }
        cursor = cursor.max(run.start + run.len);
    }
    if cursor < chunk.chars().count() {
        spans.push(RichSpan {
            text: chunk[byte_at(cursor)..].to_owned(),
            style: MdStyle::PLAIN,
        });
    }
    spans
}

/// Wraps one cell into `(text, runs)` visual lines at `width` characters.
fn wrap_cell(cell: &[RichSpan], width: usize) -> Vec<(String, Vec<StyledRun>)> {
    let line = RichLine {
        spans: cell.to_vec(),
    };
    let text = line.visible_text();
    if text.is_empty() {
        return vec![(String::new(), Vec::new())];
    }
    let runs = line.runs();
    let mut out = Vec::new();
    let mut start = 0usize;
    for chunk in char_chunks(&text, width) {
        let end = start + chunk.chars().count();
        out.push((chunk.to_owned(), slice_runs(&runs, start, end)));
        start = end;
    }
    out
}

/// Lays out a finished table at `width` cells: content-derived column
/// widths (shrunk widest-first when over budget), a boxed header with a
/// rule beneath it, and wrapped cells. Every emitted line fits `width`, so
/// downstream wrapping is the identity. Box-drawing glyphs are single-cell
/// and alignment still shows in the padding (markers have no room in the
/// box style, so the colons are dropped).
fn layout_table(builder: &TableBuilder, width: usize) -> Vec<RichLine> {
    let width = width.max(1);
    let ncols = builder
        .head_rows
        .iter()
        .chain(&builder.body_rows)
        .map(Vec::len)
        .max()
        .unwrap_or(0);
    if ncols == 0 {
        return Vec::new();
    }
    let mut aligns = builder.alignments.clone();
    aligns.resize(ncols, Alignment::None);
    let mut natural = vec![0usize; ncols];
    for row in builder.head_rows.iter().chain(&builder.body_rows) {
        for (index, cell) in row.iter().enumerate().take(ncols) {
            natural[index] = natural[index].max(cell_len(cell));
        }
    }
    // One two-cell border per column edge (`│ ` opener, ` │ ` gaps,
    // ` │` closer): same footprint as the pipe style it replaces.
    let avail = width.saturating_sub(3 * ncols + 1);
    let mut widths: Vec<usize> = natural.iter().map(|w| (*w).max(1)).collect();
    while widths.iter().sum::<usize>() > avail {
        match widths
            .iter()
            .enumerate()
            .filter(|(_, w)| **w > 1)
            .max_by_key(|(_, w)| **w)
        {
            Some((pos, _)) => widths[pos] -= 1,
            None => break,
        }
    }
    // Without a header section the first body row heads the table so the
    // separator still has something to underline.
    let mut head = builder.head_rows.clone();
    let mut body = builder.body_rows.clone();
    if head.is_empty() && !body.is_empty() {
        head.push(body.remove(0));
    }
    let mut out = Vec::new();
    for (row_index, row) in head.iter().chain(&body).enumerate() {
        let is_header = row_index < head.len();
        let mut cells: Vec<Vec<(String, Vec<StyledRun>)>> = Vec::with_capacity(ncols);
        for (col, width) in widths.iter().enumerate().take(ncols) {
            let cell = row.get(col).map(Vec::as_slice).unwrap_or(&[]);
            let mut wrapped = wrap_cell(cell, *width);
            if is_header {
                for (_, runs) in wrapped.iter_mut() {
                    for run in runs.iter_mut() {
                        run.style = run.style.union(MdStyle::BOLD);
                    }
                }
                if wrapped.iter().all(|(_, runs)| runs.is_empty()) {
                    // A header with no inline styles still reads as one.
                    let len = wrapped[0].0.chars().count();
                    wrapped[0].1.push(StyledRun {
                        start: 0,
                        len,
                        style: MdStyle::BOLD,
                    });
                }
            }
            cells.push(wrapped);
        }
        if is_header && row_index == 0 {
            out.push(box_rule(&widths, ("\u{256d}", "\u{252c}", "\u{256e}")));
        }
        let height = cells.iter().map(Vec::len).max().unwrap_or(1);
        for visual in 0..height {
            let mut spans = vec![RichSpan {
                text: "\u{2502} ".to_owned(),
                style: MdStyle::PLAIN,
            }];
            for (col, wrapped) in cells.iter().enumerate() {
                let (text, runs) = wrapped
                    .get(visual)
                    .map(|(text, runs)| (text.as_str(), runs.as_slice()))
                    .unwrap_or(("", &[]));
                let padded = pad_cell(text, widths[col], &aligns[col]);
                let mut cell_spans = spans_from_chunk(&padded, runs);
                if cell_spans.is_empty() {
                    cell_spans.push(RichSpan {
                        text: padded,
                        style: MdStyle::PLAIN,
                    });
                }
                spans.extend(cell_spans);
                spans.push(RichSpan {
                    text: if col + 1 == ncols {
                        " \u{2502}".to_owned()
                    } else {
                        " \u{2502} ".to_owned()
                    },
                    style: MdStyle::PLAIN,
                });
            }
            out.push(RichLine { spans });
        }
        if is_header && row_index + 1 == head.len() {
            out.push(box_rule(&widths, ("\u{251c}", "\u{253c}", "\u{2524}")));
        }
    }
    // An empty table (no rows at all) renders nothing, not a lone rule.
    if !out.is_empty() {
        out.push(box_rule(&widths, ("\u{2570}", "\u{2534}", "\u{256f}")));
    }
    out
}

/// Draws one horizontal box rule (`top`, `mid`, or `bottom` corner sets):
/// each column gets its width plus one padding cell per side.
fn box_rule(widths: &[usize], corners: (&str, &str, &str)) -> RichLine {
    let mut text = String::from(corners.0);
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            text.push_str(corners.1);
        }
        text.push_str(&"\u{2500}".repeat(width + 2));
    }
    text.push_str(corners.2);
    RichLine {
        spans: vec![RichSpan {
            text,
            style: MdStyle::PLAIN,
        }],
    }
}

/// Converts pulldown-cmark events into logical lines.
struct BodyParser {
    lines: Vec<RichLine>,
    current: RichLine,
    styles: Vec<MdStyle>,
    lists: Vec<ListContext>,
    /// Bullet/number prefix waiting for the first text of a list item.
    pending_prefix: Option<String>,
    /// Inside a fenced/indented code block.
    in_code_block: bool,
    /// Inside an image; tracks whether any alt text arrived.
    image_alt_seen: bool,
    /// Link destinations for open `[text](url)` constructs.
    link_dests: Vec<String>,
    /// Table under construction, while table events are open. Inline
    /// content routes into the open cell instead of the current line.
    table: Option<TableBuilder>,
    /// Finished footnote definitions in document order, appended after
    /// the body by [`BodyParser::finish`].
    footnotes: Vec<(String, Vec<RichLine>)>,
    /// Footnote definition currently collecting: its name plus a nested
    /// parser that accepts any block content (including tables).
    divert: Option<(String, Box<BodyParser>)>,
    /// Nesting depth of block containers (quotes, lists, items, tables,
    /// code, definitions). Only depth-0 boundaries separate top-level
    /// blocks, so nested content stays tight.
    block_depth: usize,
    /// A finished top-level block wants air before the next one starts.
    need_gap: bool,
}

impl BodyParser {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            current: RichLine::default(),
            styles: Vec::new(),
            lists: Vec::new(),
            pending_prefix: None,
            in_code_block: false,
            image_alt_seen: false,
            link_dests: Vec::new(),
            table: None,
            footnotes: Vec::new(),
            divert: None,
            block_depth: 0,
            need_gap: false,
        }
    }

    /// Starts one blank separator row before a new top-level block, unless
    /// this is the first block. Consumes the pending flag either way, so a
    /// lone opener never accrues air it cannot spend.
    fn gap_maybe(&mut self) {
        if self.block_depth == 0 && self.need_gap && !self.lines.is_empty() {
            self.push_line(RichLine::default());
        }
        if self.block_depth == 0 {
            self.need_gap = false;
        }
    }

    /// Enters one block container (depth-gated gaps apply inside never).
    fn enter_block(&mut self) {
        self.gap_maybe();
        self.block_depth += 1;
    }

    /// Leaves one block container; a return to the top level arms the gap
    /// for whatever block comes next.
    fn exit_block(&mut self) {
        self.block_depth = self.block_depth.saturating_sub(1);
        if self.block_depth == 0 {
            self.need_gap = true;
        }
    }

    /// True while a table cell is collecting inline content.
    fn cell_open(&self) -> bool {
        matches!(&self.table, Some(builder) if builder.cell_open)
    }

    /// Last span of the active sink: the open table cell, else the
    /// current line.
    fn last_span_mut(&mut self) -> Option<&mut RichSpan> {
        match &mut self.table {
            Some(builder) if builder.cell_open => builder.current_cell.last_mut(),
            _ => self.current.spans.last_mut(),
        }
    }

    /// Pushes one span into the active sink.
    fn push_span(&mut self, span: RichSpan) {
        match &mut self.table {
            Some(builder) if builder.cell_open => builder.current_cell.push(span),
            _ => self.current.spans.push(span),
        }
    }

    /// OR of every active style frame (nested constructs combine).
    fn combined(&self) -> MdStyle {
        self.styles
            .iter()
            .fold(MdStyle::PLAIN, |acc, style| acc.union(*style))
    }

    /// Pushes text with the combined style, merging into the previous span
    /// when the style matches so runs stay compact. Inside a table cell
    /// newlines become spaces: a cell is one logical unit and never splits
    /// the row layout.
    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.image_alt_seen {
            self.image_alt_seen = false;
        }
        let style = self.combined();
        if self.cell_open() {
            let text = text.replace('\n', " ");
            if let Some(last) = self.last_span_mut()
                && last.style == style
            {
                last.text.push_str(&text);
                return;
            }
            self.push_span(RichSpan { text, style });
            return;
        }
        if let Some(last) = self.last_span_mut()
            && last.style == style
        {
            last.text.push_str(text);
            return;
        }
        self.push_span(RichSpan {
            text: text.to_owned(),
            style,
        });
    }

    /// Emits the pending list-item prefix (bullet/number plus nesting
    /// indent) before the first text of the item.
    fn flush_prefix(&mut self) {
        let Some(prefix) = self.pending_prefix.take() else {
            return;
        };
        if let Some(last) = self.current.spans.last_mut()
            && last.style.is_plain()
        {
            last.text.insert_str(0, &prefix);
        } else {
            self.current.spans.insert(
                0,
                RichSpan {
                    text: prefix,
                    style: MdStyle::PLAIN,
                },
            );
        }
    }

    /// Finishes the current line when it holds text; empty lines are only
    /// emitted on demand (blank code rows, hard breaks).
    fn end_line(&mut self) {
        self.flush_prefix();
        if self.current.spans.iter().any(|span| !span.text.is_empty()) {
            let finished = std::mem::take(&mut self.current);
            self.lines.push(finished);
        }
    }

    /// Finishes the current line even when it is empty.
    fn end_line_force(&mut self) {
        self.flush_prefix();
        let finished = std::mem::take(&mut self.current);
        self.lines.push(finished);
    }

    fn push_line(&mut self, line: RichLine) {
        self.lines.push(line);
    }
    /// Routes one event, diverting footnote-definition content into a
    /// nested parser so definitions accept any block structure.
    fn feed(&mut self, event: Event, layout_width: usize) {
        if let Some((name, mut sub)) = self.divert.take() {
            if matches!(event, Event::End(TagEnd::FootnoteDefinition)) {
                let (deflines, nested) = sub.finish_inner();
                self.footnotes.push((name, deflines));
                self.footnotes.extend(nested);
            } else {
                sub.feed(event, layout_width);
                self.divert = Some((name, sub));
            }
            return;
        }
        self.handle(event, layout_width);
    }

    /// Drains a nested footnote-definition parser into its lines plus any
    /// definitions nested inside it.
    fn finish_inner(mut self) -> (Vec<RichLine>, Vec<(String, Vec<RichLine>)>) {
        self.end_line();
        (self.lines, self.footnotes)
    }

    /// Finishes the body: flushes the last line, then appends collected
    /// footnote definitions (`[^name]: ...` with aligned continuations).
    fn finish(mut self) -> Vec<RichLine> {
        self.end_line();
        let footnotes = std::mem::take(&mut self.footnotes);
        for (name, deflines) in footnotes {
            let prefix = format!("[^{name}]: ");
            let mut first = deflines.into_iter();
            match first.next() {
                Some(line) => {
                    let mut head = RichLine {
                        spans: vec![RichSpan {
                            text: prefix.clone(),
                            style: MdStyle::PLAIN,
                        }],
                    };
                    head.spans.extend(line.spans);
                    self.push_line(head);
                    let indent = " ".repeat(prefix.chars().count());
                    for line in first {
                        let mut cont = RichLine {
                            spans: vec![RichSpan {
                                text: indent.clone(),
                                style: MdStyle::PLAIN,
                            }],
                        };
                        cont.spans.extend(line.spans);
                        self.push_line(cont);
                    }
                }
                None => self.push_line(RichLine {
                    spans: vec![RichSpan {
                        text: prefix.trim_end().to_owned(),
                        style: MdStyle::PLAIN,
                    }],
                }),
            }
        }
        self.lines
    }

    /// Handles one event against the current parser state.
    fn handle(&mut self, event: Event, layout_width: usize) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    // Table cells carry inline content only: paragraph
                    // boundaries inside a cell are ignored, never rows.
                    if !self.cell_open() {
                        self.end_line();
                        self.enter_block();
                    }
                }
                Tag::Heading { .. } => {
                    self.end_line();
                    self.enter_block();
                    self.styles.push(MdStyle::HEADING.union(MdStyle::BOLD));
                }
                Tag::BlockQuote(kind) => {
                    self.end_line();
                    self.enter_block();
                    // GFM admonitions (`> [!NOTE] ...`) keep their marker as
                    // a bold label; plain quotes stay style-only.
                    if let Some(kind) = kind {
                        let label = match kind {
                            BlockQuoteKind::Note => "[!NOTE]",
                            BlockQuoteKind::Tip => "[!TIP]",
                            BlockQuoteKind::Important => "[!IMPORTANT]",
                            BlockQuoteKind::Warning => "[!WARNING]",
                            BlockQuoteKind::Caution => "[!CAUTION]",
                        };
                        self.push_line(RichLine {
                            spans: vec![RichSpan {
                                text: label.to_owned(),
                                style: MdStyle::BOLD,
                            }],
                        });
                    }
                    self.styles.push(MdStyle::QUOTE);
                }
                Tag::CodeBlock(_) => {
                    self.end_line();
                    self.enter_block();
                    self.in_code_block = true;
                }
                Tag::List(first) => {
                    self.end_line();
                    self.enter_block();
                    let depth = self.lists.len();
                    self.lists.push(ListContext {
                        ordered: first.is_some(),
                        next: first.unwrap_or(1),
                        depth,
                    });
                }
                Tag::Item => {
                    self.end_line();
                    self.enter_block();
                    let prefix = match self.lists.last_mut() {
                        Some(list) if list.ordered => {
                            let number = list.next;
                            list.next += 1;
                            format!("{}{number}. ", "  ".repeat(list.depth))
                        }
                        Some(list) => format!("{}• ", "  ".repeat(list.depth)),
                        None => "• ".to_owned(),
                    };
                    self.pending_prefix = Some(prefix);
                }
                Tag::Emphasis => self.styles.push(MdStyle::ITALIC),
                Tag::Strong => self.styles.push(MdStyle::BOLD),
                Tag::Strikethrough => self.styles.push(MdStyle::STRIKE),
                Tag::Table(alignments) => {
                    self.end_line();
                    self.enter_block();
                    self.table = Some(TableBuilder::new(alignments));
                }
                Tag::TableHead => {
                    self.enter_block();
                    if let Some(builder) = self.table.as_mut() {
                        builder.in_head = true;
                    }
                }
                Tag::TableRow => {
                    self.enter_block();
                    if let Some(builder) = self.table.as_mut() {
                        builder.close_cell();
                        builder.current_row = Vec::new();
                    }
                }
                Tag::TableCell => {
                    self.enter_block();
                    if let Some(builder) = self.table.as_mut() {
                        builder.close_cell();
                        builder.current_cell = Vec::new();
                        builder.cell_open = true;
                    }
                }
                Tag::FootnoteDefinition(name) => {
                    // No depth change: the matching End never reaches
                    // `handle` (the feeder intercepts it), and nesting
                    // lives in the diverted sub-parser.
                    self.end_line();
                    self.gap_maybe();
                    self.divert = Some((name.into_string(), Box::new(BodyParser::new())));
                }
                Tag::DefinitionList => {
                    self.end_line();
                    self.enter_block();
                }
                Tag::DefinitionListTitle => {
                    self.end_line();
                    self.enter_block();
                }
                Tag::DefinitionListDefinition => {
                    self.end_line();
                    self.enter_block();
                    self.pending_prefix = Some("  ".to_owned());
                }
                Tag::Link { dest_url, .. } => {
                    self.styles.push(MdStyle::LINK);
                    self.link_dests.push(dest_url.into_string());
                }
                Tag::Image { dest_url, .. } => {
                    self.styles.push(MdStyle::LINK);
                    self.image_alt_seen = true;
                    self.link_dests.push(format!("image: {}", dest_url));
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    if !self.cell_open() {
                        self.end_line();
                        self.exit_block();
                    }
                }
                TagEnd::Heading(..)
                | TagEnd::BlockQuote(..)
                | TagEnd::CodeBlock
                | TagEnd::Item
                | TagEnd::List(_)
                | TagEnd::DefinitionList
                | TagEnd::DefinitionListTitle
                | TagEnd::DefinitionListDefinition => {
                    self.end_line();
                    self.exit_block();
                    match tag {
                        TagEnd::Heading(..) | TagEnd::BlockQuote(..) => {
                            self.styles.pop();
                        }
                        TagEnd::CodeBlock => self.in_code_block = false,
                        TagEnd::List(_) => {
                            self.lists.pop();
                        }
                        _ => {}
                    }
                }
                TagEnd::Table => {
                    if let Some(builder) = self.table.take() {
                        for line in layout_table(&builder, layout_width) {
                            self.push_line(line);
                        }
                    }
                    self.exit_block();
                }
                TagEnd::TableHead => {
                    if let Some(builder) = self.table.as_mut() {
                        // The header holds bare cells (no row wrapper), so
                        // the row closes here, still flagged as header.
                        builder.close_row();
                        builder.in_head = false;
                    }
                    self.exit_block();
                }
                TagEnd::TableRow => {
                    if let Some(builder) = self.table.as_mut() {
                        builder.close_row();
                    }
                    self.exit_block();
                }
                TagEnd::TableCell => {
                    if let Some(builder) = self.table.as_mut() {
                        builder.close_cell();
                    }
                    self.exit_block();
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    self.styles.pop();
                }
                TagEnd::Link => {
                    self.styles.pop();
                    if let Some(dest) = self.link_dests.pop() {
                        // Direct push (never merged): merging into a matching
                        // outer span would restyle the link text itself.
                        self.flush_prefix();
                        self.push_span(RichSpan {
                            text: format!(" <{dest}>"),
                            style: MdStyle::LINK_URL,
                        });
                    }
                }
                TagEnd::Image => {
                    self.styles.pop();
                    if self.image_alt_seen {
                        self.image_alt_seen = false;
                        self.flush_prefix();
                        self.push_span(RichSpan {
                            text: "image".to_owned(),
                            style: MdStyle::LINK,
                        });
                    }
                    if let Some(dest) = self.link_dests.pop() {
                        self.flush_prefix();
                        self.push_span(RichSpan {
                            text: format!(" ({dest})"),
                            style: MdStyle::LINK_URL,
                        });
                    }
                }
                _ => {}
            },
            Event::Text(text) => {
                if self.in_code_block {
                    let mut segments = text.split('\n').peekable();
                    while let Some(segment) = segments.next() {
                        if !segment.is_empty() {
                            let style = self.combined().union(MdStyle::CODE);
                            let merged = match self.last_span_mut() {
                                Some(last) if last.style == style => {
                                    last.text.push_str(segment);
                                    true
                                }
                                _ => false,
                            };
                            if !merged {
                                self.push_span(RichSpan {
                                    text: segment.to_owned(),
                                    style,
                                });
                            }
                        }
                        if segments.peek().is_some() {
                            self.end_line_force();
                        }
                    }
                } else {
                    self.flush_prefix();
                    self.push_text(&text);
                }
            }
            Event::Code(code) => {
                self.flush_prefix();
                let style = self.combined().union(MdStyle::CODE);
                let merged = match self.last_span_mut() {
                    Some(last) if last.style == style => {
                        last.text.push_str(&code);
                        true
                    }
                    _ => false,
                };
                if !merged {
                    self.push_span(RichSpan {
                        text: code.into_string(),
                        style,
                    });
                }
            }
            Event::SoftBreak => {
                if self.cell_open() {
                    // Inside a table cell a source row stays a space: cells
                    // never split the row layout.
                    self.push_text(" ");
                } else {
                    // Deliberate deviation from HTML rendering (where a soft
                    // break is a space): a source row stays a row, so streamed
                    // plain-text lines never merge into one wrapped line.
                    self.end_line();
                }
            }
            Event::HardBreak => {
                if self.cell_open() {
                    self.push_text(" ");
                } else {
                    self.end_line_force();
                }
            }
            Event::Rule => {
                self.end_line();
                self.gap_maybe();
                self.need_gap = true;
                let width = layout_width.max(1);
                self.push_line(RichLine {
                    spans: vec![RichSpan {
                        text: "─".repeat(width),
                        style: MdStyle::PLAIN,
                    }],
                });
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                match self.pending_prefix.take() {
                    Some(prefix) => {
                        self.pending_prefix = Some(format!("{prefix}{marker}"));
                    }
                    None => self.push_text(marker),
                }
            }
            Event::FootnoteReference(name) => {
                self.flush_prefix();
                self.push_span(RichSpan {
                    text: format!("[^{name}]"),
                    style: MdStyle::LINK,
                });
            }
            Event::InlineMath(math) => {
                self.flush_prefix();
                let style = self.combined().union(MdStyle::CODE);
                self.push_span(RichSpan {
                    text: format!("${math}$"),
                    style,
                });
            }
            Event::DisplayMath(math) => {
                // Display math stands alone; multiline content keeps one
                // row per source line, all code-styled.
                self.end_line();
                self.gap_maybe();
                self.need_gap = true;
                let style = self.combined().union(MdStyle::CODE);
                if math.contains('\n') {
                    self.push_line(RichLine {
                        spans: vec![RichSpan {
                            text: "$$".to_owned(),
                            style,
                        }],
                    });
                    for segment in math.split('\n') {
                        if !segment.is_empty() {
                            self.push_line(RichLine {
                                spans: vec![RichSpan {
                                    text: segment.to_owned(),
                                    style,
                                }],
                            });
                        }
                    }
                    self.push_line(RichLine {
                        spans: vec![RichSpan {
                            text: "$$".to_owned(),
                            style,
                        }],
                    });
                } else {
                    self.push_line(RichLine {
                        spans: vec![RichSpan {
                            text: format!("$${math}$$"),
                            style,
                        }],
                    });
                }
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                // Raw HTML has no terminal representation: show it literally
                // (code-styled) instead of dropping model output.
                self.flush_prefix();
                for (index, segment) in html.split('\n').enumerate() {
                    if index > 0 {
                        if self.cell_open() {
                            self.push_text(" ");
                        } else {
                            self.end_line_force();
                        }
                    }
                    if !segment.is_empty() {
                        let style = self.combined().union(MdStyle::CODE);
                        let merged = match self.last_span_mut() {
                            Some(last) if last.style == style => {
                                last.text.push_str(segment);
                                true
                            }
                            _ => false,
                        };
                        if !merged {
                            self.push_span(RichSpan {
                                text: segment.to_owned(),
                                style,
                            });
                        }
                    }
                }
            }
        }
    }
}

/// Escapes a lone opening fence so an unclosed code block parses as plain
/// text. Only the unmatched opener is escaped; closed blocks pass through
/// untouched. Handles both backtick and tilde fences.
fn escape_unclosed_fence(source: &str) -> Cow<'_, str> {
    /// Splits a fence marker line: up to three leading spaces, then a run
    /// of at least three backticks or tildes. Returns the marker char, the
    /// run length, and the remainder of the line.
    fn fence_marker(line: &str) -> Option<(char, usize, &str)> {
        let trimmed = line.strip_prefix("   ").unwrap_or(line);
        let trimmed = trimmed.strip_prefix("  ").unwrap_or(trimmed);
        let trimmed = trimmed.strip_prefix(' ').unwrap_or(trimmed);
        let mut chars = trimmed.chars();
        let marker = chars.next()?;
        if marker != '`' && marker != '~' {
            return None;
        }
        let mut len = 1usize;
        let mut rest_start = marker.len_utf8();
        for char in chars {
            if char == marker {
                len += 1;
                rest_start += char.len_utf8();
            } else {
                break;
            }
        }
        if len < 3 {
            return None;
        }
        Some((marker, len, &trimmed[rest_start..]))
    }

    let mut open: Option<(char, usize, usize)> = None;
    for (index, line) in source.lines().enumerate() {
        let Some((marker, len, rest)) = fence_marker(line) else {
            continue;
        };
        match open {
            Some((open_marker, open_len, _)) if marker == open_marker && len >= open_len => {
                if rest.trim().is_empty() {
                    open = None;
                }
                // Otherwise the marker line is literal block content.
            }
            Some(_) => {
                // A different marker inside a block is literal content.
            }
            None => {
                // A backtick fence info string must not contain backticks.
                if marker == '`' && rest.contains('`') {
                    continue;
                }
                open = Some((marker, len, index));
            }
        }
    }
    let Some((_, _, opener)) = open else {
        return Cow::Borrowed(source);
    };
    let mut escaped = String::with_capacity(source.len() + 3);
    for (index, line) in source.lines().enumerate() {
        if index > 0 {
            escaped.push('\n');
        }
        if index == opener
            && let Some((marker, _, _)) = fence_marker(line)
        {
            let run: String = std::iter::repeat_n(marker, 3).collect();
            let escaped_run: String = run
                .chars()
                .map(|char| format!("\\{char}"))
                .collect::<Vec<_>>()
                .concat();
            escaped.push_str(&line.replacen(&run, &escaped_run, 1));
            continue;
        }
        escaped.push_str(line);
    }
    // Preserve a trailing newline: `lines` drops the final terminator and a
    // code block at end-of-input must not gain or lose its last empty row.
    if source.ends_with('\n') {
        escaped.push('\n');
    }
    Cow::Owned(escaped)
}

/// Parses assistant body lines into logical styled lines. `rule_width`
/// sizes the thematic-break rule and lays out tables so each fills exactly
/// its rows; those are the only width-dependent parts of the output.
#[must_use]
pub fn render_body(lines: &[String], rule_width: usize) -> Vec<RichLine> {
    if lines.iter().all(|line| line.is_empty()) {
        return Vec::new();
    }
    let source = lines.join("\n");
    if source.trim().is_empty() {
        return Vec::new();
    }
    let source = escape_unclosed_fence(&source);
    // Tables, footnotes, math, definition lists, heading attributes, and
    // GFM admonitions ride the same parser: no new dependency, only new
    // event arms below. Deliberately off: smart punctuation (rewrites the
    // author's text), metadata blocks (would change `---` handling),
    // superscript/subscript (no terminal form; markers stay literal), and
    // wikilinks (need a URL-resolver design first).
    let options = Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_MATH
        | Options::ENABLE_DEFINITION_LIST
        | Options::ENABLE_HEADING_ATTRIBUTES
        | Options::ENABLE_GFM;
    let mut parser = BodyParser::new();
    for event in Parser::new_ext(&source, options) {
        parser.feed(event, rule_width);
    }
    parser.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn visible(lines: &[RichLine]) -> Vec<String> {
        lines.iter().map(RichLine::visible_text).collect()
    }

    pub(super) fn body(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    #[test]
    fn plain_text_passes_through_unchanged() {
        let lines = render_body(&body("just a line\nsecond line"), 78);
        assert_eq!(visible(&lines), vec!["just a line", "second line"]);
        assert!(lines.iter().all(|line| line.runs().is_empty()));
    }

    #[test]
    fn single_newlines_stay_line_breaks_instead_of_merging() {
        // Soft breaks are rows, not spaces: streamed source lines never
        // collapse into one line the way HTML rendering would join them.
        let lines = render_body(&body("foo\nbar\nbaz"), 78);
        assert_eq!(visible(&lines), vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn inline_styles_combine_without_adding_width() {
        let lines = render_body(&body("a **bold** and *em* and `code` end"), 78);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].visible_text(), "a bold and em and code end");
        let runs = lines[0].runs();
        assert_eq!(runs.len(), 3);
        assert!(runs[0].style.contains(MdStyle::BOLD));
        assert!(runs[1].style.contains(MdStyle::ITALIC));
        assert!(runs[2].style.contains(MdStyle::CODE));
        assert_eq!(
            lines[0].visible_len(),
            "a bold and em and code end".chars().count()
        );
    }

    #[test]
    fn strikethrough_needs_no_extra_width() {
        let lines = render_body(&body("a ~~gone~~ b"), 78);
        assert_eq!(visible(&lines), vec!["a gone b"]);
        assert!(lines[0].runs()[0].style.contains(MdStyle::STRIKE));
    }

    #[test]
    fn heading_markers_are_stripped_and_styled() {
        let lines = render_body(&body("## Title here"), 78);
        assert_eq!(visible(&lines), vec!["Title here"]);
        let runs = lines[0].runs();
        assert_eq!(runs.len(), 1);
        assert!(runs[0].style.contains(MdStyle::HEADING));
        assert!(runs[0].style.contains(MdStyle::BOLD));
    }

    #[test]
    fn closed_fence_renders_code_without_the_fence_lines() {
        let lines = render_body(&body("```rust\nlet x = 1;\n```"), 78);
        assert_eq!(visible(&lines), vec!["let x = 1;"]);
        assert!(lines[0].runs()[0].style.contains(MdStyle::CODE));
    }

    #[test]
    fn unclosed_fence_stays_plain_text() {
        let lines = render_body(&body("```rust\nlet x = 1;\nstill coming"), 78);
        let text = visible(&lines).join("\n");
        assert!(
            text.contains("```rust"),
            "the opener stays literal: {text:?}"
        );
        assert!(text.contains("let x = 1;"));
        assert!(
            lines.iter().all(|line| line.runs().is_empty()),
            "no code styling while the fence is open: {lines:?}"
        );
    }

    #[test]
    fn closing_the_fence_later_restores_code_styling() {
        let lines = render_body(&body("```rust\nlet x = 1;\n```"), 78);
        assert_eq!(visible(&lines), vec!["let x = 1;"]);
        assert!(!lines[0].runs().is_empty());
    }

    #[test]
    fn tilde_fences_get_the_same_unclosed_treatment() {
        let lines = render_body(&body("~~~\ncode?"), 78);
        assert!(visible(&lines).join("\n").contains("~~~"));
        assert!(lines.iter().all(|line| line.runs().is_empty()));
    }

    #[test]
    fn lists_gain_bullets_numbers_and_indent() {
        let lines = render_body(&body("- a\n- b\n\n1. one\n2. two"), 78);
        assert_eq!(visible(&lines), vec!["• a", "• b", "", "1. one", "2. two"]);
        assert!(lines.iter().all(|line| line.runs().is_empty()));
    }

    #[test]
    fn nested_lists_indent_two_spaces_per_level() {
        let lines = render_body(&body("- a\n  - deep"), 78);
        assert_eq!(visible(&lines), vec!["• a", "  • deep"]);
    }

    #[test]
    fn task_markers_render_as_brackets() {
        let lines = render_body(&body("- [ ] todo\n- [x] done"), 78);
        assert_eq!(visible(&lines), vec!["• [ ] todo", "• [x] done"]);
    }

    #[test]
    fn quotes_keep_text_and_gain_quote_style() {
        let lines = render_body(&body("> quoted words"), 78);
        assert_eq!(visible(&lines), vec!["quoted words"]);
        assert!(lines[0].runs()[0].style.contains(MdStyle::QUOTE));
    }

    #[test]
    fn links_keep_text_and_append_the_url_dimmed() {
        let lines = render_body(&body("[docs](https://example.com/x)"), 78);
        assert_eq!(visible(&lines), vec!["docs <https://example.com/x>"]);
        let runs = lines[0].runs();
        assert_eq!(runs.len(), 2);
        assert!(runs[0].style.contains(MdStyle::LINK));
        assert!(runs[1].style.contains(MdStyle::LINK_URL));
    }

    #[test]
    fn rule_fills_exactly_the_given_width() {
        let lines = render_body(&body("a\n\n---\n\nb"), 40);
        let rule: String = "\u{2500}".repeat(40);
        assert_eq!(
            visible(&lines),
            vec![
                "a".to_owned(),
                String::new(),
                rule,
                String::new(),
                "b".to_owned()
            ]
        );
        assert_eq!(lines[2].visible_len(), 40);
    }

    #[test]
    fn tables_fall_back_to_plain_paragraphs() {
        let lines = render_body(&body("| a | b |\n| c | d |"), 78);
        let text = visible(&lines).join("\n");
        assert!(text.contains("| a | b |"), "pipes stay literal: {text:?}");
    }

    #[test]
    fn runs_cover_styled_spans_exactly_without_gaps() {
        let lines = render_body(&body("x **bold mid** y"), 78);
        let line = &lines[0];
        let text = line.visible_text();
        let runs = line.runs();
        assert_eq!(runs.len(), 1);
        let run = runs[0];
        assert_eq!(&text[run.start..run.start + run.len], "bold mid");
    }

    #[test]
    fn empty_and_blank_bodies_render_no_lines() {
        assert!(render_body(&[], 78).is_empty());
        assert!(render_body(&body(""), 78).is_empty());
        assert!(render_body(&body("\n\n"), 78).is_empty());
    }

    #[test]
    fn slice_runs_clips_and_rebases_to_the_window() {
        let runs = vec![
            StyledRun {
                start: 0,
                len: 5,
                style: MdStyle::BOLD,
            },
            StyledRun {
                start: 8,
                len: 4,
                style: MdStyle::CODE,
            },
        ];
        assert_eq!(slice_runs(&runs, 0, 5), vec![runs[0]]);
        assert_eq!(
            slice_runs(&runs, 2, 10),
            vec![
                StyledRun {
                    start: 0,
                    len: 3,
                    style: MdStyle::BOLD,
                },
                StyledRun {
                    start: 6,
                    len: 2,
                    style: MdStyle::CODE,
                },
            ]
        );
        assert!(slice_runs(&runs, 5, 8).is_empty());
    }
}

#[cfg(test)]
mod extended_tests {
    use super::tests::{body, visible};
    use super::*;

    #[test]
    fn blocks_breathe_with_one_air_row_between_them() {
        let lines = render_body(
            &body("# Title\n\nfirst para\n\n- a\n- b\n\n> quoted\n\nlast"),
            78,
        );
        assert_eq!(
            visible(&lines),
            vec![
                "Title",
                "",
                "first para",
                "",
                "• a",
                "• b",
                "",
                "quoted",
                "",
                "last",
            ]
        );
    }

    #[test]
    fn single_blocks_and_plain_lines_gain_no_air() {
        assert_eq!(
            visible(&render_body(&body("just one"), 78)),
            vec!["just one"]
        );
        assert_eq!(
            visible(&render_body(&body("row one\nrow two"), 78)),
            vec!["row one", "row two"],
        );
    }

    #[test]
    fn code_blocks_keep_inner_blanks_and_gain_outer_air() {
        let lines = render_body(&body("before\n\n```\nx = 1;\n\ny = 2;\n```\n\nafter"), 78);
        assert_eq!(
            visible(&lines),
            vec!["before", "", "x = 1;", "", "y = 2;", "", "after"]
        );
    }

    #[test]
    fn tables_render_header_separator_and_rows() {
        let lines = render_body(&body("| a | b |\n|---|---|\n| 1 | 2 |"), 78);
        assert_eq!(
            visible(&lines),
            vec![
                "\u{256d}\u{2500}\u{2500}\u{2500}\u{252c}\u{2500}\u{2500}\u{2500}\u{256e}",
                "\u{2502} a \u{2502} b \u{2502}",
                "\u{251c}\u{2500}\u{2500}\u{2500}\u{253c}\u{2500}\u{2500}\u{2500}\u{2524}",
                "\u{2502} 1 \u{2502} 2 \u{2502}",
                "\u{2570}\u{2500}\u{2500}\u{2500}\u{2534}\u{2500}\u{2500}\u{2500}\u{256f}",
            ]
        );
        // The header row carries the bold style; rules stay plain.
        assert!(lines[1].runs()[0].style.contains(MdStyle::BOLD));
        assert!(lines[2].runs().is_empty());
    }

    #[test]
    fn tables_honor_alignment_markers() {
        let lines = render_body(&body("| l | r | c |\n|:--|--:|:--:|\n| ab | cd | ef |"), 78);
        assert_eq!(
            visible(&lines),
            vec![
                "\u{256d}\u{2500}\u{2500}\u{2500}\u{2500}\u{252c}\u{2500}\u{2500}\u{2500}\u{2500}\u{252c}\u{2500}\u{2500}\u{2500}\u{2500}\u{256e}",
                "\u{2502} l  \u{2502}  r \u{2502} c  \u{2502}",
                "\u{251c}\u{2500}\u{2500}\u{2500}\u{2500}\u{253c}\u{2500}\u{2500}\u{2500}\u{2500}\u{253c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2524}",
                "\u{2502} ab \u{2502} cd \u{2502} ef \u{2502}",
                "\u{2570}\u{2500}\u{2500}\u{2500}\u{2500}\u{2534}\u{2500}\u{2500}\u{2500}\u{2500}\u{2534}\u{2500}\u{2500}\u{2500}\u{2500}\u{256f}",
            ]
        );
    }

    #[test]
    fn tables_shrink_wide_columns_and_wrap_cells() {
        let lines = render_body(&body("| a | b |\n|---|---|\n| 0123456789abcdef | x |"), 20);
        for line in &lines {
            assert!(
                line.visible_len() <= 20,
                "every table row fits: {:?}",
                line.visible_text()
            );
        }
        let text = visible(&lines).join("\n");
        assert!(text.contains("0123456789ab"), "{text:?}");
        assert!(text.contains("\u{2502} cdef"), "{text:?}");
    }

    #[test]
    fn tables_keep_empty_cells_aligned() {
        let lines = render_body(&body("| a | b |\n|---|---|\n|  | 2 |"), 78);
        assert_eq!(
            visible(&lines),
            vec![
                "\u{256d}\u{2500}\u{2500}\u{2500}\u{252c}\u{2500}\u{2500}\u{2500}\u{256e}",
                "\u{2502} a \u{2502} b \u{2502}",
                "\u{251c}\u{2500}\u{2500}\u{2500}\u{253c}\u{2500}\u{2500}\u{2500}\u{2524}",
                "\u{2502}   \u{2502} 2 \u{2502}",
                "\u{2570}\u{2500}\u{2500}\u{2500}\u{2534}\u{2500}\u{2500}\u{2500}\u{256f}",
            ]
        );
    }

    #[test]
    fn tables_measure_columns_in_cells() {
        // "中" needs 2 cells: the column sizes 4 cells, not 2 chars.
        let lines = render_body(&body("| \u{4e2d}\u{6587} | x |\n|---|---|\n| 1 | 2 |"), 78);
        assert_eq!(
            visible(&lines),
            vec![
                "\u{256d}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{252c}\u{2500}\u{2500}\u{2500}\u{256e}",
                "\u{2502} \u{4e2d}\u{6587} \u{2502} x \u{2502}",
                "\u{251c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{253c}\u{2500}\u{2500}\u{2500}\u{2524}",
                "\u{2502} 1    \u{2502} 2 \u{2502}",
                "\u{2570}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2534}\u{2500}\u{2500}\u{2500}\u{256f}",
            ]
        );
        for line in &lines {
            assert!(line.visible_len() <= 12, "{line:?}");
        }
    }

    #[test]
    fn tables_style_inline_content_inside_cells() {
        let lines = render_body(&body("| a |\n|---|\n| **b** |"), 78);
        assert_eq!(
            visible(&lines),
            vec![
                "\u{256d}\u{2500}\u{2500}\u{2500}\u{256e}",
                "\u{2502} a \u{2502}",
                "\u{251c}\u{2500}\u{2500}\u{2500}\u{2524}",
                "\u{2502} b \u{2502}",
                "\u{2570}\u{2500}\u{2500}\u{2500}\u{256f}",
            ]
        );
        let runs = lines[3].runs();
        assert_eq!(runs.len(), 1);
        assert!(runs[0].style.contains(MdStyle::BOLD));
    }

    #[test]
    fn footnote_reference_links_and_definition_appends() {
        let lines = render_body(&body("see this[^1].\n\n[^1]: the note"), 78);
        assert_eq!(visible(&lines), vec!["see this[^1].", "", "[^1]: the note"]);
        let runs = lines[0].runs();
        assert_eq!(runs.len(), 1);
        assert!(runs[0].style.contains(MdStyle::LINK));
    }

    #[test]
    fn footnote_without_definition_stays_literal() {
        let lines = render_body(&body("see this[^9]."), 78);
        assert_eq!(visible(&lines), vec!["see this[^9]."]);
    }

    #[test]
    fn footnote_definition_continuations_align() {
        let lines = render_body(&body("a[^1]\n\n[^1]: first\n  second"), 78);
        assert_eq!(
            visible(&lines),
            vec!["a[^1]", "", "[^1]: first", "      second"]
        );
    }

    #[test]
    fn math_renders_literally_code_styled() {
        let lines = render_body(&body("solve $x^2$ now"), 78);
        assert_eq!(visible(&lines), vec!["solve $x^2$ now"]);
        assert!(lines[0].runs()[0].style.contains(MdStyle::CODE));
        let block = render_body(&body("$$\nx^2\n$$"), 78);
        assert_eq!(visible(&block), vec!["$$", "x^2", "$$"]);
    }

    #[test]
    fn definition_lists_indent_the_definition() {
        let lines = render_body(&body("term\n: the explanation"), 78);
        assert_eq!(visible(&lines), vec!["term", "  the explanation"]);
    }

    #[test]
    fn admonitions_keep_a_bold_label() {
        let lines = render_body(&body("> [!NOTE]\n> careful"), 78);
        assert_eq!(visible(&lines), vec!["[!NOTE]", "careful"]);
        assert!(lines[0].runs()[0].style.contains(MdStyle::BOLD));
        assert!(lines[1].runs()[0].style.contains(MdStyle::QUOTE));
    }

    #[test]
    fn heading_attributes_are_consumed_not_shown() {
        let lines = render_body(&body("# Title {#custom}"), 78);
        assert_eq!(visible(&lines), vec!["Title"]);
    }
}
