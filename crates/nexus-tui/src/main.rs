//! Interactive TUI over the runtime command/event port.
//!
//! Interactive sessions use the runtime command/event port and private local
//! session storage. The explicit `--demo` path wires scripted doubles and
//! remains ephemeral; a stderr banner says which mode is active.
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

use std::collections::HashMap;
use std::io::{self, IsTerminal};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{
    Event, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind, poll, read,
};
use nexus_config::{
    AgentMode, UserConfig, load as load_config, resolve_path, resolve_with, save as save_config,
};
use nexus_core::{
    AgentError, ApprovalNotice, CommandReply, CommandResponse, ErrorCategory, EventPayload,
    GetSnapshotCommand, Limits, ListSessionsCommand, ModelInstructions, ModelRequest,
    ProviderCapabilities, ProviderContext, ProviderEvent, ProviderPort, RequestId,
    RestoreSessionCommand, RetryGuidance, RunEvent, RunId, SessionId, ToolRegistration,
};
use nexus_fakes::{FakeProvider, FakeTool};
use nexus_openai::OpenAiProvider;
use nexus_runtime::transport::{ORDERED_EVENT_BUFFER_CAPACITY, OrderedEventConsumer};
use nexus_runtime::{EventStreams, Policy, Runtime, RuntimeConfig, SessionStoreHandle};
use nexus_tui::decisions::{approve_notice_command, deny_notice_command};
use nexus_tui::{
    Action, AppState, CompletionItem, Focus, ModelChoice, RefreshGate, SessionArgs, SlashCommand,
    TOAST_TTL, TextSelection, cancel_command, cell_to_char_col, install_panic_hook, map_key,
    next_focus, render, submit_command,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

mod diagnostics;

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
const MAX_REORDER_EVENTS: usize = ORDERED_EVENT_BUFFER_CAPACITY;
const DEMO_SESSION: &str = "sess-tui-m0-test";
/// The M0 fixture default is intentionally small, but interactive coding
/// runs need room for inspect/edit/check/retry cycles.
const TUI_MODEL_TURNS_PER_RUN: u32 = 64;
const TUI_TOOL_CALLS_PER_RUN: u32 = 128;
const TUI_RUN_DURATION: Duration = Duration::from_secs(60 * 60);
const TUI_RETAINED_CONTEXT_ITEMS: usize = 512;
static SESSION_NONCE: AtomicU64 = AtomicU64::new(0);

fn new_tui_session_id() -> io::Result<SessionId> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let nonce = SESSION_NONCE.fetch_add(1, Ordering::SeqCst);
    SessionId::new(format!(
        "sess-tui-{:x}-{nanos:x}-{nonce:x}",
        std::process::id()
    ))
    .map_err(io::Error::other)
}

fn application_session_store() -> io::Result<SessionStoreHandle> {
    let directory = nexus_session::default_data_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "private local session storage is unavailable on this platform",
        )
    })?;
    let store = nexus_session::DirectorySessionStore::open(directory).map_err(io::Error::other)?;
    Ok(SessionStoreHandle::new(Box::new(store)))
}

fn tui_runtime_limits() -> Limits {
    let mut limits = Limits::m0_test();
    limits.max_model_turns_per_run = TUI_MODEL_TURNS_PER_RUN;
    limits.max_tool_calls_per_run = TUI_TOOL_CALLS_PER_RUN;
    limits.run_duration = TUI_RUN_DURATION;
    limits.per_tool_timeout = Duration::from_secs(10 * 60);
    limits.retained_context_items = TUI_RETAINED_CONTEXT_ITEMS;
    limits
}

/// Fallible production composition: the runtime validates limits, provider
/// capabilities, and every tool registration before any run can start, so an
/// invalid demo wiring is an explicit startup error, never a hung run.
#[cfg(test)]
fn build_runtime() -> io::Result<(Runtime, EventStreams)> {
    build_demo_runtime(tui_runtime_limits())
}

fn build_demo_runtime(limits: Limits) -> io::Result<(Runtime, EventStreams)> {
    let config = RuntimeConfig {
        limits,
        policy: Policy::m0_test(),
        has_approval_handler: true,
    };
    let provider = Arc::new(PerRunProvider::new());
    let tools = vec![
        ToolRegistration::read(Arc::new(FakeTool::read_only())),
        ToolRegistration::mutation(Arc::new(FakeTool::mutation())),
    ];
    Runtime::try_new(config, provider, tools).map_err(io::Error::other)
}

/// User configuration resolved once at startup, plus the path it was read
/// from (so every session writes back to the same document) and the model the
/// session boots with. The document itself has one shared owner across slots.
///
/// The provider/model document's location remains [`nexus_config`]'s
/// `NEXUS_CONFIG` / platform default. Operational base and overlay files are
/// resolved separately and never become the TUI's writeback target.
///
/// The shared document is process-owned; each slot keeps its own runtime,
/// active model, and history.
#[derive(Debug, Clone)]
struct SessionConfig {
    diagnostics: Option<diagnostics::DiagnosticLog>,
    user_config: SharedUserConfig,
    settings: nexus_config::EffectiveSettings,
    active_model: Option<String>,
    /// Project root for real file reads/writes: `--tools-root`, else the working
    /// directory. `None` (tests only) keeps every tool scripted. Cloned
    /// with the template so every slot shares the startup wiring while
    /// keeping its own runtime and selection.
    tools_root: Option<std::path::PathBuf>,
    strict_tools: bool,
    execution: ExecutionMode,
    gateway_image: Option<String>,
    workload_image: Option<String>,
    /// Scripted demo explicitly requested with `--demo`. Without it the
    /// runtime never serves fakes, however unconfigured the selection is.
    demo: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutionMode {
    Local,
    Container,
}

#[derive(Debug)]
struct UserConfigDocument {
    config: UserConfig,
    path: Option<std::path::PathBuf>,
}

type SharedUserConfig = Arc<Mutex<UserConfigDocument>>;

fn shared_user_config(config: UserConfig, path: Option<std::path::PathBuf>) -> SharedUserConfig {
    Arc::new(Mutex::new(UserConfigDocument { config, path }))
}

fn user_config_snapshot(shared: &SharedUserConfig) -> UserConfig {
    shared
        .lock()
        .expect("user configuration lockable")
        .config
        .clone()
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
        StartupAction::Help => startup_usage(true),
        StartupAction::Usage => startup_usage(false),
    };
    let settings = nexus_config::load_settings(
        nexus_config::SettingsProfile::Tui,
        args.base_config.as_deref(),
        &args.config_overlays,
    )
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut session = session_config_at_with_settings(resolve_path(None), settings)?;
    if args.check_config {
        println!("Configuration is valid.");
        std::process::exit(0);
    }
    if args.print_effective_config {
        println!("{}", nexus_config::settings_json(&session.settings));
        std::process::exit(0);
    }
    let config = user_config_snapshot(&session.user_config);
    let (effective_gateway, effective_workload) = merge_startup_images(&args, &config);
    let mut effective_args = args.clone();
    effective_args.gateway_image = effective_gateway.clone();
    effective_args.workload_image = effective_workload.clone();
    let root_resolved = resolve_tools_root(args.tools_root.clone()).is_some();
    validate_execution_args(&effective_args, root_resolved, cfg!(feature = "container"))?;
    session.tools_root = resolve_tools_root(args.tools_root);
    session.demo = args.demo;
    session.strict_tools = args.strict_tools;
    session.execution = args.execution;
    session.gateway_image = effective_gateway;
    session.workload_image = effective_workload;
    Ok(session)
}

fn merge_startup_images(
    args: &StartupArgs,
    config: &UserConfig,
) -> (Option<String>, Option<String>) {
    if args.execution == ExecutionMode::Local {
        return (args.gateway_image.clone(), args.workload_image.clone());
    }
    (
        args.gateway_image
            .clone()
            .or_else(|| config.gateway_image().map(str::to_owned)),
        args.workload_image
            .clone()
            .or_else(|| config.workload_image().map(str::to_owned)),
    )
}

fn validate_execution_args(
    args: &StartupArgs,
    root_resolved: bool,
    container_feature: bool,
) -> io::Result<()> {
    if args.execution == ExecutionMode::Local {
        if args.gateway_image.is_some() || args.workload_image.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "container images require --execution container",
            ));
        }
        return Ok(());
    }
    if !root_resolved {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "container execution requires an available workspace directory",
        ));
    }
    if args.strict_tools || args.demo {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "container execution cannot be combined with --strict-tools or --demo",
        ));
    }
    if args.gateway_image.is_none() || args.workload_image.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "container execution requires gateway and workload images; configure them in user config or pass --gateway-image and --container-image",
        ));
    }
    if !container_feature {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "container execution is not enabled in this build; rebuild nexus-tui with --features container",
        ));
    }
    Ok(())
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
#[cfg(test)]
fn session_config_at(path: Option<std::path::PathBuf>) -> io::Result<SessionConfig> {
    let settings = nexus_config::load_settings(nexus_config::SettingsProfile::Tui, None, &[])
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    session_config_at_with_settings(path, settings)
}

fn session_config_at_with_settings(
    path: Option<std::path::PathBuf>,
    settings: nexus_config::EffectiveSettings,
) -> io::Result<SessionConfig> {
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
        diagnostics: None,
        user_config: shared_user_config(config, path),
        settings,
        active_model,
        tools_root: None,
        strict_tools: false,
        execution: ExecutionMode::Local,
        gateway_image: None,
        workload_image: None,
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

/// Startup arguments select operational-settings layers, the project root,
/// strict permissions, or the scripted demo. Provider/model configuration
/// remains selected by `NEXUS_CONFIG` or the platform config path.
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
    execution: ExecutionMode,
    gateway_image: Option<String>,
    workload_image: Option<String>,
    base_config: Option<std::path::PathBuf>,
    config_overlays: Vec<std::path::PathBuf>,
    check_config: bool,
    print_effective_config: bool,
}

/// Argument-parser outcome: run with the parsed jail, or print usage.
enum StartupAction {
    /// Serve the TUI with this optional real-files jail.
    Run(StartupArgs),
    /// Print help and exit successfully.
    Help,
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
    let mut execution = ExecutionMode::Local;
    let mut execution_set = false;
    let mut gateway_image = None;
    let mut workload_image = None;
    let mut base_config = None;
    let mut config_overlays = Vec::new();
    let mut check_config = false;
    let mut print_effective_config = false;
    let mut args = argv.iter().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return StartupAction::Help,
            "--demo" => demo = true,
            "--strict-tools" => strict_tools = true,
            "--execution" | "--run" => {
                if execution_set {
                    return StartupAction::Usage;
                }
                execution_set = true;
                execution = match args.next().map(String::as_str) {
                    Some("local") => ExecutionMode::Local,
                    Some("container") => ExecutionMode::Container,
                    _ => return StartupAction::Usage,
                };
            }
            "--gateway-image" => {
                if gateway_image.is_some() {
                    return StartupAction::Usage;
                }
                gateway_image = match args.next() {
                    Some(value) if !value.is_empty() => Some(value.clone()),
                    _ => return StartupAction::Usage,
                };
            }
            "--container-image" => {
                if workload_image.is_some() {
                    return StartupAction::Usage;
                }
                workload_image = match args.next() {
                    Some(value) if !value.is_empty() => Some(value.clone()),
                    _ => return StartupAction::Usage,
                };
            }
            "--base-config" => {
                if base_config.is_some() {
                    return StartupAction::Usage;
                }
                base_config = match args.next() {
                    Some(value) if !value.is_empty() && !value.starts_with("--") => {
                        Some(std::path::PathBuf::from(value))
                    }
                    _ => return StartupAction::Usage,
                };
            }
            "--config-overlay" => match args.next() {
                Some(value) if !value.is_empty() && !value.starts_with("--") => {
                    config_overlays.push(std::path::PathBuf::from(value));
                }
                _ => return StartupAction::Usage,
            },
            "--check-config" => check_config = true,
            "--print-effective-config" => print_effective_config = true,
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
        execution,
        gateway_image,
        workload_image,
        base_config,
        config_overlays,
        check_config,
        print_effective_config,
    })
}

/// Prints actionable help or a concise invalid-argument diagnostic.
/// Keeping the exit separate from parsing makes rejection paths testable.
fn startup_usage(help: bool) -> ! {
    let text = format!(
        concat!(
            "Usage: nexus-tui [OPTIONS]\n\n",
            "Modes:\n",
            "  --execution <MODE>  Select local (default) or container execution\n",
            "  --run <MODE>        Alias for --execution\n",
            "  --demo              Run the scripted demo (fake provider/tools; submits a canned task)\n\n",
            "Workspace and tools:\n",
            "  --tools-root <PATH> Workspace jail; defaults to the current directory\n",
            "  --strict-tools      Use the strict jailed-tools policy\n\n",
            "Operational settings:\n",
            "  --base-config <PATH>       Replace the optional system base file\n",
            "  --config-overlay <PATH>    Apply a partial overlay (repeatable)\n",
            "  --check-config             Validate configuration and exit\n",
            "  --print-effective-config   Print resolved runtime settings and exit\n\n",
            "Container images:\n",
            "  --gateway-image <IMAGE>    Gateway image (overrides configured image)\n",
            "  --container-image <IMAGE>  Workload image (overrides configured image)\n\n",
            "Other:\n",
            "  -h, --help          Print this help\n\n",
            "Behavior:\n",
            "  Without --demo, live provider wiring is used when configured; otherwise\n",
            "  task submission reports that no provider is configured. Local mode is default.\n",
            "  Container mode is experimental, needs a container-enabled build, both images\n",
            "  (flags or user config), and a workspace. It does not establish isolation.\n",
            "  Build: cargo build -p nexus-tui --features container\n",
            "  Example: cargo run -p nexus-tui --features container -- --execution container\n",
            "           --tools-root \"$PWD\" --gateway-image GATEWAY --container-image WORKLOAD\n",
            "  Images must already be available locally; startup does not pull them.\n",
            "  See QUICKSTART.md and docs/design/11-container-execution.md.\n\n",
            "{}\n",
        ),
        if cfg!(feature = "container") {
            "Container support: enabled in this build."
        } else {
            "Container support: disabled; rebuild with the command above."
        }
    );
    if help {
        print!("{text}");
        std::process::exit(0);
    }
    eprintln!("Invalid command-line arguments.\n\n{text}");
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
    provider_settings: nexus_config::settings::ProviderSettings,
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

/// Capability contract shared by the adapters currently supported by this
/// TUI. Keep it explicit rather than deriving production claims from the demo
/// provider; the test suite pins it against each concrete current adapter.
fn tui_provider_capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        text: true,
        streaming: true,
        tool_calls: true,
        structured_output: false,
        usage_reporting: true,
        max_context_items: None,
        max_output_bytes: None,
    }
}

fn adapter_satisfies_tui_requirements(capabilities: &ProviderCapabilities) -> bool {
    let limits = tui_runtime_limits();
    capabilities.text
        && capabilities.streaming
        && capabilities.tool_calls
        && capabilities.max_context_items.is_none_or(|maximum| {
            maximum as usize
                >= limits
                    .retained_context_items
                    .min(nexus_core::provider::MAX_CONVERSATION_ITEMS)
        })
        && capabilities.max_output_bytes.is_none_or(|maximum| {
            maximum as usize
                >= limits
                    .max_tool_output_bytes
                    .min(Limits::M0_TEST_TOOL_OUTPUT_BYTES)
        })
}

fn admit_adapter<A>(adapter: A, capabilities: &ProviderCapabilities) -> Result<A, ErrorCategory> {
    if adapter_satisfies_tui_requirements(capabilities) {
        Ok(adapter)
    } else {
        Err(ErrorCategory::UnsupportedCapability)
    }
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
    let Ok(context_management) = selection
        .config
        .context_management_for(&entry.provider, Some(model_id))
    else {
        return LiveResolve::Failed(
            ErrorCategory::InvalidInput,
            "selected model context configuration is invalid",
        );
    };
    match OpenAiProvider::from_profile(profile, &entry.name)
        .and_then(|adapter| adapter.with_operational_settings(&selection.provider_settings))
    {
        Ok(adapter) => {
            let adapter = adapter.with_context_management(context_management);
            let capabilities = adapter.capabilities();
            match admit_adapter(adapter, &capabilities) {
                Ok(adapter) => LiveResolve::Live(adapter),
                Err(category) => LiveResolve::Failed(
                    category,
                    "provider capabilities do not satisfy TUI runtime requirements",
                ),
            }
        }
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
    store: Option<SessionStoreHandle>,
    maintenance: Option<(SessionId, tokio::sync::oneshot::Receiver<String>)>,
}

impl SessionRegistry {
    #[cfg(test)]
    fn new(first: SessionSlot, template: SessionConfig) -> Self {
        Self::new_with_store(first, template, None)
    }

    fn new_with_store(
        first: SessionSlot,
        template: SessionConfig,
        store: Option<SessionStoreHandle>,
    ) -> Self {
        Self {
            slots: vec![first],
            active: 0,
            template,
            store,
            maintenance: None,
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

    /// Poll disk work without delaying input/rendering. Worker failure does not
    /// imply rollback; recovery must happen before retrying an uncertain write.
    fn poll_maintenance(&mut self) -> bool {
        let Some((_, receiver)) = self.maintenance.as_mut() else {
            return false;
        };
        let message = match receiver.try_recv() {
            Ok(message) => message,
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return false,
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) =>
                "session maintenance worker failed; restart the TUI to reconcile storage; do not blindly retry".to_owned(),
        };
        let (origin, _) = self.maintenance.take().expect("pending operation");
        if let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| slot.front.session == origin)
        {
            slot.front.state.notice(&message);
        }
        true
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
        let mut session_config = self.template.clone();
        session_config.active_model =
            boot_model(&user_config_snapshot(&session_config.user_config));
        let (runtime, streams, live) =
            build_live_runtime_with_store(&session_config, self.store.clone())?;
        let session = if self.store.is_some() {
            new_tui_session_id()?
        } else {
            SessionId::new(format!("{DEMO_SESSION}-{number}")).map_err(io::Error::other)?
        };
        let mut front = Frontend::with_config(session, session_config);
        let label = format!("s{number}");
        front.state.set_session_label(&label);
        front
            .state
            .notice(&format!("new conversation · {label} · type a task below"));
        front.attach_live(
            live,
            self.template.tools_root.as_deref(),
            self.template.execution,
        );
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
    discovered_context: Mutex<HashMap<String, nexus_core::ContextWindow>>,
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
            provider_settings: Default::default(),
            active_model: None,
            demo: true,
            pending_run: None,
            pending_run_id: None,
        })))
    }

    /// Production wiring over the slot's shared selection.
    fn live(binding: LiveHandle) -> Self {
        Self {
            capabilities: tui_provider_capabilities(),
            binding,
            discovered_context: Mutex::new(HashMap::new()),
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

    fn context_management(&self) -> nexus_core::ContextManagementSettings {
        let selection_info = {
            let binding = self.binding.lock().expect("live selection lockable");
            let selection = binding.pending_run.as_deref().unwrap_or(&binding);
            let Some(model_id) = selection.active_model.as_deref() else {
                return nexus_core::ContextManagementSettings::default();
            };
            let Some(entry) = selection.config.model(model_id) else {
                return nexus_core::ContextManagementSettings::default();
            };
            // Invalid selections fail explicitly when resolving the live adapter;
            // no window or policy is inferred for an unavailable model.
            let settings = selection
                .config
                .context_management_for(&entry.provider, Some(model_id))
                .unwrap_or_default();
            let Some(profile) = selection.config.provider(&entry.provider) else {
                return settings;
            };
            (
                settings,
                format!("{}:{}:{}", entry.provider, entry.id, entry.name),
                profile.clone(),
                entry.name.clone(),
                selection.provider_settings.clone(),
            )
        };
        let (mut settings, cache_key, profile, model_name, provider_settings) = selection_info;
        if settings.window.is_some() {
            return settings;
        }
        if let Some(window) = self
            .discovered_context
            .lock()
            .expect("discovered context lockable")
            .get(&cache_key)
            .copied()
        {
            settings.window = Some(window);
            return settings;
        }
        let Ok(adapter) = OpenAiProvider::from_profile(&profile, &model_name)
            .and_then(|adapter| adapter.with_operational_settings(&provider_settings))
        else {
            return settings;
        };
        let adapter = adapter.with_context_management(settings);
        if let Some(window) = adapter
            .is_ollama_endpoint()
            .then(|| {
                adapter
                    .discover_context_management()
                    .context_management()
                    .window
            })
            .flatten()
        {
            self.discovered_context
                .lock()
                .expect("discovered context lockable")
                .insert(cache_key, window);
            settings.window = Some(window);
        }
        settings
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
#[cfg(test)]
fn build_live_runtime(
    session_config: &SessionConfig,
) -> io::Result<(Runtime, EventStreams, LiveHandle)> {
    build_live_runtime_with_store(session_config, None)
}

fn build_live_runtime_with_store(
    session_config: &SessionConfig,
    session_store: Option<SessionStoreHandle>,
) -> io::Result<(Runtime, EventStreams, LiveHandle)> {
    let policy = match session_config.execution {
        ExecutionMode::Local => match &session_config.tools_root {
            Some(root) if !session_config.strict_tools => {
                Policy::development(root).map_err(io::Error::other)?
            }
            _ => Policy::m0_test(),
        },
        ExecutionMode::Container => {
            #[cfg(feature = "container")]
            {
                if session_config.tools_root.is_none() || session_config.strict_tools {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "container execution requires a workspace and development policy",
                    ));
                }
                Policy::container()
            }
            #[cfg(not(feature = "container"))]
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "container execution is not enabled in this build",
                ));
            }
        }
    };
    let config = RuntimeConfig {
        limits: session_config.settings.limits(),
        policy,
        has_approval_handler: true,
    };
    let tools = match session_config.execution {
        ExecutionMode::Container => {
            #[cfg(feature = "container")]
            {
                let root = session_config.tools_root.as_deref().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "container workspace is missing",
                    )
                })?;
                let gateway_image = session_config.gateway_image.as_deref().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "gateway image is missing")
                })?;
                let workload_image = session_config.workload_image.as_deref().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "workload image is missing")
                })?;
                nexus_container::tool_registrations(root, gateway_image, workload_image)
                    .map_err(io::Error::other)?
            }
            #[cfg(not(feature = "container"))]
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "container execution is not enabled in this build",
                ));
            }
        }
        ExecutionMode::Local => match &session_config.tools_root {
            None => vec![
                ToolRegistration::read(Arc::new(FakeTool::read_only())),
                ToolRegistration::mutation(Arc::new(FakeTool::mutation())),
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
                    let tools =
                        nexus_tools::development_tools(&canonical).map_err(io::Error::other)?;
                    return runtime_with_tools(session_config, config, tools, session_store);
                }
                nexus_tools::strict_tools(&canonical).map_err(io::Error::other)?
            }
        },
    };
    runtime_with_tools(session_config, config, tools, session_store)
}

fn runtime_with_tools(
    session_config: &SessionConfig,
    config: RuntimeConfig,
    tools: Vec<ToolRegistration>,
    session_store: Option<SessionStoreHandle>,
) -> io::Result<(Runtime, EventStreams, LiveHandle)> {
    let binding = Arc::new(Mutex::new(LiveSelection {
        config: user_config_snapshot(&session_config.user_config),
        provider_settings: session_config.settings.provider().clone(),
        active_model: session_config.active_model.clone(),
        demo: session_config.demo,
        pending_run: None,
        pending_run_id: None,
    }));
    let provider = Arc::new(PerRunProvider::live(Arc::clone(&binding)));
    let (runtime, streams) = match session_store {
        Some(store) => Runtime::try_new_with_session_store_handle(
            config,
            provider,
            tools,
            ModelInstructions::default(),
            store,
        ),
        None => Runtime::try_new(config, provider, tools),
    }
    .map_err(io::Error::other)?;
    let runtime = match &session_config.diagnostics {
        Some(log) => runtime.with_diagnostic_writer(log.clone()),
        None => runtime,
    };
    Ok((runtime, streams, binding))
}

/// One-line wiring report for the transcript: which provider (if any) the
/// next run resolves to, and whether file tools are real. Only counts and the
/// operator's own identities appear; values never do.
fn wiring_notice(
    selection: &LiveSelection,
    tools_root: Option<&std::path::Path>,
    execution: ExecutionMode,
) -> String {
    let provider = match resolve_live_adapter(selection, env_credential) {
        LiveResolve::Demo => "demo script".to_owned(),
        LiveResolve::Live(adapter) => format!("live adapter ({})", adapter.model()),
        LiveResolve::Failed(_, message) => format!("unavailable ({message})"),
    };
    let tools = match (execution, tools_root) {
        (ExecutionMode::Container, Some(root)) => format!(
            "UNVALIDATED container tools at {} (workspace is writable; isolation is not established)",
            root.display()
        ),
        (ExecutionMode::Container, None) => {
            "UNVALIDATED container tools (workspace unavailable)".to_owned()
        }
        (ExecutionMode::Local, None) => "scripted tools".to_owned(),
        (ExecutionMode::Local, Some(root)) => format!(
            "real file tools + sandboxed exec at {} (writes/exec require approval)",
            root.display()
        ),
    };
    format!("wiring: {provider}; {tools}")
}

fn main() {
    install_panic_hook();
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
    eprintln!(
        "nexus-tui dev: live wiring by default. Interactive sessions use private local history storage (not encrypted); \
          A slot whose model resolves to a configured provider with a credential \
          uses the real adapter (network egress, billable); local --tools-root enables \
           jailed file tools + sandboxed exec (writes/exec require approval). \
           --execution container is experimental and does not establish isolation. \
           Without a configured provider, runs fail as not-configured; \
           relaunch with --demo for the scripted ephemeral demo."
    );
    if session_config.execution == ExecutionMode::Container {
        eprintln!(
            "SECURITY WARNING: container isolation is not validated on native Linux. \
             The selected workspace is live-mounted; model/tool code can read and modify it. \
             Public egress and host/LAN/metadata denial are not verified."
        );
    }
    if io::stdout().is_terminal() {
        match nexus_tui::TerminalGuard::setup_with_mouse(
            session_config.settings.tui().input.mouse_capture,
        ) {
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
                if session_config.execution == ExecutionMode::Container {
                    return Err(io::Error::other(format!(
                        "terminal setup failed ({error}); container execution requires an interactive terminal and cannot fall back to scripted tools"
                    )));
                }
                if !session_config.demo {
                    return Err(io::Error::other(format!(
                        "terminal setup failed ({error}); relaunch with --demo for the scripted transcript fallback"
                    )));
                }
                eprintln!("terminal setup failed ({error}); demo transcript fallback");
                let (runtime, streams) = build_demo_runtime(session_config.settings.limits())?;
                headless(runtime, streams, session_config).await
            }
        }
    } else {
        if session_config.execution == ExecutionMode::Container {
            return Err(io::Error::other(
                "container execution requires an interactive terminal; it cannot fall back to scripted tools",
            ));
        }
        if !session_config.demo {
            return Err(io::Error::other(
                "no terminal attached; relaunch with --demo for the scripted transcript fallback",
            ));
        }
        let (runtime, streams) = build_demo_runtime(session_config.settings.limits())?;
        headless(runtime, streams, session_config).await
    }
}

/// Cross-channel event merger with a finite reorder buffer.
///
/// The runtime assigns one contiguous per-run sequence across both bounded
/// channels, but frontends must not assume cross-channel arrival order. This
/// helper holds at most [`MAX_REORDER_EVENTS`] out-of-order events and yields
/// them by contiguous sequence until explicitly flushed or saturated. Unlike
/// authoritative consumers, this TUI-only best-effort presentation policy
/// skips missing sequences on saturation, counting gaps for `report_merger`.
/// Late predecessors are then rejected rather than reordered. Events for older
/// runs, duplicates, and anything arriving after the terminal outcome are
/// rejected; a finalized run never reopens.
#[derive(Debug)]
struct EventMerger(OrderedEventConsumer);

impl EventMerger {
    fn new() -> Self {
        Self(OrderedEventConsumer::new(MAX_REORDER_EVENTS))
    }

    /// Adopts the runtime-accepted run. Called with the `Submit` reply before
    /// any event for that run is merged; events observed before adoption are
    /// rejected rather than guessed into a run.
    fn adopt(&mut self, run: &RunId) {
        self.0.adopt(run);
    }

    fn active_run(&self) -> Option<&RunId> {
        self.0.active_run()
    }

    fn is_finalized(&self) -> bool {
        self.0.is_finalized()
    }

    /// True when the terminal event was received but is still waiting behind
    /// a reorder gap.
    fn has_terminal(&self) -> bool {
        self.0.has_terminal()
    }

    fn has_pending(&self) -> bool {
        self.0.has_pending()
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.0.pending_len()
    }

    fn gaps(&self) -> u64 {
        self.0.gaps()
    }

    fn rejected(&self) -> u64 {
        self.0.rejected()
    }

    /// Pushes one event, flushing counted gaps on presentation-buffer saturation.
    fn push(&mut self, event: RunEvent) -> Vec<RunEvent> {
        match self.0.push(event) {
            Ok(ready) => ready,
            Err(event) => {
                let mut ready = self.0.flush();
                ready.extend(
                    self.0
                        .push(*event)
                        .expect("flushed reorder buffer has room"),
                );
                ready
            }
        }
    }

    /// Emits every buffered event in sequence order, counting sequences that
    /// never arrived as explicit gaps. The terminal event is always last.
    fn flush(&mut self) -> Vec<RunEvent> {
        self.0.flush()
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
    /// The single process-owned user document. Reads take short snapshots;
    /// terminal usage updates and persistence are serialized through this owner.
    user_config: SharedUserConfig,
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
    execution: ExecutionMode,
    /// Queued `:session` intents in recording order: key handling appends on
    /// the active front, and the loop drains them against the registry after
    /// the batch, where runtimes can be built. At most one push per Enter
    /// keypress, drained every tick, so the queue stays tiny.
    pending_session_cmds: Vec<SessionArgs>,
    storage_busy: bool,
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
            user_config: shared_user_config(UserConfig::default_config(), None),
            active_model: None,
            run_models: HashMap::new(),
            demo: false,
            live: None,
            execution: ExecutionMode::Local,
            pending_session_cmds: Vec::new(),
            storage_busy: false,
            pointer_press: None,
            g_pending: false,
        }
    }

    /// Builds a frontend over the already-loaded startup configuration, so
    /// the interactive and headless paths share one load and one document.
    fn with_config(session: SessionId, config: SessionConfig) -> Self {
        let mut front = Self {
            user_config: config.user_config,
            active_model: config.active_model,
            run_models: HashMap::new(),
            demo: config.demo,
            ..Self::new(session)
        };
        front
            .state
            .set_settings(config.settings.tui().clone())
            .expect("validated startup preferences");
        front.sync_model_display();
        // Composer modes: built-ins first, then configuration customs, with
        // the configured default (or `build`) selected.
        let mut modes = vec![AgentMode::plan(), AgentMode::build()];
        let user_config = front.config_snapshot();
        modes.extend(user_config.modes().iter().cloned());
        front.state.set_modes(modes, user_config.default_mode());
        if let Some(dir) = session_project_dir() {
            front.state.set_project_dir(dir);
        }
        front
    }

    fn config_snapshot(&self) -> UserConfig {
        user_config_snapshot(&self.user_config)
    }

    /// Mirrors the current document and selection into the runtime's shared
    /// selection, so the next run resolves what the header shows. No-op
    /// without a production handle (unit tests).
    fn sync_live_binding(&mut self) {
        let config = self.config_snapshot();
        if let Some(binding) = &self.live {
            let mut binding = binding.lock().expect("live selection lockable");
            let pending_run = binding.pending_run.take();
            let pending_run_id = binding.pending_run_id.take();
            let provider_settings = binding.provider_settings.clone();
            *binding = LiveSelection {
                config,
                provider_settings,
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
    fn attach_live(
        &mut self,
        live: LiveHandle,
        tools_root: Option<&std::path::Path>,
        execution: ExecutionMode,
    ) {
        self.execution = execution;
        self.live = Some(live);
        self.sync_live_binding();
        let message = {
            let handle = self.live.as_ref().expect("live handle attached");
            let selection = handle.lock().expect("live selection lockable");
            wiring_notice(&selection, tools_root, execution)
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
        let config = self.config_snapshot();
        let ids: Vec<&str> = config
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
        let config = self.config_snapshot();
        if config.model(name).is_some() {
            self.active_model = Some(name.to_owned());
            self.sync_model_display();
            self.sync_live_binding();
            self.state.notice(&format!("model: {name}"));
        } else {
            let mut ids: Vec<&str> = config
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
        let config = self.config_snapshot();
        let choices: Vec<ModelChoice> = config
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
                    self.config_snapshot()
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
                    "session import".into(),
                    "Explicitly read and import the legacy session file".into(),
                ),
                (
                    "session delete ".into(),
                    "Delete an old saved session".into(),
                ),
                (
                    "session restore ".into(),
                    "Restore a saved session by ID".into(),
                ),
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
        let config = self.config_snapshot();
        let display = self.active_model.as_deref().and_then(|id| {
            config
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
        let save_failed = {
            let mut document = self
                .user_config
                .lock()
                .expect("user configuration lockable");
            if document.config.model(&id).is_none() {
                return;
            }
            // Keep mutation and writeback serialized so concurrent terminal
            // outcomes cannot persist stale snapshots over one another.
            if document.config.record_use(&id).is_err() {
                true
            } else if let Some(path) = &document.path {
                save_config(&document.config, path).is_err()
            } else {
                false
            }
        };
        if save_failed {
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

/// Copies according to the selected backend, without diagnostics. Auto tries
/// a platform helper before OSC 52; explicit backends never fall back.
/// Disabled mode and empty text never invoke either backend.
fn yank_to_clipboard(text: &str, mode: nexus_config::tui_settings::ClipboardMode) -> bool {
    if text.is_empty() {
        return false;
    }
    copy_with(text, mode, write_clipboard_helper, write_osc52)
}

fn copy_with(
    text: &str,
    mode: nexus_config::tui_settings::ClipboardMode,
    system: impl FnOnce(&str) -> bool,
    osc52: impl FnOnce(&str) -> bool,
) -> bool {
    use nexus_config::tui_settings::ClipboardMode;
    if text.is_empty() {
        return false;
    }
    match mode {
        ClipboardMode::Auto => system(text) || osc52(text),
        ClipboardMode::System => system(text),
        ClipboardMode::Osc52 => osc52(text),
        ClipboardMode::Disabled => false,
    }
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
    if !front.state.settings().input.mouse_capture {
        return false;
    }
    let wheel_lines = front.state.settings().input.scroll_lines;
    // The mouse never completes a `:` command: a press disarms it so a
    // later letter cannot fire behind a click.
    if matches!(mouse.kind, MouseEventKind::Down(_)) {
        front.state.clear_colon();
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            front.pointer_press = None;
            if front.state.approval_detail_open() {
                front.state.approval_detail_scroll(-(wheel_lines as isize));
            } else {
                front.state.scroll_up(wheel_lines);
            }
            true
        }
        MouseEventKind::ScrollDown => {
            front.pointer_press = None;
            if front.state.approval_detail_open() {
                front.state.approval_detail_scroll(wheel_lines as isize);
            } else {
                front.state.scroll_down(wheel_lines);
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
            if !front.state.settings().input.copy_on_select {
                return true;
            }
            let copied = yank_to_clipboard(&text, front.state.settings().input.clipboard);
            if !copied {
                return true;
            }
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
        SessionArgs::Pick => {
            let choices = live_session_choices(sessions);
            sessions
                .active_mut()
                .front
                .state
                .open_session_picker(choices);
            false
        }
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
        SessionArgs::Import => {
            sessions
                .active_mut()
                .front
                .state
                .notice("import requires the persistent interactive session store");
            false
        }
        SessionArgs::Restore(_) => {
            sessions
                .active_mut()
                .front
                .state
                .notice("restore requires the persistent interactive session store");
            false
        }
        SessionArgs::Delete(_) => {
            sessions
                .active_mut()
                .front
                .state
                .notice("delete requires the persistent interactive session store");
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

fn live_session_choices(sessions: &SessionRegistry) -> Vec<nexus_tui::state::SessionChoice> {
    sessions
        .slots
        .iter()
        .enumerate()
        .map(|(index, slot)| nexus_tui::state::SessionChoice {
            id: slot.front.session.as_str().to_owned(),
            label: format!(
                "{}{} · {}",
                slot.label,
                if index == sessions.active {
                    " [active]"
                } else {
                    ""
                },
                slot.front.state.status()
            ),
            saved: false,
        })
        .collect()
}

async fn execute_session_command(sessions: &mut SessionRegistry, args: &SessionArgs) {
    sessions.poll_maintenance();
    if sessions.maintenance.is_some()
        && matches!(
            args,
            SessionArgs::Import
                | SessionArgs::Delete(_)
                | SessionArgs::List
                | SessionArgs::Restore(_)
                | SessionArgs::Pick
        )
    {
        sessions.active_mut().front.state.notice("session maintenance is still running; no additional storage command was started; quitting does not roll it back");
        return;
    }
    match args {
        SessionArgs::Pick => {
            let mut choices = live_session_choices(sessions);
            if sessions.store.is_some() {
                let request = sessions.active_mut().front.next_request();
                let reply = sessions
                    .active()
                    .runtime
                    .handle(nexus_core::Command::ListSessions(
                        ListSessionsCommand::new(request, nexus_core::commands::MAX_LIST_LIMIT)
                            .expect("finite session list bound is valid"),
                    ))
                    .await
                    .0;
                if reply.reply() == CommandReply::Accepted && reply.has_sessions() {
                    for entry in reply.sessions() {
                        if choices
                            .iter()
                            .any(|choice| choice.id == entry.session.as_str())
                        {
                            continue;
                        }
                        choices.push(nexus_tui::state::SessionChoice {
                            id: entry.session.as_str().to_owned(),
                            label: format!("saved · revision {}", entry.logical_revision),
                            saved: true,
                        });
                    }
                } else {
                    sessions
                        .active_mut()
                        .front
                        .state
                        .notice("saved session listing failed; showing live sessions only");
                }
            }
            sessions
                .active_mut()
                .front
                .state
                .open_session_picker(choices);
        }
        SessionArgs::Import if sessions.store.is_some() => {
            let store = sessions.store.as_ref().expect("guarded above").clone();
            sessions
                .active_mut()
                .front
                .state
                .notice("explicitly reading the legacy session file to import saved sessions in background; quitting does not roll it back");
            let (sender, receiver) = tokio::sync::oneshot::channel();
            sessions.maintenance = Some((sessions.active().front.session.clone(), receiver));
            tokio::task::spawn_blocking(move || {
                let message = match store.import_legacy_sessions() {
                    Ok(count) => format!(
                        "imported {count} legacy sessions; use :session list, then :session restore <id>"
                    ),
                    Err(error) => format!(
                        "legacy session import failed: {error}; it may be partially complete; restart the TUI to reconcile storage before retrying"
                    ),
                };
                let _ = sender.send(message);
            });
        }
        SessionArgs::Delete(id) if sessions.store.is_some() => {
            let active = sessions
                .slots
                .iter()
                .any(|slot| slot.front.session.as_str() == id);
            if active {
                sessions.active_mut().front.state.notice(
                    "cannot delete a session open in this TUI; restart the TUI before deleting it",
                );
                return;
            }
            let session = match SessionId::new(id.clone()) {
                Ok(session) => session,
                Err(_) => {
                    sessions
                        .active_mut()
                        .front
                        .state
                        .notice("invalid session id");
                    return;
                }
            };
            let store = sessions.store.as_ref().expect("guarded above").clone();
            let (sender, receiver) = tokio::sync::oneshot::channel();
            sessions.maintenance = Some((sessions.active().front.session.clone(), receiver));
            sessions.active_mut().front.state.notice(
                "deleting saved session in background; quitting does not roll back this operation",
            );
            let id = id.clone();
            tokio::task::spawn_blocking(move || {
                let message = match store.delete_session(&session) {
                    Ok(()) => format!("deleted saved session {id}"),
                    Err(error) => format!(
                        "could not delete saved session {id}: {error}; restart the TUI to reconcile storage before retrying"
                    ),
                };
                let _ = sender.send(message);
            });
        }
        SessionArgs::List if sessions.store.is_some() => {
            for line in sessions.describe() {
                sessions.active_mut().front.state.notice(&line);
            }
            let request = sessions.active_mut().front.next_request();
            let reply = sessions
                .active()
                .runtime
                .handle(nexus_core::Command::ListSessions(
                    ListSessionsCommand::new(request, nexus_core::commands::MAX_LIST_LIMIT)
                        .expect("finite session list bound is valid"),
                ))
                .await
                .0;
            if reply.reply() == CommandReply::Accepted && reply.has_sessions() {
                if reply.sessions().is_empty() {
                    sessions
                        .active_mut()
                        .front
                        .state
                        .notice("no saved sessions");
                } else {
                    for entry in reply.sessions() {
                        if sessions
                            .slots
                            .iter()
                            .any(|slot| slot.front.session == entry.session)
                        {
                            continue;
                        }
                        sessions.active_mut().front.state.notice(&format!(
                            "saved {} (revision {})",
                            entry.session.as_str(),
                            entry.logical_revision
                        ));
                    }
                }
            } else {
                sessions
                    .active_mut()
                    .front
                    .state
                    .notice("saved session listing failed");
            }
        }
        SessionArgs::Restore(id) => restore_persisted_session(sessions, id).await,
        _ => {
            handle_session_command(sessions, args);
        }
    }
}

async fn restore_persisted_session(sessions: &mut SessionRegistry, id: &str) {
    if let Some(index) = sessions.resolve_target(id) {
        if sessions.switch(index) {
            let label = sessions.active().label.clone();
            sessions
                .active_mut()
                .front
                .state
                .notice(&format!("switched to already loaded session {label}"));
        }
        return;
    }
    if sessions.store.is_none() {
        sessions
            .active_mut()
            .front
            .state
            .notice("persistent session storage is unavailable");
        return;
    }
    if sessions.len() >= MAX_SESSIONS {
        sessions
            .active_mut()
            .front
            .state
            .notice("session limit reached (16 live sessions)");
        return;
    }
    let session = match SessionId::new(id) {
        Ok(session) => session,
        Err(_) => {
            sessions
                .active_mut()
                .front
                .state
                .notice("saved session id is invalid");
            return;
        }
    };
    let config = sessions.template.clone();
    let store = sessions.store.clone();
    let (runtime, streams, live) = match build_live_runtime_with_store(&config, store) {
        Ok(runtime) => runtime,
        Err(_) => {
            sessions
                .active_mut()
                .front
                .state
                .notice("session runtime could not be created");
            return;
        }
    };
    let request = RequestId::new(format!(
        "req-restore-{}",
        SESSION_NONCE.fetch_add(1, Ordering::SeqCst)
    ));
    let Ok(request) = request else {
        sessions
            .active_mut()
            .front
            .state
            .notice("session restore request could not be created");
        return;
    };
    let reply = runtime
        .handle(nexus_core::Command::RestoreSession(RestoreSessionCommand {
            request,
            session: session.clone(),
        }))
        .await
        .0;
    if reply.reply() != CommandReply::Accepted {
        sessions
            .active_mut()
            .front
            .state
            .notice("saved session could not be restored");
        return;
    }
    let number = sessions.slots.len() + 1;
    let label = format!("s{number}");
    let mut front = Frontend::with_config(session, config.clone());
    front.state.set_session_label(&label);
    front
        .state
        .record_restored_history(reply.restored_history());
    front.attach_live(live, config.tools_root.as_deref(), config.execution);
    if reply.restored_omitted_items() > 0 {
        front.state.notice(&format!(
            "restored recent history; {} older context items omitted",
            reply.restored_omitted_items()
        ));
    }
    front.state.notice(
        "saved history is loaded into model context; interrupted calls remain uncertain and are not replayed",
    );
    sessions.slots.push(SessionSlot {
        label,
        runtime,
        streams,
        front,
    });
    sessions.active = sessions.slots.len() - 1;
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
        KeyCode::Enter if front.state.session_picker_open() => {
            let matches = front.state.picker_matches();
            let choice = front
                .state
                .picker_confirm(matches.len())
                .and_then(|cursor| matches.get(cursor))
                .and_then(|index| front.state.session_choices().get(*index))
                .cloned();
            front.state.dismiss_overlay();
            if let Some(choice) = choice {
                front.pending_session_cmds.push(if choice.saved {
                    SessionArgs::Restore(choice.id)
                } else {
                    SessionArgs::Switch(choice.id)
                });
            }
        }
        KeyCode::Enter => confirm_model_picker(front),
        KeyCode::Up => move_picker(front, -1),
        KeyCode::Down => move_picker(front, 1),
        KeyCode::PageUp => {
            let rows = front.state.picker_page_rows() as isize;
            move_picker(front, -rows);
        }
        KeyCode::PageDown => {
            let rows = front.state.picker_page_rows() as isize;
            move_picker(front, rows);
        }
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
    handle_key_with_runtime(front, Some(runtime), key).await
}

async fn handle_key_with_runtime(
    front: &mut Frontend,
    runtime: Option<&Runtime>,
    key: KeyEvent,
) -> bool {
    if key.kind == KeyEventKind::Press
        && key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
        && key.code == crossterm::event::KeyCode::Char('s')
        && !front.state.overlay_open()
    {
        front.pending_session_cmds.push(SessionArgs::Pick);
        return false;
    }
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
            // The normal UI is available during container preparation, but
            // no task may be dispatched until its real runtime is ready.
            let Some(runtime) = runtime else {
                return false;
            };
            if front.storage_busy {
                front.state.notice("session maintenance is still running; draft preserved; submit after it completes");
                return false;
            }
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
            if front.execution == ExecutionMode::Container {
                front
                    .state
                    .notice("mode switching is unavailable in container execution");
            } else {
                front.state.cycle_mode();
            }
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
            let Some(runtime) = runtime else {
                return false;
            };
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
                    let copied = yank_to_clipboard(&text, front.state.settings().input.clipboard);
                    front.state.clear_text_selection();
                    if copied {
                        front
                            .state
                            .set_toast(format!("copied {} chars", text.chars().count()), TOAST_TTL);
                    }
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
                    let Some(runtime) = runtime else {
                        return false;
                    };
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
async fn interactive(mut session_config: SessionConfig) -> io::Result<InteractiveReport> {
    // Demo/PTY tests remain isolated from the user's local state directory.
    if !session_config.demo {
        session_config.diagnostics = Some(diagnostics::DiagnosticLog::open_default()?);
    }
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).map_err(io::Error::other)?;
    let session_store = if session_config.demo {
        None
    } else {
        Some(application_session_store()?)
    };
    let session = if session_config.demo {
        SessionId::new(DEMO_SESSION).map_err(|_| io::Error::other("demo session id rejected"))?
    } else {
        new_tui_session_id()?
    };
    let mut front = Frontend::with_config(session, session_config.clone());
    front.execution = session_config.execution;
    front.state.set_session_label("s1");
    #[cfg(feature = "container")]
    let (runtime, streams, live) = if session_config.execution == ExecutionMode::Container {
        let Some((runtime, resolved_config)) = prepare_live_runtime_with_progress(
            session_config.clone(),
            session_store.clone(),
            &mut terminal,
            &mut front,
        )
        .await?
        else {
            return Ok(InteractiveReport {
                result: Ok(()),
                unresolved_blocking: false,
            });
        };
        session_config = resolved_config;
        session_config.active_model = front.active_model.clone();
        front
            .state
            .set_toast("Container ready".to_owned(), TOAST_TTL);
        runtime
    } else {
        build_live_runtime_with_store(&session_config, session_store.clone())?
    };
    #[cfg(not(feature = "container"))]
    let (runtime, streams, live) =
        build_live_runtime_with_store(&session_config, session_store.clone())?;
    if !session_config.demo
        && nexus_session::default_data_dir()
            .is_some_and(|directory| directory.join("session-store.json").exists())
    {
        front.state.notice(
            "legacy session file detected (not read); :session import explicitly reads it to import saved sessions",
        );
    }
    front.attach_live(
        live,
        session_config.tools_root.as_deref(),
        session_config.execution,
    );
    let demo = session_config.demo;
    let mut sessions = SessionRegistry::new_with_store(
        SessionSlot {
            label: "s1".to_owned(),
            runtime,
            streams,
            front,
        },
        session_config,
        session_store,
    );
    let mut gate = RefreshGate::new(Duration::from_millis(
        sessions
            .active()
            .front
            .state
            .settings()
            .render
            .redraw_interval_ms,
    ));
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
    sessions.poll_maintenance();
    let mut unresolved_blocking = sessions.maintenance.is_some();
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

#[cfg(feature = "container")]
fn resolve_container_images(session_config: &mut SessionConfig) -> io::Result<()> {
    let resolve = |image: Option<String>, label: &str| -> io::Result<String> {
        let image = image.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("container mode requires {label} image; set it in user config or pass its image flag"),
            )
        })?;
        nexus_container::resolve_local_image(&image).map_err(|error| {
            io::Error::other(format!(
                "{label} image resolution failed: {error}; check Docker on PATH, the local default context, daemon availability, and the installed image reference (no image was pulled)"
            ))
        })
    };
    session_config.gateway_image = Some(resolve(session_config.gateway_image.clone(), "gateway")?);
    session_config.workload_image =
        Some(resolve(session_config.workload_image.clone(), "workload")?);
    Ok(())
}

#[cfg(feature = "container")]
async fn prepare_live_runtime_with_progress(
    mut session_config: SessionConfig,
    session_store: Option<SessionStoreHandle>,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    front: &mut Frontend,
) -> io::Result<Option<((Runtime, EventStreams, LiveHandle), SessionConfig)>> {
    prepare_container_with_progress(
        move || {
            resolve_container_images(&mut session_config)?;
            let runtime = build_live_runtime_with_store(&session_config, session_store)?;
            Ok((runtime, session_config))
        },
        front,
        startup_read_input,
        |front, message| {
            front.state.set_toast(message.to_owned(), TOAST_TTL);
            front.state.tick_caret();
            draw(terminal, front)
        },
    )
    .await
}

// Keep Docker and terminal I/O at the boundary so lifecycle failures can be
// exercised with a controlled resource owner and without a real daemon or TTY.
#[cfg(feature = "container")]
async fn prepare_container_with_progress<T: Send + 'static, U>(
    prepare: impl FnOnce() -> io::Result<T> + Send + 'static,
    ui: &mut U,
    mut read_input: impl AsyncFnMut(&mut U) -> io::Result<bool>,
    mut draw_progress: impl FnMut(&mut U, &str) -> io::Result<()>,
) -> io::Result<Option<T>> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("container-startup".to_owned())
        .spawn(move || {
            let _ = sender.send(prepare());
        })
        .map_err(io::Error::other)?;

    let mut quit_requested = false;
    let mut terminal_error = None;
    loop {
        if terminal_error.is_none() && !quit_requested {
            match read_input(ui).await {
                Ok(quit) => quit_requested = quit,
                Err(error) => {
                    terminal_error = Some(error);
                    quit_requested = true;
                }
            }
        } else if terminal_error.is_none() {
            // Continue draining input while initialization finishes; never
            // let queued keystrokes become the first task after startup.
            if let Err(error) = read_input(ui).await {
                terminal_error = Some(error);
            }
        }

        match receiver.try_recv() {
            Ok(Ok(runtime)) if quit_requested || terminal_error.is_some() => {
                worker.join().map_err(|_| {
                    startup_error_with_terminal_context(
                        terminal_error.take(),
                        io::Error::other("container startup worker panicked"),
                    )
                })?;
                if terminal_error.is_none()
                    && let Err(error) = draw_progress(
                        ui,
                        "Quit requested. Waiting for container startup to finish safely…",
                    )
                {
                    terminal_error = Some(error);
                }
                let cleanup = std::thread::Builder::new()
                    .name("container-startup-cleanup".to_owned())
                    .spawn(move || drop(runtime))
                    .map_err(|error| {
                        startup_error_with_terminal_context(terminal_error.take(), error)
                    })?;
                while !cleanup.is_finished() {
                    if terminal_error.is_none()
                        && let Err(error) =
                            draw_progress(ui, "Quit requested. Cleaning up the prepared container…")
                    {
                        terminal_error = Some(error);
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                if cleanup.join().is_err() {
                    return Err(startup_error_with_terminal_context(
                        terminal_error,
                        io::Error::other("container startup cleanup thread panicked"),
                    ));
                }
                if let Some(error) = terminal_error {
                    return Err(error);
                }
                return Ok(None);
            }
            Ok(result) => {
                worker.join().map_err(|_| {
                    startup_error_with_terminal_context(
                        terminal_error.take(),
                        io::Error::other("container startup worker panicked"),
                    )
                })?;
                return result
                    .map(Some)
                    .map_err(|error| startup_error_with_terminal_context(terminal_error, error));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                let _ = worker.join();
                return Err(startup_error_with_terminal_context(
                    terminal_error,
                    io::Error::other("container startup worker exited without a result"),
                ));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }

        let message = if quit_requested {
            "Quit requested. Waiting for container startup to finish safely…"
        } else {
            "Preparing container · Enter to submit is disabled"
        };
        if terminal_error.is_none()
            && let Err(error) = draw_progress(ui, message)
        {
            terminal_error = Some(error);
            quit_requested = true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(feature = "container")]
fn startup_error_with_terminal_context(
    terminal_error: Option<io::Error>,
    startup_error: io::Error,
) -> io::Error {
    match terminal_error {
        Some(terminal_error) => io::Error::new(
            startup_error.kind(),
            format!("{startup_error}; terminal I/O also failed: {terminal_error}"),
        ),
        None => startup_error,
    }
}

#[cfg(feature = "container")]
async fn startup_read_input(front: &mut Frontend) -> io::Result<bool> {
    // Drain queued startup keys before readiness is checked so an Enter
    // pressed during preparation cannot become a submission after handoff.
    while poll(Duration::ZERO)? {
        match read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if handle_key_with_runtime(front, None, key).await {
                    return Ok(true);
                }
            }
            Event::Mouse(mouse) => {
                handle_mouse(front, mouse);
            }
            Event::Paste(text) => {
                for character in text.chars() {
                    if character == '\n' {
                        front.state.composer_newline();
                    } else {
                        front.state.composer_type(character);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(false)
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
        if sessions
            .template
            .diagnostics
            .as_ref()
            .is_some_and(|log| log.take_failure())
        {
            sessions.active_mut().front.state.notice(
                "local runtime diagnostic write failed; diagnostics may be missing (redirected stderr remains a fallback)",
            );
            dirty = true;
        }
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
                dirty |= sessions.poll_maintenance();
                let storage_busy = sessions.maintenance.is_some();
                let batch = {
                    let slot = sessions.active_mut();
                    slot.front.storage_busy = storage_busy;
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
                    execute_session_command(sessions, cmd).await;
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
    fn reorder_buffer_holds_a_full_runtime_backlog_without_skipping_gaps() {
        let run = run_id("run-3");
        let mut merger = EventMerger::new();
        merger.adopt(&run);
        let mut emitted = Vec::new();
        for seq in 1..=MAX_REORDER_EVENTS as u64 {
            emitted.extend(merger.push(text(&run, seq, "x")));
        }
        assert_eq!(merger.pending_len(), MAX_REORDER_EVENTS);
        emitted.extend(merger.push(text(&run, 0, "first")));
        assert_eq!(merger.gaps(), 0, "a delayed predecessor is not a gap");
        let seqs: Vec<u64> = emitted.iter().map(|event| event.seq()).collect();
        assert_eq!(seqs.len(), MAX_REORDER_EVENTS + 1);
        assert_eq!(seqs.first(), Some(&0));
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
    fn saturated_presentation_window_counts_gaps_and_rejects_late_predecessors() {
        let run = run_id("run-saturated");
        let mut merger = EventMerger(OrderedEventConsumer::new(2));
        merger.adopt(&run);
        assert!(merger.push(text(&run, 1, "one")).is_empty());
        assert!(merger.push(text(&run, 2, "two")).is_empty());

        assert_eq!(seqs(&merger.push(finished(&run, 3))), vec![1, 2, 3]);
        assert_eq!(merger.gaps(), 1);
        assert!(merger.is_finalized());
        assert!(!merger.has_pending());
        assert!(merger.push(text(&run, 0, "late predecessor")).is_empty());
        assert_eq!(merger.rejected(), 1);
        assert_eq!(merger.gaps(), 1);
    }

    #[test]
    fn reorder_window_holds_the_runtime_backlog_until_the_gap_arrives() {
        let run = run_id("run-window");
        let mut merger = EventMerger::new();
        merger.adopt(&run);

        for seq in 1..=MAX_REORDER_EVENTS as u64 {
            assert!(merger.push(text(&run, seq, "x")).is_empty());
            assert!(
                merger.pending_len() <= MAX_REORDER_EVENTS,
                "the reorder window is finite"
            );
        }
        assert_eq!(
            merger.pending_len(),
            MAX_REORDER_EVENTS,
            "a full window still holds every buffered event"
        );

        // A full reorder buffer is not evidence that the gap is lost: a
        // predecessor may still be queued on the other bounded channel.
        let emitted = seqs(&merger.push(text(&run, 0, "first")));
        assert_eq!(emitted.len(), MAX_REORDER_EVENTS + 1);
        assert_eq!(emitted.first(), Some(&0));
        assert_eq!(emitted.last(), Some(&(MAX_REORDER_EVENTS as u64)));
        assert!(
            emitted.windows(2).all(|pair| pair[0] < pair[1]),
            "emission stays strictly ordered when the gap is filled"
        );
        assert_eq!(merger.gaps(), 0);
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

#[cfg(all(test, feature = "container"))]
mod container_startup_tests {
    use super::*;
    use std::future::{Future, poll_fn};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::Poll;

    #[tokio::test]
    async fn readiness_preserves_the_frontend_and_enables_explicit_submit() {
        use crossterm::event::{KeyCode, KeyModifiers};

        let (release_prepare, prepare_gate) = std::sync::mpsc::channel();
        let mut release_prepare = Some(release_prepare);
        let mut front = Frontend::new(SessionId::new("startup-ui").unwrap());
        let mut typed = false;
        let prepared = prepare_container_with_progress(
            move || {
                prepare_gate.recv_timeout(Duration::from_secs(5)).unwrap();
                build_runtime()
            },
            &mut front,
            async |front| {
                if !typed {
                    typed = true;
                    for code in [KeyCode::Char('q'), KeyCode::Char('u'), KeyCode::Enter] {
                        assert!(
                            !handle_key_with_runtime(
                                front,
                                None,
                                KeyEvent::new(code, KeyModifiers::NONE)
                            )
                            .await
                        );
                    }
                    front.state.caret_left();
                    front.state.open_help();
                }
                Ok(false)
            },
            |front, message| {
                assert!(front.state.help_open());
                assert_eq!(front.state.composer(), "qu");
                front.state.set_toast(message.to_owned(), TOAST_TTL);
                let area = ratatui::layout::Rect::new(0, 0, 100, 30);
                let mut buffer = ratatui::buffer::Buffer::empty(area);
                render(&mut front.state, area, &mut buffer, front.focus);
                let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
                assert!(text.contains("Nexus Agent"));
                assert!(!text.contains("Container startup"));
                if let Some(release) = release_prepare.take() {
                    release.send(()).unwrap();
                }
                Ok(())
            },
        )
        .await
        .unwrap();
        let (runtime, _streams) = prepared.expect("preparation succeeds without quitting");
        assert_eq!(front.state.composer(), "qu");
        assert!(front.state.help_open());
        assert_eq!(front.focus, Focus::Composer);
        assert_eq!(front.request_counter, 0, "readiness never auto-submits");
        assert!(front.state.active_run().is_none());
        front.state.dismiss_overlay();
        front.state.composer_type('x');
        assert_eq!(front.state.composer(), "qxu", "caret position is retained");
        assert!(
            !handle_key(
                &mut front,
                &runtime,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            )
            .await
        );
        assert_eq!(front.request_counter, 1);
        assert!(front.state.composer().is_empty());
    }

    struct Resource {
        cleanup_started: Option<tokio::sync::oneshot::Sender<()>>,
        release_cleanup: std::sync::mpsc::Receiver<()>,
        cleaned: Arc<AtomicBool>,
    }

    impl Drop for Resource {
        fn drop(&mut self) {
            let _ = self.cleanup_started.take().unwrap().send(());
            self.release_cleanup
                .recv_timeout(Duration::from_secs(5))
                .expect("test must release cleanup");
            self.cleaned.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn startup_io_failures_wait_for_preparation_and_cleanup() {
        for fail_input in [true, false] {
            let (release_prepare, prepare_gate) = std::sync::mpsc::channel();
            let (cleanup_started, cleanup_observed) = tokio::sync::oneshot::channel();
            let (release_cleanup, cleanup_gate) = std::sync::mpsc::channel();
            let cleaned = Arc::new(AtomicBool::new(false));
            let resource = Resource {
                cleanup_started: Some(cleanup_started),
                release_cleanup: cleanup_gate,
                cleaned: Arc::clone(&cleaned),
            };
            let mut input_calls = 0;
            let mut draw_calls = 0;
            let mut ui = ();
            let mut startup = Box::pin(prepare_container_with_progress(
                move || {
                    prepare_gate.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(resource)
                },
                &mut ui,
                async |_| {
                    input_calls += 1;
                    assert_eq!(input_calls, 1, "input stops after terminal failure");
                    if fail_input {
                        Err(io::Error::other("input failed"))
                    } else {
                        Ok(false)
                    }
                },
                |_, _| {
                    draw_calls += 1;
                    assert_eq!(draw_calls, 1, "drawing stops after terminal failure");
                    Err(io::Error::other("draw failed"))
                },
            ));

            poll_fn(|cx| {
                assert!(startup.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            assert!(!cleaned.load(Ordering::SeqCst));
            release_prepare.send(()).unwrap();
            tokio::select! {
                _ = &mut startup => panic!("startup returned before cleanup completed"),
                started = cleanup_observed => started.unwrap(),
            }
            poll_fn(|cx| {
                assert!(startup.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            assert!(!cleaned.load(Ordering::SeqCst));
            release_cleanup.send(()).unwrap();
            let error = match startup.await {
                Err(error) => error,
                Ok(_) => panic!("terminal error was lost"),
            };
            assert_eq!(
                error.to_string(),
                if fail_input {
                    "input failed"
                } else {
                    "draw failed"
                }
            );
            assert!(cleaned.load(Ordering::SeqCst));
            assert_eq!(draw_calls, usize::from(!fail_input));
        }
    }

    #[tokio::test]
    async fn cleanup_draw_failure_still_waits_for_resource_drop() {
        let (cleanup_started, cleanup_observed) = tokio::sync::oneshot::channel();
        let (release_cleanup, cleanup_gate) = std::sync::mpsc::channel();
        let (draw_failed, failure_observed) = tokio::sync::oneshot::channel();
        let mut draw_failed = Some(draw_failed);
        let cleaned = Arc::new(AtomicBool::new(false));
        let resource = Resource {
            cleanup_started: Some(cleanup_started),
            release_cleanup: cleanup_gate,
            cleaned: Arc::clone(&cleaned),
        };
        let mut ui = ();
        let mut startup = Box::pin(prepare_container_with_progress(
            move || Ok(resource),
            &mut ui,
            async |_| Ok(true),
            |_, message| {
                assert!(draw_failed.is_some(), "drawing stops after failure");
                if message.contains("Cleaning up") {
                    draw_failed.take().unwrap().send(()).unwrap();
                    Err(io::Error::other("cleanup draw failed"))
                } else {
                    Ok(())
                }
            },
        ));
        tokio::select! {
            _ = &mut startup => panic!("startup returned before cleanup completed"),
            failed = failure_observed => failed.unwrap(),
        }
        cleanup_observed.await.unwrap();
        poll_fn(|cx| {
            assert!(startup.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert!(!cleaned.load(Ordering::SeqCst));
        release_cleanup.send(()).unwrap();
        match startup.await {
            Err(error) => assert_eq!(error.to_string(), "cleanup draw failed"),
            Ok(_) => panic!("terminal error was lost"),
        }
        assert!(cleaned.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn startup_failure_after_input_failure_preserves_both_errors() {
        let (release_prepare, prepare_gate) = std::sync::mpsc::channel();
        let mut ui = ();
        let mut startup = Box::pin(prepare_container_with_progress::<(), _>(
            move || {
                prepare_gate.recv_timeout(Duration::from_secs(5)).unwrap();
                Err(io::Error::new(io::ErrorKind::NotFound, "image missing"))
            },
            &mut ui,
            async |_| Err(io::Error::other("input failed")),
            |_, _| panic!("must not draw after input failure"),
        ));
        poll_fn(|cx| {
            assert!(startup.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        release_prepare.send(()).unwrap();
        let error = startup.await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(
            error.to_string(),
            "image missing; terminal I/O also failed: input failed"
        );
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

    #[cfg(feature = "container")]
    #[tokio::test]
    async fn container_startup_uses_normal_keys_but_cannot_submit() {
        let mut front = Frontend::new(session());
        front.execution = ExecutionMode::Container;
        for code in [KeyCode::Char('q'), KeyCode::Char('u'), KeyCode::Enter] {
            assert!(
                !handle_key_with_runtime(&mut front, None, KeyEvent::new(code, KeyModifiers::NONE))
                    .await
            );
        }
        assert_eq!(front.state.composer(), "qu");
        assert_eq!(front.request_counter, 0);
        assert!(front.state.active_run().is_none());
        assert!(front.live.is_none());
        assert!(front.state.transcript().is_empty());

        assert!(
            !handle_key_with_runtime(
                &mut front,
                None,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)
            )
            .await
        );
        assert_eq!(front.state.composer(), "qu\n");
        assert!(
            !handle_key_with_runtime(
                &mut front,
                None,
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
            )
            .await
        );
        assert_eq!(front.focus, Focus::Viewport);
        for code in [
            KeyCode::Char(':'),
            KeyCode::Char('h'),
            KeyCode::Char('e'),
            KeyCode::Char('l'),
            KeyCode::Char('p'),
            KeyCode::Enter,
        ] {
            assert!(
                !handle_key_with_runtime(&mut front, None, KeyEvent::new(code, KeyModifiers::NONE))
                    .await
            );
        }
        assert!(front.state.help_open());
        assert_eq!(front.state.composer(), "qu\n");
    }

    #[cfg(feature = "container")]
    #[test]
    fn container_startup_failure_keeps_terminal_io_context() {
        let error = startup_error_with_terminal_context(
            Some(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "terminal draw failed",
            )),
            io::Error::new(io::ErrorKind::NotFound, "image missing"),
        );
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(
            error.to_string(),
            "image missing; terminal I/O also failed: terminal draw failed"
        );

        let startup_only = startup_error_with_terminal_context(
            None,
            io::Error::new(io::ErrorKind::NotFound, "image missing"),
        );
        assert_eq!(startup_only.to_string(), "image missing");
    }

    #[test]
    fn interactive_runtime_raises_model_turns_and_run_budget() {
        let limits = tui_runtime_limits();
        assert_eq!(limits.max_model_turns_per_run, 64);
        assert_eq!(limits.max_tool_calls_per_run, 128);
        assert_eq!(limits.run_duration, Duration::from_secs(3_600));
        assert_eq!(limits.per_tool_timeout, Duration::from_secs(600));
        assert_eq!(limits.retained_context_items, 512);
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
            diagnostics: None,
            user_config: shared_user_config(UserConfig::default_config(), None),
            settings: nexus_config::load_settings(nexus_config::SettingsProfile::Tui, None, &[])
                .unwrap(),
            active_model: None,
            tools_root: None,
            demo: false,
            strict_tools: true,
            execution: ExecutionMode::Local,
            gateway_image: None,
            workload_image: None,
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
            diagnostics: None,
            user_config: shared_user_config(config, None),
            settings: nexus_config::load_settings(nexus_config::SettingsProfile::Tui, None, &[])
                .unwrap(),
            active_model,
            tools_root: None,
            demo: false,
            strict_tools: true,
            execution: ExecutionMode::Local,
            gateway_image: None,
            workload_image: None,
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
            store: None,
            maintenance: None,
        }
    }

    /// A one-slot registry over the empty startup document.
    fn registry() -> SessionRegistry {
        let template = defaults_config();
        let first = slot("s1", "sess-test-1", &template);
        SessionRegistry::new(first, template)
    }

    #[tokio::test]
    async fn session_picker_shortcut_filters_and_switches_without_losing_drafts() {
        let mut sessions = registry();
        sessions.active_mut().front.state.composer_type('a');
        let index = sessions.create().unwrap();
        sessions.switch(index);
        sessions.active_mut().front.state.composer_type('b');
        let key = KeyEvent::new(
            crossterm::event::KeyCode::Char('s'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        let slot = sessions.active_mut();
        assert!(!handle_key_with_runtime(&mut slot.front, Some(&slot.runtime), key).await);
        assert_eq!(
            slot.front.pending_session_cmds.pop(),
            Some(SessionArgs::Pick)
        );
        execute_session_command(&mut sessions, &SessionArgs::Pick).await;
        let front = &mut sessions.active_mut().front;
        assert!(front.state.session_picker_open());
        assert_eq!(front.state.session_choices().len(), 2);
        front.state.picker_push('1');
        assert_eq!(front.state.picker_matches(), vec![0]);
        handle_overlay_key(
            front,
            KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
        );
        let command = front.pending_session_cmds.pop().unwrap();
        assert_eq!(command, SessionArgs::Switch("sess-test-1".to_owned()));
        execute_session_command(&mut sessions, &command).await;
        assert_eq!(sessions.active, 0);
        assert_eq!(sessions.active().front.state.composer(), "a");
        assert_eq!(sessions.slots[1].front.state.composer(), "b");
    }

    fn session_store_test_directory() -> std::path::PathBuf {
        let nonce = SESSION_NONCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "nexus-tui-session-commands-{}-{nonce}",
            std::process::id()
        ))
    }

    fn saved_checkpoint(id: &str) -> nexus_core::SessionCheckpoint {
        nexus_core::SessionCheckpoint::new(
            SessionId::new(id).expect("valid saved session id"),
            nexus_core::STORE_FORMAT_REVISION,
            1,
            vec![nexus_core::store::StoredMessage {
                source: "user".to_owned(),
                text: "saved conversation".to_owned(),
                complete: true,
            }],
            DEMO_PROFILE,
        )
        .expect("valid saved checkpoint")
    }

    #[tokio::test]
    async fn session_picker_restores_saved_history_and_deduplicates_live_sessions() {
        use nexus_core::SessionStore;

        let directory = session_store_test_directory();
        let mut store = nexus_session::DirectorySessionStore::open(&directory).unwrap();
        let checkpoint = saved_checkpoint("sess-saved-picker");
        store.initialize_session(&checkpoint).unwrap();
        store
            .append_source_items(
                checkpoint.session(),
                &RunId::new("run-saved-picker").unwrap(),
                &[
                    nexus_core::provider::ModelContextItem::user_text("saved conversation")
                        .unwrap(),
                ],
            )
            .unwrap();
        let store = SessionStoreHandle::new(Box::new(store));
        let mut sessions = registry();
        let (runtime, streams, _) =
            build_live_runtime_with_store(&sessions.template, Some(store.clone())).unwrap();
        sessions.slots[0].runtime = runtime;
        sessions.slots[0].streams = streams;
        sessions.store = Some(store);
        execute_session_command(&mut sessions, &SessionArgs::Pick).await;
        let front = &mut sessions.active_mut().front;
        assert_eq!(front.state.session_choices().len(), 2);
        for character in "saved-picker".chars() {
            front.state.picker_push(character);
        }
        handle_overlay_key(
            front,
            KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
        );
        let command = front.pending_session_cmds.pop().unwrap();
        assert_eq!(
            command,
            SessionArgs::Restore("sess-saved-picker".to_owned())
        );
        execute_session_command(&mut sessions, &command).await;
        assert_eq!(sessions.active().front.session, *checkpoint.session());
        assert!(transcript(&sessions.active().front).contains("saved conversation"));
        execute_session_command(&mut sessions, &SessionArgs::Pick).await;
        let choices = sessions.active().front.state.session_choices();
        assert_eq!(choices.len(), 2);
        assert!(choices.iter().all(|choice| !choice.saved));
        drop(sessions);
        std::fs::remove_dir_all(directory).unwrap();
    }

    async fn finish_maintenance(sessions: &mut SessionRegistry) {
        assert!(sessions.maintenance.is_some(), "operation started");
        tokio::time::timeout(Duration::from_secs(5), async {
            while !sessions.poll_maintenance() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("maintenance finishes");
    }

    struct BlockedMaintenanceStore {
        started: Option<tokio::sync::oneshot::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl BlockedMaintenanceStore {
        fn wait(&mut self) -> Result<(), nexus_core::AgentError> {
            self.started
                .take()
                .expect("only one worker")
                .send(())
                .unwrap();
            let _ = self.release.recv();
            Err(nexus_core::AgentError::new(
                nexus_core::ErrorCategory::StorageFailure,
                "injected publication is uncertain; reopen required",
                nexus_core::RetryGuidance::DoNotRetry,
            )
            .unwrap())
        }
    }

    impl nexus_core::SessionStore for BlockedMaintenanceStore {
        fn import_legacy_sessions(&mut self) -> Result<usize, nexus_core::AgentError> {
            self.wait().map(|()| 0)
        }
        fn delete_session(&mut self, _: &SessionId) -> Result<(), nexus_core::AgentError> {
            self.wait()
        }
        fn list_sessions(
            &self,
            _: usize,
        ) -> Result<Vec<nexus_core::SessionMetadata>, nexus_core::AgentError> {
            panic!("listing must not wait behind maintenance")
        }
        fn load_session(
            &self,
            _: &SessionId,
        ) -> Result<nexus_core::SessionCheckpoint, nexus_core::AgentError> {
            panic!("restore must not wait behind maintenance")
        }
        fn save_checkpoint(
            &mut self,
            _: &nexus_core::SessionCheckpoint,
        ) -> Result<(), nexus_core::AgentError> {
            unreachable!("maintenance test does not submit work")
        }
        fn record_intent(
            &mut self,
            _: &nexus_core::ToolIntentRecord,
        ) -> Result<(), nexus_core::AgentError> {
            unreachable!("maintenance test does not dispatch tools")
        }
        fn record_outcome(
            &mut self,
            _: &nexus_core::ToolOutcomeRecord,
        ) -> Result<(), nexus_core::AgentError> {
            unreachable!("maintenance test does not dispatch tools")
        }
    }

    #[tokio::test]
    async fn blocked_maintenance_keeps_input_responsive_and_reports_errors_to_origin() {
        for command in [
            SessionArgs::Import,
            SessionArgs::Delete("sess-old".to_owned()),
        ] {
            let mut sessions = registry();
            let (started, receiver) = tokio::sync::oneshot::channel();
            let (release, blocked) = std::sync::mpsc::channel();
            sessions.store = Some(SessionStoreHandle::new(Box::new(BlockedMaintenanceStore {
                started: Some(started),
                release: blocked,
            })));
            tokio::time::timeout(
                Duration::from_secs(1),
                execute_session_command(&mut sessions, &command),
            )
            .await
            .expect("command must return before disk finishes");
            tokio::time::timeout(Duration::from_secs(1), receiver)
                .await
                .unwrap()
                .unwrap();
            assert!(!sessions.poll_maintenance(), "worker remains blocked");
            for duplicate in [
                command.clone(),
                SessionArgs::List,
                SessionArgs::Restore("sess-old".into()),
            ] {
                tokio::time::timeout(
                    Duration::from_secs(1),
                    execute_session_command(&mut sessions, &duplicate),
                )
                .await
                .expect("no command waits behind the store lock");
            }
            assert!(
                transcript(&sessions.active().front)
                    .contains("no additional storage command was started")
            );

            // Input still reaches the real key-batch handler while the store
            // worker is blocked, including the normal desktop quit sequence.
            let current = sessions.active_mut();
            current.front.storage_busy = true;
            current.front.focus = Focus::Composer;
            current.front.state.composer_type('x');
            assert!(!handle_key(&mut current.front, &current.runtime, press(KeyCode::Enter)).await);
            assert_eq!(current.front.state.composer(), "x");
            assert!(transcript(&current.front).contains("draft preserved"));
            current.front.focus = Focus::Viewport;
            let mut keys = [
                press(KeyCode::Char(':')),
                press(KeyCode::Char('q')),
                press(KeyCode::Enter),
            ]
            .into_iter();
            let batch = tokio::time::timeout(
                Duration::from_secs(1),
                handle_key_batch(
                    &mut current.front,
                    &current.runtime,
                    || Ok(keys.next().map(Input::Key)),
                    MAX_KEYS_PER_TICK,
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(batch.quit);
            assert!(
                sessions.maintenance.is_some(),
                "quit does not imply rollback"
            );

            let second = slot("s2", "sess-test-2", &sessions.template);
            sessions.slots.push(second);
            sessions.active = 1;
            release.send(()).unwrap();
            finish_maintenance(&mut sessions).await;
            assert!(!transcript(&sessions.active().front).contains("injected publication"));
            let notices = transcript(&sessions.slots[0].front);
            assert!(notices.contains("injected publication is uncertain; reopen required"));
            assert!(notices.contains("restart the TUI to reconcile storage before retrying"));
            assert!(!notices.contains("deleted saved session sess-old"));
        }
    }

    #[test]
    fn maintenance_worker_disconnect_reports_uncertainty_without_success() {
        let mut sessions = registry();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        sessions.maintenance = Some((sessions.active().front.session.clone(), receiver));
        drop(sender);
        assert!(sessions.poll_maintenance());
        assert!(sessions.maintenance.is_none());
        let notices = transcript(&sessions.active().front);
        assert!(notices.contains("worker failed"));
        assert!(notices.contains("do not blindly retry"));
    }

    #[tokio::test]
    async fn session_import_command_imports_legacy_history_without_opening_a_slot() {
        use nexus_core::SessionStore;

        let directory = session_store_test_directory();
        let checkpoint = saved_checkpoint("sess-legacy");
        let mut legacy = nexus_session::FileSessionStore::open(&directory).expect("legacy opens");
        legacy
            .save_checkpoint(&checkpoint)
            .expect("save legacy history");
        drop(legacy);
        let backup = std::fs::read(directory.join("session-store.json")).expect("read backup");
        let store = nexus_session::DirectorySessionStore::open(&directory).expect("store opens");
        assert!(
            store
                .list_sessions(10)
                .expect("list saved sessions")
                .is_empty()
        );
        let mut sessions = registry();
        sessions.store = Some(SessionStoreHandle::new(Box::new(store)));

        execute_session_command(&mut sessions, &SessionArgs::Import).await;
        finish_maintenance(&mut sessions).await;

        let notices = transcript(&sessions.active().front);
        assert!(notices.contains("explicitly reading the legacy session file"));
        assert!(notices.contains("imported 1 legacy sessions"));
        assert_eq!(sessions.len(), 1, "import does not restore a live session");
        assert_eq!(sessions.active, 0);
        execute_session_command(&mut sessions, &SessionArgs::Import).await;
        finish_maintenance(&mut sessions).await;
        assert!(transcript(&sessions.active().front).contains("imported 0 legacy sessions"));
        drop(sessions);

        let store = nexus_session::DirectorySessionStore::open(&directory).expect("store reopens");
        assert_eq!(
            store
                .load_session(checkpoint.session())
                .expect("import persisted"),
            checkpoint
        );
        assert_eq!(
            std::fs::read(directory.join("session-store.json")).expect("backup remains"),
            backup
        );
        drop(store);
        std::fs::remove_dir_all(directory).expect("remove test store");
    }

    #[tokio::test]
    async fn session_delete_command_refuses_active_and_background_live_slots() {
        use nexus_core::SessionStore;

        let directory = session_store_test_directory();
        let mut sessions = registry();
        sessions
            .slots
            .push(slot("s2", "sess-test-2", &sessions.template));
        let checkpoints = [
            saved_checkpoint("sess-test-1"),
            saved_checkpoint("sess-test-2"),
        ];
        let mut store =
            nexus_session::DirectorySessionStore::open(&directory).expect("store opens");
        for checkpoint in &checkpoints {
            store
                .initialize_session(checkpoint)
                .expect("save live history");
        }
        sessions.store = Some(SessionStoreHandle::new(Box::new(store)));

        for checkpoint in &checkpoints {
            execute_session_command(
                &mut sessions,
                &SessionArgs::Delete(checkpoint.session().as_str().to_owned()),
            )
            .await;
        }

        let notices = transcript(&sessions.active().front);
        assert_eq!(
            notices
                .matches("cannot delete a session open in this TUI")
                .count(),
            2
        );
        assert!(!notices.contains("deleted saved session"));
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions.active, 0);
        drop(sessions);

        let store = nexus_session::DirectorySessionStore::open(&directory).expect("store reopens");
        for checkpoint in checkpoints {
            assert_eq!(
                store
                    .load_session(checkpoint.session())
                    .expect("live history retained"),
                checkpoint
            );
        }
        drop(store);
        std::fs::remove_dir_all(directory).expect("remove test store");
    }

    #[tokio::test]
    async fn session_delete_command_removes_an_old_saved_session_only() {
        use nexus_core::SessionStore;

        let directory = session_store_test_directory();
        let live = saved_checkpoint("sess-test-1");
        let old = saved_checkpoint("sess-old");
        let mut store =
            nexus_session::DirectorySessionStore::open(&directory).expect("store opens");
        store.initialize_session(&live).expect("save live history");
        store.initialize_session(&old).expect("save old history");
        let mut sessions = registry();
        sessions.store = Some(SessionStoreHandle::new(Box::new(store)));

        execute_session_command(&mut sessions, &SessionArgs::Delete("sess-old".to_owned())).await;
        finish_maintenance(&mut sessions).await;

        assert!(transcript(&sessions.active().front).contains("deleted saved session sess-old"));
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions.active, 0);
        drop(sessions);

        let store = nexus_session::DirectorySessionStore::open(&directory).expect("store reopens");
        let saved = store.list_sessions(10).expect("list remaining sessions");
        assert_eq!(saved.len(), 1);
        assert_eq!(&saved[0].session, live.session());
        assert!(store.load_session(old.session()).is_err());
        assert_eq!(
            store
                .load_session(live.session())
                .expect("live history retained"),
            live
        );
        drop(store);
        std::fs::remove_dir_all(directory).expect("remove test store");
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
            transcript(&active.front).contains("new conversation · s2 · type a task below"),
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
            boot_model(&user_config_snapshot(&configured(3, &[], &[]).user_config)).as_deref(),
            Some("m0"),
            "with nothing ranked, the first configured model boots"
        );
        assert_eq!(
            boot_model(&user_config_snapshot(
                &configured(3, &[], &["m2"]).user_config
            ))
            .as_deref(),
            Some("m2"),
            "a favourite beats admission order"
        );
        assert_eq!(
            boot_model(&user_config_snapshot(
                &configured(3, &["m1"], &["m2"]).user_config
            ))
            .as_deref(),
            Some("m1"),
            "recent beats favourites"
        );
    }

    #[test]
    fn runtime_capability_claim_is_explicit_and_current_adapters_match_it() {
        let declared = tui_provider_capabilities();
        assert_eq!(FakeProvider::demo_two_turn().capabilities(), declared);
        let config = user_config_snapshot(&configured(1, &[], &[]).user_config);
        let profile = config
            .provider("demo-provider")
            .expect("test provider exists");
        let openai = OpenAiProvider::from_profile(profile, "demo-model")
            .expect("current adapter config builds without network");
        assert_eq!(openai.capabilities(), declared);
        assert!(adapter_satisfies_tui_requirements(&openai.capabilities()));
        for protocol in [
            nexus_config::ProtocolKind::OpenAiResponses,
            nexus_config::ProtocolKind::AnthropicMessages,
        ] {
            let mut profile = profile.clone();
            profile.protocol = protocol;
            let adapter = OpenAiProvider::from_profile(&profile, "demo-model")
                .expect("configured protocol adapter builds without network");
            assert_eq!(adapter.capabilities(), declared);
            assert!(adapter_satisfies_tui_requirements(&adapter.capabilities()));
        }
    }

    #[test]
    fn limited_adapter_capabilities_are_refused_at_live_binding() {
        let mut limited = tui_provider_capabilities();
        limited.tool_calls = false;
        assert_eq!(
            admit_adapter((), &limited),
            Err(ErrorCategory::UnsupportedCapability)
        );

        limited = tui_provider_capabilities();
        limited.max_context_items = Some(1);
        assert_eq!(
            admit_adapter((), &limited),
            Err(ErrorCategory::UnsupportedCapability)
        );

        limited = tui_provider_capabilities();
        limited.max_output_bytes = Some(1);
        assert_eq!(
            admit_adapter((), &limited),
            Err(ErrorCategory::UnsupportedCapability)
        );

        let admitted = admit_adapter("bound", &tui_provider_capabilities())
            .expect("the current adapter contract is admitted");
        assert_eq!(admitted, "bound");
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
        let config = configured(3, &[], &[]);
        config.user_config.lock().expect("config lockable").path = Some(path.clone());
        let (run, mut front) = frontend_with(config);
        front.run_models.insert(run.clone(), "m1".to_owned());
        front.active_model = Some("m2".to_owned());

        assert!(front.apply_events(vec![started(&run, 0), finished(&run, 1)]));
        assert_eq!(
            front.config_snapshot().recent().first().map(String::as_str),
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
    fn sessions_serialize_recent_updates_through_the_shared_config_owner() {
        let directory = temp_config_dir("shared-recent");
        let path = directory.join("config.json");
        let config = configured(3, &[], &[]);
        config.user_config.lock().expect("config lockable").path = Some(path.clone());
        let (first_run, mut first) = frontend_with(config.clone());
        let (second_run, mut second) = frontend_with(config);
        first.run_models.insert(first_run.clone(), "m1".to_owned());
        second
            .run_models
            .insert(second_run.clone(), "m2".to_owned());

        // Complete the second slot first, then the first. Each write must
        // include the update already recorded by the other slot.
        assert!(second.apply_events(vec![started(&second_run, 0), finished(&second_run, 1)]));
        assert!(first.apply_events(vec![started(&first_run, 0), finished(&first_run, 1)]));

        let expected = vec!["m1".to_owned(), "m2".to_owned()];
        assert_eq!(first.config_snapshot().recent(), expected);
        let persisted = load_config(&path)
            .expect("shared config reloads")
            .expect("shared config was saved");
        assert_eq!(persisted.recent(), expected);
        std::fs::remove_dir_all(&directory).expect("temporary directory removed");
    }

    #[test]
    fn new_sessions_boot_from_the_latest_shared_recent_model() {
        let template = configured(3, &[], &[]);
        let mut sessions = SessionRegistry::new(slot("s1", "sess-shared-1", &template), template);
        let run = run_id("shared-config-run");
        let first = sessions.active_mut();
        first.front.merger.adopt(&run);
        first.front.run_models.insert(run.clone(), "m2".to_owned());
        assert!(
            first
                .front
                .apply_events(vec![started(&run, 0), finished(&run, 1)])
        );
        assert_eq!(first.front.active_model.as_deref(), Some("m0"));

        sessions.create().expect("new session builds");
        assert_eq!(sessions.active().front.active_model.as_deref(), Some("m2"));
    }

    #[test]
    fn prepared_provider_selection_survives_picker_changes_before_first_call() {
        let mut front = Frontend::with_config(session(), configured(2, &[], &[]));
        let binding = Arc::new(Mutex::new(LiveSelection {
            config: front.config_snapshot(),
            provider_settings: nexus_config::settings::ProviderSettings {
                fallback_call_timeout_seconds: 123,
                ..Default::default()
            },
            active_model: Some("m0".to_owned()),
            demo: false,
            pending_run: None,
            pending_run_id: None,
        }));
        front.attach_live(binding.clone(), None, ExecutionMode::Local);

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
        assert_eq!(
            captured.provider_settings.fallback_call_timeout_seconds,
            123
        );
        assert_eq!(binding.provider_settings, captured.provider_settings);
    }

    #[test]
    fn terminal_before_first_provider_call_clears_only_its_run_snapshot() {
        let mut front = Frontend::with_config(session(), configured(2, &[], &[]));
        let binding = Arc::new(Mutex::new(LiveSelection {
            config: front.config_snapshot(),
            provider_settings: Default::default(),
            active_model: Some("m0".to_owned()),
            demo: false,
            pending_run: None,
            pending_run_id: None,
        }));
        front.attach_live(binding.clone(), None, ExecutionMode::Local);

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
            config: user_config_snapshot(&configured(2, &[], &[]).user_config),
            provider_settings: Default::default(),
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
        let config = configured(2, &[], &[]);
        // A path whose parent does not exist: `save` cannot create it.
        config.user_config.lock().expect("config lockable").path = Some(
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
            user_config_snapshot(&loaded.user_config)
                .models()
                .is_empty(),
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
    async fn container_execution_refuses_mode_switching() {
        let (runtime, _streams) = build_runtime().expect("demo wiring is valid");
        let mut front = Frontend::new(session());
        front.execution = ExecutionMode::Container;
        front.focus = Focus::Composer;
        let before = front.state.mode().id.clone();

        assert!(!handle_key(&mut front, &runtime, press(KeyCode::Tab)).await);
        assert_eq!(front.state.mode().id, before);
        assert!(
            front
                .state
                .transcript()
                .iter()
                .any(|entry| entry.contains("mode switching is unavailable"))
        );
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
    async fn session_new_executes_and_dismisses_command_completion_surface() {
        let mut sessions = registry();
        sessions.active_mut().front.focus = Focus::Viewport;
        for key in ":session new"
            .chars()
            .map(KeyCode::Char)
            .chain([KeyCode::Enter])
        {
            let slot = sessions.active_mut();
            assert!(!handle_key(&mut slot.front, &slot.runtime, press(key)).await);
        }
        let commands = sessions.active_mut().front.take_session_cmds();
        assert_eq!(commands, vec![SessionArgs::New]);
        for command in &commands {
            execute_session_command(&mut sessions, command).await;
        }

        let active = sessions.active();
        assert_eq!(sessions.active, 1);
        assert_eq!(active.front.state.session_label(), Some("s2"));
        assert!(active.front.state.command_line().is_none());
        assert!(active.front.state.completions().is_empty());
        assert!(transcript(&active.front).contains("new conversation · s2"));
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
            StartupAction::Help | StartupAction::Usage => {
                panic!("expected a run action for {args:?}")
            }
        }
    }

    /// Unwraps the demo flag from the `Run` variant.
    fn demo_flag(args: &[&str]) -> bool {
        match parse_startup_args(&argv(args)) {
            StartupAction::Run(parsed) => parsed.demo,
            StartupAction::Help | StartupAction::Usage => {
                panic!("expected a run action for {args:?}")
            }
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
    fn help_flag_has_a_successful_help_action() {
        for args in [&["--help"][..], &["-h"][..]] {
            assert!(matches!(
                parse_startup_args(&argv(args)),
                StartupAction::Help
            ));
        }
        usage(&["--unknown"]);
    }

    #[test]
    fn no_arguments_yield_no_explicit_jail() {
        assert_eq!(tools_root(&[]), None);
    }

    #[test]
    fn container_selection_is_explicit_and_requires_a_workspace_and_images() {
        let incomplete = match parse_startup_args(&argv(&["--execution", "container"])) {
            StartupAction::Run(parsed) => parsed,
            StartupAction::Help | StartupAction::Usage => {
                panic!("syntactically valid flags should parse")
            }
        };
        assert!(validate_execution_args(&incomplete, true, cfg!(feature = "container")).is_err());
        let incomplete_images = match parse_startup_args(&argv(&[
            "--execution",
            "container",
            "--tools-root",
            "/workspace",
            "--gateway-image",
            "gateway@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ])) {
            StartupAction::Run(parsed) => parsed,
            StartupAction::Help | StartupAction::Usage => {
                panic!("syntactically valid flags should parse")
            }
        };
        assert!(
            validate_execution_args(&incomplete_images, true, cfg!(feature = "container")).is_err()
        );
        match parse_startup_args(&argv(&[
            "--execution",
            "container",
            "--tools-root",
            "/workspace",
            "--gateway-image",
            "gateway@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--container-image",
            "worker@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        ])) {
            StartupAction::Run(parsed) => {
                assert_eq!(parsed.execution, ExecutionMode::Container);
                assert!(validate_execution_args(&parsed, true, true).is_ok());
            }
            StartupAction::Help | StartupAction::Usage => {
                panic!("complete container options should parse")
            }
        }
        let current_directory_default = match parse_startup_args(&argv(&[
            "--execution",
            "container",
            "--gateway-image",
            "gateway@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--container-image",
            "worker@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        ])) {
            StartupAction::Run(parsed) => parsed,
            StartupAction::Help | StartupAction::Usage => {
                panic!("container images should parse without a root flag")
            }
        };
        assert!(validate_execution_args(&current_directory_default, true, true).is_ok());
    }

    #[test]
    fn run_alias_selects_container_and_mode_flags_remain_single_use() {
        let parsed = match parse_startup_args(&argv(&["--run", "container"])) {
            StartupAction::Run(parsed) => parsed,
            StartupAction::Help | StartupAction::Usage => panic!("alias should parse"),
        };
        assert_eq!(parsed.execution, ExecutionMode::Container);
        usage(&["--run", "container", "--execution", "container"]);
        usage(&["--execution", "local", "--run", "container"]);
        usage(&["--run", "container", "--run", "local"]);
    }

    #[test]
    fn local_startup_ignores_saved_images_but_rejects_explicit_image_flags() {
        let mut config = UserConfig::default_config();
        config
            .set_container_images(Some("gateway:saved".into()), Some("workload:saved".into()))
            .expect("valid saved images");
        let args = match parse_startup_args(&argv(&[])) {
            StartupAction::Run(args) => args,
            StartupAction::Help | StartupAction::Usage => panic!("default startup parses"),
        };
        let (gateway, workload) = merge_startup_images(&args, &config);
        assert_eq!((gateway, workload), (None, None));
        let mut args = args;
        args.gateway_image = Some("gateway:explicit".into());
        assert!(validate_execution_args(&args, true, cfg!(feature = "container")).is_err());
    }

    #[test]
    fn container_startup_merges_cli_and_saved_images() {
        let mut config = UserConfig::default_config();
        config
            .set_container_images(Some("gateway:saved".into()), Some("workload:saved".into()))
            .expect("valid saved images");
        let mut args = match parse_startup_args(&argv(&["--execution", "container"])) {
            StartupAction::Run(args) => args,
            StartupAction::Help | StartupAction::Usage => {
                panic!("container startup parses")
            }
        };
        args.gateway_image = Some("gateway:cli".into());
        assert_eq!(
            merge_startup_images(&args, &config),
            (Some("gateway:cli".into()), Some("workload:saved".into()))
        );
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
        assert!(matches!(
            parse_startup_args(&argv(&["--help"])),
            StartupAction::Help
        ));
        assert!(matches!(
            parse_startup_args(&argv(&["-h"])),
            StartupAction::Help
        ));
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
            provider_settings: Default::default(),
            active_model: active_model.map(str::to_owned),
            demo: true,
            pending_run: None,
            pending_run_id: None,
        }
    }

    #[test]
    fn context_settings_follow_the_pending_model_snapshot() {
        let mut selected = selection_with(Some("m0"));
        selected.config = UserConfig::from_json(
            r#"{
            "revision": 2,
            "providers": [{
                "id": "demo-provider", "display_name": "Demo", "adapter": "direct",
                "protocol": "chat_completions", "endpoint": "https://example.invalid",
                "credential": {"env": "NEXUS_TUI_TEST_KEY"}, "default_model": "demo-model"
            }],
            "models": [{"id": "m0", "provider": "demo-provider", "name": "demo-model",
                "context_window_tokens": 94208, "context_management": {"target_percent": 5}}],
            "favourites": [], "recent": []
        }"#,
        )
        .unwrap();
        let expected = selected
            .config
            .context_management_for("demo-provider", Some("m0"))
            .unwrap();
        let LiveResolve::Live(adapter) =
            resolve_live_adapter(&selected, |_| Some("secret".to_owned()))
        else {
            panic!("configured adapter must resolve without network I/O");
        };
        assert_eq!(adapter.context_management(), expected);
        let mut latest = selected.clone();
        latest.active_model = None;
        latest.pending_run = Some(Box::new(selected));
        let binding = Arc::new(Mutex::new(latest));
        let provider = PerRunProvider::live(binding.clone());
        assert_eq!(provider.context_management(), expected);
        binding.lock().unwrap().pending_run = None;
        assert_eq!(
            provider.context_management(),
            nexus_core::ContextManagementSettings::default()
        );
    }

    #[test]
    fn no_selection_without_demo_fails_instead_of_serving_fakes() {
        let mut selection = LiveSelection {
            config: UserConfig::default_config(),
            provider_settings: Default::default(),
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
            provider_settings: Default::default(),
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
            ExecutionMode::Local,
        );
        assert!(notice.contains("unavailable"));
        assert!(!notice.contains("demo script"));
        assert!(notice.contains("real file tools + sandboxed exec"));
        assert!(notice.contains("writes/exec require approval"));
    }

    #[test]
    fn container_wiring_notice_does_not_claim_isolation() {
        let notice = wiring_notice(
            &selection_with(None),
            Some(std::path::Path::new("/project")),
            ExecutionMode::Container,
        );
        assert!(notice.contains("UNVALIDATED container tools"));
        assert!(notice.contains("isolation is not established"));
        assert!(!notice.contains("real file tools + sandboxed exec"));
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
            provider_settings: Default::default(),
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
            diagnostics: None,
            user_config: shared_user_config(UserConfig::default_config(), None),
            settings: nexus_config::load_settings(nexus_config::SettingsProfile::Tui, None, &[])
                .unwrap(),
            active_model: None,
            tools_root: Some(missing),
            demo: false,
            strict_tools: true,
            execution: ExecutionMode::Local,
            gateway_image: None,
            workload_image: None,
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
            diagnostics: None,
            user_config: shared_user_config(UserConfig::default_config(), None),
            settings: nexus_config::load_settings(nexus_config::SettingsProfile::Tui, None, &[])
                .unwrap(),
            active_model: None,
            tools_root: None,
            demo: false,
            strict_tools: true,
            execution: ExecutionMode::Local,
            gateway_image: None,
            workload_image: None,
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

#[cfg(test)]
mod preference_tests {
    use super::*;
    use nexus_config::tui_settings::ClipboardMode;
    use std::cell::Cell;

    #[test]
    fn clipboard_modes_call_only_the_selected_backends() {
        for (mode, system_ok, expected_system, expected_osc, expected_result) in [
            (ClipboardMode::Auto, true, true, false, true),
            (ClipboardMode::Auto, false, true, true, true),
            (ClipboardMode::System, false, true, false, false),
            (ClipboardMode::Osc52, false, false, true, true),
            (ClipboardMode::Disabled, true, false, false, false),
        ] {
            let system_called = Cell::new(false);
            let osc_called = Cell::new(false);
            let result = copy_with(
                "selection",
                mode,
                |_| {
                    system_called.set(true);
                    system_ok
                },
                |_| {
                    osc_called.set(true);
                    true
                },
            );
            assert_eq!(result, expected_result);
            assert_eq!(system_called.get(), expected_system);
            assert_eq!(osc_called.get(), expected_osc);
        }
        assert!(!copy_with(
            "",
            ClipboardMode::Auto,
            |_| panic!("empty copy"),
            |_| panic!("empty copy")
        ));
    }

    #[test]
    fn disabled_mouse_leaves_presentation_untouched() {
        let mut front = Frontend::new(SessionId::new("preferences").unwrap());
        let mut settings = nexus_config::TuiSettings::default();
        settings.input.mouse_capture = false;
        front.state.set_settings(settings).unwrap();
        front.state.composer_type('x');
        assert!(!handle_mouse(
            &mut front,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row: 0,
                modifiers: crossterm::event::KeyModifiers::NONE,
            }
        ));
        assert_eq!(front.state.composer(), "x");
    }

    #[test]
    fn wheel_distance_and_copy_on_select_are_configurable() {
        let mut front = Frontend::new(SessionId::new("preferences").unwrap());
        let mut settings = nexus_config::TuiSettings::default();
        settings.input.scroll_lines = 7;
        settings.input.copy_on_select = false;
        front.state.set_settings(settings).unwrap();
        for _ in 0..30 {
            front.state.notice("history");
        }
        front.state.set_viewport(80, 5);
        let event = |kind| MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        assert!(handle_mouse(&mut front, event(MouseEventKind::ScrollUp)));
        assert_eq!(front.state.scrollback(), 7);
        front.state.composer_type('x');
        front.state.begin_composer_selection(0);
        front.state.extend_composer_selection(1);
        assert!(handle_mouse(
            &mut front,
            event(MouseEventKind::Up(MouseButton::Left))
        ));
        assert!(front.state.text_selection().is_some());
        assert!(front.state.toast().is_none());
    }
}
