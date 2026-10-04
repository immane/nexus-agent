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

use nexus_core::{CallId, EventPayload, Limits, RunEvent, RunId, RunOutcome, TurnId};

use crate::sanitize::{is_bidi_format, sanitize, sanitize_approval};

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
        if self.folded {
            return wrapped_height(&folded_text(&self.title, self.lines.len()), width);
        }
        let body_width = body_width(width);
        let mut height = wrapped_height(&self.title, width);
        for line in &self.lines {
            height += wrapped_height(line, body_width);
        }
        if self.truncated {
            height += wrapped_height(TRUNCATION_MARKER, body_width);
        }
        height
    }

    /// Renders at most `take` wrapped lines of this entry into `out`,
    /// skipping the first `skip` lines. Skipped lines are never allocated, so
    /// an oversized entry intersecting the window materializes only the
    /// visible slice. Line counts match [`Entry::wrapped_len`] exactly.
    fn render_range(&self, width: usize, skip: usize, take: usize, out: &mut Vec<String>) {
        if take == 0 {
            return;
        }
        let width = width.max(1);
        let limit = out.len().saturating_add(take);
        let mut index = 0usize;
        if self.folded {
            let marker = folded_text(&self.title, self.lines.len());
            for chunk in chunks(&marker, width) {
                if emit_line(out, &mut index, skip, limit, chunk, false) {
                    return;
                }
            }
            return;
        }
        for chunk in chunks(&self.title, width) {
            if emit_line(out, &mut index, skip, limit, chunk, false) {
                return;
            }
        }
        if self.title.is_empty() && emit_line(out, &mut index, skip, limit, "", false) {
            return;
        }
        let body_width = body_width(width);
        for line in &self.lines {
            if line.is_empty() {
                if emit_line(out, &mut index, skip, limit, "", true) {
                    return;
                }
            } else {
                for chunk in chunks(line, body_width) {
                    if emit_line(out, &mut index, skip, limit, chunk, true) {
                        return;
                    }
                }
            }
        }
        if self.truncated {
            for chunk in chunks(TRUNCATION_MARKER, body_width) {
                if emit_line(out, &mut index, skip, limit, chunk, true) {
                    return;
                }
            }
        }
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
struct Chunks<'a> {
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
        let mut end = self.text.len();
        for (count, (offset, _)) in self.text[self.start..].char_indices().enumerate() {
            if count == self.width {
                end = self.start + offset;
                break;
            }
        }
        let chunk = &self.text[self.start..end];
        self.start = end;
        Some(chunk)
    }
}

fn chunks(text: &str, width: usize) -> Chunks<'_> {
    Chunks {
        text,
        width: width.max(1),
        start: 0,
    }
}

/// Character-count wrapping height of one logical line (M0-test choice:
/// counts chars, not terminal cell width; wide/CJK glyphs and tab expansion
/// may misalign and are called out as a limitation, not silently remeasured
/// here).
pub(crate) fn wrapped_height(line: &str, width: usize) -> usize {
    let width = width.max(1);
    let chars = line.chars().count().max(1);
    chars.div_ceil(width).max(1)
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
    let mut out = Vec::new();
    entry.render_range(width, 0, usize::MAX, &mut out);
    out
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
    /// Wrapped lines above the window (presentation-truncated view).
    pub hidden_above: usize,
    /// True when older entries were dropped from retention.
    pub retention_truncated: bool,
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
    /// Entry index selected in the viewport (None follows the live tail).
    selected: Option<usize>,
    /// Wrapped lines hidden below the viewport bottom.
    scrollback: usize,
    pending_approval: Option<PendingApprovalCard>,
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
    /// Last viewport geometry, for selection visibility math.
    viewport: (usize, usize),
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
            selected: None,
            scrollback: 0,
            pending_approval: None,
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
        }
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
        match event.payload() {
            EventPayload::RunStarted { .. } => {
                self.streaming = true;
                self.finished = None;
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
            EventPayload::UsageUpdated(_) => {
                self.status = "running".to_owned();
            }
            EventPayload::RunFinished(finished) => {
                self.streaming = false;
                self.finished = Some(finished.outcome());
                self.resolve_approval();
                self.status = format!("finished: {:?}", finished.outcome());
                self.push_system(format!(
                    "run {} finished: {:?} (ephemeral record)",
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
    pub fn record_submitted(&mut self, input: &str) {
        let mut entry = Entry::new(EntryKind::User, "you");
        for line in sanitize(input).split('\n') {
            entry.push_line(line);
        }
        self.push_entry(entry);
        self.scrollback = 0;
        self.selected = None;
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
            entry.render_range(width, skip, take, &mut lines);
        }
        // Drop the below-the-fold scrollback, then cap to the body height.
        if self.scrollback >= lines.len() {
            lines.clear();
        } else {
            lines.truncate(lines.len() - self.scrollback);
        }
        if lines.len() > height {
            let excess = lines.len() - height;
            lines.drain(..excess);
            hidden_above += excess;
        }
        VisibleView {
            lines,
            hidden_above,
            retention_truncated: self.dropped_entries > 0,
        }
    }

    /// Moves the viewport selection; the live tail re-follows on new output.
    pub fn move_selection(&mut self, delta: isize) {
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
                true
            }
            None => false,
        }
    }

    /// Folds every retained entry; manual folds are separate per entry.
    pub fn fold_all(&mut self) {
        for entry in &mut self.entries {
            entry.folded = true;
        }
    }

    /// Unfolds every retained entry.
    pub fn unfold_all(&mut self) {
        for entry in &mut self.entries {
            entry.folded = false;
        }
    }

    /// Scrolls the viewport up by `lines` wrapped lines, clamped to the
    /// available history.
    pub fn scroll_up(&mut self, lines: usize) {
        let (width, height) = (self.viewport.0.max(1), self.viewport.1.max(1));
        let total = self.total_height(width);
        let max = total.saturating_sub(height.min(total));
        self.scrollback = self.scrollback.saturating_add(lines).min(max);
    }

    /// Scrolls the viewport down by `lines` wrapped lines.
    pub fn scroll_down(&mut self, lines: usize) {
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
        if char.is_control() || is_bidi_format(char) {
            return;
        }
        if self.composer.len() + char.len_utf8() <= MAX_COMPOSER_BYTES {
            self.composer.push(char);
        }
    }

    /// Starts a new composer line.
    pub fn composer_newline(&mut self) {
        if self.composer.len() < MAX_COMPOSER_BYTES {
            self.composer.push('\n');
        }
    }

    /// Deletes the last composer char. Returns false when already empty.
    pub fn composer_backspace(&mut self) -> bool {
        self.composer.pop().is_some()
    }

    /// Clears the composer draft, returning the stashed text.
    pub fn composer_take(&mut self) -> String {
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

    /// Lines hidden below the viewport bottom.
    #[must_use]
    pub fn scrollback(&self) -> usize {
        self.scrollback
    }

    /// Last viewport body height in wrapped lines, for page scrolling.
    #[must_use]
    pub fn viewport_height(&self) -> usize {
        self.viewport.1
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
        // Body width at total width 80 is 78: 79 chars wrap to two lines.
        entry.push_line(&"a".repeat(79));
        assert_eq!(entry.wrapped_len(80), 3);
        assert_eq!(render_entry_lines(&entry, 80).len(), 3);
        assert_eq!(entry.wrapped_len(40), 4);
        assert_eq!(render_entry_lines(&entry, 40).len(), 4);
        let mut folded = Entry::new(EntryKind::Assistant, "assistant");
        folded.push_line("body");
        folded.folded = true;
        let expected = wrapped_height(&folded_text("assistant", 1), 40);
        assert_eq!(folded.wrapped_len(40), expected);
        assert_eq!(render_entry_lines(&folded, 40).len(), expected);
        let mut truncated = Entry::new(EntryKind::Assistant, "assistant");
        truncated.push_line("body");
        truncated.truncated = true;
        let expected = 2 + wrapped_height(TRUNCATION_MARKER, 38);
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
}
