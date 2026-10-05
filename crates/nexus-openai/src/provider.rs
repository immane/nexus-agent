//! OpenAI-compatible chat completions adapter over blocking std sockets,
//! with streaming SSE parsing and an `openssl s_client` TLS bridge.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use nexus_config::{CredentialRef, ProviderProfile};
use nexus_core::{
    AgentError, CallCandidate, ErrorCategory, FinishReason, ModelContextItem, ModelRequest,
    ProviderCapabilities, ProviderContext, ProviderEvent, ProviderPort, RetryGuidance,
    TurnFinished, Usage, UsageFinality,
};
use serde_json::{Value, json};

/// Maximum response body in bytes. Anything larger fails instead of
/// allocating unboundedly.
pub const MAX_RESPONSE_BYTES: usize = 1_048_576;
/// Maximum vendor model name in bytes, matching the configuration bound.
pub const MAX_MODEL_LEN: usize = 128;
/// Socket connect timeout. Reads use the remaining run deadline.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Read poll quantum: cancellation and deadlines are re-checked between
/// quanta so a stalled peer cannot park the worker past its bound.
const READ_QUANTUM: Duration = Duration::from_secs(1);

fn provider_error(category: ErrorCategory, message: &'static str) -> ProviderEvent {
    ProviderEvent::Failed(
        AgentError::new(category, message, RetryGuidance::DoNotRetry)
            .expect("static safe provider message builds"),
    )
}

/// OpenAI-compatible chat provider over HTTP, plus `https` through the
/// system `openssl s_client` TLS bridge. Constructed from a configured
/// profile plus the vendor model string; see
/// [`OpenAiProvider::from_profile`].
pub struct OpenAiProvider {
    host: String,
    port: u16,
    base_path: String,
    credential: CredentialRef,
    model: String,
    use_tls: bool,
}

impl OpenAiProvider {
    /// Builds an adapter for one `http` or `https` base URL (for example
    /// `http://localhost:11434/v1` or `https://api.example.com/v1`),
    /// credential reference, and vendor model name.
    ///
    /// Plain `http` goes over direct TCP. `https` is bridged through the
    /// system `openssl` binary (`s_client` with hostname/IP verification),
    /// because no TLS crate is vendored; without `openssl` on `PATH` an
    /// `https` call fails `Protocol` at exchange time. Anything that is not
    /// `http` or `https` is refused here, before any secret resolves.
    pub fn new(
        endpoint: &str,
        credential: CredentialRef,
        model: impl Into<String>,
    ) -> Result<Self, AgentError> {
        let model = model.into();
        if model.is_empty() || model.len() > MAX_MODEL_LEN {
            return Err(provider_error_inner(
                ErrorCategory::InvalidInput,
                "provider model is invalid",
            ));
        }
        let (use_tls, rest) = if let Some(rest) = endpoint.strip_prefix("http://") {
            (false, rest)
        } else if let Some(rest) = endpoint.strip_prefix("https://") {
            (true, rest)
        } else {
            return Err(provider_error_inner(
                ErrorCategory::UnsupportedCapability,
                "provider endpoint scheme is unsupported",
            ));
        };
        let (authority, base_path) = match rest.split_once('/') {
            Some((authority, path)) => (authority, format!("/{path}")),
            None => (rest, String::new()),
        };
        if authority.is_empty() || authority.contains([' ', '\t']) {
            return Err(provider_error_inner(
                ErrorCategory::InvalidInput,
                "provider endpoint is invalid",
            ));
        }
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => (
                host.to_owned(),
                port.parse().map_err(|_| {
                    provider_error_inner(
                        ErrorCategory::InvalidInput,
                        "provider endpoint is invalid",
                    )
                })?,
            ),
            None => (authority.to_owned(), if use_tls { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Err(provider_error_inner(
                ErrorCategory::InvalidInput,
                "provider endpoint is invalid",
            ));
        }
        Ok(Self {
            host,
            port,
            base_path: base_path.trim_end_matches('/').to_owned(),
            credential,
            model,
            use_tls,
        })
    }

    /// Builds an adapter from a configured provider profile plus the
    /// vendor model string to send (a selected [`ModelEntry`] name or the
    /// profile default). Both adapter kinds speak the same HTTP dialect;
    /// the distinction governs credential scoping, not bytes.
    ///
    /// [`ModelEntry`]: nexus_config::ModelEntry
    pub fn from_profile(profile: &ProviderProfile, model: &str) -> Result<Self, AgentError> {
        let endpoint = profile.endpoint.as_deref().ok_or_else(|| {
            provider_error_inner(ErrorCategory::InvalidInput, "provider endpoint is missing")
        })?;
        Self::new(endpoint, profile.credential.clone(), model)
    }

    /// Returns the vendor model string sent on the wire.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Returns true when this adapter bridges through TLS (`https`).
    #[must_use]
    pub fn uses_tls(&self) -> bool {
        self.use_tls
    }

    /// Encodes one turn as a Chat Completions body, always streaming: the
    /// SSE wire is parsed incrementally for cancellation responsiveness and
    /// aggregated into one validated batch (see [`ProviderPort::stream`]).
    fn request_body(&self, request: &ModelRequest) -> Value {
        let messages: Vec<Value> = request.conversation().iter().map(message_json).collect();
        let tools: Vec<Value> = request
            .tool_definitions()
            .iter()
            .map(|spec| {
                json!({
                    "type": "function",
                    "function": {
                        "name": spec.id().name(),
                        "description": spec.description(),
                        "parameters": serde_json::from_str::<Value>(spec.input_schema_json())
                            .unwrap_or(Value::Bool(true)),
                    }
                })
            })
            .collect();
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
            body["tool_choice"] = Value::String("auto".to_owned());
        }
        body
    }

    /// Performs one blocking exchange, honouring cancellation and the run
    /// deadline between read quanta. Plain `http` goes over direct TCP;
    /// `https` is bridged through `openssl s_client` (verified TLS); the
    /// credential resolves before either path opens anything.
    fn exchange(
        &self,
        body: &[u8],
        context: &ProviderContext,
    ) -> Result<(u16, String, Vec<u8>), ProviderEvent> {
        if context.is_cancelled() {
            return Err(provider_error(
                ErrorCategory::Cancelled,
                "provider call was cancelled",
            ));
        }
        let deadline = context.deadline();
        if deadline.is_some_and(|at| Instant::now() >= at) {
            return Err(provider_error(
                ErrorCategory::Timeout,
                "provider deadline elapsed before send",
            ));
        }
        let token = nexus_config::resolve_credential(&self.credential).map_err(|_| {
            provider_error(
                ErrorCategory::Authentication,
                "provider credential is unavailable",
            )
        })?;
        let path = format!("{}/chat/completions", self.base_path);
        let address = format!("{}:{}", self.host, self.port);
        let head = format!(
            "POST {path} HTTP/1.1\r\nhost: {}\r\ncontent-type: application/json\r\naccept: text/event-stream, application/json\r\ncontent-length: {}\r\nauthorization: Bearer {token}\r\nconnection: close\r\n\r\n",
            address,
            body.len(),
        );
        let raw = if self.use_tls {
            self.exchange_tls(head.as_bytes(), body, deadline, context)
        } else {
            self.exchange_plain(head.as_bytes(), body, deadline, context)
        }?;
        split_response(&raw)
    }

    /// Plain-HTTP exchange over direct TCP with a bounded connect timeout.
    fn exchange_plain(
        &self,
        head: &[u8],
        body: &[u8],
        deadline: Option<Instant>,
        context: &ProviderContext,
    ) -> Result<Vec<u8>, ProviderEvent> {
        let address = format!("{}:{}", self.host, self.port);
        let mut stream = address
            .to_socket_addrs()
            .ok()
            .and_then(|mut addresses| {
                addresses
                    .find_map(|address| TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).ok())
            })
            .ok_or_else(|| provider_error(ErrorCategory::Protocol, "provider is unreachable"))?;
        if context.is_cancelled() {
            return Err(provider_error(
                ErrorCategory::Cancelled,
                "provider call was cancelled",
            ));
        }
        stream
            .write_all(head)
            .and_then(|()| stream.write_all(body))
            .map_err(|_| provider_error(ErrorCategory::Protocol, "provider request failed"))?;
        if context.is_cancelled() {
            return Err(provider_error(
                ErrorCategory::Cancelled,
                "provider call was cancelled",
            ));
        }
        read_bounded(&mut stream, deadline, context).map_err(map_read_failure)
    }

    /// TLS exchange bridged through the system `openssl s_client` helper.
    ///
    /// The bearer credential travels inside the encrypted pipe (argv carries
    /// only the host and port, never the secret). Host verification is
    /// enforced (`-verify_return_error` plus hostname/IP policy); a failed
    /// handshake or a missing helper is a `Protocol` error. Reads are pumped
    /// through a helper thread so cancellation and deadlines stay responsive
    /// even though child pipes expose no read timeout.
    fn exchange_tls(
        &self,
        head: &[u8],
        body: &[u8],
        deadline: Option<Instant>,
        context: &ProviderContext,
    ) -> Result<Vec<u8>, ProviderEvent> {
        let target = format!("{}:{}", self.host, self.port);
        let mut command = std::process::Command::new("openssl");
        command
            .arg("s_client")
            .arg("-connect")
            .arg(&target)
            .arg("-quiet")
            .arg("-verify_return_error");
        if is_ip_literal(&self.host) {
            command.arg("-verify_ip").arg(&self.host);
        } else {
            command
                .arg("-verify_hostname")
                .arg(&self.host)
                .arg("-servername")
                .arg(&self.host);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child: Child = command
            .spawn()
            .map_err(|_| provider_error(ErrorCategory::Protocol, "TLS helper is unavailable"))?;
        let request: Vec<u8> = head.iter().chain(body.iter()).copied().collect();
        if let Some(mut stdin) = child.stdin.take() {
            // `-quiet` implies `-ign_eof`: dropping stdin after the request
            // does not close the connection from our side.
            if stdin.write_all(&request).is_err() {
                kill_child(&mut child);
                return Err(provider_error(
                    ErrorCategory::Protocol,
                    "provider request failed",
                ));
            }
        }
        drop(child.stdin.take());
        let stdout = child.stdout.take();
        let Some(stdout) = stdout else {
            kill_child(&mut child);
            return Err(provider_error(
                ErrorCategory::Protocol,
                "provider response failed",
            ));
        };
        let (sender, receiver) = mpsc::channel::<Result<Vec<u8>, std::io::Error>>();
        std::thread::spawn(move || {
            let mut reader = stdout;
            loop {
                let mut chunk = [0u8; 8192];
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        if sender.send(Ok(chunk[..read].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        break;
                    }
                }
            }
        });
        let started = Instant::now();
        let mut out = Vec::new();
        let mut first_byte = true;
        loop {
            if context.is_cancelled() {
                kill_child(&mut child);
                return Err(provider_error(
                    ErrorCategory::Cancelled,
                    "provider call was cancelled",
                ));
            }
            if let Some(at) = deadline
                && Instant::now() >= at
            {
                kill_child(&mut child);
                return Err(provider_error(
                    ErrorCategory::Timeout,
                    "provider response timed out",
                ));
            }
            let quantum = match deadline {
                Some(at) => READ_QUANTUM.min(at.saturating_duration_since(Instant::now())),
                None => READ_QUANTUM,
            };
            match receiver.recv_timeout(quantum.max(Duration::from_millis(1))) {
                Ok(Ok(chunk)) => {
                    first_byte = false;
                    out.extend_from_slice(&chunk);
                    if out.len() > MAX_RESPONSE_BYTES {
                        kill_child(&mut child);
                        return Err(provider_error(
                            ErrorCategory::ResourceLimit,
                            "provider response is too large",
                        ));
                    }
                }
                Ok(Err(_)) => {
                    kill_child(&mut child);
                    return Err(provider_error(
                        ErrorCategory::Protocol,
                        "provider response failed",
                    ));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if first_byte && started.elapsed() >= CONNECT_TIMEOUT {
                        kill_child(&mut child);
                        return Err(provider_error(
                            ErrorCategory::Protocol,
                            "provider is unreachable",
                        ));
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = child.wait();
        if out.is_empty() {
            return Err(provider_error(
                ErrorCategory::Protocol,
                "provider response failed",
            ));
        }
        Ok(out)
    }
}

/// Terminates a TLS helper without blocking the worker.
fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// True for IP literals, which need `-verify_ip` instead of
/// `-verify_hostname` on the `s_client` command line.
fn is_ip_literal(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok()
}

/// Splits one framed HTTP response into status, head text, and decoded body
/// (de-chunked when the head declares `Transfer-Encoding: chunked`).
fn split_response(raw: &[u8]) -> Result<(u16, String, Vec<u8>), ProviderEvent> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| provider_error(ErrorCategory::Protocol, "provider response failed"))?;
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| provider_error(ErrorCategory::Protocol, "provider response failed"))?;
    let body = decode_body(head, body.as_bytes());
    Ok((status, head.to_owned(), body))
}

/// De-chunks a body when the response head declares chunked framing;
/// otherwise returns the body unchanged.
fn decode_body(head: &str, body: &[u8]) -> Vec<u8> {
    if !head.to_ascii_lowercase().contains("chunked") {
        return body.to_vec();
    }
    dechunk(body).unwrap_or_else(|| body.to_vec())
}

/// Byte-level `Transfer-Encoding: chunked` decoder. Returns `None` on any
/// framing violation.
fn dechunk(body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(body.len());
    let mut cursor = 0;
    loop {
        let line_end = find_crlf(body, cursor)?;
        let size_text = std::str::from_utf8(&body[cursor..line_end]).ok()?;
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16).ok()?;
        cursor = line_end + 2;
        if size == 0 {
            return Some(out);
        }
        if out.len().saturating_add(size) > MAX_RESPONSE_BYTES {
            return None;
        }
        let end = cursor.checked_add(size)?;
        if end > body.len() {
            return None;
        }
        out.extend_from_slice(&body[cursor..end]);
        cursor = end.checked_add(2)?;
        if body.get(cursor - 2..cursor) != Some(b"\r\n") {
            return None;
        }
    }
}

/// Finds the next `\r\n` at or after `from`.
fn find_crlf(body: &[u8], from: usize) -> Option<usize> {
    (from..body.len().saturating_sub(1))
        .find(|&index| body[index] == b'\r' && body[index + 1] == b'\n')
}

fn map_read_failure(failure: ReadFailure) -> ProviderEvent {
    match failure {
        ReadFailure::Cancelled => {
            provider_error(ErrorCategory::Cancelled, "provider call was cancelled")
        }
        ReadFailure::TimedOut => {
            provider_error(ErrorCategory::Timeout, "provider response timed out")
        }
        ReadFailure::TooLarge => provider_error(
            ErrorCategory::ResourceLimit,
            "provider response is too large",
        ),
        ReadFailure::Broken => provider_error(ErrorCategory::Protocol, "provider response failed"),
    }
}

/// Bounded read outcome for the exchange loop.
enum ReadFailure {
    Cancelled,
    TimedOut,
    TooLarge,
    Broken,
}

/// Reads one framed HTTP response, honouring cancellation and an
/// optional absolute deadline. The socket quantum keeps cancellation
/// responsive; the total is capped so a chatty peer cannot grow memory
/// without bound.
fn read_bounded(
    stream: &mut TcpStream,
    deadline: Option<Instant>,
    context: &ProviderContext,
) -> Result<Vec<u8>, ReadFailure> {
    let mut out = Vec::new();
    loop {
        if context.is_cancelled() {
            return Err(ReadFailure::Cancelled);
        }
        if let Some(at) = deadline
            && Instant::now() >= at
        {
            return Err(ReadFailure::TimedOut);
        }
        let quantum = match deadline {
            Some(at) => READ_QUANTUM.min(at.saturating_duration_since(Instant::now())),
            None => READ_QUANTUM,
        };
        stream
            .set_read_timeout(Some(quantum.max(Duration::from_millis(1))))
            .map_err(|_| ReadFailure::Broken)?;
        let mut chunk = [0u8; 8192];
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(out),
            Ok(read) => {
                out.extend_from_slice(&chunk[..read]);
                if out.len() > MAX_RESPONSE_BYTES {
                    return Err(ReadFailure::TooLarge);
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::TimedOut
                    || error.kind() == std::io::ErrorKind::WouldBlock =>
            {
                continue;
            }
            Err(_) => return Err(ReadFailure::Broken),
        }
    }
}

fn provider_error_inner(category: ErrorCategory, message: &'static str) -> AgentError {
    AgentError::new(category, message, RetryGuidance::DoNotRetry)
        .expect("static safe provider message builds")
}

/// Encodes one conversation item as a Chat Completions message. Provider
/// round-trip refs ride the tool-call ids; text item keys stay local.
fn message_json(item: &ModelContextItem) -> Value {
    match item {
        ModelContextItem::UserText(text) => {
            json!({ "role": "user", "content": text.as_str() })
        }
        ModelContextItem::AssistantText { text, .. } => {
            json!({ "role": "assistant", "content": text.as_str() })
        }
        ModelContextItem::AssistantCall {
            provider_ref, call, ..
        } => json!({
            "role": "assistant",
            "content": Value::Null,
            "tool_calls": [{
                "id": provider_ref.as_str(),
                "type": "function",
                "function": {
                    "name": call.tool().name(),
                    "arguments": call.args().as_str(),
                },
            }],
        }),
        ModelContextItem::ToolResult {
            provider_ref,
            outcome,
            ..
        } => json!({
            "role": "tool",
            "tool_call_id": provider_ref.as_str(),
            "content": outcome.content(),
        }),
    }
}

/// Maps one HTTP exchange to provider events: at most one text delta at
/// `item-0`, calls numbered from `item-1`, then exactly one terminal.
///
/// Servers that honour `stream: true` answer `text/event-stream` (possibly
/// chunked): `data:` JSON chunks plus `data: [DONE]`. A plain single-JSON
/// reply is still accepted for servers that ignore the flag.
fn events_for(status: u16, head: &str, body: &[u8]) -> Vec<ProviderEvent> {
    if !(200..300).contains(&status) {
        return vec![provider_error(
            match status {
                401 | 403 => ErrorCategory::Authentication,
                429 => ErrorCategory::RateLimited,
                _ => ErrorCategory::Protocol,
            },
            "provider request failed",
        )];
    }
    if is_sse(head, body) {
        return events_for_sse(body);
    }
    events_for_json(body)
}

/// True when the reply looks like an SSE stream: either the head declares
/// `text/event-stream` or the trimmed body opens with a `data:` line.
fn is_sse(head: &str, body: &[u8]) -> bool {
    if head.to_ascii_lowercase().contains("text/event-stream") {
        return true;
    }
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n']);
    trimmed.starts_with("data:")
}

/// Aggregates one SSE stream into the same batch shape as [`events_for_json`]:
/// streamed `delta.content` fragments concatenate to one `item-0` delta and
/// per-index `delta.tool_calls` fragments assemble before any candidate is
/// built. A strict `[DONE]` terminator is required; its absence is a
/// `Protocol` truncation, never an implied stop.
fn events_for_sse(body: &[u8]) -> Vec<ProviderEvent> {
    let text = String::from_utf8_lossy(body);
    let mut text_out = String::new();
    let mut fragments: std::collections::BTreeMap<u64, SseCall> = std::collections::BTreeMap::new();
    let mut reason = String::new();
    let mut input: Option<u64> = None;
    let mut output: Option<u64> = None;
    let mut done = false;
    for line in text.lines() {
        let line = line.trim();
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload == "[DONE]" {
            done = true;
            break;
        }
        if payload.is_empty() {
            continue;
        }
        let chunk: Value = match serde_json::from_str(payload) {
            Ok(chunk) => chunk,
            Err(_) => {
                return vec![provider_error(
                    ErrorCategory::Protocol,
                    "provider reply is not valid JSON",
                )];
            }
        };
        if input.is_none() && output.is_none() {
            (input, output) = usage_of(&chunk);
        } else {
            let (next_in, next_out) = usage_of(&chunk);
            if next_in.is_some() {
                input = next_in;
            }
            if next_out.is_some() {
                output = next_out;
            }
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            continue;
        };
        if let Some(next) = choice.get("finish_reason").and_then(Value::as_str)
            && !next.is_empty()
        {
            reason = next.to_owned();
        }
        let Some(delta) = choice.get("delta") else {
            continue;
        };
        if let Some(fragment) = delta.get("content").and_then(Value::as_str) {
            text_out.push_str(fragment);
        }
        let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) else {
            continue;
        };
        for (position, call) in calls.iter().enumerate() {
            let index = call
                .get("index")
                .and_then(Value::as_u64)
                .unwrap_or(position as u64);
            let entry = fragments.entry(index).or_default();
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                entry.id.push_str(id);
            }
            if let Some(function) = call.get("function") {
                if let Some(name) = function.get("name").and_then(Value::as_str) {
                    entry.name.push_str(name);
                }
                match function.get("arguments") {
                    None => {}
                    Some(Value::String(fragment)) => entry.arguments.push_str(fragment),
                    Some(_) => {
                        return vec![provider_error(
                            ErrorCategory::Protocol,
                            "provider reply is not valid JSON",
                        )];
                    }
                }
            }
        }
    }
    if !done {
        return vec![provider_error(
            ErrorCategory::Protocol,
            "provider stream ended without termination",
        )];
    }
    if text_out.len()
        + fragments
            .values()
            .map(|call| call.arguments.len())
            .sum::<usize>()
        > MAX_RESPONSE_BYTES
    {
        return vec![provider_error(
            ErrorCategory::ResourceLimit,
            "provider response is too large",
        )];
    }
    let mut events = Vec::new();
    if !text_out.is_empty() {
        events.push(ProviderEvent::TextDelta {
            item_key: "item-0".to_owned(),
            text: text_out,
        });
    }
    let mut ready = Vec::new();
    for (position, (_, call)) in fragments.iter().enumerate() {
        let item_key = format!("item-{}", position + 1);
        match CallCandidate::new(
            item_key,
            call.id.clone(),
            call.name.clone(),
            call.arguments.clone(),
        ) {
            Ok(candidate) => ready.push(ProviderEvent::ToolCallReady(candidate)),
            Err(_) => {
                return vec![provider_error(
                    ErrorCategory::Protocol,
                    "provider call identity is invalid",
                )];
            }
        }
    }
    events.extend(ready.iter().cloned());
    match terminal_for(&reason, !ready.is_empty(), input, output) {
        Ok(terminal) => {
            events.push(terminal);
            events
        }
        Err(failure) => failure,
    }
}

/// One streamed tool-call assembly slot, keyed by chunk index.
#[derive(Default)]
struct SseCall {
    id: String,
    name: String,
    arguments: String,
}

/// Reads `usage.prompt_tokens` / `usage.completion_tokens` from one chunk.
fn usage_of(chunk: &Value) -> (Option<u64>, Option<u64>) {
    chunk
        .get("usage")
        .map(|usage| {
            (
                usage.get("prompt_tokens").and_then(Value::as_u64),
                usage.get("completion_tokens").and_then(Value::as_u64),
            )
        })
        .unwrap_or((None, None))
}

/// Builds the terminal event shared by the JSON and SSE shapes. Returns the
/// failure event (as `Err`-style early return value) when a claimed
/// `tool_calls` finish carries no parsed candidate.
fn terminal_for(
    reason: &str,
    has_calls: bool,
    input: Option<u64>,
    output: Option<u64>,
) -> Result<ProviderEvent, Vec<ProviderEvent>> {
    let claimed_tool_calls = reason == "tool_calls";
    let finish = match reason {
        "stop" => FinishReason::Stop,
        "tool_calls" if has_calls => FinishReason::ToolCalls,
        "length" => FinishReason::OutputLimit,
        "content_filter" => FinishReason::Refusal,
        _ => FinishReason::Incomplete,
    };
    if claimed_tool_calls && !has_calls {
        return Err(vec![provider_error(
            ErrorCategory::Protocol,
            "provider claimed tool calls without items",
        )]);
    }
    Ok(ProviderEvent::TurnFinished(TurnFinished::new(
        finish,
        Usage::new(input, output, UsageFinality::Final),
        None,
    )))
}

fn events_for_json(body: &[u8]) -> Vec<ProviderEvent> {
    let reply: Value = match serde_json::from_slice(body) {
        Ok(reply) => reply,
        Err(_) => {
            return vec![provider_error(
                ErrorCategory::Protocol,
                "provider reply is not valid JSON",
            )];
        }
    };
    let message = reply
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"));
    let Some(message) = message else {
        return vec![provider_error(
            ErrorCategory::Protocol,
            "provider reply has no message",
        )];
    };
    let mut events = Vec::new();
    if let Some(text) = message.get("content").and_then(Value::as_str)
        && !text.is_empty()
    {
        events.push(ProviderEvent::TextDelta {
            item_key: "item-0".to_owned(),
            text: text.to_owned(),
        });
    }
    let calls: Vec<(String, String, String)> = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .filter_map(|call| {
                    let id = call.get("id")?.as_str()?;
                    let function = call.get("function")?;
                    let name = function.get("name")?.as_str()?;
                    let arguments = function.get("arguments")?.as_str()?;
                    Some((id.to_owned(), name.to_owned(), arguments.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut ready = Vec::new();
    for (index, (id, name, arguments)) in calls.iter().enumerate() {
        let item_key = format!("item-{}", index + 1);
        match CallCandidate::new(item_key, id.clone(), name.clone(), arguments.clone()) {
            Ok(candidate) => ready.push(ProviderEvent::ToolCallReady(candidate)),
            Err(_) => {
                return vec![provider_error(
                    ErrorCategory::Protocol,
                    "provider call identity is invalid",
                )];
            }
        }
    }
    events.extend(ready.iter().cloned());
    let reason = reply
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let (input, output) = reply
        .get("usage")
        .map(|usage| {
            (
                usage.get("prompt_tokens").and_then(Value::as_u64),
                usage.get("completion_tokens").and_then(Value::as_u64),
            )
        })
        .unwrap_or((None, None));
    match terminal_for(reason, !ready.is_empty(), input, output) {
        Ok(terminal) => {
            events.push(terminal);
            events
        }
        Err(failure) => failure,
    }
}

impl ProviderPort for OpenAiProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            text: true,
            streaming: true,
            tool_calls: true,
            structured_output: false,
            usage_reporting: true,
            max_context_items: None,
            max_output_bytes: None,
        }
    }

    fn stream(&self, request: &ModelRequest, context: &ProviderContext) -> Vec<ProviderEvent> {
        if context.is_cancelled() {
            return vec![provider_error(
                ErrorCategory::Cancelled,
                "provider call was cancelled",
            )];
        }
        let body = serde_json::to_vec(&self.request_body(request)).unwrap_or_default();
        if body.is_empty() {
            return vec![provider_error(
                ErrorCategory::Internal,
                "provider request could not be encoded",
            )];
        }
        let (status, head, payload) = match self.exchange(&body, context) {
            Ok(exchange) => exchange,
            Err(failure) => return vec![failure],
        };
        events_for(status, &head, &payload)
    }
}
