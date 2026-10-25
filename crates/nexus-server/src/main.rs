//! `nexus-server` binary: loopback HTTP frontend for the agent runtime.
//!
//! Binds `127.0.0.1` only. Real tools/providers require bearer authentication;
//! only the fake demo may run unauthenticated. Sessions created
//! without a provider selection serve scripted fakes; sessions bound to
//! a configured provider use the real OpenAI-compatible adapter.
//! Do not expose this server to a network.

#![forbid(unsafe_code)]

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use nexus_config::{UserConfig, load, resolve_path};
use nexus_core::Limits;
use nexus_server::{Server, ToolsMode};

/// Default loopback port.
const DEFAULT_PORT: u16 = 8471;
const BENCHMARK_MAX_MODEL_TURNS: u32 = 500;
const BENCHMARK_MAX_TOOL_CALLS: u32 = 4_096;
const BENCHMARK_MAX_TOOL_CALLS_PER_TURN: u32 = 64;
const BENCHMARK_RETAINED_CONTEXT_ITEMS: usize = 8_192;
/// Maximum simultaneously serviced sockets; excess connections are closed
/// immediately instead of creating another unbounded thread.
const MAX_CONNECTIONS: usize = 64;
const SESSION_CLEANUP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

fn usage() -> ! {
    eprintln!(
        "usage: nexus-server [--port N] [--config PATH] [--session-store DIRECTORY] [--tools fake|real] [--tools-root PATH] [--strict-tools] [--execution local|container] [--gateway-image IMAGE@sha256:DIGEST] [--container-image IMAGE@sha256:DIGEST] [--benchmark-pilot] [--auth-token-env NAME] [--help]"
    );
    eprintln!("  Serves the M0 test-only demo API on 127.0.0.1:N (default {DEFAULT_PORT}).");
    eprintln!("  --config overrides NEXUS_CONFIG and the platform config path.");
    eprintln!(
        "  --session-store opts into private original-source and tool-operation records in a dedicated directory; no session restore API."
    );
    eprintln!(
        "  --benchmark-pilot opts into finite 500-turn/24-hour limits; intended only for isolated benchmark runs."
    );
    eprintln!(
        "  --auth-token-env reads a 64-hex-character bearer token from the named environment variable."
    );
    eprintln!(
        "  --tools real executes filesystem tools and sandboxed host_exec with --tools-root as the project (default: working directory);"
    );
    eprintln!(
        "  real tools default to development mode; --strict-tools requires approval for writes/exec. Fake tools touch no real files."
    );
    #[cfg(feature = "container")]
    eprintln!(
        "  container mode requires --tools real and both immutable image references; --tools-root defaults to the current directory. Isolation is unvalidated."
    );
    #[cfg(not(feature = "container"))]
    eprintln!("  container mode requires a build with --features container.");
    std::process::exit(2);
}

/// CLI outcome: run with a port, or print usage and exit. Splitting the
/// decision from the exit keeps every rejection path unit-testable.
enum CliAction {
    /// Serve on this loopback port with this optional explicit config path
    /// and tool wiring.
    Run {
        port: u16,
        config: Option<String>,
        tools_real: bool,
        tools_root: Option<String>,
        strict_tools: bool,
        execution: ExecutionMode,
        gateway_image: Option<String>,
        workload_image: Option<String>,
        auth_token_env: Option<String>,
        benchmark_pilot: bool,
        session_store: Option<String>,
    },
    /// Print usage and exit 2.
    Usage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExecutionMode {
    Local,
    Container,
}

fn parse_args(argv: &[String]) -> CliAction {
    let mut port = DEFAULT_PORT;
    let mut config: Option<String> = None;
    let mut tools_real = false;
    let mut tools_root: Option<String> = None;
    let mut strict_tools = false;
    let mut execution = ExecutionMode::Local;
    let mut execution_set = false;
    let mut gateway_image = None;
    let mut workload_image = None;
    let mut auth_token_env = None;
    let mut benchmark_pilot = false;
    let mut session_store = None;
    let mut args = argv.iter().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return CliAction::Usage,
            "--strict-tools" => strict_tools = true,
            "--session-store" => {
                if session_store.is_some() {
                    return CliAction::Usage;
                }
                session_store = match args.next() {
                    Some(value) if !value.is_empty() && !value.starts_with("--") => {
                        Some(value.clone())
                    }
                    _ => return CliAction::Usage,
                };
            }
            "--benchmark-pilot" => {
                if benchmark_pilot {
                    return CliAction::Usage;
                }
                benchmark_pilot = true;
            }
            "--execution" => {
                if execution_set {
                    return CliAction::Usage;
                }
                execution_set = true;
                execution = match args.next().map(String::as_str) {
                    Some("local") => ExecutionMode::Local,
                    Some("container") => ExecutionMode::Container,
                    _ => return CliAction::Usage,
                };
            }
            "--gateway-image" => {
                if gateway_image.is_some() {
                    return CliAction::Usage;
                }
                gateway_image = match args.next() {
                    Some(value) if !value.is_empty() => Some(value.clone()),
                    _ => return CliAction::Usage,
                };
            }
            "--container-image" => {
                if workload_image.is_some() {
                    return CliAction::Usage;
                }
                workload_image = match args.next() {
                    Some(value) if !value.is_empty() => Some(value.clone()),
                    _ => return CliAction::Usage,
                };
            }
            "--auth-token-env" => match args.next() {
                Some(value) if !value.is_empty() => auth_token_env = Some(value.clone()),
                _ => return CliAction::Usage,
            },
            "--port" => match args.next().and_then(|value| value.parse().ok()) {
                Some(value) => port = value,
                None => return CliAction::Usage,
            },
            // An explicit empty value is accepted as a path the resolver
            // then rejects, rather than being silently dropped here: one
            // layer owns path precedence.
            "--config" => match args.next() {
                Some(value) => config = Some(value.clone()),
                None => return CliAction::Usage,
            },
            "--tools" => match args.next().map(String::as_str) {
                Some("real") => tools_real = true,
                Some("fake") => tools_real = false,
                _ => return CliAction::Usage,
            },
            "--tools-root" => match args.next() {
                Some(value) => tools_root = Some(value.clone()),
                None => return CliAction::Usage,
            },
            _ => return CliAction::Usage,
        }
    }
    if execution == ExecutionMode::Container
        && (!tools_real
            || tools_root.as_deref().is_some_and(str::is_empty)
            || strict_tools
            || gateway_image.is_none()
            || workload_image.is_none())
    {
        return CliAction::Usage;
    }
    if benchmark_pilot && (execution != ExecutionMode::Container || !tools_real || strict_tools) {
        return CliAction::Usage;
    }
    if execution == ExecutionMode::Local && (gateway_image.is_some() || workload_image.is_some()) {
        return CliAction::Usage;
    }
    CliAction::Run {
        port,
        config,
        tools_real,
        tools_root,
        strict_tools,
        execution,
        gateway_image,
        workload_image,
        auth_token_env,
        benchmark_pilot,
        session_store,
    }
}

/// Loads the user configuration at startup.
///
/// A missing file is not an error: the server starts on
/// [`UserConfig::default_config`] and simply has no configured selection
/// until the file appears. A file that exists but cannot be parsed is a
/// hard startup failure with a nonzero exit: serving with silent defaults
/// would let a user believe a provider is configured when it is not.
fn load_user_config(explicit: Option<&str>) -> (UserConfig, Option<PathBuf>) {
    let Some(path) = resolve_path(explicit) else {
        eprintln!("nexus-server: no configuration path is available; using built-in defaults");
        return (UserConfig::default_config(), None);
    };
    match load(&path) {
        Ok(Some(config)) => (config, Some(path)),
        Ok(None) => {
            eprintln!(
                "nexus-server: no configuration file at {}; using built-in defaults",
                path.display()
            );
            (UserConfig::default_config(), Some(path))
        }
        Err(error) => {
            eprintln!("nexus-server: {error} ({})", path.display());
            std::process::exit(1);
        }
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let (
        port,
        explicit_config,
        tools_real,
        tools_root,
        strict_tools,
        execution,
        gateway_image,
        workload_image,
        auth_token_env,
        benchmark_pilot,
        session_store,
    ) = match parse_args(&argv) {
        CliAction::Run {
            port,
            config,
            tools_real,
            tools_root,
            strict_tools,
            execution,
            gateway_image,
            workload_image,
            auth_token_env,
            benchmark_pilot,
            session_store,
        } => (
            port,
            config,
            tools_real,
            tools_root,
            strict_tools,
            execution,
            gateway_image,
            workload_image,
            auth_token_env,
            benchmark_pilot,
            session_store,
        ),
        CliAction::Usage => usage(),
    };
    #[cfg(not(feature = "container"))]
    let _ = (&gateway_image, &workload_image);
    #[cfg(not(feature = "container"))]
    if execution == ExecutionMode::Container {
        eprintln!(
            "nexus-server: container execution is not enabled; rebuild with --features container"
        );
        std::process::exit(2);
    }
    #[cfg(feature = "container")]
    if execution == ExecutionMode::Container {
        for image in [gateway_image.as_deref(), workload_image.as_deref()] {
            if let Some(image) = image
                && nexus_container::validate_image_reference(image).is_err()
            {
                eprintln!("nexus-server: container images must use immutable sha256 digests");
                std::process::exit(2);
            }
        }
    }
    let (config, path) = load_user_config(explicit_config.as_deref());
    let needs_auth = tools_real || !config.providers().is_empty();
    let token = match auth_token_env.as_deref() {
        Some(name) => match std::env::var(name) {
            Ok(value) => match nexus_server::server::validate_auth_token(&value) {
                Ok(()) => Some(value),
                Err(error) => {
                    eprintln!("nexus-server: invalid bearer credential ({error})");
                    std::process::exit(1);
                }
            },
            Err(_) => {
                eprintln!("nexus-server: authentication environment variable is unavailable");
                std::process::exit(1);
            }
        },
        None if needs_auth => {
            eprintln!("nexus-server: real tools/providers require --auth-token-env");
            std::process::exit(1);
        }
        None => None,
    };
    let session_store = session_store.map(|directory| {
        let store = nexus_session::FileSessionStore::open(directory).unwrap_or_else(|_| {
            eprintln!("nexus-server: cannot open session store");
            std::process::exit(1);
        });
        nexus_runtime::SessionStoreHandle::new(Box::new(store))
    });
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap_or_else(|error| {
        eprintln!("nexus-server: cannot bind 127.0.0.1:{port} ({error})");
        std::process::exit(1);
    });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .build()
        .unwrap_or_else(|error| {
            eprintln!("nexus-server: executor failed to start ({error})");
            std::process::exit(1);
        });
    let mut server = Server::new(runtime.handle().clone());
    if let Some(store) = session_store {
        server.set_session_store(store);
    }
    if benchmark_pilot {
        let mut limits = Limits::m0_test();
        limits.max_model_turns_per_run = BENCHMARK_MAX_MODEL_TURNS;
        limits.max_tool_calls_per_run = BENCHMARK_MAX_TOOL_CALLS;
        limits.max_tool_calls_per_turn = BENCHMARK_MAX_TOOL_CALLS_PER_TURN;
        limits.run_duration = Limits::M0_TEST_MAX_DURATION;
        limits.per_tool_timeout = std::time::Duration::from_secs(2 * 60);
        limits.retained_context_items = BENCHMARK_RETAINED_CONTEXT_ITEMS;
        if let Err(error) = server.set_limits(limits) {
            eprintln!("nexus-server: invalid benchmark pilot limits ({error})");
            std::process::exit(2);
        }
        eprintln!(
            "nexus-server: finite benchmark pilot limits enabled (500 turns, 4096 tool calls, 24 hours)"
        );
    }
    if let Some(token) = token.as_deref()
        && let Err(error) = server.set_auth_token(token)
    {
        eprintln!("nexus-server: invalid bearer credential ({error})");
        std::process::exit(1);
    }
    // The banner describes the default wiring, not every session: tools
    // follow the startup mode, while each session's provider is chosen at
    // creation (unbound sessions serve the demo script, bound sessions use
    // the real adapter). Reporting only the provider count keeps operator
    // output free of configuration values. Counted before `set_config`
    // moves the document into the server.
    let configured = config.providers().len();
    server.set_config(config, path);
    let providers = if configured == 0 {
        "no configured providers (all sessions serve the demo script)".to_owned()
    } else {
        format!("{configured} configured provider(s) available for bound sessions")
    };
    if tools_real {
        // Fail fast on an unreadable jail: a missing default working
        // directory and an explicit bad root are both startup errors.
        let root = tools_root.unwrap_or_else(|| ".".to_owned());
        let mode = match execution {
            ExecutionMode::Local if strict_tools => ToolsMode::RealFiles {
                root: PathBuf::from(&root),
            },
            ExecutionMode::Local => ToolsMode::Development {
                root: PathBuf::from(&root),
            },
            ExecutionMode::Container => {
                #[cfg(feature = "container")]
                {
                    let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|error| {
                        eprintln!("nexus-server: container workspace is unusable ({error})");
                        std::process::exit(1);
                    });
                    ToolsMode::Container {
                        root: canonical_root,
                        gateway_image: gateway_image
                            .expect("container mode requires gateway image"),
                        workload_image: workload_image
                            .expect("container mode requires workload image"),
                    }
                }
                #[cfg(not(feature = "container"))]
                unreachable!("container mode rejected for local-only build")
            }
        };
        if let Err(error) = server.set_tools_mode(mode) {
            eprintln!("nexus-server: real tool root is unusable ({error}): {root}");
            std::process::exit(1);
        }
        if execution == ExecutionMode::Container {
            eprintln!(
                "SECURITY WARNING: container isolation is not validated on native Linux; workspace is live-mounted and public egress/host-LAN-metadata denial are unverified."
            );
            eprintln!(
                "nexus-server loopback API on 127.0.0.1:{port} (container tools at {root}, {providers}, authenticated; never expose)"
            );
        } else {
            eprintln!(
                "nexus-server loopback API on 127.0.0.1:{port} (real tools at {root}, strict={strict_tools}, {providers}, authenticated; never expose)"
            );
        }
    } else {
        eprintln!(
            "nexus-server M0 TEST-ONLY demo: loopback API on 127.0.0.1:{port} (scripted fakes by default, {providers}, {}; never expose)",
            if token.is_some() {
                "authenticated"
            } else {
                "test-only unauthenticated demo"
            }
        );
    }
    let server = Arc::new(server);
    let cleanup_server = Arc::downgrade(&server);
    thread::spawn(move || {
        loop {
            thread::sleep(SESSION_CLEANUP_INTERVAL);
            let Some(server) = cleanup_server.upgrade() else {
                break;
            };
            server.cleanup_idle_sessions();
        }
    });
    let connections = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if !try_acquire_connection(&connections) {
                    drop(stream);
                    continue;
                }
                let server = Arc::clone(&server);
                let connections = Arc::clone(&connections);
                let worker_connections = Arc::clone(&connections);
                if let Err(error) = thread::Builder::new()
                    .name("nexus-http-connection".to_owned())
                    .spawn(move || {
                        let _connection = ConnectionGuard(worker_connections);
                        server.handle_connection(stream);
                    })
                {
                    connections.fetch_sub(1, Ordering::AcqRel);
                    eprintln!("nexus-server: connection worker could not start ({error})");
                }
            }
            Err(error) => eprintln!("nexus-server: accept failed ({error})"),
        }
    }
}

fn try_acquire_connection(connections: &AtomicUsize) -> bool {
    let mut current = connections.load(Ordering::Acquire);
    loop {
        if current >= MAX_CONNECTIONS {
            return false;
        }
        match connections.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

struct ConnectionGuard(Arc<AtomicUsize>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod cov_main_args {
    //! Coverage for the binary's private argv parser, which no external
    //! caller can reach and which the integration suite never invokes.
    //!
    //! `parse_args` returns instead of exiting, so every branch is
    //! assertable here, including all five rejection paths that `main`
    //! turns into the usage banner plus exit 2.
    //!
    //! Every assertion uses fixed literals with no clock, randomness,
    //! environment, or network access, so the suite is deterministic and
    //! leaves no listening socket behind.

    use super::{
        BENCHMARK_MAX_MODEL_TURNS, BENCHMARK_MAX_TOOL_CALLS, BENCHMARK_MAX_TOOL_CALLS_PER_TURN,
        BENCHMARK_RETAINED_CONTEXT_ITEMS, CliAction, DEFAULT_PORT, ExecutionMode, Limits,
        parse_args,
    };

    #[test]
    fn connection_admission_stops_at_the_hard_cap_and_reopens_on_release() {
        let active = std::sync::atomic::AtomicUsize::new(super::MAX_CONNECTIONS - 1);
        assert!(super::try_acquire_connection(&active));
        assert_eq!(
            active.load(std::sync::atomic::Ordering::Acquire),
            super::MAX_CONNECTIONS
        );
        assert!(!super::try_acquire_connection(&active));
        active.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        assert!(super::try_acquire_connection(&active));
    }

    /// Builds the full argv vector a process would receive. `parse_args`
    /// skips element 0 as the program path, so every case must supply that
    /// leading entry; a missing one would shift the first flag out of view.
    fn argv(args: &[&str]) -> Vec<String> {
        std::iter::once("nexus-server")
            .chain(args.iter().copied())
            .map(str::to_string)
            .collect()
    }

    /// Unwraps the `Run` variant, asserting the branch was taken.
    fn run(args: &[&str]) -> (u16, Option<String>, bool, Option<String>) {
        match parse_args(&argv(args)) {
            CliAction::Run {
                port,
                config,
                tools_real,
                tools_root,
                ..
            } => (port, config, tools_real, tools_root),
            CliAction::Usage => panic!("expected a run action for {args:?}"),
        }
    }

    /// The port half of a `Run` variant, for the port-only cases.
    fn port(args: &[&str]) -> u16 {
        run(args).0
    }

    /// The explicit `--config` value of a `Run` variant.
    fn config(args: &[&str]) -> Option<String> {
        run(args).1
    }

    /// The tool wiring of a `Run` variant: real reads plus optional root.
    fn tools(args: &[&str]) -> (bool, Option<String>) {
        let parsed = run(args);
        (parsed.2, parsed.3)
    }

    /// Asserts the parser rejects the flags with usage.
    fn usage(args: &[&str]) {
        assert!(
            matches!(parse_args(&argv(args)), CliAction::Usage),
            "expected usage for {args:?}"
        );
    }

    /// With no flags at all the parser must fall back to the documented
    /// default loopback port rather than guessing or rejecting the empty
    /// command line. An empty argv is included because it exercises the same
    /// "loop body never runs" path as a bare program name.
    #[test]
    fn no_arguments_selects_the_documented_default_port() {
        assert_eq!(port(&[]), DEFAULT_PORT);
        assert_eq!(port(&[]), DEFAULT_PORT);
        // Pin the advertised default so a constant change is a deliberate,
        // reviewable edit rather than a silent contract drift.
        assert_eq!(DEFAULT_PORT, 8471);
    }

    #[test]
    fn real_tools_default_to_development_and_strict_is_explicit() {
        assert!(matches!(
            parse_args(&argv(&["--tools", "real"])),
            CliAction::Run {
                tools_real: true,
                strict_tools: false,
                ..
            }
        ));
        assert!(matches!(
            parse_args(&argv(&["--tools", "real", "--strict-tools"])),
            CliAction::Run {
                tools_real: true,
                strict_tools: true,
                ..
            }
        ));
    }

    #[test]
    fn container_mode_requires_real_tools_and_digest_images_and_defaults_workspace() {
        let args = [
            "--tools",
            "real",
            "--execution",
            "container",
            "--gateway-image",
            "gateway@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--container-image",
            "worker@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        ];
        assert!(matches!(
            parse_args(&argv(&args)),
            CliAction::Run {
                execution: ExecutionMode::Container,
                tools_real: true,
                tools_root: None,
                ..
            }
        ));
        usage(&["--execution", "container"]);
        #[cfg(feature = "container")]
        assert!(nexus_container::validate_image_reference("gateway:latest").is_err());
        usage(&[
            "--tools",
            "real",
            "--strict-tools",
            "--execution",
            "container",
            "--gateway-image",
            "gateway@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--container-image",
            "worker@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        ]);
    }

    #[test]
    fn benchmark_pilot_is_explicit_and_requires_container_tools() {
        usage(&["--benchmark-pilot"]);
        usage(&["--tools", "real", "--benchmark-pilot"]);
        let args = [
            "--tools",
            "real",
            "--execution",
            "container",
            "--gateway-image",
            "gateway@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--container-image",
            "worker@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "--benchmark-pilot",
        ];
        assert!(matches!(
            parse_args(&argv(&args)),
            CliAction::Run {
                benchmark_pilot: true,
                execution: ExecutionMode::Container,
                ..
            }
        ));
        usage(&["--benchmark-pilot", "--benchmark-pilot"]);
    }

    #[test]
    fn benchmark_pilot_budget_is_large_but_finite_and_validated() {
        let mut limits = Limits::m0_test();
        limits.max_model_turns_per_run = BENCHMARK_MAX_MODEL_TURNS;
        limits.max_tool_calls_per_run = BENCHMARK_MAX_TOOL_CALLS;
        limits.max_tool_calls_per_turn = BENCHMARK_MAX_TOOL_CALLS_PER_TURN;
        limits.run_duration = Limits::M0_TEST_MAX_DURATION;
        limits.per_tool_timeout = std::time::Duration::from_secs(2 * 60);
        limits.retained_context_items = BENCHMARK_RETAINED_CONTEXT_ITEMS;
        assert_eq!(limits.max_model_turns_per_run, 500);
        assert_eq!(limits.max_tool_calls_per_run, 4_096);
        assert_eq!(
            limits.run_duration,
            std::time::Duration::from_secs(24 * 60 * 60)
        );
        assert!(limits.validate().is_ok());
    }

    #[test]
    fn auth_token_source_is_an_environment_variable_name_only() {
        assert!(matches!(
            parse_args(&argv(&["--auth-token-env", "NEXUS_SERVER_TOKEN"])),
            CliAction::Run {
                auth_token_env: Some(name),
                ..
            } if name == "NEXUS_SERVER_TOKEN"
        ));
        usage(&["--auth-token-env"]);
        usage(&["--auth-token-env", ""]);
    }

    #[test]
    fn session_store_is_explicit_and_preserves_the_directory() {
        assert!(matches!(
            parse_args(&argv(&[])),
            CliAction::Run {
                session_store: None,
                ..
            }
        ));
        assert!(matches!(
            parse_args(&argv(&["--session-store", "run/private session-store", "--port", "9401"])),
            CliAction::Run { session_store: Some(directory), port: 9401, .. }
                if directory == "run/private session-store"
        ));
        usage(&["--session-store"]);
        usage(&["--session-store", ""]);
        usage(&["--session-store", "--port", "9401"]);
        usage(&["--session-store", "first", "--session-store", "second"]);
    }

    /// `--port N` is the only way to move the listener off the default, so a
    /// valid value must be reported unchanged.
    #[test]
    fn port_flag_selects_the_requested_port() {
        assert_eq!(port(&["--port", "9401"]), 9401);
    }

    /// The value is parsed as a plain decimal `u16`: both ends of the range
    /// are accepted by the parser, and leading zeros are not read as an
    /// octal literal. Port 0 is a bind failure that belongs to `main`, not a
    /// rejection here.
    #[test]
    fn port_flag_accepts_the_whole_u16_range_in_decimal() {
        assert_eq!(port(&["--port", "0"]), 0);
        assert_eq!(port(&["--port", "65535"]), u16::MAX);
        assert_eq!(port(&["--port", "00080"]), 80);
    }

    /// Passing the default explicitly must be indistinguishable from omitting
    /// the flag, which keeps the two spellings of the same configuration
    /// equivalent.
    #[test]
    fn explicit_default_port_matches_the_omitted_flag() {
        assert_eq!(port(&["--port", "8471"]), DEFAULT_PORT);
    }

    /// Repeating the flag is accepted by the loop and the last occurrence
    /// wins, matching how `main` treats the result as the final value rather
    /// than as a list.
    #[test]
    fn repeated_port_flag_takes_the_last_value() {
        assert_eq!(port(&["--port", "9401", "--port", "9402"]), 9402);
    }

    /// Every rejection path reports usage instead of guessing: unknown
    /// flags, a flag with no value, an unparsable port, and both help
    /// spellings. `main` turns each into the usage banner plus exit 2.
    #[test]
    fn rejection_paths_report_usage() {
        usage(&["--bogus"]);
        usage(&["--port"]);
        usage(&["--port", "not-a-port"]);
        usage(&["--port", "-1"]);
        usage(&["--config"]);
        usage(&["--help"]);
        usage(&["-h"]);
    }

    /// `--config PATH` is the only way to name a file explicitly, so a
    /// supplied value must be reported verbatim and the port must be
    /// unaffected: the two flags are independent, not positional.
    #[test]
    fn config_flag_selects_the_requested_path() {
        assert_eq!(
            config(&["--config", "/tmp/nexus-config.json"]),
            Some("/tmp/nexus-config.json".to_owned())
        );
        assert_eq!(port(&["--config", "/tmp/nexus-config.json"]), DEFAULT_PORT);
    }

    /// The parser never substitutes a default path: `None` means "no
    /// explicit path", and `resolve_path` then applies the `NEXUS_CONFIG`
    /// environment variable and the platform default. Parsing the value
    /// here would duplicate that precedence and let the two disagree.
    #[test]
    fn no_config_flag_leaves_path_resolution_to_the_config_layer() {
        assert_eq!(config(&[]), None);
        assert_eq!(config(&["--port", "9401"]), None);
    }

    /// Repeating the flag follows the same last-occurrence rule as
    /// `--port`, so a wrapper script can append an override.
    #[test]
    fn repeated_config_flag_takes_the_last_value() {
        assert_eq!(
            config(&[
                "--config",
                "/tmp/first.json",
                "--config",
                "/tmp/second.json"
            ]),
            Some("/tmp/second.json".to_owned())
        );
    }

    /// An empty value is preserved rather than dropped, so the resolver
    /// sees exactly what the user passed and applies its own documented
    /// empty-path rule instead of this layer inventing a default.
    #[test]
    fn empty_config_value_is_preserved_for_the_resolver() {
        assert_eq!(config(&["--config", ""]), Some(String::new()));
    }

    /// Both flags may be combined in either order; neither consumes the
    /// other's value, so the two spellings must be equivalent. Tool flags
    /// default to scripted fakes with no root.
    #[test]
    fn port_and_config_flags_combine_independently() {
        assert_eq!(
            run(&["--port", "9401", "--config", "/tmp/c.json"]),
            (9401, Some("/tmp/c.json".to_owned()), false, None)
        );
        assert_eq!(
            run(&["--config", "/tmp/c.json", "--port", "9401"]),
            (9401, Some("/tmp/c.json".to_owned()), false, None)
        );
    }

    /// `--tools` selects the executor with no root by default; `fake` is
    /// explicit and equivalent to omitting the flag.
    #[test]
    fn tools_flag_selects_fake_by_default_and_real_on_request() {
        assert_eq!(tools(&[]), (false, None));
        assert_eq!(tools(&["--tools", "fake"]), (false, None));
        assert_eq!(tools(&["--tools", "real"]), (true, None));
    }

    /// `--tools-root` names the jail explicitly and combines with `--tools
    /// real`; anything else is usage, never a silent default.
    #[test]
    fn tools_root_names_the_jail_explicitly() {
        assert_eq!(
            tools(&["--tools", "real", "--tools-root", "/srv/root"]),
            (true, Some("/srv/root".to_owned()))
        );
        assert_eq!(
            tools(&["--tools-root", "/srv/root", "--tools", "real"]),
            (true, Some("/srv/root".to_owned()))
        );
    }

    /// Unknown tool names and a missing root value are usage errors: the
    /// server must not guess an executor.
    #[test]
    fn unknown_tools_or_missing_roots_are_usage() {
        usage(&["--tools"]);
        usage(&["--tools", "shell"]);
        usage(&["--tools", ""]);
        usage(&["--tools-root"]);
    }
}
