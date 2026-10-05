#![forbid(unsafe_code)]

//! Real-tool wiring over the loopback API: with `--tools real` sessions,
//! the admitted `host_read` executes against the jailed root instead of a
//! double, while policy authorization and the approval flow stay unchanged.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::Value;

static ROOTS: AtomicUsize = AtomicUsize::new(0);

/// Isolated jail root with one readable fixture, removed on drop.
struct FixtureRoot(std::path::PathBuf);

impl FixtureRoot {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nexus-server-real-tools-{}-{}",
            std::process::id(),
            ROOTS.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp root builds");
        std::fs::write(dir.join("a"), b"real-bytes-123").expect("fixture writes");
        Self(dir)
    }
}

impl Drop for FixtureRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn address(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

fn round_trip(port: u16, raw: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(address(port)).expect("loopback connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout sets");
    stream.write_all(raw.as_bytes()).expect("request writes");
    let mut body = Vec::new();
    stream.read_to_end(&mut body).expect("response reads");
    let text = String::from_utf8_lossy(&body);
    let (head, payload) = text.split_once("\r\n\r\n").expect("framed response");
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .expect("status line")
        .parse()
        .expect("numeric status");
    (status, serde_json::from_str(payload).expect("JSON body"))
}

fn post(port: u16, path: &str, body: &str) -> (u16, Value) {
    round_trip(
        port,
        &format!(
            "POST {path} HTTP/1.1\r\nhost: x\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
}

/// Starts the server with real jailed reads rooted at `root`.
fn spawn_real_tools(root: &std::path::Path) -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port binds");
    let port = listener.local_addr().expect("port known").port();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .expect("test executor builds");
    let mut server = nexus_server::Server::new(runtime.handle().clone());
    server
        .set_tools_mode(nexus_server::ToolsMode::RealFiles {
            root: root.to_owned(),
        })
        .expect("temp root binds");
    let server = std::sync::Arc::new(server);
    std::thread::spawn(move || {
        std::mem::forget(runtime);
        for stream in listener.incoming().flatten() {
            let server = std::sync::Arc::clone(&server);
            std::thread::spawn(move || server.handle_connection(stream));
        }
    });
    port
}

/// Reads SSE frames until the first tool outcome lands, then returns it.
fn first_tool_outcome(port: u16, session: &str, run: &str) -> Value {
    let mut stream = TcpStream::connect(address(port)).expect("loopback connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .expect("timeout sets");
    stream
        .write_all(
            format!("GET /sessions/{session}/runs/{run}/events HTTP/1.1\r\nhost: x\r\n\r\n")
                .as_bytes(),
        )
        .expect("SSE subscribes");
    let mut buffer = Vec::new();
    loop {
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).expect("stream reads");
        assert!(read > 0, "stream stays open until the first outcome");
        buffer.extend_from_slice(&chunk[..read]);
        while let Some(end) = buffer
            .windows(2)
            .position(|window| window == b"\n\n")
            .map(|index| index + 2)
        {
            let frame = String::from_utf8_lossy(&buffer[..end]).into_owned();
            buffer.drain(..end);
            for line in frame.lines() {
                let Some(payload) = line.strip_prefix("data: ") else {
                    continue;
                };
                let event: Value = serde_json::from_str(payload).expect("event JSON");
                if event["kind"] == "tool-finished" {
                    return event["detail"]["outcome"].clone();
                }
            }
        }
    }
}

#[test]
fn admitted_reads_execute_against_the_jailed_root() {
    let root = FixtureRoot::new();
    let port = spawn_real_tools(&root.0);
    let (status, body) = post(port, "/sessions", "{}");
    assert_eq!(status, 201);
    let session = body["session"].as_str().expect("session token");

    let (status, body) = post(
        port,
        &format!("/sessions/{session}/runs"),
        r#"{"input":"read the fixture"}"#,
    );
    assert_eq!(status, 201, "submit accepted: {body}");
    let run = body["run"].as_str().expect("run id");

    // The scripted read call carries {"path":"a"}: the outcome content
    // must be the real file bytes, which no double could know.
    let outcome = first_tool_outcome(port, session, run);
    assert_eq!(outcome["status"], "Succeeded");
    assert_eq!(outcome["effect"], "KnownNotApplied");
    assert_eq!(outcome["content"], "real-bytes-123");
    assert_eq!(outcome["truncated"], false);

    // The run is parked on the scripted write approval meanwhile: a second
    // submit still reports the busy slot instead of disturbing it.
    let (status, _) = post(
        port,
        &format!("/sessions/{session}/runs"),
        r#"{"input":"second"}"#,
    );
    assert_eq!(status, 409);

    let (status, _) = post(
        port,
        &format!("/sessions/{session}/runs/{run}/cancel"),
        "{}",
    );
    assert_eq!(status, 200);
}

#[test]
fn unreadable_roots_are_rejected_before_serving() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_time()
        .build()
        .expect("test executor builds");
    let mut server = nexus_server::Server::new(runtime.handle().clone());
    assert!(
        server
            .set_tools_mode(nexus_server::ToolsMode::RealFiles {
                root: std::path::PathBuf::from("/nonexistent-nexus-tools-root-9f3a"),
            })
            .is_err(),
        "missing roots never reach session wiring"
    );
    let file = std::env::temp_dir().join(format!("nexus-server-root-file-{}", std::process::id()));
    std::fs::write(&file, b"x").expect("probe file writes");
    assert!(
        server
            .set_tools_mode(nexus_server::ToolsMode::RealFiles { root: file.clone() })
            .is_err(),
        "files are not roots"
    );
    let _ = std::fs::remove_file(&file);
}
