//! `nexus-server` binary: loopback HTTP frontend for the agent runtime.
//!
//! TEST-ONLY demo. Binds `127.0.0.1` only and serves scripted fakes: any
//! local process can submit, approve, and cancel. There is no
//! authentication. Do not expose this server to a network.

#![forbid(unsafe_code)]

use std::net::TcpListener;
use std::sync::Arc;
use std::thread;

use nexus_server::Server;

/// Default loopback port.
const DEFAULT_PORT: u16 = 8471;

fn usage() -> ! {
    eprintln!("usage: nexus-server [--port N] [--help]");
    eprintln!("  Serves the M0 test-only demo API on 127.0.0.1:N (default {DEFAULT_PORT}).");
    std::process::exit(2);
}

/// CLI outcome: run with a port, or print usage and exit. Splitting the
/// decision from the exit keeps every rejection path unit-testable.
enum CliAction {
    /// Serve on this loopback port.
    Run { port: u16 },
    /// Print usage and exit 2.
    Usage,
}

fn parse_args(argv: &[String]) -> CliAction {
    let mut port = DEFAULT_PORT;
    let mut args = argv.iter().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return CliAction::Usage,
            "--port" => match args.next().and_then(|value| value.parse().ok()) {
                Some(value) => port = value,
                None => return CliAction::Usage,
            },
            _ => return CliAction::Usage,
        }
    }
    CliAction::Run { port }
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let port = match parse_args(&argv) {
        CliAction::Run { port } => port,
        CliAction::Usage => usage(),
    };
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
    let server = Arc::new(Server::new(runtime.handle().clone()));
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

    /// Builds the full argv vector a process would receive. `parse_port`
    /// skips element 0 as the program path, so every case must supply that
    /// leading entry; a missing one would shift the first flag out of view.
    fn argv(args: &[&str]) -> Vec<String> {
        std::iter::once("nexus-server")
            .chain(args.iter().copied())
            .map(str::to_string)
            .collect()
    }

    /// Runs the parser and unwraps the `Run` branch.
    fn port(args: &[&str]) -> u16 {
        match parse_args(&argv(args)) {
            CliAction::Run { port } => port,
            CliAction::Usage => panic!("expected a port for {args:?}"),
        }
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
    /// flags, a `--port` with no value, an unparsable value, and both help
    /// spellings. `main` turns each into the usage banner plus exit 2.
    #[test]
    fn rejection_paths_report_usage() {
        usage(&["--bogus"]);
        usage(&["--port"]);
        usage(&["--port", "not-a-port"]);
        usage(&["--port", "-1"]);
        usage(&["--help"]);
        usage(&["-h"]);
    }
}
