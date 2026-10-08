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

use std::collections::{BTreeMap, HashMap};
use std::io::{self, IsTerminal};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind, poll, read,
};
use nexus_config::{
    AgentMode, UserConfig, load as load_config, resolve_path, resolve_with, save as save_config,
};
use nexus_core::{
    AgentError, ApprovalNotice, CommandReply, CommandResponse, ErrorCategory, EventPayload,
    GetSnapshotCommand, Limits, ModelRequest, ProviderCapabilities, ProviderContext, ProviderEvent,
    ProviderPort, RequestId, RetryGuidance, RunEvent, RunId, SessionId,
};
use nexus_fakes::{FakeProvider, FakeTool};
use nexus_openai::OpenAiProvider;
use nexus_runtime::{EventStreams, Policy, Runtime, RuntimeConfig};
use nexus_tools::{
    SandboxedExecutor, ScopedLister, ScopedPatcher, ScopedReader, ScopedSearcher, ScopedWriter,
};
use nexus_tui::decisions::{approve_notice_command, deny_notice_command};
use nexus_tui::{
    Action, AppState, CompletionItem, Focus, MAX_PICKER_VISIBLE_ROWS, ModelChoice, RefreshGate,
    SessionArgs, SlashCommand, TOAST_TTL, TextSelection, cancel_command, cell_to_char_col,
    install_panic_hook, map_key, next_focus, render, submit_command,
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
/// The M0 fixture default is intentionally small, but interactive coding
/// runs need room for inspect/edit/check/retry cycles.
const TUI_MODEL_TURNS_PER_RUN: u32 = 64;

fn tui_runtime_limits() -> Limits {
    let mut limits = Limits::m0_test();
    limits.max_model_turns_per_run = TUI_MODEL_TURNS_PER_RUN;
    limits
}

/// Fallible production composition: the runtime validates limits, provider
/// capabilities, and every tool registration before any run can start, so an
/// invalid demo wiring is an explicit startup error, never a hung run.
fn build_runtime() -> io::Result<(Runtime, EventStreams)> {
    let config = RuntimeConfig {
        limits: tui_runtime_limits(),
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
///
/// Cloned once per session slot: every conversation shares the same startup
/// document and boot model, while each keeps its own runtime and history.
#[derive(Debug, Clone)]
struct SessionConfig {
    config: UserConfig,
    path: Option<std::path::PathBuf>,
    active_model: Option<String>,
    /// Project root for real file reads/writes: `--tools-root`, else the working
    /// directory. `None` (tests only) keeps every tool scripted. Cloned
    /// with the template so every slot shares the startup wiring while
    /// keeping its own runtime and selection.
    tools_root: Option<std::path::PathBuf>,
    strict_tools: bool,
    /// Scripted demo explicitly requested with `--demo`. Without it the
    /// runtime never serves fakes, however unconfigured the selection is.
    demo: bool,
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
    let args = match parse_startup_args(&std::env::args().collect::<Vec<_>>()) {
        StartupAction::Run(args) => args,
        StartupAction::Usage => startup_usage(),
    };
    let mut session = session_config_at(resolve_path(None))?;
    session.tools_root = resolve_tools_root(args.tools_root);
    session.demo = args.demo;
    session.strict_tools = args.strict_tools;
    Ok(session)
}

/// Resolves the file-tool jail: an explicit `--tools-root` wins, otherwise
/// the process working directory (subdirectories included; `..` escapes are
/// still refused by the jail itself). Pure apart from reading the cwd, so
/// the precedence is unit-testable.
fn resolve_tools_root(cli: Option<std::path::PathBuf>) -> Option<std::path::PathBuf> {
    cli.or_else(|| std::env::current_dir().ok())
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
        tools_root: None,
        strict_tools: false,
        demo: false,
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

/// Startup arguments. The TUI takes no `--config` flag (see
/// [`SessionConfig`]); flags select the project root, strict permissions,
/// or the scripted demo.
/// Without `--demo` nothing scripted ever serves: unconfigured runs fail
/// with a not-configured diagnostic instead of a silent fake.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StartupArgs {
    strict_tools: bool,
    /// Jail for real file reads and writes. Explicit `--tools-root`, else
    /// the working directory by default; `None` (tests only) keeps every
    /// tool scripted. Development writes in this root are automatic.
    tools_root: Option<std::path::PathBuf>,
    /// Serve the scripted demo wiring (fake provider/tools, canned
    /// submission). The dev-version default is off.
    demo: bool,
}

/// Argument-parser outcome: run with the parsed jail, or print usage.
enum StartupAction {
    /// Serve the TUI with this optional real-files jail.
    Run(StartupArgs),
    /// Print usage and exit 2.
    Usage,
}

/// Parses the supported project, permission, demo, and help flags.
/// Anything else (including a bare word,
/// which the historical parser ignored) is usage, never a silent boot
/// into scripted mode on a mistyped flag.
fn parse_startup_args(argv: &[String]) -> StartupAction {
    let mut tools_root: Option<std::path::PathBuf> = None;
    let mut demo = false;
    let mut strict_tools = false;
    let mut args = argv.iter().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return StartupAction::Usage,
            "--demo" => demo = true,
            "--strict-tools" => strict_tools = true,
            "--tools-root" => match args.next() {
                Some(value) if !value.is_empty() => {
                    tools_root = Some(std::path::PathBuf::from(value));
                }
                _ => return StartupAction::Usage,
            },
            _ => return StartupAction::Usage,
        }
    }
    StartupAction::Run(StartupArgs {
        tools_root,
        demo,
        strict_tools,
    })
}

/// Prints usage and exits 2. Splitting the exit from the parser keeps
/// every rejection path unit-testable.
fn startup_usage() -> ! {
    eprintln!("usage: nexus-tui [--tools-root PATH] [--strict-tools] [--demo] [--help]");
    eprintln!("  Without --demo the TUI attempts live wiring; unconfigured runs");
    eprintln!("  fail with a not-configured diagnostic instead of serving fakes.");
    eprintln!("  With --demo, every tool is scripted and one canned submission fires.");
    eprintln!("  Project root is --tools-root, defaulting to the working directory.");
    eprintln!("  Development tools are default: project/temp operations need no approval.");
    eprintln!("  --strict-tools keeps jailed reads and approval-required writes/exec.");
    std::process::exit(2);
}

/// Provider selection for one slot, shared by its frontend and its
/// runtime. Cloned into the provider at slot creation and refreshed on
/// every model switch, so cycling models applies to the next run without
/// a restart. The credential itself is never stored here: it is looked
/// up from this process's inherited environment on each run.
#[derive(Debug, Clone)]
struct LiveSelection {
    config: UserConfig,
    active_model: Option<String>,
    /// Scripted demo explicitly requested. The resolver serves fakes only
    /// through this flag; an unconfigured selection without it fails
    /// instead of silently demoing.
    demo: bool,
    /// Selection captured immediately before submit and consumed by the
    /// provider when this accepted run first invokes it.
    pending_run: Option<Box<LiveSelection>>,
    pending_run_id: Option<RunId>,
}

/// Shared handle between a slot's [`Frontend`] and its provider.
type LiveHandle = Arc<Mutex<LiveSelection>>;

/// Per-run resolution outcome. `Demo` (nothing selected) keeps the
/// historic scripted script; `Failed` carries a static diagnostic for a
/// selection that exists but is unusable, so a missing credential is an
/// explicit terminal error instead of a silent fake run.
enum LiveResolve {
    Demo,
    Live(OpenAiProvider),
    Failed(ErrorCategory, &'static str),
}

/// Resolves the active model to a live adapter through an injected
/// credential lookup, so the missing/empty contract is testable without
/// touching the process environment. Values never enter the diagnostic.
fn resolve_live_adapter(
    selection: &LiveSelection,
    lookup: impl FnOnce(&str) -> Option<String>,
) -> LiveResolve {
    let Some(model_id) = selection.active_model.as_deref() else {
        if selection.demo {
            return LiveResolve::Demo;
        }
        return LiveResolve::Failed(
            ErrorCategory::InvalidInput,
            "no model configured: add a model or relaunch with --demo for the scripted demo",
        );
    };
    let Some(entry) = selection.config.model(model_id) else {
        return LiveResolve::Failed(ErrorCategory::InvalidInput, "selected model is unknown");
    };
    let Some(profile) = selection.config.provider(&entry.provider) else {
        return LiveResolve::Failed(ErrorCategory::InvalidInput, "selected provider is unknown");
    };
    if resolve_with(&profile.credential, lookup).is_err() {
        return LiveResolve::Failed(
            ErrorCategory::Authentication,
            "provider credential is unavailable",
        );
    }
    match OpenAiProvider::from_profile(profile, &entry.name) {
        Ok(adapter) => LiveResolve::Live(adapter),
        Err(_) => LiveResolve::Failed(ErrorCategory::InvalidInput, "selected provider is invalid"),
    }
}

/// Reads a credential value from the process environment. Empty values
/// count as missing; the value itself never enters a diagnostic.
fn env_credential(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|value| !value.is_empty())
}

/// Terminal provider failure with a static diagnostic. A missing
/// credential is retryable after updating it and restarting; an
/// invalid selection is not.
fn live_failure(category: ErrorCategory, message: &'static str) -> ProviderEvent {
    let retry = match category {
        ErrorCategory::Authentication => RetryGuidance::SafeToRetry,
        _ => RetryGuidance::DoNotRetry,
    };
    ProviderEvent::Failed(
        AgentError::new(category, message, retry).expect("static safe provider message builds"),
    )
}

/// One conversation slot: an owned runtime, its unconsumed event streams,
/// and the frontend (presentation, focus, merge, approval) for it.
/// Switching slots never moves runs, approvals, or drafts between them; a
/// background slot's events buffer in its bounded channels until it is
/// active again.
struct SessionSlot {
    /// Short display label (`s1`, `s2`, ...), stable for the process.
    label: String,
    runtime: Runtime,
    streams: EventStreams,
    front: Frontend,
}

/// Local conversation registry: every slot shares the startup
/// configuration template but owns its runtime and history. Only the active
/// slot is polled and drawn; background slots keep buffering. A future
/// remote backend plugs in here beside the local runtime without changing
/// the picker surface.
struct SessionRegistry {
    slots: Vec<SessionSlot>,
    active: usize,
    template: SessionConfig,
}

impl SessionRegistry {
    fn new(first: SessionSlot, template: SessionConfig) -> Self {
        Self {
            slots: vec![first],
            active: 0,
            template,
        }
    }

    /// Borrows the active slot.
    fn active(&self) -> &SessionSlot {
        &self.slots[self.active]
    }

    /// Mutably borrows the active slot.
    fn active_mut(&mut self) -> &mut SessionSlot {
        &mut self.slots[self.active]
    }

    /// Counts live slots.
    fn len(&self) -> usize {
        self.slots.len()
    }

    /// Resolves a switch target: 1-based index (`2`), label (`s2`), or exact
    /// runtime session id. Anything else is `None`, never a guess.
    fn resolve_target(&self, target: &str) -> Option<usize> {
        let trimmed = target.trim();
        if let Ok(number) = trimmed.parse::<usize>()
            && (1..=self.slots.len()).contains(&number)
        {
            return Some(number - 1);
        }
        self.slots
            .iter()
            .position(|slot| slot.label == trimmed || slot.front.session.as_str() == trimmed)
    }

    /// Creates a new empty slot and switches to it. A wiring failure leaves
    /// the registry untouched and reports the error instead.
    fn create(&mut self) -> io::Result<usize> {
        let number = self.slots.len() + 1;
        let (runtime, streams, live) = build_live_runtime(&self.template)?;
        let session =
            SessionId::new(format!("{DEMO_SESSION}-{number}")).map_err(io::Error::other)?;
        let mut front = Frontend::with_config(session, self.template.clone());
        let label = format!("s{number}");
        front.state.set_session_label(&label);
        front.state.notice(&format!("session {label} started"));
        front.attach_live(live, self.template.tools_root.as_deref());
        self.slots.push(SessionSlot {
            label,
            runtime,
            streams,
            front,
        });
        self.active = self.slots.len() - 1;
        Ok(self.active)
    }

    /// Switches to `index`. Returns false for out-of-range indices and for
    /// the already-active slot.
    fn switch(&mut self, index: usize) -> bool {
        if index >= self.slots.len() || index == self.active {
            return false;
        }
        self.active = index;
        true
    }

    /// One transcript line per slot: label, runtime session id, active
    /// marker, and live status. The active slot sorts first implicitly by
    /// its marker; order is creation order otherwise.
    fn describe(&self) -> Vec<String> {
        self.slots
            .iter()
            .enumerate()
            .map(|(index, slot)| {
                let marker = if index == self.active {
                    " [active]"
                } else {
                    ""
                };
                format!(
                    "{} ({}){marker} — {}",
                    slot.label,
                    slot.front.session.as_str(),
                    slot.front.state.status()
                )
            })
            .collect()
    }
}

/// Demo-or-live provider: serves a fresh scripted demo script for every
/// run while nothing is selected, and the configured OpenAI-compatible
/// adapter once the slot's selection resolves. The shared [`FakeProvider`]
/// consumes its script queue across calls, so without a reset the second
/// task in one process would observe an exhausted script and fail
/// instantly. Resetting per run-id keeps each demo task replayable; real
/// adapters never replay.
///
/// Resolution happens per run (not per process) from the slot's shared
/// selection, so model switches apply to the next run. Environment
/// credentials are inherited at process startup; exporting in another
/// shell cannot update this process. An unusable selection (missing
/// credential, invalid endpoint) fails the run with an explicit terminal
/// error: it never falls back to a silent fake.
struct PerRunProvider {
    capabilities: ProviderCapabilities,
    binding: LiveHandle,
    current: Mutex<PerRunState>,
}

enum ActiveProvider {
    Fake(FakeProvider),
    Live(OpenAiProvider),
    Unavailable(ErrorCategory, &'static str),
}

struct PerRunState {
    run: Option<RunId>,
    provider: ActiveProvider,
}

impl PerRunProvider {
    /// Demo-only wiring: the shared selection carries no models, so every
    /// run serves the scripted demo. Unit tests use this; production uses
    /// [`PerRunProvider::live`].
    fn new() -> Self {
        Self::live(Arc::new(Mutex::new(LiveSelection {
            config: UserConfig::default_config(),
            active_model: None,
            demo: true,
            pending_run: None,
            pending_run_id: None,
        })))
    }

    /// Production wiring over the slot's shared selection.
    fn live(binding: LiveHandle) -> Self {
        // Fake and real adapters advertise identical capabilities, so one
        // static claim covers both resolutions.
        let capabilities = FakeProvider::demo_two_turn().capabilities();
        Self {
            capabilities,
            binding,
            current: Mutex::new(PerRunState {
                run: None,
                provider: ActiveProvider::Fake(FakeProvider::demo_two_turn()),
            }),
        }
    }
}

impl ProviderPort for PerRunProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities.clone()
    }

    fn stream(&self, request: &ModelRequest, context: &ProviderContext) -> Vec<ProviderEvent> {
        self.run_turn(request, context, None)
    }

    fn supports_incremental_streaming(&self) -> bool {
        // The live path forwards wire deltas; demo providers may replay
        // their completed prefix, without changing authoritative validation.
        true
    }

    fn stream_with_sink(
        &self,
        request: &ModelRequest,
        context: &ProviderContext,
        sink: &(dyn Fn(ProviderEvent) + Send + Sync),
    ) -> Vec<ProviderEvent> {
        self.run_turn(request, context, Some(sink))
    }
}

impl PerRunProvider {
    fn run_turn(
        &self,
        request: &ModelRequest,
        context: &ProviderContext,
        sink: Option<&(dyn Fn(ProviderEvent) + Send + Sync)>,
    ) -> Vec<ProviderEvent> {
        let mut current = self.current.lock().expect("demo provider lockable");
        if current.run.as_ref() != Some(request.run()) {
            current.run = Some(request.run().clone());
            let selection = take_selection_for_run(&self.binding, request.run());
            current.provider = match resolve_live_adapter(&selection, env_credential) {
                LiveResolve::Demo => ActiveProvider::Fake(FakeProvider::demo_two_turn()),
                LiveResolve::Live(adapter) => ActiveProvider::Live(adapter),
                LiveResolve::Failed(category, message) => {
                    ActiveProvider::Unavailable(category, message)
                }
            };
        }
        match &current.provider {
            ActiveProvider::Fake(provider) => match sink {
                Some(sink) => provider.stream_with_sink(request, context, sink),
                None => provider.stream(request, context),
            },
            ActiveProvider::Live(adapter) => match sink {
                Some(sink) => adapter.stream_with_sink(request, context, sink),
                None => adapter.stream(request, context),
            },
            ActiveProvider::Unavailable(category, message) => {
                vec![live_failure(*category, message)]
            }
        }
    }
}

fn take_selection_for_run(binding: &LiveHandle, run: &RunId) -> LiveSelection {
    let mut binding = binding.lock().expect("live selection lockable");
    if binding
        .pending_run_id
        .as_ref()
        .is_some_and(|pending| pending != run)
    {
        binding.pending_run = None;
        binding.pending_run_id = None;
    }
    let selection = binding
        .pending_run
        .take()
        .map_or_else(|| binding.clone(), |snapshot| *snapshot);
    binding.pending_run_id = None;
    selection
}

/// Production composition over the startup configuration: the provider
/// resolves per run from the slot's selection (demo script while nothing
/// is selected, live adapter once it resolves), and file reads/writes are
/// real and jailed to the startup root (`--tools-root`, else the working
/// directory). Returns the shared
/// selection handle alongside so the frontend keeps it in sync on model
/// switches.
///
/// A root that cannot be canonicalized or is not a directory is an
/// explicit startup error, never a silent fallback to scripted reads.
fn build_live_runtime(
    session_config: &SessionConfig,
) -> io::Result<(Runtime, EventStreams, LiveHandle)> {
    let config = RuntimeConfig {
        limits: tui_runtime_limits(),
        policy: match &session_config.tools_root {
            Some(root) if !session_config.strict_tools => {
                Policy::development(root).map_err(io::Error::other)?
            }
            _ => Policy::m0_test(),
        },
        has_approval_handler: true,
    };
    let tools: Vec<Arc<dyn nexus_core::ToolPort + Send + Sync>> = match &session_config.tools_root {
        None => vec![
            Arc::new(FakeTool::read_only()),
            Arc::new(FakeTool::mutation()),
        ],
        Some(root) => {
            let canonical = std::fs::canonicalize(root).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "tools root is unusable")
            })?;
            if !canonical.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "tools root is unusable",
                ));
            }
            if !session_config.strict_tools {
                let mut tools =
                    nexus_tools::development_file_tools(&canonical).map_err(io::Error::other)?;
                tools.push(Arc::new(
                    SandboxedExecutor::development(&canonical).map_err(io::Error::other)?,
                ));
                return runtime_with_tools(session_config, config, tools);
            }
            vec![
                Arc::new(ScopedReader::with_root(&canonical).map_err(io::Error::other)?),
                Arc::new(ScopedLister::with_root(&canonical).map_err(io::Error::other)?),
                Arc::new(ScopedSearcher::with_root(&canonical).map_err(io::Error::other)?),
                Arc::new(ScopedWriter::with_root(&canonical).map_err(io::Error::other)?),
                Arc::new(ScopedPatcher::with_root(&canonical).map_err(io::Error::other)?),
                Arc::new(SandboxedExecutor::with_root(&canonical).map_err(io::Error::other)?),
            ]
        }
    };
    runtime_with_tools(session_config, config, tools)
}

fn runtime_with_tools(
    session_config: &SessionConfig,
    config: RuntimeConfig,
    tools: Vec<Arc<dyn nexus_core::ToolPort + Send + Sync>>,
) -> io::Result<(Runtime, EventStreams, LiveHandle)> {
    let binding = Arc::new(Mutex::new(LiveSelection {
        config: session_config.config.clone(),
        active_model: session_config.active_model.clone(),
        demo: session_config.demo,
        pending_run: None,
        pending_run_id: None,
    }));
    let provider = Arc::new(PerRunProvider::live(Arc::clone(&binding)));
    let (runtime, streams) = Runtime::try_new(config, provider, tools).map_err(io::Error::other)?;
    Ok((runtime, streams, binding))
}

/// One-line wiring report for the transcript: which provider (if any) the
/// next run resolves to, and whether file tools are real. Only counts and the
/// operator's own identities appear; values never do.
fn wiring_notice(selection: &LiveSelection, tools_root: Option<&std::path::Path>) -> String {
    let provider = match resolve_live_adapter(selection, env_credential) {
        LiveResolve::Demo => "demo script".to_owned(),
        LiveResolve::Live(adapter) => format!("live adapter ({})", adapter.model()),
        LiveResolve::Failed(_, message) => format!("unavailable ({message})"),
    };
    let tools = match tools_root {
        None => "scripted tools".to_owned(),
        Some(root) => format!(
            "real file tools + sandboxed exec at {} (writes/exec require approval)",
            root.display()
        ),
    };
    format!("wiring: {provider}; {tools}")
}

fn main() {
    install_panic_hook();
    eprintln!(
        "nexus-tui dev: live wiring by default, ephemeral store. \
          A slot whose model resolves to a configured provider with a credential \
          uses the real adapter (network egress, billable); --tools-root enables \
           real jailed file tools + sandboxed exec (writes/exec require approval). \
           Without a configured provider, runs fail as not-configured; \
           relaunch with --demo for the scripted demo. No durable storage."
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
    if io::stdout().is_terminal() {
        match nexus_tui::TerminalGuard::setup() {
            Ok(mut guard) => {
                let report = interactive(session_config).await;
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
                if !session_config.demo {
                    return Err(io::Error::other(format!(
                        "terminal setup failed ({error}); relaunch with --demo for the scripted transcript fallback"
                    )));
                }
                eprintln!("terminal setup failed ({error}); demo transcript fallback");
                let (runtime, streams) = build_runtime()?;
                headless(runtime, streams, session_config).await
            }
        }
    } else {
        if !session_config.demo {
            return Err(io::Error::other(
                "no terminal attached; relaunch with --demo for the scripted transcript fallback",
            ));
        }
        let (runtime, streams) = build_runtime()?;
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
    /// Scripted demo explicitly requested with `--demo`, from
    /// [`SessionConfig`]. Mirrored into the shared selection so runs
    /// resolve fakes only through this flag.
    demo: bool,
    /// Currently selected model id, or `None` when none is configured.
    active_model: Option<String>,
    /// Actual model identity captured for each accepted run. Terminal usage
    /// is attributed from this map, never from the mutable picker selection.
    run_models: HashMap<RunId, String>,
    /// Shared provider selection for this slot's runtime. `None` in unit
    /// tests (which use the demo-only wiring); production slots set it
    /// right after [`Frontend::with_config`] and refresh it on every
    /// model switch so the next run resolves the new selection.
    live: Option<LiveHandle>,
    /// Queued `:session` intents in recording order: key handling appends on
    /// the active front, and the loop drains them against the registry after
    /// the batch, where runtimes can be built. At most one push per Enter
    /// keypress, drained every tick, so the queue stays tiny.
    pending_session_cmds: Vec<SessionArgs>,
    /// Stable tool-card identity; committed on release, never on a drag.
    pointer_press: Option<usize>,
    /// First `g` in a viewport `gg` jump is waiting for its second key.
    g_pending: bool,
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
            run_models: HashMap::new(),
            demo: false,
            live: None,
            pending_session_cmds: Vec::new(),
            pointer_press: None,
            g_pending: false,
        }
    }

    /// Builds a frontend over the already-loaded startup configuration, so
    /// the interactive and headless paths share one load and one document.
    fn with_config(session: SessionId, config: SessionConfig) -> Self {
        let mut front = Self {
            config: config.config,
            config_path: config.path,
            active_model: config.active_model,
            run_models: HashMap::new(),
            demo: config.demo,
            ..Self::new(session)
        };
        front.sync_model_display();
        // Composer modes: built-ins first, then configuration customs, with
        // the configured default (or `build`) selected.
        let mut modes = vec![AgentMode::plan(), AgentMode::build()];
        modes.extend(front.config.modes().iter().cloned());
        front.state.set_modes(modes, front.config.default_mode());
        if let Some(dir) = session_project_dir() {
            front.state.set_project_dir(dir);
        }
        front
    }

    /// Mirrors the current document and selection into the runtime's shared
    /// selection, so the next run resolves what the header shows. No-op
    /// without a production handle (unit tests).
    fn sync_live_binding(&mut self) {
        if let Some(binding) = &self.live {
            let mut binding = binding.lock().expect("live selection lockable");
            let pending_run = binding.pending_run.take();
            let pending_run_id = binding.pending_run_id.take();
            *binding = LiveSelection {
                config: self.config.clone(),
                active_model: self.active_model.clone(),
                demo: self.demo,
                pending_run,
                pending_run_id,
            };
        }
    }

    /// Freezes the current provider/model choice before dispatch. The
    /// provider consumes this snapshot on its first call for the run.
    fn prepare_run_selection(&mut self) -> bool {
        if let Some(binding) = &self.live {
            let mut binding = binding.lock().expect("live selection lockable");
            // A pending snapshot belongs to a previously accepted run whose
            // provider has not made its first call yet. A Busy submission
            // must never replace that run's execution identity.
            if binding.pending_run.is_some() {
                return false;
            }
            let mut snapshot = binding.clone();
            snapshot.pending_run = None;
            snapshot.pending_run_id = None;
            binding.pending_run = Some(Box::new(snapshot));
            binding.pending_run_id = None;
            true
        } else {
            false
        }
    }

    fn clear_unaccepted_selection(&mut self, owns_pending: bool) {
        if owns_pending && let Some(binding) = &self.live {
            let mut binding = binding.lock().expect("live selection lockable");
            binding.pending_run = None;
            binding.pending_run_id = None;
        }
    }

    fn bind_pending_selection(&mut self, run: &RunId) {
        if let Some(binding) = &self.live {
            let mut binding = binding.lock().expect("live selection lockable");
            if binding.pending_run.is_some() {
                binding.pending_run_id = Some(run.clone());
            }
        }
    }

    fn clear_unconsumed_selection(&mut self, run: &RunId) {
        if let Some(binding) = &self.live {
            let mut binding = binding.lock().expect("live selection lockable");
            if binding.pending_run_id.as_ref() == Some(run) {
                binding.pending_run = None;
                binding.pending_run_id = None;
            }
        }
    }

    async fn clear_finalized_pending_selection(&mut self, runtime: &Runtime) {
        let run = self.live.as_ref().and_then(|binding| {
            binding
                .lock()
                .expect("live selection lockable")
                .pending_run_id
                .clone()
        });
        let Some(run) = run else {
            return;
        };
        let command = GetSnapshotCommand {
            request: self.next_request(),
            run: run.clone(),
        };
        let (_, snapshot) = runtime.get_snapshot(command).await;
        if snapshot.is_some_and(|snapshot| snapshot.lifecycle() != nexus_core::RunLifecycle::Active)
        {
            self.clear_unconsumed_selection(&run);
        }
    }

    /// Attaches the slot's shared selection handle, syncs it, and reports
    /// the resolved wiring to the transcript. Production slots call this
    /// once, right after [`Frontend::with_config`].
    fn attach_live(&mut self, live: LiveHandle, tools_root: Option<&std::path::Path>) {
        self.live = Some(live);
        self.sync_live_binding();
        let message = {
            let handle = self.live.as_ref().expect("live handle attached");
            let selection = handle.lock().expect("live selection lockable");
            wiring_notice(&selection, tools_root)
        };
        self.state.notice(&message);
    }

    /// True when the next run resolves to a real adapter (selection known,
    /// credential present, endpoint valid). The interactive canned demo
    /// submission is gated on the negation: auto-firing a real network
    /// call on every launch would bill the operator for a transcript
    /// nobody asked for. No handle (unit tests) counts as demo.
    fn is_live(&self) -> bool {
        let Some(binding) = &self.live else {
            return false;
        };
        let selection = binding.lock().expect("live selection lockable");
        matches!(
            resolve_live_adapter(&selection, env_credential),
            LiveResolve::Live(_)
        )
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
        self.sync_live_binding();
    }

    /// Selects exactly the named configured model, mirroring it into the
    /// display and live binding. Unknown ids report the available list and
    /// change nothing: a failed switch never invents or clears a selection.
    fn select_model(&mut self, name: &str) {
        if self.config.model(name).is_some() {
            self.active_model = Some(name.to_owned());
            self.sync_model_display();
            self.sync_live_binding();
            self.state.notice(&format!("model: {name}"));
        } else {
            let mut ids: Vec<&str> = self
                .config
                .models()
                .iter()
                .map(|entry| entry.id.as_str())
                .collect();
            ids.sort_unstable();
            if ids.is_empty() {
                self.state.notice("unknown model (no configured models)");
            } else {
                self.state
                    .notice(&format!("unknown model: available: {}", ids.join(", ")));
            }
        }
    }

    /// Opens the searchable model picker over the configured models, or
    /// reports that nothing is configured. Shared by bare `:model` and `:m`.
    fn open_model_picker(&mut self) {
        let choices: Vec<ModelChoice> = self
            .config
            .models()
            .iter()
            .map(|entry| ModelChoice {
                id: entry.id.clone(),
                provider: entry.provider.clone(),
            })
            .collect();
        if choices.is_empty() {
            self.state.notice("no configured models");
        } else {
            self.state.open_model_picker(choices);
        }
    }

    fn refresh_command_completions(&mut self) {
        let Some(input) = self.state.command_line() else {
            self.state.clear_completions();
            return;
        };
        let mut candidates: Vec<(String, String)> = vec![
            (
                "help".into(),
                "Show desktop commands and keyboard help".into(),
            ),
            ("m".into(), "Open the model picker".into()),
            ("model".into(), "Select or inspect the active model".into()),
            (
                "usage".into(),
                "Show observed input/output token usage".into(),
            ),
            ("session".into(), "Show or manage local sessions".into()),
            ("s".into(), "List sessions".into()),
            ("q".into(), "Quit the TUI".into()),
            ("quit".into(), "Quit the TUI".into()),
        ];
        if input.starts_with("model ") {
            candidates.clear();
            candidates.push((
                "model set ".into(),
                "Set an exact configured model ID".into(),
            ));
            if let Some(prefix) = input.strip_prefix("model set ") {
                candidates.extend(
                    self.config
                        .models()
                        .iter()
                        .map(|model| {
                            (
                                format!("model set {}", model.id),
                                format!("Use model from {}", model.provider),
                            )
                        })
                        .filter(|(candidate, _)| {
                            candidate.starts_with(&format!("model set {prefix}"))
                        }),
                );
            }
        } else if input == "s" {
            candidates = vec![
                ("s".into(), "List sessions".into()),
                ("session".into(), "Show or manage local sessions".into()),
            ];
        } else if input.starts_with("session ") {
            candidates = vec![
                (
                    "session new".into(),
                    "Create and switch to a new session".into(),
                ),
                ("session list".into(), "List available sessions".into()),
                (
                    "session switch ".into(),
                    "Switch to a session by index or ID".into(),
                ),
                ("session help".into(), "Show session command usage".into()),
                ("session usage".into(), "Show session command usage".into()),
            ];
        }
        candidates.retain(|(candidate, _)| candidate.starts_with(input));
        self.state.set_completion_items(
            candidates
                .into_iter()
                .map(|(insert, info)| CompletionItem {
                    label: insert.clone(),
                    insert,
                    info,
                })
                .collect(),
        );
    }

    fn refresh_file_completions(&mut self) {
        let Some((_, _, query)) = self.state.composer_at_token() else {
            self.state.clear_completions();
            return;
        };
        let Some(root) = self.state.project_dir() else {
            self.state.clear_completions();
            return;
        };
        self.state.set_completion_items(
            find_project_paths(std::path::Path::new(root), &query)
                .into_iter()
                .map(|path| CompletionItem {
                    info: if path.ends_with('/') {
                        "Directory · continue path".to_owned()
                    } else {
                        "Project file · reference only".to_owned()
                    },
                    label: path.clone(),
                    insert: path,
                })
                .collect(),
        );
    }

    fn accept_completion(&mut self) {
        let Some(candidate) = self
            .state
            .selected_completion()
            .map(|candidate| candidate.insert.clone())
        else {
            return;
        };
        if self.state.command_line().is_none()
            && let Some((start, end, _)) = self.state.composer_at_token()
        {
            self.state.complete_composer_token(start, end, &candidate);
            return;
        }
        self.state.set_command_line(candidate);
        self.state.clear_completions();
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
    fn record_model_use(&mut self, run: &RunId) {
        let Some(id) = self.run_models.remove(run) else {
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

    /// Takes queued session intents in recording order, clearing the queue
    /// so each executes exactly once.
    fn take_session_cmds(&mut self) -> Vec<SessionArgs> {
        std::mem::take(&mut self.pending_session_cmds)
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
                self.state.clear_colon();
                // The run completed, so this model is genuinely "used":
                // record it before the notice history moves on.
                self.record_model_use(event.run());
                // A run can be cancelled or fail before its provider's first
                // call consumes the selection snapshot. Do not let that
                // abandoned identity leak into the next accepted run.
                self.clear_unconsumed_selection(event.run());
                return true;
            }
        }
        if !had_pending && self.state.pending_approval().is_some() {
            // A newly arrived approval takes keyboard focus at once; any
            // open dialog steps aside so its keys cannot shadow the
            // decision shortcuts.
            self.state.dismiss_overlay();
            self.state.clear_colon();
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
    Event(Box<RunEvent>),
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
            Some(event) => LoopStep::Event(Box::new(event)),
            None => LoopStep::Closed,
        },
        event = control.recv() => match event {
            Some(event) => LoopStep::Event(Box::new(event)),
            None => LoopStep::Closed,
        },
        _ = interval.tick() => LoopStep::Tick,
    }
}

/// Non-blocking next input event; resize traffic is drained and ignored so
/// it cannot stall the loop, while mouse wheel notches surface as scroll
/// input beside key presses.
fn read_input() -> io::Result<Option<Input>> {
    while poll(Duration::ZERO)? {
        match read()? {
            Event::Key(key) => return Ok(Some(Input::Key(key))),
            Event::Mouse(mouse) => return Ok(Some(Input::Mouse(mouse))),
            _ => {}
        }
    }
    Ok(None)
}

/// One terminal input: a key press or a mouse report. Wheel, press, drag,
/// and release drive scrolling and drag selection; other mouse traffic is
/// drained and ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Input {
    Key(KeyEvent),
    Mouse(MouseEvent),
}

/// Mouse wheel notches per scroll step (M0-test choice: a few wrapped lines
/// per notch, never a full page jump).
const WHEEL_LINES: usize = 3;

/// Applies one mouse report. Wheel notches scroll the expanded approval
/// detail while it is open, else the conversation viewport; a left press
/// starts a drag selection (body rows or the composer draft), dragging
/// extends it, and release copies it to the clipboard. Focus is never moved
/// by the mouse. Returns true when state changed and a redraw is due.
/// Maps a terminal cell onto a body content position, if the cell sits
/// on one. Columns past the text width (scrollbar, margins) and rows over
/// hero, notice, or empty space yield `None` and never anchor. Terminal
/// columns are cells; the returned column is a character index into the
/// given window line, so CJK-wide glyphs map exactly.
fn body_hit(state: &AppState, lines: &[String], column: u16, row: u16) -> Option<(usize, usize)> {
    let geo = state.pointer_geometry();
    if column < geo.body_x || row < geo.body_y {
        return None;
    }
    let cell = (column as usize).saturating_sub(geo.body_x as usize);
    if cell >= geo.text_width.max(1) {
        return None;
    }
    let rel = (row as usize).saturating_sub(geo.body_y as usize);
    let index = rel.checked_sub(geo.content_offset)?;
    if index >= geo.content_len {
        return None;
    }
    let line = lines.get(index).map(String::as_str).unwrap_or("");
    Some((index, cell_to_char_col(line, cell)))
}

/// Fresh window rows for hit mapping: the same window the renderer drew;
// heights are cached so this never re-measures history.
fn window_lines(state: &AppState) -> Vec<String> {
    state
        .visible_lines(state.viewport_width(), state.viewport_height())
        .lines
}

/// Maps a terminal cell onto a composer draft character offset, if the
/// cell sits on a draft row. Columns clamp to the line end; rows past
/// the last draft line yield `None`.
fn composer_hit(state: &AppState, column: u16, row: u16) -> Option<usize> {
    let geo = state.pointer_geometry();
    let rel_row = (row as usize).saturating_sub((geo.composer_y + 1) as usize);
    if rel_row >= geo.composer_rows.max(1) {
        return None;
    }
    let cell = (column as usize).saturating_sub((geo.composer_x + 1) as usize);
    let mut offset = 0usize;
    for (index, line) in state.composer().lines().enumerate() {
        if index == rel_row {
            return Some(offset + cell_to_char_col(line, cell));
        }
        offset += line.chars().count() + 1;
    }
    // A trailing-newline draft has one more (empty) visual row.
    (rel_row == state.composer().lines().count()).then_some(offset)
}

/// Yank text is the exact visible selection: window rows as drawn for
/// the body, the draft substring for the composer.
fn selection_text(state: &AppState) -> Option<String> {
    match state.text_selection() {
        Some(TextSelection::Composer { .. }) => state.composer_selection_text(),
        Some(TextSelection::Body { .. }) => {
            let viewport = (state.viewport_width(), state.viewport_height());
            let view = state.visible_lines(viewport.0, viewport.1);
            state.body_selection_text(&view.lines)
        }
        None => None,
    }
}

/// Minimal base64 (RFC 4648 alphabet) for the OSC 52 clipboard write.
/// Hand-rolled so selection adds no dependency.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut word = 0u32;
        for byte in chunk {
            word = (word << 8) | u32::from(*byte);
        }
        word <<= 8 * (3 - chunk.len());
        for _ in 0..chunk.len() + 1 {
            out.push(ALPHABET[(word >> 18) as usize & 63] as char);
            word = (word << 6) & 0xFF_FFFF;
        }
        for _ in chunk.len() + 1..4 {
            out.push('=');
        }
    }
    out
}

/// Copies text to the system clipboard, silently on every path: no
/// notice, no diagnostic. A platform helper goes first (`pbcopy` on macOS,
/// `wl-copy`/`xclip`/`xsel` on Linux), because helpers work in every
/// terminal while OSC 52 is honored only by some. OSC 52 stays as the
/// fallback for the rest. Empty text never copies.
fn yank_to_clipboard(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    if write_clipboard_helper(text) {
        return true;
    }
    write_osc52(text)
}

/// Selects the platform clipboard helper (program plus argv), or `None`
/// when no known helper is installed. `probe` reports an executable on
/// `PATH`; callers pass the real lookup, tests inject fakes so no process
/// ever spawns there.
fn clipboard_command(
    probe: &dyn Fn(&str) -> bool,
) -> Option<(&'static str, &'static [&'static str])> {
    #[cfg(target_os = "macos")]
    if probe("pbcopy") {
        return Some(("pbcopy", &[] as &[&str]));
    }
    #[cfg(target_os = "linux")]
    {
        if probe("wl-copy") {
            return Some(("wl-copy", &[] as &[&str]));
        }
        if probe("xclip") {
            return Some(("xclip", &["-selection", "clipboard"] as &[&str]));
        }
        if probe("xsel") {
            return Some(("xsel", &["--clipboard", "--input"] as &[&str]));
        }
    }
    None
}

/// True when `name` resolves to an executable file on `PATH`.
fn executable_on_path(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .any(|path| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                path.is_file()
                    && path
                        .metadata()
                        .map(|meta| meta.permissions().mode() & 0o111 != 0)
                        .unwrap_or(false)
            }
            #[cfg(not(unix))]
            {
                path.is_file()
            }
        })
}

/// Feeds text to the platform helper's stdin and reaps it. Every failure
/// (missing helper, failed spawn, broken pipe, bad exit) falls through to
/// the OSC 52 fallback via a false return.
fn write_clipboard_helper(text: &str) -> bool {
    use std::io::Write as _;
    let Some((program, args)) = clipboard_command(&executable_on_path) else {
        return false;
    };
    let mut child = match std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    let wrote = child
        .stdin
        .take()
        .map(|mut stdin| stdin.write_all(text.as_bytes()).is_ok())
        .unwrap_or(false);
    // stdin drops here, so the helper sees EOF and exits; reaping never
    // claims success the helper did not report.
    wrote && child.wait().map(|status| status.success()).unwrap_or(false)
}

/// Writes the OSC 52 clipboard sequence. Returns whether the bytes went
/// out; terminals decide whether to honor them. Silent outside a terminal
/// (notably under test harnesses with piped stdout).
fn write_osc52(text: &str) -> bool {
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return false;
    }
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    let encoded = base64_encode(text.as_bytes());
    write!(stdout, "\x1b]52;c;{encoded}\x07").is_ok() && stdout.flush().is_ok()
}

/// Bounded, symlink-free path completion beneath the canonical project root.
/// It only returns relative names; it never opens or attaches file contents.
fn find_project_paths(root: &std::path::Path, query: &str) -> Vec<String> {
    use std::time::{Duration, Instant};
    const MAX_VISITED: usize = 1_000;
    const MAX_DEPTH: usize = 5;
    const MAX_RESULTS: usize = 10;
    const BUDGET: Duration = Duration::from_millis(12);
    const EXCLUDED: &[&str] = &[
        ".git",
        ".nexus",
        ".next",
        ".venv",
        "__pycache__",
        "build",
        "dist",
        "node_modules",
        "target",
        "vendor",
    ];

    let Ok(root) = std::fs::canonicalize(root) else {
        return Vec::new();
    };
    if !root.is_dir() {
        return Vec::new();
    }
    let query = query.to_lowercase();
    let started = Instant::now();
    let mut visited = 0usize;
    let mut stack = vec![(root.clone(), String::new(), 0usize)];
    let mut matches = Vec::<(bool, String)>::new();
    while let Some((directory, prefix, depth)) = stack.pop() {
        if visited >= MAX_VISITED || started.elapsed() >= BUDGET {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        let remaining = MAX_VISITED.saturating_sub(visited);
        let mut entries: Vec<_> = entries
            .take(remaining.saturating_add(1))
            .filter_map(Result::ok)
            .collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            visited += 1;
            if visited > MAX_VISITED || started.elapsed() >= BUDGET {
                break;
            }
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name.is_empty()
                || name.chars().any(|char| {
                    char.is_control()
                        || nexus_tui::sanitize::sanitize(&char.to_string()) != char.to_string()
                })
            {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            // file_type does not follow symlinks; symlink targets are never
            // traversed or offered as paths.
            if file_type.is_symlink() {
                continue;
            }
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let is_dir = file_type.is_dir();
            if !is_dir || depth < MAX_DEPTH {
                let needle = relative.to_lowercase();
                if query.is_empty() || needle.contains(&query) {
                    let display = if is_dir {
                        format!("{relative}/")
                    } else {
                        relative.clone()
                    };
                    matches.push((needle.starts_with(&query), display));
                }
            }
            if is_dir && depth < MAX_DEPTH && !EXCLUDED.contains(&name.as_str()) {
                stack.push((entry.path(), relative, depth + 1));
            }
        }
    }
    matches.sort_by(|(prefix_a, path_a), (prefix_b, path_b)| {
        prefix_b.cmp(prefix_a).then_with(|| path_a.cmp(path_b))
    });
    matches
        .into_iter()
        .take(MAX_RESULTS)
        .map(|(_, path)| path)
        .collect()
}

fn handle_mouse(front: &mut Frontend, mouse: MouseEvent) -> bool {
    // The mouse never completes a `:` command: a press disarms it so a
    // later letter cannot fire behind a click.
    if matches!(mouse.kind, MouseEventKind::Down(_)) {
        front.state.clear_colon();
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            front.pointer_press = None;
            if front.state.approval_detail_open() {
                front.state.approval_detail_scroll(-(WHEEL_LINES as isize));
            } else {
                front.state.scroll_up(WHEEL_LINES);
            }
            true
        }
        MouseEventKind::ScrollDown => {
            front.pointer_press = None;
            if front.state.approval_detail_open() {
                front.state.approval_detail_scroll(WHEEL_LINES as isize);
            } else {
                front.state.scroll_down(WHEEL_LINES);
            }
            true
        }
        MouseEventKind::Down(MouseButton::Left) => {
            front.pointer_press = None;
            // A new press replaces any previous selection outright; cells
            // outside both regions only clear.
            let window = window_lines(&front.state);
            if let Some((row, col)) = body_hit(&front.state, &window, mouse.column, mouse.row) {
                front.pointer_press = front.state.tool_identity_at(row);
                front.state.begin_body_selection(row, col);
            } else if let Some(offset) = composer_hit(&front.state, mouse.column, mouse.row) {
                front.state.begin_composer_selection(offset);
            } else {
                front.state.clear_text_selection();
            }
            true
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            front.pointer_press = None;
            if front.state.text_selection().is_none() {
                return false;
            }
            let window = window_lines(&front.state);
            if let Some((row, col)) = body_hit(&front.state, &window, mouse.column, mouse.row) {
                front.state.extend_body_selection(row, col);
            } else if let Some(offset) = composer_hit(&front.state, mouse.column, mouse.row) {
                front.state.extend_composer_selection(offset);
            } else {
                return false;
            }
            true
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some(identity) = front.pointer_press.take() {
                let window = window_lines(&front.state);
                if let Some((row, _)) = body_hit(&front.state, &window, mouse.column, mouse.row)
                    && front.state.tool_identity_at(row) == Some(identity)
                    && front.state.toggle_tool_at_row(row)
                {
                    return true;
                }
            }
            // Release copies at once and confirms with a header toast that
            // fades on its own; the transcript stays clean. The highlight
            // clears with the copy. An empty cover copies nothing.
            let text = match selection_text(&front.state) {
                Some(text) if !text.is_empty() => text,
                _ => return false,
            };
            yank_to_clipboard(&text);
            front.state.clear_text_selection();
            front
                .state
                .set_toast(format!("copied {} chars", text.chars().count()), TOAST_TTL);
            true
        }
        _ => false,
    }
}

/// Outcome of one bounded keyboard batch.
struct KeyBatch {
    handled: usize,
    quit: bool,
}

/// Handles at most `limit` input events, so a paste, autorepeat burst, or
/// mouse wheel flurry cannot starve event application or redraws; remaining
/// input is picked up on later ticks. Returns true when the TUI should exit.
async fn handle_key_batch(
    front: &mut Frontend,
    runtime: &Runtime,
    mut next: impl FnMut() -> io::Result<Option<Input>>,
    limit: usize,
) -> io::Result<KeyBatch> {
    let mut handled = 0;
    for _ in 0..limit {
        let Some(input) = next()? else { break };
        match input {
            Input::Mouse(mouse) => {
                if handle_mouse(front, mouse) {
                    handled += 1;
                }
            }
            Input::Key(key) => {
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
        }
    }
    Ok(KeyBatch {
        handled,
        quit: false,
    })
}

/// Executes a local desktop command. Returns true only when the loop must
/// exit (`:quit`); everything else answers inline or opens a dialog and keeps the session
/// alive. Model switches reuse the same admission-order selection as the
/// `m` key, so both paths always agree on what is active. Session commands
/// borrow the registry; every other command answers on the active front.
fn handle_slash(front: &mut Frontend, command: SlashCommand) -> bool {
    match command {
        SlashCommand::Help => {
            front.state.open_help();
            false
        }
        SlashCommand::Model(None) => {
            // Bare `:model` opens the searchable picker; the exact-name
            // form below stays for direct switches.
            front.open_model_picker();
            false
        }
        SlashCommand::Model(Some(name)) => {
            front.select_model(&name);
            false
        }
        SlashCommand::Usage => {
            match front.state.last_usage() {
                Some(usage) => front.state.notice(&format!(
                    "tokens in:{} out:{}",
                    usage
                        .input_tokens()
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "?".to_owned()),
                    usage
                        .output_tokens()
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "?".to_owned()),
                )),
                None => front.state.notice("no usage observed yet"),
            }
            false
        }
        SlashCommand::Quit => true,
        SlashCommand::Unknown(word) => {
            front
                .state
                .notice(&format!("unknown command :{word} — type :help"));
            false
        }
        SlashCommand::Session(args) => {
            // Registry-owned work (create/switch/list) cannot run on a bare
            // front: queue the intent and let the loop execute it against
            // the registry after the batch. Usage answers inline.
            if args == SessionArgs::Usage {
                front.state.notice(SessionArgs::usage_text());
            } else {
                front.pending_session_cmds.push(args);
            }
            false
        }
    }
}

/// Maximum live conversation slots in one TUI process. Each slot owns a
/// runtime plus bounded presentation state; the cap keeps slot creation
/// from growing either without bound.
const MAX_SESSIONS: usize = 16;

/// Executes a `:session` command against the registry. Returns true only
/// when the loop must exit (never for session commands: they always answer
/// inline and keep every session alive).
fn handle_session_command(sessions: &mut SessionRegistry, args: &SessionArgs) -> bool {
    match args {
        SessionArgs::Show => {
            let line = {
                let active = sessions.active();
                format!(
                    "session {} ({}) — {} ({} of {})",
                    active.label,
                    active.front.session.as_str(),
                    active.front.state.status(),
                    sessions.active + 1,
                    sessions.len(),
                )
            };
            sessions.active_mut().front.state.notice(&line);
            false
        }
        SessionArgs::New => {
            if sessions.len() >= MAX_SESSIONS {
                sessions
                    .active_mut()
                    .front
                    .state
                    .notice("session limit reached (16 live sessions)");
                return false;
            }
            match sessions.create() {
                Ok(_) => false,
                Err(error) => {
                    let label = sessions.active().label.clone();
                    sessions.active_mut().front.state.notice(&format!(
                        "session creation failed ({error}); still on {label}"
                    ));
                    false
                }
            }
        }
        SessionArgs::List => {
            for line in sessions.describe() {
                sessions.active_mut().front.state.notice(&line);
            }
            false
        }
        SessionArgs::Switch(target) => match sessions.resolve_target(target) {
            None => {
                sessions
                    .active_mut()
                    .front
                    .state
                    .notice(&format!("unknown session {target} — type :session list"));
                false
            }
            Some(index) => {
                if sessions.switch(index) {
                    let label = sessions.active().label.clone();
                    let position = sessions.active + 1;
                    let total = sessions.len();
                    sessions.active_mut().front.state.notice(&format!(
                        "switched to session {label} ({position} of {total})"
                    ));
                } else {
                    let label = sessions.active().label.clone();
                    sessions
                        .active_mut()
                        .front
                        .state
                        .notice(&format!("already on session {label}"));
                }
                false
            }
        },
        SessionArgs::Usage => {
            sessions
                .active_mut()
                .front
                .state
                .notice(SessionArgs::usage_text());
            false
        }
    }
}
/// Applies a submit reply to the frontend. Only `Accepted` adopts the run
/// identity and records the draft; `Busy`/rejections keep the draft and
/// report the reply. Returns true when the draft was accepted.
fn settle_submit(
    front: &mut Frontend,
    draft: &str,
    reply: &CommandResponse,
    owns_pending_selection: bool,
) -> bool {
    if !front.adopt_accepted(reply) {
        front.clear_unaccepted_selection(owns_pending_selection);
        front.state.notice(&format!(
            "submit not accepted ({:?}); draft preserved",
            reply.reply()
        ));
        return false;
    }
    if owns_pending_selection && let Some(run) = reply.run() {
        front.bind_pending_selection(run);
    }
    if let Some(run) = reply.run()
        && let Some(model) = front.active_model.clone()
    {
        front.run_models.insert(run.clone(), model);
    }
    front.state.record_submitted(draft);
    true
}

/// Routes one key event to the open modal dialog. Every key is consumed
/// by the dialog, so composer, viewport, and approval shortcuts cannot
/// fire behind it. `Esc` dismisses without applying. (`Ctrl+C` never
/// reaches here: the caller falls through to the normal cancel path so
/// cancellation stays responsive while a dialog is open.)
fn handle_overlay_key(front: &mut Frontend, key: KeyEvent) -> bool {
    use crossterm::event::KeyCode;
    if key.kind != KeyEventKind::Press {
        return false;
    }
    if !key.modifiers.is_empty() {
        return false;
    }
    if front.state.help_open() {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => front.state.dismiss_overlay(),
            KeyCode::Up | KeyCode::Char('k') => front.state.scroll_help(-1),
            KeyCode::Down | KeyCode::Char('j') => front.state.scroll_help(1),
            KeyCode::PageUp => front.state.scroll_help(-10),
            KeyCode::PageDown => front.state.scroll_help(10),
            _ => {}
        }
        return false;
    }
    match key.code {
        KeyCode::Esc => front.state.dismiss_overlay(),
        KeyCode::Enter => confirm_model_picker(front),
        KeyCode::Up => move_picker(front, -1),
        KeyCode::Down => move_picker(front, 1),
        KeyCode::PageUp => move_picker(front, -(MAX_PICKER_VISIBLE_ROWS as isize)),
        KeyCode::PageDown => move_picker(front, MAX_PICKER_VISIBLE_ROWS as isize),
        KeyCode::Home => {
            let count = front.state.picker_matches().len();
            front.state.picker_move(isize::MIN, count);
        }
        KeyCode::End => {
            let count = front.state.picker_matches().len();
            front.state.picker_move(isize::MAX, count);
        }
        KeyCode::Backspace => front.state.picker_backspace(),
        KeyCode::Char(char) => front.state.picker_push(char),
        _ => {}
    }
    false
}

/// Moves the picker cursor; the match count is re-derived so navigation
/// always agrees with what rendering shows.
fn move_picker(front: &mut Frontend, delta: isize) {
    let count = front.state.picker_matches().len();
    front.state.picker_move(delta, count);
}

/// Confirms the highlighted picker row: maps the cursor through the live
/// filtered matches onto the snapshot id, then runs the same exact-id
/// selection as `:model set <name>`. An empty match list just dismisses.
fn confirm_model_picker(front: &mut Frontend) {
    let matches = front.state.picker_matches();
    let id = front
        .state
        .picker_confirm(matches.len())
        .and_then(|cursor| matches.get(cursor))
        .and_then(|index| front.state.model_choices().get(*index))
        .map(|choice| choice.id.clone());
    front.state.dismiss_overlay();
    if let Some(id) = id {
        front.select_model(&id);
    }
}

/// Whether the key is a `Ctrl+C` press: the one key that bypasses an open
/// dialog so cancellation never waits behind it.
fn is_overlay_cancel(key: KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
        && key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
        && matches!(key.code, crossterm::event::KeyCode::Char('c'))
}

/// Viewport-only Vim navigation. `None` leaves the key to the ordinary
/// focus map; `Some` means it was consumed.
fn handle_vim_viewport_key(front: &mut Frontend, key: KeyEvent) -> Option<bool> {
    if front.focus != Focus::Viewport || key.kind != KeyEventKind::Press {
        front.g_pending = false;
        return None;
    }
    let height = front.state.viewport_height().max(1);
    if key
        .modifiers
        .contains(crossterm::event::KeyModifiers::CONTROL)
    {
        front.g_pending = false;
        return match key.code {
            crossterm::event::KeyCode::Char('f') => {
                front.state.scroll_down(height);
                Some(false)
            }
            crossterm::event::KeyCode::Char('b') => {
                front.state.scroll_up(height);
                Some(false)
            }
            crossterm::event::KeyCode::Char('d') => {
                front.state.scroll_down((height / 2).max(1));
                Some(false)
            }
            crossterm::event::KeyCode::Char('u') => {
                front.state.scroll_up((height / 2).max(1));
                Some(false)
            }
            _ => None,
        };
    }
    let shift_only = key.modifiers == crossterm::event::KeyModifiers::SHIFT;
    if !key.modifiers.is_empty() && !shift_only {
        front.g_pending = false;
        return None;
    }
    let code = key.code;
    if front.g_pending {
        front.g_pending = false;
        if code == crossterm::event::KeyCode::Char('g') {
            front
                .state
                .scroll_landmark(nexus_tui::ViewportLandmark::Top);
            return Some(false);
        }
    }
    match code {
        crossterm::event::KeyCode::Char('g') => {
            front.g_pending = true;
            Some(false)
        }
        crossterm::event::KeyCode::Char('G') => {
            front
                .state
                .scroll_landmark(nexus_tui::ViewportLandmark::Bottom);
            Some(false)
        }
        crossterm::event::KeyCode::Char('H') => {
            front
                .state
                .scroll_landmark(nexus_tui::ViewportLandmark::Top);
            Some(false)
        }
        crossterm::event::KeyCode::Char('M') => {
            front
                .state
                .scroll_landmark(nexus_tui::ViewportLandmark::Middle);
            Some(false)
        }
        crossterm::event::KeyCode::Char('L') => {
            front
                .state
                .scroll_landmark(nexus_tui::ViewportLandmark::Bottom);
            Some(false)
        }
        _ => None,
    }
}

/// Handles one key event. Returns true when the TUI should exit.
async fn handle_key(front: &mut Frontend, runtime: &Runtime, key: KeyEvent) -> bool {
    if !front.state.colon_pending()
        && !front.state.overlay_open()
        && let Some(quit) = handle_vim_viewport_key(front, key)
    {
        return quit;
    }
    // The viewport command line owns input until Enter executes or Esc
    // dismisses it. No command has side effects while being typed.
    if front.state.colon_pending() {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        if key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
            && key.code == crossterm::event::KeyCode::Char('c')
        {
            front.state.clear_colon();
            return false;
        }
        if key.code == crossterm::event::KeyCode::Esc && !front.state.completions().is_empty() {
            front.state.clear_completions();
            return false;
        }
        if !key.modifiers.is_empty() {
            return false;
        }
        match key.code {
            crossterm::event::KeyCode::Esc => front.state.clear_colon(),
            crossterm::event::KeyCode::Backspace => {
                front.state.command_line_backspace();
                front.refresh_command_completions();
            }
            crossterm::event::KeyCode::Char(char) => {
                front.state.command_line_push(char);
                front.refresh_command_completions();
            }
            crossterm::event::KeyCode::Up => front.state.move_completion(-1),
            crossterm::event::KeyCode::Down => front.state.move_completion(1),
            crossterm::event::KeyCode::Tab => front.accept_completion(),
            crossterm::event::KeyCode::Enter => {
                if !front.state.completions().is_empty() {
                    let command_is_complete =
                        front.state.selected_completion().is_some_and(|candidate| {
                            Some(candidate.insert.as_str()) == front.state.command_line()
                        });
                    if !command_is_complete {
                        front.accept_completion();
                        return false;
                    }
                }
                let command = front.state.take_command_line().unwrap_or_default();
                let input = format!(":{command}");
                if let Some(command) = SlashCommand::parse(&input) {
                    return handle_slash(front, command);
                }
            }
            _ => {}
        }
        return false;
    }
    if !front.state.completions().is_empty() {
        match key.code {
            crossterm::event::KeyCode::Up if front.focus == Focus::Composer => {
                front.state.move_completion(-1);
                return false;
            }
            crossterm::event::KeyCode::Down if front.focus == Focus::Composer => {
                front.state.move_completion(1);
                return false;
            }
            crossterm::event::KeyCode::Tab if front.focus == Focus::Composer => {
                front.accept_completion();
                return false;
            }
            crossterm::event::KeyCode::Enter if front.focus == Focus::Composer => {
                front.accept_completion();
                return false;
            }
            crossterm::event::KeyCode::Esc => {
                front.state.clear_completions();
                return false;
            }
            _ => {}
        }
    }
    // A modal dialog owns the keyboard: nothing falls through to the
    // composer, viewport, or approval shortcuts while it is open, except
    // `Ctrl+C`, which dismisses the dialog and continues into the normal
    // cancel path.
    if front.state.overlay_open() && !is_overlay_cancel(key) {
        return handle_overlay_key(front, key);
    }
    if is_overlay_cancel(key) {
        front.state.dismiss_overlay();
        front.state.clear_colon();
    }
    let directory_approval = front.focus == Focus::ApprovalCard
        && key.kind == KeyEventKind::Press
        && key.modifiers.is_empty()
        && key.code == crossterm::event::KeyCode::Char('s');
    let action = if directory_approval {
        Some(Action::ApproveOnce)
    } else {
        map_key(front.focus, key)
    };
    let Some(action) = action else {
        return false;
    };
    match action {
        Action::Submit => {
            if front.state.composer().trim().is_empty() {
                return false;
            }
            let request = front.next_request();
            let read_only = front.state.mode().read_only;
            match submit_command(
                request,
                front.session.clone(),
                front.state.composer(),
                DEMO_PROFILE,
                read_only,
            ) {
                Ok(command) => {
                    let draft = front.state.composer().to_owned();
                    front.clear_finalized_pending_selection(runtime).await;
                    let owns_pending_selection = front.prepare_run_selection();
                    let response = runtime.handle(command).await.0;
                    if settle_submit(front, &draft, &response, owns_pending_selection) {
                        let stashed = front.state.composer_take();
                        debug_assert_eq!(stashed, draft);
                    }
                }
                Err(_) => front.state.notice("submit rejected: input invalid"),
            }
        }
        Action::Newline => {
            front.state.composer_newline();
            front.state.clear_completions();
        }
        Action::Type(char) => {
            front.state.composer_type(char);
            front.refresh_file_completions();
        }
        Action::Backspace => {
            front.state.composer_backspace();
            front.refresh_file_completions();
        }
        Action::CaretLeft => {
            front.state.caret_left();
            front.refresh_file_completions();
        }
        Action::CaretRight => {
            front.state.caret_right();
            front.refresh_file_completions();
        }
        Action::FocusSwitch => {
            front.state.clear_colon();
            let pending = front.state.pending_approval().is_some();
            front.focus = next_focus(front.focus, pending);
        }
        Action::CycleMode => {
            // Composer-only binding: the mode gates the next submit. No
            // transcript entry is added; the composer border and title
            // already report the switch where the user is looking.
            front.state.cycle_mode();
        }
        Action::ParkFocus => {
            front.state.clear_text_selection();
            front.state.clear_colon();
            // Step back one rung, never cancelling or deciding: an open
            // detail just closes (the card keeps focus so allow/deny stay
            // one keypress away), the composer parks in the viewport for
            // the viewport-only shortcuts, and anywhere else returns to
            // the composer, where most work happens.
            if front.state.approval_detail_open() {
                front.state.close_approval_detail();
            } else {
                front.focus = match front.focus {
                    Focus::Composer => Focus::Viewport,
                    _ => Focus::Composer,
                };
            }
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
            } else if front.focus == Focus::Composer {
                // Previous submitted input, shown right in the composer so
                // it is always visible. Free scrolling belongs to the mouse
                // wheel and the Viewport paging keys.
                front.state.recall_prev();
            } else {
                // Previous message, selection-anchored.
                front.state.move_selection(-1);
            }
        }
        Action::ScrollDown => {
            if front.state.approval_detail_open() {
                front.state.approval_detail_scroll(1);
            } else if front.focus == Focus::Composer {
                // Next submitted input (or back to the live draft).
                front.state.recall_next();
            } else {
                // Next message (or back to the live tail).
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
        Action::Insert => {
            front.state.clear_colon();
            front.focus = Focus::Composer;
        }
        Action::OpenLine => {
            front.state.clear_colon();
            front.focus = Focus::Composer;
            front.state.composer_newline();
        }
        Action::Colon => {
            front.state.arm_colon();
            front.refresh_command_completions();
        }
        Action::CycleVariant => {
            // Display-only: the title names the new variant at once.
            front.state.cycle_variant();
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
            if directory_approval && notice.session_directory.is_none() {
                front
                    .state
                    .notice("no session directory grant is offered for this approval");
                return false;
            }
            let request = front.next_request();
            let command = if action == Action::ApproveOnce {
                if directory_approval {
                    nexus_tui::decisions::approve_session_directory_command(request, &run, &notice)
                } else {
                    approve_notice_command(request, &run, &notice)
                }
            } else {
                deny_notice_command(request, &run, &notice)
            };
            let reply = runtime.handle(command).await.0;
            front
                .state
                .notice(&format!("decision reply: {:?}", reply.reply()));
            if matches!(
                reply.reply(),
                nexus_core::CommandReply::Rejected | nexus_core::CommandReply::Busy
            ) {
                return false;
            }
            front.state.resolve_approval();
            front.live_approval = None;
            // The card is gone: hand the keyboard back to the composer so
            // the next task can be typed without a Tab round-trip.
            front.focus = Focus::Composer;
        }
        Action::Cancel => {
            // A live selection copies first, silently: the highlight
            // clears as the only feedback, and the draft below survives
            // untouched. An empty cover (a bare click) is dropped so the
            // press falls through instead of swallowing it.
            match selection_text(&front.state) {
                Some(text) if !text.is_empty() => {
                    yank_to_clipboard(&text);
                    front.state.clear_text_selection();
                    front
                        .state
                        .set_toast(format!("copied {} chars", text.chars().count()), TOAST_TTL);
                    return false;
                }
                _ => front.state.clear_text_selection(),
            }
            // A non-empty draft goes next: the press clears it (and leaves
            // recall mode) instead of cancelling or quitting. The next press
            // then cancels a live run, or quits when there is nothing to
            // cancel, so quitting always takes two deliberate presses.
            if !front.state.composer().is_empty() {
                front.state.composer_take();
                return false;
            }
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
/// error, cancelling live work first. Conversations live in a session
/// registry: the first slot serves the canned submission, further slots
/// arrive through `:session new`, and only the active slot is polled.
async fn interactive(session_config: SessionConfig) -> io::Result<InteractiveReport> {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).map_err(io::Error::other)?;
    let (runtime, streams, live) = build_live_runtime(&session_config)?;
    let session =
        SessionId::new(DEMO_SESSION).map_err(|_| io::Error::other("demo session id rejected"))?;
    let mut front = Frontend::with_config(session, session_config.clone());
    front.state.set_session_label("s1");
    front.attach_live(live, session_config.tools_root.as_deref());
    let demo = session_config.demo;
    let mut sessions = SessionRegistry::new(
        SessionSlot {
            label: "s1".to_owned(),
            runtime,
            streams,
            front,
        },
        session_config,
    );
    let mut gate = RefreshGate::m0_test();
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + TICK, TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Canned submission through the same command path as typed input. The
    // draft is recorded and the run adopted only after `Accepted`. Doubly
    // gated: explicit `--demo` (never silent fakes in the dev version), and
    // not-live (a live adapter would fire a real billable network call on
    // every launch for a transcript nobody asked for).
    if demo && !sessions.active().front.is_live() {
        let slot = sessions.active_mut();
        let submit = submit_command(
            slot.front.next_request(),
            slot.front.session.clone(),
            DEMO_INPUT,
            DEMO_PROFILE,
            false,
        )
        .map_err(io::Error::other)?;
        let owns_pending_selection = slot.front.prepare_run_selection();
        let reply = slot.runtime.handle(submit).await.0;
        if settle_submit(&mut slot.front, DEMO_INPUT, &reply, owns_pending_selection) {
            slot.front.state.notice("canned M0 submission accepted");
        }
    }
    gate.request();

    let result = interactive_loop(&mut sessions, &mut gate, &mut terminal, &mut interval).await;

    // Quit, terminal, and error exits all pass through here: every slot
    // with no terminal outcome cancels and reconciles within a bounded
    // window before the terminal is restored.
    let mut unresolved_blocking = false;
    for slot in &mut sessions.slots {
        if !slot.front.merger.is_finalized() {
            unresolved_blocking |=
                reconcile_after_stop(&slot.runtime, &mut slot.streams, &mut slot.front, "exit")
                    .await;
        }
    }
    Ok(InteractiveReport {
        result,
        unresolved_blocking,
    })
}

async fn interactive_loop(
    sessions: &mut SessionRegistry,
    gate: &mut RefreshGate,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    interval: &mut tokio::time::Interval,
) -> io::Result<()> {
    let mut event_since_tick = false;
    loop {
        let mut dirty = false;
        // Only the active slot is polled and drawn; background slots keep
        // buffering in their bounded channels until switched back to. The
        // borrow ends before the step is handled so a session switch inside
        // key handling can move the active index freely.
        let step = {
            let slot = sessions.active_mut();
            next_loop_step(interval, &mut slot.streams.data, &mut slot.streams.control).await
        };
        match step {
            // A closed channel ends input, but the other one may still hold
            // queued events: apply everything already committed before exit
            // so a close racing the terminal cannot drop the outcome.
            LoopStep::Closed => {
                let slot = sessions.active_mut();
                let drained = drain_available(
                    &mut slot.streams.data,
                    &mut slot.streams.control,
                    &mut slot.front.merger,
                );
                slot.front.apply_events(drained);
                let flushed = slot.front.merger.flush();
                slot.front.apply_events(flushed);
                slot.front.report_merger();
                draw(terminal, &mut slot.front)?;
                return Ok(());
            }
            LoopStep::Event(event) => {
                event_since_tick = true;
                // A terminal outcome stays on screen and the loop keeps
                // serving: the next `Accepted` submit adopts a new run.
                // Only quit, close, or an I/O error ends the loop. Stream
                // updates render through the gate at the end of the
                // iteration: drawing here as well would paint terminal
                // outcomes twice in one pass under output load.
                let slot = sessions.active_mut();
                absorb(&mut slot.front, *event, &mut slot.streams);
                dirty = true;
            }
            LoopStep::Tick => {
                let batch = {
                    let slot = sessions.active_mut();
                    handle_key_batch(
                        &mut slot.front,
                        &slot.runtime,
                        read_input,
                        MAX_KEYS_PER_TICK,
                    )
                    .await?
                };
                if batch.quit {
                    return Ok(());
                }
                dirty |= batch.handled > 0;
                // A just-expired copy toast needs one last frame to vanish;
                // without input the loop would otherwise hold it forever.
                dirty |= sessions.active_mut().front.state.poll_toast_expired();
                dirty |= sessions.active_mut().front.state.tick_tool_animation();
                dirty |= sessions.active_mut().front.state.tick_caret();
                // Drain queued session intents against the registry in
                // recording order: creation moves the active slot, so later
                // intents resolve after earlier ones.
                let cmds = sessions.active_mut().front.take_session_cmds();
                for cmd in &cmds {
                    handle_session_command(sessions, cmd);
                    dirty = true;
                }
                let slot = sessions.active_mut();
                if !event_since_tick && slot.front.merger.has_pending() {
                    let flushed = slot.front.merger.flush();
                    slot.front.report_merger();
                    if !flushed.is_empty() {
                        // As above: leave the actual frame to the gate so a
                        // terminal outcome behind a reorder gap is painted
                        // once, not twice.
                        slot.front.apply_events(flushed);
                        dirty = true;
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
            draw(terminal, &mut sessions.active_mut().front)?;
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
/// transcript with no escape codes. Deliberately demo-only wiring: this
/// path fires without a user present, so it must never resolve a live
/// adapter or touch the network. Callers pass [`build_runtime`], never
/// [`build_live_runtime`].
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
        false,
    )
    .map_err(io::Error::other)?;
    let owns_pending_selection = front.prepare_run_selection();
    let reply = runtime.handle(submit).await.0;
    settle_submit(&mut front, DEMO_INPUT, &reply, owns_pending_selection);

    let timed_out = tokio::time::timeout(HEADLESS_TIMEOUT, async {
        let mut event_since_tick = false;
        loop {
            match next_loop_step(&mut interval, &mut streams.data, &mut streams.control).await {
                LoopStep::Closed => break,
                LoopStep::Event(event) => {
                    event_since_tick = true;
                    let pushed = front.merger.push(*event);
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
            front.record_model_use(event.run());
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
                Ok(Some(Input::Key(key)))
            },
            MAX_KEYS_PER_TICK,
        )
        .await
        .expect("key source never fails");
        assert_eq!(batch.handled, MAX_KEYS_PER_TICK);
        assert!(!batch.quit);
        assert_eq!(index, MAX_KEYS_PER_TICK, "the batch stops at the bound");

        // The vim `:q` sequence ends the batch at once.
        front.focus = Focus::Viewport;
        let quit_keys = [
            KeyEvent::new(KeyCode::Char(':'), KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            KeyEvent::new(KeyCode::F(1), KeyModifiers::empty()),
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
                Ok(Some(Input::Key(key)))
            },
            MAX_KEYS_PER_TICK,
        )
        .await
        .expect("key source never fails");
        assert!(batch.quit);
        assert_eq!(batch.handled, 3);
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
        assert!(!settle_submit(&mut front, "hi", &busy, false));
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
        assert!(settle_submit(&mut front, "hi", &accepted, false));
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
            false,
        )
        .expect("valid submit builds");
        let reply = runtime.handle(submit).await.0;
        assert!(settle_submit(&mut front, DEMO_INPUT, &reply, false));

        let mut interval = interval();
        let deadline = Instant::now() + Duration::from_secs(5);
        while front.live_approval.is_none() && Instant::now() < deadline {
            match next_loop_step(&mut interval, &mut streams.data, &mut streams.control).await {
                LoopStep::Closed => break,
                LoopStep::Event(event) => {
                    let pushed = front.merger.push(*event);
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
            false,
        )
        .expect("valid submit builds");
        let reply = runtime.handle(submit).await.0;
        assert!(settle_submit(&mut front, DEMO_INPUT, &reply, false));
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
            false,
        )
        .expect("valid submit builds");
        let reply = runtime.handle(command).await.0;
        assert_eq!(reply.reply(), CommandReply::Accepted);
        assert!(settle_submit(&mut front, "hi", &reply, false));
        assert_eq!(front.merger.active_run(), reply.run());
        assert_eq!(user_line_count(&front), 1, "the accepted draft is recorded");

        let stashed = front.state.composer_take();
        assert_eq!(stashed, "hi", "the recorded draft is the cleared draft");

        // An `Accepted` reply carrying no run identity records the draft but
        // adopts nothing, so no event can be attributed to it.
        front.state.composer_type('?');
        let orphan = CommandResponse::new(front.next_request(), CommandReply::Accepted, None);
        assert!(settle_submit(&mut front, "?", &orphan, false));
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
        assert!(!settle_submit(&mut front, &draft, &busy, false));
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
        assert!(!settle_submit(&mut front, &draft, &rejected, false));
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
        assert!(settle_submit(&mut front, &retried, &accepted, false));
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

        // The vim `:q` sequence leaves the loop from the viewport.
        front.focus = Focus::Viewport;
        assert!(
            !handle_key(&mut front, &runtime, press(KeyCode::Char(':'))).await,
            "arming consumes the colon"
        );
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('q'))).await);
        assert!(handle_key(&mut front, &runtime, press(KeyCode::Enter)).await);

        // `Ctrl+C` with a non-empty draft clears it first instead of
        // cancelling or quitting; the next press exits when idle.
        assert!(
            !handle_key(
                &mut front,
                &runtime,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            )
            .await
        );
        assert_eq!(front.state.composer(), "", "the draft is cleared first");
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
                Ok(Some(Input::Key(press(KeyCode::F(1)))))
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
                Ok(Some(Input::Key(press(KeyCode::F(1)))))
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
                let key = kinds.get(index).copied().map(Input::Key);
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
        front.focus = Focus::Viewport;
        let burst = [
            press(KeyCode::Char(':')),
            press(KeyCode::Char('q')),
            press(KeyCode::Enter),
            press(KeyCode::F(1)),
        ];
        let mut index = 0;
        let batch = handle_key_batch(
            &mut front,
            &runtime,
            || {
                let key = burst.get(index).copied().map(Input::Key);
                index += 1;
                Ok(key)
            },
            8,
        )
        .await
        .expect("key source never fails");
        assert!(batch.quit, "the loop exits on the quit key");
        assert_eq!(batch.handled, 3);
        assert_eq!(index, 3, "no key after the quit is handled");

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
            false,
        )
        .expect("valid submit builds");
        let reply = runtime.handle(submit).await.0;
        assert!(settle_submit(&mut front, DEMO_INPUT, &reply, false));
        assert!(front.merger.active_run().is_some());

        // The user quits before any event has been merged.
        front.focus = Focus::Viewport;
        let quit_keys = [
            KeyEvent::new(KeyCode::Char(':'), KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        ];
        let mut index = 0;
        let batch = handle_key_batch(
            &mut front,
            &runtime,
            || {
                let key = quit_keys.get(index).copied().map(Input::Key);
                index += 1;
                Ok(key)
            },
            MAX_KEYS_PER_TICK,
        )
        .await
        .expect("key source never fails");
        assert!(batch.quit);
        assert_eq!(batch.handled, 3);
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
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    use nexus_core::{
        ApprovalId, AssistantText, CallId, PersistenceState, RunFinished, RunOutcome, TurnId,
    };
    use nexus_tui::state::ApprovalGeometry;
    use ratatui::layout::Rect;
    use ratatui::{TerminalOptions, Viewport};

    #[test]
    fn interactive_runtime_raises_only_the_model_turn_budget_to_64() {
        let limits = tui_runtime_limits();
        assert_eq!(limits.max_model_turns_per_run, 64);
        assert_eq!(
            limits.max_tool_calls_per_run,
            Limits::M0_TEST_TOOL_CALLS_PER_RUN
        );
        assert_eq!(
            limits.run_duration,
            Duration::from_secs(Limits::M0_TEST_RUN_DURATION_SECS)
        );
        limits
            .validate()
            .expect("interactive limits remain finite and valid");
    }

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
            tools_root: None,
            demo: false,
            strict_tools: true,
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
            tools_root: None,
            demo: false,
            strict_tools: true,
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

    /// One registry slot over the demo wiring, labelled for the header.
    fn slot(label: &str, session: &str, template: &SessionConfig) -> SessionSlot {
        let (runtime, streams) = build_runtime().expect("demo wiring builds");
        let mut front = Frontend::with_config(
            SessionId::new(session).expect("test session id is valid"),
            template.clone(),
        );
        front.state.set_session_label(label);
        SessionSlot {
            label: label.to_owned(),
            runtime,
            streams,
            front,
        }
    }

    /// Wraps one front+runtime+streams triple into a single-slot registry so
    /// loop-level tests exercise the production entry point.
    fn registry_with(
        label: &str,
        front: Frontend,
        runtime: Runtime,
        streams: EventStreams,
    ) -> SessionRegistry {
        SessionRegistry {
            slots: vec![SessionSlot {
                label: label.to_owned(),
                runtime,
                streams,
                front,
            }],
            active: 0,
            template: defaults_config(),
        }
    }

    /// A one-slot registry over the empty startup document.
    fn registry() -> SessionRegistry {
        let template = defaults_config();
        let first = slot("s1", "sess-test-1", &template);
        SessionRegistry::new(first, template)
    }

    #[test]
    fn session_targets_resolve_by_index_label_or_exact_id() {
        let sessions = registry();
        assert_eq!(sessions.resolve_target("1"), Some(0));
        assert_eq!(sessions.resolve_target("s1"), Some(0));
        assert_eq!(sessions.resolve_target("sess-test-1"), Some(0));
        for unknown in ["", "0", "2", "9", "s2", "sess-test-2", "S1", "s 1"] {
            assert_eq!(
                sessions.resolve_target(unknown),
                None,
                "{unknown:?} resolves to nothing, never a guess"
            );
        }
    }

    #[test]
    fn session_switch_moves_only_to_live_slots() {
        let mut sessions = registry();
        assert!(!sessions.switch(0), "the active slot is not re-entered");
        assert!(!sessions.switch(7), "out-of-range indices change nothing");
        assert_eq!(sessions.active, 0);

        let template = defaults_config();
        sessions.slots.push(slot("s2", "sess-test-2", &template));
        assert_eq!(sessions.resolve_target("2"), Some(1));
        assert_eq!(sessions.resolve_target("s2"), Some(1));
        assert!(sessions.switch(1));
        assert_eq!(sessions.active, 1);
        assert!(!sessions.switch(1));
        assert!(sessions.switch(0));
        assert_eq!(sessions.active, 0);
    }

    #[test]
    fn session_create_labels_switches_and_announces() {
        let mut sessions = registry();
        let index = sessions.create().expect("creation succeeds on demo wiring");
        assert_eq!(index, 1);
        assert_eq!(sessions.active, 1);
        assert_eq!(sessions.len(), 2);
        let active = sessions.active();
        assert_eq!(active.label, "s2");
        assert_eq!(active.front.session.as_str(), format!("{DEMO_SESSION}-2"));
        assert_eq!(active.front.state.session_label(), Some("s2"));
        assert!(
            transcript(&active.front).contains("session s2 started"),
            "the new slot announces itself in its own history"
        );
    }

    #[test]
    fn session_commands_answer_inline_without_quitting() {
        let mut sessions = registry();
        assert!(!handle_session_command(&mut sessions, &SessionArgs::Show));
        assert!(
            transcript(&sessions.active().front).contains("session s1 (sess-test-1)"),
            "show names the active session and its runtime id"
        );

        assert!(!handle_session_command(&mut sessions, &SessionArgs::Usage));
        assert!(
            transcript(&sessions.active().front).contains(":session new"),
            "usage explains instead of guessing"
        );

        assert!(!handle_session_command(
            &mut sessions,
            &SessionArgs::Switch("nope".to_owned())
        ));
        assert!(
            transcript(&sessions.active().front).contains("unknown session nope"),
            "unknown targets name themselves and hint at the list"
        );

        assert!(!handle_session_command(
            &mut sessions,
            &SessionArgs::Switch("1".to_owned())
        ));
        assert!(
            transcript(&sessions.active().front).contains("already on session s1"),
            "re-selecting the active slot is a no-op notice"
        );

        assert!(!handle_session_command(&mut sessions, &SessionArgs::New));
        assert_eq!(sessions.active, 1);
        assert!(!handle_session_command(&mut sessions, &SessionArgs::List));
        let listed = transcript(&sessions.active().front);
        assert!(
            listed.contains("s1 (sess-test-1)") && listed.contains("[active]"),
            "the list marks the active slot: {listed:?}"
        );

        assert!(!handle_session_command(
            &mut sessions,
            &SessionArgs::Switch("s1".to_owned())
        ));
        assert_eq!(sessions.active, 0);
        assert!(
            transcript(&sessions.active().front).contains("switched to session s1"),
            "switching announces the arrival in the target history"
        );
    }

    #[test]
    fn session_creation_stops_at_the_live_slot_cap() {
        let mut sessions = registry();
        while sessions.len() < MAX_SESSIONS {
            sessions.create().expect("slots admit up to the cap");
        }
        assert_eq!(sessions.len(), MAX_SESSIONS);
        assert!(!handle_session_command(&mut sessions, &SessionArgs::New));
        assert_eq!(sessions.len(), MAX_SESSIONS, "the cap holds");
        assert!(
            transcript(&sessions.active().front).contains("session limit reached"),
            "the refusal is explicit, never a silent drop"
        );
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
    fn terminal_outcome_records_the_model_bound_to_the_run_and_saves_the_document() {
        let directory = temp_config_dir("terminal-save");
        let path = directory.join("config.json");
        let mut config = configured(3, &[], &[]);
        config.path = Some(path.clone());
        let (run, mut front) = frontend_with(config);
        front.run_models.insert(run.clone(), "m1".to_owned());
        front.active_model = Some("m2".to_owned());

        assert!(front.apply_events(vec![started(&run, 0), finished(&run, 1)]));
        assert_eq!(
            front.config.recent().first().map(String::as_str),
            Some("m1"),
            "later picker changes do not alter run attribution"
        );
        let persisted = load_config(&path)
            .expect("the document is readable")
            .expect("it exists");
        assert_eq!(
            persisted.recent().first().map(String::as_str),
            Some("m1"),
            "the usage was written back to the file the session loaded"
        );
        std::fs::remove_dir_all(&directory).expect("temporary directory removed");
    }

    #[test]
    fn prepared_provider_selection_survives_picker_changes_before_first_call() {
        let mut front = Frontend::with_config(session(), configured(2, &[], &[]));
        let binding = Arc::new(Mutex::new(LiveSelection {
            config: front.config.clone(),
            active_model: Some("m0".to_owned()),
            demo: false,
            pending_run: None,
            pending_run_id: None,
        }));
        front.attach_live(binding.clone(), None);

        front.prepare_run_selection();
        front.select_model("m1");
        assert!(
            !front.prepare_run_selection(),
            "a second submit cannot replace the pending run identity"
        );
        front.clear_unaccepted_selection(false);

        let mut binding = binding.lock().expect("selection lockable");
        let captured = binding.pending_run.take().expect("submit snapshot");
        assert_eq!(captured.active_model.as_deref(), Some("m0"));
        assert_eq!(binding.active_model.as_deref(), Some("m1"));
    }

    #[test]
    fn terminal_before_first_provider_call_clears_only_its_run_snapshot() {
        let mut front = Frontend::with_config(session(), configured(2, &[], &[]));
        let binding = Arc::new(Mutex::new(LiveSelection {
            config: front.config.clone(),
            active_model: Some("m0".to_owned()),
            demo: false,
            pending_run: None,
            pending_run_id: None,
        }));
        front.attach_live(binding.clone(), None);

        assert!(front.prepare_run_selection());
        let first = run_id("cancelled-before-provider");
        let accepted = CommandResponse::new(
            front.next_request(),
            CommandReply::Accepted,
            Some(first.clone()),
        );
        assert!(settle_submit(&mut front, "first", &accepted, true));
        assert!(
            binding
                .lock()
                .expect("selection lockable")
                .pending_run_id
                .as_ref()
                == Some(&first)
        );

        assert!(front.apply_events(vec![finished(&first, 0)]));
        let binding = binding.lock().expect("selection lockable");
        assert!(binding.pending_run.is_none());
        assert!(binding.pending_run_id.is_none());
    }

    #[test]
    fn provider_discards_snapshot_owned_by_a_different_run() {
        let mut selection = LiveSelection {
            config: configured(2, &[], &[]).config,
            active_model: Some("m1".to_owned()),
            demo: false,
            pending_run: None,
            pending_run_id: None,
        };
        let mut stale = selection.clone();
        stale.active_model = Some("m0".to_owned());
        selection.pending_run = Some(Box::new(stale));
        let first = run_id("first-run");
        let next = run_id("next-run");
        selection.pending_run_id = Some(first);
        let binding = Arc::new(Mutex::new(selection));

        let captured = take_selection_for_run(&binding, &next);

        assert_eq!(captured.active_model.as_deref(), Some("m1"));
        let binding = binding.lock().expect("selection lockable");
        assert!(binding.pending_run.is_none());
        assert!(binding.pending_run_id.is_none());
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
        front.run_models.insert(run.clone(), "m1".to_owned());

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

    /// Types a full draft one char at a time, like the key handler would.
    fn type_draft(front: &mut Frontend, text: &str) {
        front.focus = Focus::Composer;
        for char in text.chars() {
            front.state.composer_type(char);
        }
    }

    fn type_command(front: &mut Frontend, command: &str) {
        front.focus = Focus::Viewport;
        front.state.arm_colon();
        for char in command.chars() {
            front.state.command_line_push(char);
        }
    }

    #[tokio::test]
    async fn slash_prefix_is_prompt_text_not_a_desktop_command() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let mut front = Frontend::new(session());
        type_draft(&mut front, "/help");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Enter)).await);
        assert_eq!(
            front.request_counter, 1,
            "slash input is submitted as a task"
        );
        assert_eq!(front.state.composer(), "", "the accepted task is cleared");
        assert_eq!(user_line_count(&front), 1);
        assert!(
            !transcript(&front).contains("/model —"),
            "slash input was not routed to local help"
        );
    }

    #[tokio::test]
    async fn desktop_q_quit_requires_enter() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let mut front = Frontend::new(session());
        front.focus = Focus::Viewport;
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('q'))).await);
        type_command(&mut front, "q");
        assert!(handle_key(&mut front, &runtime, press(KeyCode::Enter)).await);
        assert_eq!(front.request_counter, 0, "quitting issues no command");
    }

    #[tokio::test]
    async fn desktop_model_commands_open_picker_and_set_exact_id() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let (_run, mut front) = frontend_with(configured(2, &[], &[]));
        assert_eq!(front.active_model.as_deref(), Some("m0"));

        type_command(&mut front, "m");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Enter)).await);
        assert_eq!(front.request_counter, 0, "opening the picker is local");
        assert!(front.state.overlay_open(), "bare :m opens the picker");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Enter)).await);
        assert!(!front.state.overlay_open(), "confirming closes the picker");
        assert!(
            transcript(&front).contains("model: m0"),
            "confirming the first row selects it"
        );

        type_command(&mut front, "model set m1");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Enter)).await);
        assert_eq!(front.active_model.as_deref(), Some("m1"));
        assert_eq!(front.state.active_model(), Some("m1"), "display follows");
        assert_eq!(front.request_counter, 0, "switching is local");

        type_command(&mut front, "model set ghost");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Enter)).await);
        assert_eq!(
            front.active_model.as_deref(),
            Some("m1"),
            "failed switch keeps"
        );
        assert!(
            transcript(&front).contains("unknown model"),
            "failures name the fix"
        );
        assert!(
            transcript(&front).contains("m0"),
            "failures list what exists"
        );
    }

    #[tokio::test]
    async fn model_picker_filters_navigates_confirms_and_cancels() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let (_run, mut front) = frontend_with(configured(3, &[], &[]));
        type_command(&mut front, "m");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Enter)).await);
        assert!(front.state.overlay_open());
        assert_eq!(front.state.picker_matches().len(), 3);

        // Narrow to one row, then confirm it.
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('m'))).await);
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('2'))).await);
        assert_eq!(front.state.picker_matches().len(), 1);
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Enter)).await);
        assert!(!front.state.overlay_open(), "confirm closes the picker");
        assert_eq!(front.active_model.as_deref(), Some("m2"));
        assert!(
            transcript(&front).contains("model: m2"),
            "confirmation runs the exact-id selection"
        );

        // Reopen and cancel: the selection is untouched.
        type_command(&mut front, "m");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Enter)).await);
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Down)).await);
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Esc)).await);
        assert!(!front.state.overlay_open(), "esc dismisses");
        assert_eq!(front.active_model.as_deref(), Some("m2"));
    }

    #[tokio::test]
    async fn model_picker_keys_never_reach_the_composer() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let (_run, mut front) = frontend_with(configured(2, &[], &[]));
        type_command(&mut front, "m");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Enter)).await);
        // `a`/`d` decide approvals; behind the picker they are filter text.
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('a'))).await);
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('d'))).await);
        assert_eq!(front.state.composer(), "", "the draft is untouched");
        assert_eq!(front.state.picker_filter(), "ad");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Esc)).await);
        assert_eq!(front.active_model.as_deref(), Some("m0"));
    }

    #[tokio::test]
    async fn approval_arrival_dismisses_the_model_picker() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let (run, mut front) = frontend_with(configured(2, &[], &[]));
        type_command(&mut front, "model");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Enter)).await);
        assert!(front.state.overlay_open());

        front.apply_events(vec![started(&run, 0), approval(&run, 1, "call-1")]);
        assert_eq!(front.focus, Focus::ApprovalCard);
        assert!(
            !front.state.overlay_open(),
            "the approval owns the keyboard, not the picker"
        );
    }

    #[tokio::test]
    async fn viewport_insert_keys_return_to_the_composer() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let (_run, mut front) = frontend_with(configured(2, &[], &[]));
        front.focus = Focus::Viewport;
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('i'))).await);
        assert_eq!(front.focus, Focus::Composer);
        assert_eq!(front.state.composer(), "", "insert types nothing");

        front.focus = Focus::Viewport;
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('o'))).await);
        assert_eq!(front.focus, Focus::Composer);
        assert_eq!(
            front.state.composer(),
            "\n",
            "open-line starts a fresh line"
        );
    }

    #[tokio::test]
    async fn desktop_commands_wait_for_enter_and_escape_cancels() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let (_run, mut front) = frontend_with(configured(2, &[], &[]));
        front.focus = Focus::Viewport;

        // Typing a command has no side effects until Enter.
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char(':'))).await);
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('m'))).await);
        assert!(!front.state.overlay_open());
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Enter)).await);
        assert!(front.state.overlay_open());
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Esc)).await);

        // Single q never quits; only the executed :q command does.
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('q'))).await);
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char(':'))).await);
        for char in "q".chars() {
            assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char(char))).await);
        }
        assert!(handle_key(&mut front, &runtime, press(KeyCode::Enter)).await);
    }

    #[tokio::test]
    async fn slash_usage_reports_observed_counters_or_their_absence() {
        use nexus_core::{RunId, SessionId, Usage, UsageFinality};
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let mut front = Frontend::new(session());
        type_command(&mut front, "usage");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Enter)).await);
        assert!(
            transcript(&front).contains("no usage observed yet"),
            "absence is explicit"
        );

        let run = RunId::new("run-usage").expect("valid");
        front.merger.adopt(&run);
        front.apply_events(vec![started(&run, 0)]);
        let usage = RunEvent::new(
            SessionId::new("sess-1").expect("valid"),
            run,
            1,
            EventPayload::UsageUpdated(Usage::new(Some(10), None, UsageFinality::Final)),
        );
        front.apply_events(vec![usage]);
        type_command(&mut front, "usage");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Enter)).await);
        assert!(
            transcript(&front).contains("in:10 out:?"),
            "unknown stays unknown, never zero"
        );
    }

    #[tokio::test]
    async fn slash_unknown_names_help() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let mut front = Frontend::new(session());
        type_command(&mut front, "frobnicate");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Enter)).await);
        assert_eq!(front.request_counter, 0, "unknown routes locally too");
        assert!(
            transcript(&front).contains("unknown command :frobnicate"),
            "the word is echoed for correction"
        );
        assert!(transcript(&front).contains(":help"));
    }

    #[tokio::test]
    async fn ctrl_t_cycles_the_model_variant_in_composer_and_viewport() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let mut front = Frontend::new(session());
        front.focus = Focus::Composer;
        assert_eq!(front.state.variant(), "");
        let ctrl_t = || KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(!handle_key(&mut front, &runtime, ctrl_t()).await);
        assert_eq!(front.state.variant(), "minimal");
        front.focus = Focus::Viewport;
        assert!(!handle_key(&mut front, &runtime, ctrl_t()).await);
        assert_eq!(front.state.variant(), "low");
        assert_eq!(front.request_counter, 0, "cycling issues no command");
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
        assert_eq!(
            front.focus,
            Focus::Composer,
            "Tab cycles the mode, not focus"
        );
        assert_eq!(
            front.state.mode().id,
            "plan",
            "build cycles to plan on the first Tab"
        );
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Tab)).await);
        assert_eq!(
            front.state.mode().id,
            "build",
            "the second Tab wraps back to build"
        );
        assert_eq!(front.focus, Focus::Composer, "mode cycling keeps focus");

        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Esc)).await);
        assert_eq!(front.focus, Focus::Viewport, "Esc parks in the viewport");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Esc)).await);
        assert_eq!(
            front.focus,
            Focus::Composer,
            "Esc in the viewport returns home to the composer"
        );
        assert!(!front.state.approval_detail_open(), "Esc never decides");
        assert_eq!(front.request_counter, 0, "no focus key issues a command");
    }

    #[tokio::test]
    async fn composer_pages_without_leaving_the_composer() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let mut front = Frontend::new(session());
        for index in 0..30 {
            front.state.record_submitted(&format!("entry {index}"));
        }
        front.state.set_viewport(40, 8);
        front.focus = Focus::Composer;

        assert!(!handle_key(&mut front, &runtime, press(KeyCode::PageUp)).await);
        assert_eq!(front.focus, Focus::Composer, "paging keeps typing focus");
        assert!(front.state.scrollback() > 0, "composer PgUp pages history");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::PageDown)).await);
        assert_eq!(
            front.state.scrollback(),
            0,
            "composer PgDn returns to the tail"
        );

        // Left/Right move the draft caret; folding is mouse-only, so
        // arrow keys can never disturb the draft or the selection.
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Left)).await);
        assert_eq!(front.state.composer_caret(), 0, "caret stays put on empty");
        assert_eq!(front.focus, Focus::Composer, "arrows keep typing focus");
        for char in ['a', 'b'] {
            assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char(char))).await);
        }
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Left)).await);
        assert_eq!(front.state.composer_caret(), 1);
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('X'))).await);
        assert_eq!(front.state.composer(), "aXb", "typing inserts at the caret");
        assert_eq!(
            front.request_counter, 0,
            "paging and arrows issue no command"
        );
    }

    #[tokio::test]
    async fn esc_steps_back_one_rung_from_the_approval_card() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let run = run_id("run-esc-rungs");
        let mut front = awaiting_approval(&run, "c1-0");
        front.focus = Focus::ApprovalCard;

        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('i'))).await);
        assert!(front.state.approval_detail_open(), "i opens the detail");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Esc)).await);
        assert!(!front.state.approval_detail_open(), "Esc closes the detail");
        assert_eq!(
            front.focus,
            Focus::ApprovalCard,
            "the card keeps focus so allow/deny stay one keypress away"
        );
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Esc)).await);
        assert_eq!(front.focus, Focus::Composer, "Esc then returns home");
        assert!(
            front.live_approval.is_some(),
            "stepping back decides nothing"
        );
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
    async fn viewport_arrows_never_fold_or_select() {
        // Keyboard folding is removed: Left/Right/h/l are inert everywhere,
        // and only a mouse click expands or collapses a tool card.
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let run = run_id("run-arrows-inert");
        let mut front = Frontend::new(session());
        front.merger.adopt(&run);
        front.apply_events(vec![started(&run, 0), text(&run, 1, "hello")]);
        front.focus = Focus::Viewport;
        assert!(front.state.selected().is_none());

        for key in [
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Char('h'),
            KeyCode::Char('l'),
        ] {
            assert!(!handle_key(&mut front, &runtime, press(key)).await);
        }
        assert!(front.state.selected().is_none(), "arrows select nothing");
        assert_eq!(front.request_counter, 0, "inert arrows issue no command");
        assert!(front.state.entry_count() > 0);
    }

    #[tokio::test]
    async fn composer_arrows_recall_submitted_inputs() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let mut front = Frontend::new(session());
        front.state.record_submitted("first task");
        front.state.record_submitted("second task");
        front.focus = Focus::Composer;
        assert_eq!(front.state.composer(), "");

        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Up)).await);
        assert_eq!(front.focus, Focus::Composer, "Up keeps typing focus");
        assert_eq!(front.state.composer(), "second task");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Up)).await);
        assert_eq!(front.state.composer(), "first task");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Up)).await);
        assert_eq!(
            front.state.composer(),
            "first task",
            "recall stops at the oldest"
        );
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Down)).await);
        assert_eq!(front.state.composer(), "second task");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Down)).await);
        assert_eq!(
            front.state.composer(),
            "",
            "Down past newest restores the draft"
        );
        assert_eq!(front.request_counter, 0, "recall issues no command");
    }

    #[test]
    fn base64_encode_matches_rfc4648_vectors() {
        for (raw, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("hello", "aGVsbG8="),
            ("\u{65e5}\u{672c}", "5pel5pys"),
        ] {
            assert_eq!(base64_encode(raw.as_bytes()), encoded, "{raw:?}");
        }
    }

    #[tokio::test]
    async fn ctrl_c_copies_a_live_selection_silently_before_the_draft() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let mut front = Frontend::new(session());
        type_draft(&mut front, "hello world");
        let mut terminal = fixed_terminal();
        draw(&mut terminal, &mut front).expect("frame draws");
        let geo = front.state.pointer_geometry();
        let row = geo.composer_y + 1;
        let at = |kind, column| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        };
        assert!(handle_mouse(
            &mut front,
            at(MouseEventKind::Down(MouseButton::Left), geo.composer_x + 1)
        ));
        assert!(handle_mouse(
            &mut front,
            at(MouseEventKind::Drag(MouseButton::Left), geo.composer_x + 6)
        ));
        assert_eq!(
            front.state.composer_selection_text().as_deref(),
            Some("hello")
        );
        // The first Ctrl+C copies the live selection silently: the
        // draft survives and the loop stays up.
        assert!(!handle_key(&mut front, &runtime, ctrl('c')).await);
        assert_eq!(front.state.text_selection(), None, "highlight cleared");
        assert_eq!(
            front.state.toast(),
            Some("copied 5 chars"),
            "the copy confirms with a toast"
        );
        assert_eq!(front.state.composer(), "hello world", "draft untouched");
        assert!(
            !transcript(&front).contains("copied"),
            "the copy reports nothing"
        );
        // The next Ctrl+C resumes the documented order: draft first.
        assert!(!handle_key(&mut front, &runtime, ctrl('c')).await);
        assert_eq!(front.state.composer(), "", "draft cleared second");
    }

    #[test]
    fn clipboard_helper_prefers_platform_tools_in_order() {
        // Nothing installed: no helper, OSC 52 fallback applies.
        assert_eq!(clipboard_command(&|_| false), None);
        #[cfg(target_os = "macos")]
        {
            let probe = |name: &str| name == "pbcopy";
            assert_eq!(clipboard_command(&probe), Some(("pbcopy", &[] as &[&str])));
        }
        #[cfg(target_os = "linux")]
        {
            let none = |_: &str| false;
            assert_eq!(clipboard_command(&none), None);
            let xclip = |name: &str| name == "xclip";
            assert_eq!(
                clipboard_command(&xclip),
                Some(("xclip", &["-selection", "clipboard"] as &[&str]))
            );
            // Wayland first when both exist.
            let both = |name: &str| name == "wl-copy" || name == "xclip";
            assert_eq!(clipboard_command(&both), Some(("wl-copy", &[] as &[&str])));
        }
    }

    #[test]
    fn tool_header_click_expands_on_release_and_drag_does_not_fold() {
        use nexus_core::{EffectState, Evidence, ExecutionStatus, ToolFinishedInfo, ToolOutcome};
        let mut front = Frontend::new(session());
        let run = run_id("run-tool-click");
        front.state.apply_event(&started(&run, 0));
        let output = (0..20)
            .map(|n| format!("output-{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        front.state.apply_event(&RunEvent::new(
            session(),
            run.clone(),
            1,
            EventPayload::ToolFinished(ToolFinishedInfo {
                call: CallId::new("call-click").unwrap(),
                outcome: ToolOutcome::new(
                    ExecutionStatus::Succeeded,
                    EffectState::KnownNotApplied,
                    Evidence::HostObserved,
                    output,
                    false,
                )
                .unwrap(),
            }),
        ));
        let mut terminal = fixed_terminal();
        for expanded in [true, false] {
            draw(&mut terminal, &mut front).unwrap();
            let geo = front.state.pointer_geometry();
            let window = window_lines(&front.state);
            let row = if expanded {
                window
                    .iter()
                    .position(|line| line.trim() == "output-19")
                    .unwrap()
            } else {
                (0..window.len())
                    .find(|row| front.state.tool_header_at(*row) == Some(1))
                    .unwrap()
            };
            let at = |kind| MouseEvent {
                kind,
                column: geo.body_x + 1,
                row: geo.body_y + geo.content_offset as u16 + row as u16,
                modifiers: KeyModifiers::empty(),
            };
            let before = front.state.entry(1).unwrap().folded;
            assert!(handle_mouse(
                &mut front,
                at(MouseEventKind::Down(MouseButton::Left))
            ));
            assert_eq!(front.state.entry(1).unwrap().folded, before);
            if expanded {
                front.state.apply_event(&RunEvent::new(
                    session(),
                    run.clone(),
                    2,
                    EventPayload::UsageUpdated(nexus_core::Usage::new(
                        None,
                        None,
                        nexus_core::UsageFinality::Provisional,
                    )),
                ));
                assert!(
                    front.state.text_selection().is_none(),
                    "runtime updates clear selection during a click"
                );
                draw(&mut terminal, &mut front).unwrap();
            }
            assert!(handle_mouse(
                &mut front,
                at(MouseEventKind::Up(MouseButton::Left))
            ));
            assert_eq!(front.state.entry(1).unwrap().folded, !expanded);
            assert_eq!(front.focus, Focus::Composer);
            assert!(front.live_approval.is_none());
        }
        draw(&mut terminal, &mut front).unwrap();
        let geo = front.state.pointer_geometry();
        let window = window_lines(&front.state);
        let row = (0..window.len())
            .find(|row| front.state.tool_header_at(*row) == Some(1))
            .unwrap();
        let at = |kind, column| MouseEvent {
            kind,
            column,
            row: geo.body_y + geo.content_offset as u16 + row as u16,
            modifiers: KeyModifiers::empty(),
        };
        handle_mouse(
            &mut front,
            at(MouseEventKind::Down(MouseButton::Left), geo.body_x + 1),
        );
        handle_mouse(
            &mut front,
            at(MouseEventKind::Drag(MouseButton::Left), geo.body_x + 4),
        );
        handle_mouse(
            &mut front,
            at(MouseEventKind::Up(MouseButton::Left), geo.body_x + 4),
        );
        assert!(
            front.state.entry(1).unwrap().folded,
            "drag copies, never toggles"
        );
        assert!(body_hit(&front.state, &window, geo.body_x, geo.body_y - 1).is_none());
    }

    #[test]
    fn mouse_drag_selects_body_text_and_release_toasts_silently() {
        let mut front = Frontend::new(session());
        front.state.notice("selectable words here");
        let mut terminal = fixed_terminal();
        draw(&mut terminal, &mut front).expect("frame draws");
        let geo = front.state.pointer_geometry();
        assert!(geo.content_len > 0, "content rows exist");
        // The body line reads "  selectable words here": drag over
        // "selectable" (columns 2..12 of content row 1).
        let row = geo.body_y + geo.content_offset as u16 + 1;
        let press = |column| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::empty(),
        };
        let drag = |column| MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::empty(),
        };
        assert!(handle_mouse(&mut front, press(geo.body_x + 2)));
        assert!(handle_mouse(&mut front, drag(geo.body_x + 12)));
        assert!(handle_mouse(
            &mut front,
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: geo.body_x + 12,
                row,
                modifiers: KeyModifiers::empty(),
            }
        ));
        assert_eq!(
            front.state.toast(),
            Some("copied 10 chars"),
            "release confirms with a toast"
        );
        assert_eq!(
            front.state.text_selection(),
            None,
            "the highlight clears with the copy"
        );
        assert!(
            !transcript(&front).contains("copied"),
            "the transcript stays clean"
        );
    }

    #[test]
    fn mouse_drag_maps_wide_glyph_cells_to_characters() {
        // "  \u{4e2d}\u{6587}\u{6d4b}\u{8bd5}\u{5185}\u{5bb9}": cells 2-3, 4-5, 6-7,
        // 8-9, 10-11, 12-13. A cell drag 4..10 must yank chars 1..4.
        let mut front = Frontend::new(session());
        front
            .state
            .notice("\u{4e2d}\u{6587}\u{6d4b}\u{8bd5}\u{5185}\u{5bb9}");
        let mut terminal = fixed_terminal();
        draw(&mut terminal, &mut front).expect("frame draws");
        let geo = front.state.pointer_geometry();
        let row = geo.body_y + geo.content_offset as u16 + 1;
        let at = |kind, column| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        };
        assert!(handle_mouse(
            &mut front,
            at(MouseEventKind::Down(MouseButton::Left), geo.body_x + 4)
        ));
        assert!(handle_mouse(
            &mut front,
            at(MouseEventKind::Drag(MouseButton::Left), geo.body_x + 10)
        ));
        assert!(handle_mouse(
            &mut front,
            at(MouseEventKind::Up(MouseButton::Left), geo.body_x + 10)
        ));
        assert_eq!(
            front.state.toast(),
            Some("copied 3 chars"),
            "three wide glyphs copied"
        );
    }

    #[test]
    fn mouse_drag_selects_the_composer_draft_and_release_toasts() {
        let mut front = Frontend::new(session());
        type_draft(&mut front, "hello world");
        let mut terminal = fixed_terminal();
        draw(&mut terminal, &mut front).expect("frame draws");
        let geo = front.state.pointer_geometry();
        let row = geo.composer_y + 1;
        let at = |kind, column| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        };
        assert!(handle_mouse(
            &mut front,
            at(MouseEventKind::Down(MouseButton::Left), geo.composer_x + 1)
        ));
        assert!(handle_mouse(
            &mut front,
            at(MouseEventKind::Drag(MouseButton::Left), geo.composer_x + 6)
        ));
        assert!(handle_mouse(
            &mut front,
            at(MouseEventKind::Up(MouseButton::Left), geo.composer_x + 6)
        ));
        assert_eq!(
            front.state.toast(),
            Some("copied 5 chars"),
            "the draft substring is copied"
        );
        assert!(
            !transcript(&front).contains("copied"),
            "the transcript stays clean"
        );
        // A bare click copies nothing and reports nothing.
        let before = transcript(&front).len();
        assert!(handle_mouse(
            &mut front,
            at(MouseEventKind::Down(MouseButton::Left), geo.composer_x + 1)
        ));
        assert!(!handle_mouse(
            &mut front,
            at(MouseEventKind::Up(MouseButton::Left), geo.composer_x + 1)
        ));
        assert_eq!(transcript(&front).len(), before, "bare clicks stay silent");
    }

    #[test]
    fn mouse_wheel_scrolls_the_viewport_without_moving_focus() {
        let mut front = Frontend::new(session());
        for index in 0..30 {
            front.state.record_submitted(&format!("entry {index}"));
        }
        front.state.set_viewport(40, 8);
        front.focus = Focus::Composer;
        let wheel = |kind| MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        };
        assert!(handle_mouse(&mut front, wheel(MouseEventKind::ScrollUp)));
        assert!(front.state.scrollback() > 0, "wheel up scrolls history");
        assert_eq!(front.focus, Focus::Composer, "the wheel never steals focus");
        assert!(handle_mouse(&mut front, wheel(MouseEventKind::ScrollDown)));
        assert_eq!(
            front.state.scrollback(),
            0,
            "wheel down returns to the tail"
        );
        assert!(!handle_mouse(&mut front, wheel(MouseEventKind::Moved)));
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
    async fn session_directory_key_requires_full_detail_and_a_published_directory() {
        let (runtime, _streams) = build_runtime().unwrap();
        let mut front = awaiting_approval(&run_id("run-directory-key"), "c1-0");
        front.focus = Focus::ApprovalCard;
        handle_key(&mut front, &runtime, press(KeyCode::Char('s'))).await;
        assert_eq!(front.request_counter, 0);
        assert!(front.live_approval.is_some());
        front.state.set_approval_geometry(ApprovalGeometry {
            inner_width: 80,
            inner_rows: 10,
            detail_rows: 6,
            clipped: false,
        });
        handle_key(&mut front, &runtime, press(KeyCode::Char('s'))).await;
        assert_eq!(front.request_counter, 0);
        assert!(
            front.live_approval.is_some(),
            "strict approval offers no directory grant"
        );
        front.live_approval.as_mut().unwrap().1.session_directory = Some("/etc".to_owned());
        handle_key(&mut front, &runtime, press(KeyCode::Char('s'))).await;
        assert_eq!(front.request_counter, 1);
        assert!(transcript(&front).contains("decision reply"));
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
    async fn ctrl_c_clears_the_draft_before_cancelling_or_quitting() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let mut front = Frontend::new(session());
        front.state.composer_type('h');
        front.state.composer_type('i');

        // First press clears the draft: no quit, no runtime command.
        assert!(!handle_key(&mut front, &runtime, ctrl('c')).await);
        assert_eq!(front.state.composer(), "");
        assert_eq!(front.request_counter, 0, "clearing issues no command");

        // Second press, with an empty draft and nothing live, quits.
        assert!(!front.state.can_cancel());
        assert!(handle_key(&mut front, &runtime, ctrl('c')).await);

        // While streaming, a draft is still cleared first; the run survives
        // until the next press cancels it.
        let run = run_id("run-cancel-after-clear");
        let mut live = Frontend::new(session());
        live.merger.adopt(&run);
        live.apply_events(vec![started(&run, 0)]);
        live.state.composer_type('x');
        assert!(!handle_key(&mut live, &runtime, ctrl('c')).await);
        assert_eq!(live.state.composer(), "");
        assert_eq!(live.request_counter, 0, "clearing does not cancel");
        assert!(!handle_key(&mut live, &runtime, ctrl('c')).await);
        assert_eq!(live.request_counter, 1, "the second press cancels");
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
        let (runtime, _) = build_runtime().expect("demo wiring is valid");
        let run = run_id("run-loop-closed");
        let mut front = Frontend::new(session());
        front.merger.adopt(&run);
        let mut sessions = registry_with("s1", front, runtime, closed_streams());
        let mut gate = RefreshGate::m0_test();
        let mut interval = interval();
        let mut terminal = fixed_terminal();

        interactive_loop(&mut sessions, &mut gate, &mut terminal, &mut interval)
            .await
            .expect("a closed channel pair ends the loop cleanly");
        let front = &sessions.active().front;
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
        data_tx.try_send(started(&run, 0)).expect("capacity");
        data_tx.try_send(finished(&run, 1)).expect("capacity");
        // Both senders are dropped up front: whatever the channel race
        // yields (event first or close first), the close path drains the
        // queued events before exiting, so both are always applied.
        drop(data_tx);
        drop(control_tx);
        let mut sessions = registry_with(
            "s1",
            front,
            runtime,
            EventStreams {
                data: data_rx,
                control: control_rx,
            },
        );
        let mut gate = RefreshGate::m0_test();
        let mut interval = interval();
        let mut terminal = fixed_terminal();

        // The terminal outcome is applied and presented, but the loop no
        // longer ends on it: only the channel close ends the loop.
        interactive_loop(&mut sessions, &mut gate, &mut terminal, &mut interval)
            .await
            .expect("the loop ends on close after applying the terminal outcome");
        let front = &sessions.active().front;
        assert_eq!(front.state.last_seq(), Some(1), "both events were applied");
        assert!(front.state.is_finished(), "the outcome was presented");
        assert!(
            transcript(front).contains(&format!("run {} started", run.as_str())),
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
                    let pushed = front.merger.push(*event);
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
                false,
            )
            .expect("valid submit builds");
            let reply = runtime.handle(submit).await.0;
            assert_eq!(
                reply.reply(),
                CommandReply::Accepted,
                "each demo task is accepted, even after a previous run"
            );
            assert!(settle_submit(&mut front, task, &reply, false));
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

    #[tokio::test]
    async fn command_suggestions_complete_but_wait_for_enter() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let (_run, mut front) = frontend_with(configured(2, &[], &[]));
        front.focus = Focus::Viewport;
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char(':'))).await);
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('m'))).await);
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('o'))).await);
        assert!(
            front
                .state
                .completions()
                .iter()
                .any(|item| item.insert == "model")
        );
        assert!(!front.state.overlay_open(), "typing only shows suggestions");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Enter)).await);
        assert_eq!(front.state.command_line(), Some("model"));
        assert!(!front.state.overlay_open(), "first Enter only completes");
        assert_eq!(front.focus, Focus::Viewport);
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Enter)).await);
        assert!(front.state.overlay_open(), "second Enter executes :m");
    }

    #[tokio::test]
    async fn escape_closes_suggestions_without_changing_focus_or_input() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let mut front = Frontend::new(session());
        front.focus = Focus::Viewport;
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char(':'))).await);
        assert!(!front.state.completions().is_empty());
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Esc)).await);
        assert!(front.state.completions().is_empty());
        assert!(front.state.colon_pending());
        assert_eq!(front.focus, Focus::Viewport);
    }

    #[tokio::test]
    async fn help_command_opens_modal_and_escape_closes_it_without_transcript_text() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let mut front = Frontend::new(session());
        front.focus = Focus::Viewport;
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char(':'))).await);
        for ch in "help".chars() {
            assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char(ch))).await);
        }
        let transcript_before = front.state.transcript().len();
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Enter)).await);
        assert!(front.state.help_open());
        assert_eq!(front.state.transcript().len(), transcript_before);

        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('j'))).await);
        assert!(
            front.state.help_open(),
            "modal navigation stays in the dialog"
        );
        assert_eq!(front.state.composer(), "", "keys cannot reach the composer");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Esc)).await);
        assert!(!front.state.overlay_open());
        assert_eq!(front.focus, Focus::Viewport);
    }

    #[test]
    fn file_suggestions_are_relative_bounded_and_do_not_read_files() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("nexus-tui-paths-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).expect("create fixture directory");
        std::fs::write(root.join("src/main.rs"), "secret file contents")
            .expect("create fixture file");
        std::fs::create_dir_all(root.join("target/generated"))
            .expect("create excluded generated directory");

        let matches = find_project_paths(&root, "src/mai");
        assert_eq!(matches, vec!["src/main.rs"]);
        assert!(find_project_paths(&root, "target/").is_empty());
        let mut state = AppState::new();
        state.composer_type('@');
        state.set_project_dir(root.to_string_lossy().into_owned());
        let (start, end, query) = state.composer_at_token().expect("@ token recognized");
        assert_eq!(query, "");
        state.complete_composer_token(start, end, &matches[0]);
        assert_eq!(state.composer(), "@src/main.rs");
        assert!(!state.composer().contains("secret file contents"));

        std::fs::remove_dir_all(root).expect("remove owned fixture tree");
    }

    #[tokio::test]
    async fn vim_viewport_jumps_and_half_page_keys_are_scoped_to_viewport() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let mut front = Frontend::new(session());
        for index in 0..40 {
            front
                .state
                .record_submitted(&format!("history entry {index}"));
        }
        let mut terminal = fixed_terminal();
        front.focus = Focus::Viewport;
        draw(&mut terminal, &mut front).expect("render establishes viewport geometry");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('g'))).await);
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('g'))).await);
        assert!(front.state.scrollback() > 0, "gg jumps to the history top");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('G'))).await);
        assert_eq!(front.state.scrollback(), 0, "G returns to the live tail");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('H'))).await);
        let top = front.state.scrollback();
        assert!(top > 0, "H jumps to the top");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('M'))).await);
        assert!(front.state.scrollback() < top, "M jumps to the middle");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('L'))).await);
        assert_eq!(front.state.scrollback(), 0, "L jumps to the bottom");
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('g'))).await);
        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Char('g'))).await);
        assert!(!handle_key(&mut front, &runtime, ctrl('d')).await);
        assert!(front.state.scrollback() > 0, "Ctrl+D scrolls half a page");
        assert_eq!(front.request_counter, 0, "navigation never submits work");
    }
}

#[cfg(test)]
mod cov_live_wiring {
    //! Coverage for the demo-or-live wiring: startup argument parsing,
    //! per-run provider resolution with an injected credential lookup, and
    //! real-reads root validation. No test touches the process environment
    //! or the network: credentials arrive through explicit closures and the
    //! live adapter is only ever constructed (endpoint parsing), never run.

    use super::*;

    /// Builds the full argv vector a process would receive.
    fn argv(args: &[&str]) -> Vec<String> {
        std::iter::once("nexus-tui")
            .chain(args.iter().copied())
            .map(str::to_string)
            .collect()
    }

    /// Unwraps the `Run` variant, asserting the branch was taken.
    fn tools_root(args: &[&str]) -> Option<std::path::PathBuf> {
        match parse_startup_args(&argv(args)) {
            StartupAction::Run(parsed) => parsed.tools_root,
            StartupAction::Usage => panic!("expected a run action for {args:?}"),
        }
    }

    /// Unwraps the demo flag from the `Run` variant.
    fn demo_flag(args: &[&str]) -> bool {
        match parse_startup_args(&argv(args)) {
            StartupAction::Run(parsed) => parsed.demo,
            StartupAction::Usage => panic!("expected a run action for {args:?}"),
        }
    }

    /// Asserts the parser rejects the flags with usage.
    fn usage(args: &[&str]) {
        assert!(
            matches!(parse_startup_args(&argv(args)), StartupAction::Usage),
            "expected usage for {args:?}"
        );
    }

    #[test]
    fn no_arguments_yield_no_explicit_jail() {
        assert_eq!(tools_root(&[]), None);
    }

    #[test]
    fn tools_root_defaults_to_the_working_directory() {
        let explicit = std::path::PathBuf::from("/srv/root");
        assert_eq!(
            resolve_tools_root(Some(explicit.clone())),
            Some(explicit),
            "an explicit flag always wins"
        );
        assert_eq!(
            resolve_tools_root(None),
            std::env::current_dir().ok(),
            "absent, the jail is the working directory"
        );
    }

    #[test]
    fn demo_mode_is_an_explicit_opt_in_flag() {
        assert!(!demo_flag(&[]), "the dev default serves no fakes");
        assert!(demo_flag(&["--demo"]), "--demo opts in");
        assert!(demo_flag(&["--demo", "--tools-root", "/srv/root"]));
        assert!(demo_flag(&["--tools-root", "/srv/root", "--demo"]));
        assert_eq!(tools_root(&["--demo"]), None);
    }

    #[test]
    fn tools_root_names_the_jail_explicitly() {
        assert_eq!(
            tools_root(&["--tools-root", "/srv/root"]),
            Some(std::path::PathBuf::from("/srv/root"))
        );
    }

    #[test]
    fn unknown_or_incomplete_flags_are_usage_never_silent_defaults() {
        usage(&["--help"]);
        usage(&["-h"]);
        usage(&["--tools-root"]);
        usage(&["--tools-root", ""]);
        usage(&["--tools"]);
        usage(&["--bogus"]);
        usage(&["typed-text-is-not-a-flag"]);
    }

    /// A configured selection like the `configured` helper's, with the
    /// credential lookup injected so no environment is touched.
    fn selection_with(active_model: Option<&str>) -> LiveSelection {
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
        let entry = nexus_config::ModelEntry::new("m0", "demo-provider", "demo-model")
            .expect("test model entry is valid");
        config.add_model(entry).expect("model admits");
        LiveSelection {
            config,
            active_model: active_model.map(str::to_owned),
            demo: true,
            pending_run: None,
            pending_run_id: None,
        }
    }

    #[test]
    fn no_selection_without_demo_fails_instead_of_serving_fakes() {
        let mut selection = LiveSelection {
            config: UserConfig::default_config(),
            active_model: None,
            demo: false,
            pending_run: None,
            pending_run_id: None,
        };
        let resolved = resolve_live_adapter(&selection, |_| Some("secret".to_owned()));
        assert!(
            matches!(resolved, LiveResolve::Failed(_, _)),
            "unconfigured dev runs fail instead of demoing"
        );
        if let LiveResolve::Failed(_, message) = resolved {
            assert!(message.contains("--demo"), "the failure names the opt-in");
        }
        // The same selection with the flag serves the scripted demo.
        selection.demo = true;
        assert!(matches!(
            resolve_live_adapter(&selection, |_| Some("secret".to_owned())),
            LiveResolve::Demo
        ));
    }

    #[test]
    fn no_selection_serves_the_demo_script() {
        let selection = LiveSelection {
            config: UserConfig::default_config(),
            active_model: None,
            demo: true,
            pending_run: None,
            pending_run_id: None,
        };
        assert!(
            matches!(
                resolve_live_adapter(&selection, |_| Some("secret".to_owned())),
                LiveResolve::Demo
            ),
            "nothing selected resolves to the demo, never a network call"
        );
    }

    #[test]
    fn unknown_model_is_an_explicit_failure_not_a_fake() {
        let selection = selection_with(Some("ghost"));
        assert!(
            matches!(
                resolve_live_adapter(&selection, |_| Some("secret".to_owned())),
                LiveResolve::Failed(ErrorCategory::InvalidInput, _)
            ),
            "a dangling selection must fail loudly instead of serving the demo"
        );
    }

    #[test]
    fn wiring_notice_distinguishes_unavailable_provider_and_real_writes() {
        let notice = wiring_notice(
            &selection_with(Some("ghost")),
            Some(std::path::Path::new("/project")),
        );
        assert!(notice.contains("unavailable"));
        assert!(!notice.contains("demo script"));
        assert!(notice.contains("real file tools + sandboxed exec"));
        assert!(notice.contains("writes/exec require approval"));
    }

    #[test]
    fn live_wrapper_delivers_text_before_the_http_turn_finishes() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("mock binds");
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let (observed, wait_observed) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("mock accepts");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut head = Vec::new();
            let mut byte = [0];
            while !head.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            let head = String::from_utf8(head).unwrap();
            let length: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .unwrap()
                .parse()
                .unwrap();
            stream.read_exact(&mut vec![0; length]).unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"early\"},\"finish_reason\":null}]}\n\n").unwrap();
            // Do not finish the response until the wrapper's caller has
            // observed the prefix. A batch-only wrapper cannot satisfy this.
            wait_observed
                .recv_timeout(Duration::from_secs(5))
                .expect("early sink delivery");
            stream.write_all(b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n").unwrap();
        });
        let request = ModelRequest::new(
            RunId::new("run-stream").unwrap(),
            nexus_core::TurnId::new("turn-stream").unwrap(),
            "test-profile",
            Vec::new(),
            None,
            4096,
        )
        .unwrap();
        let wrapper = PerRunProvider::new();
        {
            let mut current = wrapper.current.lock().unwrap();
            current.run = Some(request.run().clone());
            current.provider = ActiveProvider::Live(
                OpenAiProvider::new(
                    &endpoint,
                    nexus_config::CredentialRef::env_var("PATH").unwrap(),
                    "test-model",
                )
                .unwrap(),
            );
        }
        let events = wrapper.stream_with_sink(
            &request,
            &ProviderContext::new(Duration::from_secs(10), false, None),
            &|event| {
                if let ProviderEvent::TextDelta { text, .. } = event {
                    assert_eq!(text, "early");
                    observed.send(()).unwrap();
                } else {
                    assert!(!event.is_terminal());
                }
            },
        );
        assert!(wrapper.supports_incremental_streaming());
        assert!(matches!(
            events.last(),
            Some(ProviderEvent::TurnFinished(_))
        ));
        server.join().expect("mock finishes");
    }

    #[test]
    fn missing_credential_is_an_explicit_retryable_failure() {
        let selection = selection_with(Some("m0"));
        assert!(
            matches!(
                resolve_live_adapter(&selection, |_| None),
                LiveResolve::Failed(ErrorCategory::Authentication, _)
            ),
            "without a credential the run fails; it must not silently fake"
        );
        assert!(
            matches!(
                resolve_live_adapter(&selection, |_| Some(String::new())),
                LiveResolve::Failed(ErrorCategory::Authentication, _)
            ),
            "an empty credential value counts as missing"
        );
    }

    #[test]
    fn present_credential_resolves_the_live_adapter_without_network() {
        let selection = selection_with(Some("m0"));
        match resolve_live_adapter(&selection, |_| Some("secret".to_owned())) {
            LiveResolve::Live(adapter) => {
                assert_eq!(adapter.model(), "demo-model");
                assert!(
                    adapter.uses_tls(),
                    "the https test endpoint resolves to the TLS bridge"
                );
            }
            LiveResolve::Demo => panic!("a complete selection must not serve the demo"),
            LiveResolve::Failed(_, message) => {
                panic!("a complete selection must resolve: {message}")
            }
        }
    }

    #[test]
    fn unparsable_endpoint_is_an_explicit_failure() {
        // An empty host passes the document shape check but cannot become
        // an adapter: resolution must fail instead of serving the demo.
        let mut config = UserConfig::default_config();
        let profile = nexus_config::ProviderProfile::new(
            "empty-host",
            "Empty host",
            nexus_config::AdapterKind::Direct,
            Some("https://:8080".to_owned()),
            nexus_config::CredentialRef::env_var("NEXUS_TUI_TEST_KEY")
                .expect("credential reference is valid"),
            "demo-model",
        )
        .expect("the document shape accepts an empty host");
        config.add_provider(profile).expect("provider admits");
        let entry = nexus_config::ModelEntry::new("m0", "empty-host", "demo-model")
            .expect("test model entry is valid");
        config.add_model(entry).expect("model admits");
        let selection = LiveSelection {
            config,
            active_model: Some("m0".to_owned()),
            demo: true,
            pending_run: None,
            pending_run_id: None,
        };
        assert!(
            matches!(
                resolve_live_adapter(&selection, |_| Some("secret".to_owned())),
                LiveResolve::Failed(ErrorCategory::InvalidInput, _)
            ),
            "an endpoint the adapter rejects fails instead of faking"
        );
    }

    #[test]
    fn failure_events_are_terminal_with_static_diagnostics() {
        for (category, retry) in [
            (ErrorCategory::Authentication, RetryGuidance::SafeToRetry),
            (ErrorCategory::InvalidInput, RetryGuidance::DoNotRetry),
        ] {
            match live_failure(category, "static diagnostic") {
                ProviderEvent::Failed(error) => {
                    assert_eq!(error.category(), category);
                    assert_eq!(error.retry(), retry);
                }
                _ => panic!("resolution failures are terminal"),
            }
        }
    }

    #[test]
    fn live_roots_must_exist_as_directories() {
        let missing = std::path::PathBuf::from("/nonexistent-nexus-tui-jail");
        let session = SessionConfig {
            config: UserConfig::default_config(),
            path: None,
            active_model: None,
            tools_root: Some(missing),
            demo: false,
            strict_tools: true,
        };
        assert!(
            build_live_runtime(&session).is_err(),
            "a missing jail is a startup error, never silent scripted reads"
        );
        let file = std::path::PathBuf::from("/dev/null");
        let session = SessionConfig {
            tools_root: Some(file),
            ..session
        };
        assert!(
            build_live_runtime(&session).is_err(),
            "a non-directory jail is a startup error"
        );
    }

    #[test]
    fn demo_wiring_builds_without_a_jail() {
        let session = SessionConfig {
            config: UserConfig::default_config(),
            path: None,
            active_model: None,
            tools_root: None,
            demo: false,
            strict_tools: true,
        };
        let (_runtime, _streams, live) = build_live_runtime(&session).expect("demo wiring builds");
        assert!(
            !Frontend::new(SessionId::new("sess-test-live").expect("test session id is valid"))
                .is_live(),
            "no handle means demo, even beside a live-capable runtime"
        );
        drop(live);
    }

    #[test]
    fn startup_defaults_to_development_and_accepts_strict_tools() {
        let args = vec!["nexus-tui".to_owned()];
        assert!(matches!(
            parse_startup_args(&args),
            StartupAction::Run(StartupArgs {
                strict_tools: false,
                ..
            })
        ));
        let args = vec!["nexus-tui".to_owned(), "--strict-tools".to_owned()];
        assert!(matches!(
            parse_startup_args(&args),
            StartupAction::Run(StartupArgs {
                strict_tools: true,
                ..
            })
        ));
    }
}
