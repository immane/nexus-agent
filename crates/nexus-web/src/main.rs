//! `nexus-web` binary: loopback HTTP frontend for the agent runtime.
//!
//! TEST-ONLY demo. Binds `127.0.0.1` only and serves scripted fakes: any
//! local process can submit, approve, and cancel. There is no
//! authentication. Do not expose this server to a network.

#![forbid(unsafe_code)]

use std::net::TcpListener;
use std::sync::Arc;
use std::thread;

use nexus_web::Server;

/// Default loopback port.
const DEFAULT_PORT: u16 = 8471;

fn usage() -> ! {
    eprintln!("usage: nexus-web [--port N] [--help]");
    eprintln!("  Serves the M0 test-only demo API on 127.0.0.1:N (default {DEFAULT_PORT}).");
    std::process::exit(2);
}

fn parse_port(argv: &[String]) -> u16 {
    let mut port = DEFAULT_PORT;
    let mut args = argv.iter().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => usage(),
            "--port" => match args.next().and_then(|value| value.parse().ok()) {
                Some(value) => port = value,
                None => usage(),
            },
            _ => usage(),
        }
    }
    port
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let port = parse_port(&argv);
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap_or_else(|error| {
        eprintln!("nexus-web: cannot bind 127.0.0.1:{port} ({error})");
        std::process::exit(1);
    });
    eprintln!(
        "nexus-web M0 TEST-ONLY demo: loopback API on 127.0.0.1:{port} (scripted fakes, no auth; never expose)"
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .build()
        .unwrap_or_else(|error| {
            eprintln!("nexus-web: executor failed to start ({error})");
            std::process::exit(1);
        });
    let server = Arc::new(Server::new(runtime.handle().clone()));
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let server = Arc::clone(&server);
                thread::spawn(move || server.handle_connection(stream));
            }
            Err(error) => eprintln!("nexus-web: accept failed ({error})"),
        }
    }
}
