//! OpenAI-compatible chat completions adapter over blocking std sockets,
//! with streaming SSE parsing and an `openssl s_client` TLS bridge.

use std::collections::BTreeMap;
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
/// Maximum response head (status line plus headers) in bytes. Anything
/// larger fails instead of accumulating unboundedly while the head is still
/// incomplete.
const MAX_HEAD_BYTES: usize = 16_384;
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
        if authority.is_empty()
            || authority
                .chars()
                .any(|c| c.is_ascii_control() || c == ' ' || c == '@')
        {
            return Err(provider_error_inner(
                ErrorCategory::InvalidInput,
                "provider endpoint is invalid",
            ));
        }
        if base_path.chars().any(|c| c.is_ascii_control() || c == ' ') {
            return Err(provider_error_inner(
                ErrorCategory::InvalidInput,
                "provider endpoint is invalid",
            ));
        }
        let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
            let Some(end) = bracketed.find(']') else {
                return Err(provider_error_inner(
                    ErrorCategory::InvalidInput,
                    "provider endpoint is invalid",
                ));
            };
            let inner = &bracketed[..end];
            let remainder = &bracketed[end + 1..];
            if inner.is_empty()
                || inner
                    .chars()
                    .any(|c| c.is_ascii_control() || c == ' ' || c == '@' || c == '[' || c == ']')
            {
                return Err(provider_error_inner(
                    ErrorCategory::InvalidInput,
                    "provider endpoint is invalid",
                ));
            }
            let port = if remainder.is_empty() {
                if use_tls { 443 } else { 80 }
            } else if let Some(port_text) = remainder.strip_prefix(':') {
                port_text.parse().map_err(|_| {
                    provider_error_inner(
                        ErrorCategory::InvalidInput,
                        "provider endpoint is invalid",
                    )
                })?
            } else {
                return Err(provider_error_inner(
                    ErrorCategory::InvalidInput,
                    "provider endpoint is invalid",
                ));
            };
            (inner.to_owned(), port)
        } else {
            if authority.chars().any(|c| c == '[' || c == ']') {
                return Err(provider_error_inner(
                    ErrorCategory::InvalidInput,
                    "provider endpoint is invalid",
                ));
            }
            match authority.split_once(':') {
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
            }
        };
        if host.is_empty()
            || host
                .chars()
                .any(|c| c.is_ascii_control() || c == ' ' || c == '@')
        {
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
    fn request_body(&self, request: &ModelRequest) -> Result<Value, ProviderEvent> {
        let messages: Vec<Value> = request.conversation().iter().map(message_json).collect();
        let mut tools: Vec<Value> = Vec::new();
        for spec in request.tool_definitions() {
            let schema: Value = serde_json::from_str(spec.input_schema_json()).map_err(|_| {
                provider_error(
                    ErrorCategory::InvalidInput,
                    "provider tool schema is invalid",
                )
            })?;
            tools.push(json!({
                "type": "function",
                "function": {
                    "name": spec.id().name(),
                    "description": spec.description(),
                    "parameters": schema,
                }
            }));
        }
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
        Ok(body)
    }

    /// Performs one blocking exchange, honouring cancellation and the run
    /// deadline between read quanta. Plain `http` goes over direct TCP;
    /// `https` is bridged through `openssl s_client` (verified TLS); the
    /// credential resolves before either path opens anything.
    ///
    /// SSE replies stream provisional text and usage through `sink` as chunks
    /// arrive; candidates and the terminal event arrive only in the returned
    /// batch. The returned batch keeps the stable aggregated shape no matter
    /// how the wire was fragmented.
    fn stream_with_sink_impl(
        &self,
        body: &[u8],
        context: &ProviderContext,
        sink: &(dyn Fn(ProviderEvent) + Send + Sync),
    ) -> Vec<ProviderEvent> {
        if context.is_cancelled() {
            return vec![provider_error(
                ErrorCategory::Cancelled,
                "provider call was cancelled",
            )];
        }
        let deadline = context.deadline();
        if deadline.is_some_and(|at| Instant::now() >= at) {
            return vec![provider_error(
                ErrorCategory::Timeout,
                "provider deadline elapsed before send",
            )];
        }
        let token = match nexus_config::resolve_credential(&self.credential) {
            Ok(token) => token,
            Err(_) => {
                return vec![provider_error(
                    ErrorCategory::Authentication,
                    "provider credential is unavailable",
                )];
            }
        };
        let path = format!("{}/chat/completions", self.base_path);
        let address = join_host_port(&self.host, self.port);
        let head = format!(
            "POST {path} HTTP/1.1\r\nhost: {}\r\ncontent-type: application/json\r\naccept: text/event-stream, application/json\r\ncontent-length: {}\r\nauthorization: Bearer {token}\r\nconnection: close\r\n\r\n",
            address,
            body.len(),
        );
        if self.use_tls {
            match spawn_tls(&self.host, self.port, head.as_bytes(), body) {
                Ok(tls) => {
                    let mut source = tls;
                    stream_response(&mut source, deadline, context, sink)
                }
                Err(failure) => vec![failure],
            }
        } else {
            match connect_plain(&self.host, self.port) {
                Ok(mut stream) => {
                    if stream.write_all(head.as_bytes()).is_err() || stream.write_all(body).is_err()
                    {
                        return vec![provider_error(
                            ErrorCategory::Protocol,
                            "provider request failed",
                        )];
                    }
                    if context.is_cancelled() {
                        return vec![provider_error(
                            ErrorCategory::Cancelled,
                            "provider call was cancelled",
                        )];
                    }
                    let mut source = TcpSource {
                        stream: &mut stream,
                    };
                    stream_response(&mut source, deadline, context, sink)
                }
                Err(failure) => vec![failure],
            }
        }
    }
}

/// Opens a plain-HTTP connection with a bounded connect timeout.
fn connect_plain(host: &str, port: u16) -> Result<TcpStream, ProviderEvent> {
    let address = join_host_port(host, port);
    address
        .to_socket_addrs()
        .ok()
        .and_then(|mut addresses| {
            addresses.find_map(|address| TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).ok())
        })
        .ok_or_else(|| provider_error(ErrorCategory::Protocol, "provider is unreachable"))
}

/// Spawns the verified-TLS helper, writes one request into its encrypted
/// pipe, and returns the stdout pump source. The bearer credential travels
/// inside the pipe (argv carries only host and port, never the secret).
/// Dropping the source terminates the helper.
fn spawn_tls(host: &str, port: u16, head: &[u8], body: &[u8]) -> Result<TlsSource, ProviderEvent> {
    let target = join_host_port(host, port);
    let mut command = std::process::Command::new("openssl");
    command
        .arg("s_client")
        .arg("-connect")
        .arg(&target)
        .arg("-quiet")
        .arg("-verify_return_error");
    if is_ip_literal(host) {
        command.arg("-verify_ip").arg(host);
    } else {
        command
            .arg("-verify_hostname")
            .arg(host)
            .arg("-servername")
            .arg(host);
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
    Ok(TlsSource {
        receiver,
        child,
        started: Instant::now(),
        first_byte: true,
    })
}

/// One pollable response-byte source behind either transport.
trait ChunkSource {
    /// Returns the next response bytes, an empty vector when the quantum
    /// elapsed without bytes, or `None` on clean EOF. Cancellation and
    /// deadlines surface as terminal failures, never silent stalls.
    fn next_chunk(
        &mut self,
        quantum: Duration,
        deadline: Option<Instant>,
        context: &ProviderContext,
    ) -> Result<Option<Vec<u8>>, ProviderEvent>;
}

/// Direct-TCP source: socket read timeouts drive the quantum.
struct TcpSource<'a> {
    stream: &'a mut TcpStream,
}

impl ChunkSource for TcpSource<'_> {
    fn next_chunk(
        &mut self,
        quantum: Duration,
        deadline: Option<Instant>,
        context: &ProviderContext,
    ) -> Result<Option<Vec<u8>>, ProviderEvent> {
        check_live(context, deadline)?;
        self.stream
            .set_read_timeout(Some(quantum.max(Duration::from_millis(1))))
            .map_err(|_| provider_error(ErrorCategory::Protocol, "provider response failed"))?;
        let mut chunk = [0u8; 8192];
        match self.stream.read(&mut chunk) {
            Ok(0) => Ok(None),
            Ok(read) => Ok(Some(chunk[..read].to_vec())),
            Err(error)
                if error.kind() == std::io::ErrorKind::TimedOut
                    || error.kind() == std::io::ErrorKind::WouldBlock =>
            {
                check_live(context, deadline)?;
                Ok(Some(Vec::new()))
            }
            Err(_) => Err(provider_error(
                ErrorCategory::Protocol,
                "provider response failed",
            )),
        }
    }
}

/// TLS-helper source: a pump thread forwards child-stdout bytes so the
/// driver keeps its quantum discipline even though child pipes expose no
/// read timeout. Dropping the source terminates the helper.
struct TlsSource {
    receiver: mpsc::Receiver<Result<Vec<u8>, std::io::Error>>,
    child: Child,
    started: Instant,
    first_byte: bool,
}

impl Drop for TlsSource {
    fn drop(&mut self) {
        kill_child(&mut self.child);
    }
}

impl ChunkSource for TlsSource {
    fn next_chunk(
        &mut self,
        quantum: Duration,
        deadline: Option<Instant>,
        context: &ProviderContext,
    ) -> Result<Option<Vec<u8>>, ProviderEvent> {
        check_live(context, deadline)?;
        match self
            .receiver
            .recv_timeout(quantum.max(Duration::from_millis(1)))
        {
            Ok(Ok(chunk)) => {
                self.first_byte = false;
                Ok(Some(chunk))
            }
            Ok(Err(_)) => Err(provider_error(
                ErrorCategory::Protocol,
                "provider response failed",
            )),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                check_live(context, deadline)?;
                if self.first_byte && self.started.elapsed() >= CONNECT_TIMEOUT {
                    return Err(provider_error(
                        ErrorCategory::Protocol,
                        "provider is unreachable",
                    ));
                }
                Ok(Some(Vec::new()))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Ok(None),
        }
    }
}

/// Cancellation-first liveness check shared by both sources.
fn check_live(context: &ProviderContext, deadline: Option<Instant>) -> Result<(), ProviderEvent> {
    if context.is_cancelled() {
        return Err(provider_error(
            ErrorCategory::Cancelled,
            "provider call was cancelled",
        ));
    }
    if deadline.is_some_and(|at| Instant::now() >= at) {
        return Err(provider_error(
            ErrorCategory::Timeout,
            "provider response timed out",
        ));
    }
    Ok(())
}

/// One blocking wait bound: the poll quantum, truncated by the deadline.
fn quantum_for(deadline: Option<Instant>) -> Duration {
    match deadline {
        Some(at) => READ_QUANTUM.min(at.saturating_duration_since(Instant::now())),
        None => READ_QUANTUM,
    }
}

/// Drives one response: reads the head, maps non-success statuses without
/// touching the body, then either feeds SSE chunks to the live parser (with
/// provisional sink delivery) or accumulates a plain body to EOF under the
/// byte cap.
fn stream_response(
    source: &mut dyn ChunkSource,
    deadline: Option<Instant>,
    context: &ProviderContext,
    sink: &(dyn Fn(ProviderEvent) + Send + Sync),
) -> Vec<ProviderEvent> {
    let mut head_buf = Vec::new();
    let body_start = loop {
        if head_buf.len() > MAX_HEAD_BYTES {
            return vec![provider_error(
                ErrorCategory::Protocol,
                "provider response failed",
            )];
        }
        match source.next_chunk(quantum_for(deadline), deadline, context) {
            Err(failure) => return vec![failure],
            Ok(None) => {
                return vec![provider_error(
                    ErrorCategory::Protocol,
                    "provider response failed",
                )];
            }
            Ok(Some(bytes)) => {
                head_buf.extend_from_slice(&bytes);
                if let Some(end) = find_head_end(&head_buf) {
                    break head_buf.split_off(end);
                }
            }
        }
    };
    let head = String::from_utf8_lossy(&head_buf).into_owned();
    let status: u16 = match head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
    {
        Some(status) => status,
        None => {
            return vec![provider_error(
                ErrorCategory::Protocol,
                "provider response failed",
            )];
        }
    };
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
    if !head.to_ascii_lowercase().contains("text/event-stream") {
        let mut body = body_start;
        loop {
            match source.next_chunk(quantum_for(deadline), deadline, context) {
                Err(failure) => return vec![failure],
                Ok(None) => break,
                Ok(Some(bytes)) => {
                    body.extend_from_slice(&bytes);
                    if body.len() > MAX_RESPONSE_BYTES {
                        return vec![provider_error(
                            ErrorCategory::ResourceLimit,
                            "provider response is too large",
                        )];
                    }
                }
            }
        }
        return events_for(status, &head, &decode_body(&head, &body));
    }
    let chunked = head.to_ascii_lowercase().contains("chunked");
    let mut live = SseLive::new(chunked);
    if !body_start.is_empty() {
        match live.feed(&body_start, sink) {
            Ok(Some(batch)) => return batch,
            Ok(None) => {}
            Err(failure) => return failure,
        }
    }
    loop {
        match source.next_chunk(quantum_for(deadline), deadline, context) {
            Err(failure) => return vec![failure],
            Ok(None) => {
                return vec![provider_error(
                    ErrorCategory::Protocol,
                    "provider stream ended without termination",
                )];
            }
            Ok(Some(bytes)) => match live.feed(&bytes, sink) {
                Ok(Some(batch)) => return batch,
                Ok(None) => {}
                Err(failure) => return failure,
            },
        }
    }
}

/// Finds the end of an HTTP head (`\r\n\r\n`), as an exclusive byte offset.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

/// Aggregated SSE turn state shared by the batch and live parsers.
#[derive(Default)]
struct SseAccum {
    text: String,
    fragments: BTreeMap<u64, SseCall>,
    reason: String,
    input: Option<u64>,
    output: Option<u64>,
}

/// What one `data:` JSON chunk contributed, for provisional sink delivery.
struct SseApplied {
    content: String,
    usage_present: bool,
    usage: (Option<u64>, Option<u64>),
}

/// Folds one `data:` payload into the accumulator. Chunks without a first
/// choice (usage-only trailers) contribute only usage. Tool-call fragments
/// accumulate per index across chunks: an id or name that arrives in an
/// early chunk joins arguments that arrive later, so split servers parse;
/// a structurally unusable candidate still fails at finalization, never
/// dispatches partially.
fn apply_sse_data(acc: &mut SseAccum, payload: &str) -> Result<SseApplied, Vec<ProviderEvent>> {
    let invalid = || {
        vec![provider_error(
            ErrorCategory::Protocol,
            "provider reply is not valid JSON",
        )]
    };
    let chunk: Value = serde_json::from_str(payload).map_err(|_| invalid())?;
    let (input, output) = usage_of(&chunk);
    if chunk.get("usage").is_some() {
        if input.is_some() {
            acc.input = input;
        }
        if output.is_some() {
            acc.output = output;
        }
    }
    let mut applied = SseApplied {
        content: String::new(),
        usage_present: chunk.get("usage").is_some(),
        usage: (input, output),
    };
    let Some(choice) = chunk
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    else {
        return Ok(applied);
    };
    if let Some(next) = choice.get("finish_reason").and_then(Value::as_str)
        && !next.is_empty()
    {
        acc.reason = next.to_owned();
    }
    let Some(delta) = choice.get("delta") else {
        return Ok(applied);
    };
    if let Some(fragment) = delta.get("content").and_then(Value::as_str) {
        acc.text.push_str(fragment);
        applied.content.push_str(fragment);
    }
    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for (position, call) in calls.iter().enumerate() {
            let index = call
                .get("index")
                .and_then(Value::as_u64)
                .unwrap_or(position as u64);
            let entry = acc.fragments.entry(index).or_default();
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
                        return Err(vec![provider_error(
                            ErrorCategory::Protocol,
                            "provider reply is not valid JSON",
                        )]);
                    }
                }
            }
        }
    }
    Ok(applied)
}

/// Builds the authoritative batch from aggregated SSE state: at most one
/// text delta at `item-0`, calls numbered from `item-1`, then exactly one
/// terminal.
fn finalize_sse(acc: &SseAccum) -> Vec<ProviderEvent> {
    if acc.text.len()
        + acc
            .fragments
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
    if !acc.text.is_empty() {
        events.push(ProviderEvent::TextDelta {
            item_key: "item-0".to_owned(),
            text: acc.text.clone(),
        });
    }
    let mut ready = Vec::new();
    for (position, (_, call)) in acc.fragments.iter().enumerate() {
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
    match terminal_for(&acc.reason, !ready.is_empty(), acc.input, acc.output) {
        Ok(terminal) => {
            events.push(terminal);
            events
        }
        Err(failure) => failure,
    }
}

/// Incremental SSE parser: de-chunks when the head declares chunked framing,
/// splits complete lines, folds them into [`SseAccum`], and delivers
/// provisional text plus usage through the sink as chunks arrive. Tool-call
/// fragments never touch the sink: previews only make sense against the
/// finalized item keys. Returns the authoritative batch once `[DONE]`
/// completes the turn; a strict terminator is required, EOF without one is
/// a truncation failure.
struct SseLive {
    dechunker: Option<Dechunker>,
    buf: Vec<u8>,
    acc: SseAccum,
    last_usage: (Option<u64>, Option<u64>),
    raw_bytes: usize,
}

impl SseLive {
    fn new(chunked: bool) -> Self {
        Self {
            dechunker: chunked.then(Dechunker::new),
            buf: Vec::new(),
            acc: SseAccum::default(),
            last_usage: (None, None),
            raw_bytes: 0,
        }
    }

    fn feed(
        &mut self,
        bytes: &[u8],
        sink: &(dyn Fn(ProviderEvent) + Send + Sync),
    ) -> Result<Option<Vec<ProviderEvent>>, Vec<ProviderEvent>> {
        let too_large = || {
            vec![provider_error(
                ErrorCategory::ResourceLimit,
                "provider response is too large",
            )]
        };
        self.raw_bytes = self.raw_bytes.saturating_add(bytes.len());
        if self.raw_bytes > MAX_RESPONSE_BYTES {
            return Err(too_large());
        }
        if let Some(dechunker) = self.dechunker.as_mut() {
            let mut clean = Vec::new();
            dechunker.feed(bytes, &mut clean).map_err(|()| {
                vec![provider_error(
                    ErrorCategory::Protocol,
                    "provider response failed",
                )]
            })?;
            self.buf.extend_from_slice(&clean);
        } else {
            self.buf.extend_from_slice(bytes);
        }
        if self.buf.len() > MAX_RESPONSE_BYTES {
            return Err(too_large());
        }
        while let Some(line) = take_line(&mut self.buf) {
            let text = String::from_utf8_lossy(&line);
            let line = text.trim();
            let Some(payload) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload == "[DONE]" {
                return Ok(Some(finalize_sse(&self.acc)));
            }
            if payload.is_empty() {
                continue;
            }
            let applied = apply_sse_data(&mut self.acc, payload)?;
            if !applied.content.is_empty() {
                sink(ProviderEvent::TextDelta {
                    item_key: "item-0".to_owned(),
                    text: applied.content,
                });
            }
            if applied.usage_present && applied.usage != self.last_usage {
                self.last_usage = applied.usage;
                sink(ProviderEvent::Usage(Usage::new(
                    applied.usage.0,
                    applied.usage.1,
                    UsageFinality::Provisional,
                )));
            }
            if self.acc.text.len()
                + self
                    .acc
                    .fragments
                    .values()
                    .map(|call| call.arguments.len())
                    .sum::<usize>()
                > MAX_RESPONSE_BYTES
            {
                return Err(too_large());
            }
        }
        Ok(None)
    }
}

/// Incremental `Transfer-Encoding: chunked` decoder. Framing violations fail
/// the turn; bytes past the terminal zero-chunk (trailers) are ignored.
struct Dechunker {
    state: DechunkState,
}

enum DechunkState {
    Size(Vec<u8>),
    Data(usize),
    DataCrlf(u8),
    Done,
}

impl Dechunker {
    fn new() -> Self {
        Self {
            state: DechunkState::Size(Vec::new()),
        }
    }

    fn feed(&mut self, bytes: &[u8], out: &mut Vec<u8>) -> Result<(), ()> {
        let mut cursor = 0;
        while cursor < bytes.len() {
            match &mut self.state {
                DechunkState::Done => return Ok(()),
                DechunkState::Size(line) => {
                    while cursor < bytes.len() {
                        let byte = bytes[cursor];
                        cursor += 1;
                        line.push(byte);
                        if byte == b'\n' {
                            break;
                        }
                        if line.len() > 64 {
                            return Err(());
                        }
                    }
                    if line.last() != Some(&b'\n') {
                        return Ok(());
                    }
                    let text = std::str::from_utf8(line).map_err(|_| ())?;
                    let text = text.trim();
                    let size_text = text.split(';').next().unwrap_or("").trim();
                    let size = usize::from_str_radix(size_text, 16).map_err(|_| ())?;
                    if size == 0 {
                        self.state = DechunkState::Done;
                        return Ok(());
                    }
                    if out.len().saturating_add(size) > MAX_RESPONSE_BYTES {
                        return Err(());
                    }
                    self.state = DechunkState::Data(size);
                }
                DechunkState::Data(remaining) => {
                    let take = (*remaining).min(bytes.len() - cursor);
                    if out.len().saturating_add(take) > MAX_RESPONSE_BYTES {
                        return Err(());
                    }
                    out.extend_from_slice(&bytes[cursor..cursor + take]);
                    cursor += take;
                    *remaining -= take;
                    if *remaining == 0 {
                        self.state = DechunkState::DataCrlf(0);
                    }
                }
                DechunkState::DataCrlf(seen) => {
                    let expect = if *seen == 0 { b'\r' } else { b'\n' };
                    if bytes[cursor] != expect {
                        return Err(());
                    }
                    cursor += 1;
                    *seen += 1;
                    if *seen == 2 {
                        self.state = DechunkState::Size(Vec::new());
                    }
                }
            }
        }
        Ok(())
    }
}

/// Drains one complete `\n`-terminated line (without the terminator),
/// leaving any partial tail buffered.
fn take_line(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    let position = buf.iter().position(|&byte| byte == b'\n')?;
    let mut line: Vec<u8> = buf.drain(..=position).collect();
    line.pop();
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Some(line)
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

fn provider_error_inner(category: ErrorCategory, message: &'static str) -> AgentError {
    AgentError::new(category, message, RetryGuidance::DoNotRetry)
        .expect("static safe provider message builds")
}

/// Joins a validated host with a port for dialing and the `host` header,
/// bracketing IPv6 literals (`::1` becomes `[::1]:port`).
fn join_host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
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
    let mut acc = SseAccum::default();
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
        if let Err(failure) = apply_sse_data(&mut acc, payload) {
            return failure;
        }
    }
    if !done {
        return vec![provider_error(
            ErrorCategory::Protocol,
            "provider stream ended without termination",
        )];
    }
    finalize_sse(&acc)
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
    let calls: Vec<(String, String, String)> = match message.get("tool_calls") {
        None => Vec::new(),
        Some(Value::Array(calls)) => {
            let mut parsed = Vec::with_capacity(calls.len());
            for call in calls {
                let Some(id) = call.get("id").and_then(Value::as_str) else {
                    return vec![provider_error(
                        ErrorCategory::InvalidInput,
                        "provider reply tool calls are invalid",
                    )];
                };
                let Some(function) = call.get("function") else {
                    return vec![provider_error(
                        ErrorCategory::InvalidInput,
                        "provider reply tool calls are invalid",
                    )];
                };
                let Some(name) = function.get("name").and_then(Value::as_str) else {
                    return vec![provider_error(
                        ErrorCategory::InvalidInput,
                        "provider reply tool calls are invalid",
                    )];
                };
                let Some(arguments) = function.get("arguments").and_then(Value::as_str) else {
                    return vec![provider_error(
                        ErrorCategory::InvalidInput,
                        "provider reply tool calls are invalid",
                    )];
                };
                parsed.push((id.to_owned(), name.to_owned(), arguments.to_owned()));
            }
            parsed
        }
        Some(_) => {
            return vec![provider_error(
                ErrorCategory::InvalidInput,
                "provider reply tool calls are invalid",
            )];
        }
    };
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
        self.stream_with_sink(request, context, &|_| {})
    }

    fn supports_incremental_streaming(&self) -> bool {
        true
    }

    fn stream_with_sink(
        &self,
        request: &ModelRequest,
        context: &ProviderContext,
        sink: &(dyn Fn(ProviderEvent) + Send + Sync),
    ) -> Vec<ProviderEvent> {
        if context.is_cancelled() {
            return vec![provider_error(
                ErrorCategory::Cancelled,
                "provider call was cancelled",
            )];
        }
        let body_value = match self.request_body(request) {
            Ok(body_value) => body_value,
            Err(failure) => return vec![failure],
        };
        let body = serde_json::to_vec(&body_value).unwrap_or_default();
        if body.is_empty() {
            return vec![provider_error(
                ErrorCategory::Internal,
                "provider request could not be encoded",
            )];
        }
        self.stream_with_sink_impl(&body, context, sink)
    }
}
