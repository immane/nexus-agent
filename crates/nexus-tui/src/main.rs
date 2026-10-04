//! `nexus-tui` M0-test demo: scripted fakes behind the real runtime port.
//!
//! TEST-ONLY. This binary wires [`nexus_fakes`] scripted doubles through
//! the same runtime command/event port the product TUI will use. It is never
//! real configuration: no provider credentials, no plugins, no network, no
//! stored sessions. A stderr banner says so on every launch.

#![forbid(unsafe_code)]

use std::io::{self, IsTerminal};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyEvent, KeyEventKind, poll, read};
use nexus_core::{Limits, RequestId, SessionId};
use nexus_fakes::{FakeProvider, FakeTool};
use nexus_runtime::{EventStreams, Policy, Runtime, RuntimeConfig};
use nexus_tui::{
    Action, AppState, Focus, RefreshGate, approve_command, cancel_command, deny_command,
    install_panic_hook, map_key, next_focus, render, submit_command,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

const DEMO_PROFILE: &str = "m0-test";
const DEMO_INPUT: &str = "m0 test-only demo submission";
const TICK: Duration = Duration::from_millis(50);
const HEADLESS_TIMEOUT: Duration = Duration::from_secs(60);
const FINAL_DRAIN: Duration = Duration::from_millis(200);
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

fn next_request(counter: &mut u64) -> RequestId {
    *counter += 1;
    RequestId::new(format!("req-tui-{counter}")).expect("counter request id is valid")
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    install_panic_hook();
    eprintln!(
        "nexus-tui M0 TEST-ONLY demo: scripted fakes, ephemeral store, no network, \
         no credentials, no real provider. Not real configuration."
    );
    if let Err(error) = run().await {
        eprintln!("nexus-tui demo failed: {error}");
        std::process::exit(1);
    }
}

async fn run() -> io::Result<()> {
    let (runtime, streams) = build_runtime();
    if io::stdout().is_terminal() {
        match nexus_tui::TerminalGuard::setup() {
            Ok(mut guard) => {
                let result = interactive(runtime, streams).await;
                guard.teardown();
                result
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

/// Interactive scripted demo: one canned submission, live approval card,
/// full test-only keyset. Exits on quit or the terminal run outcome.
async fn interactive(runtime: Runtime, mut streams: EventStreams) -> io::Result<()> {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).map_err(io::Error::other)?;
    let mut state = AppState::new();
    let mut gate = RefreshGate::m0_test();
    let mut focus = Focus::Composer;
    let mut request_counter = 0;
    let session =
        SessionId::new(DEMO_SESSION).map_err(|_| io::Error::other("demo session id rejected"))?;

    // Canned M0 submission through the same command path as typed input.
    let submit = submit_command(
        next_request(&mut request_counter),
        session.clone(),
        DEMO_INPUT,
        DEMO_PROFILE,
    )
    .map_err(io::Error::other)?;
    let reply = runtime.handle(submit).await.0;
    state.notice(&format!("submit reply: {:?}", reply.reply()));
    state.record_submitted(DEMO_INPUT);
    gate.request();

    loop {
        tokio::select! {
            event = streams.data.recv() => {
                match event {
                    Some(event) => { state.apply_event(&event); gate.request(); }
                    None => break,
                }
            }
            event = streams.control.recv() => {
                match event {
                    Some(event) => {
                        let ended = event.is_terminal();
                        state.apply_event(&event);
                        gate.request();
                        if ended {
                            final_drain(&runtime, &mut streams, &mut state).await;
                            draw(&mut terminal, &mut state, focus)?;
                            return Ok(());
                        }
                    }
                    None => break,
                }
            }
            () = tokio::time::sleep(TICK) => {
                while poll(Duration::ZERO).map_err(io::Error::other)? {
                    let Event::Key(key) = read().map_err(io::Error::other)? else { continue };
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    if handle_key(&runtime, &mut state, &mut focus, &session, &mut request_counter, key).await {
                        draw(&mut terminal, &mut state, focus)?;
                        return Ok(());
                    }
                    gate.request();
                }
            }
        }
        let now = Instant::now();
        if gate.ready(now) {
            draw(&mut terminal, &mut state, focus)?;
            gate.mark_drawn(now);
        }
    }
    Ok(())
}

fn draw(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut AppState,
    focus: Focus,
) -> io::Result<()> {
    terminal
        .draw(|frame| render(state, frame.size(), frame.buffer_mut(), focus))
        .map_err(io::Error::other)?;
    Ok(())
}

/// Grace drain after the terminal event so late data-channel text still
/// reaches presentation before the last frame.
async fn final_drain(runtime: &Runtime, streams: &mut EventStreams, state: &mut AppState) {
    let _ = runtime;
    let deadline = Instant::now() + FINAL_DRAIN;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::select! {
            event = streams.data.recv() => {
                match event {
                    Some(event) => { state.apply_event(&event); }
                    None => break,
                }
            }
            event = streams.control.recv() => {
                match event {
                    Some(event) => { state.apply_event(&event); }
                    None => break,
                }
            }
            () = tokio::time::sleep(remaining) => break,
        }
    }
}

/// Handles one key event. Returns true when the TUI should exit.
async fn handle_key(
    runtime: &Runtime,
    state: &mut AppState,
    focus: &mut Focus,
    session: &SessionId,
    counter: &mut u64,
    key: KeyEvent,
) -> bool {
    let Some(action) = map_key(*focus, key) else {
        return false;
    };
    match action {
        Action::Submit => {
            let draft = state.composer_take();
            if draft.trim().is_empty() {
                return false;
            }
            match submit_command(next_request(counter), session.clone(), &draft, DEMO_PROFILE) {
                Ok(command) => {
                    let reply = runtime.handle(command).await.0;
                    state.notice(&format!("submit reply: {:?}", reply.reply()));
                    state.record_submitted(&draft);
                }
                Err(_) => state.notice("submit rejected: input invalid"),
            }
        }
        Action::Newline => state.composer_newline(),
        Action::Type(char) => state.composer_type(char),
        Action::Backspace => {
            state.composer_backspace();
        }
        Action::FocusSwitch => *focus = next_focus(*focus, state.pending_approval().is_some()),
        Action::ParkFocus => *focus = Focus::Viewport,
        Action::ScrollUp => state.move_selection(-1),
        Action::ScrollDown => state.move_selection(1),
        Action::PageUp => state.scroll_up(state.viewport_height()),
        Action::PageDown => state.scroll_down(state.viewport_height()),
        Action::FoldToggle => {
            if state.selected().is_none() {
                state.move_selection(-1);
            }
            if let Some(index) = state.selected() {
                state.toggle_fold(index);
            }
        }
        Action::ApproveOnce | Action::Deny => {
            let Some(card) = state.pending_approval().cloned() else {
                state.notice("no live approval to decide");
                return false;
            };
            let Some(run) = state.active_run().cloned() else {
                state.notice("no live run for the approval");
                return false;
            };
            let command = if action == Action::ApproveOnce {
                approve_command(next_request(counter), &run, &card)
            } else {
                deny_command(next_request(counter), &run, &card)
            };
            let reply = runtime.handle(command).await.0;
            state.notice(&format!("decision reply: {:?}", reply.reply()));
            state.resolve_approval();
            *focus = Focus::Viewport;
        }
        Action::Cancel => {
            if !state.can_cancel() {
                return true;
            }
            match state.active_run().cloned() {
                Some(run) => {
                    let reply = runtime
                        .handle(cancel_command(next_request(counter), &run))
                        .await
                        .0;
                    state.notice(&format!("cancel reply: {:?}", reply.reply()));
                }
                None => state.notice("nothing cancellable"),
            }
        }
        Action::Quit => return true,
    }
    false
}

/// Non-terminal fallback: drains the scripted run, denies any approval
/// (no user can confirm it), and prints the sanitized presentation
/// transcript with no escape codes.
async fn headless(runtime: Runtime, mut streams: EventStreams) -> io::Result<()> {
    let mut state = AppState::new();
    let mut counter = 0;
    let session =
        SessionId::new(DEMO_SESSION).map_err(|_| io::Error::other("demo session id rejected"))?;
    let submit = submit_command(
        next_request(&mut counter),
        session,
        DEMO_INPUT,
        DEMO_PROFILE,
    )
    .map_err(io::Error::other)?;
    let reply = runtime.handle(submit).await.0;
    state.notice(&format!("submit reply: {:?}", reply.reply()));
    state.record_submitted(DEMO_INPUT);

    let outcome = tokio::time::timeout(HEADLESS_TIMEOUT, async {
        loop {
            tokio::select! {
                event = streams.data.recv() => {
                    match event {
                        Some(event) => { state.apply_event(&event); }
                        None => break,
                    }
                }
                event = streams.control.recv() => {
                    match event {
                        Some(event) => {
                            use nexus_core::EventPayload;
                            let ended = event.is_terminal();
                            // No user can confirm: deny like headless mode.
                            if let EventPayload::ApprovalRequired(notice) = event.payload() {
                                let run = event.run().clone();
                                let card = nexus_tui::PendingApprovalCard {
                                    approval: notice.approval.clone(),
                                    call: notice.call.clone(),
                                    summary: notice.summary.clone(),
                                    scope_summary: notice.scope_summary.clone(),
                                    expires_at_elapsed: notice.expires_at_elapsed,
                                };
                                let reply = runtime
                                    .handle(deny_command(next_request(&mut counter), &run, &card))
                                    .await
                                    .0;
                                state.notice(&format!("approval denied (no user): {:?}", reply.reply()));
                            }
                            state.apply_event(&event);
                            if ended {
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
        }
    })
    .await;
    if outcome.is_err() {
        state.notice("headless drain timed out");
    }
    println!("TEST-ONLY transcript (non-terminal fallback; approvals denied):");
    for line in state.transcript() {
        println!("{line}");
    }
    Ok(())
}
