//! Presentation state only: bounded viewport, folding, approval card,
//! composer draft, and a controlled refresh gate.
//!
//! Nothing here executes tools, grants approvals, or bypasses policy.
//! Decisions leave this module as typed [`nexus_core::Command`] values via
//! [`crate::decisions`]; applying runtime events enters via
//! [`AppState::apply_event`], which rejects stale-run updates.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use nexus_core::{EventPayload, Limits, RunEvent, RunId, RunOutcome};

use crate::sanitize::sanitize;

/// Presentation entry cap, tracking the M0-test retained-context budget
/// (lock section 1: 128 retained context items). Older entries are dropped
/// from presentation only; accepted conversation records are unaffected.
pub const MAX_RETAINED_ENTRIES: usize = Limits::M0_TEST_RETAINED_CONTEXT_ITEMS;
/// Per-entry presentation cap in bytes (M0-test choice, not a product
/// default). Excess is dropped with the entry's truncation flag set.
pub const MAX_ENTRY_BYTES: usize = 8_192;
/// Composer cap tracking the command input bound.
pub const MAX_COMPOSER_BYTES: usize = nexus_core::commands::MAX_INPUT_BYTES;
/// Maximum redraw rate: event-driven redraws are coalesced to at most one
/// frame per interval so a saturated stream cannot force full relayouts.
pub const MAX_REDRAW_INTERVAL: Duration = Duration::from_millis(33);

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

/// One foldable conversation entry with pre-sanitized lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Grouping kind.
    pub kind: EntryKind,
    /// One-line header shown even when folded.
    pub title: String,
    /// Sanitized body lines.
    pub lines: Vec<String>,
    /// Folded entries render as their title only.
    pub folded: bool,
    /// True when body text was cut to [`MAX_ENTRY_BYTES`].
    pub truncated: bool,
    /// Retained body bytes, for bound enforcement.
    bytes: usize,
}

impl Entry {
    fn new(kind: EntryKind, title: String) -> Self {
        Self {
            kind,
            title,
            lines: Vec::new(),
            folded: false,
            truncated: false,
            bytes: 0,
        }
    }

    /// Appends one sanitized line, dropping excess past the byte cap.
    fn push_line(&mut self, line: String) {
        if self.bytes + line.len() > MAX_ENTRY_BYTES {
            self.truncated = true;
            return;
        }
        self.bytes += line.len();
        self.lines.push(line);
    }

    /// Wrapped line count at `width` (folded entries occupy one line).
    fn wrapped_len(&self, width: usize) -> usize {
        if self.folded {
            return 1;
        }
        let width = width.max(1);
        1 + self
            .lines
            .iter()
            .map(|line| wrapped_height(line, width))
            .sum::<usize>()
    }
}

/// Character-count wrapping height of one logical line (M0-test choice:
/// counts chars, not terminal cell width; wide/CJK glyphs may misalign and
/// are called out as a limitation, not silently remeasured here).
fn wrapped_height(line: &str, width: usize) -> usize {
    let chars = line.chars().count().max(1);
    chars.div_ceil(width).max(1)
}

/// Iterator over one entry's rendered lines, wrapping to `width`.
fn render_entry_lines(entry: &Entry, width: usize) -> Vec<String> {
    if entry.folded {
        let hidden = entry.lines.len();
        return vec![format!("{}  [folded, {hidden} lines]", entry.title)];
    }
    let mut out = vec![entry.title.clone()];
    let width = width.max(1);
    for line in &entry.lines {
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            out.push("  ".to_owned());
            continue;
        }
        for chunk in chars.chunks(width) {
            let mut rendered = String::with_capacity(width + 2);
            rendered.push_str("  ");
            rendered.extend(chunk.iter());
            out.push(rendered);
        }
    }
    if entry.truncated {
        out.push("  [output truncated to presentation bound]".to_owned());
    }
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
    /// Presentation-only drops; accepted records are unaffected.
    pub dropped_entries: usize,
    composer: String,
    /// Entry index selected in the viewport (None follows the live tail).
    selected: Option<usize>,
    /// Wrapped lines hidden below the viewport bottom.
    scrollback: usize,
    pending_approval: Option<PendingApprovalCard>,
    streaming: bool,
    active_run: Option<RunId>,
    /// Stale-run updates rejected, per the lock's two-sided rule.
    pub stale_rejected: u64,
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
            dropped_entries: 0,
            composer: String::new(),
            selected: None,
            scrollback: 0,
            pending_approval: None,
            streaming: false,
            active_run: None,
            stale_rejected: 0,
            last_seq: None,
            status: "idle".to_owned(),
            finished: None,
            viewport: (80, 20),
        }
    }

    /// Applies one runtime event to presentation state. Returns false for
    /// stale-run updates, which are counted and never applied to the live run.
    pub fn apply_event(&mut self, event: &RunEvent) -> bool {
        match &self.active_run {
            Some(active) if event.run() == active => {}
            _ => {
                let adopts = self.active_run.is_none()
                    && matches!(event.payload(), EventPayload::RunStarted { .. });
                if !adopts {
                    self.stale_rejected += 1;
                    return false;
                }
                self.active_run = Some(event.run().clone());
            }
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
                for line in sanitize(&fragment.text).split('\n') {
                    self.push_line(
                        EntryKind::Assistant,
                        "assistant".to_owned(),
                        line.to_owned(),
                    );
                }
            }
            EventPayload::ToolCallPreview { item_key } => {
                self.push_tool(
                    format!("preview {item_key}"),
                    vec!["proposed call (not executable, never authorizes)".to_owned()],
                );
            }
            EventPayload::ApprovalRequired(notice) => {
                self.pending_approval = Some(PendingApprovalCard {
                    approval: notice.approval.clone(),
                    call: notice.call.clone(),
                    summary: sanitize(&notice.summary),
                    scope_summary: sanitize(&notice.scope_summary),
                    expires_at_elapsed: notice.expires_at_elapsed,
                });
                self.push_tool(
                    format!("approval requested for {}", notice.call.as_str()),
                    vec![sanitize(&notice.summary)],
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
                for line in sanitize(&progress.preview).split('\n') {
                    self.push_line(
                        EntryKind::Tool,
                        format!("tool {} output", progress.call.as_str()),
                        line.to_owned(),
                    );
                }
                let last = self.entries.back_mut().expect("just pushed");
                last.truncated |= progress.truncated;
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
                    self.pending_approval = None;
                }
                self.status = "running".to_owned();
            }
            EventPayload::UsageUpdated(_) => {
                self.status = "running".to_owned();
            }
            EventPayload::RunFinished(finished) => {
                self.streaming = false;
                self.finished = Some(finished.outcome());
                self.pending_approval = None;
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

    fn push_entry(&mut self, entry: Entry) {
        self.entries.push_back(entry);
        while self.entries.len() > MAX_RETAINED_ENTRIES {
            self.entries.pop_front();
            self.dropped_entries += 1;
            if let Some(selected) = self.selected.as_mut() {
                *selected = selected.saturating_sub(1);
            }
        }
    }

    /// Appends a line, coalescing into the tail entry on kind+title match.
    fn push_line(&mut self, kind: EntryKind, title: String, line: String) {
        let coalesce = self
            .entries
            .back()
            .is_some_and(|tail| tail.kind == kind && tail.title == title && !tail.folded);
        if !coalesce {
            self.push_entry(Entry::new(kind, title.clone()));
        }
        let tail = self.entries.back_mut().expect("entry exists");
        tail.push_line(line);
        if self.scrollback == 0 {
            self.selected = None;
        }
    }

    fn push_tool(&mut self, title: String, lines: Vec<String>) {
        let mut entry = Entry::new(EntryKind::Tool, title);
        for line in lines {
            entry.push_line(line);
        }
        self.push_entry(entry);
    }

    fn push_system(&mut self, line: String) {
        let mut entry = Entry::new(EntryKind::System, "system".to_owned());
        entry.push_line(line);
        self.push_entry(entry);
    }

    /// Records a user submission in the viewport (the runtime send itself
    /// goes through a [`nexus_core::Command`], never through this method).
    pub fn record_submitted(&mut self, input: &str) {
        let mut entry = Entry::new(EntryKind::User, "you".to_owned());
        for line in sanitize(input).split('\n') {
            entry.push_line(line.to_owned());
        }
        self.push_entry(entry);
        self.scrollback = 0;
        self.selected = None;
    }

    /// Bottom-anchored visible window. Only entries intersecting the window
    /// are wrapped; older entries are counted, never processed.
    #[must_use]
    pub fn visible_lines(&self, width: usize, height: usize) -> VisibleView {
        let width = width.max(1);
        let height = height.max(1);
        let need = height + self.scrollback;
        // Walk from the newest entry, wrapping only entries that can
        // intersect the window plus the scrolled-below region.
        let mut rendered: Vec<Vec<String>> = Vec::new();
        let mut wrapped = 0;
        let mut examined = 0;
        for entry in self.entries.iter().rev() {
            if wrapped >= need {
                break;
            }
            wrapped += entry.wrapped_len(width);
            rendered.push(render_entry_lines(entry, width));
            examined += 1;
        }
        let hidden_entries = self.entries.len().saturating_sub(examined);
        let mut hidden_above: usize = self
            .entries
            .iter()
            .take(hidden_entries)
            .map(|entry| entry.wrapped_len(width))
            .sum();
        let mut lines: Vec<String> = rendered.into_iter().rev().flatten().collect();
        // The oldest examined entry may overshoot the window; trim its top.
        if lines.len() > need {
            let excess = lines.len() - need;
            lines.drain(..excess);
            hidden_above += excess;
        }
        // Drop the below-the-fold scrollback, then cap to the height.
        if self.scrollback < lines.len() {
            lines.truncate(lines.len() - self.scrollback);
        } else {
            lines.clear();
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

    /// Keeps the selected entry inside the viewport window.
    fn ensure_visible(&mut self) {
        let Some(selected) = self.selected else {
            self.scrollback = 0;
            return;
        };
        let (width, height) = (self.viewport.0.max(1), self.viewport.1.max(1));
        let mut start = 0;
        let mut selected_range = (0, 0);
        for (index, entry) in self.entries.iter().enumerate() {
            let len = entry.wrapped_len(width);
            if index == selected {
                selected_range = (start, start + len);
            }
            start += len;
        }
        let total = start;
        if total <= height {
            self.scrollback = 0;
            return;
        }
        let (sel_start, sel_end) = selected_range;
        let bottom_hidden = total.saturating_sub(sel_end);
        if bottom_hidden < self.scrollback {
            // Selection moved down past the window: follow it.
            self.scrollback = bottom_hidden;
        } else if sel_start < total.saturating_sub(self.scrollback + height) {
            // Selection moved up past the window: pin its top.
            self.scrollback = total.saturating_sub(sel_start + height).min(total);
        }
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
        let total: usize = self
            .entries
            .iter()
            .map(|entry| entry.wrapped_len(width))
            .sum();
        let max = total.saturating_sub(height.min(total));
        self.scrollback = (self.scrollback + lines).min(max);
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

    /// Types one char, bounded by the command input limit.
    pub fn composer_type(&mut self, char: char) {
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
    }

    /// Current status line text.
    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Records a frontend notice (replies, errors) as a system entry.
    pub fn notice(&mut self, line: &str) {
        self.push_system(sanitize(line));
    }

    /// Caches the last viewport geometry for selection math.
    pub fn set_viewport(&mut self, width: usize, height: usize) {
        self.viewport = (width.max(1), height.max(1));
        let (width, height) = (self.viewport.0, self.viewport.1);
        let total: usize = self
            .entries
            .iter()
            .map(|entry| entry.wrapped_len(width))
            .sum();
        self.scrollback = self.scrollback.min(total.saturating_sub(height.min(total)));
        self.ensure_visible();
    }

    /// Retained entry count (bounded by [`MAX_RETAINED_ENTRIES`]).
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.entries.len()
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
    /// non-terminal fallback. Bounded by [`MAX_RETAINED_ENTRIES`].
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

    fn text(seq: u64, text: &str) -> RunEvent {
        use nexus_core::{AssistantText, TurnId};
        let (session, run) = session_run();
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
    fn oversized_entry_text_sets_truncation_flag() {
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        state.push_line(
            EntryKind::Assistant,
            "assistant".to_owned(),
            "x".repeat(MAX_ENTRY_BYTES + 1),
        );
        let entry = state.entry(state.entry_count() - 1).expect("entry exists");
        assert!(entry.truncated);
    }

    #[test]
    fn stale_run_updates_are_rejected_and_counted() {
        let mut state = AppState::new();
        assert!(state.apply_event(&started(0)));
        let (session, _) = session_run();
        let stale = RunEvent::new(
            session,
            RunId::new("run-9").expect("valid"),
            1,
            EventPayload::UsageUpdated(nexus_core::Usage::new(
                None,
                None,
                nexus_core::UsageFinality::Provisional,
            )),
        );
        assert!(!state.apply_event(&stale));
        assert_eq!(state.stale_rejected, 1);
        assert_eq!(state.last_seq(), Some(0));
        assert!(state.apply_event(&text(1, "live")));
        assert_eq!(state.stale_rejected, 1);
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
