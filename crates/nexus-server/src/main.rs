//! `nexus-server` binary: loopback HTTP frontend for the agent runtime.
//!
//! TEST-ONLY demo. Binds `127.0.0.1` only and serves scripted fakes: any
//! local process can submit, approve, and cancel. There is no
//! authentication. Do not expose this server to a network.

#![forbid(unsafe_code)]

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use nexus_config::{UserConfig, load, resolve_path};
use nexus_server::Server;

/// Default loopback port.
const DEFAULT_PORT: u16 = 8471;

fn usage() -> ! {
    eprintln!("usage: nexus-server [--port N] [--config PATH] [--help]");
    eprintln!("  Serves the M0 test-only demo API on 127.0.0.1:N (default {DEFAULT_PORT}).");
    eprintln!("  --config overrides NEXUS_CONFIG and the platform config path.");
    std::process::exit(2);
}

/// CLI outcome: run with a port, or print usage and exit. Splitting the
/// decision from the exit keeps every rejection path unit-testable.
enum CliAction {
    /// Serve on this loopback port with this optional explicit config path.
    Run { port: u16, config: Option<String> },
    /// Print usage and exit 2.
    Usage,
}

fn parse_args(argv: &[String]) -> CliAction {
    let mut port = DEFAULT_PORT;
    let mut config: Option<String> = None;
    let mut args = argv.iter().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return CliAction::Usage,
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
            _ => return CliAction::Usage,
        }
    }
    CliAction::Run { port, config }
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
    let (port, explicit) = match parse_args(&argv) {
        CliAction::Run { port, config } => (port, config),
        CliAction::Usage => usage(),
    };
    let (config, path) = load_user_config(explicit.as_deref());
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap_or_else(|error| {
        eprintln!("nexus-server: cannot bind 127.0.0.1:{port} ({error})");
        std::process::exit(1);
    });
    eprintln!(
        "nexus-server M0 TEST-ONLY demo: loopback API on 127.0.0.1:{port} (scripted fakes, no auth; never expose)"
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .build()
        .unwrap_or_else(|error| {
            eprintln!("nexus-server: executor failed to start ({error})");
            std::process::exit(1);
        });
    let mut server = Server::new(runtime.handle().clone());
    server.set_config(config, path);
    let server = Arc::new(server);
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let server = Arc::clone(&server);
                thread::spawn(move || server.handle_connection(stream));
            }
            Err(error) => eprintln!("nexus-server: accept failed ({error})"),
        }
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

    use super::{CliAction, DEFAULT_PORT, parse_args};

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
    fn run(args: &[&str]) -> (u16, Option<String>) {
        match parse_args(&argv(args)) {
            CliAction::Run { port, config } => (port, config),
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
    /// other's value, so the two spellings must be equivalent.
    #[test]
    fn port_and_config_flags_combine_independently() {
        assert_eq!(
            run(&["--port", "9401", "--config", "/tmp/c.json"]),
            (9401, Some("/tmp/c.json".to_owned()))
        );
        assert_eq!(
            run(&["--config", "/tmp/c.json", "--port", "9401"]),
            (9401, Some("/tmp/c.json".to_owned()))
        );
    }
}
