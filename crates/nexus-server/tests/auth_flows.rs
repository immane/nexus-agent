//! HTTP trust-boundary regression tests for authenticated real-tool servers.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

const TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn round_trip(port: u16, raw: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout sets");
    stream.write_all(raw.as_bytes()).expect("request writes");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("response reads");
    let status = response
        .split_whitespace()
        .nth(1)
        .expect("status exists")
        .parse()
        .expect("status is numeric");
    let body = response.split_once("\r\n\r\n").expect("response framed").1;
    (status, body.to_owned())
}

fn request(authorization: Option<&str>, host: &str, origin: Option<&str>) -> String {
    let auth = authorization
        .map(|value| format!("authorization: {value}\r\n"))
        .unwrap_or_default();
    let origin = origin
        .map(|value| format!("origin: {value}\r\n"))
        .unwrap_or_default();
    format!(
        "POST /sessions HTTP/1.1\r\nhost: {host}\r\n{auth}{origin}content-length: 2\r\n\r\n{{}}"
    )
}

#[test]
fn real_tool_http_requires_bearer_and_rejects_untrusted_host_or_origin() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("nexus-auth-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&root).expect("temp root exists");
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback binds");
    let port = listener.local_addr().expect("port known").port();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .expect("runtime builds");
    let mut server = nexus_server::Server::new(runtime.handle().clone());
    server
        .set_tools_mode(nexus_server::ToolsMode::RealFiles { root: root.clone() })
        .expect("real tools configure");
    server.set_auth_token(TOKEN).expect("token validates");
    let server = Arc::new(server);
    let serving = Arc::clone(&server);
    std::thread::spawn(move || {
        std::mem::forget(runtime);
        for stream in listener.incoming().flatten() {
            let server = Arc::clone(&serving);
            std::thread::spawn(move || server.handle_connection(stream));
        }
    });

    let host = format!("127.0.0.1:{port}");
    let (status, body) = round_trip(port, &request(None, &host, None));
    assert_eq!(status, 401, "missing credentials are refused: {body}");
    assert!(!body.contains(TOKEN), "credential is never echoed");

    let (status, _) = round_trip(
        port,
        &format!(
            "POST /sessions?token={TOKEN} HTTP/1.1\r\nhost: {host}\r\ncontent-length: 2\r\n\r\n{{}}"
        ),
    );
    assert_eq!(status, 401, "URL credentials are not accepted");

    let (status, _) = round_trip(port, &request(Some("Bearer wrong"), &host, None));
    assert_eq!(status, 401, "invalid credentials are refused");

    for (method, path) in [
        ("POST", "/sessions/sess-web-1/runs/run-x/approve"),
        ("POST", "/sessions/sess-web-1/runs/run-x/deny"),
        ("POST", "/sessions/sess-web-1/runs/run-x/cancel"),
        ("GET", "/sessions/sess-web-1/snapshot?run=run-x"),
    ] {
        let (status, _) = round_trip(
            port,
            &format!("{method} {path} HTTP/1.1\r\nhost: {host}\r\n\r\n"),
        );
        assert_eq!(status, 401, "unauthorized control route {method} {path}");
    }

    let (status, _) = round_trip(
        port,
        &request(Some(&format!("Bearer {TOKEN}")), "evil.example", None),
    );
    assert_eq!(status, 403, "untrusted Host is refused even with a token");

    let (status, _) = round_trip(
        port,
        &request(
            Some(&format!("Bearer {TOKEN}")),
            &host,
            Some("http://evil.example"),
        ),
    );
    assert_eq!(status, 403, "cross-origin requests are refused");

    let (status, body) = round_trip(
        port,
        &request(
            Some(&format!("Bearer {TOKEN}")),
            &host,
            Some(&format!("http://{host}")),
        ),
    );
    assert_eq!(
        status, 201,
        "valid same-origin authorized request succeeds: {body}"
    );
    assert!(
        body.contains("sess-web-1"),
        "rejected requests did not mint sessions"
    );
    let _ = std::fs::remove_dir_all(root);
}
