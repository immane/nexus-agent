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
//!   only on an `Accepted` reply; `Busy` keeps the draft untouched. A new
//!   `Accepted` submit after a terminal outcome adopts the next run.
//! - A newly arrived approval takes keyboard focus at once -- the card is
//!   modal, so Tab-hunting is never required; later events never steal
//!   focus back, and a spent or terminal run returns focus to the composer.
//! - Data and control events are merged by the runtime's contiguous per-run
//!   sequence with a finite reorder buffer; older runs, duplicates, and
//!   post-terminal events are rejected, and preceding text is applied in
//!   order before the terminal outcome. A finalized run never reopens.
//! - Quit and error exits cancel live work and reconcile within a bounded
//!   window before the terminal is restored; unresolved native blocking
//!   work is reported instead of being presented as clean cleanup, and the
//!   Tokio runtime teardown uses an explicit `shutdown_timeout`.
//! - User configuration is loaded exactly once at startup on both the
//!   interactive and headless paths: a missing file silently yields
//!   defaults, a present but invalid file is an explicit startup error, and
//!   a run's terminal outcome records the active model in the document
//!   (a failed write is a transcript notice, never a crash).

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::{self, IsTerminal};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyEvent, KeyEventKind, poll, read};
use nexus_config::{UserConfig, load as load_config, resolve_path, save as save_config};
use nexus_core::{
    ApprovalNotice, CommandReply, CommandResponse, EventPayload, Limits, ModelRequest,
    ProviderCapabilities, ProviderContext, ProviderEvent, ProviderPort, RequestId, RunEvent, RunId,
    SessionId,
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

/// Fallible production composition: the runtime validates limits, provider
/// capabilities, and every tool registration before any run can start, so an
/// invalid demo wiring is an explicit startup error, never a hung run.
fn build_runtime() -> io::Result<(Runtime, EventStreams)> {
    let config = RuntimeConfig {
        limits: Limits::m0_test(),
        policy: Policy::m0_test(),
        has_approval_handler: true,
    };
    let provider = Arc::new(PerRunProvider::new());
    let tools: Vec<Arc<dyn nexus_core::ToolPort + Send + Sync>> = vec![
        Arc::new(FakeTool::read_only()),
        Arc::new(FakeTool::mutation()),
    ];
    Runtime::try_new(config, provider, tools).map_err(io::Error::other)
}

/// User configuration resolved once at startup, plus the path it was read
/// from (so a later write goes back to the same document) and the model the
/// session boots with.
///
/// The TUI takes no `--config` flag: the document's location is
/// [`nexus_config`]'s documented precedence (CLI path, then `NEXUS_CONFIG`,
/// then the platform default), and this binary has no argument parser to
/// receive a path. Adding a second, TUI-local way to choose the file would
/// duplicate that precedence with no consumer here, and would risk writing
/// model usage back to a file the user did not mean to write.
#[derive(Debug)]
struct SessionConfig {
    config: UserConfig,
    path: Option<std::path::PathBuf>,
    active_model: Option<String>,
}

/// Loads the user configuration for both the interactive and the headless
/// path, exactly once per process.
///
/// A missing file is not a failure: it means "not configured yet", so the
/// defaults are used silently. A file that exists but cannot be parsed or
/// validated is an explicit startup error -- silently starting with defaults
/// would hide a broken document and then overwrite it on the first terminal
/// outcome, destroying the user's actual settings.
fn load_session_config() -> io::Result<SessionConfig> {
    session_config_at(resolve_path(None))
}

/// Same contract as [`load_session_config`] with the resolved path supplied
/// directly, so both the missing-file and invalid-file branches are testable
/// without mutating the process environment.
fn session_config_at(path: Option<std::path::PathBuf>) -> io::Result<SessionConfig> {
    let config = match path.as_deref() {
        Some(path) => load_config(path)
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid user configuration: {error}"),
                )
            })?
            .unwrap_or_else(UserConfig::default_config),
        // No platform config location at all: run on defaults and never
        // write, rather than guessing a path.
        None => UserConfig::default_config(),
    };
    let active_model = boot_model(&config);
    Ok(SessionConfig {
        config,
        path,
        active_model,
    })
}

/// Project directory for the header: the working directory this process
/// was launched in, which is also the default root real tools are jailed
/// to. Canonicalized so symlinked launches display the real location.
fn session_project_dir() -> Option<String> {
    project_dir_from(std::env::current_dir())
}

/// Pure half of [`session_project_dir`]: canonicalize when possible, fall
/// back to the raw directory (a deleted cwd still displays), or `None`
/// when there is no directory or it is not representable text.
fn project_dir_from(current: io::Result<std::path::PathBuf>) -> Option<String> {
    let dir = current.ok()?;
    std::fs::canonicalize(&dir)
        .unwrap_or(dir)
        .to_str()
        .map(str::to_owned)
}

/// Boot model: the most recently used model, else the first favourite, else
/// the first configured model, else nothing (provider-dependent work then
/// reports not-ready).
fn boot_model(config: &UserConfig) -> Option<String> {
    config
        .recent()
        .first()
        .or_else(|| config.favourites().first())
        .or_else(|| config.models().first().map(|entry| &entry.id))
        .cloned()
}

/// Test-only demo provider: serves a fresh scripted demo script for every
/// run. The shared [`FakeProvider`] consumes its script queue across calls,
/// so without a reset the second task in one process would observe an
/// exhausted script and fail instantly. Resetting per run-id keeps each
/// demo task replayable; real adapters never replay.
struct PerRunProvider {
    capabilities: ProviderCapabilities,
    current: Mutex<PerRunState>,
}

struct PerRunState {
    run: Option<RunId>,
    provider: FakeProvider,
}

impl PerRunProvider {
    fn new() -> Self {
        let capabilities = FakeProvider::demo_two_turn().capabilities();
        Self {
            capabilities,
            current: Mutex::new(PerRunState {
                run: None,
                provider: FakeProvider::demo_two_turn(),
            }),
        }
    }
}

impl ProviderPort for PerRunProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities.clone()
    }

    fn stream(&self, request: &ModelRequest, context: &ProviderContext) -> Vec<ProviderEvent> {
        let mut current = self.current.lock().expect("demo provider lockable");
        if current.run.as_ref() != Some(request.run()) {
            current.run = Some(request.run().clone());
            current.provider = FakeProvider::demo_two_turn();
        }
        current.provider.stream(request, context)
    }
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
    // Loaded once, before the terminal is touched: an invalid document must
    // fail as a startup error on the plain stderr surface, never inside a
    // restored-screen report. Both paths below share this one load.
    let session_config = load_session_config()?;
    let (runtime, streams) = build_runtime()?;
    if io::stdout().is_terminal() {
        match nexus_tui::TerminalGuard::setup() {
            Ok(mut guard) => {
                let report = interactive(runtime, streams, session_config).await;
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
                headless(runtime, streams, session_config).await
            }
        }
    } else {
        headless(runtime, streams, session_config).await
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
    /// Typed user configuration: model admission order, favourites, and
    /// recents. Read once at startup, written back when a run completes.
    config: UserConfig,
    /// Where [`Self::config`] came from. `None` means the platform exposes no
    /// config location, in which case usage is never written back.
    config_path: Option<std::path::PathBuf>,
    /// Currently selected model id, or `None` when none is configured.
    active_model: Option<String>,
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
            config: UserConfig::default_config(),
            config_path: None,
            active_model: None,
        }
    }

    /// Builds a frontend over the already-loaded startup configuration, so
    /// the interactive and headless paths share one load and one document.
    fn with_config(session: SessionId, config: SessionConfig) -> Self {
        let mut front = Self {
            config: config.config,
            config_path: config.path,
            active_model: config.active_model,
            ..Self::new(session)
        };
        front.sync_model_display();
        if let Some(dir) = session_project_dir() {
            front.state.set_project_dir(dir);
        }
        front
    }

    /// Advances the active model through the configured models in admission
    /// order, wrapping at the end. With nothing configured this reports that
    /// fact and changes nothing: cycling an empty list must not clear or
    /// invent a selection.
    fn cycle_model(&mut self) {
        let ids: Vec<&str> = self
            .config
            .models()
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        if ids.is_empty() {
            self.state.notice("no configured models");
            return;
        }
        let next = match self
            .active_model
            .as_deref()
            .and_then(|current| ids.iter().position(|id| *id == current))
        {
            Some(index) => ids[(index + 1) % ids.len()],
            // No usable selection yet: start at the first configured model.
            None => ids[0],
        };
        self.active_model = Some(next.to_owned());
        self.state.notice(&format!("model: {next}"));
        self.sync_model_display();
    }

    /// Mirrors the selected model (and its provider, when the id still
    /// resolves) into presentation state. Unknown ids clear the display
    /// instead of showing a stale name.
    fn sync_model_display(&mut self) {
        let display = self.active_model.as_deref().and_then(|id| {
            self.config
                .model(id)
                .map(|entry| (id.to_owned(), entry.provider.clone()))
        });
        match display {
            Some((model, provider)) => {
                self.state.set_active_model(Some(model), Some(provider));
            }
            None => self.state.set_active_model(None, None),
        }
    }

    /// Records the active model as recently used and writes the document
    /// back. Called when a run reaches its terminal outcome, so the recents
    /// list reflects runs that actually ran.
    ///
    /// Every failure here is a transcript notice: the model list is a
    /// convenience, so a document that cannot be read back or written must
    /// never take down a session that already produced its output. The
    /// in-memory document keeps the recorded use either way.
    fn record_active_model_use(&mut self) {
        let Some(id) = self.active_model.clone() else {
            return;
        };
        if self.config.model(&id).is_none() {
            return;
        }
        // `record_use` only fails for an unknown id, which the membership
        // check above already excluded; treat a failure as a write failure
        // rather than asserting on library internals.
        if self.config.record_use(&id).is_err() {
            self.state.notice("config save failed");
            return;
        }
        let Some(path) = self.config_path.clone() else {
            return;
        };
        if save_config(&self.config, &path).is_err() {
            self.state.notice("config save failed");
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
    /// outcome was applied; the run is never reopened afterwards. A
    /// terminal outcome returns keyboard focus to the composer so the
    /// next task can be typed immediately. A newly arrived approval takes
    /// focus at once -- the card is modal, so its decision keys must work
    /// without Tab-hunting -- but later events never steal focus back
    /// once the user has deliberately moved away.
    fn apply_events(&mut self, events: impl IntoIterator<Item = RunEvent>) -> bool {
        let had_pending = self.state.pending_approval().is_some();
        for event in events {
            let terminal = event.is_terminal();
            self.note_approval(&event);
            if !self.state.apply_event(&event) {
                self.state_rejected += 1;
            }
            if terminal {
                self.focus = Focus::Composer;
                // The run completed, so this model is genuinely "used":
                // record it before the notice history moves on.
                self.record_active_model_use();
                return true;
            }
        }
        if !had_pending && self.state.pending_approval().is_some() {
            self.focus = Focus::ApprovalCard;
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
        Action::CycleModel => front.cycle_model(),
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
            // The card is gone: hand the keyboard back to the composer so
            // the next task can be typed without a Tab round-trip.
            front.focus = Focus::Composer;
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
/// full test-only keyset. The loop stays up across terminal outcomes so
/// further tasks can be submitted; it exits on quit, channel close, or
/// error, cancelling live work first.
async fn interactive(
    runtime: Runtime,
    mut streams: EventStreams,
    session_config: SessionConfig,
) -> io::Result<InteractiveReport> {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).map_err(io::Error::other)?;
    let session =
        SessionId::new(DEMO_SESSION).map_err(|_| io::Error::other("demo session id rejected"))?;
    let mut front = Frontend::with_config(session, session_config);
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
            // A closed channel ends input, but the other one may still hold
            // queued events: apply everything already committed before exit
            // so a close racing the terminal cannot drop the outcome.
            LoopStep::Closed => {
                let drained =
                    drain_available(&mut streams.data, &mut streams.control, &mut front.merger);
                front.apply_events(drained);
                let flushed = front.merger.flush();
                front.apply_events(flushed);
                front.report_merger();
                draw(terminal, front)?;
                return Ok(());
            }
            LoopStep::Event(event) => {
                event_since_tick = true;
                dirty = true;
                // A terminal outcome stays on screen and the loop keeps
                // serving: the next `Accepted` submit adopts a new run.
                // Only quit, close, or an I/O error ends the loop.
                if absorb(front, event, streams) {
                    draw(terminal, front)?;
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
async fn headless(
    runtime: Runtime,
    mut streams: EventStreams,
    session_config: SessionConfig,
) -> io::Result<()> {
    let session =
        SessionId::new(DEMO_SESSION).map_err(|_| io::Error::other("demo session id rejected"))?;
    let mut front = Frontend::with_config(session, session_config);
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
            front.record_active_model_use();
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
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
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
        let (runtime, mut streams) = build_runtime().expect("demo wiring is valid");
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
        let (runtime, mut streams) = build_runtime().expect("demo wiring is valid");
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

/// Coverage for the binary's private loop internals.
///
/// [`EventMerger`], the persistent tick, key batching, and quit
/// reconciliation live in this binary target and are therefore unreachable
/// from the library or integration tests. Each case below asserts the
/// documented invariant directly, reads private merger state where the
/// bound itself is the claim, and waits only inside an explicit bound.
#[cfg(test)]
mod cov_main_private {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};
    use nexus_core::{AssistantText, PersistenceState, RunFinished, RunOutcome, TurnId};

    fn session() -> SessionId {
        SessionId::new("sess-cov").expect("valid")
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
                request: request("req-cov"),
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

    fn seqs(events: &[RunEvent]) -> Vec<u64> {
        events.iter().map(|event| event.seq()).collect()
    }

    fn interval() -> tokio::time::Interval {
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + TICK, TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval
    }

    /// Receivers whose senders are already gone: every read reports closed.
    fn closed_streams() -> EventStreams {
        let (data_tx, data_rx) = mpsc::channel::<RunEvent>(1);
        let (control_tx, control_rx) = mpsc::channel::<RunEvent>(1);
        drop(data_tx);
        drop(control_tx);
        EventStreams {
            data: data_rx,
            control: control_rx,
        }
    }

    /// A frontend holding a run the runtime never issued, so cancellation can
    /// never produce a terminal outcome for it.
    fn adopted_orphan() -> (RunId, Frontend) {
        let run = run_id("run-orphan");
        let mut front = Frontend::new(session());
        front.merger.adopt(&run);
        (run, front)
    }

    fn user_line_count(front: &Frontend) -> usize {
        front
            .state
            .transcript()
            .iter()
            .filter(|line| line.starts_with("[User]"))
            .count()
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn merger_emits_reversed_cross_channel_traffic_once() {
        let run = run_id("run-reversed");
        let mut merger = EventMerger::new();
        merger.adopt(&run);

        // Fully reversed arrival, as two channels with no shared ordering can
        // deliver it: nothing is emitted until the head sequence shows up.
        let mut emitted = Vec::new();
        for seq in [4, 3, 2, 1, 0] {
            emitted.extend(merger.push(match seq {
                4 => finished(&run, seq),
                _ => text(&run, seq, "fragment"),
            }));
        }

        assert_eq!(seqs(&emitted), vec![0, 1, 2, 3, 4], "contiguous order");
        assert_eq!(
            emitted.iter().filter(|event| event.is_terminal()).count(),
            1,
            "the outcome is emitted exactly once"
        );
        assert_eq!(
            *emitted.last().expect("outcome emitted"),
            finished(&run, 4),
            "the terminal outcome is always last"
        );
        assert_eq!(merger.gaps(), 0);
        assert_eq!(merger.rejected(), 0);
        assert!(merger.is_finalized());
        assert!(!merger.has_pending());
        assert!(!merger.has_terminal());
    }

    #[test]
    fn merger_rejects_pre_adoption_old_duplicate_and_post_terminal_traffic() {
        let run = run_id("run-guard");
        let stale = run_id("run-previous");
        let mut merger = EventMerger::new();

        // Nothing is adopted yet: events are rejected, never guessed into a run.
        assert!(merger.push(started(&run, 0)).is_empty());
        assert_eq!(merger.rejected(), 1);
        assert!(merger.active_run().is_none());
        assert!(!merger.is_finalized());

        merger.adopt(&run);
        assert_eq!(merger.push(started(&run, 0)).len(), 1);
        assert!(merger.push(started(&stale, 1)).is_empty(), "no other run");
        assert!(
            merger.push(text(&run, 0, "older")).is_empty(),
            "an older sequence never reopens the stream"
        );
        assert!(merger.push(text(&run, 2, "held")).is_empty());
        assert!(
            merger.push(text(&run, 2, "duplicate")).is_empty(),
            "a duplicate inside the window is counted, not merged"
        );
        assert_eq!(
            seqs(&merger.push(started(&run, 1))),
            vec![1, 2],
            "the window drains contiguously"
        );
        assert_eq!(merger.gaps(), 0);
        assert_eq!(
            merger.rejected(),
            4,
            "pre-adoption, other run, older sequence, duplicate"
        );

        assert_eq!(merger.push(finished(&run, 3)).len(), 1);
        assert!(merger.is_finalized());
        assert!(merger.push(finished(&run, 4)).is_empty(), "one outcome");
        assert!(
            merger.push(text(&run, 4, "late")).is_empty(),
            "post-terminal text is rejected"
        );
        assert!(
            merger.push(started(&stale, 5)).is_empty(),
            "a next run is adopted explicitly, never implicitly"
        );
        assert_eq!(merger.rejected(), 7);
        assert!(!merger.has_pending());
        assert_eq!(
            merger.push(started(&stale, 5)).len(),
            0,
            "the rejection is stable, not a retry"
        );
        assert_eq!(merger.rejected(), 8);
    }

    #[test]
    fn adopt_resets_a_held_window_for_the_next_accepted_run() {
        let first = run_id("run-first");
        let second = run_id("run-second");
        let mut merger = EventMerger::new();
        merger.adopt(&first);
        assert_eq!(merger.push(text(&first, 0, "head")).len(), 1);
        assert!(merger.push(text(&first, 2, "held")).is_empty());
        assert!(merger.has_pending());

        merger.adopt(&second);
        assert_eq!(merger.active_run(), Some(&second));
        assert!(!merger.has_pending(), "the reorder window is dropped");
        assert!(!merger.has_terminal());
        assert!(!merger.is_finalized(), "a new accepted run reopens it");
        assert!(
            merger.push(text(&first, 1, "stray")).is_empty(),
            "the previous run can no longer merge"
        );
        assert_eq!(
            merger.push(started(&second, 0)).len(),
            1,
            "the next run starts at sequence 0"
        );
        assert_eq!(merger.gaps(), 0, "a discarded window is not a gap");
    }

    #[test]
    fn flush_counts_missing_sequences_without_reordering() {
        let run = run_id("run-gap");
        let mut merger = EventMerger::new();
        merger.adopt(&run);
        assert_eq!(seqs(&merger.push(text(&run, 0, "head"))), vec![0]);
        assert!(
            merger.push(text(&run, 3, "tail")).is_empty(),
            "sequences 1 and 2 never arrived"
        );
        assert!(merger.has_pending());

        let flushed = merger.flush();
        assert_eq!(
            seqs(&flushed),
            vec![3],
            "only what arrived is emitted, in sequence order"
        );
        assert_eq!(merger.gaps(), 2, "each missing sequence is counted");
        assert!(!merger.has_pending());
        assert!(merger.flush().is_empty(), "an empty flush is a no-op");
        assert_eq!(merger.gaps(), 2, "an empty flush counts nothing");

        // The missing sequences arriving late are rejected, never reordered in.
        assert!(merger.push(text(&run, 1, "late")).is_empty());
        assert!(merger.push(text(&run, 2, "late")).is_empty());
        assert_eq!(merger.rejected(), 2);
        assert_eq!(merger.gaps(), 2, "rejection never rewrites the count");

        // A window whose head is contiguous counts only the real head gap.
        let mut contiguous = EventMerger::new();
        contiguous.adopt(&run);
        assert!(contiguous.push(text(&run, 5, "x")).is_empty());
        assert!(contiguous.push(text(&run, 6, "y")).is_empty());
        assert_eq!(seqs(&contiguous.flush()), vec![5, 6]);
        assert_eq!(contiguous.gaps(), 5, "sequences 0 through 4 never arrived");
        assert!(!contiguous.has_pending());
    }

    #[test]
    fn reorder_window_stays_inside_its_finite_bound() {
        let run = run_id("run-window");
        let mut merger = EventMerger::new();
        merger.adopt(&run);

        for seq in 1..=MAX_REORDER_EVENTS as u64 {
            assert!(merger.push(text(&run, seq, "x")).is_empty());
            assert!(
                merger.buffered.len() <= MAX_REORDER_EVENTS,
                "the reorder window is finite"
            );
        }
        assert_eq!(
            merger.buffered.len(),
            MAX_REORDER_EVENTS,
            "a full window still holds every buffered event"
        );

        // One arrival past the bound flushes the window instead of growing it.
        let overflow = merger.push(text(&run, MAX_REORDER_EVENTS as u64 + 1, "y"));
        assert!(
            merger.buffered.is_empty(),
            "the window is force-flushed at the bound"
        );
        assert_eq!(merger.gaps(), 1, "sequence 0 was skipped explicitly");
        let emitted = seqs(&overflow);
        assert_eq!(emitted.len(), MAX_REORDER_EVENTS + 1);
        assert_eq!(emitted.first(), Some(&1));
        assert_eq!(emitted.last(), Some(&(MAX_REORDER_EVENTS as u64 + 1)));
        assert!(
            emitted.windows(2).all(|pair| pair[0] < pair[1]),
            "emission stays strictly ordered across a flush"
        );
        assert!(!merger.has_pending());
    }

    #[tokio::test]
    async fn absorb_drains_preceding_text_before_the_terminal_outcome() {
        let run = run_id("run-absorb");
        let (data_tx, data_rx) = mpsc::channel(8);
        let (control_tx, control_rx) = mpsc::channel(8);
        let mut streams = EventStreams {
            data: data_rx,
            control: control_rx,
        };
        let mut front = Frontend::new(session());
        front.merger.adopt(&run);

        // The control channel delivers the outcome first; every earlier send is
        // still queued, and sequence 3 is dropped by the data channel.
        data_tx.try_send(started(&run, 0)).expect("capacity");
        data_tx.try_send(text(&run, 1, "alpha")).expect("capacity");
        data_tx.try_send(text(&run, 2, "omega")).expect("capacity");
        control_tx.try_send(finished(&run, 4)).expect("capacity");

        let outcome = streams.control.recv().await.expect("outcome queued");
        assert!(absorb(&mut front, outcome, &mut streams));

        assert!(front.merger.is_finalized());
        assert!(!front.merger.has_terminal(), "the pending outcome drained");
        assert!(!front.merger.has_pending());
        assert_eq!(front.merger.gaps(), 1, "only sequence 3 was missing");
        assert_eq!(front.state.last_seq(), Some(4));
        assert!(front.state.is_finished());
        assert_eq!(
            front.state_rejected, 0,
            "every drained event belonged to the adopted run"
        );

        // Same-item fragments may coalesce into one entry, so ordering is
        // asserted on the presented byte stream rather than per line.
        let transcript = front.state.transcript().join("\n");
        let at = |needle: &str| {
            transcript
                .find(needle)
                .unwrap_or_else(|| panic!("{needle} is presented"))
        };
        assert!(at("alpha") < at("omega"), "channel order is restored");
        assert!(
            at("omega") < at("finished: Completed"),
            "preceding text is applied before the final outcome"
        );
        assert!(
            transcript.contains("presentation gap: 1 event(s) missing"),
            "a dropped sequence is reported, not hidden"
        );

        // A finalized run never reopens on late traffic.
        let before = front.state.transcript().len();
        assert!(!absorb(&mut front, text(&run, 5, "late"), &mut streams));
        assert!(front.merger.is_finalized());
        assert_eq!(front.state.last_seq(), Some(4), "the run is not extended");
        assert_eq!(
            front.state.transcript().len() - before,
            2,
            "one notice entry records the rejection"
        );
        assert!(
            front
                .state
                .transcript()
                .iter()
                .any(|line| line.contains("1 old, duplicate, or post-terminal event(s) rejected")),
            "the rejection is surfaced explicitly"
        );
    }

    #[tokio::test]
    async fn accepted_submit_adopts_the_run_and_takes_the_draft() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let mut front = Frontend::new(session());
        front.state.composer_type('h');
        front.state.composer_type('i');

        let first = front.next_request();
        assert_eq!(first.as_str(), "req-tui-1", "requests are per frontend");
        let command = submit_command(
            first,
            front.session.clone(),
            front.state.composer(),
            DEMO_PROFILE,
        )
        .expect("valid submit builds");
        let reply = runtime.handle(command).await.0;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        assert!(settle_submit(&mut front, "hi", &reply));
        assert_eq!(front.merger.active_run(), reply.run());
        assert_eq!(user_line_count(&front), 1, "the accepted draft is recorded");

        let stashed = front.state.composer_take();
        assert_eq!(stashed, "hi", "the recorded draft is the cleared draft");

        // An `Accepted` reply carrying no run identity records the draft but
        // adopts nothing, so no event can be attributed to it.
        front.state.composer_type('?');
        let orphan = CommandResponse::new(front.next_request(), CommandReply::Accepted, None);
        assert!(settle_submit(&mut front, "?", &orphan));
        assert_eq!(
            front.merger.active_run(),
            reply.run(),
            "no run identity is invented"
        );
        assert_eq!(user_line_count(&front), 2);
        assert_eq!(front.state.composer(), "?", "the caller owns the clear");
    }

    #[tokio::test]
    async fn busy_and_rejected_submits_preserve_the_draft() {
        let mut front = Frontend::new(session());
        front.state.composer_type('h');
        front.state.composer_type('i');
        let draft = front.state.composer().to_owned();
        let live = run_id("run-live");

        let busy =
            CommandResponse::new(front.next_request(), CommandReply::Busy, Some(live.clone()));
        assert!(!settle_submit(&mut front, &draft, &busy));
        assert_eq!(front.state.composer(), "hi", "Busy preserves the draft");
        assert!(
            front.merger.active_run().is_none(),
            "Busy adopts nothing, even with a run attached"
        );
        assert_eq!(user_line_count(&front), 0, "Busy records no submission");
        assert!(
            front
                .state
                .transcript()
                .iter()
                .any(|line| line.contains("submit not accepted (Busy); draft preserved")),
            "the refusal is reported with the preserved draft"
        );

        let rejected = CommandResponse::new(
            front.next_request(),
            CommandReply::Rejected,
            Some(live.clone()),
        );
        assert!(!settle_submit(&mut front, &draft, &rejected));
        assert_eq!(front.state.composer(), "hi", "a refusal preserves it too");
        assert!(front.merger.active_run().is_none());
        assert_eq!(user_line_count(&front), 0);
        assert_eq!(
            front.request_counter, 2,
            "each submit attempt has its own request identity"
        );

        // Retyping after a refusal keeps the same draft: only the accepted
        // reply records it, and clearing it stays the caller's job.
        front.state.composer_backspace();
        let retried = front.state.composer().to_owned();
        assert_eq!(retried, "h", "editing after a refusal stays local");
        assert!(!front.adopt_accepted(&busy), "a refusal is not retried");
        let accepted = CommandResponse::new(
            front.next_request(),
            CommandReply::Accepted,
            Some(live.clone()),
        );
        assert!(settle_submit(&mut front, &retried, &accepted));
        assert_eq!(front.merger.active_run(), Some(&live));
        assert_eq!(user_line_count(&front), 1, "recorded exactly once");
        assert_eq!(front.state.composer(), "h", "the draft is untouched");
        assert_eq!(front.state.composer_take(), "h");
    }

    #[tokio::test]
    async fn blank_submit_never_reaches_the_runtime_and_quit_keys_exit() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let mut front = Frontend::new(session());
        for _ in 0..3 {
            front.state.composer_type(' ');
        }
        let enter = press(KeyCode::Enter);

        assert!(!handle_key(&mut front, &runtime, enter).await);
        assert_eq!(front.state.composer(), "   ", "a blank draft is untouched");
        assert_eq!(
            front.request_counter, 0,
            "no command is built for a blank draft"
        );
        assert!(front.merger.active_run().is_none());
        assert_eq!(user_line_count(&front), 0);

        // `Ctrl+D` leaves the loop from any focus.
        assert!(
            handle_key(
                &mut front,
                &runtime,
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)
            )
            .await
        );

        // `Ctrl+C` with nothing cancellable exits instead of cancelling nothing.
        assert!(!front.state.can_cancel());
        assert!(
            handle_key(
                &mut front,
                &runtime,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            )
            .await
        );
        assert_eq!(
            front.request_counter, 0,
            "neither quit path issues a runtime command"
        );
    }

    #[tokio::test]
    async fn persistent_ticks_fire_while_one_channel_stays_permanently_ready() {
        let (data_tx, data_rx) = mpsc::channel(4);
        // Held open and never written, so the control channel stays a pending
        // (never closed, never ready) branch of the select.
        let (_control_tx, control_rx) = mpsc::channel::<RunEvent>(4);
        let run = run_id("run-pressure");
        let flood = tokio::spawn(async move {
            while data_tx.send(text(&run, 0, "flood")).await.is_ok() {
                // Keep the channel permanently ready.
            }
        });

        let mut interval = interval();
        let observed = tokio::time::timeout(Duration::from_secs(3), async move {
            let mut data = data_rx;
            let mut control = control_rx;
            let mut ticks = 0usize;
            let mut events = 0usize;
            // The control channel stays open and empty: only data is ever ready.
            while ticks < 3 {
                match next_loop_step(&mut interval, &mut data, &mut control).await {
                    LoopStep::Tick => ticks += 1,
                    LoopStep::Event(_) => events += 1,
                    LoopStep::Closed => break,
                }
            }
            (ticks, events)
        })
        .await;
        flood.abort();

        let (ticks, events) = observed.expect("the tick deadline keeps arriving");
        assert_eq!(ticks, 3, "a ready channel never starves the tick");
        assert!(events > 0, "events are still applied between ticks");
    }

    #[tokio::test]
    async fn key_batches_stop_at_their_bound_and_skip_non_press_kinds() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let mut front = Frontend::new(session());

        // A zero bound never polls the key source at all.
        let mut reads = 0;
        let batch = handle_key_batch(
            &mut front,
            &runtime,
            || {
                reads += 1;
                Ok(Some(press(KeyCode::F(1))))
            },
            0,
        )
        .await
        .expect("key source never fails");
        assert_eq!(batch.handled, 0);
        assert!(!batch.quit);
        assert_eq!(reads, 0, "a zero bound reads nothing");

        // A burst longer than the bound stops reading and leaves the rest for
        // a later tick.
        let mut index = 0;
        let batch = handle_key_batch(
            &mut front,
            &runtime,
            || {
                index += 1;
                Ok(Some(press(KeyCode::F(1))))
            },
            4,
        )
        .await
        .expect("key source never fails");
        assert_eq!(batch.handled, 4);
        assert!(!batch.quit);
        assert_eq!(index, 4, "the unread remainder stays queued");

        // Release and autorepeat consume a slot but produce no action.
        let kinds = [
            KeyEvent::new_with_kind(KeyCode::F(2), KeyModifiers::empty(), KeyEventKind::Release),
            KeyEvent::new_with_kind(KeyCode::F(2), KeyModifiers::empty(), KeyEventKind::Repeat),
            press(KeyCode::F(5)),
        ];
        let mut index = 0;
        let batch = handle_key_batch(
            &mut front,
            &runtime,
            || {
                let key = kinds.get(index).copied();
                index += 1;
                Ok(key)
            },
            3,
        )
        .await
        .expect("key source never fails");
        assert_eq!(index, 3, "the batch still stops at its bound");
        assert_eq!(batch.handled, 1, "only presses are handled");
        assert!(!batch.quit);

        // A quit ends the batch at once: the rest of the burst is unread.
        let burst = [
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            press(KeyCode::F(1)),
        ];
        let mut index = 0;
        let batch = handle_key_batch(
            &mut front,
            &runtime,
            || {
                let key = burst.get(index).copied();
                index += 1;
                Ok(key)
            },
            8,
        )
        .await
        .expect("key source never fails");
        assert!(batch.quit, "the loop exits on the quit key");
        assert_eq!(batch.handled, 1);
        assert_eq!(index, 1, "no key after the quit is handled");

        // A failing key source surfaces as an error, never a silent empty batch.
        let failed = handle_key_batch(
            &mut front,
            &runtime,
            || Err(io::Error::other("key source failed")),
            4,
        )
        .await;
        let error = match failed {
            Ok(_) => panic!("a failing key source is never an empty batch"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), "key source failed");
    }

    #[tokio::test]
    async fn quit_key_exits_the_batch_then_reconciles_the_live_run() {
        let (runtime, mut streams) = build_runtime().expect("demo wiring is valid");
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
        assert!(front.merger.active_run().is_some());

        // The user quits before any event has been merged.
        let mut read = false;
        let batch = handle_key_batch(
            &mut front,
            &runtime,
            || {
                if read {
                    return Ok(None);
                }
                read = true;
                Ok(Some(KeyEvent::new(
                    KeyCode::Char('d'),
                    KeyModifiers::CONTROL,
                )))
            },
            MAX_KEYS_PER_TICK,
        )
        .await
        .expect("key source never fails");
        assert!(batch.quit);
        assert_eq!(batch.handled, 1);
        assert!(
            !front.merger.is_finalized(),
            "quitting early leaves the run live"
        );

        let unresolved = reconcile_after_stop(&runtime, &mut streams, &mut front, "quit").await;
        assert!(!unresolved, "the bounded window reconciles the run");
        assert!(front.merger.is_finalized());
        assert!(front.state.is_finished());
        assert!(
            front
                .state
                .transcript()
                .iter()
                .any(|line| line.starts_with("quit: cancel reply")),
            "the reconcile records the cancel reply on the transcript"
        );
    }

    #[tokio::test]
    async fn reconcile_is_skipped_for_a_finalized_or_absent_run() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");

        // A finalized run is already reconciled; the stop path is a no-op.
        let done_run = run_id("run-done");
        let mut done = Frontend::new(session());
        done.merger.adopt(&done_run);
        assert_eq!(done.merger.push(started(&done_run, 0)).len(), 1);
        assert_eq!(done.merger.push(finished(&done_run, 1)).len(), 1);
        assert!(done.merger.is_finalized());
        let before = done.state.transcript().len();
        let mut streams = closed_streams();
        assert!(
            !reconcile_after_stop(&runtime, &mut streams, &mut done, "quit").await,
            "an already finalized run needs no cancellation"
        );
        assert_eq!(
            done.state.transcript().len(),
            before,
            "no cancel reply is recorded for a finalized run"
        );
        assert!(done.merger.is_finalized());

        // No run was ever adopted and none is live in the state: nothing to
        // cancel, so the stop path reports no outstanding work.
        let mut idle = Frontend::new(session());
        assert!(idle.merger.active_run().is_none());
        assert!(idle.state.active_run().is_none());
        assert!(!idle.state.can_cancel());
        assert!(!reconcile_after_stop(&runtime, &mut streams, &mut idle, "quit").await);
        assert_eq!(
            idle.state.transcript().len(),
            0,
            "an idle session records nothing"
        );
    }

    #[tokio::test]
    async fn reconcile_reports_unresolved_when_no_terminal_outcome_arrives() {
        let (runtime, mut streams) = build_runtime().expect("demo wiring is valid");

        // Closed channels stop the wait at once; the run is still live, so
        // unresolved work is reported instead of claimed clean.
        let (_run, mut front) = adopted_orphan();
        let mut closed = closed_streams();
        let started = Instant::now();
        assert!(
            reconcile_after_stop(&runtime, &mut closed, &mut front, "quit").await,
            "a run with no terminal outcome is reported, not hidden"
        );
        assert!(!front.merger.is_finalized());
        assert!(started.elapsed() < RECONCILE_TIMEOUT, "no window is wasted");
        assert!(
            front
                .state
                .transcript()
                .iter()
                .any(|line| line.starts_with("quit: cancel reply")),
            "the attempted cancellation is still recorded"
        );

        // Open but silent channels wait out the bound, then report unresolved.
        let (_run, mut stalled) = adopted_orphan();
        let started = Instant::now();
        assert!(
            reconcile_after_stop(&runtime, &mut streams, &mut stalled, "quit").await,
            "the reconcile never claims a rollback it did not observe"
        );
        let waited = started.elapsed();
        assert!(
            waited >= RECONCILE_TIMEOUT,
            "the cancellation window is bounded from below, got {waited:?}"
        );
        assert!(
            waited < RECONCILE_TIMEOUT * 4,
            "the window is a bound, not a hang, got {waited:?}"
        );
        assert!(!stalled.merger.is_finalized());
        assert!(!stalled.state.is_finished(), "no outcome is fabricated");
    }
}

/// Coverage top-up for the paths the two modules above leave unexercised:
/// the non-terminal fallback itself, the full `handle_key` action table, the
/// approval decision and cancellation branches, the presentation-state
/// rejection counter, the closed-control-channel select arm, the control-side
/// drain, the reconcile loop's data arm, and the redraw path behind the loop.
///
/// Determinism: the scripted `nexus-fakes` runtime and fixed id literals
/// only; no clock, sleep, thread spawn, randomness, or terminal-size query.
/// The `headless` transcript is captured by the test harness and the redraw
/// tests use a fixed viewport, so no real terminal is required.
#[cfg(test)]
mod cov_main_topup {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use nexus_core::{
        ApprovalId, AssistantText, CallId, PersistenceState, RunFinished, RunOutcome, TurnId,
    };
    use nexus_tui::state::ApprovalGeometry;
    use ratatui::layout::Rect;
    use ratatui::{TerminalOptions, Viewport};

    fn session() -> SessionId {
        SessionId::new("sess-topup").expect("valid")
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
                request: request("req-topup"),
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

    /// Exact runtime approval request published on the control channel.
    fn approval(run: &RunId, seq: u64, call: &str) -> RunEvent {
        let notice = ApprovalNotice::new(
            ApprovalId::new("a1-0").expect("valid approval id"),
            CallId::new(call).expect("valid call id"),
            "run tool host_write",
            "project scope",
            Duration::from_secs(120),
        )
        .expect("notice builds");
        RunEvent::new(
            session(),
            run.clone(),
            seq,
            EventPayload::ApprovalRequired(notice),
        )
    }

    fn interval() -> tokio::time::Interval {
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + TICK, TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn ctrl(char: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(char), KeyModifiers::CONTROL)
    }

    /// Streams whose senders are already gone: every read reports closed.
    fn closed_streams() -> EventStreams {
        let (data_tx, data_rx) = mpsc::channel::<RunEvent>(1);
        let (control_tx, control_rx) = mpsc::channel::<RunEvent>(1);
        drop(data_tx);
        drop(control_tx);
        EventStreams {
            data: data_rx,
            control: control_rx,
        }
    }

    /// A frontend holding a run the runtime never issued, so a cancel reply
    /// can never produce a terminal outcome for it.
    fn adopted_orphan() -> (RunId, Frontend) {
        let run = run_id("run-orphan-topup");
        let mut front = Frontend::new(session());
        front.merger.adopt(&run);
        (run, front)
    }

    /// One fixed-size terminal over the real stdout backend. `Viewport::Fixed`
    /// skips the backend size query, so no real terminal is needed and the
    /// frame size is the same on every run.
    fn fixed_terminal() -> Terminal<CrosstermBackend<io::Stdout>> {
        Terminal::with_options(
            CrosstermBackend::new(io::stdout()),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
            },
        )
        .expect("a fixed viewport needs no terminal size")
    }

    fn user_line_count(front: &Frontend) -> usize {
        front
            .state
            .transcript()
            .iter()
            .filter(|line| line.starts_with("[User]"))
            .count()
    }

    fn transcript(front: &Frontend) -> String {
        front.state.transcript().join("\n")
    }

    /// An empty startup configuration with no write-back path, for tests that
    /// exercise the loop rather than persistence.
    fn defaults_config() -> SessionConfig {
        SessionConfig {
            config: UserConfig::default_config(),
            path: None,
            active_model: None,
        }
    }

    /// A configuration with `count` models named `m0`.. in admission order and
    /// an optional pre-set `recent`/`favourites` list, plus the path to write
    /// back to. The provider profile is minimal but valid so model entries
    /// resolve.
    fn configured(count: usize, recent: &[&str], favourites: &[&str]) -> SessionConfig {
        let mut config = UserConfig::default_config();
        let profile = nexus_config::ProviderProfile::new(
            "demo-provider",
            "Demo provider",
            nexus_config::AdapterKind::Direct,
            Some("https://example.invalid".to_owned()),
            nexus_config::CredentialRef::env_var("NEXUS_TUI_TEST_KEY")
                .expect("credential reference is valid"),
            "demo-model",
        )
        .expect("test provider profile is valid");
        config.add_provider(profile).expect("provider admits");
        for index in 0..count {
            let entry =
                nexus_config::ModelEntry::new(format!("m{index}"), "demo-provider", "demo-model")
                    .expect("test model entry is valid");
            config.add_model(entry).expect("model admits");
        }
        for id in favourites {
            config
                .add_favourite(id)
                .expect("favourite is a known model");
        }
        for id in recent {
            config.record_use(id).expect("recent is a known model");
        }
        let active_model = boot_model(&config);
        SessionConfig {
            config,
            path: None,
            active_model,
        }
    }

    /// A frontend over `config`, with its state owned by an adopted run so a
    /// terminal outcome can be applied.
    fn frontend_with(config: SessionConfig) -> (RunId, Frontend) {
        let run = run_id("run-config");
        let mut front = Frontend::with_config(session(), config);
        front.merger.adopt(&run);
        (run, front)
    }

    #[test]
    fn boot_model_prefers_recent_then_favourites_then_admission_order() {
        assert_eq!(boot_model(&UserConfig::default_config()), None);
        assert_eq!(
            boot_model(&configured(3, &[], &[]).config).as_deref(),
            Some("m0"),
            "with nothing ranked, the first configured model boots"
        );
        assert_eq!(
            boot_model(&configured(3, &[], &["m2"]).config).as_deref(),
            Some("m2"),
            "a favourite beats admission order"
        );
        assert_eq!(
            boot_model(&configured(3, &["m1"], &["m2"]).config).as_deref(),
            Some("m1"),
            "recent beats favourites"
        );
    }

    #[test]
    fn project_dir_prefers_canonical_location_and_survives_loss() {
        let canonical = std::fs::canonicalize(".").expect("cwd canonicalizes");
        assert_eq!(
            project_dir_from(Ok(canonical.clone())),
            canonical.to_str().map(str::to_owned)
        );
        assert_eq!(
            project_dir_from(Ok(std::path::PathBuf::from("/nonexistent-nexus-dir-9f3a"))),
            Some("/nonexistent-nexus-dir-9f3a".to_owned()),
            "an unreadable directory still displays instead of hiding the scope"
        );
        assert_eq!(
            project_dir_from(Err(io::Error::new(io::ErrorKind::NotFound, "gone"))),
            None
        );
    }

    #[test]
    fn startup_frontend_records_the_project_directory() {
        let (_run, front) = frontend_with(configured(0, &[], &[]));
        assert!(
            front.state.project_dir().is_some(),
            "the header names the jail scope from the first frame"
        );
    }

    #[test]
    fn viewport_m_cycles_admission_order_and_reports_the_selection() {
        let (run, mut front) = frontend_with(configured(3, &[], &[]));
        front.focus = Focus::Viewport;
        assert_eq!(front.active_model.as_deref(), Some("m0"));

        front.cycle_model();
        assert_eq!(front.active_model.as_deref(), Some("m1"));
        assert!(transcript(&front).contains("model: m1"));

        front.cycle_model();
        front.cycle_model();
        assert_eq!(
            front.active_model.as_deref(),
            Some("m0"),
            "cycling wraps at the end of the admission order"
        );
        assert!(
            !front.state.is_finished(),
            "cycling is presentation only: {run} is untouched"
        );
    }

    #[test]
    fn model_display_follows_selection_and_clears_on_dangle() {
        let (_run, mut front) = frontend_with(configured(2, &[], &[]));
        assert_eq!(front.state.active_model(), Some("m0"));
        assert_eq!(front.state.active_provider(), Some("demo-provider"));
        front.focus = Focus::Viewport;
        front.cycle_model();
        assert_eq!(front.state.active_model(), Some("m1"));
        assert_eq!(front.state.active_provider(), Some("demo-provider"));
        // A selection that no longer resolves clears the display instead
        // of showing a stale name.
        front.active_model = Some("ghost".to_owned());
        front.sync_model_display();
        assert_eq!(front.state.active_model(), None);
        assert_eq!(front.state.active_provider(), None);
    }

    #[test]
    fn cycling_without_configured_models_notices_and_changes_nothing() {
        let mut front = Frontend::with_config(session(), defaults_config());
        front.focus = Focus::Viewport;
        assert!(front.active_model.is_none());

        front.cycle_model();
        assert!(front.active_model.is_none(), "no selection is invented");
        assert!(transcript(&front).contains("no configured models"));
    }

    #[test]
    fn terminal_outcome_records_the_active_model_and_saves_the_document() {
        let directory = temp_config_dir("terminal-save");
        let path = directory.join("config.json");
        let mut config = configured(3, &[], &[]);
        config.path = Some(path.clone());
        let (run, mut front) = frontend_with(config);
        front.active_model = Some("m2".to_owned());

        assert!(front.apply_events(vec![started(&run, 0), finished(&run, 1)]));
        assert_eq!(
            front.config.recent().first().map(String::as_str),
            Some("m2"),
            "the completed run's model leads the recents"
        );
        let persisted = load_config(&path)
            .expect("the document is readable")
            .expect("it exists");
        assert_eq!(
            persisted.recent().first().map(String::as_str),
            Some("m2"),
            "the usage was written back to the file the session loaded"
        );
        std::fs::remove_dir_all(&directory).expect("temporary directory removed");
    }

    #[test]
    fn an_unwritable_config_path_is_a_notice_and_never_a_crash() {
        let mut config = configured(2, &[], &[]);
        // A path whose parent does not exist: `save` cannot create it.
        config.path = Some(
            std::env::temp_dir()
                .join("nexus-tui-missing-parent")
                .join("config.json"),
        );
        let (run, mut front) = frontend_with(config);
        front.active_model = Some("m1".to_owned());

        assert!(
            front.apply_events(vec![started(&run, 0), finished(&run, 1)]),
            "the terminal outcome is applied"
        );
        let lines = transcript(&front);
        assert!(
            lines.contains("config save failed"),
            "a failed write is reported, not fatal"
        );
        assert!(
            lines.contains("finished: Completed"),
            "the run's outcome is still presented: {lines}"
        );
    }

    #[test]
    fn a_missing_config_file_starts_on_defaults_without_an_error() {
        let directory = temp_config_dir("missing-file");
        let loaded =
            session_config_at(Some(directory.join("config.json"))).expect("a missing file is fine");
        assert_eq!(loaded.active_model, None);
        assert!(
            loaded.config.models().is_empty(),
            "defaults carry no models"
        );
        std::fs::remove_dir_all(&directory).expect("temporary directory removed");
    }

    #[test]
    fn an_invalid_config_file_is_an_explicit_startup_error() {
        let directory = temp_config_dir("invalid-file");
        let path = directory.join("config.json");
        std::fs::write(&path, "{ not json").expect("temporary document written");
        let error = session_config_at(Some(path.clone()))
            .expect_err("a present but broken document must not start silently");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            !error.to_string().contains("not json"),
            "the diagnostic does not echo the document: {error}"
        );
        std::fs::remove_dir_all(&directory).expect("temporary directory removed");
    }

    /// A unique empty directory under the platform temp dir, removed by the
    /// caller. Named from `purpose` so parallel tests never share one.
    fn temp_config_dir(purpose: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!("nexus-tui-cfg-{purpose}"));
        std::fs::remove_dir_all(&directory).ok();
        std::fs::create_dir_all(&directory).expect("temporary directory created");
        directory
    }

    /// A frontend whose state owns `run` and is awaiting a decision on `call`.
    fn awaiting_approval(run: &RunId, call: &str) -> Frontend {
        let mut front = Frontend::new(session());
        front.merger.adopt(run);
        assert!(!front.apply_events(vec![started(run, 0), approval(run, 1, call)]));
        front
    }

    #[tokio::test]
    async fn headless_drains_the_scripted_run_and_denies_its_approval() {
        let (runtime, streams) = build_runtime().expect("demo wiring is valid");
        headless(runtime, streams, defaults_config())
            .await
            .expect("the non-terminal fallback always completes");
    }

    #[tokio::test]
    async fn composer_submit_adopts_the_run_and_clears_the_draft() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let mut front = Frontend::new(session());
        front.focus = Focus::Composer;
        front.state.composer_type('h');
        front.state.composer_type('i');

        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Enter)).await);
        assert_eq!(front.request_counter, 1, "the submit has its own identity");
        assert_eq!(front.state.composer(), "", "the accepted draft is cleared");
        assert!(
            front.merger.active_run().is_some(),
            "only an accepted reply adopts a run"
        );
        assert_eq!(user_line_count(&front), 1, "the draft is recorded once");
    }

    #[tokio::test]
    async fn composer_keys_edit_the_draft_and_cycle_focus() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let mut front = Frontend::new(session());
        front.focus = Focus::Composer;

        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('a'))).await);
        assert!(
            !handle_key(&mut front, &runtime, ctrl('j')).await,
            "Ctrl+J is an explicit newline, never typed text"
        );
        assert_eq!(front.state.composer(), "a\n");

        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Backspace)).await);
        assert_eq!(front.state.composer(), "a");

        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Tab)).await);
        assert_eq!(front.focus, Focus::Viewport, "Tab leaves the composer");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Tab)).await);
        assert_eq!(
            front.focus,
            Focus::Composer,
            "no approval is pending, so the card is not reachable"
        );

        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Esc)).await);
        assert_eq!(front.focus, Focus::Viewport, "Esc parks in the viewport");
        assert!(!front.state.approval_detail_open(), "Esc never decides");
        assert_eq!(front.request_counter, 0, "no focus key issues a command");
    }

    #[tokio::test]
    async fn approval_keys_scroll_the_detail_and_never_decide() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let run = run_id("run-approval-keys");
        let mut front = awaiting_approval(&run, "c1-0");
        front.focus = Focus::ApprovalCard;

        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('i'))).await);
        assert!(front.state.approval_detail_open(), "i opens the detail");
        for key in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::PageUp,
            KeyCode::PageDown,
        ] {
            assert!(
                !handle_key(&mut front, &runtime, press(key)).await,
                "scrolling the expanded detail never exits the loop"
            );
        }
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('i'))).await);
        assert!(!front.state.approval_detail_open(), "i closes the detail");

        front.focus = Focus::Viewport;
        for key in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::PageUp,
            KeyCode::PageDown,
        ] {
            assert!(!handle_key(&mut front, &runtime, press(key)).await);
        }
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Left)).await);
        assert_eq!(front.request_counter, 0, "navigation issues no command");
        assert!(
            front.live_approval.is_some(),
            "inspection leaves the approval undecided"
        );
        assert!(
            front.state.pending_approval().is_some(),
            "the card is still awaiting a decision"
        );
    }

    #[tokio::test]
    async fn folding_without_a_selection_selects_an_entry_first() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let run = run_id("run-fold");
        let mut front = Frontend::new(session());
        front.merger.adopt(&run);
        front.apply_events(vec![started(&run, 0), text(&run, 1, "hello")]);
        front.focus = Focus::Viewport;
        assert!(front.state.selected().is_none());

        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Left)).await);
        let selected = front.state.selected().expect("an entry is selected");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Left)).await);
        assert_eq!(front.state.selected(), Some(selected), "the fold toggles");
        assert!(front.state.entry_count() > 0);
    }

    #[tokio::test]
    async fn allow_once_stays_locked_until_the_detail_is_measured() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let run = run_id("run-locked");
        let mut front = awaiting_approval(&run, "c1-0");
        front.focus = Focus::ApprovalCard;
        assert!(
            !front.state.approval_decision_allowed(),
            "no frame has measured the card yet"
        );

        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('a'))).await);
        assert!(
            transcript(&front).contains("allow-once locked"),
            "the locked path names the inspection key"
        );
        assert_eq!(front.request_counter, 0, "a locked key issues no command");
        assert!(front.live_approval.is_some(), "the approval still awaits");

        front.state.set_approval_geometry(ApprovalGeometry {
            inner_width: 40,
            inner_rows: 6,
            detail_rows: 6,
            clipped: false,
        });
        assert!(front.state.approval_decision_allowed());
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('a'))).await);
        assert_eq!(front.request_counter, 1);
        assert!(
            transcript(&front).contains("decision reply"),
            "the runtime reply is surfaced"
        );
        assert!(front.live_approval.is_none(), "the decision is spent");
        assert!(front.state.pending_approval().is_none());
        assert_eq!(
            front.focus,
            Focus::Composer,
            "the spent card hands the keyboard back to the composer"
        );
    }

    #[tokio::test]
    async fn deny_needs_no_measurement_and_allow_once_without_a_notice_is_refused() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let run = run_id("run-deny");

        let mut undecided = Frontend::new(session());
        undecided.focus = Focus::ApprovalCard;
        assert!(!handle_key(&mut undecided, &runtime, press(KeyCode::Char('a'))).await);
        assert!(
            transcript(&undecided).contains("no live approval to decide"),
            "an absent live approval is refused, never invented"
        );
        assert_eq!(undecided.request_counter, 0);

        let mut front = awaiting_approval(&run, "c1-0");
        front.focus = Focus::ApprovalCard;
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('d'))).await);
        assert_eq!(front.request_counter, 1);
        assert!(transcript(&front).contains("decision reply"));
        assert!(front.live_approval.is_none());
        assert!(front.state.pending_approval().is_none());
        assert_eq!(
            front.focus,
            Focus::Composer,
            "the spent card hands the keyboard back to the composer"
        );
    }

    #[test]
    fn approval_arrival_takes_focus_without_tab() {
        let run = run_id("run-autofocus");
        let mut front = Frontend::new(session());
        front.merger.adopt(&run);
        assert_eq!(front.focus, Focus::Composer);
        assert!(!front.apply_events(vec![started(&run, 0)]));
        assert_eq!(
            front.focus,
            Focus::Composer,
            "plain output never steals focus"
        );
        assert!(!front.apply_events(vec![approval(&run, 1, "c1-0")]));
        assert_eq!(
            front.focus,
            Focus::ApprovalCard,
            "a newly arrived approval takes focus so its keys work at once"
        );
        // The user deliberately moves away; later traffic must not yank
        // focus back to a card they already know about.
        front.focus = Focus::Viewport;
        assert!(!front.apply_events(vec![text(&run, 2, "later")]));
        assert_eq!(front.focus, Focus::Viewport);
        // A terminal outcome returns focus for the next task.
        assert!(front.apply_events(vec![finished(&run, 3)]));
        assert_eq!(front.focus, Focus::Composer);
    }

    #[tokio::test]
    async fn ctrl_c_cancels_the_live_run_through_the_runtime() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let run = run_id("run-cancel-topup");
        let mut front = Frontend::new(session());
        front.merger.adopt(&run);
        front.apply_events(vec![started(&run, 0)]);
        assert!(front.state.can_cancel(), "output is streaming");

        assert!(!handle_key(&mut front, &runtime, ctrl('c')).await);
        assert_eq!(front.request_counter, 1);
        assert!(
            transcript(&front).contains("cancel reply"),
            "the cancel reply is surfaced"
        );
    }

    #[test]
    fn a_state_rejection_is_counted_and_surfaced_once() {
        let mut front = Frontend::new(session());
        let stale = run_id("run-unowned");
        front.merger.adopt(&stale);
        // The merger admits the event for the adopted run, but presentation
        // state owns no run, so it rejects the very same event.
        assert!(!front.apply_events(vec![text(&stale, 0, "unowned")]));
        assert_eq!(front.state_rejected, 1);
        assert_eq!(front.state.last_seq(), None, "no state was mutated");

        front.report_merger();
        let first = transcript(&front);
        assert!(
            first.contains("1 event(s) rejected by presentation state"),
            "the rejection is surfaced explicitly"
        );
        front.report_merger();
        assert_eq!(
            transcript(&front)
                .matches("rejected by presentation state")
                .count(),
            1,
            "the count is reported once, not on every tick"
        );
        assert!(front.state_rejected_reported);
    }

    #[tokio::test]
    async fn a_closed_control_channel_reports_closed_while_data_stays_open() {
        let (data_tx, mut data_rx) = mpsc::channel::<RunEvent>(1);
        let (control_tx, mut control_rx) = mpsc::channel::<RunEvent>(1);
        drop(control_tx);
        let mut interval = interval();

        let step = next_loop_step(&mut interval, &mut data_rx, &mut control_rx).await;
        drop(data_tx);
        assert!(
            matches!(step, LoopStep::Closed),
            "a closed control channel ends the wait even with data open"
        );
    }

    #[tokio::test]
    async fn absorb_drains_the_control_channel_behind_a_buffered_outcome() {
        let run = run_id("run-drain-control");
        let (data_tx, data_rx) = mpsc::channel(8);
        let (control_tx, control_rx) = mpsc::channel(8);
        let mut streams = EventStreams {
            data: data_rx,
            control: control_rx,
        };
        let mut front = Frontend::new(session());
        front.merger.adopt(&run);
        data_tx.try_send(started(&run, 0)).expect("capacity");
        control_tx
            .try_send(text(&run, 1, "alpha"))
            .expect("capacity");

        // The outcome is observed while both channels still hold earlier
        // traffic, so the control side must be drained too.
        assert!(absorb(&mut front, finished(&run, 2), &mut streams));
        assert!(front.merger.is_finalized());
        assert!(!front.merger.has_pending());
        assert_eq!(front.merger.gaps(), 0, "every sequence arrived");
        assert_eq!(front.state.last_seq(), Some(2));

        let transcript = transcript(&front);
        let at = |needle: &str| {
            transcript
                .find(needle)
                .unwrap_or_else(|| panic!("{needle} is presented"))
        };
        assert!(at("alpha") < at("finished: Completed"));
    }

    #[tokio::test]
    async fn reconcile_absorbs_a_queued_event_then_stops_on_the_closed_channel() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let (run, mut front) = adopted_orphan();
        let (data_tx, data_rx) = mpsc::channel(4);
        let (control_tx, control_rx) = mpsc::channel(4);
        let mut streams = EventStreams {
            data: data_rx,
            control: control_rx,
        };
        data_tx.try_send(text(&run, 0, "queued")).expect("capacity");
        drop(data_tx);
        let _control_still_open = control_tx;

        assert!(
            reconcile_after_stop(&runtime, &mut streams, &mut front, "quit").await,
            "no terminal outcome arrives, so unresolved work is reported"
        );
        assert!(!front.merger.is_finalized());
        let transcript = transcript(&front);
        assert!(
            transcript.contains("quit: cancel reply"),
            "the attempted cancellation is recorded"
        );
    }

    #[tokio::test]
    async fn interactive_loop_ends_at_once_when_both_channels_close() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let run = run_id("run-loop-closed");
        let mut front = Frontend::new(session());
        front.merger.adopt(&run);
        let mut streams = closed_streams();
        let mut gate = RefreshGate::m0_test();
        let mut interval = interval();
        let mut terminal = fixed_terminal();

        interactive_loop(
            &runtime,
            &mut streams,
            &mut front,
            &mut gate,
            &mut terminal,
            &mut interval,
        )
        .await
        .expect("a closed channel pair ends the loop cleanly");
        assert_eq!(front.merger.active_run(), Some(&run));
        assert_eq!(front.state.last_seq(), None, "no event was applied");
    }

    #[tokio::test]
    async fn interactive_loop_applies_terminal_and_ends_only_on_close() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let run = run_id("run-loop-event");
        let mut front = Frontend::new(session());
        front.merger.adopt(&run);
        let (data_tx, data_rx) = mpsc::channel::<RunEvent>(4);
        let (control_tx, control_rx) = mpsc::channel::<RunEvent>(4);
        let mut streams = EventStreams {
            data: data_rx,
            control: control_rx,
        };
        data_tx.try_send(started(&run, 0)).expect("capacity");
        data_tx.try_send(finished(&run, 1)).expect("capacity");
        // Both senders are dropped up front: whatever the channel race
        // yields (event first or close first), the close path drains the
        // queued events before exiting, so both are always applied.
        drop(data_tx);
        drop(control_tx);
        let mut gate = RefreshGate::m0_test();
        let mut interval = interval();
        let mut terminal = fixed_terminal();

        // The terminal outcome is applied and presented, but the loop no
        // longer ends on it: only the channel close ends the loop.
        interactive_loop(
            &runtime,
            &mut streams,
            &mut front,
            &mut gate,
            &mut terminal,
            &mut interval,
        )
        .await
        .expect("the loop ends on close after applying the terminal outcome");
        assert_eq!(front.state.last_seq(), Some(1), "both events were applied");
        assert!(front.state.is_finished(), "the outcome was presented");
        assert!(
            transcript(&front).contains(&format!("run {} started", run.as_str())),
            "the applied event is presented"
        );
    }

    #[test]
    fn second_accepted_submit_adopts_a_new_run_after_terminal() {
        let first = run_id("run-first");
        let second = run_id("run-second");
        let mut front = Frontend::new(session());
        let accept = |front: &mut Frontend, run: &RunId| {
            let reply = CommandResponse::new(
                front.next_request(),
                CommandReply::Accepted,
                Some(run.clone()),
            );
            assert!(front.adopt_accepted(&reply), "accepted submits adopt");
        };
        // Drive events through the merger exactly like the loop does, so
        // finalization, sequencing, and focus behave identically.
        let apply = |front: &mut Frontend, events: Vec<RunEvent>| {
            for event in events {
                let pushed = front.merger.push(event);
                front.apply_events(pushed);
            }
        };
        accept(&mut front, &first);
        apply(&mut front, vec![started(&first, 0), finished(&first, 1)]);
        assert!(front.merger.is_finalized(), "the first run finalized");
        assert_eq!(
            front.focus,
            Focus::Composer,
            "focus returns for the next task"
        );
        accept(&mut front, &second);
        assert!(
            !front.merger.is_finalized(),
            "the second accepted run reopens the merger"
        );
        apply(&mut front, vec![started(&second, 0), finished(&second, 1)]);
        assert_eq!(front.state.last_seq(), Some(1), "per-run sequences restart");
        assert!(
            front.state.is_finished(),
            "the second outcome was presented"
        );
    }

    /// Applies queued events until the run finalizes or an approval awaits.
    async fn drain_until_settled(
        front: &mut Frontend,
        streams: &mut EventStreams,
        interval: &mut tokio::time::Interval,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !front.merger.is_finalized()
            && front.live_approval.is_none()
            && Instant::now() < deadline
        {
            match next_loop_step(interval, &mut streams.data, &mut streams.control).await {
                LoopStep::Closed => break,
                LoopStep::Event(event) => {
                    let pushed = front.merger.push(event);
                    front.apply_events(pushed);
                }
                LoopStep::Tick => {}
            }
        }
    }

    #[tokio::test]
    async fn two_demo_runs_each_replay_the_script_and_complete() {
        let (runtime, mut streams) = build_runtime().expect("demo wiring is valid");
        let mut front = Frontend::new(session());
        let mut interval = interval();
        let mut first_run = None;
        for task in [DEMO_INPUT, "second demo task"] {
            let submit = submit_command(
                front.next_request(),
                front.session.clone(),
                task,
                DEMO_PROFILE,
            )
            .expect("valid submit builds");
            let reply = runtime.handle(submit).await.0;
            assert_eq!(
                reply.reply(),
                CommandReply::Accepted,
                "each demo task is accepted, even after a previous run"
            );
            assert!(settle_submit(&mut front, task, &reply));
            drain_until_settled(&mut front, &mut streams, &mut interval).await;
            let (run, notice) = front
                .live_approval
                .clone()
                .expect("the scripted mutation awaits a grant");
            if let Some(first) = &first_run {
                assert_ne!(&run, first, "the second task mints a fresh run identity");
            }
            first_run = Some(run.clone());
            let command = approve_notice_command(front.next_request(), &run, &notice);
            let decision = runtime.handle(command).await.0;
            assert_eq!(decision.reply(), CommandReply::Accepted);
            front.state.resolve_approval();
            front.live_approval = None;
            drain_until_settled(&mut front, &mut streams, &mut interval).await;
            assert!(front.merger.is_finalized(), "the run finalized");
            assert!(
                transcript(&front).contains("finished: Completed"),
                "the granted demo run completes instead of exhausting its script"
            );
        }
    }

    #[test]
    fn draw_renders_a_frame_without_querying_a_terminal_size() {
        let run = run_id("run-draw");
        let mut front = Frontend::new(session());
        front.merger.adopt(&run);
        front.apply_events(vec![
            started(&run, 0),
            text(&run, 1, "hello"),
            finished(&run, 2),
        ]);
        front.focus = Focus::Viewport;
        let mut terminal = fixed_terminal();

        draw(&mut terminal, &mut front).expect("the frame renders");
        assert!(front.state.is_finished());
    }
}
