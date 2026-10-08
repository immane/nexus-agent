#![forbid(unsafe_code)]

//! Transport-level tests for [`nexus_server::http`]: raw bytes travel over a
//! real loopback TCP pair into `read_request`, and every documented rejection
//! is pinned to its status and static diagnostic. Response framing is
//! compared byte for byte, so a header rename or a missing terminator fails
//! here rather than in the demo wiring.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::time::Duration;

use nexus_server::http::{
    IO_TIMEOUT, MAX_BODY_BYTES, MAX_HEAD_BYTES, Request, Response, WRITE_TIMEOUT, read_request,
    reason, write_response, write_sse_headers,
};

/// Socket timeouts for the test peer. Every case here either completes or
/// half-closes, so a regression that would wait for the transport timeout
/// fails fast instead of parking the suite.
const PEER_TIMEOUT: Duration = Duration::from_secs(5);

/// Fixed parts of the padded head built by `padded_head`.
const PAD_PREFIX: &[u8] = b"GET /padded HTTP/1.1\r\nX-Pad: ";
const PAD_SUFFIX: &[u8] = b"\r\n\r\n";

/// A real loopback connection: `client` stands in for the peer writing raw
/// bytes, `server` is the accepted connection the parser reads.
struct TcpPair {
    client: TcpStream,
    server: TcpStream,
}

impl TcpPair {
    fn new() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral loopback port binds");
        let address = listener.local_addr().expect("bound address is known");
        let client = TcpStream::connect(address).expect("loopback connects");
        let (server, _) = listener.accept().expect("pair accepts");
        client
            .set_read_timeout(Some(PEER_TIMEOUT))
            .expect("client read timeout sets");
        client
            .set_write_timeout(Some(PEER_TIMEOUT))
            .expect("client write timeout sets");
        server
            .set_write_timeout(Some(PEER_TIMEOUT))
            .expect("server write timeout sets");
        Self { client, server }
    }

    /// Sends raw request bytes, then parses whatever arrived.
    fn parse(raw: &[u8]) -> Result<Request, (u16, &'static str)> {
        let mut pair = Self::new();
        pair.client.write_all(raw).expect("raw request writes");
        pair.parse_on_server()
    }

    fn parse_on_server(&mut self) -> Result<Request, (u16, &'static str)> {
        read_request(&mut self.server).map_err(|error| (error.status, error.message))
    }

    /// Sends raw bytes, half-closes the peer's write side, then parses.
    fn parse_until_eof(raw: &[u8]) -> Result<Request, (u16, &'static str)> {
        let mut pair = Self::new();
        pair.client.write_all(raw).expect("raw request writes");
        pair.client
            .shutdown(Shutdown::Write)
            .expect("peer half-closes");
        pair.parse_on_server()
    }
}

/// Parses a well-formed-text request that must be accepted.
#[track_caller]
fn accept(raw: &str) -> Request {
    TcpPair::parse(raw.as_bytes()).unwrap_or_else(|error| panic!("{raw:?} was rejected: {error:?}"))
}

/// Asserts the status of a rejected request and returns its diagnostic.
#[track_caller]
fn reject(raw: &str, status: u16) -> &'static str {
    match TcpPair::parse(raw.as_bytes()) {
        Ok(_) => panic!("{raw:?} must be rejected with {status}"),
        Err((actual, message)) => {
            assert_eq!(actual, status, "wrong status for {raw:?}");
            message
        }
    }
}

/// Asserts the status and static diagnostic of a rejected request.
#[track_caller]
fn assert_rejected(
    result: Result<Request, (u16, &'static str)>,
    label: &str,
    status: u16,
    message: &'static str,
) {
    match result {
        Ok(_) => panic!("{label} must be rejected with {status}"),
        Err((actual, actual_message)) => {
            assert_eq!(actual, status, "wrong status for {label}");
            assert_eq!(actual_message, message, "wrong diagnostic for {label}");
        }
    }
}

/// Same as `reject`, for requests whose bytes are not text.
fn reject_bytes(raw: &[u8], status: u16, message: &'static str) {
    assert_rejected(TcpPair::parse(raw), &format!("{raw:?}"), status, message);
}

/// Same as `reject`, for a peer that half-closes after `raw`.
fn reject_until_eof(raw: &[u8], status: u16, message: &'static str) {
    assert_rejected(
        TcpPair::parse_until_eof(raw),
        &format!("{raw:?}"),
        status,
        message,
    );
}

/// Builds a request head of exactly `total` bytes that ends with the blank
/// line, using one padded header, followed by `body`.
fn padded_head(total: usize, body: &[u8]) -> Vec<u8> {
    assert!(total > PAD_PREFIX.len() + PAD_SUFFIX.len());
    let mut head = Vec::with_capacity(total + body.len());
    head.extend_from_slice(PAD_PREFIX);
    head.resize(total - PAD_SUFFIX.len(), b'a');
    head.extend_from_slice(PAD_SUFFIX);
    head.extend_from_slice(body);
    head
}

/// Reads the framed bytes a peer sees after the server closed its side.
fn read_all(stream: &mut TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).expect("peer reads to EOF");
    bytes
}

// --- Accepted request shapes ------------------------------------------------

#[test]
fn get_request_parses_without_a_body() {
    let request = accept("GET /health HTTP/1.1\r\nhost: localhost\r\n\r\n");
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/health");
    assert_eq!(request.header("host"), Some("localhost"));
    assert!(request.body.is_empty(), "no content-length means no body");
    assert_eq!(request.header("content-type"), None);
}

#[test]
fn post_request_reads_the_declared_body_exactly() {
    let request =
        accept("POST /sessions HTTP/1.1\r\nhost: x\r\ncontent-length: 12\r\n\r\nhello, world");
    assert_eq!(request.method, "POST");
    assert_eq!(request.body, b"hello, world");
    assert_eq!(request.header("content-length"), Some("12"));
}

#[test]
fn a_body_at_the_size_limit_is_read_in_full() {
    let mut pair = TcpPair::new();
    let head = format!("POST /big HTTP/1.1\r\ncontent-length: {MAX_BODY_BYTES}\r\n\r\n");
    let writer = {
        // A body the size cap allows may not fit the socket buffer, so the
        // peer writes from its own thread while the server drains it.
        let client = pair.client.try_clone().expect("client handle clones");
        let head = head.into_bytes();
        std::thread::spawn(move || {
            let mut client = client;
            client.write_all(&head).expect("head writes");
            let body = vec![b'x'; MAX_BODY_BYTES];
            client.write_all(&body).expect("body writes");
        })
    };
    let request = pair
        .parse_on_server()
        .unwrap_or_else(|error| panic!("body at the limit was rejected: {error:?}"));
    writer.join().expect("peer writer finishes");
    assert_eq!(request.body.len(), MAX_BODY_BYTES);
    assert!(request.body.iter().all(|byte| *byte == b'x'));
}

#[test]
fn post_with_zero_or_missing_content_length_reads_an_empty_body() {
    let declared = accept("POST /sessions HTTP/1.1\r\ncontent-length: 0\r\n\r\n");
    assert!(declared.body.is_empty());
    let undeclared = accept("POST /sessions HTTP/1.1\r\n\r\n");
    assert!(undeclared.body.is_empty());
    assert_eq!(undeclared.method, "POST");
}

#[test]
fn body_bytes_are_never_validated_as_text() {
    let mut pair = TcpPair::new();
    let body = [0xffu8, 0x00, 0xfe, 0x7f];
    pair.client
        .write_all(b"POST /raw HTTP/1.1\r\ncontent-length: 4\r\n\r\n")
        .expect("head writes");
    pair.client.write_all(&body).expect("body bytes write");
    let request = pair.parse_on_server().expect("opaque body parses");
    assert_eq!(request.body, body);
}

#[test]
fn header_lookup_is_case_insensitive_and_trims_values() {
    let request = accept(
        "POST /sessions HTTP/1.1\r\nCoNtEnT-LeNgTh: 2\r\n\tX-MiXeD-CaSe\t:   padded value   \r\nX-Empty:\r\n\r\nhi",
    );
    assert_eq!(request.header("content-length"), Some("2"));
    assert_eq!(request.header("x-mixed-case"), Some("padded value"));
    assert_eq!(request.header("x-absent"), None);
    assert_eq!(request.header("x-empty"), Some(""));
    assert_eq!(request.body, b"hi");
}

#[test]
fn a_lookup_name_is_not_folded_to_lower_case() {
    // Names are lowercased once on the way in; a lookup is an exact map hit,
    // so the caller must ask with the lower-case spelling.
    let request = accept("GET /x HTTP/1.1\r\nX-MiXeD: 1\r\n\r\n");
    assert_eq!(request.header("x-mixed"), Some("1"));
    assert_eq!(request.header("X-MIXED"), None);
}

#[test]
fn duplicate_header_names_keep_the_last_value() {
    let request = accept("GET /x HTTP/1.1\r\nx-trace: first\r\nx-trace: second\r\n\r\n");
    assert_eq!(request.header("x-trace"), Some("second"));
}

// --- Request line -----------------------------------------------------------

#[test]
fn malformed_request_lines_are_rejected() {
    let cases = [
        ("GET\r\n\r\n", "no path or version"),
        ("GET /x\r\n\r\n", "no version"),
        ("GET /x HTTP/1.1 extra\r\n\r\n", "fourth field"),
        ("GET /x SPDY/3.1\r\n\r\n", "unknown protocol"),
        ("GET /x HTTP\r\n\r\n", "version without the HTTP/ prefix"),
        ("\r\n\r\n", "empty request line"),
        ("GET  HTTP/1.1\r\n\r\n", "empty path"),
    ];
    for (raw, why) in cases {
        let message = reject(raw, 400);
        assert!(
            !message.is_empty(),
            "{why}: a static diagnostic is reported"
        );
    }
}

#[test]
fn a_head_that_is_not_text_is_rejected() {
    let mut raw = b"GET /x HTTP/1.1\r\nX-Bytes: ".to_vec();
    raw.push(0xff);
    raw.extend_from_slice(b"\r\n\r\n");
    reject_bytes(&raw, 400, "request head is not valid text");
}

#[test]
fn a_peer_that_closes_before_the_head_is_rejected() {
    reject_until_eof(b"", 400, "request could not be read");
}

// --- Method and path --------------------------------------------------------

#[test]
fn unsupported_methods_are_rejected_with_405() {
    for raw in [
        "PUT /x HTTP/1.1\r\n\r\n",
        "HEAD /x HTTP/1.1\r\n\r\n",
        "PATCH /x HTTP/1.1\r\n\r\n",
        "OPTIONS * HTTP/1.1\r\n\r\n",
        "TRACE /x HTTP/1.1\r\n\r\n",
        "CONNECT /x HTTP/1.1\r\n\r\n",
    ] {
        assert_eq!(reject(raw, 405), "method is not supported", "{raw:?}");
    }
}

#[test]
fn method_matching_is_exact_and_case_sensitive() {
    for raw in [
        "get /x HTTP/1.1\r\n\r\n",
        "Get /x HTTP/1.1\r\n\r\n",
        "post /x HTTP/1.1\r\n\r\n",
    ] {
        reject(raw, 405);
    }
    assert_eq!(accept("GET /x HTTP/1.1\r\n\r\n").method, "GET");
    assert_eq!(accept("POST /x HTTP/1.1\r\n\r\n").method, "POST");
}

#[test]
fn paths_must_start_with_a_slash() {
    for raw in [
        "GET health HTTP/1.1\r\n\r\n",
        "GET * HTTP/1.1\r\n\r\n",
        "GET http://example.test/x HTTP/1.1\r\n\r\n",
        "GET . HTTP/1.1\r\n\r\n",
    ] {
        assert_eq!(reject(raw, 400), "request path is malformed", "{raw:?}");
    }
    assert_eq!(accept("GET / HTTP/1.1\r\n\r\n").path, "/");
}

// --- Headers ----------------------------------------------------------------

#[test]
fn malformed_header_lines_are_rejected() {
    assert_eq!(
        reject("GET /x HTTP/1.1\r\nno-colon-here\r\n\r\n", 400),
        "header line is malformed"
    );
    for raw in [
        "GET /x HTTP/1.1\r\n: value\r\n\r\n",
        "GET /x HTTP/1.1\r\n:value\r\n\r\n",
        "GET /x HTTP/1.1\r\nBad Name: value\r\n\r\n",
        "GET /x HTTP/1.1\r\nBad\tName: value\r\n\r\n",
    ] {
        assert_eq!(reject(raw, 400), "header name is malformed", "{raw:?}");
    }
}

#[test]
fn a_valid_head_before_a_malformed_header_is_not_accepted() {
    // The head terminator is not reached, so the peer closing is also a
    // rejection: a malformed tail must never be silently dropped.
    reject_until_eof(b"GET /x HTTP/1.1\r\n", 400, "request could not be read");
}

// --- Size limits ------------------------------------------------------------

#[test]
fn a_head_at_the_limit_is_accepted_and_one_byte_over_is_rejected() {
    let request = TcpPair::parse(&padded_head(MAX_HEAD_BYTES, b""))
        .unwrap_or_else(|error| panic!("head at the limit was rejected: {error:?}"));
    assert_eq!(request.route_path(), "/padded");
    let expected_pad = MAX_HEAD_BYTES - PAD_PREFIX.len() - PAD_SUFFIX.len();
    assert_eq!(request.header("x-pad").map(str::len), Some(expected_pad));

    let oversized = padded_head(MAX_HEAD_BYTES + 1, b"");
    reject_bytes(&oversized, 431, "request head is too large");
}

#[test]
fn a_content_length_past_the_body_limit_is_rejected() {
    let raw = format!(
        "POST /x HTTP/1.1\r\ncontent-length: {}\r\n\r\n",
        MAX_BODY_BYTES + 1
    );
    assert_eq!(
        reject(&raw, 413),
        "request body is too large",
        "the body is rejected without being read"
    );
}

#[test]
fn malformed_content_length_values_are_rejected() {
    for raw in [
        "POST /x HTTP/1.1\r\ncontent-length: abc\r\n\r\n",
        "POST /x HTTP/1.1\r\ncontent-length: -1\r\n\r\n",
        "POST /x HTTP/1.1\r\ncontent-length: 1.5\r\n\r\n",
        "POST /x HTTP/1.1\r\ncontent-length: 5 5\r\n\r\n",
        "POST /x HTTP/1.1\r\ncontent-length:\r\n\r\n",
        "POST /x HTTP/1.1\r\ncontent-length: 99999999999999999999\r\n\r\n",
        "POST /x HTTP/1.1\r\nContent-Length: 0x10\r\n\r\n",
    ] {
        assert_eq!(reject(raw, 400), "content length is malformed", "{raw:?}");
    }
}

#[test]
fn a_content_length_with_surrounding_whitespace_is_tolerated() {
    // Rust's integer parser accepts a leading `+`; the value is trimmed first.
    let signed = accept("POST /x HTTP/1.1\r\ncontent-length: +2\r\n\r\nhi");
    assert_eq!(signed.body, b"hi");
    let padded = accept("POST /x HTTP/1.1\r\ncontent-length:   2  \r\n\r\nhi");
    assert_eq!(padded.body, b"hi");
}

#[test]
fn a_truncated_body_is_rejected() {
    reject_until_eof(
        b"POST /x HTTP/1.1\r\ncontent-length: 10\r\n\r\nabc",
        400,
        "request body is truncated",
    );
}

#[test]
fn chunked_transfer_encoding_is_not_implemented() {
    for raw in [
        "POST /x HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n0\r\n\r\n",
        "POST /x HTTP/1.1\r\nTransfer-Encoding: CHUNKED\r\n\r\n0\r\n\r\n",
        "POST /x HTTP/1.1\r\ntransfer-encoding: gzip\r\n\r\n",
    ] {
        assert_eq!(
            reject(raw, 501),
            "chunked transfer encoding is not supported",
            "{raw:?}"
        );
    }
}

#[test]
fn an_identity_transfer_encoding_keeps_content_length_framing() {
    let request =
        accept("POST /x HTTP/1.1\r\nTransfer-Encoding: identity\r\ncontent-length: 2\r\n\r\nhi");
    assert_eq!(request.body, b"hi");
    let rejected =
        accept("POST /x HTTP/1.1\r\ntransfer-encoding: IDENTITY\r\ncontent-length: 2\r\n\r\nhi");
    assert_eq!(rejected.body, b"hi");
}

// --- Routing helpers --------------------------------------------------------

#[test]
fn route_path_strips_the_query() {
    assert_eq!(
        accept("GET /health HTTP/1.1\r\n\r\n").route_path(),
        "/health"
    );
    assert_eq!(
        accept("GET /sessions/s1/snapshot?run=r1 HTTP/1.1\r\n\r\n").route_path(),
        "/sessions/s1/snapshot"
    );
    let kept = accept("GET /x?a=1&b=2 HTTP/1.1\r\n\r\n");
    assert_eq!(kept.route_path(), "/x");
    assert_eq!(kept.path, "/x?a=1&b=2", "the raw path keeps the query");
    let empty = accept("GET /x? HTTP/1.1\r\n\r\n");
    assert_eq!(empty.route_path(), "/x");
    assert_eq!(empty.query("a"), None);
}

#[test]
fn query_returns_requested_parameters() {
    let request = accept("GET /x?run=r1&limit=5&token=a=b HTTP/1.1\r\n\r\n");
    assert_eq!(request.query("run"), Some("r1"));
    assert_eq!(request.query("limit"), Some("5"));
    assert_eq!(
        request.query("token"),
        Some("a=b"),
        "only the first `=` splits"
    );
    assert_eq!(request.query("missing"), None);
    assert_eq!(request.query("ru"), None, "names match exactly");
}

#[test]
fn query_returns_the_first_value_for_a_repeated_key() {
    let request = accept("GET /x?run=first&run=second HTTP/1.1\r\n\r\n");
    assert_eq!(request.query("run"), Some("first"));
}

#[test]
fn query_returns_none_for_an_empty_or_unpaired_query_string() {
    let empty = accept("GET /x HTTP/1.1\r\n\r\n");
    assert_eq!(empty.query("run"), None, "no query string at all");
    let bare = accept("GET /x?flag HTTP/1.1\r\n\r\n");
    assert_eq!(bare.query("run"), None, "a pair without `=` stops the scan");
    assert_eq!(bare.query("flag"), None);
    let mixed = accept("GET /x?run=r1&flag HTTP/1.1\r\n\r\n");
    assert_eq!(
        mixed.query("run"),
        Some("r1"),
        "matching pairs still resolve"
    );
    assert_eq!(mixed.query("flag"), None);
    let empty_value = accept("GET /x?run= HTTP/1.1\r\n\r\n");
    assert_eq!(empty_value.query("run"), Some(""));
    let extra = accept("GET /x?a=1?b=2 HTTP/1.1\r\n\r\n");
    assert_eq!(
        extra.query("a"),
        Some("1"),
        "only the segment before the second `?` is scanned"
    );
    assert_eq!(extra.query("b"), None);
}

// --- Response framing -------------------------------------------------------

#[test]
fn write_response_frames_the_head_and_body() {
    let mut pair = TcpPair::new();
    write_response(
        &mut pair.server,
        &Response::new(200, "application/json", br#"{"status":"ok"}"#.to_vec()),
    )
    .expect("response writes");
    pair.server
        .shutdown(Shutdown::Write)
        .expect("server half-closes");
    assert_eq!(
        read_all(&mut pair.client).as_slice(),
        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 15\r\nconnection: close\r\n\r\n{\"status\":\"ok\"}"
    );
}

#[test]
fn write_response_frames_an_empty_body() {
    let mut pair = TcpPair::new();
    write_response(
        &mut pair.server,
        &Response::new(404, "application/json", Vec::new()),
    )
    .expect("error response writes");
    pair.server
        .shutdown(Shutdown::Write)
        .expect("server half-closes");
    assert_eq!(
        read_all(&mut pair.client).as_slice(),
        b"HTTP/1.1 404 Not Found\r\ncontent-type: application/json\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    );
}

#[test]
fn write_sse_headers_writes_the_event_stream_head() {
    let mut pair = TcpPair::new();
    write_sse_headers(&mut pair.server).expect("SSE head writes");
    pair.server
        .shutdown(Shutdown::Write)
        .expect("server half-closes");
    assert_eq!(
        read_all(&mut pair.client).as_slice(),
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n"
    );
}

#[test]
fn write_sse_headers_leaves_the_stream_to_the_caller() {
    let mut pair = TcpPair::new();
    write_sse_headers(&mut pair.server).expect("SSE head writes");
    pair.server
        .write_all(b"id: 1\ndata: {\"kind\":\"run-started\"}\n\n")
        .expect("event writes");
    pair.server
        .shutdown(Shutdown::Write)
        .expect("server half-closes");
    let bytes = read_all(&mut pair.client);
    assert!(
        bytes.starts_with(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n"),
        "the head is written once and unterminated by a body"
    );
    assert_eq!(
        bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| &bytes[index + 4..]),
        Some(&b"id: 1\ndata: {\"kind\":\"run-started\"}\n\n"[..]),
        "frames follow the head verbatim"
    );
}

#[test]
fn sse_installs_a_finite_socket_write_timeout() {
    let mut pair = TcpPair::new();
    write_sse_headers(&mut pair.server).expect("SSE head writes");
    assert_eq!(
        pair.server.write_timeout().expect("timeout is readable"),
        Some(WRITE_TIMEOUT)
    );
    assert_eq!(WRITE_TIMEOUT, Duration::from_secs(30));
}

// --- Constants and reason phrases -------------------------------------------

#[test]
fn the_io_timeout_is_installed_on_the_connection() {
    let mut pair = TcpPair::new();
    pair.client
        .write_all(b"GET /health HTTP/1.1\r\nhost: x\r\n\r\n")
        .expect("request writes");
    pair.parse_on_server()
        .unwrap_or_else(|error| panic!("valid request was rejected: {error:?}"));
    let timeout = pair.server.read_timeout().expect("timeout is readable");
    assert!(timeout.is_some_and(|remaining| remaining <= IO_TIMEOUT));
}

#[test]
fn size_limits_match_the_documented_caps() {
    assert_eq!(MAX_HEAD_BYTES, 16 * 1024);
    assert_eq!(MAX_BODY_BYTES, 64 * 1024);
}

#[test]
fn reason_phrases_cover_the_emitted_statuses() {
    for (status, phrase) in [
        (200, "OK"),
        (201, "Created"),
        (400, "Bad Request"),
        (408, "Request Timeout"),
        (404, "Not Found"),
        (405, "Method Not Allowed"),
        (409, "Conflict"),
        (411, "Length Required"),
        (413, "Content Too Large"),
        (431, "Request Header Fields Too Large"),
        (500, "Internal Server Error"),
        (501, "Not Implemented"),
        (503, "Service Unavailable"),
    ] {
        assert_eq!(reason(status), phrase, "status {status}");
    }
    assert_eq!(reason(418), "Unknown");
    assert_eq!(reason(0), "Unknown");
}
