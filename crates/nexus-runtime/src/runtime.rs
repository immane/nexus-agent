//! Single-active-run execution loop: lifecycle, policy, ordered events.
//!
//! M0 scope: one active run (a second `Submit` gets an explicit `Busy`
//! reply), declared-order sequential tool dispatch, ephemeral persistence,
//! no session-store I/O. Core ports stay synchronous; this crate adapts them
//! at its boundary with the blocking pool, never leaking async types into
//! `nexus-core`.
//!
//! Review-hardened invariants:
//! - no state guard is ever held across an `await` on the same mutex;
//! - provider/tool contexts carry the live [`CancellationToken`] and an
//!   absolute monotonic deadline (`with_control`);
//! - a timed-out worker stays owned by the runtime until it actually
//!   terminates; a quarantined worker blocks the next dispatch and a new run
//!   instead of racing an earlier operation;
//! - control delivery never silently drops a required event: a full channel
//!   buffers into a bounded outbox that keeps ownership until drained, and a
//!   closed consumer is classified separately from saturation;
//! - per-run sequence numbers commit only on a successful enqueue (or a
//!   committed outbox entry), so delivered sequences stay contiguous;
//! - every admitted candidate (including denials) consumes the per-run call
//!   budget; registration is fallible, descriptors are cached once, and
//!   schemas compile through `nexus-validation`;
//! - the model conversation is rebuilt additively with exact input, call,
//!   result, item-key, and provider-reference round-trips.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant as StdInstant, SystemTime, UNIX_EPOCH};

use nexus_core::commands::EventSequence;
use nexus_core::{
    AgentError, ApprovalBinding, ApprovalId, ApprovalNotice, ApproveCommand, AssistantText,
    CallCandidate, CallId, CancelCommand, CancellationToken, Command, CommandReply,
    CommandResponse, ContinuationData, DenyCommand, EffectState, ErrorCategory, EventPayload,
    Evidence, ExecutionStatus, FinishReason, GetSnapshotCommand, Limits, ModelContextItem,
    ModelRequest, OutcomeSummary, PersistenceState, ProviderContext, ProviderEvent, ProviderPort,
    RequestId, RetryGuidance, RunEvent, RunFinished, RunId, RunLifecycle, RunOutcome, SessionId,
    Snapshot, SubmitCommand, ToolCall, ToolContext, ToolFinishedInfo, ToolId, ToolOutcome,
    ToolPort, ToolSpec, ToolStartedInfo, TurnId, Usage,
};
use nexus_validation::CompiledSchema;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::task::JoinHandle;

use crate::policy::Policy;
use crate::transport::{CONTROL_CAPACITY, DATA_CAPACITY, EventStreams};

/// Single-run lifecycle states (design 02-execution baseline).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// No run is active.
    Idle,
    /// Submission accepted, context assembled within limits.
    Preparing,
    /// One model-turn invocation in flight.
    CallingModel,
    /// Complete turn accepted; candidates admitted in declared order.
    ValidatingTools,
    /// Confirmation-required call waiting for a grant decision.
    AwaitingApproval,
    /// An authorized admitted call is executing within its deadline.
    ExecutingTool,
    /// Actual outcome recorded; never fabricated after the fact.
    RecordingResult,
    /// Terminal event published exactly once.
    Finished,
}

/// Runtime configuration: effective budgets, policy, and whether an
/// explicit approval handler exists. Without a handler, headless
/// confirmation-required calls are denied, never auto-approved or hung.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Finite effective budgets; zero budgets are rejected at construction.
    pub limits: Limits,
    /// Auto-approve predicate plus the bound policy revision.
    pub policy: Policy,
    /// True when something consumes `ApprovalRequired` and answers.
    pub has_approval_handler: bool,
}

/// Cloneable handle over one single-run runtime.
#[derive(Clone)]
pub struct Runtime {
    shared: Arc<Shared>,
}

struct Shared {
    limits: Limits,
    effective_output_budget: usize,
    context_items_bound: usize,
    policy: Policy,
    has_approval_handler: bool,
    provider: Arc<dyn ProviderPort + Send + Sync>,
    tools: HashMap<String, RegisteredTool>,
    tool_order: Vec<String>,
    tool_definitions: Vec<ToolSpec>,
    state: Mutex<State>,
    data_tx: mpsc::Sender<RunEvent>,
    control_tx: mpsc::Sender<RunEvent>,
    /// Committed control events that could not enter the bounded channel.
    /// Bounded by construction: a run can only generate a finite number of
    /// control events, and a new run is rejected while this is non-empty.
    outbox: StdMutex<VecDeque<RunEvent>>,
    flushing: AtomicBool,
    control_closed: AtomicBool,
    incarnation: u64,
    run_counter: AtomicU64,
}

struct RegisteredTool {
    spec: ToolSpec,
    schema: CompiledSchema,
    port: Arc<dyn ToolPort + Send + Sync>,
}

struct State {
    active: Option<ActiveRun>,
    last: Option<FinishedRun>,
    /// Workers whose termination is not yet established. A non-empty list
    /// blocks new runs and further dispatch; at most one entry exists in the
    /// sequential M0 baseline.
    quarantine: Vec<QuarantineMeta>,
}

struct QuarantineMeta {
    run: RunId,
    call: Option<CallId>,
}

struct PendingApproval {
    binding: ApprovalBinding,
}

struct QueuedCall {
    call: ToolCall,
    item_key: String,
    provider_ref: String,
    needs_approval: bool,
}

struct CurrentCall {
    call: ToolCall,
    item_key: String,
    provider_ref: String,
    binding: Option<ApprovalBinding>,
}

struct ActiveRun {
    phase: RunState,
    run: RunId,
    run_n: u64,
    session: SessionId,
    request: RequestId,
    profile: String,
    started: StdInstant,
    deadline: StdInstant,
    token: CancellationToken,
    wake: Arc<Notify>,
    cancelled: bool,
    turn_seq: u64,
    call_seq: u64,
    approval_seq: u64,
    turns_used: u32,
    calls_used: u32,
    continuation: Option<ContinuationData>,
    conversation: Vec<ModelContextItem>,
    queue: VecDeque<QueuedCall>,
    current: Option<CurrentCall>,
    pending: HashMap<ApprovalId, PendingApproval>,
    decided: HashMap<ApprovalId, bool>,
    outcomes: Vec<OutcomeSummary>,
    outcome_slot: Option<(CallId, ToolOutcome)>,
    next_seq: EventSequence,
    last_seq: Option<EventSequence>,
    pending_text: Option<AssistantText>,
    data_dropped: bool,
    last_usage: Option<Usage>,
    force_terminal: Option<RunOutcome>,
    terminal: Option<RunOutcome>,
    terminal_error: Option<AgentError>,
    delivery_closed: bool,
    finished_sent: bool,
}

struct FinishedRun {
    run: RunId,
    session: SessionId,
    outcome: RunOutcome,
    last_seq: Option<EventSequence>,
    outcomes: Vec<OutcomeSummary>,
    truncated: bool,
    terminal_delivered: bool,
}

/// Terminal decision produced by one state-machine step.
struct Terminal {
    outcome: RunOutcome,
    error: Option<AgentError>,
}

impl Terminal {
    fn completed() -> Self {
        Self {
            outcome: RunOutcome::Completed,
            error: None,
        }
    }

    fn refused() -> Self {
        Self {
            outcome: RunOutcome::Refused,
            error: None,
        }
    }

    fn cancelled() -> Self {
        Self {
            outcome: RunOutcome::Cancelled,
            error: None,
        }
    }

    fn cancelled_with(error: AgentError) -> Self {
        Self {
            outcome: RunOutcome::Cancelled,
            error: Some(error),
        }
    }

    fn limit(message: &'static str) -> Self {
        Self {
            outcome: RunOutcome::LimitReached,
            error: Some(runtime_error(ErrorCategory::ResourceLimit, message)),
        }
    }

    fn failed(error: AgentError) -> Self {
        Self {
            outcome: RunOutcome::Failed,
            error: Some(error),
        }
    }

    fn failed_internal(message: &'static str) -> Self {
        Self::failed(runtime_error(ErrorCategory::Internal, message))
    }
}

/// Control-delivery classification: saturation is not a disconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlOutcome {
    Sent,
    Buffered,
    Closed,
}

/// Process-unique runtime incarnation. Recreated runtimes (tests, restarts)
/// must not alias `RunId`/`CallId`/`ApprovalId` values, so the incarnation
/// mixes pid, wall-clock nanos, and an atomic counter.
static RUNTIME_INCARNATION: AtomicU64 = AtomicU64::new(0);

fn next_incarnation() -> u64 {
    let pid = u64::from(std::process::id());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let counter = RUNTIME_INCARNATION.fetch_add(1, Ordering::SeqCst);
    pid.rotate_left(32) ^ nanos.rotate_left(17) ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

fn runtime_error(category: ErrorCategory, message: &'static str) -> AgentError {
    AgentError::new(category, message, RetryGuidance::DoNotRetry)
        .expect("static safe runtime message builds")
}

impl Runtime {
    /// Fallible production constructor: validates limits, provider
    /// capabilities, and every tool registration (one `describe()` call per
    /// tool, exact M0 revision, no duplicate names, closed-schema compile)
    /// before any run can start.
    pub fn try_new(
        config: RuntimeConfig,
        provider: Arc<dyn ProviderPort + Send + Sync>,
        tools: Vec<Arc<dyn ToolPort + Send + Sync>>,
    ) -> Result<(Self, EventStreams), AgentError> {
        config.limits.validate()?;
        let capabilities = provider.capabilities();
        if !capabilities.text {
            return Err(runtime_error(
                ErrorCategory::UnsupportedCapability,
                "provider cannot generate text",
            ));
        }
        if !tools.is_empty() && !capabilities.tool_calls {
            return Err(runtime_error(
                ErrorCategory::UnsupportedCapability,
                "provider cannot call tools",
            ));
        }
        if tools.len() > nexus_core::provider::MAX_TOOL_DEFINITIONS {
            return Err(runtime_error(
                ErrorCategory::ResourceLimit,
                "too many registered tools",
            ));
        }
        let mut registry: HashMap<String, RegisteredTool> = HashMap::new();
        let mut order: Vec<String> = Vec::with_capacity(tools.len());
        for tool in tools {
            let spec = tool.describe();
            let id = spec.id().clone();
            if id.revision() != nexus_core::M0_REVISION {
                return Err(runtime_error(
                    ErrorCategory::InvalidInput,
                    "tool revision is not the M0 revision",
                ));
            }
            if registry.contains_key(id.name()) {
                return Err(runtime_error(
                    ErrorCategory::InvalidInput,
                    "duplicate tool registration",
                ));
            }
            let schema = CompiledSchema::compile(spec.input_schema_json())?;
            order.push(id.name().to_owned());
            registry.insert(
                id.name().to_owned(),
                RegisteredTool {
                    spec,
                    schema,
                    port: tool,
                },
            );
        }
        let tool_definitions: Vec<ToolSpec> = order
            .iter()
            .filter_map(|name| registry.get(name).map(|tool| tool.spec.clone()))
            .collect();
        let effective_output_budget = config
            .limits
            .max_tool_output_bytes
            .min(Limits::M0_TEST_TOOL_OUTPUT_BYTES)
            .min(
                capabilities
                    .max_output_bytes
                    .map_or(usize::MAX, |bytes| bytes as usize),
            );
        if effective_output_budget == 0 {
            return Err(runtime_error(
                ErrorCategory::UnsupportedCapability,
                "provider declares no usable output budget",
            ));
        }
        let context_items_bound = config
            .limits
            .retained_context_items
            .min(nexus_core::provider::MAX_CONVERSATION_ITEMS)
            .min(
                capabilities
                    .max_context_items
                    .map_or(usize::MAX, |items| items as usize),
            );
        if context_items_bound == 0 {
            return Err(runtime_error(
                ErrorCategory::UnsupportedCapability,
                "provider declares no usable context budget",
            ));
        }
        let (data_tx, data_rx) = mpsc::channel(DATA_CAPACITY);
        let (control_tx, control_rx) = mpsc::channel(CONTROL_CAPACITY);
        let shared = Arc::new(Shared {
            limits: config.limits,
            effective_output_budget,
            context_items_bound,
            policy: config.policy,
            has_approval_handler: config.has_approval_handler,
            provider,
            tools: registry,
            tool_order: order,
            tool_definitions,
            state: Mutex::new(State {
                active: None,
                last: None,
                quarantine: Vec::new(),
            }),
            data_tx,
            control_tx,
            outbox: StdMutex::new(VecDeque::new()),
            flushing: AtomicBool::new(false),
            control_closed: AtomicBool::new(false),
            incarnation: next_incarnation(),
            run_counter: AtomicU64::new(1),
        });
        Ok((
            Self { shared },
            EventStreams {
                data: data_rx,
                control: control_rx,
            },
        ))
    }

    /// Compatibility constructor for fixed test wiring. Production callers
    /// use [`Self::try_new`]; invalid test wiring panics here instead of
    /// producing a runtime that hangs or misreports.
    pub fn new(
        config: RuntimeConfig,
        provider: Arc<dyn ProviderPort + Send + Sync>,
        tools: Vec<Arc<dyn ToolPort + Send + Sync>>,
    ) -> (Self, EventStreams) {
        Self::try_new(config, provider, tools)
            .expect("test-only runtime wiring must be valid; use Runtime::try_new in production")
    }

    /// Handles one typed frontend command. `ListSessions`/`RestoreSession`
    /// have no store in M0 and are explicitly rejected.
    pub async fn handle(&self, command: Command) -> (CommandResponse, Option<Snapshot>) {
        match command {
            Command::Submit(command) => (self.submit(command).await, None),
            Command::Cancel(command) => (self.cancel(command).await, None),
            Command::Approve(command) => (self.approve(command).await, None),
            Command::Deny(command) => (self.deny(command).await, None),
            Command::GetSnapshot(command) => self.get_snapshot(command).await,
            Command::ListSessions(command) => (
                CommandResponse::new(command.request.clone(), CommandReply::Rejected, None),
                None,
            ),
            Command::RestoreSession(command) => (
                CommandResponse::new(command.request.clone(), CommandReply::Rejected, None),
                None,
            ),
        }
    }

    /// Accepts a new run or rejects explicitly. A second submit while a run
    /// is active, a worker termination is unconfirmed, or a committed
    /// control outbox is still draining replies `Busy` without disturbing
    /// the retained ownership.
    pub async fn submit(&self, command: SubmitCommand) -> CommandResponse {
        if command.validate().is_err() {
            return CommandResponse::new(command.request.clone(), CommandReply::Rejected, None);
        }
        let run_n = self.shared.run_counter.fetch_add(1, Ordering::SeqCst);
        let run = RunId::new(format!("r{:x}-{run_n}", self.shared.incarnation))
            .expect("counter id is valid");
        let now = StdInstant::now();
        let Some(deadline) = now.checked_add(self.shared.limits.run_duration) else {
            return CommandResponse::new(command.request.clone(), CommandReply::Rejected, None);
        };
        let Ok(user_item) = ModelContextItem::user_text(command.input.clone()) else {
            return CommandResponse::new(command.request.clone(), CommandReply::Rejected, None);
        };
        let mut state = self.shared.state.lock().await;
        if let Some(active) = state.active.as_ref() {
            return CommandResponse::new(
                command.request.clone(),
                CommandReply::Busy,
                Some(active.run.clone()),
            );
        }
        if let Some(quarantined) = state.quarantine.first() {
            return CommandResponse::new(
                command.request.clone(),
                CommandReply::Busy,
                Some(quarantined.run.clone()),
            );
        }
        if !self
            .shared
            .outbox
            .lock()
            .expect("control outbox readable")
            .is_empty()
        {
            let run = state.last.as_ref().map(|last| last.run.clone());
            return CommandResponse::new(command.request.clone(), CommandReply::Busy, run);
        }
        state.active = Some(ActiveRun {
            phase: RunState::Preparing,
            run: run.clone(),
            run_n,
            session: command.session.clone(),
            request: command.request.clone(),
            profile: command.profile.clone(),
            started: now,
            deadline,
            token: CancellationToken::new(),
            wake: Arc::new(Notify::new()),
            cancelled: false,
            turn_seq: 0,
            call_seq: 0,
            approval_seq: 0,
            turns_used: 0,
            calls_used: 0,
            continuation: None,
            conversation: vec![user_item],
            queue: VecDeque::new(),
            current: None,
            pending: HashMap::new(),
            decided: HashMap::new(),
            outcomes: Vec::new(),
            outcome_slot: None,
            next_seq: 0,
            last_seq: None,
            pending_text: None,
            data_dropped: false,
            last_usage: None,
            force_terminal: None,
            terminal: None,
            terminal_error: None,
            delivery_closed: false,
            finished_sent: false,
        });
        drop(state);
        let driver = self.clone();
        let run_for_task = run.clone();
        // The supervisor owns the driver's join handle so a panic or an
        // abrupt return still finalizes the slot exactly once.
        tokio::spawn(async move {
            let inner = {
                let driver = driver.clone();
                let run = run_for_task.clone();
                tokio::spawn(async move { driver.drive(run).await })
            };
            if let Err(join) = inner.await {
                driver.on_driver_failure(&run_for_task, join).await;
            }
        });
        CommandResponse::new(command.request.clone(), CommandReply::Accepted, Some(run))
    }

    /// Stops future dispatch and signals active work. Idempotent: repeats
    /// never double-dispatch or double-record. Cancellation is cooperative:
    /// a blocking worker is not preempted, but the live token is observable
    /// and the run deadline remains the hard bound.
    pub async fn cancel(&self, command: CancelCommand) -> CommandResponse {
        let mut state = self.shared.state.lock().await;
        match state.active.as_mut() {
            Some(active) if active.run == command.run => {
                active.cancelled = true;
                active.token.cancel();
                active.wake.notify_one();
                CommandResponse::new(
                    command.request.clone(),
                    CommandReply::Accepted,
                    Some(active.run.clone()),
                )
            }
            _ => self.stale_or_finalized(&state, &command.request, &command.run),
        }
    }

    /// Approves a live approval without changing its bound arguments.
    /// Duplicates and stale identities never dispatch again; the decided
    /// grant leaves the pending set immediately.
    pub async fn approve(&self, command: ApproveCommand) -> CommandResponse {
        self.resolve_grant(
            command.request,
            command.run,
            command.call,
            command.approval,
            true,
        )
        .await
    }

    /// Refuses a live approval without executing its call.
    pub async fn deny(&self, command: DenyCommand) -> CommandResponse {
        self.resolve_grant(
            command.request,
            command.run,
            command.call,
            command.approval,
            false,
        )
        .await
    }

    async fn resolve_grant(
        &self,
        request: RequestId,
        run: RunId,
        call: CallId,
        approval: ApprovalId,
        decision: bool,
    ) -> CommandResponse {
        let mut state = self.shared.state.lock().await;
        let Some(active) = state.active.as_mut() else {
            return self.stale_or_finalized(&state, &request, &run);
        };
        if active.run != run {
            return self.stale_or_finalized(&state, &request, &run);
        }
        let now = active.started.elapsed();
        let Some(pending) = active.pending.get(&approval) else {
            return CommandResponse::new(request, CommandReply::StaleOrUnknownTarget, Some(run));
        };
        if pending.binding.run() != &run
            || pending.binding.call() != &call
            || pending.binding.is_expired(now)
        {
            return CommandResponse::new(request, CommandReply::StaleOrUnknownTarget, Some(run));
        }
        active.pending.remove(&approval);
        active.decided.insert(approval, decision);
        active.wake.notify_one();
        CommandResponse::new(request, CommandReply::Accepted, Some(run))
    }

    /// Returns a bounded snapshot of a known run.
    pub async fn get_snapshot(
        &self,
        command: GetSnapshotCommand,
    ) -> (CommandResponse, Option<Snapshot>) {
        let state = self.shared.state.lock().await;
        if let Some(active) = state.active.as_ref()
            && active.run == command.run
        {
            let snapshot = snapshot_of_active(active);
            return (
                CommandResponse::new(
                    command.request.clone(),
                    CommandReply::Accepted,
                    Some(active.run.clone()),
                ),
                snapshot,
            );
        }
        if let Some(last) = state.last.as_ref()
            && last.run == command.run
        {
            let snapshot = snapshot_of_finished(last);
            return (
                CommandResponse::new(
                    command.request.clone(),
                    CommandReply::Accepted,
                    Some(last.run.clone()),
                ),
                snapshot,
            );
        }
        (
            CommandResponse::new(
                command.request.clone(),
                CommandReply::StaleOrUnknownTarget,
                None,
            ),
            None,
        )
    }

    fn stale_or_finalized(
        &self,
        state: &State,
        request: &RequestId,
        run: &RunId,
    ) -> CommandResponse {
        if state.last.as_ref().is_some_and(|last| &last.run == run) {
            return CommandResponse::new(
                request.clone(),
                CommandReply::AlreadyFinalized,
                Some(run.clone()),
            );
        }
        CommandResponse::new(request.clone(), CommandReply::StaleOrUnknownTarget, None)
    }

    async fn drive(&self, run: RunId) {
        let request = {
            let state = self.shared.state.lock().await;
            match state.active.as_ref() {
                Some(active) if active.run == run => active.request.clone(),
                _ => return,
            }
        };
        if self
            .publish_control(&run, EventPayload::RunStarted { request })
            .await
            .is_err()
        {
            self.finish_run(&run, RunOutcome::Cancelled, None).await;
            return;
        }
        let (outcome, error) = self.run_loop(&run).await;
        self.finish_run(&run, outcome, error).await;
    }

    async fn run_loop(&self, run: &RunId) -> (RunOutcome, Option<AgentError>) {
        let mut phase = RunState::Preparing;
        loop {
            let step: Result<RunState, Terminal> = match phase {
                RunState::Idle => Err(Terminal::cancelled()),
                RunState::Preparing => self.on_preparing(run).await,
                RunState::CallingModel => self.on_calling_model(run).await,
                RunState::ValidatingTools => self.on_validating(run).await,
                RunState::AwaitingApproval => self.on_awaiting(run).await,
                RunState::ExecutingTool => self.on_executing(run).await,
                RunState::RecordingResult => self.on_recording(run).await,
                RunState::Finished => return self.take_terminal(run).await,
            };
            match step {
                Ok(next) => {
                    self.set_phase(run, next).await;
                    phase = next;
                }
                Err(terminal) => {
                    self.set_terminal(run, terminal.outcome, terminal.error)
                        .await;
                    self.set_phase(run, RunState::Finished).await;
                    phase = RunState::Finished;
                }
            }
        }
    }

    async fn on_preparing(&self, run: &RunId) -> Result<RunState, Terminal> {
        let state = self.shared.state.lock().await;
        let Some(active) = state.active.as_ref() else {
            return Err(Terminal::cancelled());
        };
        if &active.run != run || active.cancelled || active.token.is_cancelled() {
            return Err(Terminal::cancelled());
        }
        if StdInstant::now() >= active.deadline {
            return Err(Terminal::limit("run duration exhausted"));
        }
        if self
            .shared
            .limits
            .check_model_turns(active.turns_used)
            .is_err()
        {
            return Err(Terminal::limit("model turn budget exhausted"));
        }
        Ok(RunState::CallingModel)
    }

    async fn on_calling_model(&self, run: &RunId) -> Result<RunState, Terminal> {
        let (request, context, turn, deadline) = {
            let mut state = self.shared.state.lock().await;
            let Some(active) = state.active.as_mut() else {
                return Err(Terminal::cancelled());
            };
            if &active.run != run || active.cancelled || active.token.is_cancelled() {
                return Err(Terminal::cancelled());
            }
            if StdInstant::now() >= active.deadline {
                return Err(Terminal::limit("run duration exhausted"));
            }
            if active.conversation.len() > self.shared.context_items_bound {
                return Err(Terminal::limit("retained context budget exhausted"));
            }
            let turn_n = active.turn_seq;
            active.turn_seq += 1;
            let turn = TurnId::new(format!(
                "t{:x}-{}-{turn_n}",
                self.shared.incarnation, active.run_n
            ))
            .map_err(|_| Terminal::failed_internal("turn identity is invalid"))?;
            let enabled: Vec<ToolId> = self
                .shared
                .tool_order
                .iter()
                .filter_map(|name| {
                    self.shared
                        .tools
                        .get(name)
                        .map(|tool| tool.spec.id().clone())
                })
                .collect();
            let request = ModelRequest::new(
                active.run.clone(),
                turn.clone(),
                active.profile.clone(),
                enabled,
                active.continuation.clone(),
                self.shared.effective_output_budget,
            )
            .map_err(|_| Terminal::limit("model request budget is invalid"))?
            .with_conversation(active.conversation.clone())
            .map_err(|_| Terminal::limit("model conversation exceeds its budget"))?
            .with_tool_definitions(self.shared.tool_definitions.clone())
            .map_err(|_| Terminal::limit("model tool definitions exceed their budget"))?;
            let context = ProviderContext::new(active.started.elapsed(), active.cancelled, None)
                .with_control(active.token.clone(), active.deadline);
            (request, context, turn, active.deadline)
        };
        let (wake, token) = {
            let state = self.shared.state.lock().await;
            match state.active.as_ref().filter(|active| &active.run == run) {
                Some(active) => (active.wake.clone(), active.token.clone()),
                None => return Err(Terminal::cancelled()),
            }
        };
        let provider = self.shared.provider.clone();
        let mut handle = tokio::task::spawn_blocking(move || provider.stream(&request, &context));
        let mut cancelled_first = false;
        let joined = loop {
            if token.is_cancelled() {
                cancelled_first = true;
                break None;
            }
            tokio::select! {
                biased;
                joined = &mut handle => break Some(joined),
                () = wake.notified() => {
                    if token.is_cancelled() {
                        cancelled_first = true;
                        break None;
                    }
                }
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => break None,
            }
        };
        match joined {
            Some(Ok(events)) => self.ingest_model_batch(run, &turn, events).await,
            Some(Err(join)) => {
                let message = if join.is_panic() {
                    "provider worker panicked"
                } else {
                    "provider worker ended without a result"
                };
                Err(Terminal::failed_internal(message))
            }
            None => {
                let cancelled = cancelled_first || token.is_cancelled() || {
                    let state = self.shared.state.lock().await;
                    state
                        .active
                        .as_ref()
                        .filter(|active| &active.run == run)
                        .is_some_and(|active| active.cancelled)
                };
                // Prompt logical terminal: the blocked worker stays owned
                // (quarantined) until it actually terminates.
                self.quarantine_provider(run, handle).await;
                if cancelled {
                    Err(Terminal::cancelled_with(runtime_error(
                        ErrorCategory::Cancelled,
                        "run cancelled while provider termination was unconfirmed",
                    )))
                } else {
                    Err(Terminal::limit(
                        "run duration exhausted while awaiting provider",
                    ))
                }
            }
        }
    }

    async fn ingest_model_batch(
        &self,
        run: &RunId,
        turn: &TurnId,
        events: Vec<ProviderEvent>,
    ) -> Result<RunState, Terminal> {
        // Full-turn atomic validation: duplicate references, partial/final
        // disagreement, and finish-reason conflicts fail before any
        // admission, identity consumption, or queue mutation.
        // Full-turn atomic validation: duplicate references, partial/final
        // disagreement, and finish-reason conflicts fail before any
        // admission, identity consumption, or queue mutation. An honestly
        // failed invocation keeps its provider error primary: a protocol
        // ordering diagnostic must not replace a typed timeout/failure.
        if let Err(protocol_error) = crate::protocol::validate_batch(&events, &self.shared.limits) {
            if let Some(error) = events.iter().find_map(|event| match event {
                ProviderEvent::Failed(error) => Some(error.clone()),
                _ => None,
            }) {
                return Err(if error.category() == ErrorCategory::Cancelled {
                    Terminal::cancelled_with(error)
                } else {
                    Terminal::failed(error)
                });
            }
            return Err(Terminal::failed(protocol_error));
        }
        let mut state = self.shared.state.lock().await;
        let Some(active) = state.active.as_mut() else {
            return Err(Terminal::cancelled());
        };
        if &active.run != run || active.cancelled || active.token.is_cancelled() {
            return Err(Terminal::cancelled());
        }
        let mut candidates = Vec::new();
        let mut terminal: Option<Result<nexus_core::TurnFinished, AgentError>> = None;
        let mut text_items: Vec<(String, String)> = Vec::new();
        for event in &events {
            match event {
                ProviderEvent::TextDelta { item_key, text } => {
                    if let Some(entry) = text_items.iter_mut().find(|(key, _)| key == item_key) {
                        entry.1.push_str(text);
                    } else {
                        text_items.push((item_key.clone(), text.clone()));
                    }
                    match AssistantText::new(turn.clone(), item_key.clone(), text.clone()) {
                        Ok(fragment) => buffer_text(&self.shared, active, run, fragment),
                        Err(_) => active.data_dropped = true,
                    }
                }
                ProviderEvent::ToolCallDelta { item_key, .. } => {
                    emit_data(
                        &self.shared,
                        active,
                        run,
                        EventPayload::ToolCallPreview {
                            item_key: item_key.clone(),
                        },
                    );
                }
                ProviderEvent::ToolCallReady(candidate) => candidates.push(candidate.clone()),
                ProviderEvent::Usage(usage) => {
                    if active.last_usage != Some(*usage) {
                        emit_control(
                            &self.shared,
                            active,
                            run,
                            EventPayload::UsageUpdated(*usage),
                        );
                        active.last_usage = Some(*usage);
                    }
                }
                ProviderEvent::TurnFinished(finished) => {
                    // Per-counter final merge: terminal `Some` wins, terminal
                    // `None` retains the last final `Some`, and a missing
                    // counter stays unknown instead of being downgraded.
                    let merged = merge_final_usage(active.last_usage, finished.usage());
                    if active.last_usage != Some(merged) {
                        emit_control(
                            &self.shared,
                            active,
                            run,
                            EventPayload::UsageUpdated(merged),
                        );
                        active.last_usage = Some(merged);
                    }
                    terminal = Some(Ok(finished.clone()));
                }
                ProviderEvent::Failed(error) => terminal = Some(Err(error.clone())),
            }
        }
        flush_text(&self.shared, active);
        let Some(terminal) = terminal else {
            return Err(Terminal::failed(runtime_error(
                ErrorCategory::Protocol,
                "provider batch ended without a terminal event",
            )));
        };
        match terminal {
            Err(error) => {
                if error.category() == ErrorCategory::Cancelled {
                    Err(Terminal::cancelled_with(error))
                } else {
                    Err(Terminal::failed(error))
                }
            }
            Ok(finished) => {
                active.turns_used += 1;
                if let Some(continuation) = finished.continuation().cloned() {
                    let adapter = self.shared.provider.adapter_identity();
                    let scope = self.shared.provider.continuation_scope(&active.profile);
                    if !continuation.is_compatible_with(adapter, &scope) {
                        return Err(Terminal::failed(runtime_error(
                            ErrorCategory::UnsupportedCapability,
                            "provider continuation is incompatible with this profile",
                        )));
                    }
                    active.continuation = Some(continuation);
                }
                match finished.reason() {
                    FinishReason::Stop => {
                        append_assistant_text(active, text_items);
                        Err(Terminal::completed())
                    }
                    FinishReason::Refusal => Err(Terminal::refused()),
                    FinishReason::OutputLimit => Err(Terminal::limit("model output limit reached")),
                    FinishReason::Incomplete => Err(Terminal::failed(runtime_error(
                        ErrorCategory::Protocol,
                        "model turn ended incomplete",
                    ))),
                    FinishReason::ToolCalls => {
                        if candidates.is_empty() {
                            return Err(Terminal::failed(runtime_error(
                                ErrorCategory::Protocol,
                                "tool-call turn carried no complete candidates",
                            )));
                        }
                        append_assistant_text(active, text_items);
                        self.admit_candidates(active, turn, candidates)?;
                        Ok(RunState::ValidatingTools)
                    }
                }
            }
        }
    }

    fn admit_candidates(
        &self,
        active: &mut ActiveRun,
        turn: &TurnId,
        candidates: Vec<CallCandidate>,
    ) -> Result<(), Terminal> {
        let run = active.run.clone();
        for (index, candidate) in candidates.into_iter().enumerate() {
            if self
                .shared
                .limits
                .check_tool_calls_for_turn(index as u32)
                .is_err()
            {
                return Err(Terminal::limit("tool call budget for turn exhausted"));
            }
            if self
                .shared
                .limits
                .check_tool_calls_for_run(active.calls_used)
                .is_err()
            {
                return Err(Terminal::limit("tool call budget for run exhausted"));
            }
            // Every admitted candidate consumes the per-run budget,
            // including unknown-tool and invalid-argument denials.
            active.calls_used += 1;
            let call = self.next_call_id(active)?;
            let Some(registered) = self.shared.tools.get(candidate.tool_name()) else {
                let outcome = denied_outcome("unknown tool");
                record_denied(&self.shared, active, &run, call, outcome);
                continue;
            };
            let tool_id = registered.spec.id().clone();
            let args = match registered.schema.validate(
                candidate.arguments_json(),
                self.shared.limits.max_arg_assembly_bytes,
            ) {
                Ok(args) => args,
                Err(_) => {
                    let outcome = denied_outcome("invalid tool arguments");
                    record_denied(&self.shared, active, &run, call, outcome);
                    continue;
                }
            };
            let queued = QueuedCall {
                call: ToolCall::new(
                    active.run.clone(),
                    turn.clone(),
                    call.clone(),
                    tool_id.clone(),
                    args,
                ),
                item_key: candidate.item_key().to_owned(),
                provider_ref: candidate.provider_ref().to_owned(),
                needs_approval: self.shared.policy.requires_approval(&tool_id),
            };
            if let Ok(item) = ModelContextItem::assistant_call(
                candidate.item_key(),
                candidate.provider_ref(),
                queued.call.clone(),
            ) {
                active.conversation.push(item);
            }
            active.queue.push_back(queued);
        }
        Ok(())
    }

    fn next_call_id(&self, active: &mut ActiveRun) -> Result<CallId, Terminal> {
        let call = CallId::new(format!(
            "c{:x}-{}-{}",
            self.shared.incarnation, active.run_n, active.call_seq
        ))
        .map_err(|_| Terminal::failed_internal("call identity is invalid"))?;
        active.call_seq += 1;
        Ok(call)
    }

    async fn on_validating(&self, run: &RunId) -> Result<RunState, Terminal> {
        let mut state = self.shared.state.lock().await;
        let Some(active) = state.active.as_mut() else {
            return Err(Terminal::cancelled());
        };
        if &active.run != run || active.cancelled || active.token.is_cancelled() {
            return Err(Terminal::cancelled());
        }
        if StdInstant::now() >= active.deadline {
            return Err(Terminal::limit("run duration exhausted"));
        }
        let Some(queued) = active.queue.pop_front() else {
            return Ok(RunState::Preparing);
        };
        if queued.needs_approval && !self.shared.has_approval_handler {
            let call = queued.call.call().clone();
            active.outcome_slot = Some((
                call,
                denied_outcome("confirmation required and no approval handler exists"),
            ));
            active.current = Some(CurrentCall {
                call: queued.call,
                item_key: queued.item_key,
                provider_ref: queued.provider_ref,
                binding: None,
            });
            return Ok(RunState::RecordingResult);
        }
        if !queued.needs_approval {
            active.current = Some(CurrentCall {
                call: queued.call,
                item_key: queued.item_key,
                provider_ref: queued.provider_ref,
                binding: None,
            });
            return Ok(RunState::ExecutingTool);
        }
        if self
            .shared
            .limits
            .check_concurrent_ops(active.pending.len())
            .is_err()
        {
            return Err(Terminal::limit("concurrent operation budget exhausted"));
        }
        let approval_n = active.approval_seq;
        active.approval_seq += 1;
        let approval = ApprovalId::new(format!(
            "a{:x}-{}-{approval_n}",
            self.shared.incarnation, active.run_n
        ))
        .map_err(|_| Terminal::failed_internal("approval identity is invalid"))?;
        let scope = self
            .shared
            .policy
            .approval_scope(&queued.call)
            .map_err(Terminal::failed)?;
        let preview = self
            .shared
            .policy
            .approval_preview(&queued.call)
            .map_err(Terminal::failed)?;
        let now_elapsed = active.started.elapsed();
        let expires_at_elapsed = now_elapsed
            .saturating_add(self.shared.limits.approval_expiry)
            .min(self.shared.limits.run_duration);
        let binding = ApprovalBinding::new(
            approval.clone(),
            active.run.clone(),
            queued.call.call().clone(),
            queued.call.tool().clone(),
            queued.call.args().clone(),
            scope,
            expires_at_elapsed,
            self.shared.policy.revision(),
        );
        let summary = format!("run tool {}", queued.call.tool().name());
        let notice = ApprovalNotice::new(
            approval.clone(),
            queued.call.call().clone(),
            summary,
            binding.scope().as_str(),
            expires_at_elapsed,
        )
        .map_err(Terminal::failed)?
        .with_args_preview(preview)
        .map_err(Terminal::failed)?;
        active.pending.insert(
            approval.clone(),
            PendingApproval {
                binding: binding.clone(),
            },
        );
        active.current = Some(CurrentCall {
            call: queued.call,
            item_key: queued.item_key,
            provider_ref: queued.provider_ref,
            binding: Some(binding),
        });
        if emit_control(
            &self.shared,
            active,
            run,
            EventPayload::ApprovalRequired(notice),
        ) == ControlOutcome::Closed
        {
            active.force_terminal.get_or_insert(RunOutcome::Cancelled);
        }
        Ok(RunState::AwaitingApproval)
    }

    async fn on_awaiting(&self, run: &RunId) -> Result<RunState, Terminal> {
        enum Wake {
            Cancelled,
            Deadline,
            Expired,
            Decision(bool),
            Wait(Duration),
            NoCurrent,
        }
        loop {
            let wake = {
                let state = self.shared.state.lock().await;
                let Some(active) = state.active.as_ref() else {
                    return Err(Terminal::cancelled());
                };
                if &active.run != run {
                    return Err(Terminal::cancelled());
                }
                if active.cancelled || active.token.is_cancelled() {
                    Wake::Cancelled
                } else if StdInstant::now() >= active.deadline {
                    Wake::Deadline
                } else if active.current.is_none() {
                    Wake::NoCurrent
                } else if active
                    .current
                    .as_ref()
                    .is_some_and(|current| current.binding.is_none())
                {
                    return Ok(RunState::ExecutingTool);
                } else {
                    let binding = active
                        .current
                        .as_ref()
                        .and_then(|current| current.binding.as_ref())
                        .expect("binding checked above");
                    let approval = binding.approval().clone();
                    let now_elapsed = active.started.elapsed();
                    if binding.is_expired(now_elapsed) {
                        Wake::Expired
                    } else if let Some(decision) = active.decided.get(&approval) {
                        Wake::Decision(*decision)
                    } else {
                        let run_remaining =
                            active.deadline.saturating_duration_since(StdInstant::now());
                        let approval_remaining =
                            binding.expires_at_elapsed().saturating_sub(now_elapsed);
                        Wake::Wait(run_remaining.min(approval_remaining))
                    }
                }
            };
            match wake {
                Wake::NoCurrent => return Ok(RunState::ValidatingTools),
                Wake::Wait(wait_for) => {
                    let wake = {
                        let state = self.shared.state.lock().await;
                        state
                            .active
                            .as_ref()
                            .filter(|active| &active.run == run)
                            .map(|active| active.wake.clone())
                    };
                    let Some(wake) = wake else {
                        return Err(Terminal::cancelled());
                    };
                    if wait_for.is_zero() {
                        continue;
                    }
                    tokio::select! {
                        () = wake.notified() => {}
                        () = tokio::time::sleep(wait_for) => {}
                    }
                }
                Wake::Cancelled => {
                    let mut state = self.shared.state.lock().await;
                    if let Some(active) = state.active.as_mut()
                        && &active.run == run
                    {
                        abandon_pending(active);
                        let call = current_call_id(active);
                        if let Some(call) = call {
                            active.outcome_slot = Some((
                                call,
                                cancelled_outcome("run cancelled while awaiting approval"),
                            ));
                        }
                        active.force_terminal = Some(RunOutcome::Cancelled);
                    }
                    return Ok(RunState::RecordingResult);
                }
                Wake::Deadline => {
                    let mut state = self.shared.state.lock().await;
                    if let Some(active) = state.active.as_mut()
                        && &active.run == run
                    {
                        abandon_pending(active);
                        let call = current_call_id(active);
                        if let Some(call) = call {
                            active.outcome_slot = Some((
                                call,
                                timeout_outcome("run deadline passed while awaiting approval"),
                            ));
                        }
                        active.force_terminal = Some(RunOutcome::LimitReached);
                    }
                    return Ok(RunState::RecordingResult);
                }
                Wake::Expired => {
                    let mut state = self.shared.state.lock().await;
                    if let Some(active) = state.active.as_mut()
                        && &active.run == run
                    {
                        let approval = active
                            .current
                            .as_ref()
                            .and_then(|current| current.binding.as_ref())
                            .map(|binding| binding.approval().clone());
                        if let Some(approval) = approval {
                            active.pending.remove(&approval);
                        }
                        let call = current_call_id(active);
                        if let Some(call) = call {
                            active.outcome_slot = Some((call, denied_outcome("approval expired")));
                        }
                    }
                    return Ok(RunState::RecordingResult);
                }
                Wake::Decision(decision) => {
                    let mut state = self.shared.state.lock().await;
                    if let Some(active) = state.active.as_mut()
                        && &active.run == run
                    {
                        let approval = active
                            .current
                            .as_ref()
                            .and_then(|current| current.binding.as_ref())
                            .map(|binding| binding.approval().clone());
                        if let Some(approval) = approval {
                            active.decided.remove(&approval);
                        }
                        if !decision {
                            let call = current_call_id(active);
                            if let Some(call) = call {
                                active.outcome_slot =
                                    Some((call, denied_outcome("approval refused")));
                            }
                        }
                    }
                    if decision {
                        return Ok(RunState::ExecutingTool);
                    }
                    return Ok(RunState::RecordingResult);
                }
            }
        }
    }

    async fn on_executing(&self, run: &RunId) -> Result<RunState, Terminal> {
        let setup = {
            let mut state = self.shared.state.lock().await;
            let Some(active) = state.active.as_mut() else {
                return Err(Terminal::cancelled());
            };
            if &active.run != run {
                return Err(Terminal::cancelled());
            }
            let Some(current) = active.current.as_ref() else {
                return Ok(RunState::ValidatingTools);
            };
            let call = current.call.clone();
            let Some(registered) = self.shared.tools.get(call.tool().name()) else {
                active.outcome_slot = Some((call.call().clone(), denied_outcome("unknown tool")));
                return Ok(RunState::RecordingResult);
            };
            if !registered.spec.id().is_compatible_with(call.tool()) {
                active.outcome_slot =
                    Some((call.call().clone(), denied_outcome("tool revision changed")));
                return Ok(RunState::RecordingResult);
            }
            if active.cancelled || active.token.is_cancelled() {
                active.outcome_slot = Some((
                    call.call().clone(),
                    cancelled_outcome("cancelled before dispatch"),
                ));
                active.force_terminal = Some(RunOutcome::Cancelled);
                return Ok(RunState::RecordingResult);
            }
            let now = StdInstant::now();
            if now >= active.deadline {
                active.outcome_slot = Some((
                    call.call().clone(),
                    timeout_outcome("run deadline passed before dispatch"),
                ));
                active.force_terminal = Some(RunOutcome::LimitReached);
                return Ok(RunState::RecordingResult);
            }
            let now_elapsed = active.started.elapsed();
            if let Some(binding) = current.binding.as_ref()
                && binding
                    .check_valid_for_dispatch(
                        call.run(),
                        call.call(),
                        call.tool(),
                        call.args(),
                        self.shared.policy.revision(),
                        now_elapsed,
                    )
                    .is_err()
            {
                active.outcome_slot = Some((
                    call.call().clone(),
                    denied_outcome("approval binding mismatch"),
                ));
                return Ok(RunState::RecordingResult);
            }
            let scope = match current.binding.as_ref() {
                Some(binding) => binding.scope().clone(),
                None => match self.shared.policy.authorize(&call) {
                    Ok(scope) => scope,
                    Err(_) => {
                        active.outcome_slot = Some((
                            call.call().clone(),
                            denied_outcome("policy no longer authorizes dispatch"),
                        ));
                        return Ok(RunState::RecordingResult);
                    }
                },
            };
            let tool_deadline = now
                .checked_add(self.shared.limits.per_tool_timeout)
                .map_or(active.deadline, |candidate| candidate.min(active.deadline));
            let context = ToolContext::new(
                self.shared.effective_output_budget,
                tool_deadline.saturating_duration_since(active.started),
                false,
                scope,
            )
            .map_err(|_| Terminal::failed_internal("tool context is invalid"))?
            .with_control(active.token.clone(), tool_deadline);
            if emit_control(
                &self.shared,
                active,
                run,
                EventPayload::ToolStarted(ToolStartedInfo {
                    call: call.call().clone(),
                }),
            ) == ControlOutcome::Closed
            {
                active.force_terminal.get_or_insert(RunOutcome::Cancelled);
            }
            let tool = registered.port.clone();
            let wake = active.wake.clone();
            let token = active.token.clone();
            (call, tool, context, tool_deadline, wake, token)
        };
        let (call, tool, context, tool_deadline, wake, token) = setup;
        let call_id = call.call().clone();
        let mut handle = tokio::task::spawn_blocking(move || tool.execute(&call, &context));
        let mut cancelled_first = false;
        let joined = loop {
            if token.is_cancelled() {
                cancelled_first = true;
                break None;
            }
            tokio::select! {
                biased;
                joined = &mut handle => break Some(joined),
                () = wake.notified() => {
                    if token.is_cancelled() {
                        cancelled_first = true;
                        break None;
                    }
                }
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(tool_deadline)) => break None,
            }
        };
        let mut state = self.shared.state.lock().await;
        let Some(active) = state.active.as_mut() else {
            return Err(Terminal::cancelled());
        };
        if &active.run != run {
            return Err(Terminal::cancelled());
        }
        match joined {
            Some(Ok(outcome)) => {
                active.outcome_slot = Some((call_id, bound_outcome(&self.shared, outcome)));
                Ok(RunState::RecordingResult)
            }
            Some(Err(join)) => {
                let outcome = if join.is_panic() {
                    failed_outcome("tool worker panicked with unknown effects")
                } else {
                    cancelled_outcome("tool worker ended without an outcome")
                };
                active.outcome_slot = Some((call_id, outcome));
                Ok(RunState::RecordingResult)
            }
            None => {
                let cancelled = cancelled_first || active.cancelled || token.is_cancelled();
                let outcome = if cancelled {
                    cancelled_outcome("run cancelled while tool termination was unconfirmed")
                } else {
                    timeout_outcome("tool deadline exceeded; termination unconfirmed")
                };
                active.outcome_slot = Some((call_id.clone(), outcome));
                active.force_terminal = Some(if cancelled {
                    RunOutcome::Cancelled
                } else {
                    RunOutcome::LimitReached
                });
                drop(state);
                // Ownership is retained: the worker stays quarantined until
                // it actually terminates, and no next call or new run can
                // start meanwhile.
                self.quarantine_tool(run, call_id, handle).await;
                Ok(RunState::RecordingResult)
            }
        }
    }

    async fn on_recording(&self, run: &RunId) -> Result<RunState, Terminal> {
        let mut state = self.shared.state.lock().await;
        let Some(active) = state.active.as_mut() else {
            return Err(Terminal::cancelled());
        };
        if &active.run != run {
            return Err(Terminal::cancelled());
        }
        let Some((call, outcome)) = active.outcome_slot.take() else {
            return Ok(RunState::ValidatingTools);
        };
        active.outcomes.push(OutcomeSummary {
            call: call.clone(),
            status: outcome.status(),
            effect: outcome.effect(),
            evidence: outcome.evidence(),
        });
        if let Some(current) = active.current.take()
            && let Ok(item) = ModelContextItem::tool_result(
                call.clone(),
                current.item_key,
                current.provider_ref,
                current.call.tool().clone(),
                outcome.clone(),
            )
        {
            active.conversation.push(item);
        }
        if emit_control(
            &self.shared,
            active,
            run,
            EventPayload::ToolFinished(ToolFinishedInfo { call, outcome }),
        ) == ControlOutcome::Closed
        {
            active.force_terminal.get_or_insert(RunOutcome::Cancelled);
        }
        if let Some(outcome) = active.force_terminal.take() {
            active.terminal = Some(outcome);
            return Ok(RunState::Finished);
        }
        if active.queue.is_empty() {
            Ok(RunState::Preparing)
        } else {
            Ok(RunState::ValidatingTools)
        }
    }

    async fn set_phase(&self, run: &RunId, phase: RunState) {
        let mut state = self.shared.state.lock().await;
        if let Some(active) = state.active.as_mut()
            && &active.run == run
        {
            active.phase = phase;
        }
    }

    async fn set_terminal(&self, run: &RunId, outcome: RunOutcome, error: Option<AgentError>) {
        let mut state = self.shared.state.lock().await;
        let Some(active) = state.active.as_mut() else {
            return;
        };
        if &active.run != run || active.terminal.is_some() {
            return;
        }
        active.terminal = Some(outcome);
        active.terminal_error = error;
    }

    async fn take_terminal(&self, run: &RunId) -> (RunOutcome, Option<AgentError>) {
        let state = self.shared.state.lock().await;
        match state.active.as_ref() {
            Some(active) if &active.run == run => (
                active.terminal.unwrap_or(RunOutcome::Cancelled),
                active.terminal_error.clone(),
            ),
            _ => (RunOutcome::Cancelled, None),
        }
    }

    async fn publish_control(&self, run: &RunId, payload: EventPayload) -> Result<(), Terminal> {
        let mut state = self.shared.state.lock().await;
        let Some(active) = state.active.as_mut() else {
            return Err(Terminal::cancelled());
        };
        if &active.run != run {
            return Err(Terminal::cancelled());
        }
        if emit_control(&self.shared, active, run, payload) == ControlOutcome::Closed {
            active.delivery_closed = true;
        }
        Ok(())
    }

    async fn finish_run(&self, run: &RunId, outcome: RunOutcome, error: Option<AgentError>) {
        let mut state = self.shared.state.lock().await;
        let Some(mut active) = state.active.take() else {
            return;
        };
        if active.run != *run || active.finished_sent {
            state.active = Some(active);
            return;
        }
        if let Some((call, decided)) = active.outcome_slot.take() {
            active.outcomes.push(OutcomeSummary {
                call: call.clone(),
                status: decided.status(),
                effect: decided.effect(),
                evidence: decided.evidence(),
            });
            emit_control(
                &self.shared,
                &mut active,
                run,
                EventPayload::ToolFinished(ToolFinishedInfo {
                    call,
                    outcome: decided,
                }),
            );
        }
        // Defensive: a supervised driver failure can end the run while a
        // call was admitted but no outcome was recorded. The honest record
        // is an unknown cancellation, never a fabricated success.
        if let Some(current) = active.current.take() {
            let call = current.call.call().clone();
            if !active.outcomes.iter().any(|summary| summary.call == call) {
                let decided = cancelled_outcome("run ended before the tool outcome was recorded");
                active.outcomes.push(OutcomeSummary {
                    call: call.clone(),
                    status: decided.status(),
                    effect: decided.effect(),
                    evidence: decided.evidence(),
                });
                emit_control(
                    &self.shared,
                    &mut active,
                    run,
                    EventPayload::ToolFinished(ToolFinishedInfo {
                        call,
                        outcome: decided,
                    }),
                );
            }
        }
        let finished = RunFinished::new(outcome, PersistenceState::Ephemeral, None)
            .expect("ephemeral terminal record builds");
        let finished = match error.clone() {
            Some(error) => finished.with_error(error),
            None => finished,
        };
        let delivery = emit_control(
            &self.shared,
            &mut active,
            run,
            EventPayload::RunFinished(finished),
        );
        let terminal_delivered = delivery == ControlOutcome::Sent;
        let delivery_closed =
            delivery == ControlOutcome::Closed || self.shared.control_closed.load(Ordering::SeqCst);
        if delivery_closed {
            self.shared
                .outbox
                .lock()
                .expect("control outbox readable")
                .clear();
        }
        active.finished_sent = true;
        active.phase = RunState::Finished;
        state.last = Some(FinishedRun {
            run: active.run.clone(),
            session: active.session.clone(),
            outcome,
            last_seq: active.last_seq,
            outcomes: active.outcomes.clone(),
            truncated: active.data_dropped || delivery_closed,
            terminal_delivered,
        });
    }

    async fn on_driver_failure(&self, run: &RunId, join: tokio::task::JoinError) {
        let message = if join.is_panic() {
            "run driver panicked"
        } else {
            "run driver ended without a terminal outcome"
        };
        self.finish_run(
            run,
            RunOutcome::Failed,
            Some(runtime_error(ErrorCategory::Internal, message)),
        )
        .await;
    }

    async fn quarantine_provider(&self, run: &RunId, handle: JoinHandle<Vec<ProviderEvent>>) {
        {
            let mut state = self.shared.state.lock().await;
            state.quarantine.push(QuarantineMeta {
                run: run.clone(),
                call: None,
            });
        }
        let shared = self.shared.clone();
        let run = run.clone();
        tokio::spawn(async move {
            let _ = handle.await;
            let mut state = shared.state.lock().await;
            state
                .quarantine
                .retain(|meta| !(meta.run == run && meta.call.is_none()));
        });
    }

    async fn quarantine_tool(&self, run: &RunId, call: CallId, handle: JoinHandle<ToolOutcome>) {
        {
            let mut state = self.shared.state.lock().await;
            state.quarantine.push(QuarantineMeta {
                run: run.clone(),
                call: Some(call.clone()),
            });
        }
        let shared = self.shared.clone();
        let run = run.clone();
        tokio::spawn(async move {
            let joined = handle.await;
            let mut state = shared.state.lock().await;
            state
                .quarantine
                .retain(|meta| !(meta.run == run && meta.call.as_ref() == Some(&call)));
            // Late evidence is folded into the retained snapshot without a
            // duplicate ToolFinished event. Only an inconclusive effect is
            // refined; a recorded known effect is never rewritten.
            if let Ok(outcome) = joined
                && let Some(last) = state.last.as_mut()
                && last.run == run
                && let Some(summary) = last
                    .outcomes
                    .iter_mut()
                    .find(|summary| summary.call == call)
                && summary.effect == EffectState::Unknown
            {
                summary.status = outcome.status();
                summary.effect = outcome.effect();
                summary.evidence = outcome.evidence();
            }
        });
    }
}

fn current_call_id(active: &ActiveRun) -> Option<CallId> {
    active
        .current
        .as_ref()
        .map(|current| current.call.call().clone())
}

fn abandon_pending(active: &mut ActiveRun) {
    active.pending.clear();
}

fn append_assistant_text(active: &mut ActiveRun, text_items: Vec<(String, String)>) {
    for (item_key, text) in text_items {
        if let Ok(item) = ModelContextItem::assistant_text(item_key, text) {
            active.conversation.push(item);
        }
    }
}

fn record_denied(
    shared: &Arc<Shared>,
    active: &mut ActiveRun,
    run: &RunId,
    call: CallId,
    outcome: ToolOutcome,
) {
    active.outcomes.push(OutcomeSummary {
        call: call.clone(),
        status: outcome.status(),
        effect: outcome.effect(),
        evidence: outcome.evidence(),
    });
    if emit_control(
        shared,
        active,
        run,
        EventPayload::ToolFinished(ToolFinishedInfo { call, outcome }),
    ) == ControlOutcome::Closed
    {
        active.force_terminal.get_or_insert(RunOutcome::Cancelled);
    }
}

fn bound_outcome(shared: &Arc<Shared>, outcome: ToolOutcome) -> ToolOutcome {
    let budget = shared.effective_output_budget;
    if outcome.content().len() <= budget {
        return outcome;
    }
    let mut content = outcome.content().to_owned();
    let mut end = budget;
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    content.truncate(end);
    ToolOutcome::from_bounded_content(
        outcome.status(),
        outcome.effect(),
        outcome.evidence(),
        content,
        true,
    )
    .expect("bounded outcome preserves the original valid fields")
}

/// Per-counter final usage merge: a terminal `Some` wins, a terminal `None`
/// retains the last final `Some`, and a counter with no known final value
/// stays unknown instead of being downgraded to zero.
fn merge_final_usage(last: Option<Usage>, terminal: Usage) -> Usage {
    let last_final = last.filter(|usage| usage.finality() == nexus_core::UsageFinality::Final);
    let input = terminal
        .input_tokens()
        .or_else(|| last_final.and_then(|usage| usage.input_tokens()));
    let output = terminal
        .output_tokens()
        .or_else(|| last_final.and_then(|usage| usage.output_tokens()));
    Usage::new(input, output, nexus_core::UsageFinality::Final)
}

fn denied_outcome(message: &'static str) -> ToolOutcome {
    ToolOutcome::new(
        ExecutionStatus::Denied,
        EffectState::NotStarted,
        Evidence::HostObserved,
        message,
        false,
    )
    .expect("static safe denial builds")
}

fn cancelled_outcome(message: &'static str) -> ToolOutcome {
    ToolOutcome::new(
        ExecutionStatus::Cancelled,
        EffectState::Unknown,
        Evidence::Uncertain,
        message,
        false,
    )
    .expect("static safe cancellation builds")
}

fn timeout_outcome(message: &'static str) -> ToolOutcome {
    ToolOutcome::new(
        ExecutionStatus::TimedOut,
        EffectState::Unknown,
        Evidence::Uncertain,
        message,
        false,
    )
    .expect("static safe timeout builds")
}

fn failed_outcome(message: &'static str) -> ToolOutcome {
    ToolOutcome::new(
        ExecutionStatus::Failed,
        EffectState::Unknown,
        Evidence::Uncertain,
        message,
        false,
    )
    .expect("static safe failure builds")
}

fn run_event(active: &ActiveRun, seq: EventSequence, payload: EventPayload) -> RunEvent {
    RunEvent::new(active.session.clone(), active.run.clone(), seq, payload)
}

fn commit_sequence(active: &mut ActiveRun, seq: EventSequence) {
    if let Some(next) = nexus_core::commands::checked_next_sequence(seq) {
        active.next_seq = next;
        active.last_seq = Some(seq);
    } else {
        active.data_dropped = true;
    }
}

/// Flushes coalesced text as one sequenced data event. A full or closed data
/// channel drops presentation traffic without consuming a sequence number,
/// so delivered sequences stay gap-free; control flow continues.
fn flush_text(shared: &Arc<Shared>, active: &mut ActiveRun) {
    let Some(fragment) = active.pending_text.take() else {
        return;
    };
    let seq = active.next_seq;
    let event = run_event(active, seq, EventPayload::AssistantTextDelta(fragment));
    match shared.data_tx.try_reserve() {
        Ok(permit) => {
            permit.send(event);
            commit_sequence(active, seq);
        }
        Err(_) => active.data_dropped = true,
    }
}

/// Emits one data event after flushing coalesced text, preserving order.
fn emit_data(shared: &Arc<Shared>, active: &mut ActiveRun, run: &RunId, payload: EventPayload) {
    if &active.run != run {
        return;
    }
    flush_text(shared, active);
    let seq = active.next_seq;
    match shared.data_tx.try_reserve() {
        Ok(permit) => {
            permit.send(run_event(active, seq, payload));
            commit_sequence(active, seq);
        }
        Err(_) => active.data_dropped = true,
    }
}

/// Buffers one text fragment, coalescing with the adjacent same-item tail.
/// Batching never crosses turn, item, or lifecycle boundaries.
fn buffer_text(shared: &Arc<Shared>, active: &mut ActiveRun, run: &RunId, fragment: AssistantText) {
    if &active.run != run {
        return;
    }
    let Some(tail) = active.pending_text.take() else {
        active.pending_text = Some(fragment);
        return;
    };
    if tail.turn == fragment.turn && tail.item_key == fragment.item_key {
        let merged_text = format!("{}{}", tail.text, fragment.text);
        match AssistantText::new(tail.turn.clone(), tail.item_key.clone(), merged_text) {
            Ok(merged) => active.pending_text = Some(merged),
            Err(_) => {
                emit_data(shared, active, run, EventPayload::AssistantTextDelta(tail));
                active.pending_text = Some(fragment);
            }
        }
        return;
    }
    emit_data(shared, active, run, EventPayload::AssistantTextDelta(tail));
    active.pending_text = Some(fragment);
}

/// Publishes one control event with a contiguous per-run sequence committed
/// only after a successful enqueue or a committed outbox entry. A full
/// channel buffers into the bounded outbox (retained ownership until
/// drained); a closed channel is classified separately and never reported as
/// saturation.
fn emit_control(
    shared: &Arc<Shared>,
    active: &mut ActiveRun,
    run: &RunId,
    payload: EventPayload,
) -> ControlOutcome {
    if &active.run != run {
        return ControlOutcome::Closed;
    }
    debug_assert!(crate::transport::is_control_payload(&payload));
    flush_text(shared, active);
    let seq = active.next_seq;
    let event = run_event(active, seq, payload);
    let outcome = deliver_control(shared, event);
    if outcome != ControlOutcome::Closed {
        commit_sequence(active, seq);
    }
    outcome
}

fn deliver_control(shared: &Arc<Shared>, event: RunEvent) -> ControlOutcome {
    if shared.control_closed.load(Ordering::SeqCst) {
        return ControlOutcome::Closed;
    }
    match shared.control_tx.try_reserve() {
        Ok(permit) => {
            permit.send(event);
            ControlOutcome::Sent
        }
        Err(TrySendError::Full(_)) => {
            shared
                .outbox
                .lock()
                .expect("control outbox readable")
                .push_back(event);
            ensure_flusher(shared);
            ControlOutcome::Buffered
        }
        Err(TrySendError::Closed(_)) => {
            shared.control_closed.store(true, Ordering::SeqCst);
            ControlOutcome::Closed
        }
    }
}

fn ensure_flusher(shared: &Arc<Shared>) {
    if shared.flushing.swap(true, Ordering::SeqCst) {
        return;
    }
    let shared = shared.clone();
    tokio::spawn(async move { flush_control(shared).await });
}

async fn flush_control(shared: Arc<Shared>) {
    loop {
        let next = shared
            .outbox
            .lock()
            .expect("control outbox readable")
            .pop_front();
        let Some(event) = next else {
            break;
        };
        match shared.control_tx.reserve().await {
            Ok(permit) => {
                let terminal = event.is_terminal();
                permit.send(event);
                if terminal {
                    let mut state = shared.state.lock().await;
                    if let Some(last) = state.last.as_mut() {
                        last.terminal_delivered = true;
                    }
                }
            }
            Err(_) => {
                shared
                    .outbox
                    .lock()
                    .expect("control outbox readable")
                    .push_front(event);
                shared.control_closed.store(true, Ordering::SeqCst);
                break;
            }
        }
    }
    shared.flushing.store(false, Ordering::SeqCst);
    if !shared
        .outbox
        .lock()
        .expect("control outbox readable")
        .is_empty()
    {
        ensure_flusher(&shared);
    }
}

fn snapshot_of_active(active: &ActiveRun) -> Option<Snapshot> {
    let now = active.started.elapsed();
    let pending: Vec<ApprovalId> = active
        .pending
        .iter()
        .filter(|(_, pending)| !pending.binding.is_expired(now))
        .map(|(approval, _)| approval.clone())
        .collect();
    snapshot_view(
        &active.session,
        &active.run,
        active.last_seq,
        RunLifecycle::Active,
        pending,
        active.outcomes.clone(),
        active.data_dropped || active.delivery_closed,
    )
}

fn snapshot_of_finished(last: &FinishedRun) -> Option<Snapshot> {
    snapshot_view(
        &last.session,
        &last.run,
        last.last_seq,
        RunLifecycle::Finalized(last.outcome),
        Vec::new(),
        last.outcomes.clone(),
        last.truncated,
    )
}

fn snapshot_view(
    session: &SessionId,
    run: &RunId,
    last_seq: Option<EventSequence>,
    lifecycle: RunLifecycle,
    pending: Vec<ApprovalId>,
    mut known: Vec<OutcomeSummary>,
    truncated: bool,
) -> Option<Snapshot> {
    let truncated = if known.len() > Limits::M0_TEST_TOOL_CALLS_PER_RUN as usize {
        let excess = known.len() - Limits::M0_TEST_TOOL_CALLS_PER_RUN as usize;
        known.drain(..excess);
        true
    } else {
        truncated
    };
    Snapshot::new(
        session.clone(),
        run.clone(),
        last_seq,
        lifecycle,
        pending,
        known,
        truncated,
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::event_is_live;
    use nexus_core::{ApprovedScope, ProviderCapabilities};
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::AtomicUsize;

    struct FakeProvider {
        calls: AtomicUsize,
        script: StdMutex<VecDeque<Vec<ProviderEvent>>>,
        delay: Duration,
    }

    impl FakeProvider {
        fn new(script: Vec<Vec<ProviderEvent>>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                script: StdMutex::new(script.into()),
                delay: Duration::ZERO,
            }
        }

        fn with_delay(script: Vec<Vec<ProviderEvent>>, delay: Duration) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                script: StdMutex::new(script.into()),
                delay,
            }
        }

        fn stop_turn(text: &str) -> Vec<ProviderEvent> {
            vec![
                ProviderEvent::TextDelta {
                    item_key: "item-0".to_owned(),
                    text: text.to_owned(),
                },
                ProviderEvent::TurnFinished(nexus_core::TurnFinished::new(
                    FinishReason::Stop,
                    Usage::new(None, None, nexus_core::UsageFinality::Final),
                    None,
                )),
            ]
        }

        fn tool_turn(candidates: Vec<CallCandidate>) -> Vec<ProviderEvent> {
            let mut events: Vec<ProviderEvent> = candidates
                .into_iter()
                .map(ProviderEvent::ToolCallReady)
                .collect();
            events.push(ProviderEvent::TurnFinished(nexus_core::TurnFinished::new(
                FinishReason::ToolCalls,
                Usage::new(None, None, nexus_core::UsageFinality::Final),
                None,
            )));
            events
        }

        fn partial_only_turn() -> Vec<ProviderEvent> {
            vec![
                ProviderEvent::ToolCallDelta {
                    item_key: "item-0".to_owned(),
                    assembled_bytes: 12,
                },
                ProviderEvent::TurnFinished(nexus_core::TurnFinished::new(
                    FinishReason::Stop,
                    Usage::new(None, None, nexus_core::UsageFinality::Final),
                    None,
                )),
            ]
        }
    }

    impl ProviderPort for FakeProvider {
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

        fn stream(
            &self,
            _request: &ModelRequest,
            _context: &ProviderContext,
        ) -> Vec<ProviderEvent> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            self.script
                .lock()
                .expect("script readable")
                .pop_front()
                .unwrap_or_else(|| Self::stop_turn("idle"))
        }
    }

    struct PanicProvider;

    impl ProviderPort for PanicProvider {
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

        fn stream(
            &self,
            _request: &ModelRequest,
            _context: &ProviderContext,
        ) -> Vec<ProviderEvent> {
            panic!("provider worker panic")
        }
    }

    struct FakeTool {
        spec: ToolSpec,
        executed: AtomicUsize,
        seen_args: StdMutex<Vec<String>>,
        outcome: ToolOutcome,
        delay: Duration,
    }

    impl FakeTool {
        fn succeeding(name: &str, revision: u32) -> Self {
            Self::with_outcome(
                name,
                revision,
                ToolOutcome::new(
                    ExecutionStatus::Succeeded,
                    EffectState::KnownApplied,
                    Evidence::HostObserved,
                    "ok",
                    false,
                )
                .expect("fake outcome builds"),
            )
        }

        fn with_outcome(name: &str, revision: u32, outcome: ToolOutcome) -> Self {
            Self {
                spec: ToolSpec::new(
                    ToolId::new(name, revision).expect("valid"),
                    format!("fake {name}"),
                    r#"{"type":"object"}"#,
                )
                .expect("fake spec builds"),
                executed: AtomicUsize::new(0),
                seen_args: StdMutex::new(Vec::new()),
                outcome,
                delay: Duration::ZERO,
            }
        }

        fn slow(name: &str, delay: Duration) -> Self {
            let mut tool = Self::succeeding(name, nexus_core::M0_REVISION);
            tool.delay = delay;
            tool
        }
    }

    impl ToolPort for FakeTool {
        fn describe(&self) -> ToolSpec {
            self.spec.clone()
        }

        fn execute(&self, call: &ToolCall, _context: &ToolContext) -> ToolOutcome {
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            self.executed.fetch_add(1, Ordering::SeqCst);
            self.seen_args
                .lock()
                .expect("args readable")
                .push(call.args().as_str().to_owned());
            self.outcome.clone()
        }
    }

    struct PanicTool {
        spec: ToolSpec,
    }

    impl PanicTool {
        fn new(name: &str) -> Self {
            Self {
                spec: ToolSpec::new(
                    ToolId::new(name, nexus_core::M0_REVISION).expect("valid"),
                    "panicking fake",
                    r#"{"type":"object"}"#,
                )
                .expect("spec builds"),
            }
        }
    }

    impl ToolPort for PanicTool {
        fn describe(&self) -> ToolSpec {
            self.spec.clone()
        }

        fn execute(&self, _call: &ToolCall, _context: &ToolContext) -> ToolOutcome {
            panic!("tool worker panic")
        }
    }

    struct BlockingTool {
        spec: ToolSpec,
        entered: std::sync::mpsc::Sender<()>,
        release: StdMutex<std::sync::mpsc::Receiver<()>>,
        executions: AtomicUsize,
    }

    impl BlockingTool {
        fn new() -> (
            Self,
            std::sync::mpsc::Receiver<()>,
            std::sync::mpsc::Sender<()>,
        ) {
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            (
                Self {
                    spec: ToolSpec::new(
                        ToolId::new("host_read", nexus_core::M0_REVISION).expect("valid"),
                        "blocking fake",
                        r#"{"type":"object"}"#,
                    )
                    .expect("spec builds"),
                    entered: entered_tx,
                    release: StdMutex::new(release_rx),
                    executions: AtomicUsize::new(0),
                },
                entered_rx,
                release_tx,
            )
        }
    }

    impl ToolPort for BlockingTool {
        fn describe(&self) -> ToolSpec {
            self.spec.clone()
        }

        fn execute(&self, _call: &ToolCall, context: &ToolContext) -> ToolOutcome {
            self.executions.fetch_add(1, Ordering::SeqCst);
            let _ = self.entered.send(());
            let release = self.release.lock().expect("release lock");
            let _ = release.recv_timeout(Duration::from_secs(5));
            if context.is_cancelled() {
                cancelled_outcome("blocked tool observed cancellation")
            } else {
                ToolOutcome::new(
                    ExecutionStatus::Succeeded,
                    EffectState::KnownApplied,
                    Evidence::HostObserved,
                    "late ok",
                    false,
                )
                .expect("outcome builds")
            }
        }
    }

    struct BlockingProvider {
        entered: std::sync::mpsc::Sender<()>,
        release: StdMutex<std::sync::mpsc::Receiver<()>>,
        calls: AtomicUsize,
    }

    impl BlockingProvider {
        fn new() -> (
            Self,
            std::sync::mpsc::Receiver<()>,
            std::sync::mpsc::Sender<()>,
        ) {
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            (
                Self {
                    entered: entered_tx,
                    release: StdMutex::new(release_rx),
                    calls: AtomicUsize::new(0),
                },
                entered_rx,
                release_tx,
            )
        }
    }

    impl ProviderPort for BlockingProvider {
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

        fn stream(&self, _request: &ModelRequest, context: &ProviderContext) -> Vec<ProviderEvent> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call > 0 {
                return FakeProvider::stop_turn("later");
            }
            let _ = self.entered.send(());
            let release = self.release.lock().expect("release lock");
            let _ = release.recv_timeout(Duration::from_secs(5));
            if context.is_cancelled() {
                vec![ProviderEvent::Failed(
                    AgentError::new(
                        ErrorCategory::Cancelled,
                        "blocked provider observed cancellation",
                        RetryGuidance::DoNotRetry,
                    )
                    .expect("error builds"),
                )]
            } else {
                FakeProvider::stop_turn("late")
            }
        }
    }

    struct Bed {
        runtime: Runtime,
        data: mpsc::Receiver<RunEvent>,
        control: mpsc::Receiver<RunEvent>,
        provider: Arc<FakeProvider>,
        read_tool: Arc<FakeTool>,
        write_tool: Arc<FakeTool>,
    }

    struct Collected {
        data: Vec<RunEvent>,
        control: Vec<RunEvent>,
    }

    fn bed_with(
        script: Vec<Vec<ProviderEvent>>,
        config: RuntimeConfig,
        slow_provider: Duration,
    ) -> Bed {
        let provider = Arc::new(FakeProvider::with_delay(script, slow_provider));
        let read_tool = Arc::new(FakeTool::succeeding("host_read", nexus_core::M0_REVISION));
        let write_tool = Arc::new(FakeTool::succeeding("host_write", nexus_core::M0_REVISION));
        let tools: Vec<Arc<dyn ToolPort + Send + Sync>> =
            vec![read_tool.clone(), write_tool.clone()];
        let (runtime, streams) = Runtime::new(config, provider.clone(), tools);
        Bed {
            runtime,
            data: streams.data,
            control: streams.control,
            provider,
            read_tool,
            write_tool,
        }
    }

    fn test_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime builds")
    }

    fn quick_config() -> RuntimeConfig {
        RuntimeConfig {
            limits: Limits::m0_test(),
            policy: Policy::m0_test(),
            has_approval_handler: true,
        }
    }

    fn submit_cmd(tag: &str) -> SubmitCommand {
        SubmitCommand::new(
            RequestId::new(format!("req-{tag}")).expect("valid"),
            SessionId::new("sess-1").expect("valid"),
            "do work",
            "test-profile",
        )
        .expect("submit builds")
    }

    async fn collect_until_finished(
        data: &mut mpsc::Receiver<RunEvent>,
        control: &mut mpsc::Receiver<RunEvent>,
    ) -> (Collected, RunFinished) {
        let mut datas = Vec::new();
        let mut controls = Vec::new();
        let finished = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                tokio::select! {
                    event = data.recv() => {
                        if let Some(event) = event {
                            datas.push(event);
                        }
                    }
                    event = control.recv() => {
                        match event {
                            Some(event) => {
                                let terminal = matches!(
                                    event.payload(),
                                    EventPayload::RunFinished(_)
                                );
                                let finished = if terminal {
                                    match event.payload() {
                                        EventPayload::RunFinished(finished) => Some(finished.clone()),
                                        _ => None,
                                    }
                                } else {
                                    None
                                };
                                controls.push(event);
                                if let Some(finished) = finished {
                                    break finished;
                                }
                            }
                            None => panic!("control channel closed before terminal"),
                        }
                    }
                }
            }
        })
        .await
        .expect("run reaches terminal promptly");
        while let Ok(event) = data.try_recv() {
            datas.push(event);
        }
        (
            Collected {
                data: datas,
                control: controls,
            },
            finished,
        )
    }

    async fn collect_until_approval_or_finished(
        data: &mut mpsc::Receiver<RunEvent>,
        control: &mut mpsc::Receiver<RunEvent>,
    ) -> Collected {
        let mut datas = Vec::new();
        let mut controls = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                tokio::select! {
                    event = data.recv() => {
                        if let Some(event) = event {
                            datas.push(event);
                        }
                    }
                    event = control.recv() => {
                        match event {
                            Some(event) => {
                                let stop = matches!(
                                    event.payload(),
                                    EventPayload::ApprovalRequired(_)
                                        | EventPayload::RunFinished(_)
                                );
                                controls.push(event);
                                if stop {
                                    break;
                                }
                            }
                            None => panic!("control channel closed"),
                        }
                    }
                }
            }
        })
        .await
        .expect("approval or terminal arrives promptly");
        Collected {
            data: datas,
            control: controls,
        }
    }

    async fn collect_until_tool_started(
        data: &mut mpsc::Receiver<RunEvent>,
        control: &mut mpsc::Receiver<RunEvent>,
    ) -> Collected {
        let mut datas = Vec::new();
        let mut controls = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                tokio::select! {
                    event = data.recv() => {
                        if let Some(event) = event {
                            datas.push(event);
                        }
                    }
                    event = control.recv() => {
                        match event {
                            Some(event) => {
                                let stop = matches!(
                                    event.payload(),
                                    EventPayload::ToolStarted(_) | EventPayload::RunFinished(_)
                                );
                                controls.push(event);
                                if stop {
                                    break;
                                }
                            }
                            None => panic!("control channel closed"),
                        }
                    }
                }
            }
        })
        .await
        .expect("tool start arrives promptly");
        Collected {
            data: datas,
            control: controls,
        }
    }

    fn candidate(tool: &str, args: &str) -> CallCandidate {
        CallCandidate::new("item-0", "prov-ref-0", tool, args).expect("valid")
    }

    fn tool_started_calls(collected: &Collected) -> Vec<CallId> {
        collected
            .control
            .iter()
            .filter_map(|event| match event.payload() {
                EventPayload::ToolStarted(info) => Some(info.call.clone()),
                _ => None,
            })
            .collect()
    }

    fn tool_finished_map(collected: &Collected) -> HashMap<CallId, ToolOutcome> {
        collected
            .control
            .iter()
            .filter_map(|event| match event.payload() {
                EventPayload::ToolFinished(info) => Some((info.call.clone(), info.outcome.clone())),
                _ => None,
            })
            .collect()
    }

    fn find_approval(collected: &Collected) -> (ApprovalId, CallId) {
        collected
            .control
            .iter()
            .find_map(|event| match event.payload() {
                EventPayload::ApprovalRequired(notice) => {
                    Some((notice.approval.clone(), notice.call.clone()))
                }
                _ => None,
            })
            .expect("approval requested")
    }

    #[test]
    fn try_new_rejects_duplicate_names_and_wrong_revision() {
        let provider = Arc::new(FakeProvider::new(vec![]));
        let duplicate: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![
            Arc::new(FakeTool::succeeding("host_read", nexus_core::M0_REVISION)),
            Arc::new(FakeTool::succeeding("host_read", nexus_core::M0_REVISION)),
        ];
        assert!(
            Runtime::try_new(quick_config(), provider.clone(), duplicate).is_err(),
            "duplicate registrations fail explicitly"
        );
        let wrong_revision: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![Arc::new(
            FakeTool::succeeding("host_read", nexus_core::M0_REVISION + 1),
        )];
        assert!(
            Runtime::try_new(quick_config(), provider, wrong_revision).is_err(),
            "non-M0 revisions fail exact equality"
        );
    }

    #[test]
    fn second_submit_while_active_is_busy() {
        let mut bed = bed_with(
            vec![FakeProvider::stop_turn("slow")],
            quick_config(),
            Duration::from_millis(300),
        );
        let rt = test_rt();
        rt.block_on(async {
            let first = bed.runtime.submit(submit_cmd("a")).await;
            assert_eq!(first.reply(), CommandReply::Accepted);
            let second = bed.runtime.submit(submit_cmd("b")).await;
            assert_eq!(second.reply(), CommandReply::Busy);
            assert_eq!(second.run(), first.run());
            let (collected, finished) =
                collect_until_finished(&mut bed.data, &mut bed.control).await;
            assert_eq!(finished.outcome(), RunOutcome::Completed);
            assert_eq!(bed.provider.calls.load(Ordering::SeqCst), 1);
            let started = collected
                .control
                .iter()
                .filter(|event| matches!(event.payload(), EventPayload::RunStarted { .. }))
                .count();
            assert_eq!(started, 1);
        });
    }

    #[test]
    fn partial_call_deltas_never_execute_and_reject_the_turn() {
        // Protocol normative rule: a turn whose declared argument progress
        // never reaches a complete candidate cannot end in `Stop` success.
        let mut bed = bed_with(
            vec![FakeProvider::partial_only_turn()],
            quick_config(),
            Duration::ZERO,
        );
        let rt = test_rt();
        rt.block_on(async {
            let reply = bed.runtime.submit(submit_cmd("partial")).await;
            assert_eq!(reply.reply(), CommandReply::Accepted);
            let (collected, finished) =
                collect_until_finished(&mut bed.data, &mut bed.control).await;
            assert_eq!(finished.outcome(), RunOutcome::Failed);
            assert!(tool_started_calls(&collected).is_empty());
            assert_eq!(bed.read_tool.executed.load(Ordering::SeqCst), 0);
            assert_eq!(bed.write_tool.executed.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn approval_exact_tuple_executes() {
        let args = r#"{"path":"src"}"#;
        let mut bed = bed_with(
            vec![
                FakeProvider::tool_turn(vec![candidate("host_write", args)]),
                FakeProvider::stop_turn("done"),
            ],
            quick_config(),
            Duration::ZERO,
        );
        let rt = test_rt();
        rt.block_on(async {
            let submit = bed.runtime.submit(submit_cmd("approve")).await;
            assert_eq!(submit.reply(), CommandReply::Accepted);
            let run = submit.run().cloned().expect("run issued");
            let mut collected =
                collect_until_approval_or_finished(&mut bed.data, &mut bed.control).await;
            let (approval, call) = find_approval(&collected);
            assert_eq!(bed.write_tool.executed.load(Ordering::SeqCst), 0);
            let reply = bed
                .runtime
                .approve(ApproveCommand {
                    request: RequestId::new("req-decide").expect("valid"),
                    approval,
                    run,
                    call: call.clone(),
                })
                .await;
            assert_eq!(reply.reply(), CommandReply::Accepted);
            let (rest, finished) = collect_until_finished(&mut bed.data, &mut bed.control).await;
            collected.control.extend(rest.control);
            collected.data.extend(rest.data);
            assert_eq!(finished.outcome(), RunOutcome::Completed);
            assert_eq!(tool_started_calls(&collected), vec![call]);
            assert_eq!(bed.write_tool.executed.load(Ordering::SeqCst), 1);
            let seen = bed.write_tool.seen_args.lock().expect("readable");
            assert_eq!(seen.as_slice(), &[args.to_owned()]);
        });
    }

    #[test]
    fn approval_for_wrong_call_is_rejected_without_dispatch() {
        let mut bed = bed_with(
            vec![FakeProvider::tool_turn(vec![candidate(
                "host_write",
                r#"{"path":"src"}"#,
            )])],
            quick_config(),
            Duration::ZERO,
        );
        let rt = test_rt();
        rt.block_on(async {
            let submit = bed.runtime.submit(submit_cmd("wrong-call")).await;
            let run = submit.run().cloned().expect("run issued");
            let collected =
                collect_until_approval_or_finished(&mut bed.data, &mut bed.control).await;
            let (approval, _) = find_approval(&collected);
            let other_call = CallId::new("c9-9").expect("valid");
            let reply = bed
                .runtime
                .approve(ApproveCommand {
                    request: RequestId::new("req-bad").expect("valid"),
                    approval,
                    run: run.clone(),
                    call: other_call,
                })
                .await;
            assert_eq!(reply.reply(), CommandReply::StaleOrUnknownTarget);
            assert_eq!(bed.write_tool.executed.load(Ordering::SeqCst), 0);
            let (approval, call) = find_approval(&collected);
            let retry = bed
                .runtime
                .approve(ApproveCommand {
                    request: RequestId::new("req-good").expect("valid"),
                    approval,
                    run,
                    call,
                })
                .await;
            assert_eq!(retry.reply(), CommandReply::Accepted);
            let (_, finished) = collect_until_finished(&mut bed.data, &mut bed.control).await;
            assert_eq!(finished.outcome(), RunOutcome::Completed);
            assert_eq!(bed.write_tool.executed.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn changed_args_binding_fails_dispatch_check() {
        let args = nexus_core::NormalizedArgs::new(r#"{"path":"src"}"#).expect("valid");
        let changed = nexus_core::NormalizedArgs::new(r#"{"path":"other"}"#).expect("valid");
        let run = RunId::new("run-1").expect("valid");
        let call = CallId::new("c1-0").expect("valid");
        let tool = ToolId::new("host_write", nexus_core::M0_REVISION).expect("valid");
        let binding = ApprovalBinding::new(
            ApprovalId::new("a1-0").expect("valid"),
            run.clone(),
            call.clone(),
            tool.clone(),
            args,
            ApprovedScope::new("m0-test-grant").expect("valid"),
            Duration::from_secs(120),
            nexus_core::M0_REVISION,
        );
        let error = binding
            .check_valid_for_dispatch(
                &run,
                &call,
                &tool,
                &changed,
                nexus_core::M0_REVISION,
                Duration::from_secs(1),
            )
            .unwrap_err();
        assert_eq!(error.category(), ErrorCategory::PermissionDenied);
    }

    #[test]
    fn duplicate_approve_never_redispatches() {
        let mut bed = bed_with(
            vec![
                FakeProvider::tool_turn(vec![candidate("host_write", r#"{"path":"src"}"#)]),
                FakeProvider::stop_turn("done"),
            ],
            quick_config(),
            Duration::ZERO,
        );
        let rt = test_rt();
        rt.block_on(async {
            let submit = bed.runtime.submit(submit_cmd("dup")).await;
            let run = submit.run().cloned().expect("run issued");
            let mut collected =
                collect_until_approval_or_finished(&mut bed.data, &mut bed.control).await;
            let (approval, call) = find_approval(&collected);
            let first = bed
                .runtime
                .approve(ApproveCommand {
                    request: RequestId::new("req-first").expect("valid"),
                    approval: approval.clone(),
                    run: run.clone(),
                    call: call.clone(),
                })
                .await;
            assert_eq!(first.reply(), CommandReply::Accepted);
            let second = bed
                .runtime
                .approve(ApproveCommand {
                    request: RequestId::new("req-second").expect("valid"),
                    approval,
                    run,
                    call,
                })
                .await;
            assert_eq!(second.reply(), CommandReply::StaleOrUnknownTarget);
            let (rest, _) = collect_until_finished(&mut bed.data, &mut bed.control).await;
            collected.control.extend(rest.control);
            assert_eq!(bed.write_tool.executed.load(Ordering::SeqCst), 1);
            assert_eq!(tool_started_calls(&collected).len(), 1);
        });
    }

    #[test]
    fn expired_approval_denies_without_dispatch() {
        let mut limits = Limits::m0_test();
        limits.approval_expiry = Duration::from_millis(50);
        let mut bed = bed_with(
            vec![FakeProvider::tool_turn(vec![candidate(
                "host_write",
                r#"{"path":"src"}"#,
            )])],
            RuntimeConfig {
                limits,
                policy: Policy::m0_test(),
                has_approval_handler: true,
            },
            Duration::ZERO,
        );
        let rt = test_rt();
        rt.block_on(async {
            let submit = bed.runtime.submit(submit_cmd("expiry")).await;
            assert_eq!(submit.reply(), CommandReply::Accepted);
            let (collected, finished) =
                collect_until_finished(&mut bed.data, &mut bed.control).await;
            assert_eq!(finished.outcome(), RunOutcome::Completed);
            assert_eq!(bed.write_tool.executed.load(Ordering::SeqCst), 0);
            let outcomes = tool_finished_map(&collected);
            assert_eq!(outcomes.len(), 1);
            let outcome = outcomes.values().next().expect("one outcome");
            assert_eq!(outcome.status(), ExecutionStatus::Denied);
            assert_eq!(outcome.effect(), EffectState::NotStarted);
        });
    }

    #[test]
    fn headless_mutation_without_handler_is_denied_not_started() {
        let mut bed = bed_with(
            vec![
                FakeProvider::tool_turn(vec![candidate("host_write", r#"{"path":"src"}"#)]),
                FakeProvider::stop_turn("done"),
            ],
            RuntimeConfig {
                limits: Limits::m0_test(),
                policy: Policy::m0_test(),
                has_approval_handler: false,
            },
            Duration::ZERO,
        );
        let rt = test_rt();
        rt.block_on(async {
            let submit = bed.runtime.submit(submit_cmd("headless")).await;
            assert_eq!(submit.reply(), CommandReply::Accepted);
            let (collected, finished) =
                collect_until_finished(&mut bed.data, &mut bed.control).await;
            assert_eq!(finished.outcome(), RunOutcome::Completed);
            assert!(tool_started_calls(&collected).is_empty());
            assert_eq!(bed.write_tool.executed.load(Ordering::SeqCst), 0);
            let approvals = collected
                .control
                .iter()
                .filter(|event| matches!(event.payload(), EventPayload::ApprovalRequired(_)))
                .count();
            assert_eq!(approvals, 0);
            let outcomes = tool_finished_map(&collected);
            assert_eq!(outcomes.len(), 1);
            let outcome = outcomes.values().next().expect("denial recorded");
            assert_eq!(outcome.status(), ExecutionStatus::Denied);
            assert_eq!(outcome.effect(), EffectState::NotStarted);
            assert_eq!(outcome.evidence(), Evidence::HostObserved);
        });
    }

    #[test]
    fn cancel_under_output_is_responsive_and_honest() {
        let slow_tool = Arc::new(FakeTool::slow("host_read", Duration::from_millis(300)));
        let provider = Arc::new(FakeProvider::new(vec![
            FakeProvider::tool_turn(vec![candidate("host_read", r#"{"path":"src"}"#)]),
            FakeProvider::stop_turn("done"),
        ]));
        let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![slow_tool.clone()];
        let (runtime, streams) = Runtime::new(
            RuntimeConfig {
                limits: Limits::m0_test(),
                policy: Policy::m0_test(),
                has_approval_handler: false,
            },
            provider,
            tools,
        );
        let mut bed = Bed {
            runtime,
            data: streams.data,
            control: streams.control,
            provider: Arc::new(FakeProvider::new(vec![])),
            read_tool: slow_tool.clone(),
            write_tool: Arc::new(FakeTool::succeeding("host_write", nexus_core::M0_REVISION)),
        };
        let rt = test_rt();
        rt.block_on(async {
            let submit = bed.runtime.submit(submit_cmd("cancel")).await;
            let run = submit.run().cloned().expect("run issued");
            tokio::time::sleep(Duration::from_millis(50)).await;
            let cancel = bed
                .runtime
                .cancel(CancelCommand {
                    request: RequestId::new("req-cancel").expect("valid"),
                    run: run.clone(),
                })
                .await;
            assert_eq!(cancel.reply(), CommandReply::Accepted);
            let (collected, finished) =
                collect_until_finished(&mut bed.data, &mut bed.control).await;
            assert_eq!(finished.outcome(), RunOutcome::Cancelled);
            for started in tool_started_calls(&collected) {
                assert!(
                    tool_finished_map(&collected).contains_key(&started),
                    "every ToolStarted has an outcome"
                );
            }
            let late = bed
                .runtime
                .cancel(CancelCommand {
                    request: RequestId::new("req-cancel-2").expect("valid"),
                    run,
                })
                .await;
            assert_eq!(late.reply(), CommandReply::AlreadyFinalized);
        });
    }

    #[test]
    fn stale_run_identities_are_rejected_on_both_paths() {
        let mut bed = bed_with(
            vec![FakeProvider::stop_turn("done")],
            quick_config(),
            Duration::ZERO,
        );
        let rt = test_rt();
        rt.block_on(async {
            let submit = bed.runtime.submit(submit_cmd("stale")).await;
            let run = submit.run().cloned().expect("run issued");
            let (collected, finished) =
                collect_until_finished(&mut bed.data, &mut bed.control).await;
            assert_eq!(finished.outcome(), RunOutcome::Completed);
            let approve = bed
                .runtime
                .approve(ApproveCommand {
                    request: RequestId::new("req-late").expect("valid"),
                    approval: ApprovalId::new("a1-0").expect("valid"),
                    run: run.clone(),
                    call: CallId::new("c1-0").expect("valid"),
                })
                .await;
            assert_eq!(approve.reply(), CommandReply::AlreadyFinalized);
            let unknown = bed
                .runtime
                .cancel(CancelCommand {
                    request: RequestId::new("req-unknown").expect("valid"),
                    run: RunId::new("run-999").expect("valid"),
                })
                .await;
            assert_eq!(unknown.reply(), CommandReply::StaleOrUnknownTarget);
            let (snapshot_reply, snapshot) = bed
                .runtime
                .get_snapshot(GetSnapshotCommand {
                    request: RequestId::new("req-snap").expect("valid"),
                    run: run.clone(),
                })
                .await;
            assert_eq!(snapshot_reply.reply(), CommandReply::Accepted);
            let snapshot = snapshot.expect("finished run has a snapshot");
            assert_eq!(
                snapshot.lifecycle(),
                RunLifecycle::Finalized(RunOutcome::Completed)
            );
            for event in collected.control.iter().chain(collected.data.iter()) {
                assert!(!event_is_live(event, None));
                assert!(!event_is_live(
                    event,
                    Some(&RunId::new("run-999").expect("valid"))
                ));
            }
            assert!(event_is_live(&collected.control[0], Some(&run)));
        });
    }

    #[test]
    fn terminal_events_are_unique_with_contiguous_sequences() {
        let mut bed = bed_with(
            vec![
                FakeProvider::tool_turn(vec![candidate("host_read", r#"{"path":"src"}"#)]),
                FakeProvider::stop_turn("done"),
            ],
            quick_config(),
            Duration::ZERO,
        );
        let rt = test_rt();
        rt.block_on(async {
            let submit = bed.runtime.submit(submit_cmd("terminal")).await;
            assert_eq!(submit.reply(), CommandReply::Accepted);
            let (collected, finished) =
                collect_until_finished(&mut bed.data, &mut bed.control).await;
            assert_eq!(finished.outcome(), RunOutcome::Completed);
            let mut all: Vec<&RunEvent> = collected
                .data
                .iter()
                .chain(collected.control.iter())
                .collect();
            all.sort_by_key(|event| event.seq());
            let seqs: Vec<EventSequence> = all.iter().map(|event| event.seq()).collect();
            for (index, seq) in seqs.iter().enumerate() {
                assert_eq!(*seq, index as EventSequence);
            }
            let started = all
                .iter()
                .filter(|event| matches!(event.payload(), EventPayload::RunStarted { .. }))
                .count();
            let terminal = all.iter().filter(|event| event.is_terminal()).count();
            assert_eq!(started, 1);
            assert_eq!(terminal, 1);
            assert_eq!(all.first().expect("events").seq(), 0);
            assert!(matches!(
                all.first().expect("events").payload(),
                EventPayload::RunStarted { .. }
            ));
            assert!(all.last().expect("events").is_terminal());
            assert_eq!(bed.read_tool.executed.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn limit_exhaustion_maps_to_limit_reached() {
        let mut limits = Limits::m0_test();
        limits.max_model_turns_per_run = 1;
        let mut bed = bed_with(
            vec![
                FakeProvider::tool_turn(vec![candidate("host_read", r#"{"path":"a"}"#)]),
                FakeProvider::tool_turn(vec![candidate("host_read", r#"{"path":"b"}"#)]),
            ],
            RuntimeConfig {
                limits,
                policy: Policy::m0_test(),
                has_approval_handler: false,
            },
            Duration::ZERO,
        );
        let rt = test_rt();
        rt.block_on(async {
            let submit = bed.runtime.submit(submit_cmd("limits")).await;
            assert_eq!(submit.reply(), CommandReply::Accepted);
            let (collected, finished) =
                collect_until_finished(&mut bed.data, &mut bed.control).await;
            assert_eq!(finished.outcome(), RunOutcome::LimitReached);
            assert_eq!(bed.read_tool.executed.load(Ordering::SeqCst), 1);
            let terminal = collected
                .control
                .iter()
                .filter(|event| event.is_terminal())
                .count();
            assert_eq!(terminal, 1);
        });
    }

    #[test]
    fn denied_calls_consume_the_per_run_call_budget() {
        let mut limits = Limits::m0_test();
        limits.max_tool_calls_per_run = 2;
        let write = |item: &str, reference: &str, path: &str| {
            CallCandidate::new(
                item,
                reference,
                "host_write",
                format!(r#"{{"path":"{path}"}}"#),
            )
            .expect("candidate builds")
        };
        let mut bed = bed_with(
            vec![
                FakeProvider::tool_turn(vec![
                    write("item-0", "prov-0", "a"),
                    write("item-1", "prov-1", "b"),
                ]),
                FakeProvider::tool_turn(vec![write("item-2", "prov-2", "c")]),
            ],
            RuntimeConfig {
                limits,
                policy: Policy::m0_test(),
                has_approval_handler: false,
            },
            Duration::ZERO,
        );
        let rt = test_rt();
        rt.block_on(async {
            let submit = bed.runtime.submit(submit_cmd("denied-budget")).await;
            assert_eq!(submit.reply(), CommandReply::Accepted);
            let (collected, finished) =
                collect_until_finished(&mut bed.data, &mut bed.control).await;
            assert_eq!(finished.outcome(), RunOutcome::LimitReached);
            assert_eq!(bed.write_tool.executed.load(Ordering::SeqCst), 0);
            let denied = collected
                .control
                .iter()
                .filter(|event| matches!(event.payload(), EventPayload::ToolFinished(_)))
                .count();
            assert_eq!(denied, 2, "each admitted denial consumes the run budget");
            assert_eq!(bed.provider.calls.load(Ordering::SeqCst), 2);
        });
    }

    #[test]
    fn per_tool_timeout_records_unknown_without_rollback_claims() {
        let slow = Arc::new(FakeTool::slow("host_read", Duration::from_millis(400)));
        let provider = Arc::new(FakeProvider::new(vec![FakeProvider::tool_turn(vec![
            candidate("host_read", r#"{"path":"src"}"#),
        ])]));
        let mut limits = Limits::m0_test();
        limits.per_tool_timeout = Duration::from_millis(50);
        let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![slow.clone()];
        let (runtime, streams) = Runtime::new(
            RuntimeConfig {
                limits,
                policy: Policy::m0_test(),
                has_approval_handler: false,
            },
            provider,
            tools,
        );
        let mut bed = Bed {
            runtime,
            data: streams.data,
            control: streams.control,
            provider: Arc::new(FakeProvider::new(vec![])),
            read_tool: slow.clone(),
            write_tool: Arc::new(FakeTool::succeeding("host_write", nexus_core::M0_REVISION)),
        };
        let rt = test_rt();
        rt.block_on(async {
            let submit = bed.runtime.submit(submit_cmd("timeout")).await;
            assert_eq!(submit.reply(), CommandReply::Accepted);
            let run = submit.run().cloned().expect("run issued");
            let (collected, finished) =
                collect_until_finished(&mut bed.data, &mut bed.control).await;
            assert_eq!(finished.outcome(), RunOutcome::LimitReached);
            let outcomes = tool_finished_map(&collected);
            assert_eq!(outcomes.len(), 1);
            let outcome = outcomes.values().next().expect("timeout recorded");
            assert_eq!(outcome.status(), ExecutionStatus::TimedOut);
            assert_eq!(outcome.effect(), EffectState::Unknown);
            assert_eq!(outcome.evidence(), Evidence::Uncertain);

            // Ownership is retained: a new run is Busy until the quarantined
            // worker actually terminates.
            let blocked = bed.runtime.submit(submit_cmd("quarantine")).await;
            assert_eq!(blocked.reply(), CommandReply::Busy);
            tokio::time::sleep(Duration::from_millis(600)).await;

            // Late evidence refines the retained snapshot without emitting a
            // duplicate ToolFinished event.
            let (reply, snapshot) = bed
                .runtime
                .get_snapshot(GetSnapshotCommand {
                    request: RequestId::new("req-late-evidence").expect("valid"),
                    run,
                })
                .await;
            assert_eq!(reply.reply(), CommandReply::Accepted);
            let snapshot = snapshot.expect("timed-out run keeps a snapshot");
            let late = snapshot
                .known_outcomes()
                .first()
                .expect("recorded outcome retained");
            assert_eq!(
                late.status,
                ExecutionStatus::Succeeded,
                "late observed effects are captured, not discarded"
            );
            assert_eq!(late.effect, EffectState::KnownApplied);
            assert_eq!(
                tool_finished_map(&collected).len(),
                1,
                "no duplicate outcome event"
            );

            let released = bed.runtime.submit(submit_cmd("after-quarantine")).await;
            assert_eq!(released.reply(), CommandReply::Accepted);
            let _ = collect_until_finished(&mut bed.data, &mut bed.control).await;
        });
    }

    #[test]
    fn control_saturation_buffers_terminal_until_the_consumer_resumes() {
        let provider = Arc::new(FakeProvider::new(vec![]));
        let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = Vec::new();
        let (runtime, mut streams) = Runtime::new(quick_config(), provider, tools);
        let rt = test_rt();
        rt.block_on(async {
            let session = SessionId::new("sess-1").expect("valid");
            let run = RunId::new("r-test-1").expect("valid");
            let usage = Usage::new(None, None, nexus_core::UsageFinality::Provisional);
            for seq in 0..CONTROL_CAPACITY as u64 {
                let event = RunEvent::new(
                    session.clone(),
                    run.clone(),
                    seq,
                    EventPayload::UsageUpdated(usage),
                );
                assert_eq!(
                    deliver_control(&runtime.shared, event),
                    ControlOutcome::Sent,
                    "capacity is available before saturation"
                );
            }
            let terminal =
                RunFinished::new(RunOutcome::Completed, PersistenceState::Ephemeral, None)
                    .expect("terminal builds");
            let event = RunEvent::new(
                session.clone(),
                run.clone(),
                CONTROL_CAPACITY as u64,
                EventPayload::RunFinished(terminal),
            );
            assert_eq!(
                deliver_control(&runtime.shared, event),
                ControlOutcome::Buffered,
                "a saturated control channel retains the required terminal"
            );

            // The committed outbox holds ownership: no new run starts.
            let busy = runtime.submit(submit_cmd("saturated")).await;
            assert_eq!(busy.reply(), CommandReply::Busy);

            // The consumer resumes and receives every event in order,
            // including exactly one terminal.
            let mut received = Vec::new();
            while received.len() < CONTROL_CAPACITY + 1 {
                let event = tokio::time::timeout(Duration::from_secs(5), streams.control.recv())
                    .await
                    .expect("buffered event arrives promptly")
                    .expect("control channel stays open");
                received.push(event);
            }
            for (index, event) in received.iter().enumerate() {
                assert_eq!(
                    event.seq(),
                    index as u64,
                    "delivered sequences stay contiguous"
                );
            }
            assert_eq!(
                received.iter().filter(|event| event.is_terminal()).count(),
                1,
                "exactly one terminal is delivered after resume"
            );
        });
    }

    #[test]
    fn provider_panic_is_supervised_with_a_failed_terminal() {
        let provider = Arc::new(PanicProvider);
        let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![Arc::new(FakeTool::succeeding(
            "host_read",
            nexus_core::M0_REVISION,
        ))];
        let (runtime, streams) = Runtime::new(quick_config(), provider, tools);
        let mut data = streams.data;
        let mut control = streams.control;
        let rt = test_rt();
        rt.block_on(async {
            let submit = runtime.submit(submit_cmd("provider-panic")).await;
            assert_eq!(submit.reply(), CommandReply::Accepted);
            let (_, finished) = collect_until_finished(&mut data, &mut control).await;
            assert_eq!(finished.outcome(), RunOutcome::Failed);
            assert!(finished.error().is_some(), "typed failure is retained");
        });
    }

    #[test]
    fn tool_panic_records_unknown_and_keeps_the_terminal_honest() {
        let provider = Arc::new(FakeProvider::new(vec![
            FakeProvider::tool_turn(vec![candidate("host_read", r#"{"path":"src"}"#)]),
            FakeProvider::stop_turn("done"),
        ]));
        let tools: Vec<Arc<dyn ToolPort + Send + Sync>> =
            vec![Arc::new(PanicTool::new("host_read"))];
        let (runtime, streams) = Runtime::new(quick_config(), provider, tools);
        let mut data = streams.data;
        let mut control = streams.control;
        let rt = test_rt();
        rt.block_on(async {
            let submit = runtime.submit(submit_cmd("tool-panic")).await;
            assert_eq!(submit.reply(), CommandReply::Accepted);
            let (collected, finished) = collect_until_finished(&mut data, &mut control).await;
            assert_eq!(finished.outcome(), RunOutcome::Completed);
            let outcomes = tool_finished_map(&collected);
            assert_eq!(outcomes.len(), 1);
            let outcome = outcomes.values().next().expect("panic recorded");
            assert_eq!(outcome.status(), ExecutionStatus::Failed);
            assert_eq!(outcome.effect(), EffectState::Unknown);
            assert_eq!(outcome.evidence(), Evidence::Uncertain);
        });
    }

    #[test]
    fn run_identities_are_unique_across_runtime_instances() {
        let mut first = bed_with(
            vec![FakeProvider::stop_turn("a")],
            quick_config(),
            Duration::ZERO,
        );
        let mut second = bed_with(
            vec![FakeProvider::stop_turn("b")],
            quick_config(),
            Duration::ZERO,
        );
        let rt = test_rt();
        rt.block_on(async {
            let a = first.runtime.submit(submit_cmd("instance-a")).await;
            let b = second.runtime.submit(submit_cmd("instance-b")).await;
            assert_eq!(a.reply(), CommandReply::Accepted);
            assert_eq!(b.reply(), CommandReply::Accepted);
            assert_ne!(a.run(), b.run(), "recreated runtimes never alias run ids");
            let _ = collect_until_finished(&mut first.data, &mut first.control).await;
            let _ = collect_until_finished(&mut second.data, &mut second.control).await;
        });
    }

    #[test]
    fn cancelled_blocked_tool_finalizes_promptly_and_quarantines_until_termination() {
        let (tool, _entered, release) = BlockingTool::new();
        let tool = Arc::new(tool);
        let provider = Arc::new(FakeProvider::new(vec![
            FakeProvider::tool_turn(vec![candidate("host_read", r#"{"path":"src"}"#)]),
            FakeProvider::stop_turn("done"),
        ]));
        let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![tool.clone()];
        let (runtime, streams) = Runtime::new(quick_config(), provider, tools);
        let mut data = streams.data;
        let mut control = streams.control;
        let rt = test_rt();
        rt.block_on(async {
            let submit = runtime.submit(submit_cmd("blocked-tool-cancel")).await;
            assert_eq!(submit.reply(), CommandReply::Accepted);
            let run = submit.run().cloned().expect("run issued");
            let mut prelude = collect_until_tool_started(&mut data, &mut control).await;
            assert!(
                matches!(
                    prelude.control.last().expect("events").payload(),
                    EventPayload::ToolStarted(_)
                ),
                "the blocked worker entered execution"
            );
            let cancel = runtime
                .cancel(CancelCommand {
                    request: RequestId::new("req-blocked-tool-cancel").expect("valid"),
                    run,
                })
                .await;
            assert_eq!(cancel.reply(), CommandReply::Accepted);

            // Prompt terminal: the drain must not wait for the blocked worker.
            let (rest, finished) = collect_until_finished(&mut data, &mut control).await;
            prelude.control.extend(rest.control);
            prelude.data.extend(rest.data);
            assert_eq!(finished.outcome(), RunOutcome::Cancelled);
            let statuses: Vec<ExecutionStatus> = prelude
                .control
                .iter()
                .filter_map(|event| match event.payload() {
                    EventPayload::ToolFinished(info) => Some(info.outcome.status()),
                    _ => None,
                })
                .collect();
            assert_eq!(statuses, vec![ExecutionStatus::Cancelled]);

            // Quarantine holds ownership: no new run until termination.
            let blocked = runtime.submit(submit_cmd("blocked")).await;
            assert_eq!(blocked.reply(), CommandReply::Busy);

            release.send(()).expect("release the blocked worker");
            let accepted = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let response = runtime.submit(submit_cmd("after-blocked")).await;
                    if response.reply() == CommandReply::Accepted {
                        break response;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("quarantine clears after termination");
            assert_eq!(accepted.reply(), CommandReply::Accepted);
            let _ = collect_until_finished(&mut data, &mut control).await;
            assert_eq!(tool.executions.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn cancelled_blocked_provider_finalizes_promptly_and_quarantines_until_termination() {
        let (provider, entered, release) = BlockingProvider::new();
        let provider = Arc::new(provider);
        let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = Vec::new();
        let (runtime, streams) = Runtime::new(quick_config(), provider.clone(), tools);
        let mut data = streams.data;
        let mut control = streams.control;
        let rt = test_rt();
        rt.block_on(async {
            let submit = runtime.submit(submit_cmd("blocked-provider-cancel")).await;
            assert_eq!(submit.reply(), CommandReply::Accepted);
            let run = submit.run().cloned().expect("run issued");
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if entered.try_recv().is_ok() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("provider entered");

            let cancel = runtime
                .cancel(CancelCommand {
                    request: RequestId::new("req-blocked-provider-cancel").expect("valid"),
                    run,
                })
                .await;
            assert_eq!(cancel.reply(), CommandReply::Accepted);

            // Prompt terminal: the drain must not wait for the blocked provider.
            let (_, finished) = collect_until_finished(&mut data, &mut control).await;
            assert_eq!(finished.outcome(), RunOutcome::Cancelled);
            let blocked = runtime.submit(submit_cmd("blocked")).await;
            assert_eq!(blocked.reply(), CommandReply::Busy);

            release.send(()).expect("release the blocked provider");
            let accepted = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let response = runtime.submit(submit_cmd("after-blocked-provider")).await;
                    if response.reply() == CommandReply::Accepted {
                        break response;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("quarantine clears after provider termination");
            assert_eq!(accepted.reply(), CommandReply::Accepted);
            let (_, next_finished) = collect_until_finished(&mut data, &mut control).await;
            assert_eq!(next_finished.outcome(), RunOutcome::Completed);
            assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        });
    }

    #[test]
    fn terminal_final_usage_merges_missing_counters_without_downgrade() {
        let final_usage = Usage::new(Some(10), Some(8), nexus_core::UsageFinality::Final);
        let terminal = nexus_core::TurnFinished::new(
            FinishReason::Stop,
            Usage::new(Some(10), None, nexus_core::UsageFinality::Final),
            None,
        );
        let mut bed = bed_with(
            vec![vec![
                ProviderEvent::Usage(final_usage),
                ProviderEvent::TurnFinished(terminal),
            ]],
            quick_config(),
            Duration::ZERO,
        );
        let rt = test_rt();
        rt.block_on(async {
            let reply = bed.runtime.submit(submit_cmd("usage-merge")).await;
            assert_eq!(reply.reply(), CommandReply::Accepted);
            let (collected, finished) =
                collect_until_finished(&mut bed.data, &mut bed.control).await;
            assert_eq!(finished.outcome(), RunOutcome::Completed);
            let usages: Vec<Usage> = collected
                .control
                .iter()
                .filter_map(|event| match event.payload() {
                    EventPayload::UsageUpdated(usage) => Some(*usage),
                    _ => None,
                })
                .collect();
            assert_eq!(
                usages.len(),
                1,
                "a missing terminal counter retains the last final value instead of downgrading"
            );
            assert_eq!(usages[0], final_usage);
        });
    }

    #[test]
    fn provider_failure_after_usage_order_violation_keeps_provider_error() {
        let failed = ProviderEvent::Failed(
            AgentError::new(
                ErrorCategory::Timeout,
                "provider timed out",
                RetryGuidance::DoNotRetry,
            )
            .expect("error builds"),
        );
        let mut bed = bed_with(
            vec![vec![
                ProviderEvent::Usage(Usage::new(
                    Some(10),
                    Some(8),
                    nexus_core::UsageFinality::Final,
                )),
                ProviderEvent::Usage(Usage::new(
                    Some(11),
                    Some(9),
                    nexus_core::UsageFinality::Provisional,
                )),
                failed,
            ]],
            quick_config(),
            Duration::ZERO,
        );
        let rt = test_rt();
        rt.block_on(async {
            let reply = bed
                .runtime
                .submit(submit_cmd("failed-provider-error"))
                .await;
            assert_eq!(reply.reply(), CommandReply::Accepted);
            let (_, finished) = collect_until_finished(&mut bed.data, &mut bed.control).await;
            assert_eq!(finished.outcome(), RunOutcome::Failed);
            assert_eq!(
                finished.error().expect("typed failure retained").category(),
                ErrorCategory::Timeout,
                "the provider's original typed failure stays primary"
            );
        });
    }
}
