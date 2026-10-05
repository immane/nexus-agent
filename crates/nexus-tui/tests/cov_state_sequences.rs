#![forbid(unsafe_code)]

//! Coverage hardening for [`nexus_tui::state::AppState`] event application,
//! from outside the crate through the public API only.
//!
//! The in-module unit tests can reach private helpers; these checks cannot, so
//! everything here is observed through the published surface: the `bool`
//! returned by [`AppState::apply_event`], the two rejection counters, the
//! sequence cursor, and the retained entries.
//!
//! Contract under test:
//! - a rejected event is a complete no-op. [`View`] captures every publicly
//!   observable field (entries, retention accounting, transcript, rendered
//!   window, lifecycle status, cursor, approval card and gate, selection,
//!   composer) so a rejection that leaks any mutation fails the comparison,
//!   not just the counter check;
//! - the two rejection reasons stay partitioned: duplicate, out-of-order, and
//!   post-terminal events on the owning run count as sequence rejections,
//!   events for any other run count as stale-run rejections, and no event is
//!   counted twice or silently dropped;
//! - exactly one `RunStarted` is accepted per run, even when a later one
//!   carries a higher sequence;
//! - a terminal outcome is sticky: no later event of any payload reopens,
//!   extends, or replaces the recorded outcome, and the cursor never moves;
//! - a fresh run is adopted only from no run or from a finished one, a live
//!   run is never displaced, adoption restarts per-run numbering, and the
//!   retired run becomes stale;
//! - adjacent fragments of one stream identity merge into a single logical
//!   line, and anything that presents an entry (or folds the tail) breaks the
//!   adjacency so interleaved item identities can never be joined.
//!
//! Deterministic by construction: no clocks, threads, I/O, or randomness, and
//! every sequence number is fixed by the test.

use std::time::Duration;

use nexus_core::{
    ApprovalId, ApprovalNotice, AssistantText, CallId, EffectState, EventPayload, Evidence,
    ExecutionStatus, PersistenceState, RequestId, RunEvent, RunFinished, RunId, RunOutcome,
    SessionId, ToolFinishedInfo, ToolOutcome, ToolProgress, ToolStartedInfo, TurnId, Usage,
    UsageFinality,
};
use nexus_tui::state::{
    AppState, ApprovalGeometry, EntryKind, MAX_RETAINED_BYTES, MAX_RETAINED_ENTRIES,
};

/// Viewport used for every rendered-window comparison.
const VIEW_WIDTH: usize = 80;
const VIEW_HEIGHT: usize = 24;
/// Session identity carried by the event builders.
const SESSION: &str = "sess-1";

/// Public projection of one retained entry. Private entry bookkeeping
/// (cached heights, byte accounting, stream identity) is deliberately not
/// projected: it is not part of the observable contract.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EntryView {
    kind: EntryKind,
    title: String,
    lines: Vec<String>,
    folded: bool,
    truncated: bool,
}

/// Everything a caller of the public API can observe about one `AppState`,
/// except the rejection counters, which are asserted separately because a
/// rejection is expected to change them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct View {
    entries: Vec<EntryView>,
    transcript: Vec<String>,
    retained_bytes: usize,
    dropped_entries: usize,
    total_height: usize,
    visible: Vec<String>,
    hidden_above: usize,
    retention_truncated: bool,
    status: String,
    last_seq: Option<u64>,
    active_run: Option<String>,
    is_finished: bool,
    can_cancel: bool,
    pending_call: Option<String>,
    pending_summary: Option<String>,
    approval_args: Option<String>,
    approval_clipped: Option<bool>,
    approval_field_truncated: bool,
    approval_detail_open: bool,
    approval_detail_scroll: usize,
    approval_detail_total_rows: usize,
    approval_detail_view_rows: usize,
    approval_detail_seen_all: bool,
    approval_decision_allowed: bool,
    selected: Option<usize>,
    scrollback: usize,
    composer: String,
}

impl View {
    fn of(state: &AppState) -> Self {
        let window = state.visible_lines(VIEW_WIDTH, VIEW_HEIGHT);
        let entries = (0..state.entry_count())
            .map(|index| {
                let entry = state.entry(index).expect("index below entry_count");
                EntryView {
                    kind: entry.kind,
                    title: entry.title.clone(),
                    lines: entry.lines.clone(),
                    folded: entry.folded,
                    truncated: entry.truncated,
                }
            })
            .collect();
        Self {
            entries,
            transcript: state.transcript(),
            retained_bytes: state.retained_bytes(),
            dropped_entries: state.dropped_entries,
            total_height: state.total_height(VIEW_WIDTH),
            visible: window.lines,
            hidden_above: window.hidden_above,
            retention_truncated: window.retention_truncated,
            status: state.status().to_owned(),
            last_seq: state.last_seq(),
            active_run: state.active_run().map(RunId::as_str).map(str::to_owned),
            is_finished: state.is_finished(),
            can_cancel: state.can_cancel(),
            pending_call: state
                .pending_approval()
                .map(|card| card.call.as_str().to_owned()),
            pending_summary: state.pending_approval().map(|card| card.summary.clone()),
            approval_args: state.approval_args_preview().map(str::to_owned),
            approval_clipped: state.approval_geometry().map(|geometry| geometry.clipped),
            approval_field_truncated: state.approval_detail_truncated(),
            approval_detail_open: state.approval_detail_open(),
            approval_detail_scroll: state.approval_detail_scroll_position(),
            approval_detail_total_rows: state.approval_detail_total_rows(),
            approval_detail_view_rows: state.approval_detail_view_rows(),
            approval_detail_seen_all: state.approval_detail_seen_all(),
            approval_decision_allowed: state.approval_decision_allowed(),
            selected: state.selected(),
            scrollback: state.scrollback(),
            composer: state.composer().to_owned(),
        }
    }
}

/// Body lines of the retained entry at `index`.
fn lines(state: &AppState, index: usize) -> Vec<String> {
    state
        .entry(index)
        .unwrap_or_else(|| panic!("entry {index} is retained"))
        .lines
        .clone()
}

/// Entry envelope for an explicit session, so the session dimension can be
/// exercised independently of the run identity.
fn event_in(session: &str, run: &str, seq: u64, payload: EventPayload) -> RunEvent {
    RunEvent::new(
        SessionId::new(session).expect("session id builds"),
        RunId::new(run).expect("run id builds"),
        seq,
        payload,
    )
}

/// Envelope under [`SESSION`].
fn event(run: &str, seq: u64, payload: EventPayload) -> RunEvent {
    event_in(SESSION, run, seq, payload)
}

fn started(run: &str, seq: u64) -> RunEvent {
    event(
        run,
        seq,
        EventPayload::RunStarted {
            request: RequestId::new("req-1").expect("request id builds"),
        },
    )
}

fn text(run: &str, seq: u64, turn: &str, item: &str, fragment: &str) -> RunEvent {
    let assistant = AssistantText::new(TurnId::new(turn).expect("turn id builds"), item, fragment)
        .expect("assistant fragment builds");
    event(run, seq, EventPayload::AssistantTextDelta(assistant))
}

fn preview(run: &str, seq: u64, item_key: &str) -> RunEvent {
    event(
        run,
        seq,
        EventPayload::ToolCallPreview {
            item_key: item_key.to_owned(),
        },
    )
}

fn approval(run: &str, seq: u64, call: &str, summary: &str, scope: &str) -> RunEvent {
    approval_event(run, seq, call, summary, scope, None)
}

fn approval_with_args(
    run: &str,
    seq: u64,
    call: &str,
    summary: &str,
    scope: &str,
    args: &str,
) -> RunEvent {
    approval_event(run, seq, call, summary, scope, Some(args))
}

fn approval_event(
    run: &str,
    seq: u64,
    call: &str,
    summary: &str,
    scope: &str,
    args: Option<&str>,
) -> RunEvent {
    let notice = ApprovalNotice::new(
        ApprovalId::new("a1-0").expect("approval id builds"),
        CallId::new(call).expect("call id builds"),
        summary,
        scope,
        Duration::from_secs(120),
    )
    .expect("approval notice builds");
    let notice = match args {
        Some(preview) => notice
            .with_args_preview(preview)
            .expect("bounded args preview builds"),
        None => notice,
    };
    event(run, seq, EventPayload::ApprovalRequired(notice))
}

fn tool_started(run: &str, seq: u64, call: &str) -> RunEvent {
    event(
        run,
        seq,
        EventPayload::ToolStarted(ToolStartedInfo {
            call: CallId::new(call).expect("call id builds"),
        }),
    )
}

fn tool_output(run: &str, seq: u64, call: &str, preview: &str, truncated: bool) -> RunEvent {
    let progress = ToolProgress::new(
        CallId::new(call).expect("call id builds"),
        preview,
        truncated,
    )
    .expect("tool progress builds");
    event(run, seq, EventPayload::ToolOutput(progress))
}

fn tool_finished(run: &str, seq: u64, call: &str) -> RunEvent {
    let outcome = ToolOutcome::new(
        ExecutionStatus::Succeeded,
        EffectState::KnownApplied,
        Evidence::HostObserved,
        "ok",
        false,
    )
    .expect("tool outcome builds");
    event(
        run,
        seq,
        EventPayload::ToolFinished(ToolFinishedInfo {
            call: CallId::new(call).expect("call id builds"),
            outcome,
        }),
    )
}

fn usage(run: &str, seq: u64) -> RunEvent {
    event(
        run,
        seq,
        EventPayload::UsageUpdated(Usage::new(Some(11), Some(7), UsageFinality::Provisional)),
    )
}

fn finished(run: &str, seq: u64, outcome: RunOutcome) -> RunEvent {
    let record = RunFinished::new(outcome, PersistenceState::Ephemeral, None)
        .expect("terminal record builds");
    event(run, seq, EventPayload::RunFinished(record))
}

/// One event per [`EventPayload`] variant, in declaration order, with
/// consecutive sequences starting at `first_seq`. The terminal variant is last
/// so the batch can also be replayed as an accepted in-order run.
fn every_payload(run: &str, first_seq: u64) -> Vec<RunEvent> {
    vec![
        started(run, first_seq),
        text(run, first_seq + 1, "t1-0", "item-0", "payload text"),
        preview(run, first_seq + 2, "item-1"),
        approval(
            run,
            first_seq + 3,
            "c1-0",
            "run tool host_write",
            "project scope",
        ),
        tool_started(run, first_seq + 4, "c2-0"),
        tool_output(run, first_seq + 5, "c3-0", "progress line", false),
        tool_finished(run, first_seq + 6, "c4-0"),
        usage(run, first_seq + 7),
        finished(run, first_seq + 8, RunOutcome::Refused),
    ]
}

#[test]
fn accepted_events_advance_the_lifecycle_in_order() {
    // Positive control: the lifecycle fields the rejection tests assert on do
    // move for accepted events, so an unchanged observation means "rejected",
    // not "frozen".
    let mut state = AppState::new();
    assert_eq!(state.status(), "idle");
    assert!(!state.can_cancel());
    assert!(!state.is_finished());
    assert!(state.active_run().is_none());
    assert!(state.last_seq().is_none());

    assert!(state.apply_event(&started("run-1", 0)));
    assert_eq!(state.status(), "running");
    assert_eq!(state.active_run().map(RunId::as_str), Some("run-1"));
    assert!(state.can_cancel());

    assert!(state.apply_event(&text("run-1", 1, "t1-0", "item-0", "hi")));
    assert_eq!(state.status(), "running");
    assert!(state.apply_event(&tool_started("run-1", 2, "c1-0")));
    assert_eq!(state.status(), "executing tool");
    assert!(state.apply_event(&approval(
        "run-1",
        3,
        "c1-0",
        "run tool host_write",
        "project scope"
    )));
    assert_eq!(state.status(), "awaiting approval");
    assert!(state.pending_approval().is_some());

    assert!(state.apply_event(&tool_finished("run-1", 4, "c1-0")));
    assert!(
        state.pending_approval().is_none(),
        "the matching finish resolves the card"
    );
    assert_eq!(state.status(), "running");

    assert!(state.apply_event(&usage("run-1", 5)));
    assert_eq!(state.status(), "running");
    assert!(state.apply_event(&finished("run-1", 6, RunOutcome::Cancelled)));
    assert_eq!(state.status(), "finished: Cancelled");
    assert!(state.is_finished());
    assert!(!state.can_cancel());
    assert_eq!(state.last_seq(), Some(6));
    assert_eq!(state.seq_rejected, 0);
    assert_eq!(state.stale_rejected, 0);
}

#[test]
fn a_duplicate_or_out_of_order_sequence_is_a_complete_no_op() {
    let mut state = AppState::new();
    assert!(state.apply_event(&started("run-1", 0)));
    assert!(state.apply_event(&text("run-1", 40, "t1-0", "item-0", "live")));
    let before = View::of(&state);

    // One event per payload variant, all below the cursor.
    let rejected = every_payload("run-1", 10);
    assert_eq!(rejected.len(), 9, "one event per payload variant");
    for event in &rejected {
        assert!(!state.apply_event(event), "applied: {event:?}");
        assert_eq!(View::of(&state), before, "mutated by {event:?}");
    }
    assert_eq!(state.seq_rejected, 9);
    assert_eq!(state.stale_rejected, 0);
    assert_eq!(state.last_seq(), Some(40), "the cursor never moves");
    assert_eq!(lines(&state, 1), ["live"]);

    // The identical payloads in ascending order are accepted, so the gate is
    // the sequence rule and nothing about the payloads themselves. The
    // lifecycle variant is the documented exception: the run already owns one
    // `RunStarted`, so a second one is a duplicate at any sequence (pinned by
    // `a_duplicate_run_started_is_refused_at_every_sequence_and_position`) and
    // must not be counted as an accepted replay.
    for event in every_payload("run-1", 41) {
        let duplicate_lifecycle = matches!(event.payload(), EventPayload::RunStarted { .. });
        let applied = state.apply_event(&event);
        assert_eq!(
            applied, !duplicate_lifecycle,
            "unexpected verdict for {event:?}"
        );
    }
    assert_eq!(
        state.seq_rejected, 10,
        "the refused duplicate lifecycle event is the only extra rejection"
    );
    assert_eq!(state.stale_rejected, 0);
    assert_eq!(state.last_seq(), Some(49));
    assert!(state.is_finished(), "the terminal variant applied last");
    assert_eq!(
        lines(&state, 1),
        ["livepayload text"],
        "the applied assistant fragment continues the open tail line"
    );
    assert_eq!(
        state.entry_count(),
        8,
        "of the eight applied payloads, usage presents no entry and the assistant \
         fragment merges into the adjacent tail, so six entries are added to the two \
         already retained"
    );
}

#[test]
fn repeated_deliveries_are_counted_once_each_and_partitioned_by_reason() {
    let mut state = AppState::new();
    // With no owning run, any non-lifecycle event is stale, including repeats.
    for _ in 0..3 {
        assert!(!state.apply_event(&text("run-1", 0, "t1-0", "item-0", "orphan")));
    }
    assert_eq!(
        state.stale_rejected, 3,
        "each delivery counts, none is deduped"
    );
    assert_eq!(state.seq_rejected, 0, "a stale run is not a sequence fault");
    assert!(state.active_run().is_none());
    assert_eq!(state.entry_count(), 0, "a stale event presents nothing");

    assert!(state.apply_event(&started("run-1", 0)));
    assert!(state.apply_event(&text("run-1", 1, "t1-0", "item-0", "live")));
    let before = View::of(&state);
    // Every payload for a foreign run is stale, including a lifecycle event:
    // a live run is never displaced.
    for event in every_payload("run-9", 40) {
        assert!(!state.apply_event(&event), "applied foreign run: {event:?}");
        assert_eq!(View::of(&state), before, "mutated by {event:?}");
    }
    assert_eq!(state.stale_rejected, 3 + 9);
    assert_eq!(state.seq_rejected, 0);
    assert_eq!(state.last_seq(), Some(1));
    assert_eq!(state.active_run().map(RunId::as_str), Some("run-1"));

    // The owning run keeps applying; the counters stayed partitioned.
    assert!(state.apply_event(&text("run-1", 2, "t1-0", "item-0", " next")));
    assert_eq!(state.seq_rejected, 0);
    assert_eq!(state.stale_rejected, 12);
    assert_eq!(lines(&state, 1), ["live next"]);
}

#[test]
fn a_duplicate_run_started_is_refused_at_every_sequence_and_position() {
    let mut state = AppState::new();
    assert!(state.apply_event(&started("run-1", 0)));
    assert!(state.apply_event(&text("run-1", 1, "t1-0", "item-0", "hello")));
    let live = View::of(&state);

    // Immediately after the accepted lifecycle event, mid-stream, and with a
    // sequence far above the cursor: all are duplicate lifecycle events.
    for seq in [0, 1, 2, 7, u64::MAX] {
        let duplicate = started("run-1", seq);
        assert!(!state.apply_event(&duplicate), "applied: {duplicate:?}");
        assert_eq!(View::of(&state), live, "mutated by seq {seq}");
    }
    assert_eq!(state.seq_rejected, 5);
    assert_eq!(state.stale_rejected, 0);
    assert_eq!(
        state.last_seq(),
        Some(1),
        "a refused lifecycle event moves no cursor"
    );
    assert_eq!(state.active_run().map(RunId::as_str), Some("run-1"));
    assert_eq!(state.status(), "running", "the run is not re-announced");
    assert!(!state.is_finished());
    assert!(state.can_cancel(), "the run stays live and streaming");
    assert_eq!(
        state.entry_count(),
        2,
        "no second run-started notice is recorded"
    );
    assert_eq!(lines(&state, 1), ["hello"]);
}

#[test]
fn a_terminal_outcome_is_sticky_against_every_later_event() {
    let mut state = AppState::new();
    assert!(state.apply_event(&started("run-1", 0)));
    assert!(state.apply_event(&text("run-1", 1, "t1-0", "item-0", "live")));
    assert!(state.apply_event(&finished("run-1", 2, RunOutcome::Completed)));
    assert!(state.is_finished());
    assert!(!state.can_cancel());
    assert_eq!(state.entry_count(), 3);
    let terminal = View::of(&state);

    // Every payload variant is refused after the terminal outcome, including
    // a second terminal event carrying a different outcome.
    for event in every_payload("run-1", 3) {
        assert!(!state.apply_event(&event), "reopened by {event:?}");
        assert_eq!(View::of(&state), terminal, "mutated by {event:?}");
    }
    // A replay from before the terminal is refused by the same rule, so the
    // terminal check precedes the sequence check.
    assert!(!state.apply_event(&text("run-1", 1, "t1-0", "item-0", "late")));
    assert!(!state.apply_event(&started("run-1", 0)));
    assert_eq!(View::of(&state), terminal);

    assert_eq!(state.seq_rejected, 11);
    assert_eq!(state.stale_rejected, 0);
    assert_eq!(
        state.status(),
        "finished: Completed",
        "a different terminal replay cannot replace the sticky outcome"
    );
    assert_eq!(state.active_run().map(RunId::as_str), Some("run-1"));
    assert!(state.is_finished());
    assert!(!state.can_cancel());
    assert_eq!(
        state.last_seq(),
        Some(2),
        "the cursor never passes the terminal"
    );
    assert_eq!(state.entry_count(), 3, "no post-terminal entry is retained");
    assert!(
        state.pending_approval().is_none(),
        "a post-terminal approval cannot re-arm the card"
    );
    assert!(!state.approval_decision_allowed());
    assert_eq!(lines(&state, 1), ["live"]);
}

#[test]
fn a_fresh_run_is_adopted_only_after_the_previous_run_terminal() {
    let mut state = AppState::new();
    // Nothing owns the view yet, so any non-lifecycle event is stale.
    assert!(!state.apply_event(&text("run-1", 0, "t1-0", "item-0", "orphan")));
    assert!(state.active_run().is_none());
    assert_eq!(state.entry_count(), 0);

    assert!(state.apply_event(&started("run-1", 0)));
    let live = View::of(&state);
    // A live run is never displaced by another identity, not even one that
    // would otherwise be adoptable.
    assert!(!state.apply_event(&started("run-2", 0)));
    assert_eq!(View::of(&state), live);
    assert!(!state.apply_event(&started("run-9", 1)));
    assert_eq!(View::of(&state), live);
    assert_eq!(state.active_run().map(RunId::as_str), Some("run-1"));
    assert_eq!(state.stale_rejected, 3);
    assert_eq!(state.seq_rejected, 0);

    assert!(state.apply_event(&finished("run-1", 1, RunOutcome::Completed)));
    assert!(state.is_finished());
    let announced = state.entry_count();
    assert!(state.apply_event(&started("run-2", 0)));
    assert_eq!(state.active_run().map(RunId::as_str), Some("run-2"));
    assert!(!state.is_finished(), "a new run reopens the lifecycle");
    assert!(state.can_cancel());
    assert_eq!(state.status(), "running");
    assert_eq!(state.last_seq(), Some(0), "sequence numbering is per run");
    assert_eq!(
        state.entry_count(),
        announced + 1,
        "the adoption is announced once"
    );
    assert!(
        !state.approval_decision_allowed(),
        "a new run never inherits the previous run's approval gate"
    );
    assert!(state.pending_approval().is_none());

    // The retired run can neither extend nor terminate the live one.
    let current = View::of(&state);
    assert!(!state.apply_event(&text("run-1", 2, "t1-0", "item-0", "late")));
    assert!(!state.apply_event(&finished("run-1", 3, RunOutcome::Failed)));
    assert_eq!(View::of(&state), current);
    assert_eq!(state.active_run().map(RunId::as_str), Some("run-2"));
    assert!(
        !state.is_finished(),
        "a stale terminal outcome cannot finish the adopted run"
    );
    assert_eq!(state.status(), "running");
    assert_eq!(state.stale_rejected, 5);

    // The adopted run advances on its own numbering.
    assert!(state.apply_event(&text("run-2", 1, "t1-0", "item-0", "fresh")));
    assert_eq!(state.last_seq(), Some(1));
}

#[test]
fn sequence_numbering_is_per_run_and_restarts_on_adoption() {
    let mut state = AppState::new();
    assert!(state.apply_event(&started("run-1", 0)));
    assert!(state.apply_event(&text("run-1", 1, "t1-0", "item-0", "a")));
    assert!(state.apply_event(&text("run-1", 50, "t1-0", "item-0", "b")));
    assert!(state.apply_event(&finished("run-1", 51, RunOutcome::Completed)));
    assert_eq!(state.last_seq(), Some(51));

    // The second run numbers from zero again; the retired cursor is gone.
    assert!(state.apply_event(&started("run-2", 0)));
    assert_eq!(state.last_seq(), Some(0));
    assert!(state.apply_event(&text("run-2", 1, "t1-0", "item-0", "second run")));
    assert_eq!(state.last_seq(), Some(1));

    // A low sequence from the retired run is stale, not a sequence fault.
    let current = View::of(&state);
    assert!(!state.apply_event(&text("run-1", 2, "t1-0", "item-0", "zombie")));
    assert_eq!(View::of(&state), current);
    assert_eq!(state.stale_rejected, 1);
    assert_eq!(state.seq_rejected, 0);
    assert_eq!(state.last_seq(), Some(1));
}

#[test]
fn a_rejected_event_never_touches_the_pending_approval_card() {
    let mut state = AppState::new();
    assert!(state.apply_event(&started("run-1", 0)));
    assert!(state.apply_event(&approval_with_args(
        "run-1",
        1,
        "c1-0",
        "run tool host_write",
        "project scope",
        "{\"path\":\"notes.txt\"}",
    )));
    state.set_approval_geometry(ApprovalGeometry {
        inner_width: 60,
        inner_rows: 8,
        detail_rows: 2,
        clipped: false,
    });
    assert!(state.approval_decision_allowed(), "the card was measured");
    assert_eq!(
        state.approval_args_preview(),
        Some("{\"path\":\"notes.txt\"}")
    );
    let armed = View::of(&state);

    // An out-of-order terminal for the very call awaiting a decision.
    assert!(!state.apply_event(&tool_finished("run-1", 0, "c1-0")));
    assert_eq!(View::of(&state), armed);
    assert!(state.pending_approval().is_some());
    assert!(
        state.approval_decision_allowed(),
        "a refused event cannot move the gate"
    );

    // A duplicate approval cannot replace the displayed call with another.
    assert!(!state.apply_event(&approval(
        "run-1",
        1,
        "c9-9",
        "run tool host_delete",
        "root scope"
    )));
    assert_eq!(View::of(&state), armed);
    let card = state.pending_approval().expect("card still pending");
    assert_eq!(card.call.as_str(), "c1-0");
    assert_eq!(card.summary, "run tool host_write");
    assert_eq!(state.status(), "awaiting approval");
    assert_eq!(state.seq_rejected, 2);

    // Positive control: the same event at an advancing sequence resolves it.
    assert!(state.apply_event(&tool_finished("run-1", 2, "c1-0")));
    assert!(state.pending_approval().is_none());
    assert!(!state.approval_decision_allowed());
    assert_eq!(state.approval_args_preview(), None);
    assert_eq!(state.status(), "running");
    assert_eq!(state.last_seq(), Some(2));
    assert_eq!(state.seq_rejected, 2);
}

#[test]
fn a_rejected_event_consumes_no_retention_budget() {
    let mut state = AppState::new();
    assert!(state.apply_event(&started("run-1", 0)));
    let mut seq = 1u64;
    for index in 0..(MAX_RETAINED_ENTRIES - 1) {
        assert!(state.apply_event(&preview("run-1", seq, &format!("item-{index}"))));
        seq += 1;
    }
    assert_eq!(state.entry_count(), MAX_RETAINED_ENTRIES);
    assert_eq!(state.dropped_entries, 0);
    let saturated = View::of(&state);
    let bytes = state.retained_bytes();
    assert!(bytes > 0);

    // Out-of-order and stale deliveries change no accounting at all.
    assert!(!state.apply_event(&preview("run-1", 0, "item-replay")));
    assert!(!state.apply_event(&preview("run-9", 999, "item-stale")));
    assert!(!state.apply_event(&text("run-1", 0, "t1-0", "item-0", "replay")));
    assert_eq!(View::of(&state), saturated);
    assert_eq!(state.dropped_entries, 0);
    assert_eq!(state.retained_bytes(), bytes);
    assert_eq!(state.seq_rejected, 2);
    assert_eq!(state.stale_rejected, 1);

    // One accepted event evicts exactly the oldest retained entry and nothing
    // else, so the aggregate budget stays bounded.
    assert_eq!(state.entry(0).expect("oldest entry").title, "system");
    assert!(state.apply_event(&preview("run-1", seq, "item-new")));
    assert_eq!(state.entry_count(), MAX_RETAINED_ENTRIES);
    assert_eq!(state.dropped_entries, 1);
    assert_eq!(
        state.entry(0).expect("new oldest entry").title,
        "preview item-0",
        "only the oldest entry left the presentation"
    );
    assert!(state.retained_bytes() <= MAX_RETAINED_BYTES);
    assert!(
        state
            .visible_lines(VIEW_WIDTH, VIEW_HEIGHT)
            .retention_truncated
    );
}

#[test]
fn adjacent_fragments_merge_while_rejections_and_empty_text_do_not_break_them() {
    let mut state = AppState::new();
    assert!(state.apply_event(&started("run-1", 0)));
    assert!(state.apply_event(&text("run-1", 1, "t1-0", "item-a", "he")));
    assert!(state.apply_event(&text("run-1", 2, "t1-0", "item-a", "llo")));
    assert_eq!(state.entry_count(), 2);
    assert_eq!(lines(&state, 1), ["hello"]);

    // An accepted event that presents no entry keeps the stream adjacent.
    assert!(state.apply_event(&usage("run-1", 3)));
    assert_eq!(state.entry_count(), 2);
    // A refused event is not an event at all and cannot break adjacency.
    assert!(!state.apply_event(&text("run-1", 3, "t1-0", "item-a", "ignored")));
    assert!(!state.apply_event(&text("run-9", 99, "t1-0", "item-a", "ignored")));
    assert!(state.apply_event(&text("run-1", 4, "t1-0", "item-a", " world")));
    assert_eq!(state.entry_count(), 2);
    assert_eq!(lines(&state, 1), ["hello world"]);
    assert_eq!(state.last_seq(), Some(4));

    // Newlines split lines; the trailing segment stays open for the next
    // fragment, and a trailing newline closes the line without a blank one.
    assert!(state.apply_event(&text("run-1", 5, "t1-0", "item-a", "\nsecond")));
    assert_eq!(lines(&state, 1), ["hello world", "second"]);
    assert!(state.apply_event(&text("run-1", 6, "t1-0", "item-a", "-line")));
    assert_eq!(lines(&state, 1), ["hello world", "second-line"]);
    assert!(state.apply_event(&text("run-1", 7, "t1-0", "item-a", "\n")));
    assert_eq!(
        lines(&state, 1),
        ["hello world", "second-line"],
        "a trailing newline adds no blank line"
    );
    assert!(state.apply_event(&text("run-1", 8, "t1-0", "item-a", "third")));
    assert_eq!(lines(&state, 1), ["hello world", "second-line", "third"]);

    // A fragment that sanitizes away presents no entry, so it cannot break an
    // adjacent stream, and it still advances the cursor.
    assert!(state.apply_event(&text("run-1", 9, "t1-0", "item-z", "\x1b[2J")));
    assert_eq!(state.entry_count(), 2);
    assert_eq!(state.last_seq(), Some(9));
    assert!(state.apply_event(&text("run-1", 10, "t1-0", "item-a", "!")));
    assert_eq!(state.entry_count(), 2);
    assert_eq!(lines(&state, 1), ["hello world", "second-line", "third!"]);
    assert_eq!(state.last_seq(), Some(10));
}

#[test]
fn anything_that_presents_an_entry_breaks_the_stream() {
    let mut state = AppState::new();
    assert!(state.apply_event(&started("run-1", 0)));
    assert!(state.apply_event(&text("run-1", 1, "t1-0", "item-a", "he")));
    assert!(state.apply_event(&preview("run-1", 2, "item-1")));
    assert!(state.apply_event(&text("run-1", 3, "t1-0", "item-a", "llo")));
    assert_eq!(state.entry_count(), 4);
    assert_eq!(lines(&state, 1), ["he"]);
    assert_eq!(state.entry(2).expect("preview entry").kind, EntryKind::Tool);
    assert_eq!(lines(&state, 3), ["llo"]);
    assert!(
        !state.transcript().iter().any(|line| line.contains("hello")),
        "a broken adjacency is never repaired by a later fragment"
    );

    // Any presented entry continues to break adjacency, including another
    // tool preview and a tool progress stream of a different call.
    assert!(state.apply_event(&preview("run-1", 4, "item-2")));
    assert!(state.apply_event(&text("run-1", 5, "t1-0", "item-a", " more")));
    assert_eq!(state.entry_count(), 6);
    assert_eq!(
        state.entry(4).expect("second preview").kind,
        EntryKind::Tool
    );
    assert_eq!(lines(&state, 5), [" more"]);
    assert!(state.apply_event(&tool_output("run-1", 6, "c1-0", "progress", false)));
    assert!(state.apply_event(&text("run-1", 7, "t1-0", "item-a", "tail")));
    assert_eq!(state.entry_count(), 8);
    assert_eq!(lines(&state, 6), ["progress"]);
    assert_eq!(lines(&state, 7), ["tail"]);

    // Frontend notices and user submissions break adjacency as well.
    state.notice("frontend note");
    assert!(state.apply_event(&text("run-1", 8, "t1-0", "item-a", "-end")));
    assert_eq!(state.entry_count(), 10);
    assert_eq!(
        state.entry(8).expect("notice entry").kind,
        EntryKind::System
    );
    assert_eq!(lines(&state, 9), ["-end"]);
    state.record_submitted("next question");
    assert!(state.apply_event(&text("run-1", 9, "t1-0", "item-a", "-done")));
    assert_eq!(state.entry_count(), 12);
    assert_eq!(
        state.entry(10).expect("submission entry").kind,
        EntryKind::User
    );
    assert_eq!(lines(&state, 11), ["-done"]);
    let transcript = state.transcript();
    for joined in ["hello", "tail-end", "-end-done", " more-"] {
        assert!(
            !transcript.iter().any(|line| line.contains(joined)),
            "every presented boundary holds, so {joined:?} is never formed"
        );
    }
}

#[test]
fn a_folded_tail_never_absorbs_a_later_fragment() {
    let mut state = AppState::new();
    assert!(state.apply_event(&started("run-1", 0)));
    // Adjacent fragments of one identity concatenate into the open line:
    // "he" + "llo" is "hello".
    assert!(state.apply_event(&text("run-1", 1, "t1-0", "item-a", "he")));
    assert!(state.apply_event(&text("run-1", 2, "t1-0", "item-a", "llo")));
    assert_eq!(lines(&state, 1), ["hello"]);

    assert!(state.toggle_fold(1));
    assert!(state.entry(1).expect("folded entry").folded);
    assert!(state.apply_event(&text("run-1", 3, "t1-0", "item-a", "o")));
    assert_eq!(state.entry_count(), 3, "a folded tail is not appended into");
    let resumed = state.entry(2).expect("new entry");
    assert_eq!(resumed.kind, EntryKind::Assistant);
    assert_eq!(resumed.lines, ["o"]);
    assert!(!resumed.folded);
    assert_eq!(
        lines(&state, 1),
        ["hello"],
        "folded content stays untouched by later fragments"
    );

    // Unfolding does not retroactively merge the two entries.
    state.unfold_all();
    assert_eq!(lines(&state, 1), ["hello"]);
    assert_eq!(lines(&state, 2), ["o"]);
    assert!(
        !state
            .transcript()
            .iter()
            .any(|line| line.contains("helloo")),
        "the folded boundary keeps the resumed fragment separate"
    );
}

#[test]
fn interleaved_item_identities_never_merge() {
    let mut state = AppState::new();
    assert!(state.apply_event(&started("run-1", 0)));
    assert!(state.apply_event(&text("run-1", 1, "t1-0", "item-a", "he")));
    assert!(state.apply_event(&text("run-1", 2, "t1-0", "item-b", "XX")));
    assert!(state.apply_event(&text("run-1", 3, "t1-0", "item-a", "llo")));
    assert!(state.apply_event(&text("run-1", 4, "t2-0", "item-a", "turn two")));
    assert!(state.apply_event(&text("run-1", 5, "t2-0", "item-b", "second turn")));
    assert_eq!(
        state.entry_count(),
        6,
        "every identity break opens an entry"
    );
    assert_eq!(lines(&state, 1), ["he"]);
    assert_eq!(lines(&state, 2), ["XX"]);
    assert_eq!(lines(&state, 3), ["llo"]);
    assert_eq!(lines(&state, 4), ["turn two"]);
    assert_eq!(lines(&state, 5), ["second turn"]);
    for index in 1..6 {
        assert_eq!(
            state.entry(index).expect("assistant entry").kind,
            EntryKind::Assistant
        );
    }
    let transcript = state.transcript();
    for joined in ["hello", "turn twosecond", "second turnXX"] {
        assert!(
            !transcript.iter().any(|line| line.contains(joined)),
            "interleaved identities must not be joined into {joined:?}"
        );
    }
    // The resumed identity is a fresh entry, never a reopened older one.
    assert_eq!(state.entry(1).expect("first segment").lines, ["he"]);
    assert_eq!(state.entry(3).expect("resumed segment").lines, ["llo"]);
}

#[test]
fn interleaved_tool_output_calls_never_merge() {
    let mut state = AppState::new();
    assert!(state.apply_event(&started("run-1", 0)));
    assert!(state.apply_event(&tool_output("run-1", 1, "c1-0", "aa", false)));
    assert!(state.apply_event(&tool_output("run-1", 2, "c2-0", "bb", false)));
    assert!(state.apply_event(&tool_output("run-1", 3, "c1-0", "cc", false)));
    assert_eq!(state.entry_count(), 4);
    assert_eq!(
        state.entry(1).expect("first call").title,
        "tool c1-0 output"
    );
    assert_eq!(
        state.entry(2).expect("second call").title,
        "tool c2-0 output"
    );
    assert_eq!(
        state.entry(3).expect("resumed call").title,
        "tool c1-0 output"
    );
    assert_eq!(lines(&state, 1), ["aa"]);
    assert_eq!(lines(&state, 2), ["bb"]);
    assert_eq!(lines(&state, 3), ["cc"]);
    for index in 1..4 {
        assert_eq!(
            state.entry(index).expect("tool entry").kind,
            EntryKind::Tool
        );
    }

    // Adjacent progress for the same call still merges into one line.
    assert!(state.apply_event(&tool_output("run-1", 4, "c1-0", "dd", false)));
    assert_eq!(state.entry_count(), 4);
    assert_eq!(lines(&state, 3), ["ccdd"]);

    // An empty, untruncated preview presents nothing and keeps adjacency.
    assert!(state.apply_event(&tool_output("run-1", 5, "c1-0", "", false)));
    assert_eq!(state.entry_count(), 4);
    assert!(state.apply_event(&tool_output("run-1", 6, "c1-0", "ee", false)));
    assert_eq!(lines(&state, 3), ["ccddee"]);

    // An empty but truncated preview marks the open stream instead of
    // appending a blank line.
    assert!(state.apply_event(&tool_output("run-1", 7, "c1-0", "", true)));
    let open = state.entry(3).expect("open stream");
    assert_eq!(open.lines, ["ccddee"], "the truncation marker adds no line");
    assert!(open.truncated);

    // An assistant item never joins a tool output entry.
    assert!(state.apply_event(&text("run-1", 8, "t1-0", "item-a", "assistant")));
    assert_eq!(state.entry_count(), 5);
    let assistant = state.entry(4).expect("assistant entry");
    assert_eq!(assistant.kind, EntryKind::Assistant);
    assert_eq!(assistant.lines, ["assistant"]);
    assert!(!assistant.truncated, "truncation stays on the tool entry");
}

#[test]
fn the_event_gate_scopes_to_run_identity_and_not_to_the_session() {
    // Documented scope: `AppState` tracks run identity, not the owning
    // session, so a second session that reuses the run identity is not stale.
    // The runtime remains the authority that binds a session to a run; this
    // pins the presentation layer's actual boundary.
    let mut state = AppState::new();
    assert!(state.apply_event(&started("run-1", 0)));
    let fragment = AssistantText::new(
        TurnId::new("t1-0").expect("turn id builds"),
        "item-0",
        "shared",
    )
    .expect("assistant fragment builds");
    assert!(state.apply_event(&event_in(
        "sess-2",
        "run-1",
        1,
        EventPayload::AssistantTextDelta(fragment.clone())
    )));
    assert_eq!(state.last_seq(), Some(1));
    assert_eq!(lines(&state, 1), ["shared"]);

    // The sequence rule still applies across sessions.
    assert!(!state.apply_event(&event_in(
        "sess-2",
        "run-1",
        1,
        EventPayload::AssistantTextDelta(fragment)
    )));
    assert_eq!(state.seq_rejected, 1);
    assert_eq!(state.stale_rejected, 0);
    assert_eq!(lines(&state, 1), ["shared"]);
}
