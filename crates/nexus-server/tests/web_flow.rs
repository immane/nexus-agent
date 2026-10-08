#![forbid(unsafe_code)]

//! End-to-end loopback API flow over real TCP: sessions isolate runs,
//! approvals bind the exact runtime identity, terminal outcomes close the
//! stream, and a second task replays the demo script instead of hitting an
//! exhausted script.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::Value;

fn base(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

/// Sends one raw request and reads the framed response (the server always
/// closes outside SSE). Returns the status plus the raw body.
fn round_trip(port: u16, raw: &str) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(base(port)).expect("loopback connects");
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
    (status, payload.as_bytes().to_vec())
}

fn get(port: u16, path: &str) -> (u16, Value) {
    let (status, body) = round_trip(
        port,
        &format!("GET {path} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\n\r\n"),
    );
    (status, serde_json::from_slice(&body).expect("JSON body"))
}

fn post(port: u16, path: &str, body: &str) -> (u16, Value) {
    let (status, raw) = round_trip(
        port,
        &format!(
            "POST {path} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        ),
    );
    (status, serde_json::from_slice(&raw).expect("JSON body"))
}

/// Reads SSE frames until the run's terminal event, approving the live
/// grant mid-stream with its exact runtime identity. Returns the terminal
/// outcome name.
fn drive_run_to_terminal(port: u16, session: &str, run: &str) -> String {
    let mut stream = TcpStream::connect(base(port)).expect("loopback connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .expect("timeout sets");
    stream
        .write_all(
            format!("GET /sessions/{session}/runs/{run}/events HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\n\r\n")
                .as_bytes(),
        )
        .expect("SSE subscribes");
    let mut buffer = Vec::new();
    let mut approved = false;
    loop {
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).expect("stream reads");
        assert!(read > 0, "stream stays open until the terminal event");
        buffer.extend_from_slice(&chunk[..read]);
        while let Some(end) = find_frame_end(&buffer) {
            let frame = String::from_utf8_lossy(&buffer[..end]).into_owned();
            buffer.drain(..end);
            for line in frame.lines() {
                let Some(payload) = line.strip_prefix("data: ") else {
                    continue;
                };
                let event: Value = serde_json::from_str(payload).expect("event JSON");
                if event["kind"] == "approval-required" && !approved {
                    approved = true;
                    let approval = event["detail"]["approval"].as_str().expect("grant id");
                    let call = event["detail"]["call"].as_str().expect("call id");
                    let (status, reply) = post(
                        port,
                        &format!("/sessions/{session}/runs/{run}/approve"),
                        &format!(r#"{{"approval":{approval:?},"call":{call:?}}}"#),
                    );
                    assert_eq!(status, 200, "exact grant approves: {reply}");
                    assert_eq!(reply["reply"], "accepted");
                }
                if event["terminal"] == true {
                    assert_eq!(event["kind"], "run-finished");
                    return event["detail"]["outcome"]
                        .as_str()
                        .expect("outcome")
                        .to_owned();
                }
            }
        }
    }
}

/// Finds the end of one `\n\n`-terminated frame, if complete.
fn find_frame_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| index + 2)
}

/// Starts the demo server on an ephemeral loopback port.
fn spawn_server() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port binds");
    let port = listener.local_addr().expect("port known").port();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .expect("test executor builds");
    let server = std::sync::Arc::new(nexus_server::Server::new(runtime.handle().clone()));
    std::thread::spawn(move || {
        // The executor outlives the test: it is reclaimed at process exit,
        // after the last assertion runs.
        std::mem::forget(runtime);
        for stream in listener.incoming().flatten() {
            let server = std::sync::Arc::clone(&server);
            std::thread::spawn(move || server.handle_connection(stream));
        }
    });
    port
}

#[test]
fn health_reports_the_test_only_scope() {
    let port = spawn_server();
    let (status, body) = get(port, "/health");
    assert_eq!(status, 200);
    assert_eq!(body["status"], "ok");
}

#[test]
fn unknown_routes_sessions_and_runs_are_rejected_statically() {
    let port = spawn_server();
    let (status, _) = get(port, "/nope");
    assert_eq!(status, 404);
    let (status, _) = get(port, "/sessions/sess-web-999/snapshot?run=run-1");
    assert_eq!(status, 404);
    let (status, _) = post(port, "/sessions/sess-web-999/runs/run-1/cancel", "{}");
    assert_eq!(status, 404);
    let (status, _) = post(
        port,
        "/sessions/sess-web-999/runs/run-1/approve",
        r#"{"approval":"a1","call":"c1"}"#,
    );
    assert_eq!(status, 404);
    let (status, body) = post(port, "/sessions", "{}");
    assert_eq!(status, 201);
    let session = body["session"].as_str().expect("session token");
    let (status, _) = post(
        port,
        &format!("/sessions/{session}/runs/run-1/approve"),
        r#"{"approval":"!!!","call":"???"}"#,
    );
    assert_eq!(status, 400, "malformed identities are invalid input");
    let (status, _) = post(
        port,
        &format!("/sessions/{session}/runs/does-not-exist/cancel"),
        "{}",
    );
    assert_eq!(status, 404, "unknown runs are missing targets");
}

#[test]
fn two_tasks_complete_with_exact_grants_and_sessions_isolate() {
    let port = spawn_server();
    let (status, body) = post(port, "/sessions", "{}");
    assert_eq!(status, 201);
    let first = body["session"].as_str().expect("session token");
    let (status, body) = post(port, "/sessions", "{}");
    assert_eq!(status, 201);
    let second = body["session"].as_str().expect("session token");
    assert_ne!(first, second, "sessions mint distinct tokens");

    for task in ["first demo task", "second demo task"] {
        let (status, body) = post(
            port,
            &format!("/sessions/{first}/runs"),
            &format!(r#"{{"input":{task:?}}}"#),
        );
        assert_eq!(status, 201, "every task is accepted: {body}");
        assert_eq!(body["reply"], "accepted");
        let run = body["run"].as_str().expect("run id");
        let outcome = drive_run_to_terminal(port, first, run);
        assert_eq!(outcome, "completed", "granted demo runs complete");
        let (status, snapshot) = get(port, &format!("/sessions/{first}/snapshot?run={run}"));
        assert_eq!(status, 200);
        assert_eq!(snapshot["lifecycle"], "finalized");
        assert_eq!(snapshot["outcome"], "completed");
    }

    // The sibling session was never blocked by the first: its own task runs.
    let (status, body) = post(
        port,
        &format!("/sessions/{second}/runs"),
        r#"{"input":"sibling task"}"#,
    );
    assert_eq!(status, 201);
    let run = body["run"].as_str().expect("run id");
    let outcome = drive_run_to_terminal(port, second, run);
    assert_eq!(outcome, "completed");
}
