//! OpenAI-compatible chat completions adapter over blocking std sockets.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
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

/// OpenAI-compatible chat provider over plain HTTP. Constructed from a
/// configured profile plus the vendor model string; see
/// [`OpenAiProvider::from_profile`].
pub struct OpenAiProvider {
    host: String,
    port: u16,
    base_path: String,
    credential: CredentialRef,
    model: String,
}

impl OpenAiProvider {
    /// Builds an adapter for one `http` base URL (for example
    /// `http://localhost:11434/v1`), credential reference, and vendor
    /// model name. `https` URLs are refused explicitly: this transport
    /// has no TLS, and attempting one would only produce a confusing
    /// handshake failure.
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
        let rest = endpoint.strip_prefix("http://").ok_or_else(|| {
            provider_error_inner(
                ErrorCategory::UnsupportedCapability,
                "https endpoints need a TLS transport",
            )
        })?;
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
            None => (authority.to_owned(), 80),
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

    /// Encodes one turn as a Chat Completions body.
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
            "stream": false,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
            body["tool_choice"] = Value::String("auto".to_owned());
        }
        body
    }

    /// Performs one blocking exchange, honouring cancellation and the run
    /// deadline between read quanta.
    fn exchange(
        &self,
        body: &[u8],
        context: &ProviderContext,
    ) -> Result<(u16, Vec<u8>), ProviderEvent> {
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
        let path = format!("{}/chat/completions", self.base_path);
        let head = format!(
            "POST {path} HTTP/1.1\r\nhost: {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nauthorization: Bearer {token}\r\nconnection: close\r\n\r\n",
            address,
            body.len(),
        );
        stream
            .write_all(head.as_bytes())
            .and_then(|()| stream.write_all(body))
            .map_err(|_| provider_error(ErrorCategory::Protocol, "provider request failed"))?;
        if context.is_cancelled() {
            return Err(provider_error(
                ErrorCategory::Cancelled,
                "provider call was cancelled",
            ));
        }
        let raw =
            read_bounded(&mut stream, deadline, context).map_err(|failure| match failure {
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
                ReadFailure::Broken => {
                    provider_error(ErrorCategory::Protocol, "provider response failed")
                }
            })?;
        let text = String::from_utf8_lossy(&raw);
        let (head, body) = text
            .split_once("\r\n\r\n")
            .ok_or_else(|| provider_error(ErrorCategory::Protocol, "provider response failed"))?;
        let status: u16 = head
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .ok_or_else(|| provider_error(ErrorCategory::Protocol, "provider response failed"))?;
        Ok((status, body.as_bytes().to_vec()))
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
fn events_for(status: u16, body: &[u8]) -> Vec<ProviderEvent> {
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
    let claimed_tool_calls = reason == "tool_calls";
    let finish = match reason {
        "stop" => FinishReason::Stop,
        "tool_calls" if !ready.is_empty() => FinishReason::ToolCalls,
        "length" => FinishReason::OutputLimit,
        "content_filter" => FinishReason::Refusal,
        _ => FinishReason::Incomplete,
    };
    if claimed_tool_calls && ready.is_empty() {
        return vec![provider_error(
            ErrorCategory::Protocol,
            "provider claimed tool calls without items",
        )];
    }
    let (input, output) = reply
        .get("usage")
        .map(|usage| {
            (
                usage.get("prompt_tokens").and_then(Value::as_u64),
                usage.get("completion_tokens").and_then(Value::as_u64),
            )
        })
        .unwrap_or((None, None));
    events.push(ProviderEvent::TurnFinished(TurnFinished::new(
        finish,
        Usage::new(input, output, UsageFinality::Final),
        None,
    )));
    events
}

impl ProviderPort for OpenAiProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            text: true,
            streaming: false,
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
        let (status, payload) = match self.exchange(&body, context) {
            Ok(exchange) => exchange,
            Err(failure) => return vec![failure],
        };
        events_for(status, &payload)
    }
}
