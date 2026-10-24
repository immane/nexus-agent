#![forbid(unsafe_code)]

//! Cross-crate context reduction through real protocol encoders and loopback
//! HTTP, without a live model or external network.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nexus_config::{CredentialRef, ProtocolKind};
use nexus_core::{
    CommandReply, ContextManagementSettings, ContextWindow, ContextWindowSource, EventPayload,
    Limits, RequestId, RunOutcome, SessionId, SubmitCommand,
};
use nexus_openai::OpenAiProvider;
use nexus_runtime::{Policy, Runtime, RuntimeConfig};
use serde_json::{Value, json};

fn reply(protocol: ProtocolKind, text: &str) -> Value {
    match protocol {
        ProtocolKind::ChatCompletions => json!({
            "choices": [{"message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 3}
        }),
        ProtocolKind::OpenAiResponses => json!({
            "status": "completed", "output": [{"type": "message", "content": [{"type": "output_text", "text": text}]}],
            "usage": {"input_tokens": 10, "output_tokens": 3}
        }),
        ProtocolKind::AnthropicMessages => json!({
            "type": "message", "content": [{"type": "text", "text": text}], "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 3}
        }),
    }
}

fn serve(protocol: ProtocolKind) -> (String, std::thread::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let worker = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut requests = Vec::new();
        for text in [
            "old source history ".repeat(300),
            "old source history ".repeat(300),
            "old source history ".repeat(300),
            "Derived memory: previous work completed.".to_owned(),
            "continued".to_owned(),
            "follow-up completed".to_owned(),
        ] {
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("expected bounded protocol request: {error}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                assert!(header.len() < 65_536);
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
            }
            let length: usize = String::from_utf8(header)
                .unwrap()
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            assert!(length <= 2_097_152);
            let mut body = vec![0; length];
            socket.read_exact(&mut body).unwrap();
            requests.push(serde_json::from_slice(&body).unwrap());
            let response = reply(protocol, &text).to_string();
            write!(socket, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}", response.len(), response).unwrap();
        }
        requests
    });
    (format!("http://{address}/v1"), worker)
}

#[test]
fn summarized_history_continues_through_every_protocol() {
    for protocol in [
        ProtocolKind::ChatCompletions,
        ProtocolKind::OpenAiResponses,
        ProtocolKind::AnthropicMessages,
    ] {
        let (endpoint, worker) = serve(protocol);
        let provider = OpenAiProvider::new_for_protocol(
            &endpoint,
            CredentialRef::env_var("PATH").unwrap(),
            "fixture",
            protocol,
        )
        .unwrap()
        .with_context_management(ContextManagementSettings {
            window: Some(ContextWindow::new(8_192, ContextWindowSource::Configured).unwrap()),
            ..Default::default()
        });
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        executor.block_on(async {
            let (runtime, mut streams) = Runtime::try_new(RuntimeConfig {
                limits: Limits::m0_test(), policy: Policy::m0_test(), has_approval_handler: false,
            }, Arc::new(provider), vec![]).unwrap();
            for index in 0..5 {
                let command = SubmitCommand::new(RequestId::new(format!("request-{index}")).unwrap(), SessionId::new("session").unwrap(), "continue task", "fixture").unwrap();
                assert_eq!(runtime.submit(command).await.reply(), CommandReply::Accepted);
                let finished = tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        tokio::select! {
                            event = streams.control.recv() => {
                                if let EventPayload::RunFinished(finished) = event.unwrap().payload() { break finished.clone(); }
                            }
                            _ = streams.data.recv() => {}
                        }
                    }
                }).await.unwrap();
                assert_eq!(finished.outcome(), RunOutcome::Completed, "{protocol:?}: {finished:?}");
            }
        });
        let requests = worker.join().unwrap();
        assert_eq!(
            requests.len(),
            6,
            "three history-building answers, one summary, one continuation, one follow-up"
        );
        let summary = &requests[3];
        let summary_input = summary.to_string();
        assert!(
            summary_input.contains("old source history"),
            "summary must receive the history before it is replaced"
        );
        assert!(
            summary_input.to_ascii_lowercase().contains("summar"),
            "summary request must identify its purpose"
        );
        assert!(
            summary
                .get("tools")
                .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
        );
        let continued = requests[4].to_string();
        assert!(continued.contains("Derived memory: previous work completed."));
        assert!(!continued.contains("old source history"));
        assert!(continued.contains("continue task"));
        let follow_up = requests[5].to_string();
        assert!(
            follow_up.contains("Derived memory: previous work completed."),
            "derived memory must survive a later run in the same session"
        );
        assert!(!follow_up.contains("old source history"));
        let output_key = if protocol == ProtocolKind::OpenAiResponses {
            "max_output_tokens"
        } else {
            "max_tokens"
        };
        assert!(
            summary[output_key]
                .as_u64()
                .is_some_and(|tokens| tokens > 0 && tokens < 8_192)
        );
    }
}
