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
//!   task-list markers.
//! - Blocks: headings (markers stripped, styled instead), fenced and
//!   indented code blocks, bulleted/ordered/nested lists, block quotes
//!   (styled, no prefix), thematic breaks (full-width rule), hard breaks.
//! - Tables are OFF: pipe grids fall back to plain paragraphs, which stay
//!   readable instead of pretending to align under character wrapping.
//! - Raw HTML has no terminal representation and is shown literally
//!   (code-styled) rather than dropped, so model output is never silently
//!   lost. The sanitizer has already neutralized control sequences.
//! - An unclosed fence renders as plain text: while streaming, the closing
//!   fence has not arrived yet, and a lone opener must not restyle the tail
//!   of the message. Once the fence closes the block renders as code.

use std::borrow::Cow;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

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
        }
    }

    /// OR of every active style frame (nested constructs combine).
    fn combined(&self) -> MdStyle {
        self.styles
            .iter()
            .fold(MdStyle::PLAIN, |acc, style| acc.union(*style))
    }

    /// Pushes text with the combined style, merging into the previous span
    /// when the style matches so runs stay compact.
    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.image_alt_seen {
            self.image_alt_seen = false;
        }
        let style = self.combined();
        if let Some(last) = self.current.spans.last_mut()
            && last.style == style
        {
            last.text.push_str(text);
            return;
        }
        self.current.spans.push(RichSpan {
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

/// Parses assistant body lines into logical styled lines. `rule_width` sizes
/// the thematic-break rule so it fills exactly one wrapped row; it is the
/// only width-dependent part of the output.
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
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut parser = BodyParser::new();
    for event in Parser::new_ext(&source, options) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => parser.end_line(),
                Tag::Heading { .. } => {
                    parser.end_line();
                    parser.styles.push(MdStyle::HEADING.union(MdStyle::BOLD));
                }
                Tag::BlockQuote(_) => {
                    parser.end_line();
                    parser.styles.push(MdStyle::QUOTE);
                }
                Tag::CodeBlock(_) => {
                    parser.end_line();
                    parser.in_code_block = true;
                }
                Tag::List(first) => {
                    parser.end_line();
                    let depth = parser.lists.len();
                    parser.lists.push(ListContext {
                        ordered: first.is_some(),
                        next: first.unwrap_or(1),
                        depth,
                    });
                }
                Tag::Item => {
                    parser.end_line();
                    let prefix = match parser.lists.last_mut() {
                        Some(list) if list.ordered => {
                            let number = list.next;
                            list.next += 1;
                            format!("{}{number}. ", "  ".repeat(list.depth))
                        }
                        Some(list) => format!("{}• ", "  ".repeat(list.depth)),
                        None => "• ".to_owned(),
                    };
                    parser.pending_prefix = Some(prefix);
                }
                Tag::Emphasis => parser.styles.push(MdStyle::ITALIC),
                Tag::Strong => parser.styles.push(MdStyle::BOLD),
                Tag::Strikethrough => parser.styles.push(MdStyle::STRIKE),
                Tag::Link { dest_url, .. } => {
                    parser.styles.push(MdStyle::LINK);
                    parser.link_dests.push(dest_url.into_string());
                }
                Tag::Image { dest_url, .. } => {
                    parser.styles.push(MdStyle::LINK);
                    parser.image_alt_seen = true;
                    parser.link_dests.push(format!("image: {}", dest_url));
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph
                | TagEnd::Heading(..)
                | TagEnd::BlockQuote(..)
                | TagEnd::CodeBlock
                | TagEnd::Item
                | TagEnd::List(_) => {
                    parser.end_line();
                    match tag {
                        TagEnd::Heading(..) | TagEnd::BlockQuote(..) => {
                            parser.styles.pop();
                        }
                        TagEnd::CodeBlock => parser.in_code_block = false,
                        TagEnd::List(_) => {
                            parser.lists.pop();
                        }
                        _ => {}
                    }
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    parser.styles.pop();
                }
                TagEnd::Link => {
                    parser.styles.pop();
                    if let Some(dest) = parser.link_dests.pop() {
                        // Direct push (never merged): merging into a matching
                        // outer span would restyle the link text itself.
                        parser.flush_prefix();
                        parser.current.spans.push(RichSpan {
                            text: format!(" <{dest}>"),
                            style: MdStyle::LINK_URL,
                        });
                    }
                }
                TagEnd::Image => {
                    parser.styles.pop();
                    if parser.image_alt_seen {
                        parser.image_alt_seen = false;
                        parser.flush_prefix();
                        parser.current.spans.push(RichSpan {
                            text: "image".to_owned(),
                            style: MdStyle::LINK,
                        });
                    }
                    if let Some(dest) = parser.link_dests.pop() {
                        parser.flush_prefix();
                        parser.current.spans.push(RichSpan {
                            text: format!(" ({dest})"),
                            style: MdStyle::LINK_URL,
                        });
                    }
                }
                _ => {}
            },
            Event::Text(text) => {
                if parser.in_code_block {
                    let mut segments = text.split('\n').peekable();
                    while let Some(segment) = segments.next() {
                        if !segment.is_empty() {
                            let style = parser.combined().union(MdStyle::CODE);
                            if let Some(last) = parser.current.spans.last_mut() {
                                if last.style == style {
                                    last.text.push_str(segment);
                                } else {
                                    parser.current.spans.push(RichSpan {
                                        text: segment.to_owned(),
                                        style,
                                    });
                                }
                            } else {
                                parser.current.spans.push(RichSpan {
                                    text: segment.to_owned(),
                                    style,
                                });
                            }
                        }
                        if segments.peek().is_some() {
                            parser.end_line_force();
                        }
                    }
                } else {
                    parser.flush_prefix();
                    parser.push_text(&text);
                }
            }
            Event::Code(code) => {
                parser.flush_prefix();
                let style = parser.combined().union(MdStyle::CODE);
                if let Some(last) = parser.current.spans.last_mut()
                    && last.style == style
                {
                    last.text.push_str(&code);
                    continue;
                }
                parser.current.spans.push(RichSpan {
                    text: code.into_string(),
                    style,
                });
            }
            Event::SoftBreak => {
                // Deliberate deviation from HTML rendering (where a soft
                // break is a space): a source row stays a row, so streamed
                // plain-text lines never merge into one wrapped line.
                parser.end_line();
            }
            Event::HardBreak => parser.end_line_force(),
            Event::Rule => {
                parser.end_line();
                let width = rule_width.max(1);
                parser.push_line(RichLine {
                    spans: vec![RichSpan {
                        text: "─".repeat(width),
                        style: MdStyle::PLAIN,
                    }],
                });
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                match parser.pending_prefix.take() {
                    Some(prefix) => {
                        parser.pending_prefix = Some(format!("{prefix}{marker}"));
                    }
                    None => parser.push_text(marker),
                }
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                // Raw HTML has no terminal representation: show it literally
                // (code-styled) instead of dropping model output.
                for (index, segment) in html.split('\n').enumerate() {
                    if index > 0 {
                        parser.end_line_force();
                    }
                    if !segment.is_empty() {
                        let style = parser.combined().union(MdStyle::CODE);
                        parser.current.spans.push(RichSpan {
                            text: segment.to_owned(),
                            style,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    parser.end_line();
    parser.lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visible(lines: &[RichLine]) -> Vec<String> {
        lines.iter().map(RichLine::visible_text).collect()
    }

    fn body(text: &str) -> Vec<String> {
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
        assert_eq!(visible(&lines), vec!["• a", "• b", "1. one", "2. two"]);
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
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].visible_len(), 40);
        assert!(lines[1].visible_text().chars().all(|char| char == '─'));
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
