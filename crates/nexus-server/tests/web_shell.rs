//! Embedded assets remain public without weakening API authentication.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn embedded_assets_keep_host_origin_and_api_authentication_checks() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .unwrap();
    let mut server = nexus_server::Server::new(runtime.handle().clone());
    server.set_auth_token(&"a".repeat(64)).unwrap();
    let server = Arc::new(server);
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let host = format!("127.0.0.1:{port}");
    let cases = [
        ("GET", "/", host.as_str(), "", 200),
        ("GET", "/assets/app.css", host.as_str(), "", 200),
        ("GET", "/assets/app.js?v=1", host.as_str(), "", 200),
        ("GET", "/config", host.as_str(), "", 401),
        ("POST", "/", host.as_str(), "", 401),
        ("GET", "/assets/../app.js", host.as_str(), "", 401),
        ("GET", "/", "evil.test", "", 403),
        (
            "GET",
            "/",
            host.as_str(),
            "origin: http://evil.test\r\n",
            403,
        ),
        ("GET", "/", host.as_str(), "origin: null\r\n", 403),
    ];
    let count = cases.len();
    let worker = std::thread::spawn(move || {
        for stream in listener.incoming().take(count) {
            server.handle_connection(stream.unwrap());
        }
    });
    for (method, path, host, extra, expected) in cases {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nhost: {host}\r\n{extra}\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(
            response.starts_with(&format!("HTTP/1.1 {expected} ")),
            "{path}: {response}"
        );
        if expected == 200 {
            assert!(response.contains("cache-control: no-cache\r\n"));
            assert!(response.contains("frame-ancestors 'none'"));
            assert!(response.contains("x-content-type-options: nosniff\r\n"));
            let (head, body) = response.split_once("\r\n\r\n").unwrap();
            assert!(head.contains(&format!("content-length: {}\r\n", body.len())));
            match path {
                "/" => {
                    assert!(head.contains("text/html; charset=utf-8"));
                    assert!(body.contains("id=\"composer\""));
                    assert!(body.contains("Live session client"));
                }
                "/assets/app.css" => assert!(head.contains("text/css; charset=utf-8")),
                _ => assert!(head.contains("text/javascript; charset=utf-8")),
            }
        }
    }
    worker.join().unwrap();
}
