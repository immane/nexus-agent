#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use nexus_core::{
    ContextManagementSettings, ContextWindow, ContextWindowDiscovery, ContextWindowSource,
    EventPayload, FinishReason, Limits, ModelRequest, ModelRequestPurpose, ProviderCapabilities,
    ProviderContext, ProviderEvent, ProviderPort, RequestId, SessionId, SubmitCommand,
    TurnFinished, Usage, UsageFinality,
};
use nexus_runtime::transport::{ORDERED_EVENT_BUFFER_CAPACITY, OrderedEventConsumer};
use nexus_runtime::{Policy, Runtime, RuntimeConfig};
use nexus_tui::{AppState, Focus, render};
use ratatui::{Terminal, backend::TestBackend};

const WAIT: Duration = Duration::from_secs(10);
const WINDOW: u32 = 100_000;

struct DiscoveringProvider {
    started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Mutex<Option<mpsc::Receiver<()>>>,
}

impl ProviderPort for DiscoveringProvider {
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

    fn context_management_for_run(
        &self,
    ) -> (ContextManagementSettings, Option<ContextWindowDiscovery>) {
        let started = self.started.lock().unwrap().take().unwrap();
        let release = self.release.lock().unwrap().take().unwrap();
        (
            ContextManagementSettings::default(),
            Some(Box::new(move || {
                started.send(()).unwrap();
                release.recv_timeout(WAIT).unwrap();
                Some(ContextWindow::new(WINDOW, ContextWindowSource::ModelMetadata).unwrap())
            })),
        )
    }

    fn stream(&self, _: &ModelRequest, _: &ProviderContext) -> Vec<ProviderEvent> {
        vec![ProviderEvent::TurnFinished(TurnFinished::new(
            FinishReason::Stop,
            Usage::new(None, None, UsageFinality::Final),
            None,
        ))]
    }
}

#[tokio::test]
async fn discovered_window_reaches_state_and_footer() {
    let (started, started_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = mpsc::channel();
    let provider = Arc::new(DiscoveringProvider {
        started: Mutex::new(Some(started)),
        release: Mutex::new(Some(release_rx)),
    });
    let (runtime, mut streams) = Runtime::new(
        RuntimeConfig {
            limits: Limits::m0_test(),
            policy: Policy::m0_test(),
            has_approval_handler: false,
        },
        provider,
        vec![],
    );
    let reply = runtime
        .submit(
            SubmitCommand::new(
                RequestId::new("req-discovery").unwrap(),
                SessionId::new("sess-discovery").unwrap(),
                "test discovery",
                "test-profile",
            )
            .unwrap(),
        )
        .await;
    let mut merger = OrderedEventConsumer::new(ORDERED_EVENT_BUFFER_CAPACITY);
    merger.adopt(reply.run().expect("submit accepted"));
    let mut state = AppState::new();

    tokio::time::timeout(WAIT, started_rx)
        .await
        .unwrap()
        .unwrap();
    let event = tokio::time::timeout(WAIT, streams.control.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(event.payload(), EventPayload::RunStarted { .. }));
    for event in merger.push(event).unwrap() {
        assert!(state.apply_event(&event));
    }
    assert_eq!(state.context_estimate(), None);
    assert!(streams.control.try_recv().is_err());
    release.send(()).unwrap();

    let estimate = tokio::time::timeout(WAIT, async {
        loop {
            let event = tokio::select! {
                event = streams.control.recv() => event.expect("control channel open"),
                event = streams.data.recv() => event.expect("data channel open"),
            };
            for event in merger.push(event).unwrap() {
                let estimate = match event.payload() {
                    EventPayload::ModelRequestStarted {
                        purpose: ModelRequestPurpose::Task,
                        estimated_input_tokens: Some(tokens),
                        context_window_tokens: Some(window),
                        ..
                    } => Some((*tokens, *window)),
                    EventPayload::RunFinished(_) => panic!("run finished without task request"),
                    _ => None,
                };
                assert!(state.apply_event(&event));
                if let Some(estimate) = estimate {
                    return estimate;
                }
            }
        }
    })
    .await
    .expect("task request arrives after discovery");

    assert_eq!(estimate.1, WINDOW);
    assert_eq!(
        state.context_estimate(),
        Some((Some(estimate.0), Some(WINDOW)))
    );
    let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
    terminal
        .draw(|frame| {
            render(
                &mut state,
                frame.size(),
                frame.buffer_mut(),
                Focus::Viewport,
            )
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let footer: String = (0..buffer.area.width)
        .map(|x| buffer.get(x, buffer.area.height - 1).symbol())
        .collect();
    let expected = format!(
        "~{:.1}K ({}%)",
        estimate.0 as f64 / 1000.0,
        u128::from(estimate.0) * 100 / u128::from(WINDOW)
    );
    assert!(
        footer.contains(&expected),
        "footer {footer:?} lacks {expected:?}"
    );
}
