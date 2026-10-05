#![forbid(unsafe_code)]

//! Real adapter + real runtime, with only the vendor and tool I/O replaced.
//! The loopback peer captures actual JSON requests and fragments chunked SSE.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nexus_config::CredentialRef;
use nexus_core::*;
use nexus_openai::OpenAiProvider;
use nexus_runtime::{EventStreams, Policy, Runtime, RuntimeConfig};
use serde_json::{Value, json};

struct Mock {
    endpoint: String,
    requests: Arc<Mutex<Vec<Value>>>,
    worker: std::thread::JoinHandle<()>,
}

fn read_request(stream: &mut TcpStream) -> Value {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut head = Vec::new();
    let mut byte = [0];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
        assert!(head.len() < 16_384);
    }
    let head = String::from_utf8(head).unwrap();
    assert!(head.starts_with("POST /v1/chat/completions HTTP/1.1"));
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .unwrap()
        .parse::<usize>()
        .unwrap();
    assert!(length < 2_000_000);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn mock(replies: Vec<String>) -> Mock {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let worker = std::thread::spawn(move || {
        for reply in replies {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "expected local HTTP request");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            captured.lock().unwrap().push(read_request(&mut stream));
            stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n").unwrap();
            // Split inside JSON fields, argument escapes, and UTF-8 sequences.
            for chunk in reply.as_bytes().chunks(17) {
                stream
                    .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                    .unwrap();
                stream.write_all(chunk).unwrap();
                stream.write_all(b"\r\n").unwrap();
            }
            stream.write_all(b"0\r\n\r\n").unwrap();
        }
    });
    Mock {
        endpoint,
        requests,
        worker,
    }
}

fn chunk(delta: Value, finish: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({"choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    )
}

fn stop(text: &str) -> String {
    format!(
        "{}{}data: [DONE]\n\n",
        chunk(json!({"reasoning_content":"final trace"}), None),
        chunk(json!({"content":text}), Some("stop"))
    )
}

fn calls(entries: &[(&str, &str, &str)]) -> String {
    calls_with_content(entries, json!("Checking."))
}

fn calls_with_content(entries: &[(&str, &str, &str)], content: Value) -> String {
    let mut sse = chunk(
        json!({"role":"assistant","reasoning_content":"先 think "}),
        None,
    );
    sse += &chunk(
        json!({"reasoning_content":"then act","content":content}),
        None,
    );
    for (index, (id, name, args)) in entries.iter().enumerate() {
        let split = args.len() / 2; // Fixtures use ASCII arguments.
        sse += &chunk(
            json!({"tool_calls":[{"index":index,"id":id,"type":"function",
            "function":{"name":name,"arguments":&args[..split]}}]}),
            None,
        );
    }
    for (index, (_, _, args)) in entries.iter().enumerate().rev() {
        sse += &chunk(
            json!({"tool_calls":[{"index":index,
            "function":{"arguments":&args[args.len()/2..]}}]}),
            None,
        );
    }
    sse += &chunk(json!({}), Some("tool_calls"));
    sse += &format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":8}})
    );
    sse
}

struct LocalTool {
    name: &'static str,
    seen: Arc<Mutex<Vec<String>>>,
}

#[test]
fn translate_then_overwrite_followup_uses_history_and_real_approved_writes() {
    struct Root(std::path::PathBuf);
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let root = Root(std::env::temp_dir().join(format!(
        "nexus-conversation-files-{}-{}", std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
    )));
    std::fs::create_dir(&root.0).unwrap();
    std::fs::write(root.0.join("test.txt"), "hello").unwrap();
    let mock = mock(vec![
        calls(&[("read-original", "host_read", r#"{"path":"test.txt"}"#)]),
        calls(&[(
            "write-translation",
            "host_write",
            r#"{"path":"test_zh.txt","content":"\u4f60\u597d"}"#,
        )]),
        stop("Saved test_zh.txt without overwriting test.txt."),
        calls(&[("read-translation", "host_read", r#"{"path":"test_zh.txt"}"#)]),
        calls(&[(
            "overwrite-original",
            "host_write",
            r#"{"path":"test.txt","content":"\u4f60\u597d"}"#,
        )]),
        stop("Overwrote the original."),
    ]);
    test_rt().block_on(async {
        let provider = OpenAiProvider::new(&mock.endpoint,
            CredentialRef::env_var("PATH").unwrap(), "thinking-model").unwrap();
        let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![
            Arc::new(nexus_tools::ScopedReader::with_root(&root.0).unwrap()),
            Arc::new(nexus_tools::ScopedWriter::with_root(&root.0).unwrap()),
        ];
        let (runtime, mut streams) = Runtime::try_new(RuntimeConfig {
            limits: Limits::m0_test(), policy: Policy::m0_test(), has_approval_handler: true,
        }, Arc::new(provider), tools).unwrap();
        let mut approvals = 0;
        for (index, input) in ["translate", "覆盖原文"].into_iter().enumerate() {
            let command = SubmitCommand::new(
                RequestId::new(format!("req-files-{index}")).unwrap(),
                SessionId::new("files").unwrap(), input, "thinking-profile",
            ).unwrap();
            assert_eq!(runtime.submit(command).await.reply(), CommandReply::Accepted);
            let finished = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    tokio::select! {
                        event = streams.control.recv() => {
                            let event = event.unwrap();
                            match event.payload() {
                                EventPayload::ApprovalRequired(notice) => {
                                    approvals += 1;
                                    let reply = runtime.approve(nexus_core::ApproveCommand {
                                        request: RequestId::new(format!("approve-{approvals}")).unwrap(),
                                        approval: notice.approval.clone(), run: event.run().clone(),
                                        call: notice.call.clone(),
                                    }).await;
                                    assert_eq!(reply.reply(), CommandReply::Accepted);
                                }
                                EventPayload::RunFinished(finished) => break finished.clone(),
                                _ => {}
                            }
                        }
                        event = streams.data.recv() => { assert!(event.is_some()); }
                    }
                }
            }).await.expect("file run terminates");
            assert_eq!(finished.outcome(), RunOutcome::Completed);
            assert_eq!(std::fs::read_to_string(root.0.join("test_zh.txt")).unwrap(), "你好");
            let expected = if input == "translate" { "hello" } else { "你好" };
            assert_eq!(std::fs::read_to_string(root.0.join("test.txt")).unwrap(), expected);
        }
        assert_eq!(approvals, 2, "each real write needs its own exact grant");
    });
    mock.worker.join().unwrap();
    let requests = mock.requests.lock().unwrap();
    for request in requests.iter() {
        assert_pairing(request["messages"].as_array().unwrap());
    }
    let followup = requests[3]["messages"].as_array().unwrap();
    assert_eq!(followup[0]["content"], "translate");
    assert_eq!(
        followup[followup.len() - 2]["content"],
        "Saved test_zh.txt without overwriting test.txt."
    );
    assert_eq!(followup.last().unwrap()["content"], "覆盖原文");
    assert!(
        followup
            .iter()
            .any(|item| item["role"] == "tool" && item["content"] == "wrote 6 bytes")
    );
}

impl ToolPort for LocalTool {
    fn describe(&self) -> ToolSpec {
        ToolSpec::new(ToolId::new(self.name, M0_REVISION).unwrap(), "Local test operation",
            r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}"#).unwrap()
    }

    fn execute(&self, call: &ToolCall, _: &ToolContext) -> ToolOutcome {
        self.seen
            .lock()
            .unwrap()
            .push(call.args().as_str().to_owned());
        ToolOutcome::new(
            ExecutionStatus::Succeeded,
            EffectState::KnownNotApplied,
            Evidence::HostObserved,
            format!("observed {}", call.args().as_str()),
            false,
        )
        .unwrap()
    }
}

fn runtime(
    mock: &Mock,
    limits: Limits,
    approvals: bool,
) -> (Runtime, EventStreams, Arc<Mutex<Vec<String>>>) {
    // Existing fixture convention: resolve a nonempty local env var without
    // modifying process-global environment or recording its value.
    let provider = OpenAiProvider::new(
        &mock.endpoint,
        CredentialRef::env_var("PATH").unwrap(),
        "thinking-model",
    )
    .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = ["host_read", "host_write"]
        .into_iter()
        .map(|name| {
            Arc::new(LocalTool {
                name,
                seen: seen.clone(),
            }) as Arc<dyn ToolPort + Send + Sync>
        })
        .collect();
    let (runtime, streams) = Runtime::try_new(
        RuntimeConfig {
            limits,
            policy: Policy::m0_test(),
            has_approval_handler: approvals,
        },
        Arc::new(provider),
        tools,
    )
    .unwrap();
    (runtime, streams, seen)
}

fn submit(session: &str, input: &str) -> SubmitCommand {
    SubmitCommand::new(
        RequestId::new(format!("req-{input}")).unwrap(),
        SessionId::new(session).unwrap(),
        input,
        "thinking-profile",
    )
    .unwrap()
}

async fn finish(streams: &mut EventStreams) -> RunFinished {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            tokio::select! {
                event = streams.control.recv() => {
                    if let EventPayload::RunFinished(finished) = event.unwrap().payload() {
                        return finished.clone();
                    }
                }
                event = streams.data.recv() => { assert!(event.is_some()); }
            }
        }
    })
    .await
    .expect("local run terminates")
}

fn test_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

/// Independently enforce the vendor's correlation rule over actual wire JSON.
fn assert_pairing(messages: &[Value]) {
    let mut pending = std::collections::HashSet::new();
    for message in messages {
        match message["role"].as_str().unwrap() {
            "assistant" => {
                assert!(
                    pending.is_empty(),
                    "assistant before tool responses: {messages:?}"
                );
                if let Some(calls) = message["tool_calls"].as_array() {
                    assert_eq!(message["reasoning_content"], "先 think then act");
                    for call in calls {
                        assert!(pending.insert(call["id"].as_str().unwrap()));
                    }
                }
            }
            "tool" => {
                assert!(pending.remove(message["tool_call_id"].as_str().unwrap()));
            }
            "user" => assert!(pending.is_empty()),
            other => panic!("unexpected role {other}"),
        }
    }
    assert!(pending.is_empty(), "unanswered calls");
}

#[test]
fn thinking_multiple_calls_denials_followups_and_same_session_runs() {
    let mock = mock(vec![
        calls(&[
            ("read-a", "host_read", r#"{"path":"a"}"#),
            ("bad", "host_read", "[]"),
            ("unknown", "not_registered", "not-json"),
            ("write", "host_write", r#"{"path":"w"}"#),
            ("read-b", "host_read", r#"{"path":"b"}"#),
        ]),
        calls(&[("read-c", "host_read", r#"{"path":"c"}"#)]),
        stop("first answer"),
        stop("second answer"),
        stop("isolated answer"),
    ]);
    test_rt().block_on(async {
        let (runtime, mut streams, seen) = runtime(&mock, Limits::m0_test(), false);
        for (session, input) in [
            ("same", "first"),
            ("same", "followup"),
            ("other", "isolated"),
        ] {
            assert_eq!(
                runtime.submit(submit(session, input)).await.reply(),
                CommandReply::Accepted
            );
            assert_eq!(finish(&mut streams).await.outcome(), RunOutcome::Completed);
        }
        assert_eq!(
            *seen.lock().unwrap(),
            [r#"{"path":"a"}"#, r#"{"path":"b"}"#, r#"{"path":"c"}"#]
        );
    });
    mock.worker.join().unwrap();
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    for request in requests.iter() {
        assert_pairing(request["messages"].as_array().unwrap());
    }
    let second = requests[1]["messages"].as_array().unwrap();
    assert_eq!(
        second.len(),
        7,
        "one grouped assistant plus all five actual responses"
    );
    assert_eq!(second[1]["tool_calls"].as_array().unwrap().len(), 5);
    let results: std::collections::HashMap<_, _> = second
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| {
            (
                m["tool_call_id"].as_str().unwrap(),
                m["content"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(results["bad"], "invalid tool arguments");
    assert_eq!(results["unknown"], "unknown tool");
    assert_eq!(
        results["write"],
        "confirmation required and no approval handler exists"
    );
    assert_eq!(second[1]["tool_calls"][1]["function"]["arguments"], "[]");
    assert_eq!(
        second[1]["tool_calls"][2]["function"]["arguments"],
        "not-json"
    );
    let followup = requests[3]["messages"].as_array().unwrap();
    assert_eq!(followup[0]["content"], "first");
    assert_eq!(followup[followup.len() - 2]["content"], "first answer");
    assert_eq!(
        followup[followup.len() - 2]["reasoning_content"],
        "final trace"
    );
    assert_eq!(followup.last().unwrap()["content"], "followup");
    assert_eq!(
        requests[4]["messages"],
        json!([{"role":"user","content":"isolated"}])
    );
}

#[test]
fn reasoning_without_visible_text_survives_tool_rounds_and_successive_runs() {
    let thinking_only = format!(
        "{}data: [DONE]\n\n",
        chunk(
            json!({"reasoning_content":"silent completed trace"}),
            Some("stop")
        )
    );
    let mock = mock(vec![
        calls_with_content(&[("read", "host_read", r#"{"path":"a"}"#)], Value::Null),
        thinking_only,
        stop("followup"),
    ]);
    test_rt().block_on(async {
        let (runtime, mut streams, _) = runtime(&mock, Limits::m0_test(), false);
        for input in ["first", "next"] {
            runtime.submit(submit("s", input)).await;
            assert_eq!(finish(&mut streams).await.outcome(), RunOutcome::Completed);
        }
    });
    mock.worker.join().unwrap();
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests[1]["messages"][1]["content"], Value::Null);
    assert_eq!(
        requests[1]["messages"][1]["reasoning_content"],
        "先 think then act"
    );
    let next = requests[2]["messages"].as_array().unwrap();
    assert_pairing(next);
    assert_eq!(next[next.len() - 2]["content"], Value::Null);
    assert_eq!(
        next[next.len() - 2]["reasoning_content"],
        "silent completed trace"
    );
}

#[test]
fn cancelled_pending_multi_call_run_never_enters_next_request() {
    let mock = mock(vec![
        stop("kept"),
        calls(&[
            ("completed", "host_read", r#"{"path":"a"}"#),
            ("pending", "host_write", r#"{"path":"w"}"#),
            ("queued", "host_read", r#"{"path":"b"}"#),
        ]),
        stop("after cancel"),
    ]);
    test_rt().block_on(async {
        let (runtime, mut streams, seen) = runtime(&mock, Limits::m0_test(), true);
        runtime.submit(submit("s", "baseline")).await;
        assert_eq!(finish(&mut streams).await.outcome(), RunOutcome::Completed);
        let response = runtime.submit(submit("s", "cancelled")).await;
        let run = response.run().unwrap().clone();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = streams.control.recv().await.unwrap();
                if matches!(event.payload(), EventPayload::ApprovalRequired(_)) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        runtime
            .cancel(CancelCommand {
                request: RequestId::new("cancel").unwrap(),
                run,
            })
            .await;
        assert_eq!(finish(&mut streams).await.outcome(), RunOutcome::Cancelled);
        runtime.submit(submit("s", "next")).await;
        assert_eq!(finish(&mut streams).await.outcome(), RunOutcome::Completed);
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "cancelled pending/queued calls do not execute"
        );
    });
    mock.worker.join().unwrap();
    let requests = mock.requests.lock().unwrap();
    assert_eq!(
        requests[2]["messages"],
        json!([
            {"role":"user","content":"baseline"},
            {"role":"assistant","content":"kept","reasoning_content":"final trace"},
            {"role":"user","content":"next"}
        ])
    );
}

#[test]
fn failed_and_over_budget_runs_do_not_poison_retained_context() {
    let malformed = format!(
        "{}{}data: [DONE]\n\n",
        chunk(json!({"content":"partial"}), Some("stop")),
        chunk(json!({"reasoning_content":"late trace"}), None)
    );
    let oversized = format!(
        "{}data: [DONE]\n\n",
        chunk(
            json!({"reasoning_content":"x".repeat(65), "content":"x"}),
            Some("stop")
        )
    );
    let mock = mock(vec![stop("kept"), malformed, oversized, stop("next")]);
    test_rt().block_on(async {
        let mut limits = Limits::m0_test();
        limits.max_tool_output_bytes = 64;
        let (runtime, mut streams, _) = runtime(&mock, limits, false);
        for (input, expected) in [
            ("baseline", RunOutcome::Completed),
            ("failed", RunOutcome::Failed),
            ("over", RunOutcome::Failed),
            ("next", RunOutcome::Completed),
        ] {
            runtime.submit(submit("s", input)).await;
            assert_eq!(finish(&mut streams).await.outcome(), expected);
        }
    });
    mock.worker.join().unwrap();
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests[3]["messages"].as_array().unwrap().len(), 3);
    assert_eq!(requests[3]["messages"][0]["content"], "baseline");
    assert_eq!(requests[3]["messages"][2]["content"], "next");
}

#[test]
fn retention_evicts_whole_exchanges_and_reserves_space_for_new_input() {
    let mock = mock(vec![stop("one"), stop("two"), stop("three"), stop("four")]);
    test_rt().block_on(async {
        let mut limits = Limits::m0_test();
        limits.retained_context_items = 3;
        let (runtime, mut streams, _) = runtime(&mock, limits, false);
        for input in ["first", "second", "third", "fourth"] {
            runtime.submit(submit("s", input)).await;
            assert_eq!(finish(&mut streams).await.outcome(), RunOutcome::Completed);
        }
    });
    mock.worker.join().unwrap();
    let requests = mock.requests.lock().unwrap();
    assert_eq!(
        requests[3]["messages"],
        json!([
            {"role":"user","content":"third"},
            {"role":"assistant","content":"three","reasoning_content":"final trace"},
            {"role":"user","content":"fourth"}
        ])
    );
}

#[test]
fn inherited_history_yields_to_current_tool_round_without_splitting_pairs() {
    let mock = mock(vec![
        stop("baseline"),
        calls(&[
            ("a", "host_read", r#"{"path":"a"}"#),
            ("b", "host_read", r#"{"path":"b"}"#),
        ]),
        stop("tool answer"),
    ]);
    test_rt().block_on(async {
        let mut limits = Limits::m0_test();
        limits.retained_context_items = 7;
        let (runtime, mut streams, _) = runtime(&mock, limits, false);
        for input in ["baseline", "tools"] {
            runtime.submit(submit("s", input)).await;
            assert_eq!(finish(&mut streams).await.outcome(), RunOutcome::Completed);
        }
    });
    mock.worker.join().unwrap();
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests[1]["messages"][0]["content"], "baseline");
    let followup = requests[2]["messages"].as_array().unwrap();
    assert_eq!(followup[0]["content"], "tools");
    assert_eq!(followup.len(), 4);
    assert_pairing(followup);
}
