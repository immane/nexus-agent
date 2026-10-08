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
            "POST {path} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\nauthorization: Bearer {}\r\ncontent-length: {}\r\n\r\n{body}",
            "a".repeat(64),
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
    server
        .set_auth_token(&"a".repeat(64))
        .expect("test token valid");
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

/// Waits for the terminal event, then returns exactly `wanted` tool
/// outcomes. Tool completion alone does not mean the run slot is idle.
fn tool_outcomes(port: u16, session: &str, run: &str, wanted: usize) -> Vec<Value> {
    let mut stream = TcpStream::connect(address(port)).expect("loopback connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .expect("timeout sets");
    stream
        .write_all(
            format!("GET /sessions/{session}/runs/{run}/events HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\nauthorization: Bearer {}\r\n\r\n", "a".repeat(64))
                .as_bytes(),
        )
        .expect("SSE subscribes");
    let mut outcomes = Vec::new();
    let mut buffer = Vec::new();
    loop {
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).expect("stream reads");
        assert!(read > 0, "stream stays open until run-finished");
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
                    outcomes.push(event["detail"]["outcome"].clone());
                }
                if event["kind"] == "run-finished" {
                    assert_eq!(outcomes.len(), wanted);
                    return outcomes;
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
    // must be the real file bytes, which no double could know. The
    // scripted write call carries path-only {"path":"b"}: under the real
    // writer's closed schema that is denied at admission, before any
    // approval or execution, so no file is created.
    let outcomes = tool_outcomes(port, session, run, 2);
    let read = outcomes
        .iter()
        .find(|outcome| outcome["status"] == "Succeeded")
        .expect("the admitted read succeeds");
    assert_eq!(read["effect"], "KnownNotApplied");
    assert_eq!(read["content"], "real-bytes-123");
    assert_eq!(read["truncated"], false);
    let denied = outcomes
        .iter()
        .find(|outcome| outcome["status"] == "Denied")
        .expect("the path-only write is denied at admission");
    assert_eq!(denied["effect"], "NotStarted");
    assert!(!root.0.join("b").exists(), "a denied write creates nothing");

    // Admission denied the write, so nothing parks on approval: the stop
    // turn completes the run and the slot accepts new work.
    let (status, body) = post(
        port,
        &format!("/sessions/{session}/runs"),
        r#"{"input":"second"}"#,
    );
    assert_eq!(status, 201, "the slot is free after completion: {body}");
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
