//! Presentation state only: bounded viewport, folding, approval card,
//! composer draft, and a controlled refresh gate.
//!
//! Nothing here executes tools, grants approvals, or bypasses policy.
//! Decisions leave this module as typed [`nexus_core::Command`] values via
//! [`crate::decisions`]; applying runtime events enters via
//! [`AppState::apply_event`], which rejects stale-run, duplicate,
//! out-of-order, and post-terminal updates.
//!
//! Memory is bounded at three levels: per line (`LINE_OVERHEAD` plus its
//! bytes), per entry ([`MAX_ENTRY_BYTES`], including the title and empty
//! lines), and across retained entries ([`MAX_RETAINED_BYTES`]). Older
//! entries are dropped from presentation only; accepted conversation records
//! are unaffected.

use std::cell::Cell;
use std::collections::VecDeque;
use std::mem::size_of;
use std::time::{Duration, Instant};

use nexus_core::{CallId, EventPayload, Limits, RunEvent, RunId, RunOutcome, TurnId, Usage};

use crate::sanitize::{is_bidi_format, sanitize, sanitize_approval};
use crate::{markdown, markdown::StyledRun};

use nexus_config::{AgentMode, MODE_BUILD};

/// Presentation entry cap, tracking the M0-test retained-context budget
/// (lock section 1: 128 retained context items). Older entries are dropped
/// from presentation only; accepted conversation records are unaffected.
pub const MAX_RETAINED_ENTRIES: usize = Limits::M0_TEST_RETAINED_CONTEXT_ITEMS;
/// Per-entry presentation cap in bytes (M0-test choice, not a product
/// default). The title, every retained line (including empty ones), and the
/// per-line bookkeeping overhead count against it. Excess is dropped with
/// the entry's truncation flag set.
pub const MAX_ENTRY_BYTES: usize = 8_192;
/// Aggregate retained presentation bytes across all entries (M0-test choice,
/// not a product default). Exceeding it drops the oldest entries first,
/// counted as presentation drops.
pub const MAX_RETAINED_BYTES: usize = 256 * 1024;
/// Maximum retained title bytes (M0-test choice). Titles come from runtime
/// identities and provider item keys, so they are sanitized, bounded, and
/// flattened to one line on every insertion.
pub const MAX_TITLE_BYTES: usize = 256;
/// Post-sanitization presentation bound per approval field (M0-test choice).
/// Core notices are limited to [`nexus_core::commands::MAX_SUMMARY_BYTES`]
/// bytes before escaping, and the sanitizer expands one byte at most 4x (a
/// 2-byte format character such as U+061C or the soft hyphen becomes an
/// 8-byte visible `\u{XXXX}` escape), so 4x that bound admits every valid
/// notice uncut. It stays finite and fail-closed: a forged field above the
/// cap is cut and can never be approved.
pub const MAX_APPROVAL_FIELD_BYTES: usize = 4 * nexus_core::commands::MAX_SUMMARY_BYTES;
/// Explicit worst-case card display budget: the three bounded fields
/// (summary, scope, optional arguments preview) retained together.
pub const MAX_APPROVAL_CARD_BYTES: usize = 3 * MAX_APPROVAL_FIELD_BYTES;
/// Composer cap tracking the command input bound.
pub const MAX_COMPOSER_BYTES: usize = nexus_core::commands::MAX_INPUT_BYTES;
/// Maximum recalled composer inputs (M0-test choice). Older submissions
/// are forgotten first; the bound keeps recall memory finite.
pub const MAX_HISTORY_ENTRIES: usize = 128;
/// Maximum redraw rate: event-driven redraws are coalesced to at most one
/// frame per interval so a saturated stream cannot force full relayouts.
pub const MAX_REDRAW_INTERVAL: Duration = Duration::from_millis(33);

/// Bytes charged per retained line for the `String` bookkeeping itself, so
/// an unbounded run of empty lines still exhausts [`MAX_ENTRY_BYTES`].
const LINE_OVERHEAD: usize = size_of::<String>();
/// Prefix of every rendered body line, also reserved out of the wrap width.
const BODY_INDENT: usize = 2;
/// Rendered when an entry hit its byte bound.
const TRUNCATION_MARKER: &str = "[output truncated to presentation bound]";

/// Conversation entry kind for grouping and rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// User-submitted input.
    User,
    /// Streamed assistant text.
    Assistant,
    /// Tool previews, status, and outcomes.
    Tool,
    /// Run lifecycle and frontend notices.
    System,
}

/// Identity of a streamed text run: consecutive fragments with the same key
/// assemble into one logical line; any other entry in between breaks the
/// adjacency so interleaved items never merge.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StreamKey {
    /// Assistant text for one turn-local item.
    Assistant { turn: TurnId, item: String },
    /// Progress preview for one executing call.
    ToolOutput { call: CallId },
}

/// Cheap fingerprint of everything that affects a wrapped height. Direct
/// field writes to an [`Entry`] invalidate the cached height when they change
/// one of these; presentation code normally mutates through [`AppState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeightKey {
    width: usize,
    lines: usize,
    folded: bool,
    truncated: bool,
    title_len: usize,
    tail_len: usize,
}

/// Cached wrapped height for one width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeightCache {
    key: HeightKey,
    height: usize,
}

/// One foldable conversation entry with pre-sanitized lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Grouping kind.
    pub kind: EntryKind,
    /// One-line header shown even when folded. Sanitized, one line, and
    /// bounded to [`MAX_TITLE_BYTES`] at construction.
    pub title: String,
    /// Sanitized body lines (no embedded newlines).
    pub lines: Vec<String>,
    /// Folded entries render as their title plus a marker only.
    pub folded: bool,
    /// True when title or body text was cut to [`MAX_ENTRY_BYTES`].
    pub truncated: bool,
    /// Retained bytes: title + per-line overhead + line bytes. The bound is
    /// [`MAX_ENTRY_BYTES`].
    bytes: usize,
    /// Stream identity when the trailing line is still open for appends.
    stream: Option<StreamKey>,
    /// True while the last line accepts appended fragments.
    open_line: bool,
    /// Cached wrapped height, invalidated by the fingerprint above.
    height_cache: Cell<Option<HeightCache>>,
}

impl Entry {
    fn new(kind: EntryKind, title: &str) -> Self {
        let title = bounded_title(title);
        let bytes = title.len();
        Self {
            kind,
            title,
            lines: Vec::new(),
            folded: false,
            truncated: false,
            bytes,
            stream: None,
            open_line: false,
            height_cache: Cell::new(None),
        }
    }

    fn new_stream(kind: EntryKind, title: &str, stream: StreamKey) -> Self {
        let mut entry = Self::new(kind, title);
        entry.stream = Some(stream);
        entry
    }

    /// Retained bytes, for bound enforcement and tests.
    fn retained_bytes(&self) -> usize {
        self.bytes
    }

    /// Appends one finalized line, charging overhead and bytes and keeping a
    /// UTF-8-safe prefix when the bound is reached.
    fn push_line(&mut self, line: &str) {
        self.push_new_line(line);
        self.open_line = false;
    }

    /// Appends a streamed fragment, preserving real newlines: completed
    /// segments become lines, the trailing segment stays open for the next
    /// fragment, and a trailing newline closes the current line without
    /// materializing a stray empty one.
    fn append_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut segments = text.split('\n').peekable();
        while let Some(segment) = segments.next() {
            let more = segments.peek().is_some();
            if !more && segment.is_empty() {
                // Trailing newline: close the current line; the next
                // fragment starts a fresh one.
                self.open_line = false;
                break;
            }
            if self.open_line {
                self.append_open(segment);
            } else {
                self.push_new_line(segment);
            }
            self.open_line = !more;
        }
    }

    /// Marks streamed output truncation without adding a line.
    fn mark_truncated(&mut self) {
        self.truncated = true;
    }

    fn push_new_line(&mut self, text: &str) {
        let remaining = MAX_ENTRY_BYTES.saturating_sub(self.bytes);
        if remaining < LINE_OVERHEAD {
            self.truncated = true;
            return;
        }
        let capacity = remaining - LINE_OVERHEAD;
        if text.len() <= capacity {
            self.bytes += LINE_OVERHEAD + text.len();
            self.lines.push(text.to_owned());
        } else {
            let prefix = safe_prefix(text, capacity);
            self.bytes += LINE_OVERHEAD + prefix.len();
            self.lines.push(prefix.to_owned());
            self.truncated = true;
        }
    }

    fn append_open(&mut self, text: &str) {
        let Some(last) = self.lines.last_mut() else {
            self.push_new_line(text);
            return;
        };
        let remaining = MAX_ENTRY_BYTES.saturating_sub(self.bytes);
        if remaining == 0 {
            self.truncated = true;
            return;
        }
        if text.len() <= remaining {
            last.push_str(text);
            self.bytes += text.len();
        } else {
            let prefix = safe_prefix(text, remaining);
            last.push_str(prefix);
            self.bytes += prefix.len();
            self.truncated = true;
        }
    }

    /// Wrapped line count at `width` (folded entries render one marker line).
    /// The cached result is reused while the cheap fingerprint is unchanged.
    fn wrapped_len(&self, width: usize) -> usize {
        let width = width.max(1);
        let key = HeightKey {
            width,
            lines: self.lines.len(),
            folded: self.folded,
            truncated: self.truncated,
            title_len: self.title.len(),
            tail_len: self.lines.last().map_or(0, String::len),
        };
        if let Some(cache) = self.height_cache.get()
            && cache.key == key
        {
            return cache.height;
        }
        let height = self.compute_height(width);
        self.height_cache.set(Some(HeightCache { key, height }));
        height
    }

    fn compute_height(&self, width: usize) -> usize {
        // Every entry ends with one blank separator row so messages never
        // run together; see `render_range`, which must emit the same count.
        // Assistant bodies render as Markdown: the parsed logical lines (not
        // the raw stored lines) are what both height and rendering measure.
        if self.folded {
            return wrapped_height(&folded_text(&self.title, self.lines.len()), width) + 1;
        }
        let body_width = body_width(width);
        let mut height = wrapped_height(&self.title, width);
        if self.kind == EntryKind::Assistant {
            for rich in markdown::render_body(&self.lines, body_width) {
                height += wrapped_height(&rich.visible_text(), body_width);
            }
        } else {
            for line in &self.lines {
                height += wrapped_height(line, body_width);
            }
        }
        if self.truncated {
            height += wrapped_height(TRUNCATION_MARKER, body_width);
        }
        height + 1
    }

    /// Renders at most `take` wrapped lines of this entry into `out`,
    /// skipping the first `skip` lines. Skipped lines are never allocated, so
    /// an oversized entry intersecting the window materializes only the
    /// visible slice. Line counts match [`Entry::wrapped_len`] exactly,
    /// including the trailing blank separator row.
    ///
    /// `runs_out` receives one style-run list per emitted line, in lockstep
    /// with `out`: styles are width-neutral metadata and never change which
    /// lines are emitted.
    fn render_range(
        &self,
        width: usize,
        skip: usize,
        take: usize,
        out: &mut Vec<String>,
        runs_out: &mut Vec<Vec<StyledRun>>,
    ) {
        if take == 0 {
            return;
        }
        let width = width.max(1);
        let limit = out.len().saturating_add(take);
        let out_base = out.len();
        let runs_base = runs_out.len();
        let mut index = 0usize;
        // Single choke point for emission: text goes through `emit_line`
        // (skip/take semantics in one place) and the style runs follow the
        // same decision, shifted past the body indent so both vecs agree.
        let mut emit = |text: &str, body: bool, row_runs: Vec<StyledRun>| -> bool {
            let before = out.len();
            let stop = emit_line(out, &mut index, skip, limit, text, body);
            if out.len() > before {
                let mut shifted = row_runs;
                if body {
                    for run in &mut shifted {
                        run.start += BODY_INDENT;
                    }
                }
                runs_out.push(shifted);
            }
            stop
        };
        if self.folded {
            let marker = folded_text(&self.title, self.lines.len());
            for chunk in chunks(&marker, width) {
                if emit(chunk, false, Vec::new()) {
                    return;
                }
            }
            emit("", false, Vec::new());
            return;
        }
        for chunk in chunks(&self.title, width) {
            if emit(chunk, false, Vec::new()) {
                return;
            }
        }
        if self.title.is_empty() && emit("", false, Vec::new()) {
            return;
        }
        let body_width = body_width(width);
        if self.kind == EntryKind::Assistant {
            for rich in markdown::render_body(&self.lines, body_width) {
                let text = rich.visible_text();
                if text.is_empty() {
                    // A blank logical line (blank code row, hard break) is
                    // one row, mirroring `wrapped_height("") == 1`.
                    if emit("", true, Vec::new()) {
                        return;
                    }
                    continue;
                }
                let line_runs = rich.runs();
                let mut start = 0usize;
                for chunk in chunks(&text, body_width) {
                    let end = start + chunk.chars().count();
                    if emit(chunk, true, markdown::slice_runs(&line_runs, start, end)) {
                        return;
                    }
                    start = end;
                }
            }
        } else {
            for line in &self.lines {
                if line.is_empty() {
                    if emit("", true, Vec::new()) {
                        return;
                    }
                } else {
                    for chunk in chunks(line, body_width) {
                        if emit(chunk, true, Vec::new()) {
                            return;
                        }
                    }
                }
            }
        }
        if self.truncated {
            for chunk in chunks(TRUNCATION_MARKER, body_width) {
                if emit(chunk, true, Vec::new()) {
                    return;
                }
            }
        }
        emit("", false, Vec::new());
        debug_assert_eq!(
            runs_out.len().saturating_sub(runs_base),
            out.len().saturating_sub(out_base),
            "every emitted line carries exactly one run list"
        );
    }
}

/// Appends one wrapped line, honoring the skip offset and take budget.
/// Returns true when the budget is filled and rendering can stop.
fn emit_line(
    out: &mut Vec<String>,
    index: &mut usize,
    skip: usize,
    limit: usize,
    text: &str,
    body: bool,
) -> bool {
    if out.len() >= limit {
        return true;
    }
    if *index >= skip {
        if body {
            let mut line = String::with_capacity(text.len() + BODY_INDENT);
            line.push_str("  ");
            line.push_str(text);
            out.push(line);
        } else {
            out.push(text.to_owned());
        }
    }
    *index += 1;
    out.len() >= limit
}

/// Iterator over at most `width`-char chunks of `text`, as borrowed slices
/// (skipped chunks cost no allocation).
pub(crate) struct Chunks<'a> {
    text: &'a str,
    width: usize,
    start: usize,
}

impl<'a> Iterator for Chunks<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.start >= self.text.len() {
            return None;
        }
        // Cell-based: accumulate whole characters while they fit, so a
        // wide glyph is never split across rows. A single glyph wider than
        // the width still advances one glyph, never zero.
        let mut cells = 0usize;
        let mut end = self.text.len();
        for (offset, char) in self.text[self.start..].char_indices() {
            let wide = display_width_char(char);
            if cells > 0 && cells + wide > self.width {
                end = self.start + offset;
                break;
            }
            cells += wide;
        }
        let chunk = &self.text[self.start..end];
        self.start = end;
        Some(chunk)
    }
}

pub(crate) fn chunks(text: &str, width: usize) -> Chunks<'_> {
    Chunks {
        text,
        width: width.max(1),
        start: 0,
    }
}

/// Display width of one character in terminal cells. Unprintable widths
/// fall back to 1, matching the historical count for tabs (whose true
/// advance is position-dependent and stays a documented limitation).
fn display_width_char(char: char) -> usize {
    unicode_width::UnicodeWidthChar::width(char).unwrap_or(1)
}

/// Display width of a whole line in terminal cells.
pub(crate) fn display_width(text: &str) -> usize {
    text.chars().map(display_width_char).sum()
}

/// Maps a terminal cell column onto a character index in `line`: the first
/// character whose cells cover the column, or the line end when past it.
/// Mouse columns are cells; downstream selection math stays in characters.
pub fn cell_to_char_col(line: &str, cell: usize) -> usize {
    let mut pos = 0usize;
    for (index, char) in line.chars().enumerate() {
        if pos + display_width_char(char) > cell {
            return index;
        }
        pos += display_width_char(char);
    }
    line.chars().count()
}

/// Cell-count wrapping height of one logical line: what the terminal shows
/// is what the viewport counts, including CJK-wide glyphs. (Tab expansion
/// stays position-dependent and is still a documented limitation.)
pub(crate) fn wrapped_height(line: &str, width: usize) -> usize {
    let width = width.max(1);
    let cells = display_width(line).max(1);
    cells.div_ceil(width).max(1)
}

/// Wrap width available to an indented body line.
fn body_width(width: usize) -> usize {
    width.saturating_sub(BODY_INDENT).max(1)
}

/// Folded marker text; its own height is measured (a long title may wrap).
fn folded_text(title: &str, hidden_lines: usize) -> String {
    format!("{title}  [folded, {hidden_lines} lines]")
}

/// Returns the longest prefix of `text` that is at most `max_bytes` long and
/// ends on a UTF-8 character boundary.
fn safe_prefix(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Sanitizes a title, flattens it to one line, and bounds it to
/// [`MAX_TITLE_BYTES`] on a safe prefix. Applied on every insertion.
fn bounded_title(raw: &str) -> String {
    let clean = sanitize(raw);
    let flattened = clean.replace('\n', " ");
    safe_prefix(&flattened, MAX_TITLE_BYTES).to_owned()
}

/// Sanitizes and bounds one approval-card field, flattening newlines.
fn bounded_text(raw: &str, max_bytes: usize) -> (String, bool) {
    let clean = sanitize_approval(raw);
    let flattened = clean.replace('\n', " ");
    if flattened.len() <= max_bytes {
        (flattened, false)
    } else {
        (safe_prefix(&flattened, max_bytes).to_owned(), true)
    }
}

/// [`bounded_text`] for an optional field.
fn bounded_optional_text(raw: Option<&str>, max_bytes: usize) -> (Option<String>, bool) {
    match raw {
        Some(text) => {
            let (bounded, cut) = bounded_text(text, max_bytes);
            (Some(bounded), cut)
        }
        None => (None, false),
    }
}

/// Rendered lines for one entry (used by tests to prove that wrapped heights
/// and rendering agree).
#[cfg(test)]
fn render_entry_lines(entry: &Entry, width: usize) -> Vec<String> {
    render_entry_rows(entry, width).0
}

/// Rendered lines plus their style runs for one entry.
#[cfg(test)]
fn render_entry_rows(entry: &Entry, width: usize) -> (Vec<String>, Vec<Vec<StyledRun>>) {
    let mut out = Vec::new();
    let mut runs = Vec::new();
    entry.render_range(width, 0, usize::MAX, &mut out, &mut runs);
    (out, runs)
}

/// Live approval awaiting a decision. Decisions become
/// [`nexus_core::Command::Approve`] / [`nexus_core::Command::Deny`] via
/// [`crate::decisions`]; the card itself authorizes nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApprovalCard {
    /// Grant identity bound by the runtime.
    pub approval: nexus_core::ApprovalId,
    /// Call the grant is bound to.
    pub call: nexus_core::CallId,
    /// Exact safe operation summary from the runtime notice.
    pub summary: String,
    /// Affected resource scope from the runtime notice.
    pub scope_summary: String,
    /// Monotonic expiry reading from the runtime notice.
    pub expires_at_elapsed: Duration,
}

/// Geometry of the most recently rendered approval card. The renderer records
/// it so key handling can refuse to decide from a clipped card before the
/// user inspected the full detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ApprovalGeometry {
    /// Inner card width in cells.
    pub inner_width: usize,
    /// Inner card rows the card was allowed.
    pub inner_rows: usize,
    /// Wrapped detail rows the card wanted.
    pub detail_rows: usize,
    /// True when detail rows did not fit; approval is gated on inspection.
    pub clipped: bool,
}

/// Bottom-anchored visible window over the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleView {
    /// Rendered lines, oldest first, at most `height` entries.
    pub lines: Vec<String>,
    /// Width-neutral style runs per rendered line, in lockstep with
    /// [`VisibleView::lines`]: `styles[i]` styles `lines[i]` and is empty
    /// for unstyled rows. Window slicing applies to both identically.
    pub styles: Vec<Vec<StyledRun>>,
    /// Wrapped lines above the window (presentation-truncated view).
    pub hidden_above: usize,
    /// True when older entries were dropped from retention.
    pub retention_truncated: bool,
}

/// One endpoint of a drag selection: a visible-window row plus the
/// character column inside it. Rows index the current window output,
/// never retained entries, so any scroll or content change clears the
/// selection instead of letting it drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelPoint {
    /// Window row (0 is the oldest visible line).
    pub row: usize,
    /// Character column inside the row.
    pub col: usize,
}

/// Drag-selected text: either conversation rows or a composer draft
/// range. At most one is active; a new press replaces it outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextSelection {
    /// Conversation rows between two window points, inclusive.
    Body { anchor: SelPoint, head: SelPoint },
    /// Composer draft range as character offsets (newlines count).
    Composer { anchor: usize, head: usize },
}

/// Last rendered pointer geometry: where the body content and the
/// composer draft sit in terminal cells. Recorded by the renderer each
/// frame (mirroring [`ApprovalGeometry`); mouse handling reads it back
/// to map terminal coordinates onto window rows and draft offsets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PointerGeometry {
    /// Body area origin (terminal cells).
    pub body_x: u16,
    pub body_y: u16,
    /// Content columns per row (excludes the scrollbar column).
    pub text_width: usize,
    /// Non-content rows (hero, truncation notice) above the content.
    pub content_offset: usize,
    /// Content rows shown.
    pub content_len: usize,
    /// Composer area origin and inner geometry (inside the border).
    pub composer_x: u16,
    pub composer_y: u16,
    pub composer_inner_w: usize,
    pub composer_rows: usize,
}

/// Copy-confirmation lifetime: long enough to be seen, then the next
/// tick clears it without touching the transcript.
pub const TOAST_TTL: Duration = Duration::from_secs(2);

/// A transient header toast: copy confirmations live outside the
/// transcript so they never pollute history or scrollback.
#[derive(Debug, Clone)]
struct Toast {
    text: String,
    deadline: Instant,
}

/// The full frontend presentation model.
#[derive(Debug)]
pub struct AppState {
    entries: VecDeque<Entry>,
    /// Sum of retained per-entry bytes, bounded by [`MAX_RETAINED_BYTES`].
    retained_bytes: usize,
    /// Presentation-only drops; accepted records are unaffected.
    pub dropped_entries: usize,
    composer: String,
    /// Submitted inputs for composer recall, oldest first, bounded by
    /// [`MAX_HISTORY_ENTRIES`]. `Up` walks older, `Down` walks newer.
    composer_history: Vec<String>,
    /// Recalled history index (`None` means the live draft is shown).
    history_cursor: Option<usize>,
    /// Live draft stashed when recall starts; restored when `Down` walks
    /// past the newest entry.
    history_stash: String,
    /// Entry index selected in the viewport (None follows the live tail).
    selected: Option<usize>,
    /// Drag-selected text, if any. Window-anchored and transient:
    /// cleared by scrolling, content changes, edits, and `Esc`, so a
    /// rendered highlight can never outlive the rows it was made from.
    text_selection: Option<TextSelection>,
    /// Wrapped lines hidden below the viewport bottom.
    scrollback: usize,
    pending_approval: Option<PendingApprovalCard>,
    toast: Option<Toast>,
    /// Exact-arguments preview from the runtime notice. Kept outside
    /// [`PendingApprovalCard`] so the existing public fields stay
    /// source-compatible; exposed via [`AppState::approval_args_preview`].
    approval_args_preview: Option<String>,
    approval_geometry: Option<ApprovalGeometry>,
    approval_field_truncated: bool,
    /// Expanded detail view open (opened deliberately, never a grant).
    approval_detail_open: bool,
    /// Top detail row currently scrolled to.
    approval_detail_scroll: usize,
    /// Exclusive upper bound of detail rows seen contiguously from row 0.
    /// Approval unlocks only when this reaches the last rendered row.
    approval_detail_seen_through: usize,
    /// Full detail row count reported by the last expanded render.
    approval_detail_total_rows: usize,
    /// Detail viewport height reported by the last expanded render.
    approval_detail_view_rows: usize,
    streaming: bool,
    active_run: Option<RunId>,
    /// Stale-run updates rejected, per the lock's two-sided rule.
    pub stale_rejected: u64,
    /// Duplicate, out-of-order (non-increasing sequence), or post-terminal
    /// same-run events rejected without mutating state.
    pub seq_rejected: u64,
    last_seq: Option<u64>,
    status: String,
    finished: Option<RunOutcome>,
    /// Last rendered pointer geometry for mouse mapping.
    pointer_geometry: PointerGeometry,
    /// Last viewport geometry, for selection visibility math.
    viewport: (usize, usize),
    /// Project directory shown in the header (the filesystem scope tools
    /// are jailed to). `None` renders no segment, so tests and headless
    /// transcripts that never set it are unaffected.
    project_dir: Option<String>,
    /// Conversation session label shown in the header (`s1`, `s2`, ...).
    /// `None` renders no segment, so frames that never select a session are
    /// unaffected.
    session_label: Option<String>,
    /// Model selected in the composer, shown in its title. `None` renders
    /// the bare title, so frames that never select a model are unaffected.
    active_model: Option<String>,
    /// Provider of the selected model, shown beside it when known.
    active_provider: Option<String>,
    /// Agent modes available to the composer, in cycle order: the built-in
    /// `plan` and `build` first, then custom modes from configuration.
    /// `Tab` in the composer cycles this list; the active mode names the
    /// composer border and gates the next submit's tool policy.
    modes: Vec<AgentMode>,
    /// Index into [`AppState::modes`] of the active mode.
    mode_index: usize,
    /// Latest usage counters observed on the live run. Cleared by the next
    /// `RunStarted` so a new run never wears the previous run's numbers.
    last_usage: Option<Usage>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    /// Starts a new conversation view (no history is loaded or replayed).
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            retained_bytes: 0,
            dropped_entries: 0,
            composer: String::new(),
            composer_history: Vec::new(),
            history_cursor: None,
            history_stash: String::new(),
            selected: None,
            text_selection: None,
            scrollback: 0,
            pending_approval: None,
            toast: None,
            approval_args_preview: None,
            approval_geometry: None,
            approval_field_truncated: false,
            approval_detail_open: false,
            approval_detail_scroll: 0,
            approval_detail_seen_through: 0,
            approval_detail_total_rows: 0,
            approval_detail_view_rows: 0,
            streaming: false,
            active_run: None,
            stale_rejected: 0,
            seq_rejected: 0,
            last_seq: None,
            status: "idle".to_owned(),
            finished: None,
            viewport: (80, 20),
            pointer_geometry: PointerGeometry::default(),
            project_dir: None,
            session_label: None,
            active_model: None,
            active_provider: None,
            modes: vec![AgentMode::plan(), AgentMode::build()],
            mode_index: 1,
            last_usage: None,
        }
    }

    /// Records the project directory for the header. Accepts the
    /// canonicalized working directory; presentation abbreviates it.
    pub fn set_project_dir(&mut self, dir: impl Into<String>) {
        self.project_dir = Some(dir.into());
    }

    /// Returns the project directory, if one was recorded.
    #[must_use]
    pub fn project_dir(&self) -> Option<&str> {
        self.project_dir.as_deref()
    }

    /// Records the conversation session label for the header. Short,
    /// frontend-assigned labels (`s1`, `s2`) name the slot; the runtime
    /// session identity stays the authority for commands and events.
    pub fn set_session_label(&mut self, label: impl Into<String>) {
        self.session_label = Some(label.into());
    }

    /// Returns the session label, if one was recorded.
    #[must_use]
    pub fn session_label(&self) -> Option<&str> {
        self.session_label.as_deref()
    }

    /// Shows a selected model (and its provider, when known) in the
    /// composer title. `None` clears the display back to the bare title.
    pub fn set_active_model(&mut self, model: Option<String>, provider: Option<String>) {
        self.active_model = model;
        self.active_provider = provider;
    }

    /// Returns the displayed model, if one was selected.
    #[must_use]
    pub fn active_model(&self) -> Option<&str> {
        self.active_model.as_deref()
    }

    /// Returns the displayed provider, if one is known.
    #[must_use]
    pub fn active_provider(&self) -> Option<&str> {
        self.active_provider.as_deref()
    }

    /// Installs the composer mode list (built-ins plus configuration
    /// customs) and selects `default_id`, falling back to `build` and then
    /// to the first mode so an unconfigured or dangling default can never
    /// leave the composer modeless.
    pub fn set_modes(&mut self, modes: Vec<AgentMode>, default_id: Option<&str>) {
        self.modes = modes;
        if self.modes.is_empty() {
            self.modes = vec![AgentMode::plan(), AgentMode::build()];
        }
        self.mode_index = default_id
            .and_then(|id| self.modes.iter().position(|mode| mode.id == id))
            .or_else(|| self.modes.iter().position(|mode| mode.id == MODE_BUILD))
            .unwrap_or(0);
    }

    /// Returns the active composer mode.
    #[must_use]
    pub fn mode(&self) -> &AgentMode {
        self.modes.get(self.mode_index).unwrap_or(&self.modes[0])
    }

    /// Advances the composer to the next mode, wrapping around. Returns the
    /// newly active mode id for notices and tests.
    pub fn cycle_mode(&mut self) -> String {
        if !self.modes.is_empty() {
            self.mode_index = (self.mode_index + 1) % self.modes.len();
        }
        self.mode().id.clone()
    }

    /// Returns the latest usage counters observed on the live run.
    #[must_use]
    pub fn last_usage(&self) -> Option<Usage> {
        self.last_usage
    }

    /// Applies one runtime event to presentation state. Returns false without
    /// mutating state for stale-run updates, duplicate or out-of-order
    /// sequence numbers on the live run, and any event after that run's
    /// terminal outcome (the lifecycle cannot reopen). Rejected events are
    /// counted in [`AppState::stale_rejected`] or [`AppState::seq_rejected`].
    pub fn apply_event(&mut self, event: &RunEvent) -> bool {
        if self.active_run.as_ref() == Some(event.run()) {
            if self.finished.is_some() {
                // Terminal already recorded: no late, replayed, or duplicate
                // event may reopen or extend this run.
                self.seq_rejected += 1;
                return false;
            }
            if matches!(event.payload(), EventPayload::RunStarted { .. }) {
                // Exactly one RunStarted per run: a second one, even with a
                // higher sequence, is a duplicate lifecycle event and must
                // not reopen or re-announce the run.
                self.seq_rejected += 1;
                return false;
            }
            if self.last_seq.is_some_and(|last| event.seq() <= last) {
                self.seq_rejected += 1;
                return false;
            }
        } else {
            let adopts = matches!(event.payload(), EventPayload::RunStarted { .. })
                && (self.active_run.is_none() || self.finished.is_some());
            if !adopts {
                self.stale_rejected += 1;
                return false;
            }
            // A finished run is replaced by the next accepted RunStarted; a
            // live run is never displaced.
            self.active_run = Some(event.run().clone());
            self.last_seq = None;
            self.finished = None;
            self.resolve_approval();
        }
        self.last_seq = Some(event.seq());
        // Accepted content can move rows under a live highlight, so a
        // drag selection never survives a new event.
        self.clear_text_selection();
        match event.payload() {
            EventPayload::RunStarted { .. } => {
                self.streaming = true;
                self.finished = None;
                self.last_usage = None;
                self.status = "running".to_owned();
                self.push_system(format!("run {} started", event.run().as_str()));
            }
            EventPayload::AssistantTextDelta(fragment) => {
                self.streaming = true;
                let clean = sanitize(&fragment.text);
                if !clean.is_empty() {
                    let key = StreamKey::Assistant {
                        turn: fragment.turn.clone(),
                        item: fragment.item_key.clone(),
                    };
                    self.push_stream_fragment(EntryKind::Assistant, "assistant", key, &clean);
                }
            }
            EventPayload::ToolCallPreview { item_key } => {
                self.push_tool(
                    format!("preview {item_key}"),
                    vec!["proposed call (not executable, never authorizes)".to_owned()],
                );
            }
            EventPayload::ApprovalRequired(notice) => {
                let (summary, summary_cut) =
                    bounded_text(&notice.summary, MAX_APPROVAL_FIELD_BYTES);
                let (scope, scope_cut) =
                    bounded_text(&notice.scope_summary, MAX_APPROVAL_FIELD_BYTES);
                let (args, args_cut) =
                    bounded_optional_text(notice.args_preview.as_deref(), MAX_APPROVAL_FIELD_BYTES);
                self.pending_approval = Some(PendingApprovalCard {
                    approval: notice.approval.clone(),
                    call: notice.call.clone(),
                    summary: summary.clone(),
                    scope_summary: scope,
                    expires_at_elapsed: notice.expires_at_elapsed,
                });
                self.approval_args_preview = args;
                self.approval_geometry = None;
                self.approval_field_truncated = summary_cut || scope_cut || args_cut;
                self.reset_approval_detail();
                self.push_tool(
                    format!("approval requested for {}", notice.call.as_str()),
                    vec![summary],
                );
                self.status = "awaiting approval".to_owned();
            }
            EventPayload::ToolStarted(info) => {
                self.streaming = true;
                self.status = "executing tool".to_owned();
                self.push_tool(
                    format!("tool {} started", info.call.as_str()),
                    vec!["authorized call entered execution".to_owned()],
                );
            }
            EventPayload::ToolOutput(progress) => {
                let clean = sanitize(&progress.preview);
                let key = StreamKey::ToolOutput {
                    call: progress.call.clone(),
                };
                if clean.is_empty() {
                    if progress.truncated {
                        self.ensure_stream_entry(
                            EntryKind::Tool,
                            &format!("tool {} output", progress.call.as_str()),
                            key,
                        );
                        self.mutate_tail(Entry::mark_truncated);
                    }
                } else {
                    self.push_stream_fragment(
                        EntryKind::Tool,
                        &format!("tool {} output", progress.call.as_str()),
                        key,
                        &clean,
                    );
                    if progress.truncated {
                        self.mutate_tail(Entry::mark_truncated);
                    }
                }
            }
            EventPayload::ToolFinished(info) => {
                self.push_tool(
                    format!("tool {} finished", info.call.as_str()),
                    vec![format!(
                        "status={:?} effect={:?} evidence={:?}{} {}",
                        info.outcome.status(),
                        info.outcome.effect(),
                        info.outcome.evidence(),
                        if info.outcome.is_truncated() {
                            " truncated"
                        } else {
                            ""
                        },
                        sanitize(info.outcome.content()),
                    )],
                );
                if self
                    .pending_approval
                    .as_ref()
                    .is_some_and(|card| card.call == info.call)
                {
                    self.resolve_approval();
                }
                self.status = "running".to_owned();
            }
            EventPayload::UsageUpdated(usage) => {
                self.last_usage = Some(*usage);
                self.status = "running".to_owned();
            }
            EventPayload::RunFinished(finished) => {
                self.streaming = false;
                self.finished = Some(finished.outcome());
                self.resolve_approval();
                self.status = format!("finished: {:?}", finished.outcome());
                // Surface the typed failure on the transcript: without it a
                // failed run shows only its outcome and the next debug step
                // is a guessing game. The message is static by construction
                // (the core marker net rejects anything else), so displaying
                // it echoes no values; sanitize anyway for terminal safety.
                let detail = finished.error().map_or_else(String::new, |error| {
                    format!(
                        " error={:?} {}",
                        error.category(),
                        sanitize(error.message())
                    )
                });
                self.push_system(format!(
                    "run {} finished: {:?} (ephemeral record){detail}",
                    event.run().as_str(),
                    finished.outcome()
                ));
            }
        }
        true
    }

    /// Drops the oldest retained entries until both the entry count and the
    /// aggregate byte budgets hold.
    fn trim_retention(&mut self) {
        while self.entries.len() > MAX_RETAINED_ENTRIES
            || (self.retained_bytes > MAX_RETAINED_BYTES && self.entries.len() > 1)
        {
            let Some(entry) = self.entries.pop_front() else {
                break;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(entry.retained_bytes());
            self.dropped_entries += 1;
            if let Some(selected) = self.selected.as_mut() {
                *selected = selected.saturating_sub(1);
            }
        }
    }

    fn push_entry(&mut self, entry: Entry) {
        self.retained_bytes += entry.retained_bytes();
        self.entries.push_back(entry);
        self.trim_retention();
        if self.scrollback == 0 {
            self.selected = None;
        }
    }

    /// Mutates the tail entry and re-accounts its retained bytes.
    fn mutate_tail(&mut self, change: impl FnOnce(&mut Entry)) {
        let delta = {
            let tail = self.entries.back_mut().expect("tail entry exists");
            let before = tail.retained_bytes();
            change(tail);
            tail.retained_bytes() - before
        };
        self.retained_bytes += delta;
        self.trim_retention();
    }

    fn ensure_stream_entry(&mut self, kind: EntryKind, title: &str, stream: StreamKey) {
        let adjacent = self.entries.back().is_some_and(|tail| {
            tail.kind == kind && !tail.folded && tail.stream.as_ref() == Some(&stream)
        });
        if !adjacent {
            self.push_entry(Entry::new_stream(kind, title, stream));
        }
    }

    /// Appends a fragment to the matching open stream entry, or starts a new
    /// entry when the previous presented entry is not the same identity.
    fn push_stream_fragment(
        &mut self,
        kind: EntryKind,
        title: &str,
        stream: StreamKey,
        text: &str,
    ) {
        self.ensure_stream_entry(kind, title, stream);
        self.mutate_tail(|entry| entry.append_text(text));
    }

    /// Appends a finalized line, coalescing into the tail entry on
    /// kind+title match when that entry is not a stream or folded.
    #[cfg(test)]
    fn push_line(&mut self, kind: EntryKind, title: String, line: String) {
        let title = bounded_title(&title);
        let coalesce = self.entries.back().is_some_and(|tail| {
            tail.kind == kind && tail.title == title && !tail.folded && tail.stream.is_none()
        });
        if !coalesce {
            self.push_entry(Entry::new(kind, &title));
        }
        self.mutate_tail(|entry| entry.push_line(&line));
    }

    fn push_tool(&mut self, title: String, lines: Vec<String>) {
        let mut entry = Entry::new(EntryKind::Tool, &title);
        for line in lines {
            entry.push_line(&line);
        }
        self.push_entry(entry);
    }

    fn push_system(&mut self, line: String) {
        let mut entry = Entry::new(EntryKind::System, "system");
        entry.push_line(&line);
        self.push_entry(entry);
    }

    /// Records a user submission in the viewport (the runtime send itself
    /// goes through a [`nexus_core::Command`], never through this method).
    /// The sanitized input also joins the bounded recall history, and any
    /// in-progress recall is abandoned.
    pub fn record_submitted(&mut self, input: &str) {
        self.clear_text_selection();
        let clean = sanitize(input);
        let mut entry = Entry::new(EntryKind::User, "you");
        for line in clean.split('\n') {
            entry.push_line(line);
        }
        self.push_entry(entry);
        self.scrollback = 0;
        self.selected = None;
        if !clean.is_empty() && self.composer_history.last() != Some(&clean) {
            self.composer_history.push(clean);
            while self.composer_history.len() > MAX_HISTORY_ENTRIES {
                self.composer_history.remove(0);
            }
        }
        self.history_cursor = None;
        self.history_stash.clear();
    }

    /// Recalls the previous submitted input into the composer (`Up`).
    /// Returns false when there is no history to show. Manual edits abandon
    /// the recall position (see [`AppState::composer_type`]).
    pub fn recall_prev(&mut self) -> bool {
        self.clear_text_selection();
        if self.composer_history.is_empty() {
            return false;
        }
        let cursor = match self.history_cursor {
            None => {
                self.history_stash = std::mem::take(&mut self.composer);
                self.composer_history.len() - 1
            }
            Some(index) => index.saturating_sub(1),
        };
        self.history_cursor = Some(cursor);
        self.composer = self.composer_history[cursor].clone();
        true
    }

    /// Recalls the next submitted input (`Down`); walking past the newest
    /// entry restores the stashed live draft. Returns false when no recall
    /// is in progress.
    pub fn recall_next(&mut self) -> bool {
        self.clear_text_selection();
        let Some(cursor) = self.history_cursor else {
            return false;
        };
        if cursor + 1 < self.composer_history.len() {
            self.history_cursor = Some(cursor + 1);
            self.composer = self.composer_history[cursor + 1].clone();
        } else {
            self.history_cursor = None;
            self.composer = std::mem::take(&mut self.history_stash);
        }
        true
    }

    /// Total wrapped height of all retained entries at `width`, using cached
    /// per-entry heights so a redraw never re-measures unchanged entries.
    #[must_use]
    pub fn total_height(&self, width: usize) -> usize {
        let width = width.max(1);
        self.entries
            .iter()
            .map(|entry| entry.wrapped_len(width))
            .sum()
    }

    /// True when the frame must reserve a row for the truncation/scrollback
    /// indicator: dropped entries exist or the window hides lines above it.
    #[must_use]
    pub fn truncation_indicator(&self, width: usize, height: usize) -> bool {
        self.dropped_entries > 0
            || self.total_height(width) > height.max(1).saturating_add(self.scrollback)
    }

    /// Bottom-anchored visible window. Only entries intersecting the window
    /// are measured (cached) and only the visible slice of each is
    /// materialized; older entries are counted, never rendered.
    #[must_use]
    pub fn visible_lines(&self, width: usize, height: usize) -> VisibleView {
        let width = width.max(1);
        let height = height.max(1);
        let total = self.total_height(width);
        let need = height.saturating_add(self.scrollback);
        // First wrapped line index of the window; everything before it is
        // hidden above the viewport.
        let window_start = total.saturating_sub(need);
        let mut hidden_above = window_start;
        let mut lines: Vec<String> = Vec::with_capacity(height.min(need));
        let mut styles: Vec<Vec<StyledRun>> = Vec::with_capacity(height.min(need));
        let mut cursor = 0usize;
        for entry in &self.entries {
            if lines.len() >= need {
                break;
            }
            let entry_height = entry.wrapped_len(width);
            let entry_start = cursor;
            cursor += entry_height;
            if cursor <= window_start {
                continue;
            }
            let skip = window_start.saturating_sub(entry_start);
            let take = need - lines.len();
            entry.render_range(width, skip, take, &mut lines, &mut styles);
        }
        // Drop the below-the-fold scrollback, then cap to the body height.
        // Both vecs stay in lockstep: every slice applies to the pair.
        if self.scrollback >= lines.len() {
            lines.clear();
            styles.clear();
        } else {
            lines.truncate(lines.len() - self.scrollback);
            styles.truncate(lines.len());
        }
        if lines.len() > height {
            let excess = lines.len() - height;
            lines.drain(..excess);
            styles.drain(..excess);
            hidden_above += excess;
        }
        debug_assert_eq!(
            lines.len(),
            styles.len(),
            "styles ride alongside lines through every window slice"
        );
        VisibleView {
            lines,
            styles,
            hidden_above,
            retention_truncated: self.dropped_entries > 0,
        }
    }

    /// Moves the viewport selection; the live tail re-follows on new output.
    pub fn move_selection(&mut self, delta: isize) {
        self.clear_text_selection();
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() - 1;
        let next = match self.selected {
            Some(index) => (index as isize + delta).clamp(0, last as isize) as usize,
            None => {
                if delta < 0 {
                    last.saturating_sub(delta.unsigned_abs() - 1)
                } else {
                    last
                }
            }
        };
        self.selected = Some(next);
        self.ensure_visible();
    }

    /// Keeps the selected entry inside the viewport window. Without a
    /// selection the user's scroll position is preserved (only clamped), so
    /// PageUp survives every subsequent frame's viewport recalculation.
    fn ensure_visible(&mut self) {
        let (width, height) = (self.viewport.0.max(1), self.viewport.1.max(1));
        let total = self.total_height(width);
        let max_scroll = total.saturating_sub(height.min(total));
        let Some(selected) = self.selected else {
            self.scrollback = self.scrollback.min(max_scroll);
            return;
        };
        if total <= height {
            self.scrollback = 0;
            return;
        }
        let mut start = 0;
        let mut selected_range = (0, 0);
        for (index, entry) in self.entries.iter().enumerate() {
            let len = entry.wrapped_len(width);
            if index == selected {
                selected_range = (start, start + len);
            }
            start += len;
        }
        let (sel_start, sel_end) = selected_range;
        let bottom_hidden = total.saturating_sub(sel_end);
        if bottom_hidden < self.scrollback {
            // Selection moved down past the window: follow it.
            self.scrollback = bottom_hidden;
        } else if sel_start < total.saturating_sub(self.scrollback.saturating_add(height)) {
            // Selection moved up past the window: pin its top.
            self.scrollback = total.saturating_sub(sel_start.saturating_add(height));
        }
        self.scrollback = self.scrollback.min(max_scroll);
    }

    /// Toggles folding for entry `index` (0 is the oldest). Returns false
    /// for out-of-range indices.
    pub fn toggle_fold(&mut self, index: usize) -> bool {
        match self.entries.get_mut(index) {
            Some(entry) => {
                entry.folded = !entry.folded;
                self.clear_text_selection();
                true
            }
            None => false,
        }
    }

    /// Folds every retained entry; manual folds are separate per entry.
    pub fn fold_all(&mut self) {
        self.clear_text_selection();
        for entry in &mut self.entries {
            entry.folded = true;
        }
    }

    /// Unfolds every retained entry.
    pub fn unfold_all(&mut self) {
        self.clear_text_selection();
        for entry in &mut self.entries {
            entry.folded = false;
        }
    }

    /// Scrolls the viewport up by `lines` wrapped lines, clamped to the
    /// available history.
    pub fn scroll_up(&mut self, lines: usize) {
        self.clear_text_selection();
        let (width, height) = (self.viewport.0.max(1), self.viewport.1.max(1));
        let total = self.total_height(width);
        let max = total.saturating_sub(height.min(total));
        self.scrollback = self.scrollback.saturating_add(lines).min(max);
    }

    /// Scrolls the viewport down by `lines` wrapped lines.
    pub fn scroll_down(&mut self, lines: usize) {
        self.clear_text_selection();
        self.scrollback = self.scrollback.saturating_sub(lines);
        if self.scrollback == 0 {
            self.selected = None;
        }
    }

    /// Composer draft text.
    #[must_use]
    pub fn composer(&self) -> &str {
        &self.composer
    }

    /// Types one char, bounded by the command input limit. Control
    /// characters (including newline) and bidi formatting controls are
    /// rejected here as defense-in-depth for future paste paths; newline
    /// enters only through [`Self::composer_newline`], and the keyboard layer
    /// already filters these chars.
    pub fn composer_type(&mut self, char: char) {
        self.clear_text_selection();
        if char.is_control() || is_bidi_format(char) {
            return;
        }
        if self.composer.len() + char.len_utf8() <= MAX_COMPOSER_BYTES {
            self.composer.push(char);
            self.abandon_recall();
        }
    }

    /// Starts a new composer line.
    pub fn composer_newline(&mut self) {
        self.clear_text_selection();
        if self.composer.len() < MAX_COMPOSER_BYTES {
            self.composer.push('\n');
            self.abandon_recall();
        }
    }

    /// Deletes the last composer char. Returns false when already empty.
    pub fn composer_backspace(&mut self) -> bool {
        self.clear_text_selection();
        let removed = self.composer.pop().is_some();
        if removed {
            self.abandon_recall();
        }
        removed
    }

    /// A manual edit leaves recall mode: the draft is live again and the
    /// stash is dropped, so the next `Up` restarts from the newest entry.
    fn abandon_recall(&mut self) {
        self.history_cursor = None;
        self.history_stash.clear();
    }

    /// Clears the composer draft, returning the stashed text. Leaving
    /// recall mode as well, so a cleared draft never resurrects recalled
    /// history on the next key.
    pub fn composer_take(&mut self) -> String {
        self.clear_text_selection();
        self.abandon_recall();
        std::mem::take(&mut self.composer)
    }

    /// True while output streams or an approval awaits a decision.
    #[must_use]
    pub fn can_cancel(&self) -> bool {
        self.streaming || self.pending_approval.is_some()
    }

    /// True once the terminal run outcome arrived.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.finished.is_some()
    }

    /// Pending approval card, if any.
    #[must_use]
    pub fn pending_approval(&self) -> Option<&PendingApprovalCard> {
        self.pending_approval.as_ref()
    }

    /// Sanitized, bounded exact-arguments preview attached to the live
    /// notice, if the runtime published one. Kept beside the card so
    /// [`PendingApprovalCard`]'s existing public fields stay
    /// source-compatible.
    #[must_use]
    pub fn approval_args_preview(&self) -> Option<&str> {
        self.approval_args_preview.as_deref()
    }

    /// Geometry recorded by the last approval-card render, if any.
    #[must_use]
    pub fn approval_geometry(&self) -> Option<ApprovalGeometry> {
        self.approval_geometry
    }

    /// Records the geometry measured by [`mod@crate::render`]. Called by the
    /// renderer; kept public so alternative frontends report the same facts.
    pub fn set_approval_geometry(&mut self, geometry: ApprovalGeometry) {
        self.approval_geometry = Some(geometry);
    }

    /// Opens the bounded expanded detail view at its first row. This is a
    /// deliberate inspection, never a grant:
    /// [`AppState::approval_decision_allowed`] stays false until the
    /// renderer has actually displayed every detail row from the start (or
    /// the whole detail fits in one expanded view).
    pub fn inspect_approval(&mut self) {
        self.open_approval_detail();
    }

    /// Opens (or keeps open) the expanded detail view, restarting at row 0.
    /// A no-op without a live card.
    pub fn open_approval_detail(&mut self) {
        if self.pending_approval.is_none() {
            return;
        }
        if !self.approval_detail_open {
            self.approval_detail_open = true;
            self.approval_detail_scroll = 0;
            self.approval_detail_seen_through = 0;
        }
    }

    /// Closes the expanded detail view; rows already seen stay recorded.
    pub fn close_approval_detail(&mut self) {
        self.approval_detail_open = false;
    }

    /// True while the expanded detail view is open.
    #[must_use]
    pub fn approval_detail_open(&self) -> bool {
        self.approval_detail_open
    }

    /// Top detail row shown by the expanded view.
    #[must_use]
    pub fn approval_detail_scroll_position(&self) -> usize {
        self.approval_detail_scroll
    }

    /// Detail viewport height measured by the last expanded render.
    #[must_use]
    pub fn approval_detail_view_rows(&self) -> usize {
        self.approval_detail_view_rows
    }

    /// Full detail row count measured by the last expanded render.
    #[must_use]
    pub fn approval_detail_total_rows(&self) -> usize {
        self.approval_detail_total_rows
    }

    /// Scrolls the expanded detail view by `delta` rows (negative is up),
    /// clamped to the range measured by the last expanded render. No-op
    /// while the view is closed.
    pub fn approval_detail_scroll(&mut self, delta: isize) {
        if !self.approval_detail_open {
            return;
        }
        let max = self
            .approval_detail_total_rows
            .saturating_sub(self.approval_detail_view_rows.max(1));
        self.approval_detail_scroll = self
            .approval_detail_scroll
            .saturating_add_signed(delta)
            .min(max);
    }

    /// Scrolls one expanded-detail page (`direction` negative is up) using
    /// the viewport height measured by the last render. No-op while closed.
    pub fn approval_detail_page(&mut self, direction: isize) {
        if !self.approval_detail_open {
            return;
        }
        let step = self.approval_detail_view_rows.max(1) as isize;
        self.approval_detail_scroll(direction.signum().saturating_mul(step));
    }

    /// True once every rendered detail row has been seen contiguously from
    /// the first row. A jump past hidden rows never satisfies this.
    #[must_use]
    pub fn approval_detail_seen_all(&self) -> bool {
        self.approval_detail_total_rows > 0
            && self.approval_detail_seen_through >= self.approval_detail_total_rows
    }

    /// Records one expanded-detail frame: the visible row range and the full
    /// row count. Called by [`mod@crate::render`] after it materialized the full
    /// (untruncated) fields. Seen coverage advances only while the view
    /// starts at or before the last seen row, so paging through the detail
    /// is deliberate and jumping over hidden rows cannot unlock the gate.
    pub fn record_approval_detail_view(
        &mut self,
        scroll: usize,
        view_rows: usize,
        total_rows: usize,
    ) {
        self.approval_detail_total_rows = total_rows;
        self.approval_detail_view_rows = view_rows.max(1);
        let max = total_rows.saturating_sub(self.approval_detail_view_rows);
        self.approval_detail_scroll = scroll.min(max);
        let start = self.approval_detail_scroll;
        let end = start
            .saturating_add(self.approval_detail_view_rows)
            .min(total_rows);
        if start <= self.approval_detail_seen_through {
            self.approval_detail_seen_through = self.approval_detail_seen_through.max(end);
        }
    }

    /// True only when the live card was measured and the complete detail was
    /// actually displayed: either the compact card fit every line, or the
    /// expanded view displayed every row contiguously from the first. A
    /// field cut at storage never unlocks. Key handling must check this
    /// before issuing an approve command; the runtime still rechecks the
    /// grant at dispatch.
    #[must_use]
    pub fn approval_decision_allowed(&self) -> bool {
        self.pending_approval.is_some()
            && !self.approval_field_truncated
            && (self
                .approval_geometry
                .is_some_and(|geometry| !geometry.clipped)
                || self.approval_detail_seen_all())
    }

    /// True when a card field was cut at the presentation bound, in which
    /// case the full detail is unavailable and approval stays locked.
    #[must_use]
    pub fn approval_detail_truncated(&self) -> bool {
        self.approval_field_truncated
    }

    /// Owning run, once a `RunStarted` event arrived.
    #[must_use]
    pub fn active_run(&self) -> Option<&RunId> {
        self.active_run.as_ref()
    }

    /// Clears the approval card after a decision command is issued. The
    /// decision itself travels as a runtime command; this only updates the
    /// card so a repeated keypress cannot re-issue it.
    pub fn resolve_approval(&mut self) {
        self.pending_approval = None;
        self.approval_args_preview = None;
        self.approval_geometry = None;
        self.approval_field_truncated = false;
        self.reset_approval_detail();
    }

    fn reset_approval_detail(&mut self) {
        self.approval_detail_open = false;
        self.approval_detail_scroll = 0;
        self.approval_detail_seen_through = 0;
        self.approval_detail_total_rows = 0;
        self.approval_detail_view_rows = 0;
    }

    /// Current status line text.
    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Records a frontend notice (replies, errors) as a system entry.
    pub fn notice(&mut self, line: &str) {
        let clean = sanitize(line);
        let mut entry = Entry::new(EntryKind::System, "system");
        for part in clean.split('\n') {
            entry.push_line(part);
        }
        self.push_entry(entry);
    }

    /// Caches the last viewport geometry for selection math. Preserves an
    /// existing scroll position when nothing is selected.
    pub fn set_viewport(&mut self, width: usize, height: usize) {
        self.viewport = (width.max(1), height.max(1));
        let (width, height) = self.viewport;
        let total = self.total_height(width);
        let max_scroll = total.saturating_sub(height.min(total));
        self.scrollback = self.scrollback.min(max_scroll);
        self.ensure_visible();
    }

    /// Retained entry count (bounded by [`MAX_RETAINED_ENTRIES`]).
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Aggregate retained presentation bytes (bounded by
    /// [`MAX_RETAINED_BYTES`]).
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// Borrows one retained entry by index (0 is the oldest).
    #[must_use]
    pub fn entry(&self, index: usize) -> Option<&Entry> {
        self.entries.get(index)
    }

    /// Selected entry index, if any.
    #[must_use]
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// Starts a body selection at one window point, replacing any
    /// previous selection outright.
    pub fn begin_body_selection(&mut self, row: usize, col: usize) {
        self.text_selection = Some(TextSelection::Body {
            anchor: SelPoint { row, col },
            head: SelPoint { row, col },
        });
    }

    /// Extends the live body selection to a new head point. A no-op
    /// unless a body selection is in progress.
    pub fn extend_body_selection(&mut self, row: usize, col: usize) {
        if let Some(TextSelection::Body { head, .. }) = self.text_selection.as_mut() {
            *head = SelPoint { row, col };
        }
    }

    /// Starts a composer selection at one draft character offset.
    pub fn begin_composer_selection(&mut self, offset: usize) {
        self.text_selection = Some(TextSelection::Composer {
            anchor: offset,
            head: offset,
        });
    }

    /// Extends the live composer selection. A no-op unless a composer
    /// selection is in progress.
    pub fn extend_composer_selection(&mut self, offset: usize) {
        if let Some(TextSelection::Composer { head, .. }) = self.text_selection.as_mut() {
            *head = offset;
        }
    }

    /// Clears any drag selection. Called wherever rows or the draft can
    /// move: scrolling, folding, edits, submits, and new events.
    pub fn clear_text_selection(&mut self) {
        self.text_selection = None;
    }

    /// Borrows the live drag selection, if any.
    #[must_use]
    pub fn text_selection(&self) -> Option<TextSelection> {
        self.text_selection
    }

    /// Normalized body selection as `(row, start, len)` per covered
    /// window row, columns clamped to each line. Covered blank rows
    /// keep a zero-length entry so yanked text preserves line breaks.
    /// `None` when no body selection is live. The caller supplies the
    /// same window rows the highlight was drawn from.
    #[must_use]
    pub fn selection_body_rows(&self, lines: &[String]) -> Option<Vec<(usize, usize, usize)>> {
        let (anchor, head) = match self.text_selection {
            Some(TextSelection::Body { anchor, head }) => (anchor, head),
            _ => return None,
        };
        let (first, last) = if (anchor.row, anchor.col) <= (head.row, head.col) {
            (anchor, head)
        } else {
            (head, anchor)
        };
        if lines.is_empty() || first.row >= lines.len() {
            return None;
        }
        let mut rows = Vec::new();
        let last_row = last.row.min(lines.len() - 1);
        for (row, line) in lines.iter().enumerate().take(last_row + 1).skip(first.row) {
            let len = line.chars().count();
            let (start, end) = if first.row == last.row {
                (first.col.min(len), last.col.min(len))
            } else if row == first.row {
                (first.col.min(len), len)
            } else if row == last.row {
                (0, last.col.min(len))
            } else {
                (0, len)
            };
            let (start, end) = (start.min(end), start.max(end));
            rows.push((row, start, end - start));
        }
        Some(rows)
    }

    /// Selected body text for the given window rows, joined as shown.
    /// `None` when no body selection is live or no row is covered; an
    /// all-blank cover yields an empty string, which callers treat as
    /// nothing to copy.
    #[must_use]
    pub fn body_selection_text(&self, lines: &[String]) -> Option<String> {
        let rows = self.selection_body_rows(lines)?;
        if rows.is_empty() {
            return None;
        }
        let mut out = String::new();
        for (index, (row, start, len)) in rows.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            let text = lines.get(*row).map(String::as_str).unwrap_or("");
            out.extend(text.chars().skip(*start).take(*len));
        }
        Some(out)
    }

    /// Normalized composer selection as `(start, len)` draft character
    /// offsets, clamped to the draft. Empty and non-composer selections
    /// yield `None`.
    #[must_use]
    pub fn composer_selection_range(&self) -> Option<(usize, usize)> {
        let (anchor, head) = match self.text_selection {
            Some(TextSelection::Composer { anchor, head }) => (anchor, head),
            _ => return None,
        };
        let len = self.composer.chars().count();
        let (start, end) = (anchor.min(head).min(len), anchor.max(head).min(len));
        (start < end).then_some((start, end - start))
    }

    /// Selected composer text for the normalized range.
    #[must_use]
    pub fn composer_selection_text(&self) -> Option<String> {
        let (start, len) = self.composer_selection_range()?;
        Some(self.composer.chars().skip(start).take(len).collect())
    }

    /// Lines hidden below the viewport bottom.
    /// Shows a header toast for `ttl`. Replaces any live toast.
    pub fn set_toast(&mut self, text: impl Into<String>, ttl: Duration) {
        self.toast = Some(Toast {
            text: text.into(),
            deadline: Instant::now() + ttl,
        });
    }

    /// Borrows the live toast text, if one is showing. Expired toasts
    /// read as absent without needing a mutation.
    #[must_use]
    pub fn toast(&self) -> Option<&str> {
        self.toast.as_ref().and_then(|toast| {
            if Instant::now() < toast.deadline {
                Some(toast.text.as_str())
            } else {
                None
            }
        })
    }

    /// Clears a just-expired toast, reporting whether a redraw is due.
    /// The tick calls this so the confirmation vanishes on its own.
    pub fn poll_toast_expired(&mut self) -> bool {
        let expired = matches!(&self.toast, Some(toast) if Instant::now() >= toast.deadline);
        if expired {
            self.toast = None;
        }
        expired
    }

    #[must_use]
    pub fn scrollback(&self) -> usize {
        self.scrollback
    }

    /// Last viewport body height in wrapped lines, for page scrolling.
    #[must_use]
    pub fn viewport_height(&self) -> usize {
        self.viewport.1
    }

    /// Last viewport body width in columns, for selection math.
    #[must_use]
    pub fn viewport_width(&self) -> usize {
        self.viewport.0
    }

    /// Records the pointer geometry the renderer just laid out.
    pub fn set_pointer_geometry(&mut self, geometry: PointerGeometry) {
        self.pointer_geometry = geometry;
    }

    /// Returns the last recorded pointer geometry.
    #[must_use]
    pub fn pointer_geometry(&self) -> PointerGeometry {
        self.pointer_geometry
    }

    /// Full retained transcript (titles plus sanitized body lines) for the
    /// non-terminal fallback. Bounded by [`MAX_RETAINED_BYTES`] and
    /// [`MAX_RETAINED_ENTRIES`].
    #[must_use]
    pub fn transcript(&self) -> Vec<String> {
        let mut out = Vec::new();
        for entry in &self.entries {
            out.push(format!("[{:?}] {}", entry.kind, entry.title));
            out.extend(entry.lines.iter().cloned());
            if entry.truncated {
                out.push("[output truncated to presentation bound]".to_owned());
            }
        }
        out
    }

    /// Last applied per-run sequence number, if any event arrived.
    #[must_use]
    pub fn last_seq(&self) -> Option<u64> {
        self.last_seq
    }
}

/// Event-driven redraw gate with a maximum rate: stream deltas call
/// [`RefreshGate::request`], and [`RefreshGate::ready`] stays false until
/// the frame budget elapsed, so saturated output coalesces instead of
/// forcing a full-history relayout per delta.
#[derive(Debug)]
pub struct RefreshGate {
    min_interval: Duration,
    last_draw: Option<Instant>,
    pending: bool,
}

impl RefreshGate {
    /// Builds a gate redrawing at most once per `min_interval`.
    #[must_use]
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            last_draw: None,
            pending: false,
        }
    }

    /// Builds the M0-test gate ([`MAX_REDRAW_INTERVAL`] between frames).
    #[must_use]
    pub fn m0_test() -> Self {
        Self::new(MAX_REDRAW_INTERVAL)
    }

    /// Marks presentation state dirty after an event or keypress.
    pub fn request(&mut self) {
        self.pending = true;
    }

    /// True when a redraw is both requested and within budget.
    pub fn ready(&mut self, now: Instant) -> bool {
        if !self.pending {
            return false;
        }
        !matches!(self.last_draw, Some(last) if now.duration_since(last) < self.min_interval)
    }

    /// Records a completed redraw.
    pub fn mark_drawn(&mut self, now: Instant) {
        self.last_draw = Some(now);
        self.pending = false;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    #[test]
    fn toast_shows_while_live_and_clears_on_expiry() {
        let mut state = AppState::new();
        assert_eq!(state.toast(), None);
        assert!(!state.poll_toast_expired(), "nothing to clear");
        state.set_toast("copied 3 chars", Duration::from_secs(3600));
        assert_eq!(state.toast(), Some("copied 3 chars"));
        assert!(!state.poll_toast_expired(), "a live toast stays");
        assert_eq!(state.toast(), Some("copied 3 chars"));
        state.set_toast("gone", Duration::ZERO);
        assert_eq!(state.toast(), None, "expired reads as absent");
        assert!(state.poll_toast_expired(), "expiry reports once");
        assert!(!state.poll_toast_expired(), "second poll is quiet");
        assert_eq!(state.toast(), None);
    }

    use super::*;
    use nexus_core::{ApprovalNotice, RequestId, SessionId};

    #[test]
    fn approval_fields_visibly_escape_invisible_characters_before_bounding() {
        let (display, cut) = bounded_text("in\u{200B}voice.txt", 128);
        assert_eq!(display, "in\\u{200B}voice.txt");
        assert!(!cut);
        let (_, cut) = bounded_text("\u{200B}", 3);
        assert!(
            cut,
            "escaped display size participates in the approval bound"
        );
    }

    fn session_run() -> (SessionId, RunId) {
        (
            SessionId::new("sess-1").expect("valid"),
            RunId::new("run-1").expect("valid"),
        )
    }

    fn started(seq: u64) -> RunEvent {
        let (session, run) = session_run();
        RunEvent::new(
            session,
            run,
            seq,
            EventPayload::RunStarted {
                request: RequestId::new("req-1").expect("valid"),
            },
        )
    }

    fn started_run(run: &str, seq: u64) -> RunEvent {
        let (session, _) = session_run();
        RunEvent::new(
            session,
            RunId::new(run).expect("valid"),
            seq,
            EventPayload::RunStarted {
                request: RequestId::new("req-1").expect("valid"),
            },
        )
    }

    fn text(seq: u64, text: &str) -> RunEvent {
        text_item(seq, "t1-0", "item-0", text)
    }

    fn text_item(seq: u64, turn: &str, item: &str, text: &str) -> RunEvent {
        use nexus_core::{AssistantText, TurnId};
        let (session, run) = session_run();
        RunEvent::new(
            session,
            run,
            seq,
            EventPayload::AssistantTextDelta(
                AssistantText::new(TurnId::new(turn).expect("valid"), item, text)
                    .expect("fragment builds"),
            ),
        )
    }

    fn finished(seq: u64) -> RunEvent {
        use nexus_core::{PersistenceState, RunFinished};
        let (session, run) = session_run();
        RunEvent::new(
            session,
            run,
            seq,
            EventPayload::RunFinished(
                RunFinished::new(RunOutcome::Completed, PersistenceState::Ephemeral, None)
                    .expect("terminal record builds"),
            ),
        )
    }

    fn approval(seq: u64, summary: &str, scope: &str) -> RunEvent {
        use nexus_core::{ApprovalId, CallId};
        let (session, run) = session_run();
        let notice = ApprovalNotice::new(
            ApprovalId::new("a1-0").expect("valid"),
            CallId::new("c1-0").expect("valid"),
            summary,
            scope,
            Duration::from_secs(120),
        )
        .expect("notice builds");
        RunEvent::new(session, run, seq, EventPayload::ApprovalRequired(notice))
    }

    fn finish_state(state: &mut AppState) {
        assert!(state.apply_event(&started(0)));
        assert!(state.apply_event(&text(1, "live")));
        assert!(state.apply_event(&finished(2)));
    }

    #[test]
    fn failed_finish_names_its_error_while_clean_finish_stays_bare() {
        use nexus_core::{AgentError, ErrorCategory, PersistenceState, RetryGuidance, RunFinished};
        let (session, run) = session_run();
        // Clean finish: the historical bare line, no error suffix.
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        assert!(state.apply_event(&text(1, "live")));
        assert!(state.apply_event(&finished(2)));
        let lines: Vec<String> = state.transcript();
        assert!(
            lines.iter().any(
                |line| line.contains("finished: Completed (ephemeral record)")
                    && !line.contains("error=")
            ),
            "a clean finish carries no error detail: {lines:?}"
        );
        // Failed finish with a typed error: category plus static message.
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        let error = AgentError::new(
            ErrorCategory::Protocol,
            "provider reply tool call arguments are invalid",
            RetryGuidance::DoNotRetry,
        )
        .expect("static diagnostic builds");
        let failed = RunEvent::new(
            session,
            run,
            1,
            EventPayload::RunFinished(
                RunFinished::new(RunOutcome::Failed, PersistenceState::Ephemeral, None)
                    .expect("terminal record builds")
                    .with_error(error),
            ),
        );
        assert!(state.apply_event(&failed));
        let lines: Vec<String> = state.transcript();
        assert!(
            lines
                .iter()
                .any(|line| line.contains("finished: Failed (ephemeral record)")
                    && line.contains("error=Protocol")
                    && line.contains("provider reply tool call arguments are invalid")),
            "a failed run names its error on the transcript: {lines:?}"
        );
    }

    #[test]
    fn retained_entries_are_bounded_with_visible_drops() {
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        for index in 0..(MAX_RETAINED_ENTRIES + 10) {
            state.push_tool(format!("tool {index}"), vec!["x".to_owned()]);
        }
        assert_eq!(state.entry_count(), MAX_RETAINED_ENTRIES);
        assert!(state.dropped_entries > 0);
        let view = state.visible_lines(80, 20);
        assert!(view.retention_truncated);
    }

    #[test]
    fn oversized_entry_text_sets_truncation_flag_with_safe_prefix() {
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        state.push_line(
            EntryKind::Assistant,
            "assistant".to_owned(),
            "x".repeat(MAX_ENTRY_BYTES + 1),
        );
        let entry = state.entry(state.entry_count() - 1).expect("entry exists");
        assert!(entry.truncated);
        assert!(entry.bytes <= MAX_ENTRY_BYTES);
        let retained: usize = entry.lines.iter().map(String::len).sum();
        assert!(retained > 0, "a safe prefix is kept, not dropped wholesale");
    }

    #[test]
    fn empty_lines_and_titles_charge_overhead_against_the_entry_bound() {
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        for _ in 0..(MAX_ENTRY_BYTES / LINE_OVERHEAD + 10) {
            state.push_line(EntryKind::Assistant, "assistant".to_owned(), String::new());
        }
        let entry = state.entry(state.entry_count() - 1).expect("entry exists");
        assert!(entry.truncated, "empty-line overhead exhausts the bound");
        assert!(entry.bytes <= MAX_ENTRY_BYTES);
        let line_overhead = entry.lines.len() * LINE_OVERHEAD;
        assert!(
            line_overhead + entry.title.len() <= MAX_ENTRY_BYTES,
            "per-line overhead is charged, not just text bytes"
        );
        assert_eq!(
            entry.lines.len(),
            (MAX_ENTRY_BYTES - entry.title.len()) / LINE_OVERHEAD,
            "empty lines are capped by overhead, not unbounded"
        );
    }

    #[test]
    fn aggregate_retained_bytes_are_bounded_by_dropping_oldest() {
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        let chunk = "x".repeat(MAX_ENTRY_BYTES / 2);
        for index in 0..(MAX_RETAINED_BYTES / (MAX_ENTRY_BYTES / 2) + 8) {
            state.push_tool(format!("tool {index}"), vec![chunk.clone()]);
        }
        assert!(state.retained_bytes() <= MAX_RETAINED_BYTES);
        assert!(state.dropped_entries > 0);
        assert!(
            state.entry_count() < MAX_RETAINED_ENTRIES,
            "aggregate budget drops before the entry count cap"
        );
    }

    #[test]
    fn hostile_titles_are_sanitized_bounded_and_flattened_on_every_insertion() {
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        let hostile = format!("preview \x1b[2J{}", "t".repeat(MAX_TITLE_BYTES * 4));
        let event = {
            let (session, run) = session_run();
            RunEvent::new(
                session,
                run,
                1,
                EventPayload::ToolCallPreview {
                    item_key: hostile.clone(),
                },
            )
        };
        assert!(state.apply_event(&event));
        state.notice(&format!(
            "notice \x1b[31m{}",
            "n".repeat(MAX_TITLE_BYTES * 2)
        ));
        for index in 0..state.entry_count() {
            let entry = state.entry(index).expect("entry exists");
            assert!(entry.title.len() <= MAX_TITLE_BYTES, "title is bounded");
            assert!(!entry.title.contains('\x1b'), "title cannot inject escapes");
            assert!(!entry.title.contains('\n'), "title is one line");
        }
        let transcript = state.transcript();
        assert!(
            transcript.iter().all(|line| !line.contains('\x1b')),
            "transcript carries no terminal escapes"
        );
        assert!(
            transcript
                .iter()
                .all(|line| line.len() <= MAX_TITLE_BYTES + MAX_ENTRY_BYTES),
            "transcript lines stay within presentation bounds"
        );
    }

    #[test]
    fn adjacent_fragments_join_into_one_line_and_newlines_stay_separate() {
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        assert!(state.apply_event(&text(1, "he")));
        assert!(state.apply_event(&text(2, "llo")));
        let first = state.entry(state.entry_count() - 1).expect("entry exists");
        assert_eq!(first.lines, vec!["hello".to_owned()]);
        assert!(state.apply_event(&text(3, "\nworld")));
        assert!(state.apply_event(&text(4, "!")));
        let joined = state.entry(state.entry_count() - 1).expect("entry exists");
        assert_eq!(joined.lines, vec!["hello".to_owned(), "world!".to_owned()]);
        assert!(state.apply_event(&text(5, "\ntail\n")));
        let trailing = state.entry(state.entry_count() - 1).expect("entry exists");
        assert_eq!(
            trailing.lines,
            vec!["hello".to_owned(), "world!".to_owned(), "tail".to_owned()],
            "a trailing newline closes the line without a stray blank"
        );
        assert!(!trailing.open_line, "the next fragment starts a fresh line");
        assert!(state.apply_event(&text(6, "next")));
        let continued = state.entry(state.entry_count() - 1).expect("entry exists");
        assert_eq!(
            continued.lines,
            vec![
                "hello".to_owned(),
                "world!".to_owned(),
                "tail".to_owned(),
                "next".to_owned()
            ]
        );
    }

    #[test]
    fn interleaved_item_identities_never_merge() {
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        assert!(state.apply_event(&text_item(1, "t1-0", "item-a", "he")));
        assert!(state.apply_event(&text_item(2, "t1-0", "item-b", "XX")));
        assert!(state.apply_event(&text_item(3, "t1-0", "item-a", "llo")));
        assert!(state.apply_event(&text_item(4, "t2-0", "item-a", "turn two")));
        let rendered: Vec<String> = state
            .visible_lines(80, 20)
            .lines
            .into_iter()
            .filter(|line| !line.starts_with('['))
            .collect();
        assert!(rendered.iter().any(|line| line.contains("he")));
        assert!(rendered.iter().any(|line| line.contains("XX")));
        assert!(rendered.iter().any(|line| line.contains("llo")));
        assert!(
            !rendered.iter().any(|line| line.contains("hello")),
            "interleaved identities must not be joined"
        );
        let last = state.entry(state.entry_count() - 1).expect("entry exists");
        assert_eq!(last.lines, vec!["turn two".to_owned()]);
    }

    #[test]
    fn simple_hello_halves_merge_even_between_status_events() {
        use nexus_core::{Usage, UsageFinality};
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        assert!(state.apply_event(&text(1, "hel")));
        let usage = {
            let (session, run) = session_run();
            RunEvent::new(
                session,
                run,
                2,
                EventPayload::UsageUpdated(Usage::new(None, None, UsageFinality::Provisional)),
            )
        };
        assert!(state.apply_event(&usage));
        assert!(state.apply_event(&text(3, "lo")));
        let entry = state.entry(state.entry_count() - 1).expect("entry exists");
        assert_eq!(entry.lines, vec!["hello".to_owned()]);
    }

    #[test]
    fn empty_fragments_are_ignored_without_fragmenting_the_stream() {
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        assert!(state.apply_event(&text(1, "")));
        assert!(state.apply_event(&text(2, "he")));
        assert!(state.apply_event(&text(3, "")));
        assert!(state.apply_event(&text(4, "llo")));
        let entry = state.entry(state.entry_count() - 1).expect("entry exists");
        assert_eq!(entry.lines, vec!["hello".to_owned()]);
        assert_eq!(state.last_seq(), Some(4), "empty fragments still advance");
    }

    #[test]
    fn truncation_never_splits_utf8_characters() {
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        // Multi-byte chars sized to overflow the remaining budget.
        let line = "é".repeat(MAX_ENTRY_BYTES);
        state.push_line(EntryKind::Assistant, "assistant".to_owned(), line);
        let entry = state.entry(state.entry_count() - 1).expect("entry exists");
        assert!(entry.truncated);
        let stored = entry.lines.join("");
        assert!(stored.chars().all(|char| char == 'é'), "no split char");
        assert!(entry.bytes <= MAX_ENTRY_BYTES);
    }

    #[test]
    fn wrapped_heights_match_rendered_lines_at_available_width() {
        let mut entry = Entry::new(EntryKind::Assistant, "assistant");
        // Body width at total width 80 is 78: 79 chars wrap to two lines,
        // plus the title row and the trailing separator row.
        entry.push_line(&"a".repeat(79));
        assert_eq!(entry.wrapped_len(80), 4);
        assert_eq!(render_entry_lines(&entry, 80).len(), 4);
        assert_eq!(entry.wrapped_len(40), 5);
        assert_eq!(render_entry_lines(&entry, 40).len(), 5);
        let mut folded = Entry::new(EntryKind::Assistant, "assistant");
        folded.push_line("body");
        folded.folded = true;
        let expected = wrapped_height(&folded_text("assistant", 1), 40) + 1;
        assert_eq!(folded.wrapped_len(40), expected);
        assert_eq!(render_entry_lines(&folded, 40).len(), expected);
        let mut truncated = Entry::new(EntryKind::Assistant, "assistant");
        truncated.push_line("body");
        truncated.truncated = true;
        let expected = 3 + wrapped_height(TRUNCATION_MARKER, 38);
        assert_eq!(truncated.wrapped_len(40), expected);
        assert_eq!(render_entry_lines(&truncated, 40).len(), expected);
        for line in render_entry_lines(&truncated, 40) {
            assert!(line.chars().count() <= 40, "rendered within the frame");
        }
    }

    #[test]
    fn page_scroll_survives_viewport_recalculation() {
        let mut state = AppState::new();
        state.set_viewport(40, 8);
        assert!(state.apply_event(&started(0)));
        for index in 0..20 {
            state.push_tool(format!("tool {index}"), vec![format!("body {index}")]);
        }
        state.scroll_up(8);
        let scrolled = state.scrollback();
        assert_eq!(scrolled, 8);
        // A redraw recalculates the viewport: the scroll position persists.
        state.set_viewport(40, 8);
        assert_eq!(state.scrollback(), scrolled);
        let view = state.visible_lines(40, 8);
        assert!(view.hidden_above > 0);
        assert_eq!(view.lines.len(), 8);
        state.scroll_down(8);
        assert_eq!(state.scrollback(), 0);
    }

    #[test]
    fn duplicate_and_out_of_order_sequences_are_rejected() {
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        assert_eq!(state.seq_rejected, 0);
        assert!(state.apply_event(&text(2, "live")), "gaps still apply");
        assert!(!state.apply_event(&text(2, "duplicate")));
        assert!(!state.apply_event(&text(1, "out of order")));
        assert!(!state.apply_event(&started(0)), "replayed lifecycle event");
        assert_eq!(state.seq_rejected, 3);
        assert_eq!(state.stale_rejected, 0);
        assert_eq!(state.last_seq(), Some(2));
        let entry = state.entry(state.entry_count() - 1).expect("entry exists");
        assert_eq!(entry.lines, vec!["live".to_owned()], "state is unchanged");
    }

    #[test]
    fn duplicate_run_started_with_higher_sequence_is_rejected() {
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        let entries = state.entry_count();
        assert!(
            !state.apply_event(&started(5)),
            "a second RunStarted is a duplicate even with a higher sequence"
        );
        assert_eq!(state.seq_rejected, 1);
        assert_eq!(state.last_seq(), Some(0), "cursor does not move");
        assert_eq!(
            state.entry_count(),
            entries,
            "no second run-started notice is recorded"
        );
        assert!(state.can_cancel(), "the run stays live and streaming");
    }

    #[test]
    fn post_terminal_events_cannot_reopen_the_run() {
        let mut state = AppState::new();
        finish_state(&mut state);
        let entries_after_finish = state.entry_count();
        assert!(state.is_finished());
        assert!(!state.can_cancel());
        assert!(!state.apply_event(&text(3, "late")), "post-final data");
        assert!(!state.apply_event(&finished(2)), "duplicate terminal");
        assert!(!state.apply_event(&started(4)), "same-run restart");
        assert_eq!(state.seq_rejected, 3);
        assert!(state.is_finished(), "terminal outcome is sticky");
        assert!(!state.can_cancel());
        assert_eq!(state.entry_count(), entries_after_finish);
        assert_eq!(state.last_seq(), Some(2), "cursor never rewinds");
    }

    #[test]
    fn a_fresh_run_is_adopted_only_after_the_previous_terminal() {
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        assert!(
            !state.apply_event(&started_run("run-2", 0)),
            "live run wins"
        );
        assert_eq!(state.stale_rejected, 1);
        assert!(state.apply_event(&finished(1)));
        assert!(state.apply_event(&started_run("run-2", 0)));
        assert_eq!(state.active_run().map(RunId::as_str), Some("run-2"));
        assert!(!state.is_finished(), "a new run reopens the lifecycle");
        assert!(state.can_cancel());
        assert_eq!(state.last_seq(), Some(0));
    }

    #[test]
    fn approval_gate_requires_measured_fully_visible_detail() {
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        assert!(state.apply_event(&approval(1, "run tool host_write", "project scope")));
        assert!(state.pending_approval().is_some());
        assert!(
            !state.approval_decision_allowed(),
            "not allowed before the card was measured"
        );
        state.set_approval_geometry(ApprovalGeometry {
            inner_width: 60,
            inner_rows: 8,
            detail_rows: 6,
            clipped: false,
        });
        assert!(state.approval_decision_allowed());
        state.set_approval_geometry(ApprovalGeometry {
            inner_width: 20,
            inner_rows: 2,
            detail_rows: 9,
            clipped: true,
        });
        assert!(!state.approval_decision_allowed(), "clipped card is locked");
        state.inspect_approval();
        assert!(
            state.approval_detail_open(),
            "inspect opens the detail view"
        );
        assert!(
            !state.approval_decision_allowed(),
            "opening the detail view is not a grant"
        );
        state.record_approval_detail_view(0, 4, 9);
        assert!(
            !state.approval_decision_allowed(),
            "a partial first page stays locked"
        );
        // Jumping straight to the last page leaves a gap and never unlocks.
        state.approval_detail_scroll(5);
        state.record_approval_detail_view(state.approval_detail_scroll_position(), 4, 9);
        assert!(
            !state.approval_decision_allowed(),
            "a jump past hidden rows never unlocks"
        );
        // Paging from the first row to the last does unlock.
        state.open_approval_detail();
        state.record_approval_detail_view(0, 4, 9);
        state.approval_detail_page(1);
        state.record_approval_detail_view(state.approval_detail_scroll_position(), 4, 9);
        state.approval_detail_page(1);
        state.record_approval_detail_view(state.approval_detail_scroll_position(), 4, 9);
        assert!(state.approval_detail_seen_all());
        assert!(
            state.approval_decision_allowed(),
            "deliberate contiguous inspection unlocks the decision"
        );
        state.resolve_approval();
        assert!(state.pending_approval().is_none());
        assert!(!state.approval_decision_allowed());
        assert!(state.approval_geometry().is_none());
        assert!(!state.approval_detail_open());
    }

    #[test]
    fn truncated_card_fields_stay_locked_even_after_inspection() {
        use nexus_core::{ApprovalId, CallId};
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        let (session, run) = session_run();
        let notice = ApprovalNotice {
            approval: ApprovalId::new("a1-0").expect("valid"),
            call: CallId::new("c1-0").expect("valid"),
            summary: "s".repeat(MAX_APPROVAL_FIELD_BYTES * 3),
            scope_summary: "scope".to_owned(),
            args_preview: None,
            expires_at_elapsed: Duration::from_secs(120),
        };
        let event = RunEvent::new(session, run, 1, EventPayload::ApprovalRequired(notice));
        assert!(state.apply_event(&event));
        let card = state.pending_approval().expect("card exists");
        assert!(card.summary.len() <= MAX_APPROVAL_FIELD_BYTES);
        assert!(state.approval_detail_truncated());
        state.set_approval_geometry(ApprovalGeometry {
            inner_width: 60,
            inner_rows: 8,
            detail_rows: 2,
            clipped: false,
        });
        state.inspect_approval();
        state.record_approval_detail_view(0, 100, 3);
        assert!(state.approval_detail_seen_all());
        assert!(
            !state.approval_decision_allowed(),
            "lost detail can never be approved even after full inspection"
        );
    }

    #[test]
    fn valid_notice_bidi_expansion_is_not_truncated_and_can_be_approved() {
        use nexus_core::{ApprovalId, CallId};
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        let (session, run) = session_run();
        // 1018 ASCII + 2 U+202E = 1024 raw bytes; the visible escapes grow
        // the display form to 1018 + 2 * 8 = 1034 bytes. The display bound
        // must not cut this valid notice into a permanently locked card.
        let summary = format!("{}\u{202E}\u{202E}", "a".repeat(1018));
        assert_eq!(summary.len(), 1024);
        let notice = ApprovalNotice::new(
            ApprovalId::new("a1-0").expect("valid"),
            CallId::new("c1-0").expect("valid"),
            summary,
            "project scope",
            Duration::from_secs(120),
        )
        .expect("runtime accepts the bounded notice");
        let event = RunEvent::new(session, run, 1, EventPayload::ApprovalRequired(notice));
        assert!(state.apply_event(&event));
        let card = state.pending_approval().expect("card exists");
        assert_eq!(
            card.summary.len(),
            1018 + 2 * 8,
            "the escape is visible in the display form"
        );
        assert!(card.summary.contains("\\u{202E}"));
        assert!(
            !state.approval_detail_truncated(),
            "valid post-sanitize expansion is not cut"
        );
        assert!(state.approval_args_preview().is_none());
        // Once the renderer measured a complete compact card, approving is
        // possible; the expanded path unlocks after a complete render too.
        state.set_approval_geometry(ApprovalGeometry {
            inner_width: 78,
            inner_rows: 24,
            detail_rows: 6,
            clipped: false,
        });
        assert!(state.approval_decision_allowed());
        state.set_approval_geometry(ApprovalGeometry {
            inner_width: 20,
            inner_rows: 2,
            detail_rows: 6,
            clipped: true,
        });
        assert!(!state.approval_decision_allowed());
        state.inspect_approval();
        state.record_approval_detail_view(0, 64, 6);
        assert!(state.approval_detail_seen_all());
        assert!(state.approval_decision_allowed());
    }

    #[test]
    fn worst_case_two_byte_format_expansion_fits_the_display_bound() {
        use nexus_core::{ApprovalId, CallId};
        // 512 * 2 raw bytes = the full 1024-byte core notice bound; every
        // character escapes to an 8-byte visible form, the 4x sanitizer
        // worst case. Both must survive uncut and stay approvable.
        for (label, raw) in [
            ("arabic-letter-mark", "\u{061C}".repeat(512)),
            ("soft-hyphen", "\u{00AD}".repeat(512)),
        ] {
            assert_eq!(raw.len(), 1024, "{label}: raw notice is at the bound");
            let expected = sanitize_approval(&raw);
            let mut state = AppState::new();
            assert!(state.apply_event(&started(0)));
            let (session, run) = session_run();
            let notice = ApprovalNotice::new(
                ApprovalId::new("a1-0").expect("valid"),
                CallId::new("c1-0").expect("valid"),
                raw,
                "project scope",
                Duration::from_secs(120),
            )
            .expect("runtime accepts the bounded notice");
            let event = RunEvent::new(session, run, 1, EventPayload::ApprovalRequired(notice));
            assert!(state.apply_event(&event));
            let card = state.pending_approval().expect("card exists");
            assert_eq!(
                card.summary, expected,
                "{label}: stored field is the untruncated sanitized form"
            );
            assert_eq!(
                card.summary.len(),
                MAX_APPROVAL_FIELD_BYTES,
                "{label}: the full 4x expansion is retained"
            );
            assert!(
                !state.approval_detail_truncated(),
                "{label}: a valid notice is never cut"
            );
            let args_bytes = state.approval_args_preview().map_or(0, str::len);
            assert!(
                card.summary.len() + card.scope_summary.len() + args_bytes
                    <= MAX_APPROVAL_CARD_BYTES,
                "{label}: card stays inside the explicit display budget"
            );
            state.set_approval_geometry(ApprovalGeometry {
                inner_width: 78,
                inner_rows: 40,
                detail_rows: 80,
                clipped: false,
            });
            assert!(
                state.approval_decision_allowed(),
                "{label}: valid detail can still be approved"
            );
            // Even a frame too small for the compact card can reach approval
            // by inspecting the full expanded detail.
            state.set_approval_geometry(ApprovalGeometry {
                inner_width: 20,
                inner_rows: 2,
                detail_rows: 200,
                clipped: true,
            });
            assert!(!state.approval_decision_allowed());
            state.inspect_approval();
            state.record_approval_detail_view(0, 512, 200);
            assert!(
                state.approval_detail_seen_all(),
                "{label}: the expanded view displayed every row"
            );
            assert!(
                state.approval_decision_allowed(),
                "{label}: approvable after the expanded-detail flow"
            );
        }
    }

    #[test]
    fn approval_args_preview_is_sanitized_bounded_and_cleared() {
        use nexus_core::{ApprovalId, CallId};
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        let (session, run) = session_run();
        // A forged notice: the runtime bounds previews, but the presentation
        // boundary must not trust that.
        let hostile = format!("path\x1b[2J{}", "a".repeat(MAX_APPROVAL_FIELD_BYTES * 2));
        let notice = ApprovalNotice {
            approval: ApprovalId::new("a1-0").expect("valid"),
            call: CallId::new("c1-0").expect("valid"),
            summary: "run tool host_write".to_owned(),
            scope_summary: "project scope".to_owned(),
            args_preview: Some(hostile),
            expires_at_elapsed: Duration::from_secs(120),
        };
        let event = RunEvent::new(session, run, 1, EventPayload::ApprovalRequired(notice));
        assert!(state.apply_event(&event));
        let args = state.approval_args_preview().expect("preview retained");
        assert!(!args.contains('\x1b'), "preview is sanitized");
        assert!(args.starts_with("path"), "text after the escape survives");
        assert!(args.len() <= MAX_APPROVAL_FIELD_BYTES, "preview is bounded");
        assert!(
            state.approval_detail_truncated(),
            "a cut preview locks the decision"
        );
        state.set_approval_geometry(ApprovalGeometry {
            inner_width: 60,
            inner_rows: 8,
            detail_rows: 2,
            clipped: false,
        });
        state.inspect_approval();
        state.record_approval_detail_view(0, 64, 2);
        assert!(state.approval_detail_seen_all());
        assert!(
            !state.approval_decision_allowed(),
            "a preview cut before storage can never be approved"
        );
        state.resolve_approval();
        assert!(state.approval_args_preview().is_none());
        assert!(!state.approval_detail_open());
    }

    #[test]
    fn folding_collapses_entries_to_their_title() {
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        assert!(state.apply_event(&text(1, "line one\nline two")));
        let index = state.entry_count() - 1;
        assert!(state.toggle_fold(index));
        assert!(!state.toggle_fold(state.entry_count() + 5));
        let view = state.visible_lines(80, 20);
        assert!(
            view.lines
                .iter()
                .any(|line| line.contains("[folded, 2 lines]"))
        );
        state.unfold_all();
        let view = state.visible_lines(80, 20);
        assert!(view.lines.iter().any(|line| line.contains("line two")));
        state.fold_all();
        assert!(state.entry(0).expect("entry").folded);
    }

    #[test]
    fn viewport_is_bottom_anchored_with_hidden_indicator() {
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        for index in 0..30 {
            state.push_tool(format!("tool {index}"), vec![format!("body {index}")]);
        }
        let view = state.visible_lines(80, 10);
        assert_eq!(view.lines.len(), 10);
        assert!(view.hidden_above > 0);
        assert!(view.lines.iter().any(|line| line.contains("tool 29")));
    }

    #[test]
    fn scrollback_clamps_to_available_lines() {
        let mut state = AppState::new();
        state.set_viewport(80, 10);
        assert!(state.apply_event(&started(0)));
        state.push_tool("solo".to_owned(), vec!["one".to_owned()]);
        state.scroll_up(1_000_000);
        assert!(state.scrollback() <= 4, "clamped to available lines");
        state.scroll_down(1_000_000);
        assert_eq!(state.scrollback(), 0);
    }

    #[test]
    fn approval_card_lifecycle_and_cancel_availability() {
        use nexus_core::{ApprovalId, CallId};
        let mut state = AppState::new();
        assert!(!state.can_cancel());
        assert!(state.apply_event(&started(0)));
        assert!(state.can_cancel(), "streaming is cancellable");
        let (session, run) = session_run();
        let notice = ApprovalNotice::new(
            ApprovalId::new("a1-0").expect("valid"),
            CallId::new("c1-0").expect("valid"),
            "run tool host_write",
            "project scope",
            Duration::from_secs(120),
        )
        .expect("notice builds");
        let event = RunEvent::new(session, run, 1, EventPayload::ApprovalRequired(notice));
        assert!(state.apply_event(&event));
        assert!(state.pending_approval().is_some());
        assert!(state.can_cancel(), "pending approval is cancellable");
        state.resolve_approval();
        assert!(state.pending_approval().is_none());
        assert!(state.apply_event(&finished(2)));
        assert!(!state.can_cancel());
        assert!(state.is_finished());
    }

    #[test]
    fn composer_editing_is_bounded_and_multiline() {
        let mut state = AppState::new();
        state.composer_type('a');
        state.composer_newline();
        state.composer_type('b');
        assert_eq!(state.composer(), "a\nb");
        assert!(state.composer_backspace());
        assert_eq!(state.composer(), "a\n");
        let draft = state.composer_take();
        assert_eq!(draft, "a\n");
        assert_eq!(state.composer(), "");
        assert!(!state.composer_backspace());
    }

    #[test]
    fn composer_recall_walks_submissions_and_restores_the_draft() {
        let mut state = AppState::new();
        assert!(!state.recall_prev(), "no history recalls nothing");
        assert!(!state.recall_next(), "no recall in progress");
        state.record_submitted("first");
        state.record_submitted("second");
        state.composer_type('d');
        state.composer_type('r');
        assert_eq!(state.composer(), "dr");
        assert!(state.recall_prev());
        assert_eq!(state.composer(), "second");
        assert!(state.recall_prev());
        assert_eq!(state.composer(), "first");
        assert!(state.recall_prev());
        assert_eq!(state.composer(), "first", "recall stops at the oldest");
        assert!(state.recall_next());
        assert_eq!(state.composer(), "second");
        assert!(state.recall_next());
        assert_eq!(state.composer(), "dr", "the stashed draft is restored");
        assert!(!state.recall_next(), "recall ended");
    }

    #[test]
    fn composer_edits_and_new_submits_abandon_recall() {
        let mut state = AppState::new();
        state.record_submitted("first");
        state.record_submitted("second");
        assert!(state.recall_prev());
        assert_eq!(state.composer(), "second");
        state.composer_type('!');
        assert_eq!(state.composer(), "second!");
        // The edit abandoned recall: Up restarts from the newest entry.
        assert!(state.recall_prev());
        assert_eq!(state.composer(), "second");
        state.record_submitted("third");
        assert!(!state.recall_next(), "a submit abandons recall");
        assert!(state.recall_prev());
        assert_eq!(state.composer(), "third");
    }

    #[test]
    fn composer_history_is_bounded_and_skips_empty_submits() {
        let mut state = AppState::new();
        state.record_submitted("");
        assert!(!state.recall_prev(), "empty submits leave no history");
        for index in 0..(MAX_HISTORY_ENTRIES + 10) {
            state.record_submitted(&format!("task {index}"));
        }
        // The oldest entries were forgotten; the newest is recalled first.
        assert!(state.recall_prev());
        assert_eq!(
            state.composer(),
            format!("task {}", MAX_HISTORY_ENTRIES + 9)
        );
    }
    #[test]
    fn composer_type_rejects_control_and_bidi_chars_directly() {
        let mut state = AppState::new();
        for rejected in [
            '\x1b', '\n', '\r', '\t', '\x07', '\u{202E}', '\u{2066}', '\u{200E}', '\u{061C}',
        ] {
            state.composer_type(rejected);
        }
        assert_eq!(state.composer(), "", "raw controls never enter the draft");
        state.composer_type('a');
        state.composer_newline();
        state.composer_type('é');
        assert_eq!(state.composer(), "a\né");
    }

    #[test]
    fn untrusted_text_is_sanitized_at_the_boundary() {
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        assert!(state.apply_event(&text(1, "ok\x1b[2J overwritten")));
        let view = state.visible_lines(80, 20);
        assert!(view.lines.iter().all(|line| !line.contains('\x1b')));
        assert!(
            view.lines
                .iter()
                .any(|line| line.contains("ok overwritten"))
        );
    }

    #[test]
    fn refresh_gate_coalesces_saturated_deltas() {
        let mut gate = RefreshGate::new(Duration::from_millis(33));
        let start = Instant::now();
        assert!(!gate.ready(start), "nothing requested");
        gate.request();
        assert!(gate.ready(start), "first redraw is immediate");
        gate.mark_drawn(start);
        gate.request();
        assert!(!gate.ready(start), "second redraw waits for the budget");
        assert!(gate.ready(start + Duration::from_millis(33)));
        gate.mark_drawn(start + Duration::from_millis(33));
        assert!(!gate.ready(start + Duration::from_millis(34)));
    }

    #[test]
    fn model_display_and_usage_track_explicit_updates_only() {
        use nexus_core::{Usage, UsageFinality};
        let mut state = AppState::new();
        assert_eq!(state.active_model(), None);
        assert_eq!(state.active_provider(), None);
        assert_eq!(state.last_usage(), None);
        state.set_active_model(Some("m0".to_owned()), Some("acme".to_owned()));
        assert_eq!(state.active_model(), Some("m0"));
        assert_eq!(state.active_provider(), Some("acme"));
        state.set_active_model(None, None);
        assert_eq!(state.active_model(), None);

        let usage = |seq, input, output| {
            RunEvent::new(
                SessionId::new("sess-1").expect("valid"),
                RunId::new("run-1").expect("valid"),
                seq,
                EventPayload::UsageUpdated(Usage::new(input, output, UsageFinality::Final)),
            )
        };
        assert!(state.apply_event(&started(0)));
        assert!(state.apply_event(&usage(1, Some(10), None)));
        assert_eq!(
            state.last_usage().expect("usage recorded"),
            Usage::new(Some(10), None, UsageFinality::Final)
        );
        // A new run never wears the previous run's numbers.
        assert!(state.apply_event(&finished(3)));
        assert!(state.apply_event(&started_run("run-2", 4)));
        assert_eq!(state.last_usage(), None);
    }
}

/// Coverage for the private helpers no external caller can reach on its own:
/// the bounded-text cut flags, geometry recording, detail-view row tracking,
/// and the approval gate's internal flags. All of it is deterministic and
/// clock-free; nothing here changes production behavior.
#[cfg(test)]
mod cov_state_private {
    use super::*;
    use nexus_core::{ApprovalId, ApprovalNotice, RequestId, SessionId};

    /// Injected cache height that must never survive a fingerprint change.
    const POISONED: usize = 9_999;

    fn cov_session() -> SessionId {
        SessionId::new("cov-sess").expect("valid")
    }

    fn cov_run() -> RunId {
        RunId::new("cov-run").expect("valid")
    }

    fn started(seq: u64) -> RunEvent {
        RunEvent::new(
            cov_session(),
            cov_run(),
            seq,
            EventPayload::RunStarted {
                request: RequestId::new("cov-req").expect("valid"),
            },
        )
    }

    fn approval_notice(seq: u64, summary: &str) -> RunEvent {
        let notice = ApprovalNotice::new(
            ApprovalId::new("cov-a").expect("valid"),
            CallId::new("cov-c").expect("valid"),
            summary,
            "project scope",
            Duration::from_secs(120),
        )
        .expect("notice builds");
        RunEvent::new(
            cov_session(),
            cov_run(),
            seq,
            EventPayload::ApprovalRequired(notice),
        )
    }

    /// State holding one live, unmeasured approval card.
    fn card_state() -> AppState {
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        assert!(state.apply_event(&approval_notice(1, "run tool host_write")));
        state
    }

    /// State parked at the entry-count retention bound.
    fn filled_state() -> AppState {
        let mut state = AppState::new();
        for index in 0..(MAX_RETAINED_ENTRIES + 2) {
            state.push_tool(format!("tool {index}"), vec!["body".to_owned()]);
        }
        assert_eq!(state.entry_count(), MAX_RETAINED_ENTRIES);
        assert!(state.dropped_entries > 0);
        state
    }

    fn geometry(
        inner_width: usize,
        inner_rows: usize,
        detail_rows: usize,
        clipped: bool,
    ) -> ApprovalGeometry {
        ApprovalGeometry {
            inner_width,
            inner_rows,
            detail_rows,
            clipped,
        }
    }

    /// The fingerprint production code computed for `width`.
    fn live_key(entry: &Entry, width: usize) -> HeightKey {
        assert!(entry.wrapped_len(width) > 0);
        entry.height_cache.get().expect("cache is filled").key
    }

    fn poison(entry: &Entry, key: HeightKey) {
        entry.height_cache.set(Some(HeightCache {
            key,
            height: POISONED,
        }));
    }

    #[test]
    fn safe_prefix_keeps_short_and_exact_text_whole() {
        assert_eq!(safe_prefix("abc", 10), "abc");
        assert_eq!(safe_prefix("abc", 3), "abc", "the exact bound is not a cut");
        assert_eq!(safe_prefix("abc", 0), "", "no bytes means no text");
        assert_eq!(safe_prefix("", 4), "");
        assert_eq!(safe_prefix("é", 1), "", "a 2-byte char does not fit 1 byte");
        assert_eq!(safe_prefix("aé", 2), "a");
        assert_eq!(safe_prefix("aé", 3), "aé");
    }

    #[test]
    fn titles_are_flattened_escaped_and_cut_on_a_char_boundary() {
        assert_eq!(bounded_title("plain"), "plain");
        assert_eq!(bounded_title(""), "");
        assert_eq!(
            bounded_title("line\nbreak"),
            "line break",
            "a title is one line"
        );
        assert_eq!(
            bounded_title("\x1b[2Jclean"),
            "clean",
            "escapes never reach it"
        );
        assert_eq!(
            bounded_title("safe\u{202E}tail"),
            "safe\\u{202E}tail",
            "an attempted reorder stays visible"
        );

        let exact = "é".repeat(MAX_TITLE_BYTES / 2);
        assert_eq!(exact.len(), MAX_TITLE_BYTES);
        assert_eq!(bounded_title(&exact), exact, "a title at the bound is kept");
        let cut = bounded_title(&format!("a{exact}"));
        assert!(
            cut.len() < MAX_TITLE_BYTES,
            "an over-bound title is cut: {} bytes",
            cut.len()
        );
        assert!(
            cut.chars().all(|char| char == 'a' || char == 'é'),
            "the cut backs off to a character boundary"
        );
        assert_eq!(
            bounded_title(&"t".repeat(MAX_TITLE_BYTES * 2)).len(),
            MAX_TITLE_BYTES
        );
    }

    #[test]
    fn bounded_text_flags_a_cut_after_sanitizing_and_flattening() {
        let (text, cut) = bounded_text("plain", 5);
        assert_eq!(text, "plain");
        assert!(!cut, "the exact bound is not a cut");

        let (text, cut) = bounded_text("newline\nhere", 64);
        assert_eq!(text, "newline here", "newlines flatten before bounding");
        assert!(!cut);

        let (text, cut) = bounded_text("abcdef", 3);
        assert_eq!(text, "abc");
        assert!(cut);

        let (text, cut) = bounded_text("anything", 0);
        assert_eq!(text, "");
        assert!(cut, "an impossible bound cuts instead of passing text");

        let (text, cut) = bounded_text(&"é".repeat(4), 5);
        assert!(cut);
        assert_eq!(text.len(), 4, "the cut backs off to a char boundary");
        assert!(text.chars().all(|char| char == 'é'));

        let (text, cut) = bounded_text(&"a\nb".repeat(10), 19);
        assert!(cut, "flattened bytes are charged against the bound");
        assert!(text.len() <= 19);
        assert!(!text.contains('\n'));
    }

    #[test]
    fn an_absent_preview_is_not_a_cut() {
        assert_eq!(bounded_optional_text(None, 64), (None, false));
        let (preview, cut) = bounded_optional_text(Some("args"), 64);
        assert_eq!(preview.as_deref(), Some("args"));
        assert!(!cut);
        let (preview, cut) = bounded_optional_text(Some("args preview"), 4);
        assert_eq!(preview.as_deref(), Some("args"));
        assert!(cut, "the bounded helper's cut flag is propagated");
        let (preview, cut) = bounded_optional_text(Some("a\nb"), 64);
        assert_eq!(
            preview.as_deref(),
            Some("a b"),
            "previews flatten newlines too"
        );
        assert!(!cut);
    }

    #[test]
    fn chunking_is_char_based_and_always_makes_progress() {
        assert_eq!(chunks("", 4).count(), 0, "an empty string yields no chunk");
        assert_eq!(chunks("abc", 3).collect::<Vec<_>>(), vec!["abc"]);
        assert_eq!(
            chunks("abcdefg", 3).collect::<Vec<_>>(),
            vec!["abc", "def", "g"]
        );
        assert_eq!(
            chunks("abcdef", 0).collect::<Vec<_>>(),
            vec!["a", "b", "c", "d", "e", "f"],
            "a zero width still advances one char at a time"
        );
        assert_eq!(
            chunks("éé", 1).collect::<Vec<_>>(),
            vec!["é", "é"],
            "chunks are characters, not bytes"
        );
        assert_eq!(chunks("héllo", 4).collect::<Vec<_>>(), vec!["héll", "o"]);
    }

    #[test]
    fn wrapping_primitives_measure_chars_and_reserve_the_body_indent() {
        assert_eq!(wrapped_height("", 10), 1, "an empty line still takes a row");
        assert_eq!(
            wrapped_height("abc", 0),
            3,
            "a zero width degrades to one char"
        );
        assert_eq!(wrapped_height("abcde", 5), 1);
        assert_eq!(
            wrapped_height("abcde", 4),
            2,
            "an exact multiple adds no row"
        );
        assert_eq!(wrapped_height("abcdefgh", 4), 2);

        assert_eq!(body_width(0), 1, "the indent never exhausts the frame");
        assert_eq!(body_width(1), 1);
        assert_eq!(body_width(2), 1);
        assert_eq!(body_width(80), 80 - BODY_INDENT);

        assert_eq!(folded_text("title", 3), "title  [folded, 3 lines]");
        assert_eq!(folded_text("", 0), "  [folded, 0 lines]");
    }

    #[test]
    fn emit_line_skips_earlier_rows_and_stops_at_the_budget() {
        let mut out = vec!["pre".to_owned()];
        let mut index = 0;
        assert!(!emit_line(&mut out, &mut index, 0, 4, "tail", false));
        assert_eq!(index, 1, "a skipped row still advances the cursor");
        assert_eq!(out, vec!["pre".to_owned(), "tail".to_owned()]);

        let mut window = Vec::new();
        let mut cursor = 0;
        emit_line(&mut window, &mut cursor, 2, usize::MAX, "skipped", true);
        assert!(
            window.is_empty(),
            "rows above the skip offset are never materialized"
        );
        assert_eq!(cursor, 1);
        emit_line(&mut window, &mut cursor, 1, usize::MAX, "body", true);
        assert_eq!(
            window,
            vec!["  body".to_owned()],
            "body rows carry the indent"
        );
        assert_eq!(cursor, 2);

        let mut budget = Vec::new();
        let mut at = 0;
        assert!(!emit_line(&mut budget, &mut at, 0, 2, "one", false));
        assert!(
            emit_line(&mut budget, &mut at, 0, 2, "two", false),
            "filling the budget stops the caller"
        );
        assert!(
            emit_line(&mut budget, &mut at, 0, 2, "three", false),
            "an already full budget stops without emitting"
        );
        assert_eq!(budget, vec!["one".to_owned(), "two".to_owned()]);
    }

    #[test]
    fn render_range_honors_skip_take_and_empty_titles() {
        let mut entry = Entry::new(EntryKind::Assistant, "assistant");
        entry.push_line("body");

        let mut none = Vec::new();
        entry.render_range(40, 0, 0, &mut none, &mut Vec::new());
        assert!(none.is_empty(), "a zero budget renders nothing");

        let mut past = Vec::new();
        entry.render_range(40, 99, 10, &mut past, &mut Vec::new());
        assert!(past.is_empty(), "a skip past the entry renders nothing");

        let full = render_entry_lines(&entry, 40);
        assert_eq!(
            full,
            vec!["assistant".to_owned(), "  body".to_owned(), String::new()]
        );
        let mut slice = Vec::new();
        entry.render_range(40, 1, 1, &mut slice, &mut Vec::new());
        assert_eq!(slice, vec![full[1].clone()], "only the requested window");

        let untitled = Entry::new(EntryKind::System, "");
        assert_eq!(
            render_entry_lines(&untitled, 40),
            vec![String::new(), String::new()]
        );
        assert_eq!(untitled.wrapped_len(40), 2, "height and rendering agree");

        // A fold marker that wraps at this width still honors the window.
        let mut folded = Entry::new(EntryKind::Assistant, &"t".repeat(45));
        folded.push_line("hidden");
        folded.folded = true;
        let all = render_entry_lines(&folded, 40);
        assert_eq!(all.len(), 3, "the fold marker wraps at this width");
        assert_eq!(
            folded.wrapped_len(40),
            wrapped_height(&folded_text(&folded.title, 1), 40) + 1
        );
        let mut folded_slice = Vec::new();
        folded.render_range(40, 1, 1, &mut folded_slice, &mut Vec::new());
        assert_eq!(folded_slice, vec![all[1].clone()]);
    }

    #[test]
    fn assistant_markdown_renders_styles_with_matching_heights() {
        use crate::markdown::MdStyle;

        let mut entry = Entry::new(EntryKind::Assistant, "assistant");
        entry.push_line("# Title");
        entry.push_line("a **bold** word");
        entry.push_line("- item one");
        entry.push_line("- item two");
        let (rows, runs) = render_entry_rows(&entry, 40);
        assert_eq!(
            rows,
            vec![
                "assistant".to_owned(),
                "  Title".to_owned(),
                "  ".to_owned(),
                "  a bold word".to_owned(),
                "  ".to_owned(),
                "  • item one".to_owned(),
                "  • item two".to_owned(),
                String::new(),
            ]
        );
        assert_eq!(rows.len(), entry.wrapped_len(40), "heights match rendering");
        assert_eq!(runs.len(), rows.len(), "one run list per row");
        assert!(runs[0].is_empty(), "the title stays plain");
        assert_eq!(
            runs[1],
            vec![StyledRun {
                start: 2,
                len: 5,
                style: MdStyle::HEADING.union(MdStyle::BOLD),
            }],
            "the heading style sits past the indent"
        );
        assert_eq!(
            runs[3],
            vec![StyledRun {
                start: 4,
                len: 4,
                style: MdStyle::BOLD,
            }],
            "only the bold word is styled"
        );
        assert!(
            runs[2].is_empty()
                && runs[4].is_empty()
                && runs[5].is_empty()
                && runs[6].is_empty()
                && runs[7].is_empty(),
            "gaps, bullets, and the separator carry no styles"
        );
    }

    #[test]
    fn assistant_tables_and_footnotes_survive_the_full_window() {
        use crate::markdown::MdStyle;

        let mut entry = Entry::new(EntryKind::Assistant, "assistant");
        entry.push_line("| a | b |");
        entry.push_line("|---|---|");
        entry.push_line("| 1 | 2 |");
        entry.push_line("");
        entry.push_line("note[^1]");
        entry.push_line("[^1]: the footnote");
        let (rows, runs) = render_entry_rows(&entry, 40);
        assert_eq!(
            rows,
            vec![
                "assistant".to_owned(),
                "  \u{256d}\u{2500}\u{2500}\u{2500}\u{252c}\u{2500}\u{2500}\u{2500}\u{256e}"
                    .to_owned(),
                "  \u{2502} a \u{2502} b \u{2502}".to_owned(),
                "  \u{251c}\u{2500}\u{2500}\u{2500}\u{253c}\u{2500}\u{2500}\u{2500}\u{2524}"
                    .to_owned(),
                "  \u{2502} 1 \u{2502} 2 \u{2502}".to_owned(),
                "  \u{2570}\u{2500}\u{2500}\u{2500}\u{2534}\u{2500}\u{2500}\u{2500}\u{256f}"
                    .to_owned(),
                "  ".to_owned(),
                "  note[^1]".to_owned(),
                "  ".to_owned(),
                "  [^1]: the footnote".to_owned(),
                String::new(),
            ]
        );
        assert_eq!(rows.len(), entry.wrapped_len(40), "heights match rendering");
        assert_eq!(runs.len(), rows.len(), "one run list per row");
        // Header bold, footnote reference linked, rules plain.
        assert!(runs[2][0].style.contains(MdStyle::BOLD));
        assert!(runs[7].iter().any(|run| run.style.contains(MdStyle::LINK)));
        assert!(runs[3].iter().all(|run| !run.style.contains(MdStyle::BOLD)));
    }

    #[test]
    fn non_assistant_entries_ignore_markdown_syntax() {
        let mut user = Entry::new(EntryKind::User, "you");
        user.push_line("**not bold** `not code`");
        let (rows, runs) = render_entry_rows(&user, 40);
        assert_eq!(
            rows,
            vec![
                "you".to_owned(),
                "  **not bold** `not code`".to_owned(),
                String::new(),
            ]
        );
        assert!(runs.iter().all(Vec::is_empty));
        assert_eq!(user.wrapped_len(40), rows.len());
    }

    #[test]
    fn streaming_markdown_stays_plain_until_constructs_close() {
        use nexus_core::{AssistantText, EventPayload, TurnId};

        fn fragment(seq: u64, text: &str) -> RunEvent {
            let session = cov_session();
            let run = cov_run();
            RunEvent::new(
                session,
                run,
                seq,
                EventPayload::AssistantTextDelta(
                    AssistantText::new(TurnId::new("t1-0").expect("valid"), "item-0", text)
                        .expect("fragment builds"),
                ),
            )
        }

        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        assert!(state.apply_event(&fragment(1, "```rust\nlet x = 1;\n")));
        let open = state.visible_lines(80, 30);
        assert_eq!(open.lines.len(), open.styles.len());
        assert!(
            open.lines.iter().any(|line| line.contains("```rust")),
            "the unclosed opener stays literal: {open:?}"
        );
        assert!(
            open.styles.iter().all(Vec::is_empty),
            "no code styling while the fence is open: {open:?}"
        );

        assert!(state.apply_event(&fragment(2, "```\n")));
        let closed = state.visible_lines(80, 30);
        assert!(
            !closed.lines.iter().any(|line| line.contains("```")),
            "fence markers never render: {closed:?}"
        );
        assert!(
            closed.styles.iter().any(|runs| !runs.is_empty()),
            "the closed block gains code styling: {closed:?}"
        );
        assert_eq!(
            state.total_height(80),
            closed.lines.len(),
            "the whole conversation fits, so heights match rows"
        );
    }

    #[test]
    fn the_height_cache_memoizes_on_every_fingerprint_field() {
        let mut entry = Entry::new(EntryKind::Assistant, "assistant");
        entry.push_line(&"a".repeat(50));
        // Title 1 row plus 50 body chars at body width 38 = 2 rows,
        // plus the trailing separator row.
        assert_eq!(entry.wrapped_len(40), 4);
        let key = live_key(&entry, 40);
        assert_eq!(key.lines, 1);
        assert_eq!(key.title_len, "assistant".len());
        assert_eq!(key.tail_len, 50);
        assert!(!key.folded);
        assert!(!key.truncated);

        poison(&entry, key);
        assert_eq!(
            entry.wrapped_len(40),
            POISONED,
            "an unchanged fingerprint reuses the cached height"
        );
        for changed in [
            HeightKey { width: 41, ..key },
            HeightKey { lines: 2, ..key },
            HeightKey {
                folded: true,
                ..key
            },
            HeightKey {
                truncated: true,
                ..key
            },
            HeightKey {
                title_len: key.title_len + 1,
                ..key
            },
            HeightKey {
                tail_len: key.tail_len + 1,
                ..key
            },
        ] {
            poison(&entry, changed);
            assert_eq!(entry.wrapped_len(40), 4, "{changed:?} must be a cache miss");
        }

        // The cached key tracks the entry: one field moved, height redone.
        entry.append_open("extra");
        assert_eq!(entry.lines.len(), 1, "the fragment joins the open line");
        let updated = live_key(&entry, 40);
        assert_eq!(updated.lines, key.lines, "only the tail length moved");
        assert_eq!(updated.tail_len, key.tail_len + 5);
        assert_eq!(
            entry.height_cache.get().expect("cache").height,
            4,
            "55 chars still wrap to two body rows at width 38"
        );

        // Truncation adds the marker rows, in height and in rendering.
        entry.mark_truncated();
        let truncated = entry.wrapped_len(40);
        assert_eq!(
            truncated,
            4 + wrapped_height(TRUNCATION_MARKER, body_width(40))
        );
        assert_eq!(render_entry_lines(&entry, 40).len(), truncated);
    }

    #[test]
    fn streamed_appends_stop_at_the_entry_byte_bound() {
        let filler = MAX_ENTRY_BYTES - LINE_OVERHEAD - "assistant".len();
        let mut full = Entry::new(EntryKind::Assistant, "assistant");
        full.append_text(&"x".repeat(filler));
        assert_eq!(full.retained_bytes(), MAX_ENTRY_BYTES);
        assert!(full.open_line, "the line stays open for the next fragment");
        let before = full.lines.clone();
        full.append_text("more");
        assert!(full.truncated, "an append past the bound is flagged");
        assert_eq!(full.lines, before, "no bytes are added past the bound");
        assert_eq!(full.retained_bytes(), MAX_ENTRY_BYTES);

        // A partially fitting fragment keeps a safe prefix.
        let mut nearly = Entry::new(EntryKind::Assistant, "assistant");
        nearly.append_text(&"x".repeat(filler - 2));
        nearly.append_text("yzw");
        assert!(nearly.truncated);
        assert_eq!(nearly.retained_bytes(), MAX_ENTRY_BYTES);
        assert!(nearly.lines.last().expect("line").ends_with('z'));

        // Appending to an entry with no lines materializes one.
        let mut orphan = Entry::new(EntryKind::Assistant, "assistant");
        orphan.append_open("orphan");
        assert_eq!(orphan.lines, vec!["orphan".to_owned()]);

        // An empty fragment changes nothing at all.
        let mut blank = Entry::new(EntryKind::Assistant, "assistant");
        blank.append_text("");
        assert!(blank.lines.is_empty());
        assert_eq!(blank.retained_bytes(), "assistant".len());
    }

    #[test]
    fn a_stream_entry_restarts_when_the_tail_stops_matching() {
        let mut state = AppState::new();
        let key = StreamKey::Assistant {
            turn: TurnId::new("t1").expect("valid"),
            item: "item-0".to_owned(),
        };
        state.ensure_stream_entry(EntryKind::Assistant, "assistant", key.clone());
        state.mutate_tail(|entry| entry.append_text("he"));
        state.ensure_stream_entry(EntryKind::Assistant, "assistant", key.clone());
        state.mutate_tail(|entry| entry.append_text("llo"));
        assert_eq!(state.entry_count(), 1, "the same identity keeps one entry");
        assert_eq!(
            state.entry(0).expect("entry").lines,
            vec!["hello".to_owned()]
        );

        // Folding the tail breaks adjacency even for the same identity.
        assert!(state.toggle_fold(0));
        state.ensure_stream_entry(EntryKind::Assistant, "assistant", key);
        state.mutate_tail(|entry| entry.append_text("fresh"));
        assert_eq!(state.entry_count(), 2, "a folded tail absorbs nothing");
        assert_eq!(
            state.entry(1).expect("entry").lines,
            vec!["fresh".to_owned()],
            "the new entry starts a fresh open line"
        );
    }

    #[test]
    fn notices_and_submissions_record_kinds_and_reset_the_view() {
        let mut state = AppState::new();
        state.notice("");
        let entry = state.entry(0).expect("entry exists");
        assert_eq!(entry.kind, EntryKind::System);
        assert_eq!(
            entry.lines,
            vec![String::new()],
            "an empty notice keeps one blank row"
        );
        assert_eq!(
            render_entry_lines(entry, 40),
            vec!["system".to_owned(), "  ".to_owned(), String::new()],
            "the blank body row still renders indented"
        );
        assert_eq!(entry.wrapped_len(40), 3, "height and rendering agree");

        let mut live = filled_state();
        live.move_selection(1);
        live.scroll_up(3);
        assert!(live.scrollback() > 0);
        live.record_submitted("a\nb");
        assert_eq!(live.scrollback(), 0, "submitting pins the view to the tail");
        assert!(live.selected().is_none());
        let submitted = live.entry(live.entry_count() - 1).expect("entry exists");
        assert_eq!(submitted.kind, EntryKind::User);
        assert_eq!(submitted.lines, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn retention_trim_keeps_the_selection_on_the_same_entry() {
        let mut pinned = filled_state();
        pinned.move_selection(1);
        let selected = pinned.selected().expect("selection follows the last entry");
        pinned.scroll_up(1);
        let drops = pinned.dropped_entries;
        pinned.push_tool("newest".to_owned(), vec!["body".to_owned()]);
        assert_eq!(pinned.entry_count(), MAX_RETAINED_ENTRIES);
        assert_eq!(pinned.dropped_entries, drops + 1);
        assert_eq!(
            pinned.selected(),
            Some(selected - 1),
            "the index follows its entry when the oldest is dropped"
        );

        let mut live = filled_state();
        live.move_selection(1);
        live.push_tool("newest".to_owned(), vec!["body".to_owned()]);
        assert!(
            live.selected().is_none(),
            "an unpinned tail re-follows new output instead of keeping an index"
        );
    }

    #[test]
    fn geometry_recording_reports_the_latest_frame_only() {
        assert_eq!(
            ApprovalGeometry::default(),
            geometry(0, 0, 0, false),
            "an unrendered card measures nothing"
        );
        let mut state = card_state();
        assert!(state.approval_geometry().is_none(), "no frame yet");

        let compact = geometry(78, 24, 6, false);
        state.set_approval_geometry(compact);
        assert_eq!(state.approval_geometry(), Some(compact));
        assert!(
            state.approval_decision_allowed(),
            "a complete compact card unlocks on its own"
        );

        let clipped = geometry(20, 2, 9, true);
        state.set_approval_geometry(clipped);
        assert_eq!(
            state.approval_geometry(),
            Some(clipped),
            "the newest frame replaces the previous measurement"
        );
        assert!(!state.approval_decision_allowed());

        // A replacement notice invalidates the measurement.
        assert!(state.apply_event(&approval_notice(2, "run tool host_write again")));
        assert!(
            state.approval_geometry().is_none(),
            "the new card has not been measured"
        );
        assert!(!state.approval_decision_allowed());
        assert!(!state.approval_detail_open());

        // So does issuing the decision.
        state.set_approval_geometry(compact);
        assert!(state.approval_decision_allowed());
        state.resolve_approval();
        assert!(state.approval_geometry().is_none());
        assert_eq!(state.approval_detail_total_rows(), 0);
        assert_eq!(state.approval_detail_view_rows(), 0);
        assert_eq!(state.approval_detail_seen_through, 0);
        assert!(!state.approval_detail_open());
    }

    #[test]
    fn detail_row_measurement_clamps_scroll_and_needs_contiguous_coverage() {
        let mut state = card_state();
        state.inspect_approval();
        // A zero-row viewport still records one row, so coverage can move.
        state.record_approval_detail_view(0, 0, 6);
        assert_eq!(state.approval_detail_view_rows(), 1);
        assert_eq!(state.approval_detail_scroll_position(), 0);
        assert!(!state.approval_detail_seen_all());

        // A scroll past the final page is clamped to the final page.
        state.record_approval_detail_view(99, 2, 6);
        assert_eq!(state.approval_detail_scroll_position(), 4);
        assert_eq!(
            state.approval_detail_seen_through, 1,
            "a frame starting past the seen cursor cannot advance coverage"
        );

        // Contiguous pages from row 1 reach the end.
        state.record_approval_detail_view(1, 2, 6);
        assert_eq!(state.approval_detail_seen_through, 3);
        state.record_approval_detail_view(3, 2, 6);
        assert_eq!(state.approval_detail_seen_through, 5);
        state.record_approval_detail_view(4, 2, 6);
        assert_eq!(state.approval_detail_seen_through, 6);
        assert!(state.approval_detail_seen_all());
        assert!(
            state.approval_decision_allowed(),
            "a clipped card unlocks after a complete contiguous inspection"
        );

        // Zero detail rows means there is nothing to have seen.
        let mut bare = card_state();
        bare.inspect_approval();
        bare.record_approval_detail_view(0, 1, 0);
        assert!(!bare.approval_detail_seen_all());
        assert!(!bare.approval_decision_allowed());
    }

    #[test]
    fn the_detail_view_needs_a_card_and_reopens_deliberately() {
        let mut bare = AppState::new();
        bare.open_approval_detail();
        assert!(!bare.approval_detail_open(), "no card, no detail view");
        bare.inspect_approval();
        assert!(!bare.approval_detail_open());

        let mut state = card_state();
        state.inspect_approval();
        state.record_approval_detail_view(0, 2, 6);
        assert_eq!(
            state.approval_detail_seen_through, 2,
            "the first frame covers rows 0..2"
        );
        state.record_approval_detail_view(1, 2, 6);
        assert_eq!(
            state.approval_detail_seen_through, 3,
            "row 1 is adjacent to what was seen"
        );
        state.open_approval_detail();
        assert_eq!(
            state.approval_detail_scroll_position(),
            1,
            "re-opening an open view keeps its row position"
        );
        assert_eq!(
            state.approval_detail_seen_through, 3,
            "and keeps its recorded coverage"
        );
    }

    #[test]
    fn detail_scrolling_is_inert_while_the_view_is_closed() {
        let mut state = card_state();
        state.inspect_approval();
        state.record_approval_detail_view(0, 2, 6);
        state.record_approval_detail_view(1, 2, 6);
        assert_eq!(state.approval_detail_scroll_position(), 1);
        state.approval_detail_page(0);
        assert_eq!(
            state.approval_detail_scroll_position(),
            1,
            "a zero-direction page never moves"
        );
        state.approval_detail_page(-1);
        assert_eq!(
            state.approval_detail_scroll_position(),
            0,
            "paging up clamps at the first row"
        );

        state.record_approval_detail_view(1, 2, 6);
        state.close_approval_detail();
        assert!(!state.approval_detail_open());
        state.approval_detail_scroll(3);
        state.approval_detail_page(1);
        assert_eq!(
            state.approval_detail_scroll_position(),
            1,
            "scroll and page are no-ops while closed"
        );
        assert_eq!(
            state.approval_detail_seen_through, 3,
            "closing keeps the recorded coverage"
        );

        state.inspect_approval();
        assert!(state.approval_detail_open());
        assert_eq!(
            state.approval_detail_scroll_position(),
            0,
            "a reopened view restarts at row 0"
        );
        assert_eq!(
            state.approval_detail_seen_through, 0,
            "a reopened view re-requires inspection"
        );
        assert!(!state.approval_detail_seen_all());

        // A page is one recorded viewport height, clamped at the last page.
        state.record_approval_detail_view(0, 3, 9);
        state.approval_detail_page(1);
        assert_eq!(state.approval_detail_scroll_position(), 3);
        state.approval_detail_page(1);
        assert_eq!(state.approval_detail_scroll_position(), 6);
        state.approval_detail_page(1);
        assert_eq!(
            state.approval_detail_scroll_position(),
            6,
            "paging past the last row clamps"
        );
        // A taller viewport re-clamps the retained scroll position.
        state.record_approval_detail_view(3, 8, 9);
        assert_eq!(state.approval_detail_scroll_position(), 1);
    }

    #[test]
    fn the_gate_reads_stored_flags_not_rendered_output() {
        let mut state = card_state();
        // Forge the exact internal combination the gate consults.
        state.approval_field_truncated = true;
        state.set_approval_geometry(geometry(78, 24, 6, false));
        assert!(state.approval_detail_truncated());
        assert!(
            !state.approval_decision_allowed(),
            "a field cut at storage outranks a clean measurement"
        );
        // Coverage advances from any recorded frame, open view or not: the
        // renderer is the only caller that records a frame it actually drew.
        state.record_approval_detail_view(0, 64, 6);
        assert!(state.approval_detail_seen_all());
        assert!(
            !state.approval_decision_allowed(),
            "detail cut before storage can never be approved"
        );
    }

    #[test]
    fn project_dir_is_absent_until_recorded() {
        let mut state = AppState::new();
        assert_eq!(state.project_dir(), None);
        state.set_project_dir("/Volumes/work/proj");
        assert_eq!(state.project_dir(), Some("/Volumes/work/proj"));
    }

    #[test]
    fn wrapping_counts_cells_not_characters() {
        // ASCII is unchanged: one char is one cell.
        assert_eq!(display_width("hello"), 5);
        assert_eq!(wrapped_height("hello", 5), 1);
        assert_eq!(wrapped_height("hello!", 5), 2);
        // CJK glyphs need two cells each.
        assert_eq!(display_width("\u{4e2d}\u{6587}"), 4);
        assert_eq!(wrapped_height("\u{4e2d}\u{6587}", 4), 1);
        assert_eq!(wrapped_height("\u{4e2d}\u{6587}", 3), 2);
        assert_eq!(wrapped_height("", 10), 1, "blank rows stay one row");
        // Chunks never split a wide glyph.
        let parts: Vec<&str> = chunks("\u{4e2d}a\u{6587}", 3).collect();
        assert_eq!(parts, vec!["\u{4e2d}a", "\u{6587}"]);
        // A lone glyph wider than the width still advances.
        let tiny: Vec<&str> = chunks("\u{4e2d}", 1).collect();
        assert_eq!(tiny, vec!["\u{4e2d}"]);
    }

    #[test]
    fn cell_columns_map_onto_character_indexes() {
        // "a\u{4e2d}b": cells 0, 1-2, 3.
        assert_eq!(cell_to_char_col("a\u{4e2d}b", 0), 0);
        assert_eq!(cell_to_char_col("a\u{4e2d}b", 1), 1);
        assert_eq!(cell_to_char_col("a\u{4e2d}b", 2), 1);
        assert_eq!(cell_to_char_col("a\u{4e2d}b", 3), 2);
        assert_eq!(cell_to_char_col("a\u{4e2d}b", 99), 3);
        assert_eq!(cell_to_char_col("", 0), 0);
    }

    #[test]
    fn body_selection_normalizes_clamps_and_yanks() {
        let mut state = AppState::new();
        state.notice("alpha");
        state.notice("beta!");
        // Window rows: [system, "  alpha", "", system, "  beta!", ""].
        let rows = state.visible_lines(80, 30).lines;
        assert_eq!(rows.len(), 6);

        state.begin_body_selection(1, 2);
        state.extend_body_selection(1, 7);
        assert_eq!(
            state.selection_body_rows(&rows),
            Some(vec![(1, 2, 5)]),
            "one row selects within itself"
        );
        assert_eq!(state.body_selection_text(&rows).as_deref(), Some("alpha"));

        // Reversed drags normalize and columns clamp to each line.
        state.begin_body_selection(4, 99);
        state.extend_body_selection(1, 0);
        assert_eq!(
            state.selection_body_rows(&rows),
            Some(vec![(1, 0, 7), (2, 0, 0), (3, 0, 6), (4, 0, 7)]),
        );
        assert_eq!(
            state.body_selection_text(&rows).as_deref(),
            Some("  alpha\n\nsystem\n  beta!"),
            "blank rows keep their line break"
        );

        // A press without a drag covers nothing to copy.
        state.begin_body_selection(0, 0);
        assert_eq!(
            state.body_selection_text(&rows).as_deref(),
            Some(""),
            "a point selection yanks empty"
        );
    }

    #[test]
    fn composer_selection_clamps_to_the_draft() {
        let mut state = AppState::new();
        for char in "hi there".chars() {
            state.composer_type(char);
        }
        state.begin_composer_selection(3);
        state.extend_composer_selection(8);
        assert_eq!(state.composer_selection_range(), Some((3, 5)));
        assert_eq!(state.composer_selection_text().as_deref(), Some("there"));
        // Reversed and over-long heads normalize and clamp.
        state.begin_composer_selection(99);
        state.extend_composer_selection(0);
        assert_eq!(state.composer_selection_range(), Some((0, 8)));
    }

    #[test]
    fn selection_clears_on_scroll_content_and_edits() {
        let mut state = AppState::new();
        state.notice("alpha");
        state.begin_body_selection(1, 0);
        state.extend_body_selection(1, 3);
        assert!(state.text_selection().is_some());
        state.scroll_up(1);
        assert_eq!(state.text_selection(), None, "scrolling clears");

        state.begin_body_selection(1, 0);
        state.composer_type('x');
        assert_eq!(state.text_selection(), None, "edits clear");

        state.begin_body_selection(1, 0);
        assert!(state.toggle_fold(0));
        assert_eq!(state.text_selection(), None, "folding clears");
    }

    #[test]
    fn composer_modes_default_to_build_and_cycle_in_order() {
        let mut state = AppState::new();
        assert_eq!(state.mode().id, "build");
        assert!(!state.mode().read_only);
        assert_eq!(state.cycle_mode(), "plan");
        assert!(state.mode().read_only);
        assert_eq!(state.cycle_mode(), "build");
        assert!(!state.mode().read_only);
    }

    #[test]
    fn composer_modes_follow_configuration_and_fall_back_safely() {
        use nexus_config::AgentMode;

        let mut state = AppState::new();
        state.set_modes(
            vec![
                AgentMode::plan(),
                AgentMode::build(),
                AgentMode::custom("review", "Review", true).expect("valid mode"),
            ],
            Some("review"),
        );
        assert_eq!(state.mode().id, "review");
        assert_eq!(state.cycle_mode(), "plan");
        assert_eq!(state.cycle_mode(), "build");
        assert_eq!(state.cycle_mode(), "review");

        // A dangling default falls back to build, never to nothing.
        state.set_modes(vec![AgentMode::plan(), AgentMode::build()], Some("missing"));
        assert_eq!(state.mode().id, "build");

        // An empty install resets to the built-ins.
        state.set_modes(Vec::new(), None);
        assert_eq!(state.mode().id, "build");
        assert_eq!(state.cycle_mode(), "plan");
    }
}
