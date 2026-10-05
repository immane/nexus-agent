#![forbid(unsafe_code)]

//! Sessions bound to a configured provider serve the real OpenAI-compatible
//! adapter over loopback: selection is validated before any run is minted,
//! missing credentials fail without network touch, and the mock's text
//! reaches the terminal outcome. Fake sessions keep the demo script.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

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

fn get(port: u16, path: &str) -> (u16, Value) {
    round_trip(port, &format!("GET {path} HTTP/1.1\r\nhost: x\r\n\r\n"))
}

/// Loopback mock speaking just enough Chat Completions: one stop turn
/// with fixed text, recording hits and the raw request.
struct Mock {
    hits: Arc<AtomicUsize>,
    request: Arc<Mutex<Vec<u8>>>,
}

fn serve_mock() -> (String, Mock) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback binds");
    let port = listener.local_addr().expect("port known").port();
    let mock = Mock {
        hits: Arc::new(AtomicUsize::new(0)),
        request: Arc::new(Mutex::new(Vec::new())),
    };
    let captured = Mock {
        hits: Arc::clone(&mock.hits),
        request: Arc::clone(&mock.request),
    };
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                match stream.read_exact(&mut byte) {
                    Ok(()) => head.push(byte[0]),
                    Err(_) => break,
                }
                if head.len() > 65_536 {
                    break;
                }
            }
            let length: usize = String::from_utf8_lossy(&head)
                .lines()
                .filter_map(|line| {
                    line.strip_prefix("content-length:")
                        .or_else(|| line.strip_prefix("Content-Length:"))
                })
                .filter_map(|value| value.trim().parse().ok())
                .next()
                .unwrap_or(0);
            let mut rest = vec![0u8; length.min(1_048_576)];
            let _ = stream.read_exact(&mut rest);
            captured
                .request
                .lock()
                .expect("request log writable")
                .extend_from_slice(&head);
            captured
                .request
                .lock()
                .expect("request log writable")
                .extend_from_slice(&rest);
            captured.hits.fetch_add(1, Ordering::SeqCst);
            let body = r#"{"choices":[{"message":{"role":"assistant","content":"mock says hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":2}}"#;
            let response = format!(
                "HTTP/1.1 200 Test\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), mock)
}

fn configured(endpoint: &str, credential: &str) -> nexus_config::UserConfig {
    let mut config = nexus_config::UserConfig::default_config();
    config
        .add_provider(
            nexus_config::ProviderProfile::new(
                "mock",
                "Mock",
                nexus_config::AdapterKind::Direct,
                Some(endpoint.to_owned()),
                nexus_config::CredentialRef::env_var(credential).expect("valid"),
                "mock-model",
            )
            .expect("profile builds"),
        )
        .expect("provider admits");
    config
        .add_model(nexus_config::ModelEntry::new("m", "mock", "mock-model").expect("valid"))
        .expect("model admits");
    config
        .add_model(nexus_config::ModelEntry::new("other", "mock", "mock-model-2").expect("valid"))
        .expect("model admits");
    config
}

/// Starts the API server with the given document (in memory only).
fn spawn_server(config: nexus_config::UserConfig) -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port binds");
    let port = listener.local_addr().expect("port known").port();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .expect("test executor builds");
    let mut server = nexus_server::Server::new(runtime.handle().clone());
    server.set_config(config, None);
    let server = Arc::new(server);
    std::thread::spawn(move || {
        std::mem::forget(runtime);
        for stream in listener.incoming().flatten() {
            let server = Arc::clone(&server);
            std::thread::spawn(move || server.handle_connection(stream));
        }
    });
    port
}

fn create_session(port: u16, body: &str) -> (u16, Value) {
    post(port, "/sessions", body)
}

/// Reads SSE frames until the terminal event, returning every data event.
fn events_until_terminal(port: u16, session: &str, run: &str) -> Vec<Value> {
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
    let mut events = Vec::new();
    loop {
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).expect("stream reads");
        assert!(read > 0, "stream stays open until the terminal event");
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
                let terminal = event["terminal"] == true;
                events.push(event);
                if terminal {
                    return events;
                }
            }
        }
    }
}

#[test]
fn selection_is_validated_before_any_run_is_minted() {
    let (base, _) = serve_mock();
    let port = spawn_server(configured(&base, "PATH"));
    let (status, _) = create_session(port, r#"{"provider":"ghost"}"#);
    assert_eq!(status, 400);
    let (status, _) = create_session(port, r#"{"provider":"mock","model":"ghost"}"#);
    assert_eq!(status, 400);
    let (status, _) = create_session(port, r#"{"model":"m"}"#);
    assert_eq!(status, 400, "a model without its provider is rejected");
    let (status, _) = create_session(port, r#"{"provider":42}"#);
    assert_eq!(status, 400, "non-string identities are invalid");
    // Default sessions keep the demo script untouched.
    let (status, body) = create_session(port, "{}");
    assert_eq!(status, 201);
    assert!(body.get("provider").is_none());
    assert!(body.get("model").is_none());
}

#[test]
fn missing_credentials_fail_without_network_touch() {
    let (base, mock) = serve_mock();
    let port = spawn_server(configured(&base, "NEXUS_SERVER_TEST_ABSENT_ZZZ"));
    let (status, body) = create_session(port, r#"{"provider":"mock"}"#);
    assert_eq!(status, 503);
    assert_eq!(body["error"], "provider credential is unavailable");
    assert_eq!(
        mock.hits.load(Ordering::SeqCst),
        0,
        "no socket opens without a credential"
    );
}

#[test]
fn real_sessions_complete_with_mock_text_and_fake_sessions_coexist() {
    let (base, mock) = serve_mock();
    let port = spawn_server(configured(&base, "PATH"));

    let (status, body) = create_session(port, r#"{"provider":"mock","model":"m"}"#);
    assert_eq!(status, 201, "selection accepted: {body}");
    assert_eq!(body["provider"], "mock");
    assert_eq!(body["model"], "m");
    let session = body["session"].as_str().expect("session token");

    let (status, body) = post(
        port,
        &format!("/sessions/{session}/runs"),
        r#"{"input":"hello mock"}"#,
    );
    assert_eq!(status, 201, "submit accepted: {body}");
    let run = body["run"].as_str().expect("run id");

    let events = events_until_terminal(port, session, run);
    let terminal = events.last().expect("terminal event");
    assert_eq!(terminal["kind"], "run-finished");
    assert_eq!(terminal["detail"]["outcome"], "completed");
    let texts: Vec<&str> = events
        .iter()
        .filter(|event| event["kind"] == "assistant-text")
        .filter_map(|event| event["detail"]["text"].as_str())
        .collect();
    assert_eq!(texts, vec!["mock says hi"]);

    let raw = mock.request.lock().expect("request readable").clone();
    let text = String::from_utf8_lossy(&raw);
    assert!(text.starts_with("POST /v1/chat/completions HTTP/1.1"));
    assert!(text.contains("authorization: Bearer "));
    assert!(text.contains(r#""model":"mock-model""#));

    let (status, snapshot) = get(port, &format!("/sessions/{session}/snapshot?run={run}"));
    assert_eq!(status, 200);
    assert_eq!(snapshot["lifecycle"], "finalized");
    assert_eq!(snapshot["outcome"], "completed");

    // A default session in the same process still serves the demo script.
    let (status, _) = create_session(port, "{}");
    assert_eq!(status, 201);
}
