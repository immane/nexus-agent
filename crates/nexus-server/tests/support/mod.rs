#![forbid(unsafe_code)]

//! Shared loopback HTTP mock for provider-bound server sessions.
//!
//! The mock speaks just enough Chat Completions to complete one stop turn
//! with fixed text, recording its hit count and the raw request. It is the
//! only boundary that proves which adapter a bound session actually invoked,
//! so the acceptance assertions in the server suites share it here.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Loopback mock speaking just enough Chat Completions: one stop turn
/// with fixed text, recording hits and the raw request.
pub struct Mock {
    /// Number of requests the mock has answered.
    pub hits: Arc<AtomicUsize>,
    /// Raw bytes of every request the mock has received, concatenated.
    pub request: Arc<Mutex<Vec<u8>>>,
}

/// Starts the mock on an ephemeral loopback port and returns its base URL
/// (`http://127.0.0.1:<port>/v1`) with the recording handle.
pub fn serve_mock() -> (String, Mock) {
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
