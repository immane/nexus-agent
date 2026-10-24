#![forbid(unsafe_code)]

//! Adapter coverage against a loopback mock speaking the Chat Completions
//! dialect: message/tool mapping, finish reasons, usage honesty, HTTP and
//! transport failures, credential gating, cancellation, deadlines, and
//! construction validation. No external network is ever touched.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nexus_config::{AdapterKind, CredentialRef, ProviderProfile};
use nexus_core::{
    CallId, CancellationToken, ContinuationData, EffectState, ErrorCategory, Evidence,
    ExecutionStatus, FinishReason, M0_REVISION, ModelContextItem, ModelInstructions, ModelRequest,
    NormalizedArgs, ProviderContext, ProviderEvent, ProviderPort, RetryGuidance, RunId, ToolCall,
    ToolId, ToolOutcome, ToolSpec, TurnFinished, TurnId, Usage, UsageFinality,
};
use nexus_openai::OpenAiProvider;
use serde_json::Value;

/// Observed mock interaction: connection count plus the raw request bytes.
struct Served {
    hits: Arc<AtomicUsize>,
    request: Arc<Mutex<Vec<u8>>>,
}

/// Serves one canned HTTP response after an optional delay, then closes.
fn serve(status: u16, body: &str, delay: Duration) -> (String, Served) {
    serve_n(status, body, delay, 1)
}

/// Serves the same response for a bounded number of connections.
fn serve_n(status: u16, body: &str, delay: Duration, requests: usize) -> (String, Served) {
    serve_sequence(&vec![status; requests], body, delay)
}

/// Serves one response status per connection, allowing retry tests to end
/// with a non-retryable response rather than waiting through the full policy.
fn serve_sequence(statuses: &[u16], body: &str, delay: Duration) -> (String, Served) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback binds");
    let port = listener.local_addr().expect("port known").port();
    let served = Served {
        hits: Arc::new(AtomicUsize::new(0)),
        request: Arc::new(Mutex::new(Vec::new())),
    };
    let captured = Served {
        hits: Arc::clone(&served.hits),
        request: Arc::clone(&served.request),
    };
    let body = body.to_owned();
    let statuses = statuses.to_vec();
    std::thread::spawn(move || {
        for status in statuses {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
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
            std::thread::sleep(delay);
            let response = format!(
                "HTTP/1.1 {status} Test\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), served)
}

/// Credential that resolves in practice (`PATH` is set and non-empty in
/// every test runner); values are never asserted.
fn present_credential() -> CredentialRef {
    CredentialRef::env_var("PATH").expect("credential reference builds")
}

fn provider(base: &str) -> OpenAiProvider {
    OpenAiProvider::new(base, present_credential(), "test-model").expect("provider builds")
}

fn live_context() -> ProviderContext {
    ProviderContext::new(Duration::from_secs(60), false, None)
}

fn base_request() -> ModelRequest {
    ModelRequest::new(
        RunId::new("run-1").expect("valid"),
        TurnId::new("turn-1").expect("valid"),
        "web-test",
        Vec::new(),
        None,
        4096,
    )
    .expect("request builds")
}

fn result_for(call: &str, item: &str, provider_ref: &str) -> ModelContextItem {
    ModelContextItem::tool_result(
        CallId::new(call).unwrap(),
        item,
        provider_ref,
        ToolId::new("host_read", M0_REVISION).unwrap(),
        ToolOutcome::new(
            ExecutionStatus::Succeeded,
            EffectState::KnownNotApplied,
            Evidence::HostObserved,
            "observed result",
            false,
        )
        .unwrap(),
    )
    .unwrap()
}

fn failed(events: Vec<ProviderEvent>) -> (ErrorCategory, nexus_core::RetryGuidance) {
    assert_eq!(events.len(), 1, "failures are single events: {events:?}");
    match &events[0] {
        ProviderEvent::Failed(error) => (error.category(), error.retry()),
        other => panic!("expected a failure, got {other:?}"),
    }
}

fn finished(events: &[ProviderEvent]) -> &TurnFinished {
    let terminals: Vec<&TurnFinished> = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::TurnFinished(finished) => Some(finished),
            _ => None,
        })
        .collect();
    assert_eq!(terminals.len(), 1, "exactly one terminal: {events:?}");
    assert!(
        matches!(events.last(), Some(ProviderEvent::TurnFinished(_))),
        "the terminal event is last"
    );
    terminals.into_iter().next().expect("one terminal")
}

fn stop_body(text: &str) -> String {
    format!(
        r#"{{"id":"chatcmpl-1","choices":[{{"message":{{"role":"assistant","content":{text:?}}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":10,"completion_tokens":3}}}}"#
    )
}

#[test]
fn capabilities_describe_text_tool_calls_and_streaming() {
    let capabilities = OpenAiProvider::new("http://localhost:11434/v1", present_credential(), "m")
        .expect("provider builds")
        .capabilities();
    assert!(capabilities.text);
    assert!(capabilities.tool_calls);
    assert!(capabilities.streaming);
    assert!(capabilities.usage_reporting);
    assert_eq!(capabilities.max_context_items, None);
    assert_eq!(capabilities.max_output_bytes, None);
    assert_eq!(
        OpenAiProvider::new("http://localhost:11434/v1", present_credential(), "m")
            .expect("provider builds")
            .model(),
        "m"
    );
}

#[test]
fn construction_accepts_https_with_verified_tls_bridge() {
    let plain = OpenAiProvider::new("http://h/v1", present_credential(), "m").expect("http builds");
    assert!(!plain.uses_tls());
    let tls = OpenAiProvider::new("https://api.example.com/v1", present_credential(), "m")
        .expect("https builds");
    assert!(tls.uses_tls());
    // Explicit ports still parse on both schemes.
    assert!(
        OpenAiProvider::new("https://h:8443/v1", present_credential(), "m")
            .expect("explicit TLS port builds")
            .uses_tls()
    );
    assert!(
        !OpenAiProvider::new("http://h:11434/v1", present_credential(), "m")
            .expect("explicit plain port builds")
            .uses_tls()
    );
}

#[test]
fn construction_rejects_bad_endpoints_models_and_profiles() {
    for bad in [
        "wss://api.example.com",
        "ftp://api.example.com/v1",
        "api.example.com/v1",
        "",
        "http:///v1",
        "https:///v1",
        "http://h:abc/v1",
        "https://h:abc/v1",
    ] {
        assert!(
            OpenAiProvider::new(bad, present_credential(), "m").is_err(),
            "{bad:?} rejected"
        );
    }
    assert!(OpenAiProvider::new("http://h/v1", present_credential(), "").is_err());
    assert!(OpenAiProvider::new("http://h/v1", present_credential(), "m".repeat(129)).is_err());
    assert!(OpenAiProvider::new("http://h:1/v1", present_credential(), "m").is_ok());
    assert!(OpenAiProvider::new("http://h/v1/", present_credential(), "m").is_ok());

    let mut profile = ProviderProfile::new(
        "acme",
        "Acme",
        AdapterKind::Direct,
        Some("http://localhost:11434/v1".to_owned()),
        present_credential(),
        "default-model",
    )
    .expect("profile builds");
    assert!(OpenAiProvider::from_profile(&profile, "chosen").is_ok());
    let relay = ProviderProfile::new(
        "relay",
        "Relay",
        AdapterKind::Relay,
        Some("http://localhost:11434/v1".to_owned()),
        present_credential(),
        "default-model",
    )
    .expect("profile builds");
    assert!(OpenAiProvider::from_profile(&relay, "chosen").is_ok());
    profile.endpoint = None;
    assert!(OpenAiProvider::from_profile(&profile, "chosen").is_err());
    assert!(
        OpenAiProvider::from_profile(
            &ProviderProfile::new(
                "acme",
                "Acme",
                AdapterKind::Direct,
                Some("http://localhost:11434/v1".to_owned()),
                present_credential(),
                "default-model",
            )
            .expect("profile builds"),
            ""
        )
        .is_err()
    );
}

#[test]
fn configured_responses_and_messages_protocols_use_their_wire_contracts() {
    for (protocol, path, expected_text, response) in [
        (
            nexus_config::ProtocolKind::OpenAiResponses,
            "/v1/responses",
            "Responses hello",
            r#"{"id":"resp_1","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"Responses hello"}]}],"usage":{"input_tokens":2,"output_tokens":3}}"#,
        ),
        (
            nexus_config::ProtocolKind::AnthropicMessages,
            "/v1/messages",
            "Messages hello",
            r#"{"type":"message","content":[{"type":"text","text":"Messages hello"}],"stop_reason":"end_turn","usage":{"input_tokens":4,"output_tokens":5}}"#,
        ),
    ] {
        let (base, served) = serve(200, response, Duration::ZERO);
        let profile = nexus_config::ProviderProfile::new_with_protocol(
            "fixture-provider",
            "Fixture provider",
            nexus_config::AdapterKind::Direct,
            protocol,
            Some(base),
            present_credential(),
            "fixture-model",
        )
        .expect("protocol profile builds");
        let adapter = OpenAiProvider::from_profile(&profile, "fixture-model")
            .expect("profile selects its protocol adapter");
        assert_eq!(adapter.continuation_scope("profile-a"), "profile-a");
        let expected_identity = match protocol {
            nexus_config::ProtocolKind::ChatCompletions => unreachable!(),
            nexus_config::ProtocolKind::OpenAiResponses => "openai-responses-v1",
            nexus_config::ProtocolKind::AnthropicMessages => "anthropic-messages-v1",
        };
        assert_eq!(adapter.adapter_identity(), expected_identity);
        let request = base_request()
            .with_conversation(vec![
                ModelContextItem::user_text("fixture prompt").expect("user text builds"),
            ])
            .expect("conversation attaches")
            .with_tool_definitions(vec![
                ToolSpec::new(
                    ToolId::new("host_read", M0_REVISION).expect("tool id builds"),
                    "Read a file",
                    r#"{"type":"object"}"#,
                )
                .expect("tool spec builds"),
            ])
            .expect("tool definitions attach");
        let events = adapter.stream(&request, &live_context());
        assert!(matches!(
            events.first(),
            Some(ProviderEvent::TextDelta { text, .. }) if text == expected_text
        ));
        assert!(matches!(
            events.last(),
            Some(ProviderEvent::TurnFinished(_))
        ));
        let request = String::from_utf8_lossy(&served.request.lock().expect("request captured"))
            .to_ascii_lowercase();
        assert!(request.contains(path), "request uses the selected endpoint");
        match protocol {
            nexus_config::ProtocolKind::OpenAiResponses => {
                assert!(request.contains("authorization: bearer "));
                assert!(request.contains("\"type\":\"function\""));
                assert!(
                    request
                        .contains("\"input\":[{\"content\":\"fixture prompt\",\"role\":\"user\"}]")
                );
            }
            nexus_config::ProtocolKind::AnthropicMessages => {
                assert!(request.contains("x-api-key:"));
                assert!(request.contains("anthropic-version: 2023-06-01"));
                assert!(request.contains("\"messages\":[{\"content\":[{\"text\":\"fixture prompt\",\"type\":\"text\"}],\"role\":\"user\"}]"));
                assert!(request.contains("\"input_schema\""));
            }
            nexus_config::ProtocolKind::ChatCompletions => unreachable!(),
        }
    }
}

#[test]
fn all_protocols_encode_instructions_without_changing_conversation() {
    for (protocol, response) in [
        (nexus_config::ProtocolKind::ChatCompletions, stop_body("done")),
        (
            nexus_config::ProtocolKind::OpenAiResponses,
            r#"{"id":"resp_1","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"done"}]}]}"#.to_owned(),
        ),
        (
            nexus_config::ProtocolKind::AnthropicMessages,
            r#"{"type":"message","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}"#.to_owned(),
        ),
    ] {
        for instruction_parts in [
            None,
            Some(("", "")),
            Some(("Agent fixture: \"quoted\"\nUnicode: 日本語", "")),
            Some(("", "Environment fixture: /workspace/project")),
            Some(("Agent fixture", "Environment fixture\nsecond line")),
        ] {
            let (base, served) = serve(200, &response, Duration::ZERO);
            let adapter = OpenAiProvider::new_for_protocol(
                &base,
                present_credential(),
                "test-model",
                protocol,
            )
            .expect("adapter builds");
            let mut request = base_request()
                .with_conversation(vec![
                    ModelContextItem::user_text("fixture prompt").expect("user text builds"),
                ])
                .expect("conversation attaches");
            if let Some((agent, environment)) = instruction_parts {
                request = request
                    .with_instructions(
                        ModelInstructions::new(agent, environment).expect("instructions build"),
                    )
                    .expect("instructions attach");
            }
            request = request
                .with_context_note("Earlier completed exchanges were omitted.")
                .expect("runtime note attaches");
            let rendered = request
                .rendered_instructions()
                .expect("note renders in native instruction field");
            let events = adapter.stream(&request, &live_context());
            assert_eq!(finished(&events).reason(), FinishReason::Stop);
            let raw = served.request.lock().expect("request captured");
            let raw = String::from_utf8_lossy(&raw);
            let (_, body) = raw.split_once("\r\n\r\n").expect("HTTP body exists");
            let body: Value = serde_json::from_str(body).expect("request body is JSON");
            match protocol {
                nexus_config::ProtocolKind::ChatCompletions => {
                    let mut expected = Vec::new();
                    expected.push(serde_json::json!({"role":"system","content":rendered}));
                    expected.push(serde_json::json!({"role":"user","content":"fixture prompt"}));
                    assert_eq!(body["messages"], Value::Array(expected));
                    assert!(body.get("instructions").is_none());
                    assert!(body.get("system").is_none());
                }
                nexus_config::ProtocolKind::OpenAiResponses => {
                    assert_eq!(
                        body["input"],
                        serde_json::json!([{"role":"user","content":"fixture prompt"}])
                    );
                    assert_eq!(
                        body.get("instructions"),
                        Some(Value::String(rendered)).as_ref()
                    );
                    assert!(body.get("system").is_none());
                    assert!(body.get("messages").is_none());
                }
                nexus_config::ProtocolKind::AnthropicMessages => {
                    assert_eq!(
                        body["messages"],
                        serde_json::json!([{"role":"user","content":[{"type":"text","text":"fixture prompt"}]}])
                    );
                    assert_eq!(
                        body.get("system"),
                        Some(Value::String(rendered)).as_ref()
                    );
                    assert!(body.get("instructions").is_none());
                }
            }
        }
    }
}

#[test]
fn all_protocols_reject_unconsumed_opaque_continuation_state() {
    for protocol in [
        nexus_config::ProtocolKind::ChatCompletions,
        nexus_config::ProtocolKind::OpenAiResponses,
        nexus_config::ProtocolKind::AnthropicMessages,
    ] {
        let adapter = OpenAiProvider::new_for_protocol(
            "http://127.0.0.1:1",
            present_credential(),
            "model",
            protocol,
        )
        .expect("adapter builds without connecting");
        let continuation = ContinuationData::new(
            adapter.adapter_identity(),
            "profile",
            b"opaque provider state".to_vec(),
        )
        .expect("continuation is bounded");
        let request = ModelRequest::new(
            RunId::new("run-continuation").expect("run id is valid"),
            TurnId::new("turn-continuation").expect("turn id is valid"),
            "profile",
            Vec::new(),
            Some(continuation),
            1024,
        )
        .expect("request with continuation builds");
        let events = adapter.stream(&request, &live_context());
        assert!(matches!(
            events.first(),
            Some(ProviderEvent::Failed(error))
                if error.category() == ErrorCategory::UnsupportedCapability
        ));
    }
}

#[test]
fn responses_and_messages_reject_reasoning_they_cannot_replay_faithfully() {
    for protocol in [
        nexus_config::ProtocolKind::OpenAiResponses,
        nexus_config::ProtocolKind::AnthropicMessages,
    ] {
        let adapter = OpenAiProvider::new_for_protocol(
            "http://127.0.0.1:1",
            present_credential(),
            "model",
            protocol,
        )
        .expect("protocol adapter builds without connecting");
        let request = base_request()
            .with_conversation(vec![
                ModelContextItem::assistant_reasoning("private reasoning".to_owned())
                    .expect("reasoning fits the context bound"),
            ])
            .expect("reasoning transcript attaches");
        assert!(matches!(
            adapter.stream(&request, &live_context()).first(),
            Some(ProviderEvent::Failed(error))
                if error.category() == ErrorCategory::UnsupportedCapability
        ));
    }
}

#[test]
fn responses_and_messages_sse_normalize_text_tool_calls_and_termination() {
    let responses_body = concat!(
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"hello\"}]},{\"type\":\"function_call\",\"call_id\":\"call-1\",\"name\":\"host_read\",\"arguments\":\"{\\\"path\\\":\\\"a\\\"}\"}],\"usage\":{\"input_tokens\":2,\"output_tokens\":3}}}\n\n"
    );
    let (base, _) = serve_raw(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
        responses_body,
    );
    let responses = OpenAiProvider::new_for_protocol(
        &base,
        present_credential(),
        "model",
        nexus_config::ProtocolKind::OpenAiResponses,
    )
    .expect("Responses adapter builds");
    let provisional = Mutex::new(Vec::new());
    let events = responses.stream_with_sink(&base_request(), &live_context(), &|event| {
        provisional.lock().expect("sink list writable").push(event)
    });
    assert!(
        matches!(provisional.lock().expect("sink list readable").first(), Some(ProviderEvent::TextDelta { text, .. }) if text == "hello")
    );
    assert!(
        matches!(events.get(1), Some(ProviderEvent::ToolCallReady(candidate)) if candidate.provider_ref() == "call-1")
    );
    assert!(matches!(
        events.last(),
        Some(ProviderEvent::TurnFinished(_))
    ));

    let anthropic_body = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":4}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu-1\",\"name\":\"host_read\",\"input\":{}}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"b\\\"}\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    );
    let (base, _) = serve_raw(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
        anthropic_body,
    );
    let messages = OpenAiProvider::new_for_protocol(
        &base,
        present_credential(),
        "model",
        nexus_config::ProtocolKind::AnthropicMessages,
    )
    .expect("Messages adapter builds");
    let events = messages.stream(&base_request(), &live_context());
    assert!(
        matches!(events.first(), Some(ProviderEvent::ToolCallReady(candidate)) if candidate.provider_ref() == "toolu-1" && candidate.arguments_json() == "{\"path\":\"b\"}")
    );
    assert!(matches!(
        events.last(),
        Some(ProviderEvent::TurnFinished(_))
    ));
}

#[test]
fn responses_incomplete_tool_proposals_are_never_dispatchable() {
    for (status, body) in [
        (
            "JSON",
            r#"{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[{"type":"function_call","call_id":"partial-call","name":"host_read","arguments":"{\"path\":\"x\"}"}]}"#,
        ),
        (
            "SSE",
            r#"data: {"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[{"type":"function_call","call_id":"partial-call","name":"host_read","arguments":"{\"path\":\"x\"}"}]}}"#,
        ),
    ] {
        let (base, _) = if status == "JSON" {
            serve(200, body, Duration::ZERO)
        } else {
            serve_raw(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                &format!("{body}\n\n"),
            )
        };
        let adapter = OpenAiProvider::new_for_protocol(
            &base,
            present_credential(),
            "model",
            nexus_config::ProtocolKind::OpenAiResponses,
        )
        .expect("Responses adapter builds");
        let events = adapter.stream(&base_request(), &live_context());
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ProviderEvent::ToolCallReady(_))),
            "{status} incomplete response exposed a tool call: {events:?}"
        );
        assert!(matches!(
            events.last(),
            Some(ProviderEvent::TurnFinished(finished))
                if finished.reason() == FinishReason::OutputLimit
        ));
    }
}

#[test]
fn responses_and_messages_reject_unreplayable_reasoning_in_replies() {
    for (protocol, body) in [
        (
            nexus_config::ProtocolKind::OpenAiResponses,
            r#"{"status":"completed","output":[{"type":"reasoning","summary":[]},{"type":"message","content":[{"type":"output_text","text":"answer"}]}]}"#,
        ),
        (
            nexus_config::ProtocolKind::AnthropicMessages,
            r#"{"type":"message","content":[{"type":"thinking","thinking":"hidden trace","signature":"sig"}],"stop_reason":"end_turn"}"#,
        ),
    ] {
        let (base, _) = serve(200, body, Duration::ZERO);
        let adapter =
            OpenAiProvider::new_for_protocol(&base, present_credential(), "model", protocol)
                .expect("protocol adapter builds");
        assert!(matches!(
            adapter.stream(&base_request(), &live_context()).first(),
            Some(ProviderEvent::Failed(error))
                if error.category() == ErrorCategory::UnsupportedCapability
        ));
    }
}

#[test]
fn responses_and_messages_sse_reject_unreplayable_reasoning() {
    for (protocol, body) in [
        (
            nexus_config::ProtocolKind::OpenAiResponses,
            "event: response.reasoning_summary_text.delta\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"private\"}\n\n",
        ),
        (
            nexus_config::ProtocolKind::AnthropicMessages,
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"private\",\"signature\":\"sig\"}}\n\n",
        ),
    ] {
        let (base, _) = serve_raw(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
            body,
        );
        let adapter =
            OpenAiProvider::new_for_protocol(&base, present_credential(), "model", protocol)
                .expect("protocol adapter builds");
        assert!(matches!(
            adapter.stream(&base_request(), &live_context()).first(),
            Some(ProviderEvent::Failed(error))
                if error.category() == ErrorCategory::UnsupportedCapability
        ));
    }
}

#[test]
fn messages_sse_rejects_unclosed_or_misordered_content_blocks() {
    for body in [
        concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu-1\",\"name\":\"host_read\",\"input\":{}}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        ),
        concat!(
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        ),
    ] {
        let (base, _) = serve_raw(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
            body,
        );
        let adapter = OpenAiProvider::new_for_protocol(
            &base,
            present_credential(),
            "model",
            nexus_config::ProtocolKind::AnthropicMessages,
        )
        .expect("Messages adapter builds");
        assert!(matches!(
            adapter.stream(&base_request(), &live_context()).first(),
            Some(ProviderEvent::Failed(error)) if error.category() == ErrorCategory::Protocol
        ));
    }
}

#[test]
fn messages_sse_rejects_invalid_delta_fields_and_block_types() {
    let tool = serde_json::json!({"type":"tool_use","id":"toolu-1","name":"host_read","input":{}});
    let text = serde_json::json!({"type":"text","text":""});
    for (block, delta) in [
        (tool.clone(), serde_json::json!({"type":"input_json_delta"})),
        (
            tool.clone(),
            serde_json::json!({"type":"input_json_delta","partial_json":42}),
        ),
        (
            tool.clone(),
            serde_json::json!({"type":"input_json_delta","partial_json":null}),
        ),
        (
            tool.clone(),
            serde_json::json!({"type":"text_delta","text":"wrong block"}),
        ),
        (tool.clone(), serde_json::json!({"type":"unknown_delta"})),
        (tool, serde_json::json!({})),
        (
            text.clone(),
            serde_json::json!({"type":"input_json_delta","partial_json":"{}"}),
        ),
        (text.clone(), serde_json::json!({"type":"text_delta"})),
        (text, serde_json::json!({"type":"text_delta","text":42})),
    ] {
        let body = format!(
            "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{block}}}\n\n\
             data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{delta}}}\n\n\
             data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
             data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\"}}}}\n\n\
             data: {{\"type\":\"message_stop\"}}\n\n"
        );
        let (base, _) = serve_raw(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
            &body,
        );
        let adapter = OpenAiProvider::new_for_protocol(
            &base,
            present_credential(),
            "model",
            nexus_config::ProtocolKind::AnthropicMessages,
        )
        .expect("Messages adapter builds");
        let provisional = Mutex::new(Vec::new());
        let events = adapter.stream_with_sink(&base_request(), &live_context(), &|event| {
            provisional.lock().unwrap().push(event);
        });
        assert!(
            matches!(events.as_slice(), [ProviderEvent::Failed(error)] if error.category() == ErrorCategory::Protocol),
            "invalid delta {delta} for block {block} was accepted: {events:?}"
        );
        assert!(provisional.lock().unwrap().is_empty());
    }
}

#[test]
fn messages_sse_rejects_repeated_starts_stops_and_post_stop_deltas() {
    let start = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu-1\",\"name\":\"host_read\",\"input\":{}}}\n\n";
    let stop = "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n";
    let delta = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n";
    for prefix in [
        format!("{start}{start}"),
        format!("{start}{stop}{start}"),
        format!("{start}{stop}{stop}"),
        format!("{start}{stop}{delta}"),
        stop.to_owned(),
    ] {
        let body = format!(
            "{prefix}data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\"}}}}\n\ndata: {{\"type\":\"message_stop\"}}\n\n"
        );
        let (base, _) = serve_raw(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
            &body,
        );
        let adapter = OpenAiProvider::new_for_protocol(
            &base,
            present_credential(),
            "model",
            nexus_config::ProtocolKind::AnthropicMessages,
        )
        .expect("Messages adapter builds");
        let events = adapter.stream(&base_request(), &live_context());
        assert!(matches!(
            events.as_slice(),
            [ProviderEvent::Failed(error)] if error.category() == ErrorCategory::Protocol
        ));
    }
}

#[test]
fn messages_rejects_redacted_thinking_in_json_and_sse() {
    let block = r#"{"type":"redacted_thinking","data":"opaque"}"#;
    for sse in [false, true] {
        let (base, _) = if sse {
            serve_raw(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                &format!(
                    "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{block}}}\n\n"
                ),
            )
        } else {
            serve(
                200,
                &format!(
                    "{{\"type\":\"message\",\"content\":[{block}],\"stop_reason\":\"end_turn\"}}"
                ),
                Duration::ZERO,
            )
        };
        let adapter = OpenAiProvider::new_for_protocol(
            &base,
            present_credential(),
            "model",
            nexus_config::ProtocolKind::AnthropicMessages,
        )
        .expect("Messages adapter builds");
        let events = adapter.stream(&base_request(), &live_context());
        assert!(matches!(
            events.as_slice(),
            [ProviderEvent::Failed(error)] if error.category() == ErrorCategory::UnsupportedCapability
        ));
    }
}

#[test]
fn responses_and_messages_replay_tool_calls_with_their_results() {
    let call = ToolCall::new(
        RunId::new("run-1").expect("run id builds"),
        TurnId::new("turn-1").expect("turn id builds"),
        CallId::new("call-1").expect("call id builds"),
        ToolId::new("host_read", M0_REVISION).expect("tool id builds"),
        NormalizedArgs::new(r#"{"path":"src/main.rs"}"#).expect("arguments normalize"),
    );
    let outcome = ToolOutcome::new(
        ExecutionStatus::Succeeded,
        EffectState::KnownNotApplied,
        Evidence::HostObserved,
        "file contents",
        false,
    )
    .expect("tool outcome builds");
    let conversation = vec![
        ModelContextItem::user_text("read the file").expect("user text builds"),
        ModelContextItem::assistant_call("item-1", "provider-call-1", call)
            .expect("assistant call builds"),
        ModelContextItem::tool_result(
            CallId::new("call-1").expect("call id builds"),
            "item-1",
            "provider-call-1",
            ToolId::new("host_read", M0_REVISION).expect("tool id builds"),
            outcome,
        )
        .expect("tool result builds"),
    ];
    for protocol in [
        nexus_config::ProtocolKind::OpenAiResponses,
        nexus_config::ProtocolKind::AnthropicMessages,
    ] {
        let response = match protocol {
            nexus_config::ProtocolKind::OpenAiResponses => {
                r#"{"id":"resp_2","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"done"}]}],"usage":{"input_tokens":3,"output_tokens":2}}"#
            }
            nexus_config::ProtocolKind::AnthropicMessages => {
                r#"{"type":"message","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":2}}"#
            }
            nexus_config::ProtocolKind::ChatCompletions => unreachable!(),
        };
        let (base, served) = serve(200, response, Duration::ZERO);
        let adapter =
            OpenAiProvider::new_for_protocol(&base, present_credential(), "model", protocol)
                .expect("protocol adapter builds");
        let request = base_request()
            .with_conversation(conversation.clone())
            .expect("tool transcript attaches")
            .with_tool_definitions(vec![
                ToolSpec::new(
                    ToolId::new("host_read", M0_REVISION).expect("tool id builds"),
                    "Read a file",
                    r#"{"type":"object"}"#,
                )
                .expect("tool spec builds"),
            ])
            .expect("tool definition attaches");
        let events = adapter.stream(&request, &live_context());
        assert!(
            matches!(
                events.first(),
                Some(ProviderEvent::TextDelta { text, .. }) if text == "done"
            ),
            "protocol {protocol:?} response events: {events:?}"
        );
        let captured = served.request.lock().expect("request captured").clone();
        let captured = String::from_utf8_lossy(&captured);
        let (_, body) = captured.split_once("\r\n\r\n").expect("request framed");
        let body: Value = serde_json::from_str(body).expect("request body is JSON");
        match protocol {
            nexus_config::ProtocolKind::OpenAiResponses => {
                assert_eq!(body["input"][1]["type"], "function_call");
                assert_eq!(body["input"][1]["call_id"], "provider-call-1");
                assert_eq!(body["input"][1]["arguments"], r#"{"path":"src/main.rs"}"#);
                assert_eq!(body["input"][2]["type"], "function_call_output");
                assert_eq!(body["input"][2]["call_id"], "provider-call-1");
                assert_eq!(body["input"][2]["output"], "file contents");
            }
            nexus_config::ProtocolKind::AnthropicMessages => {
                assert_eq!(body["messages"][1]["role"], "assistant");
                assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
                assert_eq!(body["messages"][1]["content"][0]["id"], "provider-call-1");
                assert_eq!(
                    body["messages"][1]["content"][0]["input"]["path"],
                    "src/main.rs"
                );
                assert_eq!(body["messages"][2]["role"], "user");
                assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
                assert_eq!(
                    body["messages"][2]["content"][0]["tool_use_id"],
                    "provider-call-1"
                );
                assert_eq!(
                    body["messages"][2]["content"][0]["content"],
                    "file contents"
                );
            }
            nexus_config::ProtocolKind::ChatCompletions => unreachable!(),
        }
    }
}

#[test]
fn responses_and_messages_errors_are_static_and_truncated_streams_fail() {
    for protocol in [
        nexus_config::ProtocolKind::OpenAiResponses,
        nexus_config::ProtocolKind::AnthropicMessages,
    ] {
        let (base, _) = serve(
            401,
            r#"{"error":"private vendor response containing sensitive text"}"#,
            Duration::ZERO,
        );
        let adapter =
            OpenAiProvider::new_for_protocol(&base, present_credential(), "model", protocol)
                .expect("protocol adapter builds");
        let events = adapter.stream(
            &base_request(),
            &ProviderContext::new(Duration::from_millis(1_500), false, None),
        );
        assert_eq!(failed(events).0, ErrorCategory::Authentication);

        let (base, _) = match protocol {
            nexus_config::ProtocolKind::OpenAiResponses => serve_raw(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
            ),
            nexus_config::ProtocolKind::AnthropicMessages => serve_raw(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
            ),
            nexus_config::ProtocolKind::ChatCompletions => unreachable!(),
        };
        let adapter =
            OpenAiProvider::new_for_protocol(&base, present_credential(), "model", protocol)
                .expect("protocol adapter builds");
        let events = adapter.stream(&base_request(), &live_context());
        assert_eq!(failed(events).0, ErrorCategory::Protocol);
    }
}

#[test]
fn stop_turn_maps_text_usage_and_terminal() {
    let (base, served) = serve(200, &stop_body("hello there"), Duration::ZERO);
    let events = provider(&base).stream(
        &base_request()
            .with_conversation(vec![ModelContextItem::user_text("hi").expect("valid")])
            .expect("conversation attaches"),
        &live_context(),
    );
    assert_eq!(served.hits.load(Ordering::SeqCst), 1);
    assert!(matches!(
        &events[0],
        ProviderEvent::TextDelta { item_key, text }
            if item_key == "item-0" && text == "hello there"
    ));
    let terminal = finished(&events);
    assert_eq!(terminal.reason(), FinishReason::Stop);
    assert_eq!(
        terminal.usage(),
        Usage::new(Some(10), Some(3), UsageFinality::Final)
    );
}

#[test]
fn tool_calls_map_to_candidates_with_exact_refs_and_arguments() {
    let body = r#"{"id":"x","choices":[{"message":{"role":"assistant","content":"","tool_calls":[{"id":"call-9","type":"function","function":{"name":"host_read","arguments":"{\"path\":\"src\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":5,"completion_tokens":7}}"#;
    let (base, _) = serve(200, body, Duration::ZERO);
    let events = provider(&base).stream(&base_request(), &live_context());
    assert!(
        !events.iter().any(|event| matches!(
            event,
            ProviderEvent::TextDelta { text, .. } if !text.is_empty()
        )),
        "empty content emits no text delta"
    );
    let ready: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ToolCallReady(candidate) => Some(candidate),
            _ => None,
        })
        .collect();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].item_key(), "item-1");
    assert_eq!(ready[0].provider_ref(), "call-9");
    let terminal = finished(&events);
    assert_eq!(terminal.reason(), FinishReason::ToolCalls);
    assert_eq!(
        terminal.usage(),
        Usage::new(Some(5), Some(7), UsageFinality::Final)
    );
}

#[test]
fn finish_reasons_map_honestly_without_fabrication() {
    for (reason, expected) in [
        ("length", FinishReason::OutputLimit),
        ("content_filter", FinishReason::Refusal),
        ("weird-future-reason", FinishReason::Incomplete),
    ] {
        let body = format!(
            r#"{{"choices":[{{"message":{{"role":"assistant","content":"x"}},"finish_reason":{reason:?}}}]}}"#
        );
        let (base, _) = serve(200, &body, Duration::ZERO);
        let events = provider(&base).stream(&base_request(), &live_context());
        assert_eq!(finished(&events).reason(), expected, "{reason}");
    }
    // Claimed tool calls without any parsed item violate the contract.
    let body = r#"{"choices":[{"message":{"role":"assistant"},"finish_reason":"tool_calls"}]}"#;
    let (base, _) = serve(200, body, Duration::ZERO);
    let error = failed(provider(&base).stream(&base_request(), &live_context()));
    assert_eq!(error.0, ErrorCategory::Protocol);
    // Empty tool name is not a valid candidate either.
    let body = r#"{"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"c","type":"function","function":{"name":"","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#;
    let (base, _) = serve(200, body, Duration::ZERO);
    let error = failed(provider(&base).stream(&base_request(), &live_context()));
    assert_eq!(error.0, ErrorCategory::Protocol);
}

#[test]
fn missing_usage_stays_unknown_never_zero() {
    let body =
        r#"{"choices":[{"message":{"role":"assistant","content":"x"},"finish_reason":"stop"}]}"#;
    let (base, _) = serve(200, body, Duration::ZERO);
    let events = provider(&base).stream(&base_request(), &live_context());
    assert_eq!(
        finished(&events).usage(),
        Usage::new(None, None, UsageFinality::Final)
    );
}

#[test]
fn http_failures_map_to_typed_categories() {
    for (status, category) in [
        (401, ErrorCategory::Authentication),
        (403, ErrorCategory::Authentication),
    ] {
        let (base, _) = serve(status, r#"{"error":"nope"}"#, Duration::ZERO);
        let error = failed(provider(&base).stream(&base_request(), &live_context()));
        assert_eq!(error.0, category, "{status}");
        assert_eq!(error.1, RetryGuidance::DoNotRetry);
    }
    let (base, _) = serve(200, "not json{{{", Duration::ZERO);
    let error = failed(provider(&base).stream(&base_request(), &live_context()));
    assert_eq!(error.0, ErrorCategory::Protocol);
    let (base, _) = serve(200, r#"{"choices":[]}"#, Duration::ZERO);
    let error = failed(provider(&base).stream(&base_request(), &live_context()));
    assert_eq!(error.0, ErrorCategory::Protocol);
}

#[test]
fn transient_http_failure_retries_before_nonretryable_status() {
    let (base, served) = serve_sequence(
        &[503, 401],
        r#"{"error":"temporarily unavailable"}"#,
        Duration::ZERO,
    );
    let started = Instant::now();
    let error = failed(provider(&base).stream(&base_request(), &live_context()));
    assert_eq!(error.0, ErrorCategory::Authentication);
    assert_eq!(
        served.hits.load(Ordering::SeqCst),
        2,
        "the retry follows the first transient response"
    );
    assert!(started.elapsed() >= Duration::from_millis(1_000));
}

#[test]
fn missing_credentials_fail_before_any_network_touch() {
    let (base, served) = serve(200, &stop_body("x"), Duration::ZERO);
    let provider = OpenAiProvider::new(
        &base,
        CredentialRef::env_var("NEXUS_OPENAI_TEST_ABSENT_ZZZ").expect("valid"),
        "m",
    )
    .expect("provider builds");
    let error = failed(provider.stream(&base_request(), &live_context()));
    assert_eq!(error.0, ErrorCategory::Authentication);
    assert_eq!(
        served.hits.load(Ordering::SeqCst),
        0,
        "no socket opens without a credential"
    );
}

#[test]
fn cancelled_calls_fail_without_network_touch() {
    let (base, served) = serve(200, &stop_body("x"), Duration::ZERO);
    let context = ProviderContext::new(Duration::from_secs(60), true, None);
    let error = failed(provider(&base).stream(&base_request(), &context));
    assert_eq!(error.0, ErrorCategory::Cancelled);
    assert_eq!(
        served.hits.load(Ordering::SeqCst),
        0,
        "no socket opens for a cancelled call"
    );
}

#[test]
fn elapsed_deadlines_fail_without_network_touch() {
    let (base, served) = serve(200, &stop_body("x"), Duration::ZERO);
    let context = ProviderContext::new(Duration::from_secs(60), false, None)
        .with_control(CancellationToken::new(), Instant::now());
    let error = failed(provider(&base).stream(&base_request(), &context));
    assert_eq!(error.0, ErrorCategory::Timeout);
    assert_eq!(served.hits.load(Ordering::SeqCst), 0);
}

#[test]
fn slow_peers_hit_the_run_deadline() {
    let (base, _) = serve(200, &stop_body("x"), Duration::from_secs(30));
    let context = ProviderContext::new(Duration::from_secs(60), false, None).with_control(
        CancellationToken::new(),
        Instant::now() + Duration::from_millis(150),
    );
    let error = failed(provider(&base).stream(&base_request(), &context));
    assert_eq!(error.0, ErrorCategory::Timeout);
}

#[test]
fn oversized_replies_fail_resource_limit() {
    let big = "x".repeat(nexus_openai::provider::MAX_RESPONSE_BYTES + 1024);
    let (base, _) = serve(200, &stop_body(&big), Duration::ZERO);
    let error = failed(provider(&base).stream(&base_request(), &live_context()));
    assert_eq!(error.0, ErrorCategory::ResourceLimit);
}

#[test]
fn chunked_sse_wire_overhead_can_exceed_content_budget() {
    let padding_line = format!(":{}\n", "x".repeat(512));
    let padding = padding_line.repeat(18_000);
    assert!(padding.len() > nexus_openai::provider::MAX_RESPONSE_BYTES);
    let data = format!(
        "{padding}data: {{\"choices\":[{{\"delta\":{{\"content\":\"ok\"}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n"
    );
    let body = format!("{:X}\r\n{data}\r\n0\r\n\r\n", data.len());
    let head = "HTTP/1.1 200 Test\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
    let (base, _) = serve_raw(head, &body);
    let events = provider(&base).stream(&base_request(), &live_context());
    assert_eq!(finished(&events).reason(), FinishReason::Stop);
    assert!(events.iter().any(|event| matches!(
        event,
        ProviderEvent::TextDelta { text, .. } if text == "ok"
    )));
}

#[test]
fn sse_single_record_is_bounded_independently_of_wire_budget() {
    let content = "x".repeat(nexus_openai::provider::MAX_RESPONSE_BYTES + 1);
    let payload = serde_json::json!({
        "choices": [{"delta": {"content": content}, "finish_reason": "stop"}]
    });
    let body = format!("data: {payload}\n\ndata: [DONE]\n");
    let (base, _) = serve_raw(&sse_head(body.len()), &body);
    let error = failed(provider(&base).stream(&base_request(), &live_context()));
    assert_eq!(error.0, ErrorCategory::ResourceLimit);
}

#[test]
fn requests_carry_model_messages_tools_and_credentials() {
    let (base, served) = serve(200, &stop_body("ok"), Duration::ZERO);
    let call = ToolCall::new(
        RunId::new("run-1").expect("valid"),
        TurnId::new("turn-1").expect("valid"),
        CallId::new("call-1").expect("valid"),
        ToolId::new("host_read", M0_REVISION).expect("valid"),
        NormalizedArgs::new(r#"{"path":"src"}"#).expect("valid"),
    );
    let outcome = ToolOutcome::new(
        ExecutionStatus::Succeeded,
        EffectState::KnownNotApplied,
        Evidence::HostObserved,
        "out",
        false,
    )
    .expect("outcome builds");
    let request = base_request()
        .with_conversation(vec![
            ModelContextItem::user_text("do it").expect("valid"),
            ModelContextItem::assistant_call("item-9", "prov-9", call).expect("valid"),
            ModelContextItem::tool_result(
                CallId::new("call-1").expect("valid"),
                "item-9",
                "prov-9",
                ToolId::new("host_read", M0_REVISION).expect("valid"),
                outcome,
            )
            .expect("valid"),
        ])
        .expect("conversation attaches")
        .with_tool_definitions(vec![
            ToolSpec::new(
                ToolId::new("host_read", M0_REVISION).expect("valid"),
                "Read a file",
                r#"{"type":"object"}"#,
            )
            .expect("spec builds"),
        ])
        .expect("definitions attach");
    let events = provider(&base).stream(&request, &live_context());
    assert_eq!(finished(&events).reason(), FinishReason::Stop);
    let raw = served.request.lock().expect("request readable").clone();
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").expect("framed request");
    assert!(head.starts_with("POST /v1/chat/completions HTTP/1.1"));
    assert!(head.contains("authorization: Bearer "));
    let payload: serde_json::Value = serde_json::from_str(body).expect("JSON body");
    assert_eq!(payload["model"], "test-model");
    assert_eq!(payload["stream"], true);
    assert_eq!(payload["stream_options"]["include_usage"], true);
    assert_eq!(payload["max_tokens"], 1024);
    let messages = payload["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "do it");
    assert_eq!(messages[1]["tool_calls"][0]["id"], "prov-9");
    assert_eq!(
        messages[1]["tool_calls"][0]["function"]["arguments"],
        r#"{"path":"src"}"#
    );
    assert_eq!(messages[2]["role"], "tool");
    assert_eq!(messages[2]["tool_call_id"], "prov-9");
    assert_eq!(messages[2]["content"], "out");
    let tools = payload["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["function"]["name"], "host_read");
}

/// Serves one canned raw HTTP response head plus body, then closes. Used for
/// SSE (`text/event-stream`) and chunked-framing cases the JSON helper
/// cannot express.
fn serve_raw(head: &str, body: &str) -> (String, Served) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback binds");
    let port = listener.local_addr().expect("port known").port();
    let served = Served {
        hits: Arc::new(AtomicUsize::new(0)),
        request: Arc::new(Mutex::new(Vec::new())),
    };
    let captured = Served {
        hits: Arc::clone(&served.hits),
        request: Arc::clone(&served.request),
    };
    let head = head.to_owned();
    let body = body.to_owned();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let mut seen = Vec::new();
        let mut byte = [0u8; 1];
        while !seen.ends_with(b"\r\n\r\n") {
            match stream.read_exact(&mut byte) {
                Ok(()) => seen.push(byte[0]),
                Err(_) => break,
            }
            if seen.len() > 65_536 {
                break;
            }
        }
        let length: usize = String::from_utf8_lossy(&seen)
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
            .extend_from_slice(&seen);
        captured
            .request
            .lock()
            .expect("request log writable")
            .extend_from_slice(&rest);
        captured.hits.fetch_add(1, Ordering::SeqCst);
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body.as_bytes());
    });
    (format!("http://127.0.0.1:{port}/v1"), served)
}

fn sse_head(body_len: usize) -> String {
    format!(
        "HTTP/1.1 200 Test\r\ncontent-type: text/event-stream\r\ncontent-length: {body_len}\r\nconnection: close\r\n\r\n"
    )
}

#[test]
fn sse_text_fragments_concatenate_to_one_delta() {
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hello \"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"there\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2}}\n\ndata: [DONE]\n";
    let (base, served) = serve_raw(&sse_head(body.len()), body);
    let events = provider(&base).stream(&base_request(), &live_context());
    assert_eq!(served.hits.load(Ordering::SeqCst), 1);
    assert!(matches!(
        &events[0],
        ProviderEvent::TextDelta { item_key, text }
        if item_key == "item-0" && text == "hello there"
    ));
    let terminal = finished(&events);
    assert_eq!(terminal.reason(), FinishReason::Stop);
    assert_eq!(
        terminal.usage(),
        Usage::new(Some(4), Some(2), UsageFinality::Final)
    );
}

#[test]
fn sse_tool_fragments_assemble_per_index_before_admission() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-9\",\"function\":{\"name\":\"host_read\",\"arguments\":\"{\\\"path\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\":\\\"src\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n",
    );
    let (base, _) = serve_raw(&sse_head(body.len()), body);
    let events = provider(&base).stream(&base_request(), &live_context());
    let ready: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ToolCallReady(candidate) => Some(candidate),
            _ => None,
        })
        .collect();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].item_key(), "item-1");
    assert_eq!(ready[0].provider_ref(), "call-9");
    assert_eq!(finished(&events).reason(), FinishReason::ToolCalls);
}

#[test]
fn sse_identity_budget_counts_all_new_indices_in_one_record() {
    let calls: Vec<_> = (0..17)
        .map(|index| {
            serde_json::json!({
                "index": index,
                "id": format!("call-{index}"),
                "function": {"name": "host_read", "arguments": "{}"}
            })
        })
        .collect();
    let record = serde_json::json!({
        "choices": [{"delta": {"tool_calls": calls}, "finish_reason": "tool_calls"}]
    });
    let body = format!("data: {record}\n\ndata: [DONE]\n");
    let (base, _) = serve_raw(&sse_head(body.len()), &body);
    let error = failed(provider(&base).stream(&base_request(), &live_context()));
    assert_eq!(error.0, ErrorCategory::ResourceLimit);
}

#[test]
fn sse_without_termination_is_a_truncation_not_a_stop() {
    let body =
        "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n";
    let (base, _) = serve_raw(&sse_head(body.len()), body);
    let error = failed(provider(&base).stream(&base_request(), &live_context()));
    assert_eq!(error.0, ErrorCategory::Protocol);
}

#[test]
fn sse_claimed_tool_calls_without_items_still_violates_the_contract() {
    let body =
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n";
    let (base, _) = serve_raw(&sse_head(body.len()), body);
    let error = failed(provider(&base).stream(&base_request(), &live_context()));
    assert_eq!(error.0, ErrorCategory::Protocol);
}

#[test]
fn chunked_sse_streams_parse_after_dechunking() {
    let inner = "data: {\"choices\":[{\"delta\":{\"content\":\"ab\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"cd\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n";
    let framed = format!(
        "{:x}\r\n{}\r\n{:x}\r\n{}\r\n0\r\n\r\n",
        24,
        &inner[..24],
        inner.len() - 24,
        &inner[24..]
    );
    let head = "HTTP/1.1 200 Test\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n"
        .to_owned();
    let (base, _) = serve_raw(&head, &framed);
    let events = provider(&base).stream(&base_request(), &live_context());
    assert!(matches!(
        &events[0],
        ProviderEvent::TextDelta { item_key, text }
        if item_key == "item-0" && text == "abcd"
    ));
    assert_eq!(finished(&events).reason(), FinishReason::Stop);
}

#[test]
fn sse_accepts_code_responses_with_more_than_sixty_five_thousand_records() {
    let mut body = String::new();
    for _ in 0..70_000 {
        body.push_str(
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":null}]}\n\n",
        );
    }
    body.push_str("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n");
    body.push_str("data: [DONE]\n");
    let (base, _) = serve_raw(&sse_head(body.len()), &body);
    let events = provider(&base).stream(&base_request(), &live_context());
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text.len(), 70_000);
    assert_eq!(finished(&events).reason(), FinishReason::Stop);
}

#[test]
fn https_construction_accepts_explicit_and_default_ports() {
    assert!(
        OpenAiProvider::new("https://h/v1", present_credential(), "m")
            .expect("default TLS port builds")
            .uses_tls()
    );
    assert!(
        OpenAiProvider::new("https://h:8443/v1/", present_credential(), "m")
            .expect("explicit TLS port builds")
            .uses_tls()
    );
}

#[test]
fn https_unreachable_peers_fail_protocol_not_unsupported() {
    let provider = OpenAiProvider::new("https://127.0.0.1:9/", present_credential(), "m")
        .expect("https provider builds");
    let error = failed(provider.stream(&base_request(), &live_context()));
    assert_eq!(error.0, ErrorCategory::Protocol);
}

#[test]
fn incremental_sink_streams_text_and_usage_but_withholds_dispatch() {
    use std::sync::Mutex;
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hello \"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-9\",\"function\":{\"name\":\"host_read\",\"arguments\":\"{\\\"path\\\":\\\"src\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":7}}\n\n",
        "data: [DONE]\n",
    );
    let (base, _) = serve_raw(&sse_head(body.len()), body);
    let seen = Mutex::new(Vec::new());
    let sink = |event: ProviderEvent| {
        seen.lock().expect("sink log writable").push(event);
    };
    let batch = provider(&base).stream_with_sink(&base_request(), &live_context(), &sink);
    let seen = seen.lock().expect("sink log readable").clone();
    assert!(
        seen.iter().any(|event| matches!(
            event,
            ProviderEvent::TextDelta { text, .. } if text == "hello "
        )),
        "text fragments stream provisionally: {seen:?}"
    );
    assert!(
        seen.iter().any(|event| matches!(
            event,
            ProviderEvent::Usage(usage) if usage.input_tokens() == Some(5)
        )),
        "usage streams provisionally: {seen:?}"
    );
    assert!(
        !seen.iter().any(|event| matches!(
            event,
            ProviderEvent::ToolCallReady(_) | ProviderEvent::TurnFinished(_)
        )),
        "candidates and terminals never travel the sink: {seen:?}"
    );
    let ready = batch
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ToolCallReady(candidate) => Some(candidate),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(ready.len(), 1, "the batch stays authoritative: {batch:?}");
    assert_eq!(finished(&batch).reason(), FinishReason::ToolCalls);
}

#[test]
fn incremental_sink_sees_fragments_as_chunks_arrive() {
    use std::sync::Mutex;
    // One SSE line split across TCP segments: the live parser must still
    // emit the fragment once the line completes, not only at EOF.
    let full = "data: {\"choices\":[{\"delta\":{\"content\":\"abcdef\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n";
    let head = sse_head(full.len());
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("loopback binds");
    let port = listener.local_addr().expect("port known").port();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        use std::io::{Read, Write};
        stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let mut head_buf = Vec::new();
        let mut byte = [0u8; 1];
        while !head_buf.ends_with(b"\r\n\r\n") {
            match stream.read_exact(&mut byte) {
                Ok(()) => head_buf.push(byte[0]),
                Err(_) => break,
            }
            if head_buf.len() > 65_536 {
                break;
            }
        }
        let _ = stream.write_all(head.as_bytes());
        let raw = full.as_bytes();
        let _ = stream.write_all(&raw[..20]);
        std::thread::sleep(Duration::from_millis(50));
        let _ = stream.write_all(&raw[20..]);
    });
    let seen = Mutex::new(Vec::new());
    let sink = |event: ProviderEvent| {
        seen.lock().expect("sink log writable").push(event);
    };
    let provider = OpenAiProvider::new(
        &format!("http://127.0.0.1:{port}/v1"),
        present_credential(),
        "m",
    )
    .expect("provider builds");
    let batch = provider.stream_with_sink(&base_request(), &live_context(), &sink);
    let seen = seen.lock().expect("sink log readable").clone();
    assert!(
        seen.iter().any(|event| matches!(
            event,
            ProviderEvent::TextDelta { text, .. } if text == "abcdef"
        )),
        "split lines still stream: {seen:?}"
    );
    assert_eq!(finished(&batch).reason(), FinishReason::Stop);
}

/// Shape-only diagnostics name the failing field without echoing values.
fn failed_message(events: Vec<ProviderEvent>) -> (ErrorCategory, String) {
    assert_eq!(events.len(), 1, "failures are single events: {events:?}");
    match &events[0] {
        ProviderEvent::Failed(error) => (error.category(), error.message().to_owned()),
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[test]
fn malformed_tool_calls_report_which_shape_failed() {
    for (body, category, message) in [
        (
            r#"{"choices":[{"message":{"role":"assistant","tool_calls":[{"type":"function","function":{"name":"host_read","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
            ErrorCategory::InvalidInput,
            "provider reply tool call is missing an id",
        ),
        (
            r#"{"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"c","type":"function"}]},"finish_reason":"tool_calls"}]}"#,
            ErrorCategory::InvalidInput,
            "provider reply tool call is missing a function",
        ),
        (
            r#"{"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"c","type":"function","function":{"arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
            ErrorCategory::InvalidInput,
            "provider reply tool call name is invalid",
        ),
        (
            r#"{"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"c","type":"function","function":{"name":"host_read","arguments":{"path":"src"}}}]},"finish_reason":"tool_calls"}]}"#,
            ErrorCategory::InvalidInput,
            "provider reply tool call arguments are invalid",
        ),
        (
            r#"{"choices":[{"message":{"role":"assistant","tool_calls":{"id":"c"}},"finish_reason":"tool_calls"}]}"#,
            ErrorCategory::InvalidInput,
            "provider reply tool calls are invalid",
        ),
    ] {
        let (base, _) = serve(200, body, Duration::ZERO);
        let (actual_category, actual_message) =
            failed_message(provider(&base).stream(&base_request(), &live_context()));
        assert_eq!(actual_category, category, "{body}");
        assert_eq!(actual_message, message, "{body}");
        assert!(
            !actual_message.contains("host_read") && !actual_message.contains("src"),
            "diagnostics never echo vendor values: {actual_message}"
        );
    }
}

#[test]
fn sse_object_arguments_report_the_field_not_generic_json() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-9\",\"function\":{\"name\":\"host_read\",\"arguments\":{\"path\":\"src\"}}}]},\"finish_reason\":null}]}\n\n",
        "data: [DONE]\n",
    );
    let (base, _) = serve_raw(&sse_head(body.len()), body);
    let (category, message) =
        failed_message(provider(&base).stream(&base_request(), &live_context()));
    assert_eq!(category, ErrorCategory::Protocol);
    assert_eq!(message, "provider reply tool call arguments are invalid");
}

#[test]
fn sse_idless_fragments_report_the_missing_id() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"host_read\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n",
    );
    let (base, _) = serve_raw(&sse_head(body.len()), body);
    let (category, message) =
        failed_message(provider(&base).stream(&base_request(), &live_context()));
    assert_eq!(category, ErrorCategory::Protocol);
    assert_eq!(message, "provider reply tool call is missing an id");
}

#[test]
fn http_status_classes_point_in_different_directions() {
    for (status, category, message) in [
        (
            400,
            ErrorCategory::InvalidInput,
            "provider request was rejected",
        ),
        (
            422,
            ErrorCategory::InvalidInput,
            "provider request was rejected",
        ),
        (
            402,
            ErrorCategory::PermissionDenied,
            "provider payment is required",
        ),
    ] {
        let (base, _) = serve(status, r#"{"error":"nope"}"#, Duration::ZERO);
        let events = provider(&base).stream(&base_request(), &live_context());
        assert_eq!(events.len(), 1, "failures are single events: {events:?}");
        match &events[0] {
            ProviderEvent::Failed(error) => {
                assert_eq!(error.category(), category, "{status}");
                assert_eq!(error.message(), message, "{status}");
            }
            other => panic!("expected a failure for {status}, got {other:?}"),
        }
    }
}

#[test]
fn sse_reasoning_fragments_accumulate_withheld_from_sink() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think \",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"again\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n",
    );
    let (base, _) = serve_raw(&sse_head(body.len()), body);
    let events = provider(&base).stream(&base_request(), &live_context());
    let reasoning: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ReasoningDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, vec!["think again".to_owned()]);
    assert_eq!(finished(&events).reason(), FinishReason::Stop);
}

#[test]
fn ollama_style_reasoning_fields_feed_the_same_accumulator() {
    // SSE deltas.
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning\":\"think \",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning\":\"again\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n",
    );
    let (base, _) = serve_raw(&sse_head(body.len()), body);
    let events = provider(&base).stream(&base_request(), &live_context());
    let reasoning: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ReasoningDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, vec!["think again".to_owned()]);

    // Non-streaming message bodies.
    let body = r#"{"choices":[{"message":{"role":"assistant","content":"hi","reasoning":"because"},"finish_reason":"stop"}]}"#;
    let (base, _) = serve(200, body, Duration::ZERO);
    let events = provider(&base).stream(&base_request(), &live_context());
    let reasoning: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ReasoningDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, vec!["because".to_owned()]);
}

#[test]
fn json_reasoning_content_maps_to_a_reasoning_event() {
    let body = r#"{"choices":[{"message":{"role":"assistant","content":"hi","reasoning_content":"because"},"finish_reason":"stop"}]}"#;
    let (base, _) = serve(200, body, Duration::ZERO);
    let events = provider(&base).stream(&base_request(), &live_context());
    assert!(
        events.iter().any(|event| matches!(
            event,
            ProviderEvent::ReasoningDelta { text } if text == "because"
        )),
        "reasoning is recorded: {events:?}"
    );
    assert_eq!(finished(&events).reason(), FinishReason::Stop);
}

#[test]
fn request_echoes_recorded_reasoning_on_assistant_messages() {
    use nexus_core::{CallId, M0_REVISION, ModelContextItem, ToolId};
    let call = nexus_core::ToolCall::new(
        RunId::new("run-1").expect("valid"),
        nexus_core::TurnId::new("turn-1").expect("valid"),
        CallId::new("call-1").expect("valid"),
        ToolId::new("host_read", M0_REVISION).expect("valid"),
        nexus_core::NormalizedArgs::new(r#"{"path":"src"}"#).expect("valid args build"),
    );
    let request = base_request()
        .with_conversation(vec![
            ModelContextItem::assistant_text_with_reasoning(
                "item-0",
                "reading",
                Some("trace-a".to_owned()),
            )
            .expect("text builds"),
            ModelContextItem::assistant_call_with_reasoning(
                "item-1",
                "prov-9",
                call,
                Some("trace-a".to_owned()),
            )
            .expect("call builds"),
            result_for("call-1", "item-1", "prov-9"),
        ])
        .expect("conversation attaches");
    let (base, served) = serve(200, &stop_body("x"), Duration::ZERO);
    let _ = provider(&base).stream(&request, &live_context());
    let raw = served.request.lock().expect("request readable").clone();
    let text = String::from_utf8_lossy(&raw);
    let (_, body) = text.split_once("\r\n\r\n").expect("framed request");
    let payload: serde_json::Value = serde_json::from_str(body).expect("JSON body");
    let messages = payload["messages"].as_array().expect("messages");
    // One turn's text plus its call merge into a single assistant message.
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["reasoning_content"], "trace-a");
    assert_eq!(messages[0]["content"], "reading");
    assert_eq!(messages[0]["tool_calls"][0]["id"], "prov-9");
}

#[test]
fn consecutive_turn_calls_merge_per_turn_never_across_results() {
    use nexus_core::{CallId, M0_REVISION, ModelContextItem, ToolId};
    fn read_call(call: &str) -> nexus_core::ToolCall {
        nexus_core::ToolCall::new(
            RunId::new("run-1").expect("valid"),
            nexus_core::TurnId::new("turn-1").expect("valid"),
            CallId::new(call).expect("valid"),
            ToolId::new("host_read", M0_REVISION).expect("valid"),
            nexus_core::NormalizedArgs::new(r#"{"path":"src"}"#).expect("valid args build"),
        )
    }
    let request = base_request()
        .with_conversation(vec![
            ModelContextItem::assistant_text("item-0", "first").expect("text builds"),
            ModelContextItem::assistant_call("item-1", "prov-1", read_call("call-1"))
                .expect("call builds"),
            ModelContextItem::assistant_call("item-2", "prov-2", read_call("call-2"))
                .expect("call builds"),
            result_for("call-1", "item-1", "prov-1"),
            result_for("call-2", "item-2", "prov-2"),
        ])
        .expect("conversation attaches");
    let (base, served) = serve(200, &stop_body("x"), Duration::ZERO);
    let _ = provider(&base).stream(&request, &live_context());
    let raw = served.request.lock().expect("request readable").clone();
    let text = String::from_utf8_lossy(&raw);
    let (_, body) = text.split_once("\r\n\r\n").expect("framed request");
    let payload: serde_json::Value = serde_json::from_str(body).expect("JSON body");
    let messages = payload["messages"].as_array().expect("messages");
    // Text plus both calls of one turn: one assistant message, vendor
    // pairing rule satisfied (tool responses follow it directly).
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "assistant");
    assert_eq!(messages[0]["content"], "first");
    let calls = messages[0]["tool_calls"].as_array().expect("calls");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["id"], "prov-1");
    assert_eq!(calls[1]["id"], "prov-2");
}

#[test]
fn vendor_rejection_text_is_not_echoed_even_without_secret_markers() {
    let body = r#"{"error":{"message":"messages[3].reasoning_content is required in thinking mode","type":"invalid_request_error"}}"#;
    let (base, _) = serve(400, body, Duration::ZERO);
    let events = provider(&base).stream(&base_request(), &live_context());
    assert_eq!(events.len(), 1, "failures are single events: {events:?}");
    match &events[0] {
        ProviderEvent::Failed(error) => {
            assert_eq!(error.category(), ErrorCategory::InvalidInput);
            assert!(
                error
                    .correlation()
                    .iter()
                    .any(|(key, value)| key == "http_status" && value == "400")
            );
            assert_eq!(error.message(), "provider request was rejected");
        }
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[test]
fn vendor_rejection_without_usable_text_falls_back_to_static() {
    for body in [
        r#"{"error":{"message":"Rejected confidential prompt: customer medical record and /private/client-contract.txt"}}"#,
        r#"{"error":"nope"}"#,
        r#"{"error":{"message":""}}"#,
        r#"{"ok":true}"#,
        "not json at all",
    ] {
        let (base, _) = serve(422, body, Duration::ZERO);
        let events = provider(&base).stream(&base_request(), &live_context());
        assert_eq!(events.len(), 1, "failures are single events: {events:?}");
        match &events[0] {
            ProviderEvent::Failed(error) => {
                assert_eq!(error.category(), ErrorCategory::InvalidInput, "{body}");
                assert_eq!(error.message(), "provider request was rejected", "{body}");
            }
            other => panic!("expected a failure for {body}, got {other:?}"),
        }
    }
}

#[test]
fn malformed_conversation_pairing_is_rejected_before_network_io() {
    let call = ToolCall::new(
        RunId::new("run-1").unwrap(),
        TurnId::new("turn-1").unwrap(),
        CallId::new("call-1").unwrap(),
        ToolId::new("host_read", M0_REVISION).unwrap(),
        NormalizedArgs::new("{}").unwrap(),
    );
    let assistant = ModelContextItem::assistant_call("item-1", "prov-1", call).unwrap();
    let result = result_for("call-1", "item-1", "prov-1");
    for conversation in [
        vec![assistant.clone()],
        vec![result.clone(), assistant.clone()],
        vec![
            assistant.clone(),
            ModelContextItem::user_text("interrupt").unwrap(),
            result.clone(),
        ],
        vec![
            assistant.clone(),
            result_for("different", "item-1", "prov-1"),
        ],
        vec![
            assistant.clone(),
            result_for("call-1", "different", "prov-1"),
        ],
        vec![
            assistant.clone(),
            result_for("call-1", "item-1", "different"),
        ],
        vec![assistant.clone(), result.clone(), result.clone()],
        vec![assistant.clone(), assistant.clone(), result.clone()],
    ] {
        let request = base_request().with_conversation(conversation).unwrap();
        let (category, message) =
            failed_message(provider("http://127.0.0.1:9/v1").stream(&request, &live_context()));
        assert_eq!(category, ErrorCategory::Protocol);
        assert_eq!(
            message,
            "model conversation has invalid tool response ordering"
        );
    }
}

#[test]
fn sse_rejects_choice_payload_after_finish_and_unindexed_tool_fragments() {
    for body in [
        concat!(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"late\"}}]}\n\ndata: [DONE]\n"
        ),
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"c\",\"function\":{\"name\":\"host_read\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n",
    ] {
        let (base, _) = serve_raw(&sse_head(body.len()), body);
        assert_eq!(
            failed(provider(&base).stream(&base_request(), &live_context())).0,
            ErrorCategory::Protocol
        );
    }
}
