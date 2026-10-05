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
    CallId, CancellationToken, EffectState, ErrorCategory, Evidence, ExecutionStatus, FinishReason,
    M0_REVISION, ModelContextItem, ModelRequest, NormalizedArgs, ProviderContext, ProviderEvent,
    ProviderPort, RunId, ToolCall, ToolId, ToolOutcome, ToolSpec, TurnFinished, TurnId, Usage,
    UsageFinality,
};
use nexus_openai::OpenAiProvider;

/// Observed mock interaction: connection count plus the raw request bytes.
struct Served {
    hits: Arc<AtomicUsize>,
    request: Arc<Mutex<Vec<u8>>>,
}

/// Serves one canned HTTP response after an optional delay, then closes.
fn serve(status: u16, body: &str, delay: Duration) -> (String, Served) {
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
    std::thread::spawn(move || {
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
        (429, ErrorCategory::RateLimited),
        (500, ErrorCategory::Protocol),
        (503, ErrorCategory::Protocol),
    ] {
        let (base, _) = serve(status, r#"{"error":"nope"}"#, Duration::ZERO);
        let error = failed(provider(&base).stream(&base_request(), &live_context()));
        assert_eq!(error.0, category, "{status}");
        assert_eq!(error.1, nexus_core::RetryGuidance::DoNotRetry);
    }
    let (base, _) = serve(200, "not json{{{", Duration::ZERO);
    let error = failed(provider(&base).stream(&base_request(), &live_context()));
    assert_eq!(error.0, ErrorCategory::Protocol);
    let (base, _) = serve(200, r#"{"choices":[]}"#, Duration::ZERO);
    let error = failed(provider(&base).stream(&base_request(), &live_context()));
    assert_eq!(error.0, ErrorCategory::Protocol);
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
    let big = "x".repeat(1_200_000);
    let (base, _) = serve(200, &stop_body(&big), Duration::ZERO);
    let error = failed(provider(&base).stream(&base_request(), &live_context()));
    assert_eq!(error.0, ErrorCategory::ResourceLimit);
}

#[test]
fn unreachable_peers_fail_protocol() {
    let provider = OpenAiProvider::new("http://127.0.0.1:9/", present_credential(), "m")
        .expect("provider builds");
    let error = failed(provider.stream(&base_request(), &live_context()));
    assert_eq!(error.0, ErrorCategory::Protocol);
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
