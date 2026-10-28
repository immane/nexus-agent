//! OpenAI-compatible chat completions adapter over blocking std sockets,
//! with streaming SSE parsing and an `openssl s_client` TLS bridge.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use nexus_config::settings::ProviderSettings;
use nexus_config::{CredentialRef, ProtocolKind, ProviderProfile};
use nexus_core::{
    AgentError, CallCandidate, CorrelationData, ErrorCategory, FinishReason, Limits,
    ModelContextItem, ModelRequest, ProviderCapabilities, ProviderContext, ProviderEvent,
    ProviderPort, RetryGuidance, TurnFinished, Usage, UsageFinality,
};
use serde_json::{Value, json};

/// Maximum aggregated response content in bytes per turn, covering answer
/// text, reasoning traces, and tool-call arguments together.
pub const MAX_RESPONSE_BYTES: usize = 8_388_608;
/// SSE framing and JSON envelopes can be substantially larger than the
/// response content. Bound total bytes read separately from retained content.
const MAX_STREAM_WIRE_BYTES: usize = 64 * 1_024 * 1_024;
/// Bound one not-yet-parsed SSE record so a peer cannot stream an endless line.
const MAX_SSE_RECORD_BYTES: usize = MAX_RESPONSE_BYTES;
/// Maximum vendor model name in bytes, matching the configuration bound.
pub const MAX_MODEL_LEN: usize = 128;
const MAX_CONTEXT_METADATA_BYTES: usize = 256 * 1024;
const MAX_CATALOG_BYTES: usize = 12 * 1024 * 1024;
/// Maximum response head (status line plus headers) in bytes. Anything
/// larger fails instead of accumulating unboundedly while the head is still
/// incomplete.
const MAX_HEAD_BYTES: usize = 16_384;
/// Read poll quantum: cancellation and deadlines are re-checked between
/// quanta so a stalled peer cannot park the worker past its bound.
const READ_QUANTUM: Duration = Duration::from_secs(1);
/// The TLS stdout pump retains at most this many fixed-size chunks while the
/// parser is behind (128 KiB total). A closed consumer releases a blocked send.
const TLS_PUMP_CHUNKS: usize = 16;
const TLS_PUMP_CHUNK_BYTES: usize = 8_192;
/// Bound distinct provider tool identities before creating accumulator state.
const MAX_SSE_TOOL_IDENTITIES: usize = Limits::M0_TEST_TOOL_CALLS_PER_TURN as usize;
/// Bound record count independently of byte limits to cap parser CPU work.
/// Leave room for several framing events per output token at the request cap.
const MAX_SSE_RECORDS: usize = 262_144;

fn provider_error(category: ErrorCategory, message: &'static str) -> ProviderEvent {
    ProviderEvent::Failed(
        AgentError::new(category, message, RetryGuidance::DoNotRetry)
            .expect("static safe provider message builds"),
    )
}

fn retryable_provider_error(category: ErrorCategory, message: &'static str) -> ProviderEvent {
    ProviderEvent::Failed(
        AgentError::new(category, message, RetryGuidance::RetryAfterBackoff)
            .expect("static safe retryable provider message builds"),
    )
}

/// Splits a non-2xx status into its typed class and static fallback.
/// Classes stay value-free; the split exists so a rejected request body
/// (ours to fix), a billing refusal (theirs to top up), and a server
/// outage point in different directions instead of merging into one
/// undifferentiated failure.
fn status_class(status: u16) -> (ErrorCategory, &'static str) {
    (
        match status {
            401 | 403 => ErrorCategory::Authentication,
            402 => ErrorCategory::PermissionDenied,
            400 | 413 | 422 | 431 => ErrorCategory::InvalidInput,
            429 => ErrorCategory::RateLimited,
            _ => ErrorCategory::Protocol,
        },
        match status {
            401 | 403 => "provider credentials were rejected",
            402 => "provider payment is required",
            400 | 422 => "provider request was rejected",
            413 => "provider request body is too large",
            431 => "provider request headers are too large",
            429 => "provider rate limit reached",
            _ => "provider request failed",
        },
    )
}

fn status_retry_guidance(status: u16) -> RetryGuidance {
    // Remote access refusals (including auth, billing, routing and redirects)
    // can recover without changing the local request. Request validation and
    // size errors remain terminal; redirects are never followed with credentials.
    if (300..=599).contains(&status) && !matches!(status, 400 | 413 | 422 | 431) {
        RetryGuidance::RetryAfterBackoff
    } else {
        RetryGuidance::DoNotRetry
    }
}

/// Vendor bodies can echo prompts, paths, or credentials without any secret
/// marker. Only the status classification crosses the diagnostic boundary.
fn status_error(status: u16) -> ProviderEvent {
    status_error_with_head(status, "")
}

fn status_error_with_head(status: u16, head: &str) -> ProviderEvent {
    let (category, fallback) = status_class(status);
    let mut correlation = CorrelationData::new();
    correlation
        .push("http_status", status.to_string())
        .expect("numeric status is safe");
    // Support delta-seconds without adding a date parser dependency. Values
    // stay numeric and never expose untrusted header contents in diagnostics.
    if let Some(seconds) = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("retry-after")
            .then(|| value.trim().parse::<u64>().ok())
            .flatten()
    }) {
        correlation
            .push("retry_after_seconds", seconds.to_string())
            .expect("numeric retry delay is safe");
    }
    ProviderEvent::Failed(
        AgentError::with_correlation(
            category,
            fallback,
            correlation,
            status_retry_guidance(status),
        )
        .expect("static status diagnostic is safe"),
    )
}

/// Configured HTTP protocol provider, plus `https` through the system
/// `openssl s_client` TLS bridge. The profile selects Chat Completions,
/// Responses, or Anthropic Messages; see [`OpenAiProvider::from_profile`].
pub struct OpenAiProvider {
    operational_settings: ProviderSettings,
    context_management: nexus_core::ContextManagementSettings,
    host: String,
    port: u16,
    base_path: String,
    credential: CredentialRef,
    model: String,
    use_tls: bool,
    protocol: ProtocolKind,
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
        Self::new_for_protocol(endpoint, credential, model, ProtocolKind::ChatCompletions)
    }

    /// Builds a provider for a configured wire protocol.
    pub fn new_for_protocol(
        endpoint: &str,
        credential: CredentialRef,
        model: impl Into<String>,
        protocol: ProtocolKind,
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
            operational_settings: ProviderSettings::default(),
            context_management: nexus_core::ContextManagementSettings::default(),
            host,
            port,
            base_path: base_path.trim_end_matches('/').to_owned(),
            credential,
            model,
            use_tls,
            protocol,
        })
    }

    /// Builds an adapter from a configured provider profile plus the vendor
    /// model string to send (a selected [`ModelEntry`] name or the profile
    /// default). The protocol determines endpoint, authentication headers,
    /// request/response mapping, and stream termination rules; direct/relay
    /// remains an independent routing selection.
    ///
    /// [`ModelEntry`]: nexus_config::ModelEntry
    pub fn from_profile(profile: &ProviderProfile, model: &str) -> Result<Self, AgentError> {
        let endpoint = profile.endpoint.as_deref().ok_or_else(|| {
            provider_error_inner(ErrorCategory::InvalidInput, "provider endpoint is missing")
        })?;
        Self::new_for_protocol(
            endpoint,
            profile.credential.clone(),
            model,
            profile.protocol,
        )
    }

    /// Binds timeout and retry settings without performing any network I/O.
    /// Apply this before context discovery so its timeout also takes effect.
    ///
    /// # Errors
    /// Rejects zero, excessive, empty, or decreasing operational bounds.
    pub fn with_operational_settings(
        mut self,
        settings: &ProviderSettings,
    ) -> Result<Self, AgentError> {
        Self::validate_operational_settings(settings)?;
        self.operational_settings = settings.clone();
        Ok(self)
    }

    /// Checks public operational settings defensively before installing them.
    pub fn validate_operational_settings(settings: &ProviderSettings) -> Result<(), AgentError> {
        if !(1..=86_400).contains(&settings.connect_timeout_seconds)
            || !(1..=86_400).contains(&settings.context_discovery_timeout_seconds)
            || !(1..=86_400).contains(&settings.fallback_call_timeout_seconds)
            || settings.retry_backoff_seconds.is_empty()
            || settings.retry_backoff_seconds.len() > 8
            || settings
                .retry_backoff_seconds
                .iter()
                .any(|seconds| !(1..=3_600).contains(seconds))
            || settings
                .retry_backoff_seconds
                .windows(2)
                .any(|pair| pair[0] > pair[1])
        {
            return Err(provider_error_inner(
                ErrorCategory::InvalidInput,
                "provider operational settings are invalid",
            ));
        }
        Ok(())
    }

    /// Binds validated host settings. This does not perform discovery or enable
    /// token counting or automatic compaction for any protocol.
    #[must_use]
    pub fn with_context_management(
        mut self,
        settings: nexus_core::ContextManagementSettings,
    ) -> Self {
        self.context_management = settings;
        self
    }

    /// Best-effort model metadata discovery. Explicit configuration wins;
    /// malformed metadata and transient failures leave the window unknown.
    /// Frontends should use `supports_context_discovery` before calling this.
    #[must_use]
    pub fn discover_context_management(self) -> Self {
        if self.context_management.window.is_some()
            || self.protocol != ProtocolKind::ChatCompletions
        {
            return self;
        }
        let tokens = if self.is_opencode_go_endpoint() {
            self.discover_opencode_go_context_window()
        } else {
            self.discover_ollama_context_window()
        };
        self.apply_discovered_context(tokens)
    }

    fn apply_discovered_context(mut self, tokens: Option<u32>) -> Self {
        if let Some(tokens) = tokens
            && let Ok(window) = nexus_core::ContextWindow::new(
                tokens,
                nexus_core::ContextWindowSource::ModelMetadata,
            )
        {
            self.context_management.window = Some(window);
        }
        self
    }

    /// Conservative frontend hint for when to invoke automatic Ollama
    /// discovery without probing unrelated Chat Completions APIs.
    #[must_use]
    pub fn is_ollama_endpoint(&self) -> bool {
        self.protocol == ProtocolKind::ChatCompletions
            && (self.port == 11_434 || self.host.to_ascii_lowercase().contains("ollama"))
    }

    /// Whether this is the official OpenCode Go Chat Completions endpoint.
    #[must_use]
    pub fn is_opencode_go_endpoint(&self) -> bool {
        self.protocol == ProtocolKind::ChatCompletions
            && self.use_tls
            && self.host.eq_ignore_ascii_case("opencode.ai")
            && self.port == 443
            && self.base_path == "/inference/go/openai/v1"
    }

    /// Whether the frontend should attempt automatic metadata discovery.
    #[must_use]
    pub fn supports_context_discovery(&self) -> bool {
        self.is_ollama_endpoint() || self.is_opencode_go_endpoint()
    }

    fn discover_opencode_go_context_window(&self) -> Option<u32> {
        let catalog = Self::new("https://models.dev", self.credential.clone(), &self.model)
            .ok()?
            .with_operational_settings(&self.operational_settings)
            .ok()?;
        self.discover_opencode_go_from(&catalog)
    }

    fn discover_opencode_go_from(&self, catalog: &Self) -> Option<u32> {
        let address = join_host_port(&catalog.host, catalog.port);
        let request = format!(
            "GET /api.json HTTP/1.1\r\nhost: {address}\r\nuser-agent: nexus-agent/{}\r\naccept: application/json\r\nconnection: close\r\n\r\n",
            env!("CARGO_PKG_VERSION")
        );
        let response =
            read_context_metadata_response(catalog, request.as_bytes(), MAX_CATALOG_BYTES)?;
        parse_opencode_go_context_window(&response, &self.model)
    }

    fn discover_ollama_context_window(&self) -> Option<u32> {
        let token = nexus_config::resolve_credential(&self.credential).ok()?;
        if token.bytes().any(|byte| byte.is_ascii_control()) {
            return None;
        }
        let api_prefix = self
            .base_path
            .strip_suffix("/v1")
            .unwrap_or(&self.base_path);
        let path = format!("{api_prefix}/api/show");
        let body = serde_json::to_vec(&json!({ "model": self.model })).ok()?;
        let address = join_host_port(&self.host, self.port);
        let head = format!(
            "POST {path} HTTP/1.1\r\nhost: {address}\r\ncontent-type: application/json\r\naccept: application/json\r\nauthorization: Bearer {token}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        let mut request = head.into_bytes();
        request.extend_from_slice(&body);
        let response = read_context_metadata_response(self, &request, MAX_CONTEXT_METADATA_BYTES)?;
        parse_ollama_context_window(&response)
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
        if request.continuation().is_some() {
            return Err(provider_error(
                ErrorCategory::UnsupportedCapability,
                "provider does not accept opaque continuation state",
            ));
        }
        validate_conversation(request.conversation())?;
        if self.protocol != ProtocolKind::ChatCompletions {
            return protocol_request_body(self.protocol, &self.model, request);
        }
        let mut messages = Vec::new();
        if let Some(instructions) = request.rendered_instructions() {
            messages.push(json!({
                "role": "system",
                "content": instructions,
            }));
        }
        messages.extend(conversation_messages(request.conversation()));
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
            "max_tokens": output_token_limit(request),
        });
        if self.is_ollama_endpoint()
            && let Some(effort) = request.reasoning_effort()
        {
            body["reasoning_effort"] = Value::String(effort.as_str().to_owned());
        }
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
        let deadline = Some(call_deadline(
            context.deadline(),
            &self.operational_settings,
            Instant::now(),
        ));
        if deadline.is_some_and(|at| Instant::now() >= at) {
            return vec![provider_error(
                ErrorCategory::Timeout,
                "provider deadline elapsed before send",
            )];
        }
        let extra_headers = match self.opencode_go_headers(request) {
            Ok(headers) => headers,
            Err(failure) => return vec![failure],
        };
        let token = match nexus_config::resolve_credential(&self.credential) {
            Ok(token) => token,
            Err(_) => {
                return vec![provider_error(
                    ErrorCategory::Authentication,
                    "provider credential is unavailable",
                )];
            }
        };
        if token.bytes().any(|byte| byte.is_ascii_control()) {
            return vec![provider_error(
                ErrorCategory::Authentication,
                "provider credential contains invalid header control characters",
            )];
        }
        let path = format!(
            "{}{}",
            self.base_path,
            protocol_path(self.protocol, &self.base_path)
        );
        let address = join_host_port(&self.host, self.port);
        let auth = match self.protocol {
            ProtocolKind::AnthropicMessages => {
                format!("x-api-key: {token}\r\nanthropic-version: 2023-06-01\r\n")
            }
            ProtocolKind::ChatCompletions | ProtocolKind::OpenAiResponses => {
                format!("authorization: Bearer {token}\r\n")
            }
        };
        let head = format!(
            "POST {path} HTTP/1.1\r\nhost: {}\r\ncontent-type: application/json\r\naccept: text/event-stream, application/json\r\ncontent-length: {}\r\n{auth}{extra_headers}connection: close\r\n\r\n",
            address,
            body.len(),
        );
        retry_exchange(
            &self.operational_settings,
            context,
            deadline,
            sink,
            |tracked_sink| {
                if self.use_tls {
                    match spawn_tls(
                        &self.host,
                        self.port,
                        head.as_bytes(),
                        body,
                        context,
                        deadline,
                        Duration::from_secs(self.operational_settings.connect_timeout_seconds),
                    ) {
                        Ok(mut source) => stream_response(
                            &mut source,
                            deadline,
                            context,
                            tracked_sink,
                            self.protocol,
                        ),
                        Err(failure) => vec![failure],
                    }
                } else {
                    match connect_plain(
                        &self.host,
                        self.port,
                        context,
                        deadline,
                        Duration::from_secs(self.operational_settings.connect_timeout_seconds),
                    ) {
                        Ok(mut stream) => {
                            if let Err(failure) =
                                write_request(&mut stream, head.as_bytes(), body, context, deadline)
                            {
                                return vec![failure];
                            }
                            let mut source = TcpSource {
                                stream: &mut stream,
                            };
                            stream_response(
                                &mut source,
                                deadline,
                                context,
                                tracked_sink,
                                self.protocol,
                            )
                        }
                        Err(failure) => vec![failure],
                    }
                }
            },
            wait_before_retry,
        )
    }

    fn opencode_go_headers(&self, request: &ModelRequest) -> Result<String, ProviderEvent> {
        if !self.use_tls
            || self.port != 443
            || !self.host.eq_ignore_ascii_case("opencode.ai")
            || self.base_path != "/inference/go/openai/v1"
            || self.protocol != ProtocolKind::ChatCompletions
        {
            return Ok(String::new());
        }
        let session = request.session().ok_or_else(|| {
            provider_error(
                ErrorCategory::InvalidInput,
                "OpenCode Go requires a conversation session identity",
            )
        })?;
        Ok(format!(
            "user-agent: nexus-agent/{}\r\nx-opencode-session: {}\r\n",
            env!("CARGO_PKG_VERSION"),
            session.as_str()
        ))
    }
}

fn retry_exchange(
    settings: &ProviderSettings,
    context: &ProviderContext,
    deadline: Option<Instant>,
    sink: &(dyn Fn(ProviderEvent) + Send + Sync),
    mut exchange: impl FnMut(&(dyn Fn(ProviderEvent) + Send + Sync)) -> Vec<ProviderEvent>,
    mut wait: impl FnMut(&ProviderContext, Option<Instant>, Duration) -> Result<(), ProviderEvent>,
) -> Vec<ProviderEvent> {
    // Keep this guard here as well as at the public entry point: no caller of
    // the retry driver can accidentally create an unbounded deadline-free loop.
    let deadline = Some(call_deadline(deadline, settings, Instant::now()));
    let mut retry = 0usize;
    loop {
        if let Err(failure) = check_live(context, deadline) {
            return vec![failure];
        }
        let emitted = AtomicBool::new(false);
        let tracked_sink = |event| {
            emitted.store(true, Ordering::Relaxed);
            sink(event);
        };
        let events = exchange(&tracked_sink);
        if let Err(failure) = check_live(context, deadline) {
            return vec![failure];
        }
        let will_retry = !emitted.load(Ordering::Relaxed) && has_retryable_terminal(&events);
        let mut delay = retry_delay(settings, retry);
        if let Some(ProviderEvent::Failed(error)) = events.last() {
            if let Some(seconds) = error
                .correlation()
                .iter()
                .find(|(key, _)| key == "retry_after_seconds")
                .and_then(|(_, value)| value.parse::<u64>().ok())
            {
                delay = delay.max(Duration::from_secs(seconds));
            }
            log_provider_failure(error, retry.saturating_add(1), will_retry.then_some(delay));
        }
        if !will_retry {
            return events;
        }
        if let Err(failure) = wait(context, deadline, delay) {
            return vec![failure];
        }
        retry = retry.saturating_add(1);
    }
}

fn has_retryable_terminal(events: &[ProviderEvent]) -> bool {
    matches!(
        events.last(),
        Some(ProviderEvent::Failed(error))
            if error.retry() == RetryGuidance::RetryAfterBackoff
    )
}

/// Logs only static provider diagnostics, retry classification, and numeric
/// HTTP status. Request/response bodies, prompts, and credentials never enter
/// this diagnostic line.
fn log_provider_failure(error: &AgentError, attempt: usize, delay: Option<Duration>) {
    let http_status = error
        .correlation()
        .iter()
        .find(|(key, _)| key == "http_status")
        .map_or("none", |(_, value)| value.as_str());
    let retry = delay.map_or_else(
        || "not-scheduled".to_owned(),
        |delay| format!("in {delay:?}"),
    );
    eprintln!(
        "nexus-openai: provider attempt {attempt} failed: category={:?}, retry={:?} ({retry}), http_status={http_status}, message={}",
        error.category(),
        error.retry(),
        error.message(),
    );
}

fn call_deadline(deadline: Option<Instant>, settings: &ProviderSettings, now: Instant) -> Instant {
    deadline.unwrap_or_else(|| now + Duration::from_secs(settings.fallback_call_timeout_seconds))
}

fn retry_delay(settings: &ProviderSettings, retry: usize) -> Duration {
    let delays = &settings.retry_backoff_seconds;
    Duration::from_secs(delays[retry.min(delays.len() - 1)])
}

fn connect_timeout_for(timeout: Duration, deadline: Option<Instant>, now: Instant) -> Duration {
    deadline.map_or(timeout, |at| timeout.min(at.saturating_duration_since(now)))
}

fn wait_before_retry(
    context: &ProviderContext,
    deadline: Option<Instant>,
    delay: Duration,
) -> Result<(), ProviderEvent> {
    let started = Instant::now();
    loop {
        check_live(context, deadline)?;
        let remaining = delay.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Ok(());
        }
        std::thread::sleep(
            remaining
                .min(Duration::from_millis(50))
                .min(quantum_for(deadline)),
        );
    }
}

fn protocol_path(protocol: ProtocolKind, base_path: &str) -> &'static str {
    match protocol {
        ProtocolKind::ChatCompletions => "/chat/completions",
        ProtocolKind::OpenAiResponses if base_path.ends_with("/v1") => "/responses",
        ProtocolKind::OpenAiResponses => "/v1/responses",
        ProtocolKind::AnthropicMessages if base_path.ends_with("/v1") => "/messages",
        ProtocolKind::AnthropicMessages => "/v1/messages",
    }
}

fn output_token_limit(request: &ModelRequest) -> usize {
    request.max_output_tokens().map_or_else(
        || request.output_budget_bytes().div_ceil(4).max(1),
        |tokens| tokens as usize,
    )
}

fn protocol_request_body(
    protocol: ProtocolKind,
    model: &str,
    request: &ModelRequest,
) -> Result<Value, ProviderEvent> {
    let tool_defs = request
        .tool_definitions()
        .iter()
        .map(|spec| {
            let schema: Value = serde_json::from_str(spec.input_schema_json()).map_err(|_| {
                provider_error(
                    ErrorCategory::InvalidInput,
                    "provider tool schema is invalid",
                )
            })?;
            Ok((spec, schema))
        })
        .collect::<Result<Vec<_>, ProviderEvent>>()?;
    let max_tokens = output_token_limit(request);
    match protocol {
        ProtocolKind::ChatCompletions => unreachable!("handled by the chat request encoder"),
        ProtocolKind::OpenAiResponses => {
            let mut input = Vec::new();
            for item in request.conversation() {
                if item.reasoning().is_some() {
                    return Err(provider_error(
                        ErrorCategory::UnsupportedCapability,
                        "Responses protocol cannot replay this reasoning context",
                    ));
                }
                match item {
                    ModelContextItem::UserText(text) => {
                        input.push(json!({"role":"user","content":text.as_str()}));
                    }
                    ModelContextItem::AssistantText { text, .. } => {
                        input.push(json!({"role":"assistant","content":text.as_str()}));
                    }
                    ModelContextItem::AssistantCall {
                        provider_ref, call, ..
                    } => input.push(json!({
                        "type":"function_call",
                        "call_id":provider_ref.as_str(),
                        "name":call.tool().name(),
                        "arguments":call.args().as_str(),
                    })),
                    ModelContextItem::AssistantDeniedCall { candidate, .. } => {
                        input.push(json!({
                            "type":"function_call",
                            "call_id":candidate.provider_ref(),
                            "name":candidate.tool_name(),
                            "arguments":candidate.arguments_json(),
                        }));
                    }
                    ModelContextItem::ToolResult {
                        provider_ref,
                        outcome,
                        ..
                    } => input.push(json!({
                        "type":"function_call_output",
                        "call_id":provider_ref.as_str(),
                        "output":outcome.content(),
                    })),
                    ModelContextItem::AssistantReasoning { .. } => {
                        return Err(provider_error(
                            ErrorCategory::UnsupportedCapability,
                            "Responses protocol cannot replay this reasoning context",
                        ));
                    }
                }
            }
            let tools: Vec<Value> = tool_defs
                .into_iter()
                .map(|(spec, schema)| {
                    json!({
                        "type":"function",
                        "name":spec.id().name(),
                        "description":spec.description(),
                        "parameters":schema,
                    })
                })
                .collect();
            let mut body = json!({
                "model":model,
                "input":input,
                "stream":true,
                "max_output_tokens":max_tokens,
            });
            if let Some(instructions) = request.rendered_instructions() {
                body["instructions"] = Value::String(instructions);
            }
            if !tools.is_empty() {
                body["tools"] = Value::Array(tools);
            }
            Ok(body)
        }
        ProtocolKind::AnthropicMessages => {
            let mut messages: Vec<Value> = Vec::new();
            for item in request.conversation() {
                if item.reasoning().is_some() {
                    return Err(provider_error(
                        ErrorCategory::UnsupportedCapability,
                        "Messages protocol cannot replay this reasoning context",
                    ));
                }
                let next = match item {
                    ModelContextItem::UserText(text) => {
                        json!({"role":"user","content":[{"type":"text","text":text.as_str()}]})
                    }
                    ModelContextItem::AssistantText { text, .. } => {
                        json!({"role":"assistant","content":[{"type":"text","text":text.as_str()}]})
                    }
                    ModelContextItem::AssistantCall {
                        provider_ref, call, ..
                    } => {
                        let input: Value =
                            serde_json::from_str(call.args().as_str()).map_err(|_| {
                                provider_error(
                                    ErrorCategory::InvalidInput,
                                    "provider tool arguments are invalid",
                                )
                            })?;
                        json!({"role":"assistant","content":[{"type":"tool_use","id":provider_ref.as_str(),"name":call.tool().name(),"input":input}]})
                    }
                    ModelContextItem::AssistantDeniedCall { candidate, .. } => {
                        let input: Value = serde_json::from_str(candidate.arguments_json())
                            .map_err(|_| {
                                provider_error(
                                    ErrorCategory::InvalidInput,
                                    "provider tool arguments are invalid",
                                )
                            })?;
                        json!({"role":"assistant","content":[{"type":"tool_use","id":candidate.provider_ref(),"name":candidate.tool_name(),"input":input}]})
                    }
                    ModelContextItem::ToolResult {
                        provider_ref,
                        outcome,
                        ..
                    } => {
                        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":provider_ref.as_str(),"content":outcome.content()}]})
                    }
                    ModelContextItem::AssistantReasoning { .. } => {
                        return Err(provider_error(
                            ErrorCategory::UnsupportedCapability,
                            "Messages protocol cannot replay this reasoning context",
                        ));
                    }
                };
                if let Some(last) = messages.last_mut()
                    && last["role"] == next["role"]
                    && let (Some(old), Some(add)) =
                        (last["content"].as_array_mut(), next["content"].as_array())
                {
                    old.extend(add.iter().cloned());
                } else {
                    messages.push(next);
                }
            }
            let tools: Vec<Value> = tool_defs
                .into_iter()
                .map(|(spec, schema)| {
                    json!({
                        "name":spec.id().name(),
                        "description":spec.description(),
                        "input_schema":schema,
                    })
                })
                .collect();
            let mut body = json!({
                "model":model,
                "messages":messages,
                "max_tokens":max_tokens,
                "stream":true,
            });
            if let Some(instructions) = request.rendered_instructions() {
                body["system"] = Value::String(instructions);
            }
            if !tools.is_empty() {
                body["tools"] = Value::Array(tools);
            }
            Ok(body)
        }
    }
}

/// Opens a plain-HTTP connection with a bounded connect timeout.
fn connect_plain(
    host: &str,
    port: u16,
    context: &ProviderContext,
    deadline: Option<Instant>,
    connect_timeout: Duration,
) -> Result<TcpStream, ProviderEvent> {
    check_live(context, deadline)?;
    let address = join_host_port(host, port);
    if let Ok(addresses) = address.to_socket_addrs() {
        for address in addresses {
            check_live(context, deadline)?;
            let timeout = connect_timeout_for(connect_timeout, deadline, Instant::now());
            match TcpStream::connect_timeout(&address, timeout.max(Duration::from_millis(1))) {
                Ok(stream) => {
                    check_live(context, deadline)?;
                    return Ok(stream);
                }
                Err(error) if retryable_connection_error(&error) => {}
                Err(_) => {
                    check_live(context, deadline)?;
                    return Err(provider_error(
                        ErrorCategory::Protocol,
                        "provider socket creation failed locally",
                    ));
                }
            }
        }
    }
    check_live(context, deadline)?;
    Err(retryable_provider_error(
        ErrorCategory::Protocol,
        "provider is unreachable",
    ))
}

fn retryable_connection_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::AddrNotAvailable
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::HostUnreachable
            | std::io::ErrorKind::NetworkUnreachable
            | std::io::ErrorKind::NetworkDown
    )
}

fn write_request(
    stream: &mut TcpStream,
    head: &[u8],
    body: &[u8],
    context: &ProviderContext,
    deadline: Option<Instant>,
) -> Result<(), ProviderEvent> {
    for mut bytes in [head, body] {
        while !bytes.is_empty() {
            check_live(context, deadline)?;
            stream
                .set_write_timeout(Some(quantum_for(deadline).max(Duration::from_millis(1))))
                .map_err(|_| {
                    provider_error(
                        ErrorCategory::Protocol,
                        "provider socket configuration failed",
                    )
                })?;
            match stream.write(bytes) {
                Ok(0) => {
                    return Err(retryable_provider_error(
                        ErrorCategory::Protocol,
                        "provider request failed",
                    ));
                }
                Ok(written) => bytes = &bytes[written..],
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::Interrupted
                            | std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => {
                    return Err(retryable_provider_error(
                        ErrorCategory::Protocol,
                        "provider request failed",
                    ));
                }
            }
        }
    }
    check_live(context, deadline)
}

/// Sends one tightly bounded metadata request using the provider's configured
/// origin. Discovery is deliberately not retried and never changes run state.
fn read_context_metadata_response(
    provider: &OpenAiProvider,
    request: &[u8],
    max_bytes: usize,
) -> Option<Vec<u8>> {
    // The request is already assembled and bounded. Split at the HTTP header
    // terminator so the verified TLS helper can write the exact wire bytes.
    let boundary = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")? + 4;
    let (request_head, request_body) = request.split_at(boundary);
    let deadline = Instant::now().checked_add(Duration::from_secs(
        provider
            .operational_settings
            .context_discovery_timeout_seconds,
    ))?;
    let connect_timeout =
        Duration::from_secs(provider.operational_settings.connect_timeout_seconds);
    let context = ProviderContext::new(Duration::ZERO, false, None);
    let mut response = Vec::new();
    if provider.use_tls {
        let mut source = spawn_tls(
            &provider.host,
            provider.port,
            request_head,
            request_body,
            &context,
            Some(deadline),
            connect_timeout,
        )
        .ok()?;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match source
                .next_chunk(
                    remaining.min(Duration::from_millis(100)),
                    Some(deadline),
                    &context,
                )
                .ok()?
            {
                Some(chunk) => {
                    response.extend_from_slice(&chunk);
                    if response.len() > max_bytes {
                        return None;
                    }
                }
                None => break,
            }
        }
    } else {
        let address = join_host_port(&provider.host, provider.port);
        let mut stream = address.to_socket_addrs().ok()?.find_map(|address| {
            let timeout = connect_timeout_for(connect_timeout, Some(deadline), Instant::now());
            (!timeout.is_zero())
                .then(|| TcpStream::connect_timeout(&address, timeout))
                .and_then(Result::ok)
        })?;
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .ok()?;
        stream.write_all(request).ok()?;
        let mut buffer = [0u8; 8192];
        while Instant::now() < deadline {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    response.extend_from_slice(&buffer[..read]);
                    if response.len() > max_bytes {
                        return None;
                    }
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::TimedOut
                        || error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return None,
            }
        }
    }
    let boundary = response.windows(4).position(|bytes| bytes == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&response[..boundary]).ok()?;
    if !head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .starts_with('2')
    {
        return None;
    }
    let body = if head.to_ascii_lowercase().contains("chunked") {
        dechunk_bounded(&response[boundary + 4..], max_bytes)?
    } else {
        response[boundary + 4..].to_vec()
    };
    (body.len() <= max_bytes).then_some(body)
}

fn parse_opencode_go_context_window(response: &[u8], model: &str) -> Option<u32> {
    let value: Value = serde_json::from_slice(response).ok()?;
    value
        .get("opencode-go")?
        .get("models")?
        .get(model)?
        .get("limit")?
        .get("context")?
        .as_u64()
        .and_then(|tokens| u32::try_from(tokens).ok())
        .filter(|tokens| *tokens > 0)
}

#[cfg(test)]
mod catalog_tests {
    use super::*;
    use nexus_core::{ContextWindow, ContextWindowSource, ProviderPort};
    use std::net::TcpListener;

    fn adapter(endpoint: &str, protocol: ProtocolKind) -> OpenAiProvider {
        OpenAiProvider::new_for_protocol(
            endpoint,
            CredentialRef::env_var("CATALOG_TEST_UNUSED").unwrap(),
            "model-a",
            protocol,
        )
        .unwrap()
    }

    #[test]
    fn catalog_requires_exact_provider_model_and_positive_u32_limit() {
        let json = br#"{"opencode-go":{"models":{"model-a":{"limit":{"context":1000000}},"zero":{"limit":{"context":0}},"huge":{"limit":{"context":4294967296}}}},"other":{"models":{"missing":{"limit":{"context":123}}}}}"#;
        assert_eq!(
            parse_opencode_go_context_window(json, "model-a"),
            Some(1_000_000)
        );
        for model in ["Model-a", "missing", "zero", "huge"] {
            assert_eq!(parse_opencode_go_context_window(json, model), None);
        }
        assert_eq!(
            parse_opencode_go_context_window(b"not json", "model-a"),
            None
        );
    }

    #[test]
    fn scope_and_configured_precedence() {
        let official = adapter(
            "https://opencode.ai/inference/go/openai/v1",
            ProtocolKind::ChatCompletions,
        );
        assert!(official.supports_context_discovery());
        for endpoint in [
            "http://opencode.ai/inference/go/openai/v1",
            "https://opencode.ai:444/inference/go/openai/v1",
            "https://opencode.ai.example/inference/go/openai/v1",
            "https://opencode.ai/inference/go/openai/v2",
        ] {
            assert!(!adapter(endpoint, ProtocolKind::ChatCompletions).supports_context_discovery());
        }
        assert!(
            !adapter(
                "https://opencode.ai/inference/go/openai/v1",
                ProtocolKind::OpenAiResponses
            )
            .supports_context_discovery()
        );
        let settings = nexus_core::ContextManagementSettings {
            window: Some(ContextWindow::new(42_000, ContextWindowSource::Configured).unwrap()),
            ..Default::default()
        };
        assert_eq!(
            official
                .with_context_management(settings)
                .discover_context_management()
                .context_management(),
            settings
        );
    }

    #[test]
    fn catalog_request_is_unauthenticated_and_worker_finishes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0; 1];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let body = r#"{"opencode-go":{"models":{"model-a":{"limit":{"context":1000000}}}}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            String::from_utf8(request).unwrap()
        });
        let official = adapter(
            "https://opencode.ai/inference/go/openai/v1",
            ProtocolKind::ChatCompletions,
        );
        let catalog = adapter(&format!("http://{address}"), ProtocolKind::ChatCompletions);
        let tokens = official.discover_opencode_go_from(&catalog);
        let window = official
            .apply_discovered_context(tokens)
            .context_management()
            .window
            .unwrap();
        assert_eq!(window.tokens(), 1_000_000);
        assert_eq!(window.source(), ContextWindowSource::ModelMetadata);
        let request = worker.join().unwrap();
        assert!(request.starts_with("GET /api.json HTTP/1.1\r\n"));
        assert!(!request.to_ascii_lowercase().contains("authorization"));
        assert!(request.contains(concat!(
            "user-agent: nexus-agent/",
            env!("CARGO_PKG_VERSION")
        )));
    }
}

/// Reads the positive context limit exposed by Ollama's model-details API.
/// A configured `num_ctx` parameter takes precedence over architecture max.
fn parse_ollama_context_window(response: &[u8]) -> Option<u32> {
    let value: Value = serde_json::from_slice(response).ok()?;
    if let Some(parameters) = value.get("parameters").and_then(Value::as_str) {
        for line in parameters.lines() {
            let mut fields = line.split_whitespace();
            if fields.next() == Some("num_ctx")
                && let Some(tokens) = fields.next().and_then(|value| value.parse::<u32>().ok())
                && tokens > 0
            {
                return Some(tokens);
            }
        }
    }
    value
        .get("model_info")?
        .as_object()?
        .iter()
        .find(|(key, _)| key.as_str() == "context_length" || key.ends_with(".context_length"))
        .and_then(|(_, value)| value.as_u64())
        .and_then(|tokens| u32::try_from(tokens).ok())
        .filter(|tokens| *tokens > 0)
}

/// Spawns the verified-TLS helper, writes one request into its encrypted
/// pipe, and returns the stdout pump source. The bearer credential travels
/// inside the pipe (argv carries only host and port, never the secret).
/// Dropping the source terminates the helper.
fn spawn_tls(
    host: &str,
    port: u16,
    head: &[u8],
    body: &[u8],
    context: &ProviderContext,
    deadline: Option<Instant>,
    connect_timeout: Duration,
) -> Result<TlsSource, ProviderEvent> {
    check_live(context, deadline)?;
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
        .stderr(Stdio::piped());
    check_live(context, deadline)?;
    let mut child: Child = command
        .spawn()
        .map_err(|_| provider_error(ErrorCategory::Protocol, "TLS helper is unavailable"))?;
    let request: Vec<u8> = head.iter().chain(body.iter()).copied().collect();
    let stderr = child.stderr.take().ok_or_else(|| {
        kill_child(&mut child);
        provider_error(ErrorCategory::Protocol, "TLS helper pipe is unavailable")
    })?;
    let (security_sender, security_receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = security_sender.send(tls_terminal_diagnostic(stderr));
    });
    let Some(mut stdin) = child.stdin.take() else {
        kill_child(&mut child);
        return Err(provider_error(
            ErrorCategory::Protocol,
            "TLS helper pipe is unavailable",
        ));
    };
    {
        // `-quiet` implies `-ign_eof`: dropping stdin after the request
        // does not close the connection from our side.
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = sender.send(stdin.write_all(&request));
        });
        loop {
            if let Err(failure) = check_live(context, deadline) {
                kill_child(&mut child);
                return Err(failure);
            }
            match receiver.recv_timeout(quantum_for(deadline).max(Duration::from_millis(1))) {
                Ok(Ok(())) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                _ => {
                    kill_child(&mut child);
                    let security = security_receiver
                        .recv_timeout(quantum_for(deadline).max(Duration::from_millis(1)))
                        .unwrap_or(true);
                    check_live(context, deadline)?;
                    return Err(if security {
                        provider_error(ErrorCategory::Protocol, "TLS verification failed")
                    } else {
                        retryable_provider_error(ErrorCategory::Protocol, "provider request failed")
                    });
                }
            }
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
    let (sender, receiver) = mpsc::sync_channel::<Result<Vec<u8>, std::io::Error>>(TLS_PUMP_CHUNKS);
    std::thread::spawn(move || {
        let mut reader = stdout;
        loop {
            let mut chunk = [0u8; TLS_PUMP_CHUNK_BYTES];
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
        connect_timeout,
        security_receiver,
    })
}

fn tls_terminal_diagnostic(mut reader: impl Read) -> bool {
    // Drain without retaining/logging diagnostics. Only a terminal-failure bit
    // crosses this boundary, with bounded rolling storage across reads.
    let mut tail = String::new();
    let mut terminal = false;
    let mut bytes = [0u8; 1024];
    loop {
        match reader.read(&mut bytes) {
            Ok(0) => return terminal,
            Ok(read) => {
                tail.push_str(&String::from_utf8_lossy(&bytes[..read]).to_ascii_lowercase());
                terminal |= [
                    "verify error",
                    "verification error",
                    "certificate verify failed",
                    "hostname mismatch",
                    "ip address mismatch",
                    "no protocols available",
                    "no shared cipher",
                    "unknown option",
                    "unrecognized option",
                    "invalid command",
                    "error setting certificate",
                    "error setting verify",
                    "unable to load",
                ]
                .iter()
                .any(|marker| tail.contains(marker));
                tail = tail
                    .chars()
                    .rev()
                    .take(128)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return true,
        }
    }
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
            Err(_) => Err(retryable_provider_error(
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
    connect_timeout: Duration,
    security_receiver: mpsc::Receiver<bool>,
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
            Ok(Err(_)) => Err(retryable_provider_error(
                ErrorCategory::Protocol,
                "provider response failed",
            )),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                check_live(context, deadline)?;
                if self.first_byte && self.started.elapsed() >= self.connect_timeout {
                    return Err(retryable_provider_error(
                        ErrorCategory::Protocol,
                        "provider is unreachable",
                    ));
                }
                Ok(Some(Vec::new()))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => loop {
                check_live(context, deadline)?;
                match self
                    .security_receiver
                    .recv_timeout(quantum_for(deadline).max(Duration::from_millis(1)))
                {
                    Ok(true) => {
                        return Err(provider_error(
                            ErrorCategory::Protocol,
                            "TLS verification failed",
                        ));
                    }
                    Ok(false) => return Ok(None),
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(_) => {
                        return Err(provider_error(
                            ErrorCategory::Protocol,
                            "TLS helper diagnostics failed",
                        ));
                    }
                }
            },
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
    protocol: ProtocolKind,
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
                return vec![retryable_provider_error(
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
        return vec![status_error_with_head(status, &head)];
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
        if let Some(length) = head.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        }) && body.len() < length
        {
            return vec![retryable_provider_error(
                ErrorCategory::Protocol,
                "provider response ended early",
            )];
        }
        let body = decode_body(&head, &body);
        let events = events_for_protocol(status, &head, &body, protocol);
        if matches!(events.as_slice(), [ProviderEvent::Failed(error)] if error.category() == ErrorCategory::Protocol)
            && serde_json::from_slice::<Value>(&body).is_err_and(|error| error.is_eof())
        {
            return vec![retryable_provider_error(
                ErrorCategory::Protocol,
                "provider response ended early",
            )];
        }
        return events;
    }
    let chunked = head.to_ascii_lowercase().contains("chunked");
    if protocol != ProtocolKind::ChatCompletions {
        let mut live = ProtocolSseLive::new(chunked, protocol);
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
                    return vec![retryable_provider_error(
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
                return vec![retryable_provider_error(
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
    /// Thinking trace to echo on later turns. Withheld from the provisional
    /// sink: it is recorded for the batch only, never presented.
    reasoning: String,
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
    if !acc.reason.is_empty() {
        return Err(vec![provider_error(
            ErrorCategory::Protocol,
            "provider choice follows its finish reason",
        )]);
    }
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
    // Thinking-mode trace. Accumulated for echo on later turns; unlike
    // text it never reaches the provisional sink. Vendors disagree on the
    // field name (DeepSeek `reasoning_content`, Ollama `reasoning`), so
    // both feed the same accumulator.
    if let Some(trace) = delta
        .get("reasoning_content")
        .or_else(|| delta.get("reasoning"))
        .and_then(Value::as_str)
    {
        acc.reasoning.push_str(trace);
    }
    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let index = call.get("index").and_then(Value::as_u64).ok_or_else(|| {
                vec![provider_error(
                    ErrorCategory::Protocol,
                    "provider tool fragment is missing its index",
                )]
            })?;
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
                            "provider reply tool call arguments are invalid",
                        )]);
                    }
                }
            }
        }
    }
    Ok(applied)
}

/// Builds the authoritative batch from aggregated SSE state: at most one
/// text delta at `item-0`, the withheld thinking trace (when the vendor
/// sent one), calls numbered from `item-1`, then exactly one terminal.
fn finalize_sse(acc: &SseAccum) -> Vec<ProviderEvent> {
    if acc.text.len()
        + acc.reasoning.len()
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
    if !acc.reasoning.is_empty() {
        events.push(ProviderEvent::ReasoningDelta {
            text: acc.reasoning.clone(),
        });
    }
    let mut ready = Vec::new();
    for (position, (_, call)) in acc.fragments.iter().enumerate() {
        let item_key = format!("item-{}", position + 1);
        // Shape-only triage before identity validation: an empty id or name
        // means fragments never arrived, which is a different vendor shape
        // problem than a malformed identifier. No values are echoed.
        if call.id.is_empty() {
            return vec![provider_error(
                ErrorCategory::Protocol,
                "provider reply tool call is missing an id",
            )];
        }
        if call.name.is_empty() {
            return vec![provider_error(
                ErrorCategory::Protocol,
                "provider reply tool call is missing a name",
            )];
        }
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

#[derive(Default)]
struct ProtocolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Bounded event accumulator for the Responses and Messages SSE dialects.
/// Terminal events are authoritative; only text deltas reach the provisional
/// sink before the completed batch validates.
struct ProtocolSseLive {
    protocol: ProtocolKind,
    dechunker: Option<Dechunker>,
    buf: Vec<u8>,
    raw_bytes: usize,
    records: usize,
    calls: BTreeMap<u64, ProtocolCall>,
    blocks: BTreeMap<u64, Value>,
    open_blocks: BTreeSet<u64>,
    response_text: String,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    stop_reason: Option<String>,
}

impl ProtocolSseLive {
    fn new(chunked: bool, protocol: ProtocolKind) -> Self {
        Self {
            protocol,
            dechunker: chunked.then(Dechunker::new),
            buf: Vec::new(),
            raw_bytes: 0,
            records: 0,
            calls: BTreeMap::new(),
            blocks: BTreeMap::new(),
            open_blocks: BTreeSet::new(),
            response_text: String::new(),
            input_tokens: None,
            output_tokens: None,
            stop_reason: None,
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
        if self.raw_bytes > MAX_STREAM_WIRE_BYTES {
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
        if self.buf.len() > MAX_SSE_RECORD_BYTES {
            return Err(too_large());
        }
        while let Some(line) = take_line(&mut self.buf) {
            let line = String::from_utf8_lossy(&line);
            let Some(payload) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            self.records = self.records.saturating_add(1);
            if self.records > MAX_SSE_RECORDS {
                return Err(vec![provider_error(
                    ErrorCategory::ResourceLimit,
                    "provider response has too many stream records",
                )]);
            }
            let event: Value = serde_json::from_str(payload).map_err(|_| {
                vec![provider_error(
                    ErrorCategory::Protocol,
                    "provider reply is not valid JSON",
                )]
            })?;
            match self.protocol {
                ProtocolKind::OpenAiResponses => {
                    let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
                    match kind {
                        "response.output_text.delta" => {
                            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                                self.response_text.push_str(delta);
                                sink(ProviderEvent::TextDelta {
                                    item_key: "item-0".to_owned(),
                                    text: delta.to_owned(),
                                });
                            }
                        }
                        "response.output_item.added" => {
                            let index = event.get("output_index").and_then(Value::as_u64);
                            let item = event.get("item");
                            if item.is_some_and(|item| {
                                item.get("type").and_then(Value::as_str) == Some("reasoning")
                            }) {
                                return Err(vec![provider_error(
                                    ErrorCategory::UnsupportedCapability,
                                    "Responses reasoning items cannot be replayed faithfully",
                                )]);
                            }
                            if let (Some(index), Some(item)) = (index, item)
                                && item.get("type").and_then(Value::as_str) == Some("function_call")
                            {
                                let call = self.calls.entry(index).or_default();
                                call.id = item
                                    .get("call_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_owned();
                                call.name = item
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_owned();
                                call.arguments = item
                                    .get("arguments")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_owned();
                                if self.calls.len() > MAX_SSE_TOOL_IDENTITIES {
                                    return Err(vec![provider_error(
                                        ErrorCategory::ResourceLimit,
                                        "provider response has too many tool identities",
                                    )]);
                                }
                            }
                        }
                        "response.function_call_arguments.delta" => {
                            let index = event.get("output_index").and_then(Value::as_u64);
                            if let Some(index) = index {
                                let call = self.calls.entry(index).or_default();
                                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                                    call.arguments.push_str(delta);
                                }
                                if self.calls.len() > MAX_SSE_TOOL_IDENTITIES {
                                    return Err(vec![provider_error(
                                        ErrorCategory::ResourceLimit,
                                        "provider response has too many tool identities",
                                    )]);
                                }
                            }
                        }
                        kind if kind.contains("reasoning") => {
                            return Err(vec![provider_error(
                                ErrorCategory::UnsupportedCapability,
                                "Responses reasoning items cannot be replayed faithfully",
                            )]);
                        }
                        "response.completed" | "response.incomplete" => {
                            let response = event.get("response").unwrap_or(&event);
                            return Ok(Some(events_for_responses(response)));
                        }
                        "response.failed" | "error" => {
                            return Ok(Some(vec![provider_error(
                                ErrorCategory::Protocol,
                                "provider response failed",
                            )]));
                        }
                        _ => {}
                    }
                }
                ProtocolKind::AnthropicMessages => {
                    let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
                    match kind {
                        "message_start" => {
                            self.input_tokens = event
                                .pointer("/message/usage/input_tokens")
                                .and_then(Value::as_u64);
                        }
                        "content_block_start" => {
                            let (Some(index), Some(block)) = (
                                event.get("index").and_then(Value::as_u64),
                                event.get("content_block").filter(|block| block.is_object()),
                            ) else {
                                return Err(vec![provider_error(
                                    ErrorCategory::Protocol,
                                    "provider content block start is invalid",
                                )]);
                            };
                            if self.blocks.contains_key(&index) {
                                return Err(vec![provider_error(
                                    ErrorCategory::Protocol,
                                    "provider content block index is repeated",
                                )]);
                            }
                            let block = block.clone();
                            if matches!(
                                block.get("type").and_then(Value::as_str),
                                Some("thinking" | "redacted_thinking")
                            ) {
                                return Err(vec![provider_error(
                                    ErrorCategory::UnsupportedCapability,
                                    "Messages thinking blocks cannot be replayed faithfully",
                                )]);
                            }
                            self.open_blocks.insert(index);
                            if block.get("type").and_then(Value::as_str) == Some("text") {
                                self.blocks.insert(index, json!({"type":"text","text":""}));
                            } else if block.get("type").and_then(Value::as_str) == Some("tool_use")
                            {
                                let call = self.calls.entry(index).or_default();
                                call.id = block
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_owned();
                                call.name = block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_owned();
                                self.blocks.insert(index, block);
                            } else {
                                self.blocks.insert(index, block);
                            }
                        }
                        "content_block_delta" => {
                            let (Some(index), Some(delta)) = (
                                event.get("index").and_then(Value::as_u64),
                                event.get("delta").filter(|delta| delta.is_object()),
                            ) else {
                                return Err(vec![provider_error(
                                    ErrorCategory::Protocol,
                                    "provider content block delta is invalid",
                                )]);
                            };
                            if !self.open_blocks.contains(&index) {
                                return Err(vec![provider_error(
                                    ErrorCategory::Protocol,
                                    "provider content block delta has no open block",
                                )]);
                            }
                            let invalid_delta = || {
                                vec![provider_error(
                                    ErrorCategory::Protocol,
                                    "provider content block delta does not match its block or has invalid fields",
                                )]
                            };
                            let block = self.blocks.get_mut(&index).ok_or_else(invalid_delta)?;
                            match (
                                block.get("type").and_then(Value::as_str),
                                delta.get("type").and_then(Value::as_str),
                            ) {
                                (Some("text"), Some("text_delta")) => {
                                    let text = delta
                                        .get("text")
                                        .and_then(Value::as_str)
                                        .ok_or_else(invalid_delta)?;
                                    let existing =
                                        block.get("text").and_then(Value::as_str).unwrap_or("");
                                    block["text"] = Value::String(format!("{existing}{text}"));
                                    self.response_text.push_str(text);
                                    sink(ProviderEvent::TextDelta {
                                        item_key: "item-0".to_owned(),
                                        text: text.to_owned(),
                                    });
                                }
                                (Some("tool_use"), Some("input_json_delta")) => {
                                    let text = delta
                                        .get("partial_json")
                                        .and_then(Value::as_str)
                                        .ok_or_else(invalid_delta)?;
                                    self.calls
                                        .get_mut(&index)
                                        .ok_or_else(invalid_delta)?
                                        .arguments
                                        .push_str(text);
                                }
                                _ => return Err(invalid_delta()),
                            }
                        }
                        "content_block_stop" => {
                            let Some(index) = event.get("index").and_then(Value::as_u64) else {
                                return Err(vec![provider_error(
                                    ErrorCategory::Protocol,
                                    "provider content block stop is invalid",
                                )]);
                            };
                            if !self.open_blocks.remove(&index) {
                                return Err(vec![provider_error(
                                    ErrorCategory::Protocol,
                                    "provider content block stop has no open block",
                                )]);
                            }
                        }
                        "message_delta" => {
                            self.stop_reason = event
                                .pointer("/delta/stop_reason")
                                .and_then(Value::as_str)
                                .map(str::to_owned);
                            self.output_tokens = event
                                .pointer("/usage/output_tokens")
                                .and_then(Value::as_u64);
                        }
                        "message_stop" => {
                            if !self.open_blocks.is_empty() {
                                return Err(vec![provider_error(
                                    ErrorCategory::Protocol,
                                    "provider message ended with open content blocks",
                                )]);
                            }
                            return Ok(Some(self.finish_anthropic()));
                        }
                        "error" => {
                            return Ok(Some(vec![provider_error(
                                ErrorCategory::Protocol,
                                "provider response failed",
                            )]));
                        }
                        _ => {}
                    }
                    if self.calls.len() > MAX_SSE_TOOL_IDENTITIES {
                        return Err(vec![provider_error(
                            ErrorCategory::ResourceLimit,
                            "provider response has too many tool identities",
                        )]);
                    }
                }
                ProtocolKind::ChatCompletions => unreachable!(),
            }
            let content_bytes = self.response_text.len().saturating_add(
                self.calls
                    .values()
                    .map(|call| call.arguments.len())
                    .sum::<usize>(),
            );
            if content_bytes > MAX_RESPONSE_BYTES {
                return Err(too_large());
            }
        }
        Ok(None)
    }

    fn finish_anthropic(&self) -> Vec<ProviderEvent> {
        let mut content = Vec::new();
        for (index, mut block) in self.blocks.clone() {
            if block.get("type").and_then(Value::as_str) == Some("tool_use")
                && let Some(call) = self.calls.get(&index)
                && !call.arguments.is_empty()
            {
                let Ok(input) = serde_json::from_str::<Value>(&call.arguments) else {
                    return vec![provider_error(
                        ErrorCategory::Protocol,
                        "provider tool arguments are invalid",
                    )];
                };
                block["input"] = input;
            }
            content.push(block);
        }
        events_for_anthropic(&json!({
            "type":"message",
            "content":content,
            "stop_reason":self.stop_reason,
            "usage":{"input_tokens":self.input_tokens,"output_tokens":self.output_tokens},
        }))
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
    records: usize,
}

impl SseLive {
    fn new(chunked: bool) -> Self {
        Self {
            dechunker: chunked.then(Dechunker::new),
            buf: Vec::new(),
            acc: SseAccum::default(),
            last_usage: (None, None),
            raw_bytes: 0,
            records: 0,
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
        if self.raw_bytes > MAX_STREAM_WIRE_BYTES {
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
        if self.buf.len() > MAX_SSE_RECORD_BYTES {
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
            self.records = self.records.saturating_add(1);
            if self.records > MAX_SSE_RECORDS {
                return Err(vec![provider_error(
                    ErrorCategory::ResourceLimit,
                    "provider response has too many stream records",
                )]);
            }
            if let Ok(chunk) = serde_json::from_str::<Value>(payload)
                && let Some(calls) = chunk
                    .get("choices")
                    .and_then(Value::as_array)
                    .and_then(|choices| choices.first())
                    .and_then(|choice| choice.get("delta"))
                    .and_then(|delta| delta.get("tool_calls"))
                    .and_then(Value::as_array)
            {
                let mut new_indices = HashSet::new();
                for call in calls {
                    let Some(index) = call.get("index").and_then(Value::as_u64) else {
                        continue;
                    };
                    if !self.acc.fragments.contains_key(&index) {
                        new_indices.insert(index);
                    }
                    if self.acc.fragments.len().saturating_add(new_indices.len())
                        > MAX_SSE_TOOL_IDENTITIES
                    {
                        return Err(vec![provider_error(
                            ErrorCategory::ResourceLimit,
                            "provider response has too many tool identities",
                        )]);
                    }
                }
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
                + self.acc.reasoning.len()
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
                    if out.len().saturating_add(size) > MAX_STREAM_WIRE_BYTES {
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
    dechunk_bounded(body, MAX_RESPONSE_BYTES)
}

fn dechunk_bounded(body: &[u8], max_bytes: usize) -> Option<Vec<u8>> {
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
        if out.len().saturating_add(size) > max_bytes {
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

/// Groups one turn's assistant items into a single assistant message.
/// The runtime records text and calls as separate items, but vendors
/// validate pairing per message: an assistant message carrying
/// `tool_calls` must be followed by the tool responses for those ids
/// before any other assistant message. Emitting one call per message
/// (`assistant, assistant, tool, tool`) fails that check as soon as a
/// turn carries more than one call; merging keeps the standard shape
/// (`assistant, tool, tool`) with identical content. Only consecutive
/// assistant items merge, so tool results (and turn boundaries they mark)
/// are never crossed. Text fragments join with a newline; the first
/// thinking trace wins (a turn carries at most one).
fn conversation_messages(conversation: &[ModelContextItem]) -> Vec<Value> {
    let mut messages = Vec::new();
    let mut pending: Vec<Value> = Vec::new();
    for item in conversation {
        match item {
            ModelContextItem::AssistantText { .. }
            | ModelContextItem::AssistantCall { .. }
            | ModelContextItem::AssistantDeniedCall { .. }
            | ModelContextItem::AssistantReasoning { .. } => {
                pending.push(message_json(item));
            }
            _ => {
                if let Some(merged) = merge_assistant(std::mem::take(&mut pending)) {
                    messages.push(merged);
                }
                messages.push(message_json(item));
            }
        }
    }
    if let Some(merged) = merge_assistant(pending) {
        messages.push(merged);
    }
    messages
}

/// Validate pairing before opening a connection, including host identities
/// that are intentionally absent from the vendor wire representation.
fn validate_conversation(conversation: &[ModelContextItem]) -> Result<(), ProviderEvent> {
    let invalid = || {
        provider_error(
            ErrorCategory::Protocol,
            "model conversation has invalid tool response ordering",
        )
    };
    let mut pending = BTreeMap::new();
    let mut responding = false;
    let mut trace = None;
    for item in conversation {
        match item {
            ModelContextItem::UserText(_) | ModelContextItem::ToolResult { .. } => trace = None,
            _ => {
                if let Some(next) = item.reasoning() {
                    if trace.is_some_and(|previous| previous != next) {
                        return Err(provider_error(
                            ErrorCategory::Protocol,
                            "model assistant group has conflicting reasoning traces",
                        ));
                    }
                    trace = Some(next);
                }
            }
        }
        match item {
            ModelContextItem::AssistantCall {
                item_key,
                provider_ref,
                call,
                ..
            } => {
                if responding
                    || pending
                        .insert(
                            provider_ref.as_str(),
                            (call.call(), item_key.as_str(), call.tool().name()),
                        )
                        .is_some()
                {
                    return Err(invalid());
                }
            }
            ModelContextItem::AssistantDeniedCall {
                call, candidate, ..
            } => {
                if responding
                    || pending
                        .insert(
                            candidate.provider_ref(),
                            (call, candidate.item_key(), candidate.tool_name()),
                        )
                        .is_some()
                {
                    return Err(invalid());
                }
            }
            ModelContextItem::ToolResult {
                call,
                item_key,
                provider_ref,
                tool,
                ..
            } => {
                if pending.remove(provider_ref.as_str())
                    != Some((call, item_key.as_str(), tool.name()))
                {
                    return Err(invalid());
                }
                responding = !pending.is_empty();
            }
            ModelContextItem::UserText(_) => {
                if !pending.is_empty() {
                    return Err(invalid());
                }
                responding = false;
            }
            ModelContextItem::AssistantText { .. }
            | ModelContextItem::AssistantReasoning { .. } => {
                if responding {
                    return Err(invalid());
                }
                responding = false;
            }
        }
    }
    if !pending.is_empty() {
        return Err(invalid());
    }
    Ok(())
}

/// Merges rendered assistant messages into one: concatenated text content
/// (null when no text), concatenated `tool_calls` in order, first thinking
/// trace. A single message renders byte-identical to its input.
fn merge_assistant(rendered: Vec<Value>) -> Option<Value> {
    if rendered.is_empty() {
        return None;
    }
    if rendered.len() == 1 {
        return rendered.into_iter().next();
    }
    let mut texts = Vec::new();
    let mut calls = Vec::new();
    let mut reasoning = None;
    for message in &rendered {
        if let Some(Value::String(text)) = message.get("content") {
            texts.push(text.clone());
        }
        if let Some(chunk) = message.get("tool_calls").and_then(Value::as_array) {
            calls.extend(chunk.iter().cloned());
        }
        if reasoning.is_none() {
            reasoning = message
                .get("reasoning_content")
                .or_else(|| message.get("reasoning"))
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
    }
    let mut merged = serde_json::Map::new();
    merged.insert("role".to_owned(), Value::String("assistant".to_owned()));
    merged.insert(
        "content".to_owned(),
        if texts.is_empty() {
            Value::Null
        } else {
            Value::String(texts.join("\n"))
        },
    );
    if let Some(trace) = reasoning {
        merged.insert("reasoning_content".to_owned(), Value::String(trace));
    }
    if !calls.is_empty() {
        merged.insert("tool_calls".to_owned(), Value::Array(calls));
    }
    Some(Value::Object(merged))
}

/// Encodes one conversation item as a Chat Completions message. Provider
/// round-trip refs ride the tool-call ids; text item keys stay local.
/// Turn-level grouping happens in [`conversation_messages`]; this renders
/// single items (and the per-item unit tests pin these shapes).
fn message_json(item: &ModelContextItem) -> Value {
    match item {
        ModelContextItem::AssistantReasoning { reasoning } => {
            json!({"role":"assistant","content":Value::Null,"reasoning_content":reasoning})
        }
        ModelContextItem::AssistantDeniedCall {
            candidate,
            reasoning,
            ..
        } => {
            let mut message = json!({"role":"assistant", "content":Value::Null,
                "tool_calls":[{"id":candidate.provider_ref(),"type":"function",
                    "function":{"name":candidate.tool_name(),"arguments":candidate.arguments_json()}}]});
            if let Some(trace) = reasoning {
                message["reasoning_content"] = Value::String(trace.clone());
            }
            message
        }
        ModelContextItem::UserText(text) => {
            json!({ "role": "user", "content": text.as_str() })
        }
        ModelContextItem::AssistantText {
            text, reasoning, ..
        } => {
            let mut message = json!({ "role": "assistant", "content": text.as_str() });
            if let Some(trace) = reasoning {
                message["reasoning_content"] = Value::String(trace.clone());
            }
            message
        }
        ModelContextItem::AssistantCall {
            provider_ref,
            call,
            reasoning,
            ..
        } => {
            let mut message = json!({
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
            });
            if let Some(trace) = reasoning {
                message["reasoning_content"] = Value::String(trace.clone());
            }
            message
        }
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
        return vec![status_error(status)];
    }
    if is_sse(head, body) {
        return events_for_sse(body);
    }
    events_for_json(body)
}

fn events_for_protocol(
    status: u16,
    head: &str,
    body: &[u8],
    protocol: ProtocolKind,
) -> Vec<ProviderEvent> {
    if protocol == ProtocolKind::ChatCompletions {
        return events_for(status, head, body);
    }
    if !(200..300).contains(&status) {
        return vec![status_error(status)];
    }
    if is_sse(head, body) {
        return vec![provider_error(
            ErrorCategory::Protocol,
            "provider stream parser is not available for this protocol response",
        )];
    }
    let value: Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => {
            return vec![provider_error(
                ErrorCategory::Protocol,
                "provider reply is not valid JSON",
            )];
        }
    };
    match protocol {
        ProtocolKind::OpenAiResponses => events_for_responses(&value),
        ProtocolKind::AnthropicMessages => events_for_anthropic(&value),
        ProtocolKind::ChatCompletions => unreachable!(),
    }
}

fn events_for_responses(reply: &Value) -> Vec<ProviderEvent> {
    if reply.get("status").and_then(Value::as_str) == Some("failed") {
        return vec![provider_error(
            ErrorCategory::Protocol,
            "provider response failed",
        )];
    }
    let mut events = Vec::new();
    let mut call_count = 0usize;
    let mut refusal = false;
    let Some(output) = reply.get("output").and_then(Value::as_array) else {
        return vec![provider_error(
            ErrorCategory::Protocol,
            "provider reply has no output",
        )];
    };
    let status = reply.get("status").and_then(Value::as_str);
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => {
                return vec![provider_error(
                    ErrorCategory::UnsupportedCapability,
                    "Responses reasoning items cannot be replayed faithfully",
                )];
            }
            Some("message") => {
                if let Some(parts) = item.get("content").and_then(Value::as_array) {
                    for part in parts {
                        if part.get("type").and_then(Value::as_str) == Some("reasoning") {
                            return vec![provider_error(
                                ErrorCategory::UnsupportedCapability,
                                "Responses reasoning items cannot be replayed faithfully",
                            )];
                        }
                        refusal |= part.get("type").and_then(Value::as_str) == Some("refusal");
                        if let Some(text) = part.get("text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            events.push(ProviderEvent::TextDelta {
                                item_key: "item-0".to_owned(),
                                text: text.to_owned(),
                            });
                        }
                    }
                }
            }
            Some("function_call") => {
                // An incomplete response may contain a partial tool proposal.
                // Never expose it as dispatchable work.
                if status != Some("completed") {
                    continue;
                }
                if call_count >= MAX_SSE_TOOL_IDENTITIES {
                    return vec![provider_error(
                        ErrorCategory::ResourceLimit,
                        "provider response has too many tool identities",
                    )];
                }
                let Some(id) = item.get("call_id").and_then(Value::as_str) else {
                    return vec![provider_error(
                        ErrorCategory::Protocol,
                        "provider call identity is invalid",
                    )];
                };
                let Some(name) = item.get("name").and_then(Value::as_str) else {
                    return vec![provider_error(
                        ErrorCategory::Protocol,
                        "provider call identity is invalid",
                    )];
                };
                let Some(arguments) = item.get("arguments").and_then(Value::as_str) else {
                    return vec![provider_error(
                        ErrorCategory::Protocol,
                        "provider call arguments are invalid",
                    )];
                };
                let candidate = match CallCandidate::new(
                    format!("item-{}", call_count + 1),
                    id,
                    name,
                    arguments,
                ) {
                    Ok(candidate) => candidate,
                    Err(_) => {
                        return vec![provider_error(
                            ErrorCategory::Protocol,
                            "provider call identity is invalid",
                        )];
                    }
                };
                call_count += 1;
                events.push(ProviderEvent::ToolCallReady(candidate));
            }
            _ => {}
        }
    }
    let (input, output) = reply
        .get("usage")
        .map(|usage| {
            (
                usage.get("input_tokens").and_then(Value::as_u64),
                usage.get("output_tokens").and_then(Value::as_u64),
            )
        })
        .unwrap_or((None, None));
    let reason = if status == Some("incomplete") {
        if reply
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
            == Some("max_output_tokens")
        {
            FinishReason::OutputLimit
        } else {
            FinishReason::Incomplete
        }
    } else if status != Some("completed") {
        FinishReason::Incomplete
    } else if call_count > 0 {
        FinishReason::ToolCalls
    } else if refusal {
        FinishReason::Refusal
    } else {
        FinishReason::Stop
    };
    events.push(ProviderEvent::TurnFinished(TurnFinished::new(
        reason,
        Usage::new(input, output, UsageFinality::Final),
        None,
    )));
    events
}

fn events_for_anthropic(reply: &Value) -> Vec<ProviderEvent> {
    if reply.get("type").and_then(Value::as_str) != Some("message") {
        return vec![provider_error(
            ErrorCategory::Protocol,
            "provider reply has no message",
        )];
    }
    let mut events = Vec::new();
    let mut call_count = 0usize;
    if let Some(content) = reply.get("content").and_then(Value::as_array) {
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str)
                        && !text.is_empty()
                    {
                        events.push(ProviderEvent::TextDelta {
                            item_key: "item-0".to_owned(),
                            text: text.to_owned(),
                        });
                    }
                }
                Some("thinking") | Some("redacted_thinking") => {
                    return vec![provider_error(
                        ErrorCategory::UnsupportedCapability,
                        "Messages thinking blocks cannot be replayed faithfully",
                    )];
                }
                Some("tool_use") => {
                    if call_count >= MAX_SSE_TOOL_IDENTITIES {
                        return vec![provider_error(
                            ErrorCategory::ResourceLimit,
                            "provider response has too many tool identities",
                        )];
                    }
                    let (Some(id), Some(name), Some(input)) = (
                        block.get("id").and_then(Value::as_str),
                        block.get("name").and_then(Value::as_str),
                        block.get("input"),
                    ) else {
                        return vec![provider_error(
                            ErrorCategory::Protocol,
                            "provider call is invalid",
                        )];
                    };
                    let arguments = input.to_string();
                    let candidate = match CallCandidate::new(
                        format!("item-{}", call_count + 1),
                        id,
                        name,
                        arguments,
                    ) {
                        Ok(candidate) => candidate,
                        Err(_) => {
                            return vec![provider_error(
                                ErrorCategory::Protocol,
                                "provider call identity is invalid",
                            )];
                        }
                    };
                    call_count += 1;
                    events.push(ProviderEvent::ToolCallReady(candidate));
                }
                _ => {}
            }
        }
    }
    let usage = reply.get("usage");
    let input = usage
        .and_then(|usage| usage.get("input_tokens"))
        .and_then(Value::as_u64);
    let output = usage
        .and_then(|usage| usage.get("output_tokens"))
        .and_then(Value::as_u64);
    let reason = match reply.get("stop_reason").and_then(Value::as_str) {
        Some("end_turn") | Some("stop_sequence") => FinishReason::Stop,
        Some("tool_use") if call_count > 0 => FinishReason::ToolCalls,
        Some("tool_use") => {
            return vec![provider_error(
                ErrorCategory::Protocol,
                "provider claimed tool use without items",
            )];
        }
        Some("max_tokens") => FinishReason::OutputLimit,
        Some("refusal") => FinishReason::Refusal,
        Some(_) => FinishReason::Incomplete,
        None => {
            return vec![provider_error(
                ErrorCategory::Protocol,
                "provider reply is incomplete",
            )];
        }
    };
    events.push(ProviderEvent::TurnFinished(TurnFinished::new(
        reason,
        Usage::new(input, output, UsageFinality::Final),
        None,
    )));
    events
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
    if let Some(trace) = message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
        .and_then(Value::as_str)
        && !trace.is_empty()
    {
        events.push(ProviderEvent::ReasoningDelta {
            text: trace.to_owned(),
        });
    }
    let calls: Vec<(String, String, String)> = match message.get("tool_calls") {
        None => Vec::new(),
        Some(Value::Array(calls)) => {
            let mut parsed = Vec::with_capacity(calls.len());
            for call in calls {
                // Each check reports which shape failed so a vendor deviation
                // points at its field. Diagnostics are static: no values,
                // keys, or fragments are echoed.
                let Some(id) = call.get("id").and_then(Value::as_str) else {
                    return vec![provider_error(
                        ErrorCategory::InvalidInput,
                        "provider reply tool call is missing an id",
                    )];
                };
                let Some(function) = call.get("function") else {
                    return vec![provider_error(
                        ErrorCategory::InvalidInput,
                        "provider reply tool call is missing a function",
                    )];
                };
                let Some(name) = function.get("name").and_then(Value::as_str) else {
                    return vec![provider_error(
                        ErrorCategory::InvalidInput,
                        "provider reply tool call name is invalid",
                    )];
                };
                let Some(arguments) = function.get("arguments").and_then(Value::as_str) else {
                    return vec![provider_error(
                        ErrorCategory::InvalidInput,
                        "provider reply tool call arguments are invalid",
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
    fn context_management(&self) -> nexus_core::ContextManagementSettings {
        self.context_management
    }

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

    fn adapter_identity(&self) -> &str {
        match self.protocol {
            ProtocolKind::ChatCompletions => "openai-chat-completions-v1",
            ProtocolKind::OpenAiResponses => "openai-responses-v1",
            ProtocolKind::AnthropicMessages => "anthropic-messages-v1",
        }
    }

    fn continuation_scope(&self, profile: &str) -> String {
        profile.to_owned()
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
        self.stream_with_sink_impl(&body, request, context, sink)
    }
}

#[cfg(test)]
mod opencode_go_tests {
    use super::*;
    use nexus_core::{RunId, SessionId, TurnId};

    fn request(session: Option<&str>, run: &str) -> ModelRequest {
        let request = ModelRequest::new(
            RunId::new(run).unwrap(),
            TurnId::new("turn").unwrap(),
            "profile",
            Vec::new(),
            None,
            1024,
        )
        .unwrap();
        match session {
            Some(id) => request.with_session(SessionId::new(id).unwrap()),
            None => request,
        }
    }

    fn provider(endpoint: &str, protocol: ProtocolKind) -> OpenAiProvider {
        OpenAiProvider::new_for_protocol(
            endpoint,
            CredentialRef::env_var("TEST_OPENCODE_KEY").unwrap(),
            "model",
            protocol,
        )
        .unwrap()
    }

    #[test]
    fn headers_are_stable_per_session_and_require_identity() {
        let provider = provider(
            "https://opencode.ai/inference/go/openai/v1",
            ProtocolKind::ChatCompletions,
        );
        let first = provider
            .opencode_go_headers(&request(Some("session-a"), "run-a"))
            .unwrap();
        let next = provider
            .opencode_go_headers(&request(Some("session-a"), "run-b"))
            .unwrap();
        let other = provider
            .opencode_go_headers(&request(Some("session-b"), "run-c"))
            .unwrap();
        assert_eq!(first, next);
        assert!(first.contains("x-opencode-session: session-a\r\n"));
        assert!(first.contains(concat!(
            "user-agent: nexus-agent/",
            env!("CARGO_PKG_VERSION"),
            "\r\n"
        )));
        assert_ne!(first, other);
        assert!(
            provider
                .opencode_go_headers(&request(None, "run-d"))
                .is_err()
        );
    }

    #[test]
    fn headers_never_reach_other_endpoints_or_protocols() {
        for endpoint in [
            "http://opencode.ai/inference/go/openai/v1",
            "https://opencode.ai:444/inference/go/openai/v1",
            "https://opencode.ai.evil.test/inference/go/openai/v1",
            "https://opencode.ai/inference/openai/v1",
            "https://opencode.ai/inference/go/openai/v1/other",
        ] {
            assert_eq!(
                provider(endpoint, ProtocolKind::ChatCompletions)
                    .opencode_go_headers(&request(Some("session-a"), "run"))
                    .unwrap(),
                ""
            );
        }
        assert_eq!(
            provider(
                "https://opencode.ai/inference/go/openai/v1",
                ProtocolKind::OpenAiResponses
            )
            .opencode_go_headers(&request(Some("session-a"), "run"))
            .unwrap(),
            ""
        );
    }
}

#[cfg(test)]
mod reasoning_effort_tests {
    use super::OpenAiProvider;
    use nexus_config::CredentialRef;
    use nexus_core::{ModelContextItem, ModelRequest, ReasoningEffort, RunId, TurnId};

    fn request(with_effort: bool) -> ModelRequest {
        let request = ModelRequest::new(
            RunId::new("reasoning-effort-run").unwrap(),
            TurnId::new("reasoning-effort-turn").unwrap(),
            "summary",
            Vec::new(),
            None,
            8192,
        )
        .unwrap()
        .with_conversation(vec![
            ModelContextItem::user_text("Summarize history").unwrap(),
        ])
        .unwrap();
        if with_effort {
            request.with_reasoning_effort(ReasoningEffort::Low)
        } else {
            request
        }
    }

    #[test]
    fn reasoning_effort_is_sent_only_to_ollama_chat_completions() {
        let credential = CredentialRef::env_var("TEST_TOKEN").unwrap();
        let ollama = OpenAiProvider::new(
            "http://ollama:11434/v1",
            credential.clone(),
            "qwen3.8-27b-iq3m-92k:latest",
        )
        .unwrap();
        let body = ollama.request_body(&request(true)).unwrap();
        assert_eq!(body["reasoning_effort"], "low");

        let ordinary = OpenAiProvider::new(
            "http://127.0.0.1:43210/v1",
            credential,
            "generic-compatible-model",
        )
        .unwrap();
        let body = ordinary.request_body(&request(true)).unwrap();
        assert!(body.get("reasoning_effort").is_none());
        let body = ollama.request_body(&request(false)).unwrap();
        assert!(body.get("reasoning_effort").is_none());
    }
}

#[cfg(test)]
mod retry_policy_tests {
    use super::*;
    use nexus_core::CancellationToken;

    #[test]
    fn custom_settings_select_timeouts_without_overriding_a_run_deadline() {
        let settings = ProviderSettings {
            connect_timeout_seconds: 7,
            context_discovery_timeout_seconds: 11,
            fallback_call_timeout_seconds: 19,
            retry_backoff_seconds: vec![3, 9],
        };
        let provider = OpenAiProvider::new(
            "http://localhost/v1",
            CredentialRef::env_var("NEXUS_TEST_UNUSED_TOKEN").unwrap(),
            "model",
        )
        .unwrap()
        .with_operational_settings(&settings)
        .unwrap();
        assert_eq!(provider.operational_settings, settings);
        let now = Instant::now();
        assert_eq!(
            call_deadline(None, &settings, now),
            now + Duration::from_secs(19)
        );
        for seconds in [2, 30] {
            let deadline = now + Duration::from_secs(seconds);
            assert_eq!(call_deadline(Some(deadline), &settings, now), deadline);
            assert_eq!(
                connect_timeout_for(Duration::from_secs(7), Some(deadline), now),
                Duration::from_secs(seconds.min(7))
            );
        }
        assert_eq!(
            connect_timeout_for(Duration::from_secs(7), None, now),
            Duration::from_secs(7)
        );
        assert_eq!(
            connect_timeout_for(Duration::from_secs(7), Some(now), now),
            Duration::ZERO
        );
    }

    #[test]
    fn custom_retry_schedule_is_used_and_last_delay_repeats() {
        let settings = ProviderSettings {
            retry_backoff_seconds: vec![3, 9],
            fallback_call_timeout_seconds: 17,
            ..ProviderSettings::default()
        };
        let context = ProviderContext::new(Duration::ZERO, false, None);
        let mut attempts = 0;
        let mut waits = Vec::new();
        let events = retry_exchange(
            &settings,
            &context,
            None,
            &|_| {},
            |_| {
                attempts += 1;
                if attempts <= 4 {
                    vec![status_error(503)]
                } else {
                    Vec::new()
                }
            },
            |_, _, delay| {
                waits.push(delay);
                Ok(())
            },
        );
        assert!(events.is_empty());
        assert_eq!(attempts, 5);
        assert_eq!(waits, [3, 9, 9, 9].map(Duration::from_secs));
        assert_eq!(retry_delay(&settings, usize::MAX), Duration::from_secs(9));
    }

    #[test]
    fn public_operational_settings_are_validated_defensively() {
        let defaults = ProviderSettings::default();
        assert_eq!(defaults.connect_timeout_seconds, 10);
        assert_eq!(defaults.context_discovery_timeout_seconds, 2);
        assert_eq!(defaults.fallback_call_timeout_seconds, 600);
        assert!(OpenAiProvider::validate_operational_settings(&defaults).is_ok());
        for settings in [
            ProviderSettings {
                connect_timeout_seconds: 0,
                ..defaults.clone()
            },
            ProviderSettings {
                connect_timeout_seconds: 86_401,
                ..defaults.clone()
            },
            ProviderSettings {
                context_discovery_timeout_seconds: 0,
                ..defaults.clone()
            },
            ProviderSettings {
                context_discovery_timeout_seconds: 86_401,
                ..defaults.clone()
            },
            ProviderSettings {
                fallback_call_timeout_seconds: 0,
                ..defaults.clone()
            },
            ProviderSettings {
                fallback_call_timeout_seconds: 86_401,
                ..defaults.clone()
            },
            ProviderSettings {
                retry_backoff_seconds: vec![],
                ..defaults.clone()
            },
            ProviderSettings {
                retry_backoff_seconds: vec![0],
                ..defaults.clone()
            },
            ProviderSettings {
                retry_backoff_seconds: vec![3_601],
                ..defaults.clone()
            },
            ProviderSettings {
                retry_backoff_seconds: vec![2, 1],
                ..defaults.clone()
            },
            ProviderSettings {
                retry_backoff_seconds: vec![1; 9],
                ..defaults.clone()
            },
        ] {
            assert!(OpenAiProvider::validate_operational_settings(&settings).is_err());
        }
    }

    #[test]
    fn retry_schedule_increases_then_stays_at_one_minute() {
        let settings = ProviderSettings::default();
        assert_eq!(
            (0..6)
                .map(|retry| retry_delay(&settings, retry))
                .collect::<Vec<_>>(),
            vec![
                std::time::Duration::from_secs(1),
                std::time::Duration::from_secs(2),
                std::time::Duration::from_secs(5),
                std::time::Duration::from_secs(10),
                std::time::Duration::from_secs(30),
                std::time::Duration::from_secs(60),
            ]
        );
        for index in [6, 7, 100, usize::MAX] {
            assert_eq!(retry_delay(&settings, index), Duration::from_secs(60));
        }
    }

    #[test]
    fn remote_access_refusals_retry_but_invalid_requests_do_not() {
        for status in [301, 401, 402, 403, 404, 408, 425, 429, 500, 503, 599] {
            assert_eq!(
                status_retry_guidance(status),
                RetryGuidance::RetryAfterBackoff,
                "status {status}"
            );
        }
        for status in [200, 400, 413, 422, 431, 600] {
            assert_eq!(
                status_retry_guidance(status),
                RetryGuidance::DoNotRetry,
                "status {status}"
            );
        }
        assert_eq!(status_class(429).0, ErrorCategory::RateLimited);
        assert_eq!(status_class(401).0, ErrorCategory::Authentication);
        assert_eq!(status_class(403).0, ErrorCategory::Authentication);
        assert_eq!(status_class(402).0, ErrorCategory::PermissionDenied);
        assert!(matches!(status_error(402), ProviderEvent::Failed(error)
            if error.category() == ErrorCategory::PermissionDenied
                && error.message() == "provider payment is required"
                && error.retry() == RetryGuidance::RetryAfterBackoff));
    }

    #[test]
    fn connection_refusal_is_retryable_before_a_request_is_sent() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let context = ProviderContext::new(Duration::ZERO, false, None);
        let error = connect_plain(
            "127.0.0.1",
            port,
            &context,
            Some(Instant::now() + Duration::from_secs(1)),
            Duration::from_secs(10),
        )
        .expect_err("closed port refuses connections");
        assert!(matches!(
            error,
            ProviderEvent::Failed(error) if error.retry() == RetryGuidance::RetryAfterBackoff
        ));
    }

    #[test]
    fn local_socket_failures_are_not_misclassified_as_remote_outages() {
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::OutOfMemory,
            std::io::ErrorKind::InvalidInput,
            std::io::ErrorKind::Other,
        ] {
            assert!(!retryable_connection_error(&std::io::Error::from(kind)));
        }
        assert!(retryable_connection_error(&std::io::Error::from(
            std::io::ErrorKind::ConnectionRefused
        )));
    }

    fn assert_category(events: &[ProviderEvent], category: ErrorCategory) {
        assert!(
            matches!(events, [ProviderEvent::Failed(error)] if error.category() == category),
            "{events:?}"
        );
    }

    #[test]
    fn success_after_more_than_six_failures_uses_capped_schedule() {
        let context = ProviderContext::new(Duration::ZERO, false, None);
        let mut attempts = 0;
        let mut waits = Vec::new();
        let events = retry_exchange(
            &ProviderSettings::default(),
            &context,
            None,
            &|_| {},
            |_| {
                attempts += 1;
                if attempts <= 10 {
                    vec![status_error(401)]
                } else {
                    Vec::new()
                }
            },
            |_, deadline, delay| {
                assert!(
                    deadline.is_some(),
                    "deadline-free callers receive a fallback bound"
                );
                waits.push(delay);
                Ok(())
            },
        );
        assert!(events.is_empty());
        assert_eq!(attempts, 11);
        assert_eq!(
            waits,
            (0..10)
                .map(|retry| retry_delay(&ProviderSettings::default(), retry))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn emitted_events_prevent_replay_even_for_retryable_failures() {
        let context = ProviderContext::new(Duration::ZERO, false, None);
        let mut attempts = 0;
        let events = retry_exchange(
            &ProviderSettings::default(),
            &context,
            None,
            &|_| {},
            |sink| {
                attempts += 1;
                sink(ProviderEvent::TextDelta {
                    item_key: "item-0".to_owned(),
                    text: "provisional".to_owned(),
                });
                vec![retryable_provider_error(
                    ErrorCategory::Protocol,
                    "provider response failed",
                )]
            },
            |_, _, _| panic!("must not retry after provisional delivery"),
        );
        assert_eq!(attempts, 1);
        assert_category(&events, ErrorCategory::Protocol);
    }

    #[test]
    fn cancellation_during_retry_wait_prevents_another_attempt() {
        let token = CancellationToken::new();
        let context = ProviderContext::new(Duration::ZERO, false, None)
            .with_control(token.clone(), Instant::now() + Duration::from_secs(10));
        let mut attempts = 0;
        let events = retry_exchange(
            &ProviderSettings::default(),
            &context,
            context.deadline(),
            &|_| {},
            |_| {
                attempts += 1;
                vec![status_error(503)]
            },
            |context, deadline, delay| {
                let token = token.clone();
                let canceller = std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(5));
                    token.cancel();
                });
                let result = wait_before_retry(context, deadline, delay);
                canceller.join().unwrap();
                result
            },
        );
        assert_eq!(attempts, 1);
        assert_category(&events, ErrorCategory::Cancelled);
    }

    #[test]
    fn deadline_interrupts_a_one_minute_wait() {
        let context = ProviderContext::new(Duration::ZERO, false, None);
        let deadline = Some(Instant::now() + Duration::from_millis(10));
        let started = Instant::now();
        let events = wait_before_retry(&context, deadline, Duration::from_secs(60)).unwrap_err();
        assert_category(&[events], ErrorCategory::Timeout);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cancellation_takes_priority_over_an_elapsed_deadline() {
        let context = ProviderContext::new(Duration::ZERO, true, None);
        let error =
            wait_before_retry(&context, Some(Instant::now()), Duration::from_secs(60)).unwrap_err();
        assert_category(&[error], ErrorCategory::Cancelled);
    }

    #[test]
    fn liveness_is_rechecked_after_wait_even_if_wait_returns_success() {
        let token = CancellationToken::new();
        let context = ProviderContext::new(Duration::ZERO, false, None)
            .with_control(token.clone(), Instant::now() + Duration::from_secs(10));
        let mut attempts = 0;
        let events = retry_exchange(
            &ProviderSettings::default(),
            &context,
            context.deadline(),
            &|_| {},
            |_| {
                attempts += 1;
                vec![status_error(503)]
            },
            |_, _, _| {
                token.cancel();
                Ok(())
            },
        );
        assert_eq!(attempts, 1);
        assert_category(&events, ErrorCategory::Cancelled);
    }

    #[test]
    fn retry_after_delta_seconds_is_a_minimum_not_an_attempt_limit() {
        let context = ProviderContext::new(Duration::ZERO, false, None);
        let mut attempts = 0;
        retry_exchange(
            &ProviderSettings::default(),
            &context,
            None,
            &|_| {},
            |_| {
                attempts += 1;
                if attempts == 1 {
                    vec![status_error_with_head(
                        429,
                        "HTTP/1.1 429 Test\r\nRetry-After: 120\r\n\r\n",
                    )]
                } else {
                    Vec::new()
                }
            },
            |_, _, delay| {
                assert_eq!(delay, Duration::from_secs(120));
                Ok(())
            },
        );
        assert_eq!(attempts, 2);
    }

    struct ScriptedSource(std::collections::VecDeque<Vec<u8>>);

    impl ChunkSource for ScriptedSource {
        fn next_chunk(
            &mut self,
            _: Duration,
            _: Option<Instant>,
            _: &ProviderContext,
        ) -> Result<Option<Vec<u8>>, ProviderEvent> {
            Ok(self.0.pop_front())
        }
    }

    #[test]
    fn premature_eof_before_delivery_retries_and_can_complete() {
        for first in [
            "",
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n",
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"choices\":",
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 100\r\n\r\n{}",
        ] {
            let context = ProviderContext::new(Duration::ZERO, false, None);
            let mut attempts = 0;
            let events = retry_exchange(
                &ProviderSettings::default(),
                &context,
                None,
                &|_| {},
                |sink| {
                    attempts += 1;
                    let response = if attempts == 1 {
                        first
                    } else {
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"recovered\"},\"finish_reason\":\"stop\"}]}"
                    };
                    let mut source = ScriptedSource([response.as_bytes().to_vec()].into());
                    stream_response(
                        &mut source,
                        None,
                        &context,
                        sink,
                        ProtocolKind::ChatCompletions,
                    )
                },
                |_, _, _| Ok(()),
            );
            assert_eq!(attempts, 2, "{first}");
            assert!(matches!(
                events.last(),
                Some(ProviderEvent::TurnFinished(_))
            ));
        }
    }

    #[test]
    fn message_start_without_provisional_delivery_does_not_block_retry() {
        let context = ProviderContext::new(Duration::ZERO, false, None);
        let mut attempts = 0;
        let events = retry_exchange(
            &ProviderSettings::default(),
            &context,
            None,
            &|_| panic!("metadata must not publish provisional output"),
            |sink| {
                attempts += 1;
                if attempts == 2 {
                    return vec![ProviderEvent::TurnFinished(TurnFinished::new(
                        FinishReason::Stop,
                        Usage::new(None, None, UsageFinality::Final),
                        None,
                    ))];
                }
                let response = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n";
                let mut source = ScriptedSource([response.to_vec()].into());
                stream_response(
                    &mut source,
                    None,
                    &context,
                    sink,
                    ProtocolKind::AnthropicMessages,
                )
            },
            |_, _, _| Ok(()),
        );
        assert_eq!(attempts, 2);
        assert!(matches!(
            events.last(),
            Some(ProviderEvent::TurnFinished(_))
        ));
    }

    #[test]
    fn transport_eof_after_provisional_text_never_replays() {
        let context = ProviderContext::new(Duration::ZERO, false, None);
        let mut attempts = 0;
        let events = retry_exchange(
            &ProviderSettings::default(),
            &context,
            None,
            &|_| {},
            |sink| {
                attempts += 1;
                let response = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n";
                let mut source = ScriptedSource([response.to_vec()].into());
                stream_response(
                    &mut source,
                    None,
                    &context,
                    sink,
                    ProtocolKind::ChatCompletions,
                )
            },
            |_, _, _| panic!("must not replay provisional text"),
        );
        assert_eq!(attempts, 1);
        assert_category(&events, ErrorCategory::Protocol);
    }

    #[test]
    fn tls_security_and_local_configuration_diagnostics_are_terminal() {
        for diagnostic in [
            "verify error:num=20:unable to get local issuer certificate",
            "certificate verify failed",
            "Verification error: hostname mismatch",
            "s_client: Unknown option: -verify_hostname",
            "no shared cipher",
        ] {
            assert!(tls_terminal_diagnostic(diagnostic.as_bytes()));
        }
        for diagnostic in [
            "connect:errno=61",
            "unexpected eof while reading",
            "Connection refused",
        ] {
            assert!(!tls_terminal_diagnostic(diagnostic.as_bytes()));
        }
    }

    #[test]
    fn failed_request_write_is_retryable_before_delivery() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let mut stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_peer, _) = listener.accept().unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let context = ProviderContext::new(Duration::ZERO, false, None);
        let deadline = Some(Instant::now() + Duration::from_secs(1));
        let error =
            write_request(&mut stream, b"request", b"body", &context, deadline).unwrap_err();
        assert!(has_retryable_terminal(&[error]));
    }
}
