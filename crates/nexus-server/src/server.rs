//! Loopback HTTP server over the runtime command/event port.
//!
//! One [`Runtime`] per session: sessions never share runs, calls, or
//! approvals. Connection threads are plain blocking I/O; every runtime
//! interaction goes through the shared Tokio handle. Event receivers live
//! behind mutexes because `mpsc::Receiver` is `!Sync`; each session takes at
//! most one SSE subscriber at a time.
//!
//! # User configuration
//!
//! [`Server::set_config`] installs one [`UserConfig`] plus the file it is
//! persisted to. A server built by [`Server::new`] alone holds
//! [`UserConfig::default_config`] with no path: submit selection then
//! resolves nothing, favourites are refused, and recent-model recording
//! stays in memory, so the existing demo wiring is unchanged.
//!
//! `submit` accepts an optional `"provider"` and `"model"`. Both must name
//! configured identities (`400` otherwise) and, when they disagree about
//! which provider owns the model, the pair is rejected. The selected
//! provider's credential must resolve from the process environment at
//! submit time (`503` otherwise). The diagnostic names only the provider
//! id, which is a bounded lock identifier: no environment value can reach
//! it. An accepted run with a selected model is remembered in a
//! run-to-model map, and the SSE terminal event for that run records the
//! use in the configuration and saves it.
//!
//! Persistence is best effort by design: a failed save is logged to stderr
//! and the run, its stream, and the in-memory record all continue. Usage
//! telemetry must never convert a completed run into a transport failure,
//! and `record_use` is applied in memory before the save is attempted so
//! the two can never disagree about what was used.

use std::collections::HashMap;
use std::io::Write;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nexus_config::{ConfigError, UserConfig, resolve_credential, save, summary};
use nexus_core::{
    ApproveCommand, CommandReply, DenyCommand, GetSnapshotCommand, ModelRequest,
    ProviderCapabilities, ProviderContext, ProviderEvent, ProviderPort, RequestId, RunId,
    SessionId, SubmitCommand,
};
use nexus_fakes::{FakeProvider, FakeTool};
use nexus_runtime::{EventStreams, Policy, Runtime, RuntimeConfig};
use nexus_tools::{ScopedLister, ScopedPatcher, ScopedReader, ScopedSearcher, ScopedWriter};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::http::{self, Request, Response};
use crate::json;

/// Idle SSE window: a `: ping` comment keeps the stream alive.
const SSE_IDLE: Duration = Duration::from_secs(15);
/// Default execution profile for web-submitted tasks.
const WEB_PROFILE: &str = "web-test";

/// Tool wiring for demo sessions. Fakes are the default: nothing touches
/// the real filesystem. `RealFiles` executes jailed `host_read`, `host_list`,
/// `host_search`, `host_write`, and `host_patch` tools (mutations require approval); the root is
/// canonicalized and validated up front and per session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolsMode {
    /// Scripted doubles only; the default.
    Fakes,
    /// Real jailed reads and approval-gated writes rooted here.
    RealFiles {
        /// Canonical jail root.
        root: std::path::PathBuf,
    },
}

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
    /// Events consumed from the channels that belong to a run other than
    /// the one being served. `try_recv` consumes, so a foreign event met
    /// while draining cannot be left in the channel; it is stashed here
    /// for its own waiter instead of being dropped.
    pending: Mutex<Vec<nexus_core::RunEvent>>,
}

impl Session {
    /// Builds the next correlation identity for this session.
    fn request_id(&self) -> RequestId {
        let n = self.next_request.fetch_add(1, Ordering::SeqCst);
        RequestId::new(format!("req-web-{n}")).expect("counter request id is valid")
    }
}

/// Shared server state: the Tokio handle driving runtime ports, every live
/// session, and the user configuration. Bound to loopback by the binary;
/// there is no auth.
pub struct Server {
    handle: tokio::runtime::Handle,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    next_session: AtomicU64,
    /// The live configuration document. Every read and write goes through
    /// this one lock so a run's usage record and the redacted summary can
    /// never observe a half-applied change.
    config: Mutex<UserConfig>,
    /// File the configuration is persisted to. `None` keeps every change in
    /// memory: a server without a configured path never writes a file.
    config_path: Option<PathBuf>,
    /// Accepted run id to the model id it was submitted with. Entries are
    /// consumed by that run's SSE terminal event, so the map holds at most
    /// the runs that were accepted but never streamed to a terminal.
    run_models: Mutex<HashMap<String, String>>,
    /// Tool wiring for sessions created after the call. Startup-only: set
    /// before serving, alongside [`Server::set_config`].
    tools_mode: ToolsMode,
}

/// Session creation failure with its HTTP status and static diagnostic.
/// Rejected input never echoes caller text.
pub struct SessionError {
    status: u16,
    message: &'static str,
}

impl SessionError {
    fn new(status: u16, message: &'static str) -> Self {
        Self { status, message }
    }

    /// Returns the HTTP status for this failure.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.status
    }

    /// Returns the static diagnostic.
    #[must_use]
    pub fn message(&self) -> &'static str {
        self.message
    }

    /// Maps back to a domain error for the legacy constructor path.
    fn into_agent_error(self) -> nexus_core::AgentError {
        nexus_core::AgentError::new(
            nexus_core::ErrorCategory::InvalidInput,
            self.message,
            nexus_core::RetryGuidance::DoNotRetry,
        )
        .expect("static safe session message builds")
    }
}

impl Server {
    /// Creates shared state around the Tokio handle used for `block_on`.
    ///
    /// The server starts with [`UserConfig::default_config`], no
    /// configuration path, and scripted fake tools. Call
    /// [`Server::set_config`] and [`Server::set_tools_mode`] before serving
    /// to enable submit selection, favourites, persisted recents, and real
    /// jailed file tools.
    #[must_use]
    pub fn new(handle: tokio::runtime::Handle) -> Self {
        Self {
            handle,
            sessions: Mutex::new(HashMap::new()),
            next_session: AtomicU64::new(1),
            config: Mutex::new(UserConfig::default_config()),
            config_path: None,
            run_models: Mutex::new(HashMap::new()),
            tools_mode: ToolsMode::Fakes,
        }
    }

    /// Installs the tool wiring for sessions created after this call.
    /// Real roots are validated eagerly so an unreadable jail fails at
    /// startup, never at the first submit.
    pub fn set_tools_mode(&mut self, mode: ToolsMode) -> Result<(), nexus_core::AgentError> {
        if let ToolsMode::RealFiles { root } = &mode {
            ScopedReader::with_root(root)?;
        }
        self.tools_mode = mode;
        Ok(())
    }

    /// Installs the user configuration and the file it is saved to.
    ///
    /// `path` of `None` keeps later changes in memory only. Changing the
    /// document never touches live runs: selection is resolved per submit
    /// and a run's recorded model is already fixed at acceptance.
    pub fn set_config(&mut self, config: UserConfig, path: Option<PathBuf>) {
        *self.config.lock().expect("config lockable") = config;
        self.config_path = path;
    }

    /// Creates a session with a fresh demo-wired runtime and returns its
    /// token. Wiring failures are startup-class errors, never hung runs.
    pub fn create_session(&self) -> Result<String, nexus_core::AgentError> {
        self.create_session_with(None, None)
            .map_err(|error| error.into_agent_error())
    }

    /// Creates a session, optionally bound to a configured provider and
    /// model. Without a selection the session serves the demo script;
    /// with one it serves the real OpenAI-compatible adapter while tools
    /// follow the server tool mode. Unknown identities fail before any
    /// run is minted; a missing credential fails without network touch.
    pub fn create_session_with(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Result<String, SessionError> {
        if provider.is_none() && model.is_some() {
            return Err(SessionError::new(400, "selected model needs a provider"));
        }
        let provider = match provider {
            None => None,
            Some(id) => Some(self.select_provider(id, model)?),
        };
        let n = self.next_session.fetch_add(1, Ordering::SeqCst);
        let token = format!("sess-web-{n}");
        let session = SessionId::new(&token).expect("counter session id is valid");
        let config = RuntimeConfig {
            limits: nexus_core::Limits::m0_test(),
            policy: Policy::m0_test(),
            has_approval_handler: true,
        };
        let provider: Arc<dyn nexus_core::ProviderPort + Send + Sync> = match provider {
            Some(adapter) => Arc::new(adapter),
            None => Arc::new(PerRunProvider::new()),
        };
        let tools: Vec<Arc<dyn nexus_core::ToolPort + Send + Sync>> = match &self.tools_mode {
            ToolsMode::Fakes => vec![
                Arc::new(FakeTool::read_only()),
                Arc::new(FakeTool::mutation()),
            ],
            ToolsMode::RealFiles { root } => vec![
                Arc::new(
                    ScopedReader::with_root(root)
                        .map_err(|_| SessionError::new(500, "demo wiring is invalid"))?,
                ),
                Arc::new(
                    ScopedLister::with_root(root)
                        .map_err(|_| SessionError::new(500, "demo wiring is invalid"))?,
                ),
                Arc::new(
                    ScopedSearcher::with_root(root)
                        .map_err(|_| SessionError::new(500, "demo wiring is invalid"))?,
                ),
                Arc::new(
                    ScopedWriter::with_root(root)
                        .map_err(|_| SessionError::new(500, "demo wiring is invalid"))?,
                ),
                Arc::new(
                    ScopedPatcher::with_root(root)
                        .map_err(|_| SessionError::new(500, "demo wiring is invalid"))?,
                ),
            ],
        };
        let (runtime, streams): (Runtime, EventStreams) = Runtime::try_new(config, provider, tools)
            .map_err(|_| SessionError::new(500, "demo wiring is invalid"))?;
        let entry = Arc::new(Session {
            session,
            runtime: Mutex::new(runtime),
            data: Mutex::new(streams.data),
            control: Mutex::new(streams.control),
            streaming: Mutex::new(false),
            next_request: AtomicU64::new(1),
            pending: Mutex::new(Vec::new()),
        });
        self.sessions
            .lock()
            .expect("sessions lockable")
            .insert(token.clone(), entry);
        Ok(token)
    }

    /// Resolves a configured provider plus an optional model override to
    /// a live adapter. The credential resolves here, before any socket
    /// opens; unknown identities and model/provider mismatches fail with
    /// static diagnostics.
    fn select_provider(
        &self,
        id: &str,
        model: Option<&str>,
    ) -> Result<nexus_openai::OpenAiProvider, SessionError> {
        let unknown_provider = || SessionError::new(400, "selected provider is unknown");
        let profile = {
            let config = self.config.lock().expect("config lockable");
            let profile = config.provider(id).ok_or_else(unknown_provider)?.clone();
            match model {
                None => (profile, None),
                Some(name) => {
                    let entry = config
                        .model(name)
                        .ok_or_else(|| SessionError::new(400, "selected model is unknown"))?;
                    if entry.provider != id {
                        return Err(SessionError::new(
                            400,
                            "selected model belongs to another provider",
                        ));
                    }
                    (profile, Some(entry.name.clone()))
                }
            }
        };
        let (profile, vendor) = profile;
        let vendor = vendor.unwrap_or(profile.default_model.clone());
        nexus_config::resolve_credential(&profile.credential)
            .map_err(|_| SessionError::new(503, "provider credential is unavailable"))?;
        nexus_openai::OpenAiProvider::from_profile(&profile, &vendor)
            .map_err(|_| SessionError::new(400, "selected provider is invalid"))
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
            match serde_json::from_slice(&request.body) {
                Ok(value) => value,
                Err(_) => {
                    return Some(json_response(
                        400,
                        &json::error_body("request body is invalid"),
                    ));
                }
            }
        };
        let body = &parsed;
        match route_path(&request.method, &request.path) {
            Route::Health => Some(json_response(200, &body_json(crate::json::health_json()))),
            Route::CreateSession => {
                let field = |key: &str| match body.get(key) {
                    None | Some(Value::Null) => Ok(None),
                    Some(Value::String(text)) => Ok(Some(text.as_str())),
                    Some(_) => Err(json_response(
                        400,
                        &json::error_body("selected identity is invalid"),
                    )),
                };
                let (provider, model) = match (field("provider"), field("model")) {
                    (Ok(provider), Ok(model)) => (provider, model),
                    _ => {
                        return Some(json_response(
                            400,
                            &json::error_body("selected identity is invalid"),
                        ));
                    }
                };
                match self.create_session_with(provider, model) {
                    Ok(token) => {
                        let mut reply = serde_json::json!({ "session": token });
                        if let Some(id) = provider {
                            reply["provider"] = Value::String(id.to_owned());
                        }
                        if let Some(name) = model {
                            reply["model"] = Value::String(name.to_owned());
                        }
                        Some(json_response(201, &body_json(reply)))
                    }
                    Err(error) => Some(json_response(
                        error.status(),
                        &json::error_body(error.message()),
                    )),
                }
            }
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
            Route::Config => Some(self.config_summary()),
            Route::AddFavourite => Some(self.add_favourite(body)),
            Route::RemoveFavourite { model } => Some(self.remove_favourite(model)),
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

    /// Returns the redacted configuration summary: identities, labels,
    /// credential variable names, favourites, and recents. The document
    /// holds no secret values, so nothing here can disclose one.
    fn config_summary(&self) -> Response {
        let config = self.config.lock().expect("config lockable");
        json_response(200, &body_json(summary(&config)))
    }

    /// Adds a favourite model and persists it. An unknown model is invalid
    /// input (`400`) and a full list is a conflict (`409`), matching the
    /// distinctions the configuration model already draws. A save failure
    /// still leaves the in-memory change applied and reports `500`: the
    /// edit succeeded, only its durability did not.
    fn add_favourite(&self, body: &Value) -> Response {
        let id = match body_str(body, "id") {
            Some(id) => id.to_owned(),
            None => {
                return json_response(400, &json::error_body("favourite identity is invalid"));
            }
        };
        let mut config = self.config.lock().expect("config lockable");
        if config.model(&id).is_none() {
            return json_response(400, &json::error_body("favourite model is unknown"));
        }
        match config.add_favourite(&id) {
            Ok(()) => match self.persist(&config) {
                Ok(()) => json_response(200, &Self::favourites_body(&config)),
                Err(_) => json_response(500, &json::error_body("configuration could not be saved")),
            },
            Err(_) => json_response(409, &json::error_body("favourite list is full")),
        }
    }

    /// Removes a favourite model and persists it. An id that is not a
    /// favourite is a missing target (`404`); the model itself may still
    /// exist, which is why this is not the same refusal as `add_favourite`.
    fn remove_favourite(&self, model: &str) -> Response {
        let mut config = self.config.lock().expect("config lockable");
        match config.remove_favourite(model) {
            Ok(()) => match self.persist(&config) {
                Ok(()) => json_response(200, &Self::favourites_body(&config)),
                Err(_) => json_response(500, &json::error_body("configuration could not be saved")),
            },
            Err(_) => json_response(404, &json::error_body("favourite is unknown")),
        }
    }

    /// Builds the response body for a favourites change: the resulting
    /// list in admission order. A `200` means the change is live in memory
    /// and reached the file; a `500` means it is live in memory only.
    fn favourites_body(config: &UserConfig) -> Vec<u8> {
        body_json(serde_json::json!({ "favourites": config.favourites() }))
    }

    /// Writes the configuration to its configured path. With no path there
    /// is nothing to save, which is a success: an unconfigured server
    /// keeps every change in memory without reporting an error.
    fn persist(&self, config: &UserConfig) -> Result<(), ConfigError> {
        match self.config_path.as_deref() {
            None => Ok(()),
            Some(path) => save(config, path),
        }
    }

    /// Accepts a task for the session. Only `Accepted` mints a run; every
    /// other reply keeps the slot untouched.
    ///
    /// An optional `provider`/`model` pair selects the integration. Both
    /// are validated against the configured document before the run is
    /// submitted, so an invalid or unavailable selection never occupies the
    /// session's single run slot and never mints a run. An accepted run
    /// with a selected model is remembered for terminal-time recording.
    fn submit_run(&self, session: &Arc<Session>, body: &Value) -> Option<Response> {
        let model = match self.select_model(body) {
            Ok(model) => model,
            Err(response) => return Some(response),
        };
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
        // Only an accepted run may be recorded: a rejected, busy, or
        // missing reply mints nothing, so there is no run to attribute
        // usage to.
        if reply.reply() == CommandReply::Accepted
            && let (Some(run), Some(model)) = (reply.run(), model.as_deref())
        {
            self.run_models
                .lock()
                .expect("run model map lockable")
                .insert(run.as_str().to_owned(), model.to_owned());
        }
        let body = match reply.run() {
            Some(run) => {
                serde_json::json!({ "reply": crate::json::reply_name(reply.reply()), "run": run.as_str() })
            }
            None => serde_json::json!({ "reply": crate::json::reply_name(reply.reply()) }),
        };
        Some(json_response(status, &body_json(body)))
    }

    /// Resolves the optional `provider`/`model` submit selection to the
    /// model id whose use will be recorded at the run's terminal event.
    ///
    /// `Ok(None)` means no selection was requested: the run proceeds on the
    /// demo wiring and records no usage. An unknown `provider` or `model`
    /// is invalid input (`400`). When both name configured identities but
    /// disagree about ownership, the pair is rejected rather than silently
    /// preferring one field.
    ///
    /// A selection whose provider credential cannot be resolved is a
    /// readiness failure (`503`), not bad input: the request is well-formed
    /// and retryable once the environment is set. The body names only the
    /// provider id, which the configuration charset bounds to lock
    /// identifier text, so no credential value can reach the diagnostic.
    fn select_model(&self, body: &Value) -> Result<Option<String>, Response> {
        let provider_field = optional_text(body, "provider")?;
        let model_field = optional_text(body, "model")?;
        if provider_field.is_none() && model_field.is_none() {
            return Ok(None);
        }
        // The document is locked once and released before any credential
        // resolution: the environment lookup must never run while the
        // configuration lock is held, so a slow or blocking resolution
        // cannot stall every other config request.
        let (provider_id, credential) = {
            let config = self.config.lock().expect("config lockable");
            let provider_id = match (provider_field, model_field) {
                (Some(provider), Some(model)) => {
                    let Some(entry) = config.model(model) else {
                        return Err(json_response(
                            400,
                            &json::error_body("selected model is unknown"),
                        ));
                    };
                    let Some(profile) = config.provider(provider) else {
                        return Err(json_response(
                            400,
                            &json::error_body("selected provider is unknown"),
                        ));
                    };
                    if entry.provider != profile.id {
                        return Err(json_response(
                            400,
                            &json::error_body("selected model does not belong to that provider"),
                        ));
                    }
                    profile.id.clone()
                }
                (Some(provider), None) => {
                    let Some(profile) = config.provider(provider) else {
                        return Err(json_response(
                            400,
                            &json::error_body("selected provider is unknown"),
                        ));
                    };
                    profile.id.clone()
                }
                (None, Some(model)) => {
                    let Some(entry) = config.model(model) else {
                        return Err(json_response(
                            400,
                            &json::error_body("selected model is unknown"),
                        ));
                    };
                    // A model entry always names an existing provider:
                    // admission rejects the reference otherwise.
                    config
                        .provider(&entry.provider)
                        .expect("model names a configured provider")
                        .id
                        .clone()
                }
                (None, None) => unreachable!("both-absent is handled above"),
            };
            let credential = config
                .provider(&provider_id)
                .expect("provider was resolved above")
                .credential
                .clone();
            (provider_id, credential)
        };
        if resolve_credential(&credential).is_err() {
            return Err(self.credential_unavailable(&provider_id));
        }
        Ok(model_field.map(str::to_owned))
    }

    /// Builds the credential-readiness refusal for one provider. The id
    /// comes from the validated document, so it is bounded lock-identifier
    /// text; the resolved value never appears here.
    fn credential_unavailable(&self, provider: &str) -> Response {
        json_response(
            503,
            &body_json(serde_json::json!({
                "error": format!("provider credential is unavailable: {provider}"),
                "retry": "set the referenced environment variable and retry",
            })),
        )
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
            let event = match Self::take_pending(session, &run) {
                Some(event) => event,
                None => {
                    let next = self.handle.block_on(async {
                        tokio::time::timeout(SSE_IDLE, async {
                            tokio::select! {
                                event = data.recv() => event.map(StreamSide::Data),
                                event = control.recv() => event.map(StreamSide::Control),
                            }
                        })
                        .await
                    });
                    match next {
                        Ok(Some(StreamSide::Data(event)))
                        | Ok(Some(StreamSide::Control(event))) => event,
                        Ok(None) => break,
                        Err(_) => {
                            if write!(stream, ": ping\n\n").is_err() || stream.flush().is_err() {
                                break;
                            }
                            continue;
                        }
                    }
                }
            };
            if event.run() != &run {
                // Another run's event: stash it for its own waiter
                // instead of dropping it.
                session
                    .pending
                    .lock()
                    .expect("pending lockable")
                    .push(event);
                continue;
            }
            let terminal = event.is_terminal();
            // Usage is recorded before the terminal frame is published, not
            // after. A client that reads the terminal event is entitled to
            // observe the recorded usage and the persisted file immediately
            // afterwards; saving afterwards would leave a window in which
            // the run looks finished but its usage is not yet durable.
            if terminal {
                self.record_model_use(run.as_str());
                // The terminal may win the channel race while presentation
                // traffic is still queued behind it (data and control are
                // independent channels): forward everything already
                // committed first, terminal last, so a select! ordering can
                // never strand a predecessor event.
                if Self::drain_predecessors(stream, &mut data, &mut control, &run, &session.pending)
                    .is_err()
                {
                    break;
                }
            }
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

    /// Forwards events already queued on either channel for this run. Called
    /// with the terminal in hand but before publishing it, so predecessors
    /// stranded by a channel race still precede it on the wire. Only the
    /// caller knows which event is authoritative; everything drained here is
    /// forwarded, including a duplicate terminal if one ever appears (the
    /// runtime publishes exactly one, so its presence would itself be the
    /// finding, never something to hide).
    fn drain_predecessors(
        stream: &mut TcpStream,
        data: &mut tokio::sync::mpsc::Receiver<nexus_core::RunEvent>,
        control: &mut tokio::sync::mpsc::Receiver<nexus_core::RunEvent>,
        run: &RunId,
        pending: &Mutex<Vec<nexus_core::RunEvent>>,
    ) -> std::io::Result<()> {
        loop {
            let mut progressed = false;
            for channel in [&mut *data, &mut *control] {
                while let Ok(queued) = channel.try_recv() {
                    progressed = true;
                    if queued.run() != run {
                        // `try_recv` already consumed this event, so it
                        // cannot stay queued: stash it for its own waiter
                        // and stop draining this channel, leaving the
                        // events behind it untouched.
                        pending.lock().expect("pending lockable").push(queued);
                        break;
                    }
                    let frame = format!(
                        "id: {}\ndata: {}\n\n",
                        queued.seq(),
                        crate::json::event_json(&queued)
                    );
                    stream.write_all(frame.as_bytes())?;
                    stream.flush()?;
                }
            }
            if !progressed {
                return Ok(());
            }
        }
    }

    /// Pops the earliest stashed event owned by `run`, if any. Stashed
    /// events come from foreign-run encounters in the receive and drain
    /// paths; serving them here keeps every event deliverable to its own
    /// waiter exactly once.
    fn take_pending(session: &Session, run: &RunId) -> Option<nexus_core::RunEvent> {
        let mut pending = session.pending.lock().expect("pending lockable");
        let pos = pending.iter().position(|event| event.run() == run)?;
        Some(pending.remove(pos))
    }
    /// the document. A run submitted without a model has no entry and is
    /// skipped without touching the configuration.
    ///
    /// The map entry is consumed on every call, terminal or not, so a
    /// client that abandons the stream cannot leave a stale attribution
    /// behind for a later run. Recording is applied to the in-memory
    /// document first, so a failed save leaves usage visible in `GET
    /// /config` and is reported on stderr rather than discarded; the run
    /// itself has already finished and its outcome is never retracted.
    fn record_model_use(&self, run: &str) {
        let Some(model) = self
            .run_models
            .lock()
            .expect("run model map lockable")
            .remove(run)
        else {
            return;
        };
        let mut config = self.config.lock().expect("config lockable");
        if let Err(error) = config.record_use(&model) {
            eprintln!("nexus-server: model usage was not recorded ({error})");
            return;
        }
        if let Err(error) = self.persist(&config) {
            eprintln!("nexus-server: configuration could not be saved ({error})");
        }
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
    /// `GET /config`: the redacted configuration summary.
    Config,
    /// `POST /config/favourites` with `{"id": "<model id>"}`.
    AddFavourite,
    /// `DELETE /config/favourites/{mid}`.
    RemoveFavourite { model: &'a str },
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
        ("GET", ["", "config"]) => Route::Config,
        ("POST", ["", "config", "favourites"]) => Route::AddFavourite,
        // An empty trailing segment is a malformed path, not a request to
        // remove the empty id: `/config/favourites/` must not reach the
        // handler as a well-formed removal.
        ("DELETE", ["", "config", "favourites", model]) if !model.is_empty() => {
            Route::RemoveFavourite { model }
        }
        _ => Route::NotFound,
    }
}

/// Extracts one string field from a JSON object body.
fn body_str<'a>(body: &'a Value, key: &str) -> Option<&'a str> {
    body.get(key)?.as_str()
}

/// Reads an optional string selection field.
///
/// A field that is present but not a string is rejected instead of being
/// read as absent: `{"model": 12}` is a malformed selection, and treating it
/// as "no selection" would silently drop the caller's intent and dispatch an
/// unselected run.
fn optional_text<'a>(body: &'a Value, key: &str) -> Result<Option<&'a str>, Response> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(_) => Err(json_response(
            400,
            &json::error_body("selection field is not text"),
        )),
    }
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
    use nexus_config::{AdapterKind, CredentialRef, ModelEntry, ProviderProfile};
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

    /// One provider whose credential cannot resolve. The name is chosen so
    /// that no environment defines it, which is the only way to exercise the
    /// refusal without mutating the process environment.
    fn gated_config() -> UserConfig {
        let mut config = UserConfig::default_config();
        config
            .add_provider(
                ProviderProfile::new(
                    "gated",
                    "Gated",
                    AdapterKind::Direct,
                    None,
                    CredentialRef::env_var("NEXUS_SERVER_UNIT_ABSENT_7C1E").expect("valid"),
                    "m",
                )
                .expect("valid provider builds"),
            )
            .expect("provider admits");
        config
            .add_model(ModelEntry::new("gated-fast", "gated", "f").expect("valid"))
            .expect("model admits");
        config
    }

    /// A server carrying `gated_config` and no path, on a current handle.
    fn server_with_gated_config() -> (Server, tokio::runtime::Runtime) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_time()
            .build()
            .expect("unit executor builds");
        let mut server = Server::new(runtime.handle().clone());
        server.set_config(gated_config(), None);
        (server, runtime)
    }

    /// A JSON submit body naming the given optional selection fields.
    fn submit_body(provider: Option<&str>, model: Option<&str>) -> Value {
        let mut body = serde_json::Map::new();
        body.insert("input".to_owned(), serde_json::json!("unit task"));
        if let Some(provider) = provider {
            body.insert("provider".to_owned(), serde_json::json!(provider));
        }
        if let Some(model) = model {
            body.insert("model".to_owned(), serde_json::json!(model));
        }
        Value::Object(body)
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
    fn config_routes_match_the_documented_methods_and_paths() {
        assert_eq!(route_path("GET", "/config"), Route::Config);
        assert_eq!(
            route_path("POST", "/config/favourites"),
            Route::AddFavourite
        );
        assert_eq!(
            route_path("DELETE", "/config/favourites/sonnet"),
            Route::RemoveFavourite { model: "sonnet" }
        );
    }

    /// Each configuration route refuses the other methods, so a read cannot
    /// be issued as a write or a removal as an addition.
    #[test]
    fn config_routes_reject_the_wrong_method() {
        for (method, path) in [
            ("POST", "/config"),
            ("DELETE", "/config"),
            ("GET", "/config/favourites"),
            ("GET", "/config/favourites/sonnet"),
            ("POST", "/config/favourites/sonnet"),
            ("POST", "/config/favourites/sonnet/extra"),
            ("DELETE", "/config/favourites"),
            ("DELETE", "/config/favourites/"),
        ] {
            assert_eq!(route_path(method, path), Route::NotFound, "{method} {path}");
        }
    }

    /// A submit without selection fields is not a selection request at all:
    /// the demo wiring serves it and nothing is recorded against it. This is
    /// the path every pre-configuration client uses.
    #[test]
    fn submit_without_selection_records_nothing() {
        let (server, _runtime) = server_with_gated_config();
        // `Response` has no `Debug`, so the outcome is matched rather than
        // unwrapped through `expect`.
        match server.select_model(&submit_body(None, None)) {
            Ok(model) => assert_eq!(model, None, "no selection names no model"),
            Err(response) => panic!("no selection is never refused: {}", response.status),
        }
    }

    /// Every unresolvable selection is refused as a readiness failure, not
    /// as invalid input, and the diagnostic names only the provider id.
    #[test]
    fn unresolvable_selections_are_refused_as_not_ready() {
        let (server, _runtime) = server_with_gated_config();
        for body in [
            submit_body(None, Some("gated-fast")),
            submit_body(Some("gated"), Some("gated-fast")),
            submit_body(Some("gated"), None),
        ] {
            let response = server
                .select_model(&body)
                .expect_err("an unresolvable credential cannot be selected");
            assert_eq!(response.status, 503, "{body}");
            let reported = serde_json::from_slice::<Value>(&response.body).expect("JSON body");
            let message = reported["error"].as_str().expect("diagnostic");
            assert!(message.contains("gated"), "names the provider: {message}");
            assert!(
                !message.contains("NEXUS_SERVER_UNIT_ABSENT_7C1E"),
                "carries no credential detail: {message}"
            );
        }
    }

    /// Unknown identities and an inconsistent pair are invalid input, which
    /// is a different status and a different remedy from not-ready: the
    /// request itself must change, not the environment.
    #[test]
    fn unknown_or_inconsistent_selections_are_invalid_input() {
        let (server, _runtime) = server_with_gated_config();
        for body in [
            submit_body(Some("absent"), None),
            submit_body(None, Some("absent")),
            submit_body(Some("absent"), Some("gated-fast")),
            submit_body(Some("gated"), Some("absent")),
            // A present-but-non-string field is malformed input, not an
            // absent selection.
            serde_json::json!({ "input": "t", "model": 12 }),
            serde_json::json!({ "input": "t", "provider": ["gated"] }),
        ] {
            let response = server
                .select_model(&body)
                .expect_err("an unknown identity cannot be selected");
            assert_eq!(response.status, 400, "{body}");
        }
    }

    /// A model that belongs to a different provider is refused rather than
    /// resolved through whichever field happened to be read first: a
    /// selection that silently contradicts itself is not a selection.
    #[test]
    fn a_model_from_another_provider_is_refused() {
        let mut config = gated_config();
        config
            .add_provider(
                ProviderProfile::new(
                    "second",
                    "Second",
                    AdapterKind::Direct,
                    None,
                    CredentialRef::env_var("NEXUS_SERVER_UNIT_ABSENT_7C1E").expect("valid"),
                    "m",
                )
                .expect("valid provider builds"),
            )
            .expect("provider admits");
        config
            .add_model(ModelEntry::new("second-fast", "second", "f").expect("valid"))
            .expect("model admits");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_time()
            .build()
            .expect("unit executor builds");
        let mut server = Server::new(runtime.handle().clone());
        server.set_config(config, None);

        let response = server
            .select_model(&submit_body(Some("gated"), Some("second-fast")))
            .expect_err("the pair contradicts itself");
        assert_eq!(response.status, 400);
    }

    /// Recording is idempotent per run because the map entry is consumed on
    /// the first call: a second terminal event, or a client that abandons
    /// the stream after one, cannot inflate a model's use count or leave a
    /// stale attribution behind for a later run.
    #[test]
    fn recording_consumes_the_attribution_and_is_not_repeatable() {
        let (server, _runtime) = server_with_gated_config();
        server
            .run_models
            .lock()
            .expect("run model map lockable")
            .insert("run-x".to_owned(), "gated-fast".to_owned());

        server.record_model_use("run-x");
        assert_eq!(
            server.config.lock().expect("config lockable").recent(),
            ["gated-fast".to_owned()],
            "the recorded use is visible immediately"
        );
        assert!(
            server
                .run_models
                .lock()
                .expect("run model map lockable")
                .is_empty(),
            "the attribution is consumed, not left behind"
        );

        server.record_model_use("run-x");
        assert_eq!(
            server.config.lock().expect("config lockable").recent(),
            ["gated-fast".to_owned()],
            "a second terminal event records nothing further"
        );
    }

    /// A run with no recorded model is skipped entirely: an unselected run
    /// must not invent a recent entry or touch the document.
    #[test]
    fn an_unselected_run_records_nothing() {
        let (server, _runtime) = server_with_gated_config();
        server.record_model_use("run-without-selection");
        assert!(
            server
                .config
                .lock()
                .expect("config lockable")
                .recent()
                .is_empty()
        );
    }

    /// The favourites handlers answer with the resulting list, so a client
    /// never has to re-read the document to learn what changed. `save`
    /// failing is reported as such while the in-memory change stands.
    #[test]
    fn favourites_handlers_report_the_resulting_list() {
        let (server, _runtime) = server_with_gated_config();
        let response = server.add_favourite(&serde_json::json!({ "id": "gated-fast" }));
        assert_eq!(response.status, 200);
        assert_eq!(
            serde_json::from_slice::<Value>(&response.body).expect("JSON body")["favourites"],
            serde_json::json!(["gated-fast"])
        );

        let response = server.add_favourite(&serde_json::json!({ "id": "absent" }));
        assert_eq!(response.status, 400, "an unknown model is invalid input");

        let response = server.remove_favourite("gated-fast");
        assert_eq!(response.status, 200);
        assert_eq!(
            serde_json::from_slice::<Value>(&response.body).expect("JSON body")["favourites"],
            serde_json::json!([])
        );
        let response = server.remove_favourite("gated-fast");
        assert_eq!(response.status, 404, "removing twice is a missing target");
    }

    /// A removal that exists succeeds and is durable; one that does not is
    /// a missing target, distinct from the unknown-model refusal of the
    /// addition path even though both are "unknown" in different senses.
    ///
    /// Exercised here rather than over a socket to keep file persistence
    /// assertions hermetic.
    #[test]
    fn removing_a_favourite_answers_with_the_resulting_list() {
        let directory =
            std::env::temp_dir().join(format!("nexus-server-unit-fav-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("temp directory is creatable");
        let path = directory.join("config.json");

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_time()
            .build()
            .expect("unit executor builds");
        let mut config = gated_config();
        config
            .add_favourite("gated-fast")
            .expect("favourite admits");
        nexus_config::save(&config, &path).expect("document saves");
        let mut server = Server::new(runtime.handle().clone());
        server.set_config(config, Some(path.clone()));

        assert_eq!(server.remove_favourite("gated-fast").status, 200);
        assert!(
            nexus_config::load(&path)
                .expect("document loads")
                .expect("document exists")
                .favourites()
                .is_empty(),
            "the removal reached the configured file"
        );

        // A model that exists but is not a favourite, an id that was never
        // configured, and a repeated removal are all missing targets, and
        // none of them disturbs the stored list.
        assert_eq!(server.remove_favourite("gated-fast").status, 404);
        assert_eq!(server.remove_favourite("absent").status, 404);
        assert_eq!(server.remove_favourite("").status, 404);
        assert!(
            nexus_config::load(&path)
                .expect("document loads")
                .expect("document exists")
                .favourites()
                .is_empty(),
            "a refused removal leaves the document untouched"
        );
        let _ = std::fs::remove_dir_all(&directory);
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
