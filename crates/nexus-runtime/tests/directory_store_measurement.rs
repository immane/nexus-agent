#![forbid(unsafe_code)]

//! Offline end-to-end measurement, not a CI latency assertion.
//!
//! Run with `cargo test -p nexus-runtime --test directory_store_measurement
//! --locked --offline -- --ignored --nocapture`. Each sample starts with an
//! empty directory and one run: one fixed read call per model turn, followed
//! by a stop turn. Timings include submit, provider/tool doubles, runtime,
//! event draining and durable store writes, but not setup or cleanup. Compare
//! only runs on the same machine, filesystem, build profile and cache state.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use nexus_core::{
    CallCandidate, CommandReply, EffectState, EventPayload, Evidence, ExecutionStatus,
    FinishReason, Limits, M0_REVISION, ModelInstructions, ModelRequest, PersistenceState,
    ProviderCapabilities, ProviderContext, ProviderEvent, ProviderPort, RequestId, RunOutcome,
    SessionId, SubmitCommand, ToolCall, ToolContext, ToolId, ToolOutcome, ToolPort,
    ToolRegistration, ToolSpec, TurnFinished, Usage, UsageFinality,
};
use nexus_runtime::{Policy, Runtime, RuntimeConfig};
use nexus_session::DirectorySessionStore;

struct Provider {
    remaining: AtomicUsize,
    calls: AtomicUsize,
}

impl ProviderPort for Provider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            text: true,
            streaming: true,
            tool_calls: true,
            structured_output: false,
            usage_reporting: false,
            max_context_items: None,
            max_output_bytes: None,
        }
    }

    fn stream(&self, _: &ModelRequest, _: &ProviderContext) -> Vec<ProviderEvent> {
        let turn = self.calls.fetch_add(1, Ordering::SeqCst);
        let has_tool = self
            .remaining
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        let mut events = Vec::new();
        if has_tool {
            events.push(ProviderEvent::ToolCallReady(
                CallCandidate::new(
                    format!("item-{turn}"),
                    format!("ref-{turn}"),
                    "host_read",
                    r#"{"path":"src"}"#,
                )
                .expect("valid candidate"),
            ));
        } else {
            events.push(ProviderEvent::TextDelta {
                item_key: "final".to_owned(),
                text: "done".to_owned(),
            });
        }
        events.push(ProviderEvent::TurnFinished(TurnFinished::new(
            if has_tool {
                FinishReason::ToolCalls
            } else {
                FinishReason::Stop
            },
            Usage::new(None, None, UsageFinality::Final),
            None,
        )));
        events
    }
}

struct Tool {
    calls: AtomicUsize,
    spec: ToolSpec,
}

impl ToolPort for Tool {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn execute(&self, _: &ToolCall, _: &ToolContext) -> ToolOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        ToolOutcome::new(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            "ok",
            false,
        )
        .expect("valid outcome")
    }
}

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "nexus-runtime-measure-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create temporary directory: {error}"),
            }
        }
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            eprintln!("could not remove {}: {error}", self.0.display());
        }
    }
}

#[test]
#[ignore = "manual offline performance measurement; no wall-clock threshold"]
fn directory_store_full_runtime_scaling() {
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("executor");
    for count in [16_u32, 64, 256, 512] {
        let directory = TempDirectory::new();
        let store = DirectorySessionStore::open(directory.0.join("store")).expect("open store");
        let provider = Arc::new(Provider {
            remaining: AtomicUsize::new(count as usize),
            calls: AtomicUsize::new(0),
        });
        let tool = Arc::new(Tool {
            calls: AtomicUsize::new(0),
            spec: ToolSpec::new(
                ToolId::new("host_read", M0_REVISION).expect("tool id"),
                "fixed offline read",
                r#"{"type":"object"}"#,
            )
            .expect("tool spec"),
        });
        let mut limits = Limits::m0_test();
        limits.max_model_turns_per_run = count + 1;
        limits.max_tool_calls_per_run = count;
        limits.run_duration = Limits::M0_TEST_MAX_DURATION;
        let (runtime, mut streams) = Runtime::try_new_with_session_store(
            RuntimeConfig {
                limits,
                policy: Policy::m0_test(),
                has_approval_handler: false,
            },
            provider.clone(),
            vec![ToolRegistration::read(tool.clone())],
            ModelInstructions::default(),
            Box::new(store),
        )
        .expect("runtime wiring");
        let elapsed = executor.block_on(async {
            let start = Instant::now();
            let command = SubmitCommand::new(
                RequestId::new("measurement").expect("request id"),
                SessionId::new("measurement").expect("session id"),
                "run fixed reads",
                "measurement",
            )
            .expect("submit command");
            assert_eq!(
                runtime.submit(command).await.reply(),
                CommandReply::Accepted
            );
            let finished = tokio::time::timeout(Duration::from_secs(24 * 60 * 60), async {
                loop {
                    tokio::select! {
                        event = streams.data.recv() => {
                            assert!(event.is_some(), "data channel closed");
                        }
                        event = streams.control.recv() => {
                            let event = event.expect("control channel closed");
                            if let EventPayload::RunFinished(finished) = event.payload() {
                                break finished.clone();
                            }
                        }
                    }
                }
            })
            .await
            .expect("measurement timed out");
            let elapsed = start.elapsed();
            assert_eq!(
                finished.outcome(),
                RunOutcome::Completed,
                "run failed: {finished:?}"
            );
            assert_ne!(
                finished.persistence(),
                PersistenceState::SaveFailed,
                "persistence failed: {finished:?}"
            );
            elapsed
        });
        assert_eq!(tool.calls.load(Ordering::SeqCst), count as usize);
        assert_eq!(provider.calls.load(Ordering::SeqCst), count as usize + 1);
        println!(
            "calls={count} elapsed_ms={:.3} ms_per_call={:.3}",
            elapsed.as_secs_f64() * 1000.0,
            elapsed.as_secs_f64() * 1000.0 / f64::from(count)
        );
        drop(runtime);
    }
}
