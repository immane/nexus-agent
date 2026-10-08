//! Minimal blocking HTTP/1.1 transport for the loopback API.
//!
//! No framework: requests are parsed with a bounded reader and responses
//! are written with explicit framing. Anything outside the narrow supported
//! shape (unknown method, oversized head/body, bad `Content-Length`,
//! chunked encoding) is a static client error, never a best-effort parse.

use std::collections::HashMap;
use std::io::{self, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// Maximum request head (request line plus headers) in bytes.
pub const MAX_HEAD_BYTES: usize = 16_384;
/// Maximum request body in bytes.
pub const MAX_BODY_BYTES: usize = 65_536;
/// Per-read timeout so a stalled peer cannot park a connection thread.
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum wall-clock time to receive one complete request, including slow
/// clients that make progress just before each per-read timeout.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Bound response writes, including SSE frames to clients that stop reading.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Parsed request: method, path (with query), lowercased headers, raw body.
pub struct Request {
    /// Uppercase method as sent (`GET`, `POST`).
    pub method: String,
    /// Raw path including any `?query`.
    pub path: String,
    /// Lowercased header names to raw values.
    pub headers: HashMap<String, String>,
    /// Exact body bytes (`Content-Length` many, possibly empty).
    pub body: Vec<u8>,
}

impl Request {
    /// Returns the header value, or `None` when absent.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// Returns the path without any `?query` suffix.
    #[must_use]
    pub fn route_path(&self) -> &str {
        self.path.split('?').next().unwrap_or(&self.path)
    }

    /// Returns the value of a `?key=value` query parameter, if present.
    #[must_use]
    pub fn query(&self, key: &str) -> Option<&str> {
        let query = self.path.split('?').nth(1)?;
        for pair in query.split('&') {
            let (name, value) = match pair.split_once('=') {
                Some(split) => split,
                None => continue,
            };
            if name == key {
                return Some(value);
            }
        }
        None
    }
}

/// Client error with an HTTP status and a static diagnostic.
pub struct HttpError {
    /// HTTP status code to report.
    pub status: u16,
    /// Static diagnostic; rejected input is never echoed.
    pub message: &'static str,
}

fn bad(status: u16, message: &'static str) -> HttpError {
    HttpError { status, message }
}

/// Reads one request from a connection. `Connection: close` always: after
/// the response the caller closes the stream (except SSE, which owns its
/// stream for the event loop instead).
pub fn read_request(stream: &mut TcpStream) -> Result<Request, HttpError> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|_| bad(500, "connection setup failed"))?;
    let mut reader = BufReader::new(stream);
    let mut head: Vec<u8> = Vec::new();
    loop {
        set_read_deadline(reader.get_ref(), deadline)?;
        let mut byte = [0u8; 1];
        reader.read_exact(&mut byte).map_err(|error| {
            if matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) {
                bad(408, "request timed out")
            } else {
                bad(400, "request could not be read")
            }
        })?;
        head.push(byte[0]);
        if head.len() > MAX_HEAD_BYTES {
            return Err(bad(431, "request head is too large"));
        }
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let head_text =
        std::str::from_utf8(&head).map_err(|_| bad(400, "request head is not valid text"))?;
    let mut lines = head_text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split(' ');
    let (Some(method), Some(path), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(bad(400, "request line is malformed"));
    };
    if parts.next().is_some() {
        return Err(bad(400, "request line is malformed"));
    }
    if !version.starts_with("HTTP/") {
        return Err(bad(400, "request line is malformed"));
    }
    if method != "GET" && method != "POST" && method != "DELETE" {
        return Err(bad(405, "method is not supported"));
    }
    if !path.starts_with('/') {
        return Err(bad(400, "request path is malformed"));
    }
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or(bad(400, "header line is malformed"))?;
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty()
            || name.contains(' ')
            || name.bytes().any(|b| b.is_ascii_control() || b == b':')
        {
            return Err(bad(400, "header name is malformed"));
        }
        if matches!(
            name.as_str(),
            "content-length" | "host" | "origin" | "authorization"
        ) && headers.contains_key(&name)
        {
            return Err(bad(400, "request header is repeated"));
        }
        headers.insert(name, value.trim().to_owned());
    }
    if let Some(encoding) = headers.get("transfer-encoding")
        && !encoding.eq_ignore_ascii_case("identity")
    {
        return Err(bad(501, "chunked transfer encoding is not supported"));
    }
    let content_length: usize = match headers.get("content-length") {
        None => 0,
        Some(raw) => raw
            .parse()
            .map_err(|_| bad(400, "content length is malformed"))?,
    };
    if content_length > MAX_BODY_BYTES {
        return Err(bad(413, "request body is too large"));
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        set_read_deadline(reader.get_ref(), deadline)?;
        reader.read_exact(&mut body).map_err(|error| {
            if matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) {
                bad(408, "request timed out")
            } else {
                bad(400, "request body is truncated")
            }
        })?;
    }
    Ok(Request {
        method: method.to_owned(),
        path: path.to_owned(),
        headers,
        body,
    })
}

fn set_read_deadline(stream: &TcpStream, deadline: Instant) -> Result<(), HttpError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(bad(408, "request timed out"));
    }
    stream
        .set_read_timeout(Some(remaining.min(IO_TIMEOUT)))
        .map_err(|_| bad(500, "connection setup failed"))
}

/// Outgoing response with explicit framing.
pub struct Response {
    /// HTTP status code.
    pub status: u16,
    /// MIME type of the body.
    pub content_type: &'static str,
    /// Complete body bytes.
    pub body: Vec<u8>,
}

impl Response {
    /// Builds a response with the conventional reason phrase.
    #[must_use]
    pub fn new(status: u16, content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type,
            body,
        }
    }
}

/// Canonical reason phrase for the statuses this server emits.
#[must_use]
pub const fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        408 => "Request Timeout",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

/// Writes one framed response and flushes. The caller closes the stream
/// afterwards unless it is handing the stream to the SSE loop.
pub fn write_response(stream: &mut TcpStream, response: &Response) -> io::Result<()> {
    if response.status == 401 {
        write!(
            stream,
            "HTTP/1.1 401 Unauthorized\r\nwww-authenticate: Bearer\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            response.content_type,
            response.body.len(),
        )?;
    } else {
        write!(
            stream,
            "HTTP/1.1 {} {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            response.status,
            reason(response.status),
            response.content_type,
            response.body.len(),
        )?;
    }
    stream.write_all(&response.body)?;
    stream.flush()
}

/// Writes SSE stream headers; the caller then owns the stream and writes
/// `data:` frames until the terminal event.
pub fn write_sse_headers(stream: &mut TcpStream) -> io::Result<()> {
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n",
    )?;
    stream.flush()
}
