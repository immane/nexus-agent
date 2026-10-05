//! Loopback HTTP server over the runtime command/event port.
//!
//! One [`Runtime`] per session: sessions never share runs, calls, or
//! approvals. Connection threads are plain blocking I/O; every runtime
//! interaction goes through the shared Tokio handle. Event receivers live
//! behind mutexes because `mpsc::Receiver` is `!Sync`; each session takes at
//! most one SSE subscriber at a time.

use std::collections::HashMap;
use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nexus_core::{
    ApproveCommand, CommandReply, DenyCommand, GetSnapshotCommand, ModelRequest,
    ProviderCapabilities, ProviderContext, ProviderEvent, ProviderPort, RequestId, RunId,
    SessionId, SubmitCommand,
};
use nexus_fakes::{FakeProvider, FakeTool};
use nexus_runtime::{EventStreams, Policy, Runtime, RuntimeConfig};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::http::{self, Request, Response};
use crate::json;

/// Idle SSE window: a `: ping` comment keeps the stream alive.
const SSE_IDLE: Duration = Duration::from_secs(15);
/// Default execution profile for web-submitted tasks.
const WEB_PROFILE: &str = "web-test";

/// Test-only demo provider: serves a fresh scripted demo script for every
/// run. The shared [`FakeProvider`] consumes its script queue across calls,
/// so without a reset the second task in one session would observe an
/// exhausted script and fail instantly. Real adapters never replay.
struct PerRunProvider {
    capabilities: ProviderCapabilities,
    adapter_label: String,
    scope_label: String,
    current: Mutex<PerRunState>,
}

struct PerRunState {
    run: Option<RunId>,
    provider: FakeProvider,
}

impl PerRunProvider {
    fn new() -> Self {
        let probe = FakeProvider::demo_two_turn();
        Self {
            capabilities: probe.capabilities(),
            adapter_label: probe.adapter_identity().to_owned(),
            scope_label: probe.continuation_scope(WEB_PROFILE),
            current: Mutex::new(PerRunState {
                run: None,
                provider: FakeProvider::demo_two_turn(),
            }),
        }
    }
}

impl ProviderPort for PerRunProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities.clone()
    }

    fn adapter_identity(&self) -> &str {
        &self.adapter_label
    }

    fn continuation_scope(&self, _profile: &str) -> String {
        self.scope_label.clone()
    }

    fn stream(&self, request: &ModelRequest, context: &ProviderContext) -> Vec<ProviderEvent> {
        let mut current = self.current.lock().expect("demo provider lockable");
        if current.run.as_ref() != Some(request.run()) {
            current.run = Some(request.run().clone());
            current.provider = FakeProvider::demo_two_turn();
        }
        current.provider.stream(request, context)
    }
}

/// One web session: an owned runtime plus its unconsumed event receivers.
pub struct Session {
    session: SessionId,
    runtime: Mutex<Runtime>,
    data: Mutex<mpsc::Receiver<nexus_core::RunEvent>>,
    control: Mutex<mpsc::Receiver<nexus_core::RunEvent>>,
    streaming: Mutex<bool>,
    next_request: AtomicU64,
}

impl Session {
    /// Builds the next correlation identity for this session.
    fn request_id(&self) -> RequestId {
        let n = self.next_request.fetch_add(1, Ordering::SeqCst);
        RequestId::new(format!("req-web-{n}")).expect("counter request id is valid")
    }
}

/// Shared server state: the Tokio handle driving runtime ports plus every
/// live session. Bound to loopback by the binary; there is no auth.
pub struct Server {
    handle: tokio::runtime::Handle,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    next_session: AtomicU64,
}

impl Server {
    /// Creates shared state around the Tokio handle used for `block_on`.
    #[must_use]
    pub fn new(handle: tokio::runtime::Handle) -> Self {
        Self {
            handle,
            sessions: Mutex::new(HashMap::new()),
            next_session: AtomicU64::new(1),
        }
    }

    /// Creates a session with a fresh demo-wired runtime and returns its
    /// token. Wiring failures are startup-class errors, never hung runs.
    pub fn create_session(&self) -> Result<String, nexus_core::AgentError> {
        let n = self.next_session.fetch_add(1, Ordering::SeqCst);
        let token = format!("sess-web-{n}");
        let session = SessionId::new(&token).expect("counter session id is valid");
        let config = RuntimeConfig {
            limits: nexus_core::Limits::m0_test(),
            policy: Policy::m0_test(),
            has_approval_handler: true,
        };
        let provider = Arc::new(PerRunProvider::new());
        let tools: Vec<Arc<dyn nexus_core::ToolPort + Send + Sync>> = vec![
            Arc::new(FakeTool::read_only()),
            Arc::new(FakeTool::mutation()),
        ];
        let (runtime, streams): (Runtime, EventStreams) =
            Runtime::try_new(config, provider, tools)?;
        let entry = Arc::new(Session {
            session,
            runtime: Mutex::new(runtime),
            data: Mutex::new(streams.data),
            control: Mutex::new(streams.control),
            streaming: Mutex::new(false),
            next_request: AtomicU64::new(1),
        });
        self.sessions
            .lock()
            .expect("sessions lockable")
            .insert(token.clone(), entry);
        Ok(token)
    }

    /// Looks up a session by token.
    fn session(&self, token: &str) -> Option<Arc<Session>> {
        self.sessions
            .lock()
            .expect("sessions lockable")
            .get(token)
            .cloned()
    }

    /// Serves one connection to completion, then returns for the caller to
    /// close the stream (except SSE, which closes its own stream at the
    /// terminal event).
    pub fn handle_connection(&self, mut stream: TcpStream) {
        let response = match http::read_request(&mut stream) {
            Ok(request) => self.route(&request, &mut stream),
            Err(error) => Some(json_response(
                error.status,
                &json::error_body(error.message),
            )),
        };
        if let Some(response) = response {
            let _ = http::write_response(&mut stream, &response);
        }
    }

    /// Routes a parsed request. Returns `None` when the SSE loop took over
    /// the stream (headers already written); anything else is framed here.
    fn route(&self, request: &Request, stream: &mut TcpStream) -> Option<Response> {
        let parsed: Value = if request.body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&request.body).unwrap_or(Value::Null)
        };
        let body = &parsed;
        match route_path(&request.method, &request.path) {
            Route::Health => Some(json_response(200, &body_json(crate::json::health_json()))),
            Route::CreateSession => match self.create_session() {
                Ok(token) => Some(json_response(
                    201,
                    &body_json(serde_json::json!({ "session": token })),
                )),
                Err(_) => Some(json_response(
                    500,
                    &json::error_body("demo wiring is invalid"),
                )),
            },
            Route::SubmitRun { session } => {
                self.with_session(session, |entry| self.submit_run(&entry, body))
            }
            Route::Snapshot { session } => self.with_session(session, |entry| {
                let run = request.query("run").unwrap_or("");
                self.snapshot(&entry, run)
            }),
            Route::Cancel { session, run } => {
                self.with_session(session, |entry| self.cancel(&entry, run))
            }
            Route::Decide {
                session,
                run,
                decision,
            } => self.with_session(session, |entry| self.decide(&entry, run, decision, body)),
            Route::Events { session, run } => match self.session(session) {
                // The stream loop owns the connection on success (`None`)
                // and closes it; a refusal (400/409) must still be framed.
                Some(entry) => self.stream_events(&entry, run, stream),
                None => Some(json_response(404, &json::error_body("unknown session"))),
            },
            Route::NotFound => Some(json_response(404, &json::error_body("unknown route"))),
        }
    }

    /// Runs a session-scoped handler or reports the missing session.
    fn with_session(
        &self,
        token: &str,
        handler: impl FnOnce(Arc<Session>) -> Option<Response>,
    ) -> Option<Response> {
        match self.session(token) {
            Some(entry) => handler(entry),
            None => Some(json_response(404, &json::error_body("unknown session"))),
        }
    }

    /// Accepts a task for the session. Only `Accepted` mints a run; every
    /// other reply keeps the slot untouched.
    fn submit_run(&self, session: &Arc<Session>, body: &Value) -> Option<Response> {
        let input = body_str(body, "input").unwrap_or("");
        let profile = body_str(body, "profile").unwrap_or(WEB_PROFILE);
        let command = match SubmitCommand::new(
            session.request_id(),
            session.session.clone(),
            input,
            profile,
        ) {
            Ok(command) => command,
            Err(_) => {
                return Some(json_response(
                    400,
                    &json::error_body("submit input is invalid"),
                ));
            }
        };
        let runtime = session.runtime.lock().expect("runtime lockable");
        let reply = self.handle.block_on(runtime.submit(command));
        let status = match reply.reply() {
            CommandReply::Accepted => 201,
            _ => crate::json::reply_status(reply.reply()),
        };
        let body = match reply.run() {
            Some(run) => {
                serde_json::json!({ "reply": crate::json::reply_name(reply.reply()), "run": run.as_str() })
            }
            None => serde_json::json!({ "reply": crate::json::reply_name(reply.reply()) }),
        };
        Some(json_response(status, &body_json(body)))
    }

    /// Returns the bounded snapshot for a known run of this session.
    fn snapshot(&self, session: &Arc<Session>, run: &str) -> Option<Response> {
        let run = match RunId::new(run) {
            Ok(run) => run,
            Err(_) => return Some(json_response(400, &json::error_body("run id is invalid"))),
        };
        let command = GetSnapshotCommand {
            request: session.request_id(),
            run,
        };
        let runtime = session.runtime.lock().expect("runtime lockable");
        let (response, snapshot) = self.handle.block_on(runtime.get_snapshot(command));
        match (response.reply(), snapshot) {
            (CommandReply::Accepted, Some(snapshot)) => Some(json_response(
                200,
                &body_json(crate::json::snapshot_json(&snapshot)),
            )),
            (reply, _) => Some(json_response(
                crate::json::reply_status(reply),
                &body_json(serde_json::json!({ "reply": crate::json::reply_name(reply) })),
            )),
        }
    }

    /// Cancels the named run of this session.
    fn cancel(&self, session: &Arc<Session>, run: &str) -> Option<Response> {
        let run = match RunId::new(run) {
            Ok(run) => run,
            Err(_) => return Some(json_response(400, &json::error_body("run id is invalid"))),
        };
        let command = nexus_core::CancelCommand {
            request: session.request_id(),
            run,
        };
        let runtime = session.runtime.lock().expect("runtime lockable");
        let reply = self.handle.block_on(runtime.cancel(command));
        Some(command_reply(reply))
    }

    /// Applies an approval decision with the exact runtime identity from
    /// the `approval-required` event. Nothing is inferred: the grant and
    /// call must name the live tuple or the runtime refuses them.
    fn decide(
        &self,
        session: &Arc<Session>,
        run: &str,
        decision: Decision,
        body: &Value,
    ) -> Option<Response> {
        let run = match RunId::new(run) {
            Ok(run) => run,
            Err(_) => return Some(json_response(400, &json::error_body("run id is invalid"))),
        };
        let approval = match body_str(body, "approval").map(nexus_core::ApprovalId::new) {
            Some(Ok(approval)) => approval,
            _ => {
                return Some(json_response(
                    400,
                    &json::error_body("approval identity is invalid"),
                ));
            }
        };
        let call = match body_str(body, "call").map(nexus_core::CallId::new) {
            Some(Ok(call)) => call,
            _ => {
                return Some(json_response(
                    400,
                    &json::error_body("call identity is invalid"),
                ));
            }
        };
        let runtime = session.runtime.lock().expect("runtime lockable");
        let reply = match decision {
            Decision::Approve => {
                let command = ApproveCommand {
                    request: session.request_id(),
                    approval,
                    run,
                    call,
                };
                self.handle.block_on(runtime.approve(command))
            }
            Decision::Deny => {
                let command = DenyCommand {
                    request: session.request_id(),
                    approval,
                    run,
                    call,
                };
                self.handle.block_on(runtime.deny(command))
            }
        };
        Some(command_reply(reply))
    }

    /// Streams this run's events as SSE until its terminal event, then
    /// closes. Only events owned by the requested run are forwarded; one
    /// subscriber per session at a time.
    fn stream_events(
        &self,
        session: &Arc<Session>,
        run: &str,
        stream: &mut TcpStream,
    ) -> Option<Response> {
        let run = match RunId::new(run) {
            Ok(run) => run,
            Err(_) => return Some(json_response(400, &json::error_body("run id is invalid"))),
        };
        {
            let mut streaming = session.streaming.lock().expect("stream flag lockable");
            if *streaming {
                return Some(json_response(
                    409,
                    &json::error_body("event stream already attached"),
                ));
            }
            *streaming = true;
        }
        if http::write_sse_headers(stream).is_err() {
            *session.streaming.lock().expect("stream flag lockable") = false;
            return None;
        }
        let mut data = session.data.lock().expect("data channel lockable");
        let mut control = session.control.lock().expect("control channel lockable");
        loop {
            let next = self.handle.block_on(async {
                tokio::time::timeout(SSE_IDLE, async {
                    tokio::select! {
                        event = data.recv() => event.map(StreamSide::Data),
                        event = control.recv() => event.map(StreamSide::Control),
                    }
                })
                .await
            });
            let event = match next {
                Ok(Some(StreamSide::Data(event))) | Ok(Some(StreamSide::Control(event))) => event,
                Ok(None) => break,
                Err(_) => {
                    if write!(stream, ": ping\n\n").is_err() || stream.flush().is_err() {
                        break;
                    }
                    continue;
                }
            };
            if event.run() != &run {
                continue;
            }
            let terminal = event.is_terminal();
            let frame = format!(
                "id: {}\ndata: {}\n\n",
                event.seq(),
                crate::json::event_json(&event)
            );
            if stream.write_all(frame.as_bytes()).is_err() || stream.flush().is_err() {
                break;
            }
            if terminal {
                break;
            }
        }
        *session.streaming.lock().expect("stream flag lockable") = false;
        None
    }
}

/// Data or control origin of a received event (kept only to satisfy the
/// `select!` arms; both sides forward identically).
enum StreamSide {
    Data(nexus_core::RunEvent),
    Control(nexus_core::RunEvent),
}

/// Approve or deny branch of the decision route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Grant the exact live call once.
    Approve,
    /// Refuse the live call without executing it.
    Deny,
}

/// Pure route match over method plus path segments.
#[derive(Debug, PartialEq, Eq)]
enum Route<'a> {
    /// `GET /health`.
    Health,
    /// `POST /sessions`.
    CreateSession,
    /// `POST /sessions/{sid}/runs`.
    SubmitRun { session: &'a str },
    /// `GET /sessions/{sid}/snapshot?run=<rid>`.
    Snapshot { session: &'a str },
    /// `POST /sessions/{sid}/runs/{rid}/cancel`.
    Cancel { session: &'a str, run: &'a str },
    /// `POST .../approve` or `/deny` with the exact grant identity.
    Decide {
        session: &'a str,
        run: &'a str,
        decision: Decision,
    },
    /// `GET /sessions/{sid}/runs/{rid}/events`.
    Events { session: &'a str, run: &'a str },
    /// Anything else.
    NotFound,
}

/// Matches a method plus route path to a [`Route`]. Any `?query` suffix
/// is stripped before matching; handlers read it separately.
fn route_path<'a>(method: &str, path: &'a str) -> Route<'a> {
    let path = path.split('?').next().unwrap_or(path);
    let segments: Vec<&str> = path.split('/').collect();
    match (method, segments.as_slice()) {
        ("GET", ["", "health"]) => Route::Health,
        ("POST", ["", "sessions"]) => Route::CreateSession,
        ("POST", ["", "sessions", session, "runs"]) => Route::SubmitRun { session },
        ("GET", ["", "sessions", session, "snapshot"]) => Route::Snapshot { session },
        ("POST", ["", "sessions", session, "runs", run, "cancel"]) => {
            Route::Cancel { session, run }
        }
        ("POST", ["", "sessions", session, "runs", run, "approve"]) => Route::Decide {
            session,
            run,
            decision: Decision::Approve,
        },
        ("POST", ["", "sessions", session, "runs", run, "deny"]) => Route::Decide {
            session,
            run,
            decision: Decision::Deny,
        },
        ("GET", ["", "sessions", session, "runs", run, "events"]) => Route::Events { session, run },
        _ => Route::NotFound,
    }
}

/// Extracts one string field from a JSON object body.
fn body_str<'a>(body: &'a Value, key: &str) -> Option<&'a str> {
    body.get(key)?.as_str()
}

/// Builds a runtime reply body with its HTTP status.
fn command_reply(reply: nexus_core::CommandResponse) -> Response {
    let status = crate::json::reply_status(reply.reply());
    let body = match reply.run() {
        Some(run) => {
            serde_json::json!({ "reply": crate::json::reply_name(reply.reply()), "run": run.as_str() })
        }
        None => serde_json::json!({ "reply": crate::json::reply_name(reply.reply()) }),
    };
    json_response(status, &body_json(body))
}

/// Serializes a JSON body.
fn body_json(body: Value) -> Vec<u8> {
    serde_json::to_vec(&body).expect("API body encodes")
}
/// Frames a JSON response.
fn json_response(status: u16, body: &[u8]) -> Response {
    Response::new(status, "application/json", body.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::{ErrorCategory, FinishReason, TurnId};

    /// Live demo context: a 60s elapsed reading, never cancelled, no
    /// credential, so the served script is never short-circuited.
    fn live_context() -> ProviderContext {
        ProviderContext::new(Duration::from_secs(60), false, None)
    }

    /// One demo request for `run`/`turn` with no tools and no continuation.
    fn request(run: &str, turn: &str) -> ModelRequest {
        ModelRequest::new(
            RunId::new(run).expect("valid"),
            TurnId::new(turn).expect("valid"),
            WEB_PROFILE,
            vec![],
            None,
            1024,
        )
        .expect("valid request builds")
    }

    /// Returns the batch's single terminal event, asserting it is last: a
    /// script that continued past its terminal would be a fake-contract bug,
    /// not a wrapper behavior.
    fn terminal(events: &[ProviderEvent]) -> &ProviderEvent {
        assert!(!events.is_empty(), "every invocation returns a terminal");
        let terminals: Vec<usize> = events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.is_terminal())
            .map(|(index, _)| index)
            .collect();
        assert_eq!(terminals.len(), 1, "exactly one terminal event");
        assert_eq!(terminals[0], events.len() - 1, "the terminal event is last");
        &events[terminals[0]]
    }

    /// Provider references proposed by one invocation, in declared order.
    fn ready_refs(events: &[ProviderEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::ToolCallReady(candidate) => Some(candidate.provider_ref()),
                _ => None,
            })
            .collect()
    }

    /// Locks the live per-run state. The inner fake is observable only from
    /// inside this module, and its call counter plus request log are what
    /// prove whether the shared script survived or was replaced.
    fn state(provider: &PerRunProvider) -> std::sync::MutexGuard<'_, PerRunState> {
        provider.current.lock().expect("demo provider lockable")
    }

    #[test]
    fn route_table_matches_every_documented_path() {
        assert_eq!(route_path("GET", "/health"), Route::Health);
        assert_eq!(route_path("POST", "/sessions"), Route::CreateSession);
        assert_eq!(
            route_path("POST", "/sessions/sess-web-1/runs"),
            Route::SubmitRun {
                session: "sess-web-1"
            }
        );
        assert_eq!(
            route_path("GET", "/sessions/sess-web-1/snapshot?run=r1"),
            Route::Snapshot {
                session: "sess-web-1"
            }
        );
        assert_eq!(
            route_path("POST", "/sessions/s/runs/r/cancel"),
            Route::Cancel {
                session: "s",
                run: "r"
            }
        );
        assert_eq!(
            route_path("POST", "/sessions/s/runs/r/approve"),
            Route::Decide {
                session: "s",
                run: "r",
                decision: Decision::Approve,
            }
        );
        assert_eq!(
            route_path("POST", "/sessions/s/runs/r/deny"),
            Route::Decide {
                session: "s",
                run: "r",
                decision: Decision::Deny,
            }
        );
        assert_eq!(
            route_path("GET", "/sessions/s/runs/r/events"),
            Route::Events {
                session: "s",
                run: "r"
            }
        );
    }

    #[test]
    fn unknown_methods_paths_and_shapes_miss() {
        for (method, path) in [
            ("DELETE", "/health"),
            ("GET", "/sessions"),
            ("POST", "/health"),
            ("GET", "/sessions/s/runs"),
            ("POST", "/sessions/s/snapshot"),
            ("GET", "/sessions/s/runs/r/cancel"),
            ("POST", "/sessions/s/runs/r/events"),
            ("GET", "/"),
            ("GET", "/sessions/s/runs/r/approve"),
            ("POST", "/sessions/s/runs/r/events/extra"),
        ] {
            assert_eq!(route_path(method, path), Route::NotFound, "{method} {path}");
        }
    }

    #[test]
    fn reply_status_mapping_is_total() {
        assert_eq!(crate::json::reply_status(CommandReply::Accepted), 200);
        assert_eq!(crate::json::reply_status(CommandReply::Busy), 409);
        assert_eq!(crate::json::reply_status(CommandReply::Rejected), 400);
        assert_eq!(
            crate::json::reply_status(CommandReply::StaleOrUnknownTarget),
            404
        );
        assert_eq!(
            crate::json::reply_status(CommandReply::AlreadyFinalized),
            409
        );
    }

    /// The wrapper's whole reason for existing: one shared fake serves many
    /// runs, so a run that keeps its identity keeps the shared script queue.
    /// Call one is the tool turn, call two is the stop turn, and call three is
    /// the fake's explicit exhausted failure — never a silent replay of the
    /// tool turn, which would re-propose calls after the run already ended.
    #[test]
    fn same_run_reuses_the_script_queue_until_it_exhausts() {
        let provider = PerRunProvider::new();

        let first = provider.stream(&request("run-same", "turn-1"), &live_context());
        assert_eq!(ready_refs(&first), vec!["prov-ref-1", "prov-ref-2"]);
        assert!(matches!(
            terminal(&first),
            ProviderEvent::TurnFinished(finished) if finished.reason() == FinishReason::ToolCalls
        ));

        let second = provider.stream(&request("run-same", "turn-2"), &live_context());
        assert!(
            ready_refs(&second).is_empty(),
            "the stop turn proposes no call, so the second turn cannot be the tool turn again"
        );
        assert!(matches!(
            terminal(&second),
            ProviderEvent::TurnFinished(finished) if finished.reason() == FinishReason::Stop
        ));

        let third = provider.stream(&request("run-same", "turn-3"), &live_context());
        assert_eq!(
            third.len(),
            1,
            "an exhausted script yields only its terminal failure"
        );
        // The wrapper is ungated and uncancelled, so a lone `Protocol`
        // failure is the fake's exhausted script, not a cancellation or a
        // gate that was never released.
        assert!(matches!(
            terminal(&third),
            ProviderEvent::Failed(error) if error.category() == ErrorCategory::Protocol
        ));

        // All three calls reached one live fake: a per-run reset would have
        // left the replacement with a single recorded call.
        let state = state(&provider);
        assert_eq!(state.run.as_ref().map(RunId::as_str), Some("run-same"));
        assert_eq!(state.provider.call_count(), 3);
        let seen = state.provider.requests();
        let runs: Vec<&str> = seen.iter().map(|seen| seen.run().as_str()).collect();
        assert_eq!(runs, vec!["run-same", "run-same", "run-same"]);
    }

    /// A run change swaps in a whole fresh demo script, which is what keeps
    /// the second task in one session from failing instantly on the shared
    /// fake's exhausted script.
    #[test]
    fn different_run_resets_to_a_fresh_whole_script() {
        let provider = PerRunProvider::new();

        let first = provider.stream(&request("run-a", "turn-1"), &live_context());
        assert_eq!(ready_refs(&first), vec!["prov-ref-1", "prov-ref-2"]);
        let stop_a = provider.stream(&request("run-a", "turn-2"), &live_context());
        assert!(matches!(
            terminal(&stop_a),
            ProviderEvent::TurnFinished(finished) if finished.reason() == FinishReason::Stop
        ));
        let exhausted = provider.stream(&request("run-a", "turn-3"), &live_context());
        assert!(matches!(terminal(&exhausted), ProviderEvent::Failed(_)));

        let reset = provider.stream(&request("run-b", "turn-1"), &live_context());
        assert_eq!(
            ready_refs(&reset),
            vec!["prov-ref-1", "prov-ref-2"],
            "a new run serves the tool turn again instead of an exhausted failure"
        );
        assert!(matches!(
            terminal(&reset),
            ProviderEvent::TurnFinished(finished) if finished.reason() == FinishReason::ToolCalls
        ));

        // The replacement is a whole script, not just its first entry: the
        // second turn of the new run still completes with the stop turn.
        let stop = provider.stream(&request("run-b", "turn-2"), &live_context());
        assert!(matches!(
            terminal(&stop),
            ProviderEvent::TurnFinished(finished) if finished.reason() == FinishReason::Stop
        ));

        // Only the last run is remembered, so returning to an earlier run
        // resets again instead of resuming its exhausted script.
        let returned = provider.stream(&request("run-a", "turn-4"), &live_context());
        assert_eq!(ready_refs(&returned), vec!["prov-ref-1", "prov-ref-2"]);

        let state = state(&provider);
        assert_eq!(state.run.as_ref().map(RunId::as_str), Some("run-a"));
        // Three fakes served this sequence: two were replaced wholesale, so the
        // live one has seen only its own single call. A wrapper that kept a
        // per-run fake map would still report a reused counter here.
        assert_eq!(state.provider.call_count(), 1);
        let seen = state.provider.requests();
        let runs: Vec<&str> = seen.iter().map(|seen| seen.run().as_str()).collect();
        assert_eq!(runs, vec!["run-a"]);
    }

    /// The wrapper advertises exactly the demo fixture's capabilities, and the
    /// cached description survives the per-run script swap: a runtime that
    /// negotiated capabilities before the first turn must not see them change.
    #[test]
    fn capabilities_match_the_demo_fixture_across_run_changes() {
        let provider = PerRunProvider::new();
        let expected = FakeProvider::demo_two_turn().capabilities();
        assert_eq!(provider.capabilities(), expected);

        let first = provider.stream(&request("run-a", "turn-1"), &live_context());
        assert!(matches!(
            terminal(&first),
            ProviderEvent::TurnFinished(finished) if finished.reason() == FinishReason::ToolCalls
        ));
        let reset = provider.stream(&request("run-b", "turn-1"), &live_context());
        assert_eq!(ready_refs(&reset), vec!["prov-ref-1", "prov-ref-2"]);
        assert_eq!(provider.capabilities(), expected);
    }

    /// The wrapper forwards the demo fixture's identity and scope, so a
    /// continuation-carrying script would observe the same labels through
    /// the wrapper as through the fake itself.
    #[test]
    fn identity_and_scope_match_the_demo_fixture() {
        let provider = PerRunProvider::new();
        let probe = FakeProvider::demo_two_turn();
        assert_eq!(provider.adapter_identity(), probe.adapter_identity());
        assert_eq!(
            provider.continuation_scope(WEB_PROFILE),
            probe.continuation_scope(WEB_PROFILE)
        );
        assert_eq!(
            provider.continuation_scope("other-profile"),
            probe.continuation_scope("other-profile")
        );
    }
}
