//! `nexus-tui` M0-test demo: scripted fakes behind the real runtime port.
//!
//! TEST-ONLY. This binary wires [`nexus_fakes`] scripted doubles through
//! the same runtime command/event port the product TUI will use. It is never
//! real configuration: no provider credentials, no plugins, no network, no
//! stored sessions. A stderr banner says so on every launch.
//!
//! Frontend loop invariants (M0-test scope, not product defaults):
//!
//! - The tick interval is persistent: continuously ready events cannot
//!   starve keyboard polling or stalled-reorder handling.
//! - Keyboard handling per tick is bounded, so an input flood cannot starve
//!   event application or redraws.
//! - A `Submit` clears and records the draft and adopts the run identity
//!   only on an `Accepted` reply; `Busy` keeps the draft untouched.
//! - Data and control events are merged by the runtime's contiguous per-run
//!   sequence with a finite reorder buffer; older runs, duplicates, and
//!   post-terminal events are rejected, and preceding text is applied in
//!   order before the terminal outcome. A finalized run never reopens.
//! - Quit and error exits cancel live work and reconcile within a bounded
//!   window before the terminal is restored; unresolved native blocking
//!   work is reported instead of being presented as clean cleanup, and the
//!   Tokio runtime teardown uses an explicit `shutdown_timeout`.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::{self, IsTerminal};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyEvent, KeyEventKind, poll, read};
use nexus_core::{
    ApprovalNotice, CommandReply, CommandResponse, EventPayload, Limits, RequestId, RunEvent,
    RunId, SessionId,
};
use nexus_fakes::{FakeProvider, FakeTool};
use nexus_runtime::{EventStreams, Policy, Runtime, RuntimeConfig};
use nexus_tui::decisions::{approve_notice_command, deny_notice_command};
use nexus_tui::{
    Action, AppState, Focus, RefreshGate, cancel_command, install_panic_hook, map_key, next_focus,
    render, submit_command,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

const DEMO_PROFILE: &str = "m0-test";
const DEMO_INPUT: &str = "m0 test-only demo submission";
/// Persistent tick: keyboard polling and stalled-reorder handling happen at
/// this cadence even while events are continuously ready.
const TICK: Duration = Duration::from_millis(50);
const HEADLESS_TIMEOUT: Duration = Duration::from_secs(60);
/// Bounded window for cancellation reconciliation before terminal restore.
const RECONCILE_TIMEOUT: Duration = Duration::from_millis(500);
/// Explicit Tokio teardown bound: native blocking provider/tool work is never
/// awaited indefinitely.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// Keyboard events read per tick; a paste or autorepeat burst is drained
/// across later ticks instead of starving events and redraws.
const MAX_KEYS_PER_TICK: usize = 32;
/// Finite cross-channel reorder buffer. Beyond this, missing sequences are
/// skipped as explicitly counted presentation gaps, never reordered.
const MAX_REORDER_EVENTS: usize = 256;
const DEMO_SESSION: &str = "sess-tui-m0-test";

fn build_runtime() -> (Runtime, EventStreams) {
    let config = RuntimeConfig {
        limits: Limits::m0_test(),
        policy: Policy::m0_test(),
        has_approval_handler: true,
    };
    let provider = Arc::new(FakeProvider::interleaved_items());
    let tools: Vec<Arc<dyn nexus_core::ToolPort + Send + Sync>> = vec![
        Arc::new(FakeTool::read_only()),
        Arc::new(FakeTool::mutation()),
    ];
    Runtime::new(config, provider, tools)
}

fn main() {
    install_panic_hook();
    eprintln!(
        "nexus-tui M0 TEST-ONLY demo: scripted fakes, ephemeral store, no network, \
         no credentials, no real provider. Not real configuration."
    );
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("nexus-tui demo failed to start: {error}");
            std::process::exit(1);
        }
    };
    let result = runtime.block_on(run());
    // Explicit bounded teardown: a blocking provider/tool call that outlives
    // the frontend must not hold process exit open indefinitely. Any work
    // still outstanding after the bound is reported, never claimed clean.
    let shutdown_started = Instant::now();
    runtime.shutdown_timeout(SHUTDOWN_TIMEOUT);
    if shutdown_started.elapsed() >= SHUTDOWN_TIMEOUT {
        eprintln!(
            "nexus-tui: Tokio shutdown_timeout elapsed with work outstanding; \
             native blocking work was not joined (no rollback claim)"
        );
    }
    if let Err(error) = result {
        eprintln!("nexus-tui demo failed: {error}");
        std::process::exit(1);
    }
}

async fn run() -> io::Result<()> {
    let (runtime, streams) = build_runtime();
    if io::stdout().is_terminal() {
        match nexus_tui::TerminalGuard::setup() {
            Ok(mut guard) => {
                let report = interactive(runtime, streams).await;
                // Retryable restoration: a step that fails stays owned and is
                // attempted again on drop, and the failure is reported rather
                // than presented as a clean restore.
                let restored = guard.restore();
                match report {
                    Ok(report) => {
                        if report.unresolved_blocking {
                            eprintln!(
                                "nexus-tui: run cancellation did not reconcile; native \
                                 provider/tool work may still be running (bounded by the \
                                 explicit Tokio shutdown_timeout; no rollback claim)"
                            );
                        }
                        if let Err(error) = restored {
                            eprintln!(
                                "nexus-tui: terminal restore incomplete ({error}); a retry \
                                 will be attempted on exit"
                            );
                        }
                        report.result
                    }
                    Err(error) => {
                        if let Err(restore_error) = restored {
                            eprintln!(
                                "nexus-tui: terminal restore incomplete ({restore_error}); \
                                 a retry will be attempted on exit"
                            );
                        }
                        Err(error)
                    }
                }
            }
            Err(error) => {
                eprintln!("terminal setup failed ({error}); test transcript fallback");
                headless(runtime, streams).await
            }
        }
    } else {
        headless(runtime, streams).await
    }
}

/// Cross-channel event merger with a finite reorder buffer.
///
/// The runtime assigns one contiguous per-run sequence across both bounded
/// channels, but frontends must not assume cross-channel arrival order. This
/// helper holds at most [`MAX_REORDER_EVENTS`] out-of-order events and yields
/// them strictly by contiguous sequence. Missing sequences (the data channel
/// may legitimately drop presentation traffic) are skipped as explicitly
/// counted gaps, never reordered. Events for older runs, duplicates, and
/// anything arriving after the terminal outcome are rejected; a finalized
/// run never reopens.
#[derive(Debug)]
struct EventMerger {
    active: Option<RunId>,
    expected: u64,
    buffered: BTreeMap<u64, RunEvent>,
    terminal_pending: bool,
    finalized: bool,
    gaps: u64,
    rejected: u64,
}

impl EventMerger {
    fn new() -> Self {
        Self {
            active: None,
            expected: 0,
            buffered: BTreeMap::new(),
            terminal_pending: false,
            finalized: false,
            gaps: 0,
            rejected: 0,
        }
    }

    /// Adopts the runtime-accepted run. Called with the `Submit` reply before
    /// any event for that run is merged; events observed before adoption are
    /// rejected rather than guessed into a run.
    fn adopt(&mut self, run: &RunId) {
        self.active = Some(run.clone());
        self.expected = 0;
        self.buffered.clear();
        self.terminal_pending = false;
        self.finalized = false;
    }

    fn active_run(&self) -> Option<&RunId> {
        self.active.as_ref()
    }

    fn is_finalized(&self) -> bool {
        self.finalized
    }

    /// True when the terminal event was received but is still waiting behind
    /// a reorder gap.
    fn has_terminal(&self) -> bool {
        self.terminal_pending
    }

    fn has_pending(&self) -> bool {
        !self.buffered.is_empty()
    }

    fn gaps(&self) -> u64 {
        self.gaps
    }

    fn rejected(&self) -> u64 {
        self.rejected
    }

    /// Pushes one event and returns every newly contiguous event in order.
    fn push(&mut self, event: RunEvent) -> Vec<RunEvent> {
        let mut ready = Vec::new();
        if self.finalized || self.active.as_ref() != Some(event.run()) {
            self.rejected += 1;
            return ready;
        }
        if event.seq() < self.expected {
            self.rejected += 1;
            return ready;
        }
        if event.is_terminal() {
            self.terminal_pending = true;
        }
        if event.seq() == self.expected {
            self.note_emitted(&event);
            ready.push(event);
            self.expected = self.expected.saturating_add(1);
            ready.extend(self.take_ready());
        } else {
            if self.buffered.insert(event.seq(), event).is_some() {
                // A duplicate inside the reorder window replaces its earlier
                // copy but is still counted as rejected.
                self.rejected += 1;
            }
            if self.buffered.len() > MAX_REORDER_EVENTS {
                ready.extend(self.flush());
            }
        }
        ready
    }

    fn take_ready(&mut self) -> Vec<RunEvent> {
        let mut ready = Vec::new();
        while let Some(event) = self.buffered.remove(&self.expected) {
            self.note_emitted(&event);
            ready.push(event);
            self.expected = self.expected.saturating_add(1);
        }
        ready
    }

    /// Emits every buffered event in sequence order, counting sequences that
    /// never arrived as explicit gaps. The terminal event is always last.
    fn flush(&mut self) -> Vec<RunEvent> {
        let mut ready = Vec::new();
        while let Some((&seq, _)) = self.buffered.iter().next() {
            if seq > self.expected {
                self.gaps += seq - self.expected;
                self.expected = seq;
            }
            let event = self.buffered.remove(&seq).expect("peeked key exists");
            self.note_emitted(&event);
            ready.push(event);
            self.expected = self.expected.saturating_add(1);
        }
        ready
    }

    fn note_emitted(&mut self, event: &RunEvent) {
        if event.is_terminal() {
            self.terminal_pending = false;
            self.finalized = true;
        }
    }
}

/// Frontend session state: presentation, focus, accepted-run merge, and the
/// live runtime approval notice used to build decisions.
struct Frontend {
    state: AppState,
    focus: Focus,
    session: SessionId,
    merger: EventMerger,
    request_counter: u64,
    /// Exact runtime notice for the live approval, captured in merged order.
    live_approval: Option<(RunId, ApprovalNotice)>,
    reported_gaps: u64,
    rejected_reported: bool,
    state_rejected: u64,
    state_rejected_reported: bool,
}

impl Frontend {
    fn new(session: SessionId) -> Self {
        Self {
            state: AppState::new(),
            focus: Focus::Composer,
            session,
            merger: EventMerger::new(),
            request_counter: 0,
            live_approval: None,
            reported_gaps: 0,
            rejected_reported: false,
            state_rejected: 0,
            state_rejected_reported: false,
        }
    }

    fn next_request(&mut self) -> RequestId {
        self.request_counter += 1;
        RequestId::new(format!("req-tui-{}", self.request_counter))
            .expect("counter request id is valid")
    }

    /// Adopts the accepted submit reply before any event is merged. `Busy`
    /// and rejected replies adopt nothing; the caller keeps the draft.
    fn adopt_accepted(&mut self, reply: &CommandResponse) -> bool {
        if reply.reply() != CommandReply::Accepted {
            return false;
        }
        if let Some(run) = reply.run() {
            self.merger.adopt(run);
        }
        true
    }

    /// Applies merged events in order. Returns true when the terminal
    /// outcome was applied; the run is never reopened afterwards.
    fn apply_events(&mut self, events: impl IntoIterator<Item = RunEvent>) -> bool {
        for event in events {
            let terminal = event.is_terminal();
            self.note_approval(&event);
            if !self.state.apply_event(&event) {
                self.state_rejected += 1;
            }
            if terminal {
                return true;
            }
        }
        false
    }

    /// Tracks the exact runtime approval identity from merged events; the
    /// presentation card is display-only and is never used to infer a target.
    fn note_approval(&mut self, event: &RunEvent) {
        match event.payload() {
            EventPayload::ApprovalRequired(notice) => {
                self.live_approval = Some((event.run().clone(), notice.clone()));
            }
            EventPayload::ToolFinished(info) => {
                if self
                    .live_approval
                    .as_ref()
                    .is_some_and(|(_, notice)| notice.call == info.call)
                {
                    self.live_approval = None;
                }
            }
            EventPayload::RunFinished(_) => self.live_approval = None,
            _ => {}
        }
    }

    /// Surfaces merge truncation and rejections as explicit, bounded notices
    /// instead of silently losing presentation content.
    fn report_merger(&mut self) {
        let gaps = self.merger.gaps();
        if gaps > self.reported_gaps {
            let delta = gaps - self.reported_gaps;
            self.reported_gaps = gaps;
            self.state.notice(&format!(
                "presentation gap: {delta} event(s) missing from the data channel"
            ));
        }
        if self.merger.rejected() > 0 && !self.rejected_reported {
            self.rejected_reported = true;
            self.state.notice(&format!(
                "{} old, duplicate, or post-terminal event(s) rejected",
                self.merger.rejected()
            ));
        }
        if self.state_rejected > 0 && !self.state_rejected_reported {
            self.state_rejected_reported = true;
            self.state.notice(&format!(
                "{} event(s) rejected by presentation state",
                self.state_rejected
            ));
        }
    }
}

/// One frontend loop step: a merged channel event, the persistent tick, or
/// both channels closed.
enum LoopStep {
    Event(RunEvent),
    Tick,
    Closed,
}

/// Waits for the next data/control event or the next tick. The interval is
/// borrowed so its deadline persists across iterations: continuously ready
/// events can delay a tick only until its scheduled instant, never restart
/// the wait.
async fn next_loop_step(
    interval: &mut tokio::time::Interval,
    data: &mut mpsc::Receiver<RunEvent>,
    control: &mut mpsc::Receiver<RunEvent>,
) -> LoopStep {
    tokio::select! {
        event = data.recv() => match event {
            Some(event) => LoopStep::Event(event),
            None => LoopStep::Closed,
        },
        event = control.recv() => match event {
            Some(event) => LoopStep::Event(event),
            None => LoopStep::Closed,
        },
        _ = interval.tick() => LoopStep::Tick,
    }
}

/// Non-blocking next key press; non-key terminal events are drained and
/// ignored, so resize/mouse traffic cannot stall the loop.
fn read_key() -> io::Result<Option<KeyEvent>> {
    while poll(Duration::ZERO)? {
        if let Event::Key(key) = read()? {
            return Ok(Some(key));
        }
    }
    Ok(None)
}

/// Outcome of one bounded keyboard batch.
struct KeyBatch {
    handled: usize,
    quit: bool,
}

/// Handles at most `limit` key events, so a paste or autorepeat burst cannot
/// starve event application or redraws; remaining input is picked up on later
/// ticks. Returns true when the TUI should exit.
async fn handle_key_batch(
    front: &mut Frontend,
    runtime: &Runtime,
    mut next: impl FnMut() -> io::Result<Option<KeyEvent>>,
    limit: usize,
) -> io::Result<KeyBatch> {
    let mut handled = 0;
    for _ in 0..limit {
        let Some(key) = next()? else { break };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        handled += 1;
        if handle_key(front, runtime, key).await {
            return Ok(KeyBatch {
                handled,
                quit: true,
            });
        }
    }
    Ok(KeyBatch {
        handled,
        quit: false,
    })
}

/// Applies a submit reply to the frontend. Only `Accepted` adopts the run
/// identity and records the draft; `Busy`/rejections keep the draft and
/// report the reply. Returns true when the draft was accepted.
fn settle_submit(front: &mut Frontend, draft: &str, reply: &CommandResponse) -> bool {
    if !front.adopt_accepted(reply) {
        front.state.notice(&format!(
            "submit not accepted ({:?}); draft preserved",
            reply.reply()
        ));
        return false;
    }
    front.state.record_submitted(draft);
    true
}

/// Handles one key event. Returns true when the TUI should exit.
async fn handle_key(front: &mut Frontend, runtime: &Runtime, key: KeyEvent) -> bool {
    let Some(action) = map_key(front.focus, key) else {
        return false;
    };
    match action {
        Action::Submit => {
            if front.state.composer().trim().is_empty() {
                return false;
            }
            let request = front.next_request();
            match submit_command(
                request,
                front.session.clone(),
                front.state.composer(),
                DEMO_PROFILE,
            ) {
                Ok(command) => {
                    let draft = front.state.composer().to_owned();
                    let response = runtime.handle(command).await.0;
                    if settle_submit(front, &draft, &response) {
                        let stashed = front.state.composer_take();
                        debug_assert_eq!(stashed, draft);
                    }
                }
                Err(_) => front.state.notice("submit rejected: input invalid"),
            }
        }
        Action::Newline => front.state.composer_newline(),
        Action::Type(char) => front.state.composer_type(char),
        Action::Backspace => {
            front.state.composer_backspace();
        }
        Action::FocusSwitch => {
            let pending = front.state.pending_approval().is_some();
            front.focus = next_focus(front.focus, pending);
        }
        Action::ParkFocus => {
            // Esc steps back one rung: close the expanded detail first.
            front.state.close_approval_detail();
            front.focus = Focus::Viewport;
        }
        Action::InspectApproval => {
            if front.state.approval_detail_open() {
                front.state.close_approval_detail();
            } else {
                front.state.open_approval_detail();
            }
        }
        Action::ScrollUp => {
            if front.state.approval_detail_open() {
                front.state.approval_detail_scroll(-1);
            } else {
                front.state.move_selection(-1);
            }
        }
        Action::ScrollDown => {
            if front.state.approval_detail_open() {
                front.state.approval_detail_scroll(1);
            } else {
                front.state.move_selection(1);
            }
        }
        Action::PageUp => {
            if front.state.approval_detail_open() {
                front.state.approval_detail_page(-1);
            } else {
                front.state.scroll_up(front.state.viewport_height());
            }
        }
        Action::PageDown => {
            if front.state.approval_detail_open() {
                front.state.approval_detail_page(1);
            } else {
                front.state.scroll_down(front.state.viewport_height());
            }
        }
        Action::FoldToggle => {
            if front.state.selected().is_none() {
                front.state.move_selection(-1);
            }
            if let Some(index) = front.state.selected() {
                front.state.toggle_fold(index);
            }
        }
        Action::ApproveOnce | Action::Deny => {
            let Some((run, notice)) = front.live_approval.clone() else {
                front.state.notice("no live approval to decide");
                return false;
            };
            if action == Action::ApproveOnce && !front.state.approval_decision_allowed() {
                front.state.notice(
                    "allow-once locked: press i to inspect the full runtime detail (deny is available)",
                );
                return false;
            }
            let request = front.next_request();
            let command = if action == Action::ApproveOnce {
                approve_notice_command(request, &run, &notice)
            } else {
                deny_notice_command(request, &run, &notice)
            };
            let reply = runtime.handle(command).await.0;
            front
                .state
                .notice(&format!("decision reply: {:?}", reply.reply()));
            front.state.resolve_approval();
            front.live_approval = None;
            front.focus = Focus::Viewport;
        }
        Action::Cancel => {
            if !front.state.can_cancel() {
                return true;
            }
            match front.state.active_run().cloned() {
                Some(run) => {
                    let reply = runtime
                        .handle(cancel_command(front.next_request(), &run))
                        .await
                        .0;
                    front
                        .state
                        .notice(&format!("cancel reply: {:?}", reply.reply()));
                }
                None => front.state.notice("nothing cancellable"),
            }
        }
        Action::Quit => return true,
    }
    false
}

/// Result of one interactive session, consumed after terminal restoration so
/// diagnostics land on the restored screen.
struct InteractiveReport {
    result: io::Result<()>,
    unresolved_blocking: bool,
}

/// Interactive scripted demo: one canned submission, live approval card,
/// full test-only keyset. Exits on quit or the terminal run outcome.
async fn interactive(runtime: Runtime, mut streams: EventStreams) -> io::Result<InteractiveReport> {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).map_err(io::Error::other)?;
    let session =
        SessionId::new(DEMO_SESSION).map_err(|_| io::Error::other("demo session id rejected"))?;
    let mut front = Frontend::new(session);
    let mut gate = RefreshGate::m0_test();
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + TICK, TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Canned M0 submission through the same command path as typed input. The
    // draft is recorded and the run adopted only after `Accepted`.
    let submit = submit_command(
        front.next_request(),
        front.session.clone(),
        DEMO_INPUT,
        DEMO_PROFILE,
    )
    .map_err(io::Error::other)?;
    let reply = runtime.handle(submit).await.0;
    if settle_submit(&mut front, DEMO_INPUT, &reply) {
        front.state.notice("canned M0 submission accepted");
    }
    gate.request();

    let result = interactive_loop(
        &runtime,
        &mut streams,
        &mut front,
        &mut gate,
        &mut terminal,
        &mut interval,
    )
    .await;

    // Quit, terminal, and error exits all pass through here: while the run
    // has no terminal outcome, cancel it and reconcile within a bounded
    // window before the terminal is restored.
    let unresolved_blocking = if front.merger.is_finalized() {
        false
    } else {
        reconcile_after_stop(&runtime, &mut streams, &mut front, "exit").await
    };
    Ok(InteractiveReport {
        result,
        unresolved_blocking,
    })
}

async fn interactive_loop(
    runtime: &Runtime,
    streams: &mut EventStreams,
    front: &mut Frontend,
    gate: &mut RefreshGate,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    interval: &mut tokio::time::Interval,
) -> io::Result<()> {
    let mut event_since_tick = false;
    loop {
        let mut dirty = false;
        match next_loop_step(interval, &mut streams.data, &mut streams.control).await {
            LoopStep::Closed => return Ok(()),
            LoopStep::Event(event) => {
                event_since_tick = true;
                dirty = true;
                if absorb(front, event, streams) {
                    draw(terminal, front)?;
                    return Ok(());
                }
            }
            LoopStep::Tick => {
                let batch = handle_key_batch(front, runtime, read_key, MAX_KEYS_PER_TICK).await?;
                if batch.quit {
                    return Ok(());
                }
                dirty |= batch.handled > 0;
                if !event_since_tick && front.merger.has_pending() {
                    let flushed = front.merger.flush();
                    front.report_merger();
                    if !flushed.is_empty() {
                        dirty = true;
                        if front.apply_events(flushed) {
                            draw(terminal, front)?;
                            return Ok(());
                        }
                    }
                }
                event_since_tick = false;
            }
        }
        if dirty {
            gate.request();
        }
        let now = Instant::now();
        if gate.ready(now) {
            draw(terminal, front)?;
            gate.mark_drawn(now);
        }
    }
}

/// Merges one received event; when the terminal outcome is pending behind a
/// reorder gap, drains the remaining channel traffic first (every earlier
/// send is already queued), force-flushes missing sequences as counted gaps,
/// and applies the run's preceding text in order before the final outcome.
/// Returns true when the terminal was applied.
fn absorb(front: &mut Frontend, event: RunEvent, streams: &mut EventStreams) -> bool {
    let pushed = front.merger.push(event);
    let mut terminal = front.apply_events(pushed);
    if front.merger.has_terminal() {
        let drained = drain_available(&mut streams.data, &mut streams.control, &mut front.merger);
        terminal |= front.apply_events(drained);
        let flushed = front.merger.flush();
        terminal |= front.apply_events(flushed);
    }
    front.report_merger();
    terminal
}

/// Non-blocking drain of everything currently queued on both channels. The
/// runtime sends every event before the terminal outcome, so once the
/// terminal is observed this collects all preceding sends (or the channel
/// drops that created the gap).
fn drain_available(
    data: &mut mpsc::Receiver<RunEvent>,
    control: &mut mpsc::Receiver<RunEvent>,
    merger: &mut EventMerger,
) -> Vec<RunEvent> {
    let mut ready = Vec::new();
    while let Ok(event) = data.try_recv() {
        ready.extend(merger.push(event));
    }
    while let Ok(event) = control.try_recv() {
        ready.extend(merger.push(event));
    }
    ready
}

/// Cancels the live run (if any) and waits a bounded window for its terminal
/// outcome, applying merged events in order. Returns true when the run still
/// has no terminal outcome: native provider/tool work may still be running,
/// and no rollback or cleanup is claimed.
async fn reconcile_after_stop(
    runtime: &Runtime,
    streams: &mut EventStreams,
    front: &mut Frontend,
    reason: &str,
) -> bool {
    if front.merger.is_finalized() {
        return false;
    }
    let run = front
        .merger
        .active_run()
        .cloned()
        .or_else(|| front.state.active_run().cloned());
    let Some(run) = run else {
        return false;
    };
    let reply = runtime
        .handle(cancel_command(front.next_request(), &run))
        .await
        .0;
    front
        .state
        .notice(&format!("{reason}: cancel reply {:?}", reply.reply()));
    let deadline = Instant::now() + RECONCILE_TIMEOUT;
    while !front.merger.is_finalized() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        tokio::select! {
            event = streams.data.recv() => {
                let Some(event) = event else { break };
                let _ = absorb(front, event, streams);
            }
            event = streams.control.recv() => {
                let Some(event) = event else { break };
                let _ = absorb(front, event, streams);
            }
            () = tokio::time::sleep(remaining) => break,
        }
    }
    if !front.merger.is_finalized() {
        let flushed = front.merger.flush();
        front.report_merger();
        let _ = front.apply_events(flushed);
    }
    !front.merger.is_finalized()
}

fn draw(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    front: &mut Frontend,
) -> io::Result<()> {
    let focus = front.focus;
    terminal
        .draw(|frame| render(&mut front.state, frame.size(), frame.buffer_mut(), focus))
        .map_err(io::Error::other)?;
    Ok(())
}

/// Non-terminal fallback: drains the scripted run, denies any approval
/// (no user can confirm it), and prints the sanitized presentation
/// transcript with no escape codes.
async fn headless(runtime: Runtime, mut streams: EventStreams) -> io::Result<()> {
    let session =
        SessionId::new(DEMO_SESSION).map_err(|_| io::Error::other("demo session id rejected"))?;
    let mut front = Frontend::new(session);
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + TICK, TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let submit = submit_command(
        front.next_request(),
        front.session.clone(),
        DEMO_INPUT,
        DEMO_PROFILE,
    )
    .map_err(io::Error::other)?;
    let reply = runtime.handle(submit).await.0;
    settle_submit(&mut front, DEMO_INPUT, &reply);

    let timed_out = tokio::time::timeout(HEADLESS_TIMEOUT, async {
        let mut event_since_tick = false;
        loop {
            match next_loop_step(&mut interval, &mut streams.data, &mut streams.control).await {
                LoopStep::Closed => break,
                LoopStep::Event(event) => {
                    event_since_tick = true;
                    let pushed = front.merger.push(event);
                    if apply_headless(&runtime, &mut front, pushed).await {
                        break;
                    }
                    if front.merger.has_terminal() {
                        let drained = drain_available(
                            &mut streams.data,
                            &mut streams.control,
                            &mut front.merger,
                        );
                        if apply_headless(&runtime, &mut front, drained).await {
                            break;
                        }
                        let flushed = front.merger.flush();
                        front.report_merger();
                        if apply_headless(&runtime, &mut front, flushed).await {
                            break;
                        }
                    } else {
                        front.report_merger();
                    }
                }
                LoopStep::Tick => {
                    if !event_since_tick && front.merger.has_pending() {
                        let flushed = front.merger.flush();
                        front.report_merger();
                        if apply_headless(&runtime, &mut front, flushed).await {
                            break;
                        }
                    }
                    event_since_tick = false;
                }
            }
        }
    })
    .await;

    let mut unresolved = false;
    if timed_out.is_err() {
        front.state.notice("headless drain timed out");
        unresolved =
            reconcile_after_stop(&runtime, &mut streams, &mut front, "headless timeout").await;
    }
    if unresolved {
        eprintln!(
            "nexus-tui headless: run did not reconcile after cancellation; native provider/tool \
             work may still be running (no rollback claim)"
        );
    }
    println!("TEST-ONLY transcript (non-terminal fallback; approvals denied):");
    for line in front.state.transcript() {
        println!("{line}");
    }
    Ok(())
}

/// Applies merged headless events in order. Approvals are denied through the
/// runtime command path (no user can confirm them); returns true when the
/// terminal outcome was applied.
async fn apply_headless(runtime: &Runtime, front: &mut Frontend, events: Vec<RunEvent>) -> bool {
    for event in events {
        if let EventPayload::ApprovalRequired(notice) = event.payload() {
            let reply = runtime
                .handle(deny_notice_command(
                    front.next_request(),
                    event.run(),
                    notice,
                ))
                .await
                .0;
            front
                .state
                .notice(&format!("approval denied (no user): {:?}", reply.reply()));
        }
        let terminal = event.is_terminal();
        front.note_approval(&event);
        if !front.state.apply_event(&event) {
            front.state_rejected += 1;
        }
        if terminal {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};
    use nexus_core::{AssistantText, PersistenceState, RunFinished, RunOutcome, TurnId};

    fn session() -> SessionId {
        SessionId::new("sess-test").expect("valid")
    }

    fn run_id(id: &str) -> RunId {
        RunId::new(id).expect("valid")
    }

    fn request(id: &str) -> RequestId {
        RequestId::new(id).expect("valid")
    }

    fn started(run: &RunId, seq: u64) -> RunEvent {
        RunEvent::new(
            session(),
            run.clone(),
            seq,
            EventPayload::RunStarted {
                request: request("req-test"),
            },
        )
    }

    fn text(run: &RunId, seq: u64, body: &str) -> RunEvent {
        RunEvent::new(
            session(),
            run.clone(),
            seq,
            EventPayload::AssistantTextDelta(
                AssistantText::new(TurnId::new("t1-0").expect("valid"), "item-0", body)
                    .expect("fragment builds"),
            ),
        )
    }

    fn finished(run: &RunId, seq: u64) -> RunEvent {
        RunEvent::new(
            session(),
            run.clone(),
            seq,
            EventPayload::RunFinished(
                RunFinished::new(RunOutcome::Completed, PersistenceState::Ephemeral, None)
                    .expect("terminal record builds"),
            ),
        )
    }

    fn interval() -> tokio::time::Interval {
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + TICK, TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval
    }

    #[test]
    fn merger_yields_cross_channel_order_and_rejects_old_and_duplicate() {
        let run = run_id("run-1");
        let mut merger = EventMerger::new();
        merger.adopt(&run);
        // Cross-channel arrival order is arbitrary: terminal first, then a
        // buffered duplicate, a middle fragment, and the opening events.
        let arrivals = vec![
            finished(&run, 3),
            finished(&run, 3),
            text(&run, 2, " second"),
            started(&run, 0),
            text(&run, 1, "first"),
        ];
        let mut emitted = Vec::new();
        for event in arrivals {
            emitted.extend(merger.push(event));
        }
        let seqs: Vec<u64> = emitted.iter().map(|event| event.seq()).collect();
        assert_eq!(seqs, vec![0, 1, 2, 3], "contiguous cross-channel order");
        assert!(merger.is_finalized());
        assert_eq!(merger.gaps(), 0);

        // Older runs, duplicates, and post-terminal events never reopen it.
        assert!(merger.push(text(&run_id("run-0"), 0, "old")).is_empty());
        assert!(merger.push(text(&run, 2, "dup")).is_empty());
        assert!(merger.push(text(&run, 4, "late")).is_empty());
        assert_eq!(
            merger.rejected(),
            4,
            "buffered duplicate, old run, duplicate, and post-final"
        );
        assert_eq!(
            merger.push(finished(&run, 5)).len(),
            0,
            "terminal cannot repeat"
        );
    }

    #[test]
    fn force_flush_counts_gaps_without_reordering() {
        let run = run_id("run-2");
        let mut merger = EventMerger::new();
        merger.adopt(&run);
        assert_eq!(merger.push(text(&run, 0, "a")).len(), 1);
        assert!(
            merger.push(text(&run, 2, "c")).is_empty(),
            "seq 1 missing: held in the finite buffer"
        );
        assert!(merger.has_pending());
        let flushed = merger.flush();
        let seqs: Vec<u64> = flushed.iter().map(|event| event.seq()).collect();
        assert_eq!(seqs, vec![2]);
        assert_eq!(merger.gaps(), 1);
        // The missing sequence arriving late is rejected, never reordered in.
        assert!(merger.push(text(&run, 1, "b")).is_empty());
        assert_eq!(merger.rejected(), 1);
    }

    #[test]
    fn reorder_buffer_is_finite_and_stays_ordered() {
        let run = run_id("run-3");
        let mut merger = EventMerger::new();
        merger.adopt(&run);
        let mut emitted = Vec::new();
        for seq in 1..=(MAX_REORDER_EVENTS as u64 + 32) {
            emitted.extend(merger.push(text(&run, seq, "x")));
        }
        assert!(
            merger.gaps() >= 1,
            "missing sequence 0 was skipped explicitly"
        );
        let seqs: Vec<u64> = emitted.iter().map(|event| event.seq()).collect();
        assert!(
            seqs.windows(2).all(|pair| pair[0] < pair[1]),
            "emission order is strictly increasing"
        );
        let flushed = merger.flush();
        let mut all = seqs;
        all.extend(flushed.iter().map(|event| event.seq()));
        assert!(all.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[tokio::test]
    async fn terminal_drains_preceding_text_before_final_outcome() {
        let run = run_id("run-4");
        let (data_tx, data_rx) = mpsc::channel(8);
        let (control_tx, control_rx) = mpsc::channel(8);
        let mut streams = EventStreams {
            data: data_rx,
            control: control_rx,
        };
        let mut front = Frontend::new(session());
        front.merger.adopt(&run);
        // The preceding sends are still queued when the terminal is received.
        data_tx.try_send(started(&run, 0)).expect("capacity");
        data_tx
            .try_send(text(&run, 1, "before terminal"))
            .expect("capacity");
        control_tx.try_send(finished(&run, 2)).expect("capacity");

        let terminal_event = streams.control.recv().await.expect("terminal queued");
        assert!(absorb(&mut front, terminal_event, &mut streams));
        assert!(front.merger.is_finalized());
        assert_eq!(front.state.last_seq(), Some(2));

        let transcript = front.state.transcript().join("\n");
        let before = transcript
            .find("before terminal")
            .expect("preceding text presented");
        let finish = transcript
            .find("finished: Completed")
            .expect("terminal outcome presented");
        assert!(
            before < finish,
            "preceding text is applied before the final outcome"
        );

        // A finalized run never reopens on late traffic.
        assert!(front.merger.push(text(&run, 3, "late")).is_empty());
        assert!(front.state.is_finished());
        assert_eq!(front.state.last_seq(), Some(2));
    }

    #[tokio::test]
    async fn persistent_interval_ticks_under_continuously_ready_events() {
        let (data_tx, data_rx) = mpsc::channel(4);
        let (control_tx, control_rx) = mpsc::channel(4);
        let data_task = tokio::spawn(async move {
            loop {
                if data_tx
                    .send(started(&run_id("synthetic"), 0))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let control_task = tokio::spawn(async move {
            loop {
                if control_tx
                    .send(started(&run_id("synthetic"), 0))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut interval = interval();
        let observed = tokio::time::timeout(Duration::from_secs(2), async move {
            let mut data = data_rx;
            let mut control = control_rx;
            loop {
                if matches!(
                    next_loop_step(&mut interval, &mut data, &mut control).await,
                    LoopStep::Tick
                ) {
                    return true;
                }
            }
        })
        .await;
        data_task.abort();
        control_task.abort();
        assert_eq!(
            observed,
            Ok(true),
            "a persistent interval must not be starved by ready events"
        );
    }

    #[tokio::test]
    async fn key_batches_are_bounded_and_stop_on_quit() {
        let (runtime, _streams) = build_runtime();
        let mut front = Frontend::new(session());
        let keys: Vec<KeyEvent> = (0..100)
            .map(|_| KeyEvent::new(KeyCode::F(1), KeyModifiers::empty()))
            .collect();
        let mut index = 0;
        let batch = handle_key_batch(
            &mut front,
            &runtime,
            || {
                if index >= keys.len() {
                    return Ok(None);
                }
                let key = keys[index];
                index += 1;
                Ok(Some(key))
            },
            MAX_KEYS_PER_TICK,
        )
        .await
        .expect("key source never fails");
        assert_eq!(batch.handled, MAX_KEYS_PER_TICK);
        assert!(!batch.quit);
        assert_eq!(index, MAX_KEYS_PER_TICK, "the batch stops at the bound");

        let quit_keys = [
            KeyEvent::new(KeyCode::F(1), KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
        ];
        let mut index = 0;
        let batch = handle_key_batch(
            &mut front,
            &runtime,
            || {
                if index >= quit_keys.len() {
                    return Ok(None);
                }
                let key = quit_keys[index];
                index += 1;
                Ok(Some(key))
            },
            MAX_KEYS_PER_TICK,
        )
        .await
        .expect("key source never fails");
        assert!(batch.quit);
        assert_eq!(batch.handled, 2);
    }

    #[tokio::test]
    async fn closed_channels_surface_as_closed_step() {
        let (data_tx, mut data_rx) = mpsc::channel::<RunEvent>(1);
        let (_control_tx, mut control_rx) = mpsc::channel::<RunEvent>(1);
        drop(data_tx);
        let mut interval = interval();
        let step = next_loop_step(&mut interval, &mut data_rx, &mut control_rx).await;
        assert!(matches!(step, LoopStep::Closed));
    }

    #[test]
    fn busy_submit_keeps_draft_and_accepted_adopts_run() {
        let mut front = Frontend::new(session());
        front.state.composer_type('h');
        front.state.composer_type('i');
        let busy = CommandResponse::new(
            request("req-busy"),
            CommandReply::Busy,
            Some(run_id("run-1")),
        );
        assert!(!settle_submit(&mut front, "hi", &busy));
        assert_eq!(front.state.composer(), "hi", "Busy preserves the draft");
        assert!(front.merger.active_run().is_none(), "Busy adopts nothing");
        assert!(
            front
                .state
                .transcript()
                .iter()
                .all(|line| !line.starts_with("[User]")),
            "Busy records no submission"
        );

        let accepted = CommandResponse::new(
            request("req-ok"),
            CommandReply::Accepted,
            Some(run_id("run-2")),
        );
        assert!(settle_submit(&mut front, "hi", &accepted));
        assert_eq!(front.merger.active_run(), Some(&run_id("run-2")));
        assert!(
            front
                .state
                .transcript()
                .iter()
                .any(|line| line.starts_with("[User]")),
            "Accepted records the draft"
        );
    }

    #[tokio::test]
    async fn quit_while_approval_awaits_reconciles() {
        let (runtime, mut streams) = build_runtime();
        let mut front = Frontend::new(session());
        let submit = submit_command(
            front.next_request(),
            front.session.clone(),
            DEMO_INPUT,
            DEMO_PROFILE,
        )
        .expect("valid submit builds");
        let reply = runtime.handle(submit).await.0;
        assert!(settle_submit(&mut front, DEMO_INPUT, &reply));

        let mut interval = interval();
        let deadline = Instant::now() + Duration::from_secs(5);
        while front.live_approval.is_none() && Instant::now() < deadline {
            match next_loop_step(&mut interval, &mut streams.data, &mut streams.control).await {
                LoopStep::Closed => break,
                LoopStep::Event(event) => {
                    let pushed = front.merger.push(event);
                    front.apply_events(pushed);
                }
                LoopStep::Tick => {}
            }
        }
        assert!(
            front.live_approval.is_some(),
            "the scripted mutation reaches an approval"
        );

        let unresolved = reconcile_after_stop(&runtime, &mut streams, &mut front, "quit").await;
        assert!(
            !unresolved,
            "cancellation reconciles while an approval awaits"
        );
        assert!(front.merger.is_finalized());
    }

    #[tokio::test]
    async fn quit_path_cancels_and_reconciles_within_the_bound() {
        let (runtime, mut streams) = build_runtime();
        let mut front = Frontend::new(session());
        let submit = submit_command(
            front.next_request(),
            front.session.clone(),
            DEMO_INPUT,
            DEMO_PROFILE,
        )
        .expect("valid submit builds");
        let reply = runtime.handle(submit).await.0;
        assert!(settle_submit(&mut front, DEMO_INPUT, &reply));
        let unresolved = reconcile_after_stop(&runtime, &mut streams, &mut front, "quit").await;
        assert!(
            !unresolved,
            "cancellation reconciles while the run awaits approval"
        );
        assert!(front.merger.is_finalized());
        assert!(front.state.is_finished());
    }
}
