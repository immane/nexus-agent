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
//!   buffers into a count/byte-bounded outbox with terminal reserve, and a
//!   closed consumer is classified separately from saturation;
//! - per-run sequence numbers commit only on a successful enqueue (or a
//!   committed outbox entry), so delivered sequences stay contiguous;
//! - every admitted candidate (including denials) consumes the per-run call
//!   budget; registration is fallible, descriptors are cached once, and
//!   schemas compile through `nexus-validation`;
//! - the model conversation is rebuilt additively with exact input, call,
//!   result, item-key, and provider-reference round-trips.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant as StdInstant, SystemTime, UNIX_EPOCH};

use nexus_core::commands::{EventSequence, MAX_TEXT_FRAGMENT_BYTES};
use nexus_core::{
    AgentError, ApprovalBinding, ApprovalId, ApprovalNotice, ApproveCommand, AssistantText,
    CallCandidate, CallId, CancelCommand, CancellationToken, Command, CommandReply,
    CommandResponse, ContinuationData, DenyCommand, EffectState, ErrorCategory, EventPayload,
    Evidence, ExecutionStatus, FinishReason, GetSnapshotCommand, ItemKey, Limits, ModelContextItem,
    ModelRequest, OutcomeSummary, PersistenceState, ProviderContext, ProviderEvent, ProviderPort,
    RequestId, RetryGuidance, RunEvent, RunFinished, RunId, RunLifecycle, RunOutcome, SessionId,
    Snapshot, SubmitCommand, ToolCall, ToolContext, ToolFinishedInfo, ToolId, ToolOutcome,
    ToolPort, ToolSpec, ToolStartedInfo, TurnId, Usage, UsageFinality,
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
    session_directories: StdMutex<HashMap<SessionId, Vec<std::path::PathBuf>>>,
    has_approval_handler: bool,
    provider: Arc<dyn ProviderPort + Send + Sync>,
    tools: HashMap<String, RegisteredTool>,
    tool_order: Vec<String>,
    tool_definitions: Vec<ToolSpec>,
    state: Mutex<State>,
    data_tx: mpsc::Sender<RunEvent>,
    control_tx: mpsc::Sender<RunEvent>,
    /// Committed control events that could not enter the bounded channel.
    /// Explicit event and byte ceilings bound retained transport state.
    outbox: StdMutex<VecDeque<RunEvent>>,
    outbox_events: AtomicUsize,
    outbox_bytes: AtomicUsize,
    control_exhausted: AtomicBool,
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
    /// Whole completed exchanges, globally bounded by the context budgets.
    history: VecDeque<RetainedExchange>,
    /// Workers whose termination is not yet established. A non-empty list
    /// blocks new runs and further dispatch; at most one entry exists in the
    /// sequential M0 baseline.
    quarantine: Vec<QuarantineMeta>,
}

struct RetainedExchange {
    session: SessionId,
    items: Vec<ModelContextItem>,
}

struct QuarantineMeta {
    run: RunId,
    call: Option<CallId>,
}

struct PendingApproval {
    binding: ApprovalBinding,
    directory: Option<std::path::PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ApprovalDecision {
    Once,
    SessionDirectory,
    Deny,
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

/// Provisional prefixes already published live while a streaming provider
/// turn is still in flight. The authoritative batch still carries the full
/// event list for validation and records; ingestion consults these keys to
/// publish each prefix exactly once instead of replaying it.
#[derive(Default)]
struct StreamedPrefix {
    /// Text already published per item, used to validate and append the
    /// authoritative batch suffix without replaying or hiding mismatches.
    text: HashMap<String, String>,
    /// Preview item keys already announced as `ToolCallPreview`.
    preview_keys: HashSet<String>,
    retained_bytes: usize,
    identity_bytes: usize,
    event_count: usize,
    usage_updates: usize,
    exceeded: bool,
}

/// Provisional bridge has independent queue, payload, text-output, and event
/// ceilings; none of these substitutes for the authoritative batch budget.
const PROVISIONAL_STREAM_CAPACITY: usize = 64;
const PROVISIONAL_STREAM_BYTE_CAPACITY: usize = 1_048_576;
const PROVISIONAL_EVENT_LIMIT: usize = crate::protocol::MAX_BATCH_EVENTS;
const PROVISIONAL_USAGE_UPDATE_LIMIT: usize = 16;
const CONTROL_OUTBOX_EVENT_LIMIT: usize = 256;
const CONTROL_OUTBOX_BYTE_LIMIT: usize = 1_048_576;
const CONTROL_OUTBOX_TERMINAL_RESERVE_BYTES: usize = 65_536;

struct ActiveRun {
    phase: RunState,
    run: RunId,
    run_n: u64,
    session: SessionId,
    request: RequestId,
    profile: String,
    /// True when confirmation-required tools must be denied without
    /// prompting. Copied from [`SubmitCommand::read_only`]: the frontend
    /// sets it from the resolved agent mode and the runtime enforces it at
    /// tool dispatch, so a read-only run can never execute a write no
    /// matter which frontend submitted it.
    read_only: bool,
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
    history_len: usize,
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
    /// Set once `u64::MAX` is committed: no further sequence exists, so
    /// every later emit is refused instead of reusing the final value
    /// (frontends reject duplicate sequences).
    sequence_exhausted: bool,
    last_usage: Option<Usage>,
    published_usage: Option<Usage>,
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
    Exhausted,
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
            session_directories: StdMutex::new(HashMap::new()),
            has_approval_handler: config.has_approval_handler,
            provider,
            tools: registry,
            tool_order: order,
            tool_definitions,
            state: Mutex::new(State {
                active: None,
                last: None,
                history: VecDeque::new(),
                quarantine: Vec::new(),
            }),
            data_tx,
            control_tx,
            outbox: StdMutex::new(VecDeque::new()),
            outbox_events: AtomicUsize::new(0),
            outbox_bytes: AtomicUsize::new(0),
            control_exhausted: AtomicBool::new(false),
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
            Command::ApproveSessionDirectory(command) => {
                (self.approve_session_directory(command).await, None)
            }
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
        if self.shared.outbox_events.load(Ordering::SeqCst) != 0 {
            let run = state.last.as_ref().map(|last| last.run.clone());
            return CommandResponse::new(command.request.clone(), CommandReply::Busy, run);
        }
        // Exhaustion belongs to the previous run. Its outbox is empty here,
        // so the next accepted run starts with a fresh control budget.
        self.shared.control_exhausted.store(false, Ordering::SeqCst);
        let mut conversation = Vec::new();
        let mut count = 1;
        let mut bytes = user_item.payload_bytes();
        let mut retained = Vec::new();
        for exchange in state
            .history
            .iter()
            .rev()
            .filter(|exchange| exchange.session == command.session)
        {
            let size = conversation_bytes(&exchange.items);
            if count + exchange.items.len() > self.shared.context_items_bound
                || bytes.saturating_add(size) > nexus_core::provider::MAX_CONVERSATION_BYTES
            {
                break;
            }
            count += exchange.items.len();
            bytes += size;
            retained.push(exchange);
        }
        for exchange in retained.into_iter().rev() {
            conversation.extend(exchange.items.iter().cloned());
        }
        let history_len = conversation.len();
        conversation.push(user_item);
        state.active = Some(ActiveRun {
            phase: RunState::Preparing,
            run: run.clone(),
            run_n,
            session: command.session.clone(),
            request: command.request.clone(),
            profile: command.profile.clone(),
            read_only: command.read_only,
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
            conversation,
            history_len,
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
            sequence_exhausted: false,
            last_usage: None,
            published_usage: None,
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
            ApprovalDecision::Once,
        )
        .await
    }

    pub async fn approve_session_directory(&self, command: ApproveCommand) -> CommandResponse {
        self.resolve_grant(
            command.request,
            command.run,
            command.call,
            command.approval,
            ApprovalDecision::SessionDirectory,
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
            ApprovalDecision::Deny,
        )
        .await
    }

    async fn resolve_grant(
        &self,
        request: RequestId,
        run: RunId,
        call: CallId,
        approval: ApprovalId,
        decision: ApprovalDecision,
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
        if decision == ApprovalDecision::SessionDirectory {
            if active.cancelled
                || active.token.is_cancelled()
                || active.read_only
                || StdInstant::now() >= active.deadline
            {
                return CommandResponse::new(
                    request,
                    CommandReply::StaleOrUnknownTarget,
                    Some(run),
                );
            }
            if !active.current.as_ref().is_some_and(|current| {
                current.call.call() == &call
                    && pending
                        .binding
                        .check_valid_for_dispatch(
                            current.call.run(),
                            current.call.call(),
                            current.call.tool(),
                            current.call.args(),
                            self.shared.policy.revision(),
                            now,
                        )
                        .is_ok()
                    && self
                        .shared
                        .policy
                        .approval_scope(&current.call)
                        .as_ref()
                        .ok()
                        == Some(pending.binding.scope())
            }) {
                return CommandResponse::new(
                    request,
                    CommandReply::StaleOrUnknownTarget,
                    Some(run),
                );
            }
            let directory = active
                .current
                .as_ref()
                .and_then(|current| self.shared.policy.approval_directory(&current.call).ok())
                .flatten();
            let Some(directory) = directory else {
                return CommandResponse::new(
                    request,
                    CommandReply::StaleOrUnknownTarget,
                    Some(run),
                );
            };
            if pending.directory.as_ref() != Some(&directory) {
                return CommandResponse::new(
                    request,
                    CommandReply::StaleOrUnknownTarget,
                    Some(run),
                );
            }
            let mut sessions = self
                .shared
                .session_directories
                .lock()
                .expect("session permissions lock");
            if !sessions.contains_key(&active.session) && sessions.len() >= 128 {
                return CommandResponse::new(request, CommandReply::Rejected, Some(run));
            }
            let directories = sessions.entry(active.session.clone()).or_default();
            if !directories.contains(&directory) {
                if directories.len() >= 64 {
                    return CommandResponse::new(request, CommandReply::Rejected, Some(run));
                }
                directories.push(directory);
            }
        }
        active.pending.remove(&approval);
        active
            .decided
            .insert(approval, decision != ApprovalDecision::Deny);
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
        if active.terminal.is_some() {
            return Ok(RunState::Finished);
        }
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
            if active.terminal.is_some() {
                return Ok(RunState::Finished);
            }
            if &active.run != run || active.cancelled || active.token.is_cancelled() {
                return Err(Terminal::cancelled());
            }
            if StdInstant::now() >= active.deadline {
                return Err(Terminal::limit("run duration exhausted"));
            }
            trim_inherited_history(active, self.shared.context_items_bound);
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
        let incremental = provider.supports_incremental_streaming();
        let (stream_tx, mut stream_rx) =
            mpsc::channel::<ProviderEvent>(PROVISIONAL_STREAM_CAPACITY);
        let stream_overflow = Arc::new(AtomicBool::new(false));
        let producer_overflow = stream_overflow.clone();
        let producer_token = token.clone();
        let stream_bytes = Arc::new(AtomicUsize::new(0));
        let producer_bytes = stream_bytes.clone();
        let mut handle = tokio::task::spawn_blocking(move || {
            if incremental {
                let sink = |event: ProviderEvent| {
                    let bytes = provisional_event_bytes(&event);
                    let previous = producer_bytes.fetch_add(bytes, Ordering::SeqCst);
                    if previous.saturating_add(bytes) > PROVISIONAL_STREAM_BYTE_CAPACITY {
                        producer_bytes.fetch_sub(bytes, Ordering::SeqCst);
                        producer_overflow.store(true, Ordering::SeqCst);
                        producer_token.cancel();
                        return;
                    }
                    if let Err(error) = stream_tx.try_send(event) {
                        producer_bytes.fetch_sub(bytes, Ordering::SeqCst);
                        drop(error);
                        producer_overflow.store(true, Ordering::SeqCst);
                        producer_token.cancel();
                    }
                };
                provider.stream_with_sink(&request, &context, &sink)
            } else {
                provider.stream(&request, &context)
            }
        });
        let mut prefix = StreamedPrefix::default();
        let mut cancelled_first = false;
        let joined = loop {
            if token.is_cancelled() {
                cancelled_first = true;
                break None;
            }
            tokio::select! {
                biased;
                joined = &mut handle => break Some(joined),
                streamed = stream_rx.recv() => {
                        if let Some(event) = streamed {
                            stream_bytes.fetch_sub(provisional_event_bytes(&event), Ordering::SeqCst);
                            self.publish_provisional(run, &turn, event, &mut prefix)
                            .await;
                    }
                }
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
            Some(Ok(events)) => {
                // The join may win the race while provisional events are
                // still buffered: drain them first so every streamed prefix
                // publishes before the authoritative batch ingests.
                self.drain_stream(run, &turn, &mut stream_rx, &stream_bytes, &mut prefix)
                    .await;
                if stream_overflow.load(Ordering::SeqCst) {
                    return Err(Terminal::limit(
                        "provisional provider stream exceeded its bound",
                    ));
                }
                if prefix.exceeded {
                    return Err(Terminal::limit("provisional output budget exhausted"));
                }
                self.ingest_model_batch(run, &turn, events, &mut prefix)
                    .await
            }
            Some(Err(join)) => {
                let message = if join.is_panic() {
                    "provider worker panicked"
                } else {
                    "provider worker ended without a result"
                };
                Err(Terminal::failed_internal(message))
            }
            None => {
                if stream_overflow.load(Ordering::SeqCst) || prefix.exceeded {
                    self.quarantine_provider(run, handle).await;
                    return Err(Terminal::limit(if prefix.exceeded {
                        "provisional output budget exhausted"
                    } else {
                        "provisional provider stream exceeded its bound"
                    }));
                }
                let forced = {
                    let state = self.shared.state.lock().await;
                    state
                        .active
                        .as_ref()
                        .filter(|active| &active.run == run)
                        .and_then(|active| {
                            active.terminal.map(|outcome| Terminal {
                                outcome,
                                error: active.terminal_error.clone(),
                            })
                        })
                };
                if let Some(forced) = forced {
                    self.quarantine_provider(run, handle).await;
                    return Err(forced);
                }
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

    /// Publishes one provisional provider event while its turn is still in
    /// flight. Only presentation-safe prefixes publish here: text fragments
    /// (flushed at once so the frontend updates now, not at the terminal),
    /// call previews, and usage estimates. Candidates and terminals are
    /// withheld by contract and arrive through the validated batch, so
    /// nothing streamed can dispatch work or conclude a turn early. Events
    /// for a stale, cancelled, or finished run publish nothing.
    async fn publish_provisional(
        &self,
        run: &RunId,
        turn: &TurnId,
        event: ProviderEvent,
        prefix: &mut StreamedPrefix,
    ) {
        let mut state = self.shared.state.lock().await;
        let Some(active) = state.active.as_mut() else {
            return;
        };
        if &active.run != run || active.cancelled || active.token.is_cancelled() {
            return;
        }
        if prefix.event_count >= PROVISIONAL_EVENT_LIMIT {
            prefix.exceeded = true;
            active.token.cancel();
            return;
        }
        prefix.event_count += 1;
        if let ProviderEvent::TextDelta { item_key, .. }
        | ProviderEvent::ToolCallDelta { item_key, .. } = &event
            && let Err(error) = ItemKey::new(item_key.as_str())
        {
            active.data_dropped = true;
            active.force_terminal = Some(RunOutcome::Failed);
            active.terminal = Some(RunOutcome::Failed);
            active.terminal_error = Some(error);
            active.token.cancel();
            active.wake.notify_one();
            return;
        }
        match event {
            ProviderEvent::TextDelta { item_key, text } => {
                if !record_provisional(prefix, text.len(), self.shared.effective_output_budget)
                    || !record_identity(prefix, item_key.len())
                {
                    active.data_dropped = true;
                    prefix.exceeded = true;
                    active.token.cancel();
                    return;
                }
                prefix
                    .text
                    .entry(item_key.clone())
                    .or_default()
                    .push_str(&text);
                buffer_text_fragments(&self.shared, active, run, turn, &item_key, &text);
                flush_text(&self.shared, active);
            }
            ProviderEvent::ToolCallDelta { item_key, .. } => {
                if !record_identity(prefix, item_key.len()) {
                    active.data_dropped = true;
                    prefix.exceeded = true;
                    active.token.cancel();
                    return;
                }
                prefix.preview_keys.insert(item_key.clone());
                emit_data(
                    &self.shared,
                    active,
                    run,
                    EventPayload::ToolCallPreview { item_key },
                );
            }
            ProviderEvent::Usage(usage) => {
                publish_usage(
                    &self.shared,
                    active,
                    run,
                    usage,
                    &mut prefix.usage_updates,
                    true,
                );
            }
            ProviderEvent::ToolCallReady(_)
            | ProviderEvent::TurnFinished(_)
            | ProviderEvent::ReasoningDelta { .. }
            | ProviderEvent::Failed(_) => {}
        }
    }

    /// Drains buffered provisional events after the provider turn joined,
    /// publishing each exactly once ahead of batch ingestion. The join can
    /// win the channel race while streamed prefixes are still queued; without
    /// this drain they would be silently dropped and the batch would publish
    /// them a second time.
    async fn drain_stream(
        &self,
        run: &RunId,
        turn: &TurnId,
        stream_rx: &mut mpsc::Receiver<ProviderEvent>,
        stream_bytes: &AtomicUsize,
        prefix: &mut StreamedPrefix,
    ) {
        while let Ok(event) = stream_rx.try_recv() {
            stream_bytes.fetch_sub(provisional_event_bytes(&event), Ordering::SeqCst);
            self.publish_provisional(run, turn, event, prefix).await;
        }
    }

    async fn ingest_model_batch(
        &self,
        run: &RunId,
        turn: &TurnId,
        events: Vec<ProviderEvent>,
        prefix: &mut StreamedPrefix,
    ) -> Result<RunState, Terminal> {
        // Full-turn atomic validation: duplicate references, partial/final
        // disagreement, and finish-reason conflicts fail before any
        // admission, identity consumption, or queue mutation. An honestly
        // failed invocation keeps its provider error primary: a protocol
        // ordering diagnostic must not replace a typed timeout/failure.
        let mut effective_limits = self.shared.limits;
        effective_limits.max_tool_output_bytes = self.shared.effective_output_budget;
        if let Err(protocol_error) = crate::protocol::validate_batch(&events, &effective_limits) {
            if let Some(ProviderEvent::Failed(error)) = events.last()
                && !events[..events.len() - 1]
                    .iter()
                    .any(ProviderEvent::is_terminal)
            {
                let error = error.clone();
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
        if active.terminal.is_some() {
            return Ok(RunState::Finished);
        }
        if &active.run != run || active.cancelled || active.token.is_cancelled() {
            return Err(Terminal::cancelled());
        }
        let mut candidates = Vec::new();
        let mut text_items: Vec<(String, String)> = Vec::new();
        for event in &events {
            if let ProviderEvent::TextDelta { item_key, text } = event {
                if let Some(entry) = text_items.iter_mut().find(|(key, _)| key == item_key) {
                    entry.1.push_str(text);
                } else {
                    text_items.push((item_key.clone(), text.clone()));
                }
            }
        }
        let terminal = match events.last() {
            Some(ProviderEvent::TurnFinished(finished)) => Some(Ok(finished.clone())),
            Some(ProviderEvent::Failed(error)) => Some(Err(error.clone())),
            _ => None,
        };
        let successful_turn = terminal.as_ref().is_some_and(Result::is_ok);
        if successful_turn {
            for (item_key, streamed) in &prefix.text {
                if !text_items.iter().any(|(key, _)| key == item_key) && !streamed.is_empty() {
                    return Err(Terminal::failed(runtime_error(
                        ErrorCategory::Protocol,
                        "authoritative provider batch omitted streamed text",
                    )));
                }
            }
            for (item_key, authoritative) in &text_items {
                if !authoritative.starts_with(prefix.text.get(item_key).map_or("", String::as_str))
                {
                    return Err(Terminal::failed(runtime_error(
                        ErrorCategory::Protocol,
                        "authoritative provider text does not match its streamed prefix",
                    )));
                }
            }
        }
        let mut remaining_prefix: HashMap<String, usize> = prefix
            .text
            .iter()
            .map(|(key, text)| (key.clone(), text.len()))
            .collect();
        // Thinking trace for this turn, echoed back on later turns when
        // present. Accumulated silently: reasoning is never presented and
        // never previewed, only recorded into the turn's assistant items.
        let mut reasoning = String::new();
        for event in &events {
            if active.terminal.is_some() {
                break;
            }
            match event {
                ProviderEvent::ReasoningDelta { text } => {
                    reasoning.push_str(text);
                }
                ProviderEvent::TextDelta { item_key, text } => {
                    let skip = if successful_turn {
                        let remaining = remaining_prefix.entry(item_key.clone()).or_default();
                        let mut skip = (*remaining).min(text.len());
                        while !text.is_char_boundary(skip) {
                            skip -= 1;
                        }
                        *remaining -= skip;
                        skip
                    } else if prefix.text.contains_key(item_key) {
                        text.len()
                    } else {
                        0
                    };
                    if skip < text.len() {
                        buffer_text_fragments(
                            &self.shared,
                            active,
                            run,
                            turn,
                            item_key,
                            &text[skip..],
                        );
                    }
                }
                ProviderEvent::ToolCallDelta { item_key, .. } => {
                    if !prefix.preview_keys.contains(item_key) {
                        emit_data(
                            &self.shared,
                            active,
                            run,
                            EventPayload::ToolCallPreview {
                                item_key: item_key.clone(),
                            },
                        );
                    }
                }
                ProviderEvent::ToolCallReady(candidate) => candidates.push(candidate.clone()),
                ProviderEvent::Usage(usage) => {
                    publish_usage(
                        &self.shared,
                        active,
                        run,
                        *usage,
                        &mut prefix.usage_updates,
                        usage.finality() == UsageFinality::Provisional,
                    );
                }
                ProviderEvent::TurnFinished(finished) => {
                    // Per-counter final merge: terminal `Some` wins, terminal
                    // `None` retains the last final `Some`, and a missing
                    // counter stays unknown instead of being downgraded.
                    let merged = merge_final_usage(active.last_usage, finished.usage());
                    publish_usage(
                        &self.shared,
                        active,
                        run,
                        merged,
                        &mut prefix.usage_updates,
                        false,
                    );
                }
                ProviderEvent::Failed(_) => {}
            }
        }
        if active.terminal.is_some() {
            return Ok(RunState::Finished);
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
                        if text_items.iter().all(|(_, text)| text.is_empty())
                            && !reasoning.is_empty()
                        {
                            active.conversation.push(
                                ModelContextItem::assistant_reasoning(reasoning.clone())
                                    .map_err(Terminal::failed)?,
                            );
                        }
                        append_assistant_text(active, text_items, &reasoning)?;
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
                        let has_text = text_items.iter().any(|(_, text)| !text.is_empty());
                        append_assistant_text(active, text_items, &reasoning)?;
                        self.admit_candidates(
                            active,
                            turn,
                            candidates,
                            if has_text { "" } else { &reasoning },
                        )?;
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
        reasoning: &str,
    ) -> Result<(), Terminal> {
        let run = active.run.clone();
        let mut denied_results = Vec::new();
        let mut trace = (!reasoning.is_empty()).then(|| reasoning.to_owned());
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
                let outcome = bound_outcome(&self.shared, denied_outcome("unknown tool"));
                denied_results.push(denied_context(
                    active,
                    &candidate,
                    &call,
                    &outcome,
                    trace.take(),
                )?);
                record_denied(&self.shared, active, &run, call, outcome);
                if active.terminal.is_some() {
                    break;
                }
                continue;
            };
            let tool_id = registered.spec.id().clone();
            let args = match registered.schema.validate(
                candidate.arguments_json(),
                self.shared.limits.max_arg_assembly_bytes,
            ) {
                Ok(args) => args,
                Err(_) => {
                    let outcome =
                        bound_outcome(&self.shared, denied_outcome("invalid tool arguments"));
                    denied_results.push(denied_context(
                        active,
                        &candidate,
                        &call,
                        &outcome,
                        trace.take(),
                    )?);
                    record_denied(&self.shared, active, &run, call, outcome);
                    if active.terminal.is_some() {
                        break;
                    }
                    continue;
                }
            };
            if active.terminal.is_some() {
                break;
            }
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
            let item = ModelContextItem::assistant_call_with_reasoning(
                candidate.item_key(),
                candidate.provider_ref(),
                queued.call.clone(),
                trace.take(),
            )
            .map_err(Terminal::failed)?;
            active.conversation.push(item);
            active.queue.push_back(queued);
        }
        active.conversation.extend(denied_results);
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
        if active.terminal.is_some() {
            return Ok(RunState::Finished);
        }
        if &active.run != run || active.cancelled || active.token.is_cancelled() {
            return Err(Terminal::cancelled());
        }
        if StdInstant::now() >= active.deadline {
            return Err(Terminal::limit("run duration exhausted"));
        }
        let Some(mut queued) = active.queue.pop_front() else {
            return Ok(RunState::Preparing);
        };
        if self.shared.policy.is_development() {
            queued.needs_approval = self
                .shared
                .policy
                .call_requires_approval(
                    &queued.call,
                    self.shared
                        .session_directories
                        .lock()
                        .expect("session permissions lock")
                        .get(&active.session)
                        .map_or(&[], Vec::as_slice),
                )
                .unwrap_or(true);
        }
        if active.read_only
            && (queued.needs_approval
                || matches!(
                    queued.call.tool().name(),
                    "host_write" | "host_patch" | "host_exec"
                ))
        {
            // Read-only runs never prompt and never execute: the denial is
            // recorded exactly like the no-handler denial below, so no
            // approval card is minted and no grant can exist to approve.
            let call = queued.call.call().clone();
            active.outcome_slot = Some((
                call,
                denied_outcome("confirmation required but the run is read-only"),
            ));
            active.current = Some(CurrentCall {
                call: queued.call,
                item_key: queued.item_key,
                provider_ref: queued.provider_ref,
                binding: None,
            });
            return Ok(RunState::RecordingResult);
        }
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
        let mut notice = ApprovalNotice::new(
            approval.clone(),
            queued.call.call().clone(),
            summary,
            binding.scope().as_str(),
            expires_at_elapsed,
        )
        .map_err(Terminal::failed)?
        .with_args_preview(preview)
        .map_err(Terminal::failed)?;
        notice.session_directory = self
            .shared
            .policy
            .approval_directory(&queued.call)
            .map_err(Terminal::failed)?
            .map(|path| path.to_string_lossy().into_owned());
        notice.validate().map_err(Terminal::failed)?;
        active.pending.insert(
            approval.clone(),
            PendingApproval {
                binding: binding.clone(),
                directory: notice
                    .session_directory
                    .as_ref()
                    .map(std::path::PathBuf::from),
            },
        );
        active.current = Some(CurrentCall {
            call: queued.call,
            item_key: queued.item_key,
            provider_ref: queued.provider_ref,
            binding: Some(binding),
        });
        let delivery = emit_control(
            &self.shared,
            active,
            run,
            EventPayload::ApprovalRequired(notice),
        );
        apply_required_control_outcome(active, delivery);
        Ok(RunState::AwaitingApproval)
    }

    async fn on_awaiting(&self, run: &RunId) -> Result<RunState, Terminal> {
        enum Wake {
            Forced,
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
                if active.terminal.is_some() {
                    Wake::Forced
                } else if active.delivery_closed || active.cancelled || active.token.is_cancelled()
                {
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
                Wake::Forced => return Ok(RunState::Finished),
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
                        let disconnected = active.delivery_closed;
                        abandon_pending(active);
                        let call = current_call_id(active);
                        if let Some(call) = call {
                            active.outcome_slot = Some((
                                call,
                                if disconnected {
                                    denied_outcome("approval consumer disconnected")
                                } else {
                                    cancelled_outcome("run cancelled while awaiting approval")
                                },
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
                None => match self.shared.policy.authorize_with_directories(
                    &call,
                    self.shared
                        .session_directories
                        .lock()
                        .expect("session permissions lock")
                        .get(&active.session)
                        .map_or(&[], Vec::as_slice),
                ) {
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
            if self.shared.policy.is_development()
                && self.shared.policy.approval_scope(&call).as_ref().ok() != Some(&scope)
            {
                active.outcome_slot = Some((
                    call.call().clone(),
                    denied_outcome("filesystem target changed after authorization"),
                ));
                return Ok(RunState::RecordingResult);
            }
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
            let delivery = emit_control(
                &self.shared,
                active,
                run,
                EventPayload::ToolStarted(ToolStartedInfo {
                    call: call.call().clone(),
                    tool: call.tool().clone(),
                    args_preview: self.shared.policy.approval_preview(&call).ok(),
                }),
            );
            apply_required_control_outcome(active, delivery);
            if active.terminal.is_some() {
                return Ok(RunState::Finished);
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
                if active.terminal.is_none() {
                    active.force_terminal = Some(if cancelled {
                        RunOutcome::Cancelled
                    } else {
                        RunOutcome::LimitReached
                    });
                }
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
        let outcome = bound_outcome(&self.shared, outcome);
        active.outcomes.push(OutcomeSummary {
            call: call.clone(),
            status: outcome.status(),
            effect: outcome.effect(),
            evidence: outcome.evidence(),
        });
        if let Some(current) = active.current.take() {
            let item = ModelContextItem::tool_result(
                call.clone(),
                current.item_key,
                current.provider_ref,
                current.call.tool().clone(),
                outcome.clone(),
            )
            .map_err(Terminal::failed)?;
            active.conversation.push(item);
        }
        let delivery = emit_control(
            &self.shared,
            active,
            run,
            EventPayload::ToolFinished(ToolFinishedInfo { call, outcome }),
        );
        apply_required_control_outcome(active, delivery);
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
        let delivery = emit_control(&self.shared, active, run, payload);
        apply_control_outcome(active, delivery);
        if delivery == ControlOutcome::Exhausted {
            return Err(Terminal::limit("control event budget exhausted"));
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
        let delivery_truncated = delivery_closed || delivery == ControlOutcome::Exhausted;
        if delivery_closed {
            let mut outbox = self.shared.outbox.lock().expect("control outbox readable");
            let discarded_bytes = outbox.iter().map(control_event_bytes).sum::<usize>();
            let discarded_events = outbox.len();
            outbox.clear();
            self.shared
                .outbox_bytes
                .fetch_sub(discarded_bytes, Ordering::SeqCst);
            self.shared
                .outbox_events
                .fetch_sub(discarded_events, Ordering::SeqCst);
        }
        active.finished_sent = true;
        active.phase = RunState::Finished;
        // Failed/cancelled/limited runs are not committed: their partial
        // calls or provisional text must never contaminate the next request.
        if outcome == RunOutcome::Completed {
            let items = active.conversation[active.history_len..].to_vec();
            if items.len() <= self.shared.context_items_bound
                && conversation_bytes(&items) <= nexus_core::provider::MAX_CONVERSATION_BYTES
            {
                state.history.push_back(RetainedExchange {
                    session: active.session.clone(),
                    items,
                });
                while state
                    .history
                    .iter()
                    .map(|exchange| exchange.items.len())
                    .sum::<usize>()
                    > self.shared.context_items_bound
                    || state
                        .history
                        .iter()
                        .map(|exchange| conversation_bytes(&exchange.items))
                        .sum::<usize>()
                        > nexus_core::provider::MAX_CONVERSATION_BYTES
                {
                    state.history.pop_front();
                }
            }
        }
        state.last = Some(FinishedRun {
            run: active.run.clone(),
            session: active.session.clone(),
            outcome,
            last_seq: active.last_seq,
            outcomes: active.outcomes.clone(),
            truncated: active.data_dropped || delivery_truncated,
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

fn append_assistant_text(
    active: &mut ActiveRun,
    text_items: Vec<(String, String)>,
    reasoning: &str,
) -> Result<(), Terminal> {
    let mut trace = (!reasoning.is_empty()).then(|| reasoning.to_owned());
    for (item_key, text) in text_items {
        if text.is_empty() {
            continue;
        }
        let item = ModelContextItem::assistant_text_with_reasoning(item_key, text, trace.take())
            .map_err(Terminal::failed)?;
        active.conversation.push(item);
    }
    Ok(())
}

fn conversation_bytes(items: &[ModelContextItem]) -> usize {
    items.iter().fold(0usize, |total, item| {
        total.saturating_add(item.payload_bytes())
    })
}

/// Make room for the current run without splitting any inherited exchange.
/// Current-run items are never evicted to hide a budget violation or pending
/// tool call; an oversized current run still terminates with a limit error.
fn trim_inherited_history(active: &mut ActiveRun, bound: usize) {
    while active.history_len > 0
        && (active.conversation.len() > bound
            || conversation_bytes(&active.conversation)
                > nexus_core::provider::MAX_CONVERSATION_BYTES)
    {
        let remove = active.conversation[..active.history_len]
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(index, item)| {
                matches!(item, ModelContextItem::UserText(_)).then_some(index)
            })
            .unwrap_or(active.history_len);
        active.conversation.drain(..remove);
        active.history_len -= remove;
    }
}

fn denied_context(
    active: &mut ActiveRun,
    candidate: &CallCandidate,
    call: &CallId,
    outcome: &ToolOutcome,
    reasoning: Option<String>,
) -> Result<ModelContextItem, Terminal> {
    let tool = ToolId::new(candidate.tool_name(), nexus_core::M0_REVISION)
        .map_err(|_| Terminal::failed_internal("denied tool identity is invalid"))?;
    active.conversation.push(
        ModelContextItem::assistant_denied_call(call.clone(), candidate.clone(), reasoning)
            .map_err(Terminal::failed)?,
    );
    ModelContextItem::tool_result(
        call.clone(),
        candidate.item_key(),
        candidate.provider_ref(),
        tool,
        outcome.clone(),
    )
    .map_err(Terminal::failed)
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
    let delivery = emit_control(
        shared,
        active,
        run,
        EventPayload::ToolFinished(ToolFinishedInfo { call, outcome }),
    );
    apply_required_control_outcome(active, delivery);
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

fn record_provisional(prefix: &mut StreamedPrefix, bytes: usize, byte_limit: usize) -> bool {
    if prefix.retained_bytes.saturating_add(bytes) > byte_limit {
        return false;
    }
    prefix.retained_bytes += bytes;
    true
}

fn record_identity(prefix: &mut StreamedPrefix, bytes: usize) -> bool {
    if prefix.identity_bytes.saturating_add(bytes) > PROVISIONAL_STREAM_BYTE_CAPACITY {
        return false;
    }
    prefix.identity_bytes += bytes;
    true
}

fn provisional_event_bytes(event: &ProviderEvent) -> usize {
    match event {
        ProviderEvent::TextDelta { item_key, text } => item_key.len().saturating_add(text.len()),
        ProviderEvent::ToolCallDelta { item_key, .. } => item_key.len(),
        ProviderEvent::ToolCallReady(candidate) => candidate
            .item_key()
            .len()
            .saturating_add(candidate.provider_ref().len())
            .saturating_add(candidate.tool_name().len())
            .saturating_add(candidate.arguments_json().len()),
        ProviderEvent::ReasoningDelta { text } => text.len(),
        ProviderEvent::Failed(error) => error.message().len(),
        ProviderEvent::Usage(_) | ProviderEvent::TurnFinished(_) => 0,
    }
}

fn control_event_bytes(event: &RunEvent) -> usize {
    let payload_bytes = match event.payload() {
        EventPayload::RunStarted { request } => request.as_str().len(),
        EventPayload::AssistantTextDelta(text) => {
            text.turn.as_str().len() + text.item_key.len() + text.text.len()
        }
        EventPayload::ToolCallPreview { item_key } => item_key.len(),
        EventPayload::ApprovalRequired(notice) => {
            notice.approval.as_str().len()
                + notice.call.as_str().len()
                + notice.summary.len()
                + notice.scope_summary.len()
                + notice.args_preview.as_ref().map_or(0, String::len)
                + notice.session_directory.as_ref().map_or(0, String::len)
        }
        EventPayload::ToolStarted(info) => {
            info.call.as_str().len()
                + info.tool.name().len()
                + info.args_preview.as_ref().map_or(0, String::len)
        }
        EventPayload::ToolOutput(progress) => progress.call.as_str().len() + progress.preview.len(),
        EventPayload::ToolFinished(info) => info.call.as_str().len() + info.outcome.content().len(),
        EventPayload::UsageUpdated(_) => 16,
        EventPayload::RunFinished(finished) => {
            finished.error().map_or(0, |error| error.message().len())
                + finished
                    .persistence_error()
                    .map_or(0, |error| error.message().len())
        }
    };
    // Include a fixed allowance for the event/envelope and owned collection
    // bookkeeping; variable text is counted at its actual UTF-8 byte length.
    payload_bytes.saturating_add(128)
}

fn control_outbox_fits(
    event_count: usize,
    retained_bytes: usize,
    next_bytes: usize,
    terminal: bool,
) -> bool {
    let event_limit = if terminal {
        CONTROL_OUTBOX_EVENT_LIMIT
    } else {
        CONTROL_OUTBOX_EVENT_LIMIT.saturating_sub(1)
    };
    let byte_limit = if terminal {
        CONTROL_OUTBOX_BYTE_LIMIT
    } else {
        CONTROL_OUTBOX_BYTE_LIMIT.saturating_sub(CONTROL_OUTBOX_TERMINAL_RESERVE_BYTES)
    };
    event_count < event_limit && retained_bytes.saturating_add(next_bytes) <= byte_limit
}

fn commit_sequence(active: &mut ActiveRun, seq: EventSequence) {
    if let Some(next) = nexus_core::commands::checked_next_sequence(seq) {
        active.next_seq = next;
        active.last_seq = Some(seq);
    } else {
        // The delivered `u64::MAX` event is the last sequence the run can
        // ever assign: record it, report truncation, and refuse every later
        // emit so the final value is never reused (frontends reject
        // duplicates, which would otherwise repeat forever).
        active.last_seq = Some(seq);
        active.data_dropped = true;
        active.sequence_exhausted = true;
    }
}

/// Flushes coalesced text as one sequenced data event. A full or closed data
/// channel drops presentation traffic without consuming a sequence number,
/// so delivered sequences stay gap-free; control flow continues.
fn flush_text(shared: &Arc<Shared>, active: &mut ActiveRun) {
    if active.sequence_exhausted {
        active.pending_text = None;
        active.data_dropped = true;
        return;
    }
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
    if active.sequence_exhausted {
        active.pending_text = None;
        active.data_dropped = true;
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

fn buffer_text_fragments(
    shared: &Arc<Shared>,
    active: &mut ActiveRun,
    run: &RunId,
    turn: &TurnId,
    item_key: &str,
    text: &str,
) {
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + MAX_TEXT_FRAGMENT_BYTES).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        match AssistantText::new(turn.clone(), item_key, &text[start..end]) {
            Ok(fragment) => buffer_text(shared, active, run, fragment),
            Err(_) => {
                active.data_dropped = true;
                return;
            }
        }
        start = end;
    }
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
    if active.sequence_exhausted {
        active.pending_text = None;
        active.data_dropped = true;
        return ControlOutcome::Closed;
    }
    debug_assert!(crate::transport::is_control_payload(&payload));
    flush_text(shared, active);
    let seq = active.next_seq;
    let event = run_event(active, seq, payload);
    let outcome = deliver_control(shared, event);
    if matches!(outcome, ControlOutcome::Sent | ControlOutcome::Buffered) {
        commit_sequence(active, seq);
    }
    outcome
}

fn publish_usage(
    shared: &Arc<Shared>,
    active: &mut ActiveRun,
    run: &RunId,
    usage: Usage,
    update_count: &mut usize,
    throttle: bool,
) {
    active.last_usage = Some(usage);
    if active.published_usage == Some(usage)
        || (throttle && *update_count >= PROVISIONAL_USAGE_UPDATE_LIMIT)
    {
        return;
    }
    let outcome = emit_control(shared, active, run, EventPayload::UsageUpdated(usage));
    apply_control_outcome(active, outcome);
    if matches!(outcome, ControlOutcome::Sent | ControlOutcome::Buffered) {
        active.published_usage = Some(usage);
        if throttle {
            *update_count += 1;
        }
    }
}

fn deliver_control(shared: &Arc<Shared>, event: RunEvent) -> ControlOutcome {
    if shared.control_closed.load(Ordering::SeqCst) || shared.control_tx.is_closed() {
        shared.control_closed.store(true, Ordering::SeqCst);
        return ControlOutcome::Closed;
    }
    let terminal = event.is_terminal();
    if !terminal && shared.control_exhausted.load(Ordering::SeqCst) {
        return ControlOutcome::Exhausted;
    }
    let mut outbox = shared.outbox.lock().expect("control outbox readable");
    // Count includes the flusher's in-flight event until it enters the
    // channel. New events must not bypass either queued or in-flight work.
    if shared.outbox_events.load(Ordering::SeqCst) == 0 {
        match shared.control_tx.try_reserve() {
            Ok(permit) => {
                permit.send(event);
                return ControlOutcome::Sent;
            }
            Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Closed(_)) => {
                shared.control_closed.store(true, Ordering::SeqCst);
                return ControlOutcome::Closed;
            }
        }
    }
    let bytes = control_event_bytes(&event);
    let retained = shared.outbox_bytes.load(Ordering::SeqCst);
    let retained_events = shared.outbox_events.load(Ordering::SeqCst);
    if !control_outbox_fits(retained_events, retained, bytes, terminal) {
        if terminal {
            return ControlOutcome::Exhausted;
        }
        shared.control_exhausted.store(true, Ordering::SeqCst);
        return ControlOutcome::Exhausted;
    }
    shared.outbox_bytes.fetch_add(bytes, Ordering::SeqCst);
    shared.outbox_events.fetch_add(1, Ordering::SeqCst);
    outbox.push_back(event);
    drop(outbox);
    ensure_flusher(shared);
    ControlOutcome::Buffered
}

fn apply_control_outcome(active: &mut ActiveRun, outcome: ControlOutcome) {
    match outcome {
        ControlOutcome::Sent | ControlOutcome::Buffered => return,
        ControlOutcome::Closed => {
            active.delivery_closed = true;
            return;
        }
        ControlOutcome::Exhausted => {
            active.data_dropped = true;
            active
                .force_terminal
                .get_or_insert(RunOutcome::LimitReached);
            active.terminal.get_or_insert(RunOutcome::LimitReached);
            active.terminal_error.get_or_insert_with(|| {
                runtime_error(
                    ErrorCategory::ResourceLimit,
                    "control event budget exhausted",
                )
            });
        }
    }
    active.token.cancel();
    active.wake.notify_one();
}

fn apply_required_control_outcome(active: &mut ActiveRun, outcome: ControlOutcome) {
    apply_control_outcome(active, outcome);
    if outcome == ControlOutcome::Closed {
        active.force_terminal.get_or_insert(RunOutcome::Cancelled);
    }
}

fn ensure_flusher(shared: &Arc<Shared>) {
    // A disconnected receiver is a permanent transport failure. Retained
    // events remain available for honest run state/finalization, but retrying
    // them cannot make progress and would create a busy reschedule loop.
    if shared.control_closed.load(Ordering::SeqCst) {
        return;
    }
    if shared.flushing.swap(true, Ordering::SeqCst) {
        return;
    }
    let shared = shared.clone();
    tokio::spawn(async move { flush_control(shared).await });
}

async fn flush_control(shared: Arc<Shared>) {
    loop {
        let next = {
            let mut outbox = shared.outbox.lock().expect("control outbox readable");
            outbox.pop_front()
        };
        let Some(event) = next else {
            break;
        };
        match shared.control_tx.reserve().await {
            Ok(permit) => {
                let terminal = event.is_terminal();
                let event_bytes = control_event_bytes(&event);
                permit.send(event);
                shared.outbox_bytes.fetch_sub(event_bytes, Ordering::SeqCst);
                shared.outbox_events.fetch_sub(1, Ordering::SeqCst);
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
    if !shared.control_closed.load(Ordering::SeqCst)
        && !shared
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

    #[cfg(unix)]
    #[test]
    fn development_directory_grants_cover_queued_calls_and_stay_session_local() {
        let root = std::env::temp_dir();
        let args = r#"{"path":"/etc/hosts"}"#;
        let mut config = quick_config();
        config.policy = Policy::development(&root).unwrap();
        let turn = || {
            FakeProvider::tool_turn(vec![
                CallCandidate::new("item-1", "ref-1", "host_read", args).unwrap(),
                CallCandidate::new("item-2", "ref-2", "host_write", args).unwrap(),
            ])
        };
        let mut bed = bed_with(
            vec![
                turn(),
                FakeProvider::stop_turn("done"),
                turn(),
                FakeProvider::stop_turn("done"),
                turn(),
                FakeProvider::stop_turn("done"),
            ],
            config,
            Duration::ZERO,
        );
        test_rt().block_on(async {
            let first = bed.runtime.submit(submit_cmd("directory-first")).await;
            let notice = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let event = bed.control.recv().await.unwrap();
                    if let EventPayload::ApprovalRequired(notice) = event.payload() {
                        break notice.clone();
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(
                notice.session_directory.as_deref(),
                Some(std::fs::canonicalize("/etc").unwrap().to_str().unwrap())
            );
            let command = ApproveCommand {
                request: RequestId::new("directory-grant").unwrap(),
                run: first.run().unwrap().clone(),
                call: notice.call.clone(),
                approval: notice.approval.clone(),
            };
            assert_eq!(
                bed.runtime
                    .approve_session_directory(command.clone())
                    .await
                    .reply(),
                CommandReply::Accepted
            );
            assert_eq!(
                bed.runtime.approve_session_directory(command).await.reply(),
                CommandReply::StaleOrUnknownTarget
            );
            let (events, finished) = collect_until_finished(&mut bed.data, &mut bed.control).await;
            assert_eq!(finished.outcome(), RunOutcome::Completed);
            assert!(
                !events
                    .control
                    .iter()
                    .any(|event| matches!(event.payload(), EventPayload::ApprovalRequired(_)))
            );
            assert_eq!(bed.write_tool.executed.load(Ordering::SeqCst), 1);
            bed.runtime.submit(submit_cmd("directory-second")).await;
            let (events, _) = collect_until_finished(&mut bed.data, &mut bed.control).await;
            assert!(
                !events
                    .control
                    .iter()
                    .any(|event| matches!(event.payload(), EventPayload::ApprovalRequired(_)))
            );
            let mut other = submit_cmd("directory-other");
            other.session = SessionId::new("sess-other").unwrap();
            let other = bed.runtime.submit(other).await;
            let approval = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let event = bed.control.recv().await.unwrap();
                    if let EventPayload::ApprovalRequired(notice) = event.payload() {
                        break notice.clone();
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(approval.session_directory, notice.session_directory);
            bed.runtime
                .cancel(CancelCommand {
                    request: RequestId::new("cancel-other").unwrap(),
                    run: other.run().unwrap().clone(),
                })
                .await;
            collect_until_finished(&mut bed.data, &mut bed.control).await;
        });
    }

    #[test]
    fn development_automatic_writes_are_denied_in_read_only_runs() {
        let root = std::env::temp_dir();
        let mut config = quick_config();
        config.policy = Policy::development(&root).unwrap();
        let mut bed = bed_with(
            vec![
                FakeProvider::tool_turn(vec![candidate(
                    "host_write",
                    r#"{"path":"nexus-plan-write"}"#,
                )]),
                FakeProvider::stop_turn("done"),
            ],
            config,
            Duration::ZERO,
        );
        test_rt().block_on(async {
            bed.runtime
                .submit(submit_cmd("development-plan").with_read_only(true))
                .await;
            let (events, _) = collect_until_finished(&mut bed.data, &mut bed.control).await;
            assert_eq!(bed.write_tool.executed.load(Ordering::SeqCst), 0);
            assert!(
                tool_finished_map(&events)
                    .values()
                    .all(|outcome| outcome.status() == ExecutionStatus::Denied)
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn development_allow_once_and_cancelled_approvals_never_cache_directories() {
        let mut config = quick_config();
        config.policy = Policy::development(&std::env::temp_dir()).unwrap();
        let turn =
            || FakeProvider::tool_turn(vec![candidate("host_read", r#"{"path":"/etc/hosts"}"#)]);
        let mut bed = bed_with(
            vec![turn(), FakeProvider::stop_turn("done"), turn()],
            config,
            Duration::ZERO,
        );
        test_rt().block_on(async {
            let first = bed.runtime.submit(submit_cmd("development-once")).await;
            let events = collect_until_approval_or_finished(&mut bed.data, &mut bed.control).await;
            let (approval, call) = find_approval(&events);
            assert_eq!(
                bed.runtime
                    .approve(ApproveCommand {
                        request: RequestId::new("allow-once").unwrap(),
                        run: first.run().unwrap().clone(),
                        approval,
                        call
                    })
                    .await
                    .reply(),
                CommandReply::Accepted
            );
            collect_until_finished(&mut bed.data, &mut bed.control).await;
            assert!(
                bed.runtime
                    .shared
                    .session_directories
                    .lock()
                    .unwrap()
                    .is_empty()
            );
            let second = bed.runtime.submit(submit_cmd("development-cancel")).await;
            let events = collect_until_approval_or_finished(&mut bed.data, &mut bed.control).await;
            let (approval, call) = find_approval(&events);
            bed.runtime
                .cancel(CancelCommand {
                    request: RequestId::new("cancel-directory").unwrap(),
                    run: second.run().unwrap().clone(),
                })
                .await;
            let response = bed
                .runtime
                .approve_session_directory(ApproveCommand {
                    request: RequestId::new("late-directory").unwrap(),
                    run: second.run().unwrap().clone(),
                    approval,
                    call,
                })
                .await;
            assert_ne!(response.reply(), CommandReply::Accepted);
            assert!(
                bed.runtime
                    .shared
                    .session_directories
                    .lock()
                    .unwrap()
                    .is_empty()
            );
            collect_until_finished(&mut bed.data, &mut bed.control).await;
        });
    }

    #[test]
    fn development_project_writes_are_automatic_without_an_approval_handler() {
        let mut config = quick_config();
        config.policy = Policy::development(&std::env::temp_dir()).unwrap();
        config.has_approval_handler = false;
        let mut bed = bed_with(
            vec![
                FakeProvider::tool_turn(vec![candidate(
                    "host_write",
                    r#"{"path":"nexus-automatic-write"}"#,
                )]),
                FakeProvider::stop_turn("done"),
            ],
            config,
            Duration::ZERO,
        );
        test_rt().block_on(async {
            bed.runtime.submit(submit_cmd("development-headless")).await;
            let (events, finished) = collect_until_finished(&mut bed.data, &mut bed.control).await;
            assert_eq!(finished.outcome(), RunOutcome::Completed);
            assert_eq!(bed.write_tool.executed.load(Ordering::SeqCst), 1);
            assert!(
                !events
                    .control
                    .iter()
                    .any(|event| matches!(event.payload(), EventPayload::ApprovalRequired(_)))
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn development_changed_symlink_target_cannot_create_a_directory_grant() {
        let root =
            std::env::temp_dir().join(format!("nexus-runtime-grant-link-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let link = root.join("external");
        std::os::unix::fs::symlink("/etc/hosts", &link).unwrap();
        let mut config = quick_config();
        config.policy = Policy::development(&root).unwrap();
        let mut bed = bed_with(
            vec![
                FakeProvider::tool_turn(vec![candidate("host_read", r#"{"path":"external"}"#)]),
                FakeProvider::stop_turn("done"),
            ],
            config,
            Duration::ZERO,
        );
        test_rt().block_on(async {
            let run = bed
                .runtime
                .submit(submit_cmd("changed-link"))
                .await
                .run()
                .unwrap()
                .clone();
            let events = collect_until_approval_or_finished(&mut bed.data, &mut bed.control).await;
            let (approval, call) = find_approval(&events);
            std::fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink("/usr/bin/true", &link).unwrap();
            let reply = bed
                .runtime
                .approve_session_directory(ApproveCommand {
                    request: RequestId::new("changed-directory").unwrap(),
                    run: run.clone(),
                    approval: approval.clone(),
                    call: call.clone(),
                })
                .await;
            assert_eq!(reply.reply(), CommandReply::StaleOrUnknownTarget);
            assert!(
                bed.runtime
                    .shared
                    .session_directories
                    .lock()
                    .unwrap()
                    .is_empty()
            );
            bed.runtime
                .deny(DenyCommand {
                    request: RequestId::new("deny-changed").unwrap(),
                    run,
                    approval,
                    call,
                })
                .await;
            collect_until_finished(&mut bed.data, &mut bed.control).await;
            assert_eq!(bed.read_tool.executed.load(Ordering::SeqCst), 0);
        });
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
    fn automatic_tool_started_identifies_the_tool_and_only_publishes_safe_arguments() {
        for (args, preview) in [
            (r#"{"path":"src"}"#, Some(r#"host_read {"path":"src"}"#)),
            (r#"{"path":"api_key=hidden"}"#, None),
        ] {
            let mut bed = bed_with(
                vec![
                    FakeProvider::tool_turn(vec![candidate("host_read", args)]),
                    FakeProvider::stop_turn("done"),
                ],
                quick_config(),
                Duration::ZERO,
            );
            test_rt().block_on(async {
                let reply = bed.runtime.submit(submit_cmd("automatic-preview")).await;
                assert_eq!(reply.reply(), CommandReply::Accepted);
                let (collected, finished) =
                    collect_until_finished(&mut bed.data, &mut bed.control).await;
                assert_eq!(finished.outcome(), RunOutcome::Completed);
                let info = collected
                    .control
                    .iter()
                    .find_map(|event| match event.payload() {
                        EventPayload::ToolStarted(info) => Some(info),
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(info.tool.name(), "host_read");
                assert_eq!(info.args_preview.as_deref(), preview);
                assert_eq!(bed.read_tool.executed.load(Ordering::SeqCst), 1);
            });
        }
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
            let info = collected
                .control
                .iter()
                .find_map(|event| match event.payload() {
                    EventPayload::ToolStarted(info) => Some(info),
                    _ => None,
                })
                .unwrap();
            assert_eq!(info.tool.name(), "host_write");
            assert_eq!(
                info.args_preview.as_deref(),
                Some(r#"host_write {"path":"src"}"#)
            );
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
    fn control_outbox_exhaustion_still_reserves_and_delivers_terminal() {
        let provider = Arc::new(FakeProvider::new(vec![FakeProvider::stop_turn(
            "recovered",
        )]));
        let (runtime, mut streams) = Runtime::new(quick_config(), provider.clone(), Vec::new());
        let rt = test_rt();
        rt.block_on(async {
            let session = SessionId::new("sess-outbox-limit").expect("valid");
            let run = RunId::new("run-outbox-limit").expect("valid");
            let usage = Usage::new(Some(1), Some(1), UsageFinality::Provisional);
            for seq in 0..CONTROL_CAPACITY as u64 {
                let event = RunEvent::new(
                    session.clone(),
                    run.clone(),
                    seq,
                    EventPayload::UsageUpdated(usage),
                );
                assert_eq!(
                    deliver_control(&runtime.shared, event),
                    ControlOutcome::Sent
                );
            }
            for seq in
                CONTROL_CAPACITY as u64..(CONTROL_CAPACITY + CONTROL_OUTBOX_EVENT_LIMIT - 1) as u64
            {
                let event = RunEvent::new(
                    session.clone(),
                    run.clone(),
                    seq,
                    EventPayload::UsageUpdated(usage),
                );
                assert_eq!(
                    deliver_control(&runtime.shared, event),
                    ControlOutcome::Buffered
                );
            }
            let terminal_seq = (CONTROL_CAPACITY + CONTROL_OUTBOX_EVENT_LIMIT - 1) as u64;
            let excess = RunEvent::new(
                session.clone(),
                run.clone(),
                terminal_seq,
                EventPayload::UsageUpdated(usage),
            );
            assert_eq!(
                deliver_control(&runtime.shared, excess),
                ControlOutcome::Exhausted
            );
            assert!(runtime.shared.control_exhausted.load(Ordering::SeqCst));
            assert!(!runtime.shared.control_closed.load(Ordering::SeqCst));

            let terminal =
                RunFinished::new(RunOutcome::LimitReached, PersistenceState::Ephemeral, None)
                    .expect("terminal builds");
            let event = RunEvent::new(
                session,
                run,
                terminal_seq,
                EventPayload::RunFinished(terminal),
            );
            assert_eq!(
                deliver_control(&runtime.shared, event),
                ControlOutcome::Buffered
            );
            assert_eq!(
                runtime.shared.outbox_events.load(Ordering::SeqCst),
                CONTROL_OUTBOX_EVENT_LIMIT
            );

            let mut received = Vec::new();
            let total = CONTROL_CAPACITY + CONTROL_OUTBOX_EVENT_LIMIT;
            while received.len() < total {
                received.push(
                    tokio::time::timeout(Duration::from_secs(5), streams.control.recv())
                        .await
                        .expect("outbox event arrives")
                        .expect("control remains open on capacity exhaustion"),
                );
            }
            assert_eq!(received.len(), total);
            assert!(received.last().is_some_and(RunEvent::is_terminal));
            assert!(
                received
                    .windows(2)
                    .all(|pair| pair[0].seq() + 1 == pair[1].seq())
            );

            let accepted = runtime.submit(submit_cmd("after-outbox-exhaustion")).await;
            assert_eq!(accepted.reply(), CommandReply::Accepted);
            assert!(!runtime.shared.control_exhausted.load(Ordering::SeqCst));
            let (_, next_finished) =
                collect_until_finished(&mut streams.data, &mut streams.control).await;
            assert_eq!(next_finished.outcome(), RunOutcome::Completed);
            assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn control_terminal_never_bypasses_queued_or_in_flight_events() {
        let rt = test_rt();
        rt.block_on(async {
            for in_flight in [false, true] {
                let provider = Arc::new(FakeProvider::new(vec![]));
                let (runtime, mut streams) = Runtime::new(quick_config(), provider, Vec::new());
                let session = SessionId::new("sess-control-order").expect("valid");
                let run = RunId::new("run-control-order").expect("valid");
                for seq in 0..=CONTROL_CAPACITY as u64 {
                    let event = RunEvent::new(
                        session.clone(),
                        run.clone(),
                        seq,
                        EventPayload::UsageUpdated(Usage::new(
                            Some(seq),
                            None,
                            UsageFinality::Provisional,
                        )),
                    );
                    assert_eq!(
                        deliver_control(&runtime.shared, event),
                        if seq < CONTROL_CAPACITY as u64 {
                            ControlOutcome::Sent
                        } else {
                            ControlOutcome::Buffered
                        }
                    );
                }
                if in_flight {
                    tokio::task::yield_now().await;
                    assert!(runtime.shared.outbox.lock().expect("readable").is_empty());
                    assert_eq!(runtime.shared.outbox_events.load(Ordering::SeqCst), 1);
                    assert_eq!(
                        runtime
                            .submit(submit_cmd("in-flight-control"))
                            .await
                            .reply(),
                        CommandReply::Busy
                    );
                }
                let mut received = Vec::new();
                for _ in 0..CONTROL_CAPACITY {
                    received.push(streams.control.try_recv().expect("channel event"));
                }
                let terminal =
                    RunFinished::new(RunOutcome::Completed, PersistenceState::Ephemeral, None)
                        .expect("valid terminal");
                let event = RunEvent::new(
                    session,
                    run,
                    CONTROL_CAPACITY as u64 + 1,
                    EventPayload::RunFinished(terminal),
                );
                assert_eq!(
                    deliver_control(&runtime.shared, event),
                    ControlOutcome::Buffered
                );
                for _ in 0..2 {
                    received.push(
                        tokio::time::timeout(Duration::from_secs(5), streams.control.recv())
                            .await
                            .expect("event arrives")
                            .expect("control open"),
                    );
                }
                assert!(
                    received
                        .windows(2)
                        .all(|pair| pair[0].seq() + 1 == pair[1].seq())
                );
                assert!(received.last().expect("terminal").is_terminal());
            }
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

#[cfg(test)]
mod cov_runtime_private {
    use super::*;
    use nexus_core::ProviderCapabilities;
    use nexus_core::commands::MAX_TEXT_FRAGMENT_BYTES;

    #[test]
    fn provisional_retention_enforces_event_and_byte_ceilings() {
        let mut prefix = StreamedPrefix::default();
        for _ in 0..PROVISIONAL_EVENT_LIMIT {
            prefix.event_count += 1;
        }
        assert!(prefix.event_count >= PROVISIONAL_EVENT_LIMIT);
        assert_eq!(prefix.event_count, PROVISIONAL_EVENT_LIMIT);

        let mut bytes = StreamedPrefix::default();
        assert!(record_provisional(&mut bytes, 4, 4));
        assert!(!record_provisional(&mut bytes, 1, 4));
        assert_eq!(bytes.retained_bytes, 4);

        let mut identities = StreamedPrefix::default();
        assert!(record_identity(
            &mut identities,
            PROVISIONAL_STREAM_BYTE_CAPACITY
        ));
        assert!(!record_identity(&mut identities, 1));
    }

    #[test]
    fn control_outbox_bounds_reserve_one_terminal_slot_and_bytes() {
        assert!(control_outbox_fits(
            CONTROL_OUTBOX_EVENT_LIMIT - 1,
            CONTROL_OUTBOX_BYTE_LIMIT - CONTROL_OUTBOX_TERMINAL_RESERVE_BYTES,
            CONTROL_OUTBOX_TERMINAL_RESERVE_BYTES,
            true,
        ));
        assert!(!control_outbox_fits(
            CONTROL_OUTBOX_EVENT_LIMIT - 1,
            CONTROL_OUTBOX_BYTE_LIMIT - CONTROL_OUTBOX_TERMINAL_RESERVE_BYTES,
            1,
            false,
        ));
        assert!(!control_outbox_fits(CONTROL_OUTBOX_EVENT_LIMIT, 0, 1, true,));
    }

    #[test]
    fn control_budget_exhaustion_marks_active_snapshot_truncated() {
        let run = RunId::new("run-control-limit").expect("valid run id");
        let mut active = active_for(&run);
        apply_control_outcome(&mut active, ControlOutcome::Exhausted);
        assert_eq!(active.force_terminal, Some(RunOutcome::LimitReached));
        assert!(
            snapshot_of_active(&active)
                .expect("active snapshot builds")
                .is_content_truncated()
        );
    }

    /// Minimal provider only used to build a real `Shared` with its bounded
    /// channels; the covered helpers never invoke it.
    struct NoopProvider;

    impl ProviderPort for NoopProvider {
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
            Vec::new()
        }
    }

    fn shared_with_data() -> (Arc<Shared>, mpsc::Receiver<RunEvent>) {
        let (runtime, streams) = Runtime::new(
            RuntimeConfig {
                limits: Limits::m0_test(),
                policy: Policy::m0_test(),
                has_approval_handler: false,
            },
            Arc::new(NoopProvider),
            Vec::new(),
        );
        (runtime.shared, streams.data)
    }

    fn active_for(run: &RunId) -> ActiveRun {
        ActiveRun {
            phase: RunState::Preparing,
            run: run.clone(),
            run_n: 1,
            session: SessionId::new("sess-cov-private").expect("valid session id"),
            request: RequestId::new("req-cov-private").expect("valid request id"),
            profile: "cov-profile".to_owned(),
            read_only: false,
            started: StdInstant::now(),
            deadline: StdInstant::now() + Duration::from_secs(60),
            token: CancellationToken::new(),
            wake: Arc::new(Notify::new()),
            cancelled: false,
            turn_seq: 0,
            call_seq: 0,
            approval_seq: 0,
            turns_used: 0,
            calls_used: 0,
            continuation: None,
            conversation: Vec::new(),
            history_len: 0,
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
            sequence_exhausted: false,
            last_usage: None,
            published_usage: None,
            force_terminal: None,
            terminal: None,
            terminal_error: None,
            delivery_closed: false,
            finished_sent: false,
        }
    }

    fn fragment(turn: &str, item_key: &str, text: &str) -> AssistantText {
        AssistantText::new(TurnId::new(turn).expect("valid turn id"), item_key, text)
            .expect("valid text fragment")
    }

    fn drain(data: &mut mpsc::Receiver<RunEvent>) -> Vec<RunEvent> {
        let mut events = Vec::new();
        while let Ok(event) = data.try_recv() {
            events.push(event);
        }
        events
    }

    fn assert_delta(event: &RunEvent, turn: &str, item_key: &str, text: &str, seq: EventSequence) {
        assert_eq!(event.seq(), seq);
        match event.payload() {
            EventPayload::AssistantTextDelta(fragment) => {
                assert_eq!(fragment.turn.as_str(), turn);
                assert_eq!(fragment.item_key, item_key);
                assert_eq!(fragment.text, text);
            }
            other => panic!("expected an assistant text delta, got {other:?}"),
        }
    }

    #[test]
    fn merge_final_usage_prefers_terminal_counters() {
        let last = Usage::new(Some(10), Some(8), nexus_core::UsageFinality::Final);
        let terminal = Usage::new(Some(3), Some(4), nexus_core::UsageFinality::Final);
        assert_eq!(
            merge_final_usage(Some(last), terminal),
            Usage::new(Some(3), Some(4), nexus_core::UsageFinality::Final)
        );
    }

    #[test]
    fn merge_final_usage_retains_last_final_unknowns() {
        let last = Usage::new(Some(10), Some(8), nexus_core::UsageFinality::Final);
        let terminal = Usage::new(None, None, nexus_core::UsageFinality::Final);
        assert_eq!(
            merge_final_usage(Some(last), terminal),
            Usage::new(Some(10), Some(8), nexus_core::UsageFinality::Final)
        );
    }

    #[test]
    fn merge_final_usage_never_borrows_provisional_counters() {
        let provisional = Usage::new(Some(10), Some(8), nexus_core::UsageFinality::Provisional);
        let terminal = Usage::new(None, None, nexus_core::UsageFinality::Final);
        assert_eq!(
            merge_final_usage(Some(provisional), terminal),
            Usage::new(None, None, nexus_core::UsageFinality::Final),
            "an unknown final counter stays unknown instead of using a provisional value"
        );
        assert_eq!(
            merge_final_usage(None, terminal),
            Usage::new(None, None, nexus_core::UsageFinality::Final),
            "no last usage still yields final unknown counters"
        );
    }

    #[test]
    fn merge_final_usage_merges_each_counter_independently() {
        let last = Usage::new(Some(10), Some(8), nexus_core::UsageFinality::Final);
        let terminal = Usage::new(Some(11), None, nexus_core::UsageFinality::Final);
        assert_eq!(
            merge_final_usage(Some(last), terminal),
            Usage::new(Some(11), Some(8), nexus_core::UsageFinality::Final)
        );
    }

    #[test]
    fn merge_final_usage_relabels_terminal_counters_as_final() {
        let terminal = Usage::new(Some(5), Some(6), nexus_core::UsageFinality::Provisional);
        let merged = merge_final_usage(None, terminal);
        assert_eq!(merged.input_tokens(), Some(5));
        assert_eq!(merged.output_tokens(), Some(6));
        assert_eq!(merged.finality(), nexus_core::UsageFinality::Final);
    }

    #[test]
    fn private_outcome_builders_are_exact_and_untruncated() {
        let denied = denied_outcome("denied by policy");
        assert_eq!(denied.status(), ExecutionStatus::Denied);
        assert_eq!(denied.effect(), EffectState::NotStarted);
        assert_eq!(denied.evidence(), Evidence::HostObserved);
        assert_eq!(denied.content(), "denied by policy");
        assert!(!denied.is_truncated());

        let cancelled = cancelled_outcome("cancelled while queued");
        assert_eq!(cancelled.status(), ExecutionStatus::Cancelled);
        assert_eq!(cancelled.effect(), EffectState::Unknown);
        assert_eq!(cancelled.evidence(), Evidence::Uncertain);
        assert_eq!(cancelled.content(), "cancelled while queued");
        assert!(!cancelled.is_truncated());

        let timed_out = timeout_outcome("deadline exhausted");
        assert_eq!(timed_out.status(), ExecutionStatus::TimedOut);
        assert_eq!(timed_out.effect(), EffectState::Unknown);
        assert_eq!(timed_out.evidence(), Evidence::Uncertain);
        assert_eq!(timed_out.content(), "deadline exhausted");
        assert!(!timed_out.is_truncated());

        let failed = failed_outcome("worker failed");
        assert_eq!(failed.status(), ExecutionStatus::Failed);
        assert_eq!(failed.effect(), EffectState::Unknown);
        assert_eq!(failed.evidence(), Evidence::Uncertain);
        assert_eq!(failed.content(), "worker failed");
        assert!(!failed.is_truncated());
    }

    #[test]
    fn commit_sequence_advances_on_the_committed_sequence() {
        let run = RunId::new("run-commit").expect("valid run id");
        let mut active = active_for(&run);
        commit_sequence(&mut active, 0);
        assert_eq!(active.next_seq, 1);
        assert_eq!(active.last_seq, Some(0));
        assert!(!active.data_dropped);

        commit_sequence(&mut active, 1);
        assert_eq!(active.next_seq, 2);
        assert_eq!(active.last_seq, Some(1));
        assert!(!active.data_dropped);
    }

    #[test]
    fn commit_sequence_overflow_reports_dropped_without_advancing() {
        let (shared, mut data) = shared_with_data();
        let run = RunId::new("run-overflow").expect("valid run id");
        let mut active = active_for(&run);
        active.next_seq = u64::MAX;
        active.last_seq = Some(u64::MAX - 1);
        commit_sequence(&mut active, u64::MAX);
        assert!(active.data_dropped, "overflow is reported, never wrapped");
        assert!(active.sequence_exhausted, "no sequence follows the maximum");
        assert_eq!(active.next_seq, u64::MAX, "exhaustion advances nothing");
        assert_eq!(active.last_seq, Some(u64::MAX));

        buffer_text(
            &shared,
            &mut active,
            &run,
            fragment("turn-1", "item-0", "late"),
        );
        flush_text(&shared, &mut active);
        emit_data(
            &shared,
            &mut active,
            &run,
            EventPayload::ToolCallPreview {
                item_key: "item-1".to_owned(),
            },
        );
        let outcome = emit_control(
            &shared,
            &mut active,
            &run,
            EventPayload::UsageUpdated(Usage::new(None, None, nexus_core::UsageFinality::Final)),
        );
        assert_eq!(
            outcome,
            ControlOutcome::Closed,
            "an exhausted run emits nothing further"
        );
        assert!(active.pending_text.is_none());
        assert_eq!(active.next_seq, u64::MAX, "refused emits reuse no sequence");
        assert_eq!(active.last_seq, Some(u64::MAX));
        assert!(drain(&mut data).is_empty());
    }

    #[test]
    fn buffer_text_keeps_the_first_fragment_pending() {
        let (shared, mut data) = shared_with_data();
        let run = RunId::new("run-buffer-first").expect("valid run id");
        let mut active = active_for(&run);
        buffer_text(
            &shared,
            &mut active,
            &run,
            fragment("turn-1", "item-0", "hello"),
        );

        let pending = active
            .pending_text
            .as_ref()
            .expect("fragment stays pending");
        assert_eq!(pending.turn.as_str(), "turn-1");
        assert_eq!(pending.item_key, "item-0");
        assert_eq!(pending.text, "hello");
        assert_eq!(active.next_seq, 0);
        assert_eq!(active.last_seq, None);
        assert!(!active.data_dropped);
        assert!(drain(&mut data).is_empty());
    }

    #[test]
    fn buffer_text_coalesces_adjacent_same_turn_item_fragments() {
        let (shared, mut data) = shared_with_data();
        let run = RunId::new("run-buffer-merge").expect("valid run id");
        let mut active = active_for(&run);
        buffer_text(
            &shared,
            &mut active,
            &run,
            fragment("turn-1", "item-0", "hel"),
        );
        buffer_text(
            &shared,
            &mut active,
            &run,
            fragment("turn-1", "item-0", "lo"),
        );
        assert_eq!(active.pending_text.as_ref().expect("pending").text, "hello");
        assert!(drain(&mut data).is_empty(), "coalescing emits nothing yet");

        flush_text(&shared, &mut active);
        let events = drain(&mut data);
        assert_eq!(events.len(), 1);
        assert_delta(&events[0], "turn-1", "item-0", "hello", 0);
        assert!(active.pending_text.is_none());
        assert_eq!(active.next_seq, 1);
        assert_eq!(active.last_seq, Some(0));
    }

    #[test]
    fn buffer_text_emits_the_tail_on_turn_and_item_boundaries() {
        let (shared, mut data) = shared_with_data();
        let run = RunId::new("run-buffer-boundary").expect("valid run id");
        let mut active = active_for(&run);
        buffer_text(
            &shared,
            &mut active,
            &run,
            fragment("turn-1", "item-0", "a"),
        );
        buffer_text(
            &shared,
            &mut active,
            &run,
            fragment("turn-2", "item-0", "b"),
        );
        assert_eq!(active.pending_text.as_ref().expect("pending").text, "b");
        buffer_text(
            &shared,
            &mut active,
            &run,
            fragment("turn-2", "item-1", "c"),
        );
        assert_eq!(active.pending_text.as_ref().expect("pending").text, "c");
        flush_text(&shared, &mut active);

        let events = drain(&mut data);
        assert_eq!(events.len(), 3);
        assert_delta(&events[0], "turn-1", "item-0", "a", 0);
        assert_delta(&events[1], "turn-2", "item-0", "b", 1);
        assert_delta(&events[2], "turn-2", "item-1", "c", 2);
        assert_eq!(active.next_seq, 3);
        assert_eq!(active.last_seq, Some(2));
    }

    #[test]
    fn buffer_text_splits_an_oversized_merge_instead_of_dropping_it() {
        let (shared, mut data) = shared_with_data();
        let run = RunId::new("run-buffer-split").expect("valid run id");
        let mut active = active_for(&run);
        let at_bound = "x".repeat(MAX_TEXT_FRAGMENT_BYTES);
        buffer_text(
            &shared,
            &mut active,
            &run,
            fragment("turn-1", "item-0", &at_bound),
        );
        buffer_text(
            &shared,
            &mut active,
            &run,
            fragment("turn-1", "item-0", "y"),
        );

        assert_eq!(active.pending_text.as_ref().expect("pending").text, "y");
        let events = drain(&mut data);
        assert_eq!(
            events.len(),
            1,
            "the boundary-sized tail is emitted, not lost"
        );
        assert_delta(&events[0], "turn-1", "item-0", &at_bound, 0);

        flush_text(&shared, &mut active);
        let events = drain(&mut data);
        assert_eq!(events.len(), 1);
        assert_delta(&events[0], "turn-1", "item-0", "y", 1);
    }

    #[test]
    fn text_helpers_ignore_a_stale_run_identity() {
        let (shared, mut data) = shared_with_data();
        let run = RunId::new("run-live").expect("valid run id");
        let stale = RunId::new("run-stale").expect("valid run id");
        let mut active = active_for(&run);

        buffer_text(
            &shared,
            &mut active,
            &stale,
            fragment("turn-1", "item-0", "stale"),
        );
        assert!(active.pending_text.is_none());
        emit_data(
            &shared,
            &mut active,
            &stale,
            EventPayload::ToolCallPreview {
                item_key: "item-0".to_owned(),
            },
        );
        assert!(drain(&mut data).is_empty());
        assert_eq!(active.next_seq, 0);

        buffer_text(
            &shared,
            &mut active,
            &run,
            fragment("turn-1", "item-0", "live"),
        );
        emit_data(
            &shared,
            &mut active,
            &stale,
            EventPayload::ToolCallPreview {
                item_key: "item-0".to_owned(),
            },
        );
        assert_eq!(
            active.pending_text.as_ref().expect("pending").text,
            "live",
            "a refused stale emit must not flush the live pending fragment"
        );
        assert!(drain(&mut data).is_empty());
        assert_eq!(active.next_seq, 0);
    }

    #[test]
    fn flush_text_without_pending_is_a_noop() {
        let (shared, mut data) = shared_with_data();
        let run = RunId::new("run-flush-empty").expect("valid run id");
        let mut active = active_for(&run);
        flush_text(&shared, &mut active);
        assert_eq!(active.next_seq, 0);
        assert_eq!(active.last_seq, None);
        assert!(!active.data_dropped);
        assert!(drain(&mut data).is_empty());
    }

    #[test]
    fn emit_data_flushes_pending_text_before_the_payload() {
        let (shared, mut data) = shared_with_data();
        let run = RunId::new("run-emit-order").expect("valid run id");
        let mut active = active_for(&run);
        buffer_text(
            &shared,
            &mut active,
            &run,
            fragment("turn-1", "item-0", "hello"),
        );
        emit_data(
            &shared,
            &mut active,
            &run,
            EventPayload::ToolCallPreview {
                item_key: "item-1".to_owned(),
            },
        );

        let events = drain(&mut data);
        assert_eq!(events.len(), 2);
        assert_delta(&events[0], "turn-1", "item-0", "hello", 0);
        assert_eq!(events[1].seq(), 1);
        assert!(matches!(
            events[1].payload(),
            EventPayload::ToolCallPreview { item_key } if item_key == "item-1"
        ));
        assert_eq!(active.next_seq, 2);
        assert_eq!(active.last_seq, Some(1));
    }

    #[test]
    fn flush_text_marks_dropped_without_consuming_a_sequence_when_data_is_full() {
        let (shared, mut data) = shared_with_data();
        let run = RunId::new("run-data-full").expect("valid run id");
        let session = SessionId::new("sess-data-full").expect("valid session id");
        let mut filler = 0usize;
        loop {
            let filler_event = RunEvent::new(
                session.clone(),
                run.clone(),
                filler as EventSequence,
                EventPayload::ToolCallPreview {
                    item_key: "filler".to_owned(),
                },
            );
            if shared.data_tx.try_send(filler_event).is_err() {
                break;
            }
            filler += 1;
        }
        assert_eq!(
            filler, DATA_CAPACITY,
            "filler saturates the bounded data channel"
        );

        let mut active = active_for(&run);
        buffer_text(
            &shared,
            &mut active,
            &run,
            fragment("turn-1", "item-0", "lost"),
        );
        flush_text(&shared, &mut active);
        assert!(
            active.pending_text.is_none(),
            "a dropped fragment is not retained"
        );
        assert!(active.data_dropped);
        assert_eq!(
            active.next_seq, 0,
            "dropped presentation traffic consumes no sequence"
        );
        assert_eq!(active.last_seq, None);

        emit_data(
            &shared,
            &mut active,
            &run,
            EventPayload::ToolCallPreview {
                item_key: "also-lost".to_owned(),
            },
        );
        assert!(active.data_dropped);
        assert_eq!(active.next_seq, 0);

        let events = drain(&mut data);
        assert_eq!(events.len(), DATA_CAPACITY);
        assert!(
            events
                .iter()
                .all(|event| matches!(event.payload(), EventPayload::ToolCallPreview { .. })),
            "the dropped fragment never reached the channel"
        );
    }
}

/// Top-up coverage for the private state-machine steps, reached only through a
/// direct in-file call with a hand-built [`ActiveRun`].
///
/// Every fixture uses the production constructor, so the shared registry,
/// policy, and bounded channels are real; only the run state is installed by
/// hand, which is what a step observes. Each case asserts the honest outcome a
/// step records rather than only that it returned. Deterministic: fixed
/// inputs, no fixed sleeps (blocking workers are released through channels),
/// and every wait is bounded.
#[cfg(test)]
mod cov_runtime_topup_private {
    use super::*;
    use nexus_core::{
        ApprovedScope, M0_REVISION, NormalizedArgs, ProviderCapabilities, TurnFinished,
        UsageFinality,
    };
    use std::sync::atomic::AtomicUsize;

    /// Bounded failure backstop for a gated blocking worker: passing cases
    /// release the worker immediately.
    const GATE_WAIT: Duration = Duration::from_secs(5);
    /// Bounded failure backstop for the control flusher observing a vanished
    /// consumer.
    const FLUSH_WAIT: Duration = Duration::from_secs(5);

    /// Entry/release gate for one blocking worker double. Entry is observed
    /// through the asynchronous receiver and release through the synchronous
    /// sender, so no test sleeps to order a worker step.
    struct Gate {
        entered: mpsc::UnboundedSender<()>,
        release: StdMutex<std::sync::mpsc::Receiver<()>>,
    }

    /// The test-side handles of one gate.
    struct GateHandle {
        entered: mpsc::UnboundedReceiver<()>,
        release: std::sync::mpsc::Sender<()>,
    }

    impl Gate {
        fn wait(&self) {
            let _ = self.entered.send(());
            let release = self
                .release
                .lock()
                .expect("gate release lock is never poisoned by a test");
            let _ = release.recv_timeout(GATE_WAIT);
        }
    }

    fn gate() -> (Gate, GateHandle) {
        let (entered_tx, entered_rx) = mpsc::unbounded_channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        (
            Gate {
                entered: entered_tx,
                release: StdMutex::new(release_rx),
            },
            GateHandle {
                entered: entered_rx,
                release: release_tx,
            },
        )
    }

    /// Waits for the worker to enter its blocking section, then releases it.
    async fn await_worker(handle: &mut GateHandle) {
        handle
            .entered
            .recv()
            .await
            .expect("the worker enters its blocking section");
        handle
            .release
            .send(())
            .expect("the worker release channel accepts a release");
    }

    /// Scripted provider double: one complete event batch per call, with an
    /// optional gate that keeps a worker deterministically live.
    struct ScriptedProvider {
        calls: AtomicUsize,
        script: StdMutex<VecDeque<Vec<ProviderEvent>>>,
        gate: Option<Gate>,
    }

    impl ScriptedProvider {
        fn new(script: Vec<Vec<ProviderEvent>>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                script: StdMutex::new(script.into()),
                gate: None,
            }
        }

        fn gated(script: Vec<Vec<ProviderEvent>>, gate: Gate) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                script: StdMutex::new(script.into()),
                gate: Some(gate),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl ProviderPort for ScriptedProvider {
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
            if let Some(gate) = &self.gate {
                gate.wait();
            }
            self.script
                .lock()
                .expect("script mutex is never poisoned by a test")
                .pop_front()
                .unwrap_or_else(|| stop_turn("script exhausted"))
        }
    }

    /// Tool double with an execution counter, a fixed observed success, and an
    /// optional gate that keeps the worker live until released.
    struct ScriptedTool {
        spec: ToolSpec,
        executed: AtomicUsize,
        outcome: ToolOutcome,
        gate: Option<Gate>,
    }

    impl ScriptedTool {
        fn new(name: &str, gate: Option<Gate>) -> Self {
            Self {
                spec: ToolSpec::new(
                    ToolId::new(name, M0_REVISION).expect("valid tool identity"),
                    format!("test double for {name}"),
                    r#"{"type":"object"}"#,
                )
                .expect("valid tool spec"),
                executed: AtomicUsize::new(0),
                outcome: ToolOutcome::new(
                    ExecutionStatus::Succeeded,
                    EffectState::KnownApplied,
                    Evidence::HostObserved,
                    "test double executed",
                    false,
                )
                .expect("valid tool outcome"),
                gate,
            }
        }

        fn execution_count(&self) -> usize {
            self.executed.load(Ordering::SeqCst)
        }
    }

    impl ToolPort for ScriptedTool {
        fn describe(&self) -> ToolSpec {
            self.spec.clone()
        }

        fn execute(&self, _call: &ToolCall, _context: &ToolContext) -> ToolOutcome {
            self.executed.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.gate {
                gate.wait();
            }
            self.outcome.clone()
        }
    }

    /// Real runtime over hand-installed run state, plus its event receivers and
    /// inspectable doubles.
    struct Bed {
        runtime: Runtime,
        shared: Arc<Shared>,
        data: mpsc::Receiver<RunEvent>,
        control: mpsc::Receiver<RunEvent>,
        provider: Arc<ScriptedProvider>,
        read: Arc<ScriptedTool>,
        write: Arc<ScriptedTool>,
    }

    fn make_bed(
        limits: Limits,
        has_approval_handler: bool,
        script: Vec<Vec<ProviderEvent>>,
        provider_gate: Option<Gate>,
        read_gate: Option<Gate>,
    ) -> Bed {
        let provider = Arc::new(match provider_gate {
            Some(gate) => ScriptedProvider::gated(script, gate),
            None => ScriptedProvider::new(script),
        });
        // The gate belongs to `host_read`: the automatic read tool is the one
        // an executed-call fixture dispatches, so that is where a live worker
        // is created deterministically.
        let read = Arc::new(ScriptedTool::new("host_read", read_gate));
        let write = Arc::new(ScriptedTool::new("host_write", None));
        let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![read.clone(), write.clone()];
        let (runtime, streams) = Runtime::try_new(
            RuntimeConfig {
                limits,
                policy: Policy::m0_test(),
                has_approval_handler,
            },
            provider.clone(),
            tools,
        )
        .expect("test wiring is valid");
        let shared = runtime.shared.clone();
        Bed {
            runtime,
            shared,
            data: streams.data,
            control: streams.control,
            provider,
            read,
            write,
        }
    }

    fn test_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime builds")
    }

    fn run_id(tag: &str) -> RunId {
        RunId::new(format!("run-{tag}")).expect("valid run id")
    }

    fn turn_id(tag: &str) -> TurnId {
        TurnId::new(format!("turn-{tag}")).expect("valid turn id")
    }

    fn call_id(tag: &str) -> CallId {
        CallId::new(format!("call-{tag}")).expect("valid call id")
    }

    fn approval_id(tag: &str) -> ApprovalId {
        ApprovalId::new(format!("approval-{tag}")).expect("valid approval id")
    }

    fn tool_id(name: &str) -> ToolId {
        ToolId::new(name, M0_REVISION).expect("valid tool identity")
    }

    fn session_id() -> SessionId {
        SessionId::new("sess-cov-topup").expect("valid session id")
    }

    fn stop_turn(text: &str) -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::TextDelta {
                item_key: "item-0".to_owned(),
                text: text.to_owned(),
            },
            ProviderEvent::TurnFinished(TurnFinished::new(
                FinishReason::Stop,
                Usage::new(None, None, UsageFinality::Final),
                None,
            )),
        ]
    }

    fn started_payload() -> EventPayload {
        EventPayload::RunStarted {
            request: RequestId::new("req-cov-topup").expect("valid request id"),
        }
    }

    fn usage_payload() -> EventPayload {
        EventPayload::UsageUpdated(Usage::new(Some(1), Some(2), UsageFinality::Final))
    }

    fn read_call(run: &RunId, call: CallId) -> ToolCall {
        ToolCall::new(
            run.clone(),
            turn_id("topup"),
            call,
            tool_id("host_read"),
            NormalizedArgs::new(r#"{"path":"src"}"#).expect("valid arguments"),
        )
    }

    fn write_call(run: &RunId, call: CallId) -> ToolCall {
        ToolCall::new(
            run.clone(),
            turn_id("topup"),
            call,
            tool_id("host_write"),
            NormalizedArgs::new(r#"{"path":"dst"}"#).expect("valid arguments"),
        )
    }

    fn current_call(call: &ToolCall, binding: Option<ApprovalBinding>) -> CurrentCall {
        CurrentCall {
            call: call.clone(),
            item_key: "item-0".to_owned(),
            provider_ref: "prov-ref-0".to_owned(),
            binding,
        }
    }

    fn binding_for(
        run: &RunId,
        call: &ToolCall,
        approval: ApprovalId,
        expires_at_elapsed: Duration,
    ) -> ApprovalBinding {
        ApprovalBinding::new(
            approval,
            run.clone(),
            call.call().clone(),
            call.tool().clone(),
            call.args().clone(),
            ApprovedScope::new("path:dst").expect("valid approved scope"),
            expires_at_elapsed,
            M0_REVISION,
        )
    }

    /// A live run with a fresh deadline and no admitted work.
    fn active_for(run: &RunId) -> ActiveRun {
        let now = StdInstant::now();
        ActiveRun {
            phase: RunState::Preparing,
            run: run.clone(),
            run_n: 1,
            session: session_id(),
            request: RequestId::new("req-cov-topup").expect("valid request id"),
            profile: "cov-profile".to_owned(),
            read_only: false,
            started: now,
            deadline: now + Duration::from_secs(60),
            token: CancellationToken::new(),
            wake: Arc::new(Notify::new()),
            cancelled: false,
            turn_seq: 0,
            call_seq: 0,
            approval_seq: 0,
            turns_used: 0,
            calls_used: 0,
            continuation: None,
            conversation: Vec::new(),
            history_len: 0,
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
            sequence_exhausted: false,
            last_usage: None,
            published_usage: None,
            force_terminal: None,
            terminal: None,
            terminal_error: None,
            delivery_closed: false,
            finished_sent: false,
        }
    }

    async fn install(bed: &Bed, active: ActiveRun) {
        let mut state = bed.shared.state.lock().await;
        state.active = Some(active);
    }

    /// Reads one value out of the retained run state, or `None` once the
    /// runtime no longer owns a run.
    async fn inspect<T>(bed: &Bed, read: impl FnOnce(&ActiveRun) -> T) -> Option<T> {
        let state = bed.shared.state.lock().await;
        state.active.as_ref().map(read)
    }

    /// The recorded `(status, effect, content)` of a step's pending outcome.
    fn recorded(active: &ActiveRun) -> (ExecutionStatus, EffectState, String) {
        let (_, outcome) = active
            .outcome_slot
            .clone()
            .expect("the step records exactly one outcome");
        (
            outcome.status(),
            outcome.effect(),
            outcome.content().to_owned(),
        )
    }

    fn assert_cancelled(step: Result<RunState, Terminal>) {
        let Err(terminal) = step else {
            panic!("a step for a run the state does not own never advances the phase");
        };
        assert_eq!(terminal.outcome, RunOutcome::Cancelled);
        assert!(
            terminal.error.is_none(),
            "an unowned run is a bare cancellation, not a fabricated failure"
        );
    }

    fn assert_limit(step: Result<RunState, Terminal>, message: &str) {
        let Err(terminal) = step else {
            panic!("an exhausted budget never advances the phase");
        };
        assert_eq!(terminal.outcome, RunOutcome::LimitReached);
        let error = terminal.error.expect("a limit outcome keeps its cause");
        assert_eq!(error.category(), ErrorCategory::ResourceLimit);
        assert_eq!(error.message(), message);
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    }

    #[test]
    fn a_step_for_an_unowned_run_is_a_bare_cancellation() {
        let bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
        let run = run_id("absent");
        let rt = test_rt();
        rt.block_on(async {
            assert_cancelled(bed.runtime.on_preparing(&run).await);
            assert_cancelled(bed.runtime.on_calling_model(&run).await);
            assert_cancelled(
                bed.runtime
                    .ingest_model_batch(
                        &run,
                        &turn_id("absent"),
                        stop_turn("ignored"),
                        &mut StreamedPrefix::default(),
                    )
                    .await,
            );
            assert_cancelled(bed.runtime.on_validating(&run).await);
            assert_cancelled(bed.runtime.on_awaiting(&run).await);
            assert_cancelled(bed.runtime.on_executing(&run).await);
            assert_cancelled(bed.runtime.on_recording(&run).await);
            assert_eq!(
                bed.provider.call_count(),
                0,
                "no step for an unowned run reaches the provider"
            );
            assert_eq!(bed.read.execution_count(), 0);
            assert_eq!(bed.write.execution_count(), 0);
        });
    }

    #[test]
    fn a_step_refuses_a_run_the_retained_state_does_not_own() {
        let mut bed = make_bed(
            Limits::m0_test(),
            true,
            vec![stop_turn("ignored")],
            None,
            None,
        );
        let live = run_id("live");
        let stale = run_id("stale");
        let rt = test_rt();
        rt.block_on(async {
            install(&bed, active_for(&live)).await;

            assert_cancelled(bed.runtime.on_calling_model(&stale).await);
            assert_cancelled(
                bed.runtime
                    .ingest_model_batch(
                        &stale,
                        &turn_id("stale"),
                        stop_turn("ignored"),
                        &mut StreamedPrefix::default(),
                    )
                    .await,
            );
            assert_cancelled(bed.runtime.on_validating(&stale).await);
            assert_cancelled(bed.runtime.on_awaiting(&stale).await);
            assert_cancelled(bed.runtime.on_executing(&stale).await);
            assert_cancelled(bed.runtime.on_recording(&stale).await);

            bed.runtime
                .set_terminal(&stale, RunOutcome::Completed, None)
                .await;
            assert_eq!(
                inspect(&bed, |active| active.terminal).await,
                Some(None),
                "a stale identity never decides the retained run"
            );

            assert_eq!(
                bed.runtime.take_terminal(&stale).await,
                (RunOutcome::Cancelled, None),
                "an unknown run has no terminal to report"
            );

            assert!(
                bed.runtime
                    .publish_control(&stale, usage_payload())
                    .await
                    .is_err(),
                "a stale identity publishes nothing"
            );
            assert!(bed.control.try_recv().is_err());

            bed.runtime
                .finish_run(&stale, RunOutcome::Failed, None)
                .await;
            assert_eq!(
                inspect(&bed, |active| active.run.clone()).await,
                Some(live.clone()),
                "the retained run is restored, never replaced"
            );
            assert_eq!(
                inspect(&bed, |active| active.finished_sent).await,
                Some(false),
                "a stale finalization never marks the retained run finished"
            );
            let state = bed.shared.state.lock().await;
            assert!(state.last.is_none(), "no finished run is recorded");
            drop(state);
            assert!(bed.control.try_recv().is_err());
            assert_eq!(bed.provider.call_count(), 0);
        });
    }

    #[test]
    fn terminal_bookkeeping_ignores_a_missing_or_already_decided_run() {
        let mut bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
        let missing = run_id("missing");
        let rt = test_rt();
        rt.block_on(async {
            bed.runtime
                .set_terminal(&missing, RunOutcome::Completed, None)
                .await;
            assert_eq!(
                bed.runtime.take_terminal(&missing).await,
                (RunOutcome::Cancelled, None)
            );
            assert!(
                bed.runtime
                    .publish_control(&missing, started_payload())
                    .await
                    .is_err()
            );
            bed.runtime
                .finish_run(&missing, RunOutcome::Failed, None)
                .await;
            let state = bed.shared.state.lock().await;
            assert!(state.active.is_none());
            assert!(state.last.is_none(), "a missing run records no run");
            drop(state);
            assert!(bed.control.try_recv().is_err());

            // A run that already decided its terminal keeps the first decision.
            let decided = run_id("decided");
            let mut active = active_for(&decided);
            active.terminal = Some(RunOutcome::Cancelled);
            install(&bed, active).await;
            bed.runtime
                .set_terminal(&decided, RunOutcome::Completed, None)
                .await;
            assert_eq!(
                inspect(&bed, |active| active.terminal).await,
                Some(Some(RunOutcome::Cancelled)),
                "the first terminal decision stands"
            );
            assert_eq!(
                inspect(&bed, |active| active.terminal_error.is_none()).await,
                Some(true),
                "a refused decision adds no cause"
            );
        });
    }

    #[test]
    fn an_exhausted_run_deadline_refuses_the_next_step() {
        let bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
        let run = run_id("deadline");
        let rt = test_rt();
        rt.block_on(async {
            let mut active = active_for(&run);
            active.started = StdInstant::now() - Duration::from_secs(120);
            active.deadline = StdInstant::now() - Duration::from_secs(60);
            install(&bed, active).await;

            assert_limit(
                bed.runtime.on_preparing(&run).await,
                "run duration exhausted",
            );
            assert_limit(
                bed.runtime.on_calling_model(&run).await,
                "run duration exhausted",
            );
            assert_limit(
                bed.runtime.on_validating(&run).await,
                "run duration exhausted",
            );
            assert_eq!(
                bed.provider.call_count(),
                0,
                "an exhausted run never reaches the provider"
            );
        });
    }

    #[test]
    fn executing_records_an_honest_outcome_before_any_dispatch() {
        let bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
        let run = run_id("pre-dispatch");
        let rt = test_rt();
        rt.block_on(async {
            // Cancelled before dispatch: unknown effect, no dispatch.
            let call = read_call(&run, call_id("cancelled"));
            let mut active = active_for(&run);
            active.cancelled = true;
            active.current = Some(current_call(&call, None));
            install(&bed, active).await;
            let step = bed.runtime.on_executing(&run).await;
            assert!(matches!(step, Ok(RunState::RecordingResult)));
            assert_eq!(
                inspect(&bed, recorded).await,
                Some((
                    ExecutionStatus::Cancelled,
                    EffectState::Unknown,
                    "cancelled before dispatch".to_owned(),
                )),
                "a cancelled dispatch claims no effect and states the reason"
            );
            assert_eq!(
                inspect(&bed, |active| active.force_terminal).await,
                Some(Some(RunOutcome::Cancelled))
            );
            assert_eq!(bed.read.execution_count(), 0);

            // Deadline passed before dispatch: timed out, no dispatch.
            let call = read_call(&run, call_id("deadline"));
            let mut active = active_for(&run);
            active.deadline = StdInstant::now() - Duration::from_secs(1);
            active.current = Some(current_call(&call, None));
            install(&bed, active).await;
            let step = bed.runtime.on_executing(&run).await;
            assert!(matches!(step, Ok(RunState::RecordingResult)));
            assert_eq!(
                inspect(&bed, recorded).await,
                Some((
                    ExecutionStatus::TimedOut,
                    EffectState::Unknown,
                    "run deadline passed before dispatch".to_owned(),
                ))
            );
            assert_eq!(
                inspect(&bed, |active| active.force_terminal).await,
                Some(Some(RunOutcome::LimitReached))
            );
            assert_eq!(bed.read.execution_count(), 0);

            // An approval binding that expired between the decision and the
            // dispatch is denied without claiming a timeout or an effect.
            let call = read_call(&run, call_id("expired-grant"));
            let approval = approval_id("expired");
            let expired = binding_for(&run, &call, approval, Duration::ZERO);
            let mut active = active_for(&run);
            active.current = Some(current_call(&call, Some(expired)));
            install(&bed, active).await;
            let step = bed.runtime.on_executing(&run).await;
            assert!(matches!(step, Ok(RunState::RecordingResult)));
            assert_eq!(
                inspect(&bed, recorded).await,
                Some((
                    ExecutionStatus::Denied,
                    EffectState::NotStarted,
                    "approval binding mismatch".to_owned(),
                )),
                "an unusable grant is denied as not started, never retried"
            );
            assert_eq!(
                inspect(&bed, |active| active.force_terminal).await,
                Some(None),
                "a denied dispatch lets the run continue honestly"
            );
            assert_eq!(bed.read.execution_count(), 0);
        });
    }

    #[test]
    fn validating_refuses_an_exhausted_concurrent_operation_budget() {
        let mut limits = Limits::m0_test();
        limits.max_concurrent_ops = 1;
        let bed = make_bed(limits, true, Vec::new(), None, None);
        let run = run_id("concurrency");
        let rt = test_rt();
        rt.block_on(async {
            let call = write_call(&run, call_id("queued"));
            let approval = approval_id("pending");
            let mut active = active_for(&run);
            active.queue.push_back(QueuedCall {
                call: call.clone(),
                item_key: "item-0".to_owned(),
                provider_ref: "prov-ref-0".to_owned(),
                needs_approval: true,
            });
            active.pending.insert(
                approval.clone(),
                PendingApproval {
                    binding: binding_for(&run, &call, approval, Duration::from_secs(60)),
                    directory: None,
                },
            );
            install(&bed, active).await;

            assert_limit(
                bed.runtime.on_validating(&run).await,
                "concurrent operation budget exhausted",
            );
            assert_eq!(
                inspect(&bed, |active| active.pending.len()).await,
                Some(1),
                "the live grant is neither consumed nor abandoned"
            );
            assert_eq!(
                inspect(&bed, |active| active.outcome_slot.is_none()).await,
                Some(true),
                "a refused validation records no call outcome"
            );
            assert_eq!(bed.write.execution_count(), 0);
        });
    }

    #[test]
    fn a_wake_that_does_not_cancel_never_abandons_the_model_worker() {
        let (gate, mut handle) = gate();
        let mut bed = make_bed(
            Limits::m0_test(),
            false,
            vec![stop_turn("ingested")],
            Some(gate),
            None,
        );
        let run = run_id("wake-provider");
        let rt = test_rt();
        rt.block_on(async {
            let active = active_for(&run);
            // A stored wake permit with no cancellation behind it: the worker
            // is still owned and its batch must still be ingested.
            active.wake.notify_one();
            install(&bed, active).await;

            let (step, ()) = tokio::join!(
                bed.runtime.on_calling_model(&run),
                await_worker(&mut handle),
            );
            let Err(terminal) = step else {
                panic!("an ingested stop turn is a terminal completion");
            };
            assert_eq!(terminal.outcome, RunOutcome::Completed);
            assert!(terminal.error.is_none());
            assert_eq!(bed.provider.call_count(), 1);
            let event = bed
                .data
                .try_recv()
                .expect("the batch after the stray wake is published");
            assert!(
                matches!(event.payload(), EventPayload::AssistantTextDelta(fragment) if fragment.text == "ingested"),
                "the batch after the stray wake is ingested, not dropped"
            );
            assert!(bed.data.try_recv().is_err());
            let usage = bed
                .control
                .try_recv()
                .expect("the ingested turn published its usage update");
            assert!(matches!(usage.payload(), EventPayload::UsageUpdated(_)));
            assert!(bed.control.try_recv().is_err());
            assert_eq!(
                inspect(&bed, |active| (active.next_seq, active.last_seq, active.data_dropped))
                    .await,
                Some((2, Some(1), false)),
                "the one text delta and the one usage update hold sequences 0 and 1: \
                 a stray wake consumes none and drops nothing"
            );
            assert_eq!(
                inspect(&bed, |active| active.force_terminal.is_none()).await,
                Some(true),
                "a stray wake forces no terminal"
            );
        });
    }

    #[test]
    fn a_wake_that_does_not_cancel_never_abandons_the_tool_worker() {
        let (gate, mut handle) = gate();
        let mut bed = make_bed(Limits::m0_test(), false, Vec::new(), None, Some(gate));
        let run = run_id("wake-tool");
        let rt = test_rt();
        rt.block_on(async {
            let call = read_call(&run, call_id("woken"));
            let mut active = active_for(&run);
            active.wake.notify_one();
            active.current = Some(current_call(&call, None));
            install(&bed, active).await;

            let (step, ()) =
                tokio::join!(bed.runtime.on_executing(&run), await_worker(&mut handle),);
            assert!(matches!(step, Ok(RunState::RecordingResult)));
            assert_eq!(
                bed.read.execution_count(),
                1,
                "a stray wake never cancels, retries, or double-dispatches a live call"
            );
            assert_eq!(
                inspect(&bed, recorded).await,
                Some((
                    ExecutionStatus::Succeeded,
                    EffectState::KnownApplied,
                    "test double executed".to_owned(),
                )),
                "the worker's own outcome is recorded, not a cancellation"
            );
            assert_eq!(
                inspect(&bed, |active| active.force_terminal.is_none()).await,
                Some(true),
                "a stray wake forces no terminal"
            );
            let started = bed.control.try_recv().expect("the dispatch was announced");
            assert!(matches!(started.payload(), EventPayload::ToolStarted(_)));
            assert!(bed.control.try_recv().is_err());
        });
    }

    #[test]
    fn emit_control_refuses_a_run_it_does_not_own() {
        let mut bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
        let live = run_id("emit-live");
        let stale = run_id("emit-stale");
        let rt = test_rt();
        rt.block_on(async {
            install(&bed, active_for(&live)).await;
            let mut state = bed.shared.state.lock().await;
            let mut active = state
                .active
                .take()
                .expect("the run is retained while the emit is attempted");
            let outcome = emit_control(&bed.shared, &mut active, &stale, started_payload());
            assert_eq!(
                outcome,
                ControlOutcome::Closed,
                "an emit for another run is refused, never published"
            );
            assert_eq!(active.next_seq, 0, "a refused emit consumes no sequence");
            assert!(active.last_seq.is_none());
            assert!(
                !active.data_dropped,
                "a refused emit is not reported as dropped presentation traffic"
            );
            state.active = Some(active);
            drop(state);
            assert!(bed.control.try_recv().is_err());
        });
    }

    #[test]
    fn a_vanished_control_consumer_retains_the_committed_outbox_event() {
        let bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
        let run = run_id("outbox");
        let rt = test_rt();
        rt.block_on(async {
            install(&bed, active_for(&run)).await;
            // Saturate the bounded control channel through the production
            // sender, so a control event that cannot enter it must be committed
            // to the bounded outbox instead of being dropped.
            for index in 0..CONTROL_CAPACITY as u64 {
                let filler = RunEvent::new(session_id(), run.clone(), index, started_payload());
                bed.shared
                    .control_tx
                    .try_reserve()
                    .expect("a filler fits the declared control bound")
                    .send(filler);
            }
            assert_eq!(bed.shared.control_tx.capacity(), 0);
            let buffered = RunEvent::new(session_id(), run.clone(), 0, usage_payload());
            bed.shared
                .outbox
                .lock()
                .expect("the control outbox is readable")
                .push_back(buffered.clone());

            // The only consumer disappears while an event is committed to the
            // outbox. The flusher must retain that undeliverable event and
            // classify the channel as closed rather than as saturation.
            drop(bed.control);
            tokio::time::timeout(FLUSH_WAIT, flush_control(bed.shared.clone()))
                .await
                .expect("the flusher settles on a closed control channel");

            assert!(
                bed.shared.control_closed.load(Ordering::SeqCst),
                "a vanished consumer is classified as closed, never as saturation"
            );
            let outbox = bed
                .shared
                .outbox
                .lock()
                .expect("the control outbox is readable");
            assert_eq!(
                outbox.len(),
                1,
                "the undeliverable event is retained, never dropped"
            );
            assert_eq!(
                outbox.front().map(RunEvent::seq),
                Some(buffered.seq()),
                "the retained event is the committed one, in order"
            );
            assert!(!bed.shared.flushing.load(Ordering::SeqCst));
            drop(outbox);
            ensure_flusher(&bed.shared);
            assert!(
                !bed.shared.flushing.load(Ordering::SeqCst),
                "closed transport is terminal and cannot schedule another pass"
            );
        });
    }

    #[test]
    fn the_run_disappearing_mid_tool_execution_cancels_the_step() {
        // The exact state a supervised driver failure leaves behind: the
        // blocking worker is live while the runtime no longer retains its run.
        let (gate, mut handle) = gate();
        let bed = make_bed(Limits::m0_test(), false, Vec::new(), None, Some(gate));
        let run = run_id("vanishing");
        let rt = test_rt();
        rt.block_on(async {
            let call = read_call(&run, call_id("orphaned"));
            install(&bed, active_for(&run)).await;
            {
                let mut state = bed.shared.state.lock().await;
                let mut active = state.active.take().expect("the run is retained");
                active.current = Some(current_call(&call, None));
                state.active = Some(active);
            }

            let (step, ()) = tokio::join!(bed.runtime.on_executing(&run), async {
                handle.entered.recv().await;
                // The worker is inside its blocking section when the retained
                // run disappears underneath the executing step.
                bed.shared.state.lock().await.active = None;
                let _ = handle.release.send(());
            });
            assert_cancelled(step);
            assert_eq!(bed.read.execution_count(), 1);
        });
    }

    #[test]
    fn a_replaced_run_mid_tool_execution_cancels_the_step() {
        let (gate, mut handle) = gate();
        let bed = make_bed(Limits::m0_test(), false, Vec::new(), None, Some(gate));
        let run = run_id("replaced");
        let rt = test_rt();
        rt.block_on(async {
            let call = read_call(&run, call_id("replaced"));
            install(&bed, active_for(&run)).await;
            {
                let mut state = bed.shared.state.lock().await;
                let mut active = state.active.take().expect("the run is retained");
                active.current = Some(current_call(&call, None));
                state.active = Some(active);
            }

            let (step, ()) = tokio::join!(bed.runtime.on_executing(&run), async {
                handle.entered.recv().await;
                // A different run now owns the slot the step resumes into.
                let mut state = bed.shared.state.lock().await;
                let mut active = state.active.take().expect("the run is retained");
                active.run = run_id("successor");
                active.current = None;
                state.active = Some(active);
                let _ = handle.release.send(());
            });
            assert_cancelled(step);
        });
    }

    #[test]
    fn finish_run_records_an_outcome_a_step_never_recorded() {
        let mut bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
        let run = run_id("unrecorded-outcome");
        let rt = test_rt();
        rt.block_on(async {
            let call = write_call(&run, call_id("decided-outcome"));
            let mut active = active_for(&run);
            // The call already produced an outcome that no recording step
            // consumed, and no current call is left to re-report.
            active.outcome_slot = Some((call.call().clone(), cancelled_outcome("left over")));
            install(&bed, active).await;

            bed.runtime
                .finish_run(&run, RunOutcome::Cancelled, None)
                .await;

            let finished = bed
                .control
                .try_recv()
                .expect("the pending outcome is published at finalization");
            let EventPayload::ToolFinished(info) = finished.payload() else {
                panic!("a pending call outcome is published as ToolFinished");
            };
            assert_eq!(info.call, *call.call());
            assert_eq!(info.outcome.status(), ExecutionStatus::Cancelled);
            assert_eq!(info.outcome.effect(), EffectState::Unknown);

            let terminal = bed
                .control
                .try_recv()
                .expect("the terminal follows the retained outcome");
            assert!(terminal.is_terminal());
            assert!(bed.control.try_recv().is_err());
            let state = bed.shared.state.lock().await;
            let last = state.last.as_ref().expect("the finished run is retained");
            assert_eq!(last.outcome, RunOutcome::Cancelled);
            assert_eq!(last.outcomes.len(), 1);
            assert_eq!(last.outcomes[0].call, *call.call());
        });
    }

    #[test]
    fn finish_run_reports_an_unknown_cancellation_for_an_unreported_call() {
        let mut bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
        let run = run_id("unreported-call");
        let rt = test_rt();
        rt.block_on(async {
            let call = write_call(&run, call_id("never-reported"));
            let mut active = active_for(&run);
            // The call was admitted and dispatched, but no outcome was ever
            // recorded for it: the honest record is an unknown cancellation.
            active.current = Some(current_call(&call, None));
            install(&bed, active).await;

            bed.runtime
                .finish_run(&run, RunOutcome::Cancelled, None)
                .await;

            let reported = bed
                .control
                .try_recv()
                .expect("an unreported call is reported honestly, not dropped");
            let EventPayload::ToolFinished(info) = reported.payload() else {
                panic!("an unreported call is published as ToolFinished");
            };
            assert_eq!(info.call, *call.call());
            assert_eq!(info.outcome.status(), ExecutionStatus::Cancelled);
            assert_eq!(info.outcome.effect(), EffectState::Unknown);
            assert_eq!(info.outcome.evidence(), Evidence::Uncertain);
            assert_eq!(
                info.outcome.content(),
                "run ended before the tool outcome was recorded"
            );

            let terminal = bed
                .control
                .try_recv()
                .expect("the terminal follows the honest cancellation");
            assert!(terminal.is_terminal());
            assert!(bed.control.try_recv().is_err());
            let state = bed.shared.state.lock().await;
            let last = state.last.as_ref().expect("the finished run is retained");
            assert_eq!(last.outcomes.len(), 1);
            assert_eq!(last.outcomes[0].status, ExecutionStatus::Cancelled);
            assert_eq!(last.outcomes[0].effect, EffectState::Unknown);
            assert_eq!(last.outcomes[0].evidence, Evidence::Uncertain);
        });
    }

    #[test]
    fn closed_control_transport_retains_events_without_rescheduling() {
        let bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
        let run = run_id("outbox-respawn");
        let rt = test_rt();
        rt.block_on(async {
            // Two committed events and no consumer. The flusher must retain
            // both in order. A disconnected receiver is terminal, so it must
            // not respawn itself while those events remain undeliverable.
            let first = RunEvent::new(session_id(), run.clone(), 0, usage_payload());
            let second = RunEvent::new(session_id(), run.clone(), 1, started_payload());
            {
                let mut outbox = bed
                    .shared
                    .outbox
                    .lock()
                    .expect("the control outbox is readable");
                outbox.push_back(first.clone());
                outbox.push_back(second.clone());
            }
            drop(bed.control);
            ensure_flusher(&bed.shared);

            tokio::time::timeout(FLUSH_WAIT, async {
                while !bed.shared.control_closed.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the flusher classifies the vanished consumer and stops");

            assert!(bed.shared.control_closed.load(Ordering::SeqCst));
            assert!(!bed.shared.flushing.load(Ordering::SeqCst));
            let outbox = bed
                .shared
                .outbox
                .lock()
                .expect("the control outbox is readable");
            assert_eq!(
                outbox.len(),
                2,
                "neither retained event is dropped after transport closure"
            );
            assert_eq!(outbox.front().map(RunEvent::seq), Some(first.seq()));
            assert_eq!(outbox.back().map(RunEvent::seq), Some(second.seq()));
            drop(outbox);
            ensure_flusher(&bed.shared);
            assert!(
                !bed.shared.flushing.load(Ordering::SeqCst),
                "a retained outbox does not restart a flusher after closure"
            );
        });
    }

    #[test]
    fn finalization_does_not_claim_a_terminal_delivered_to_a_closed_channel() {
        let bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
        let run = run_id("terminal-undelivered");
        let rt = test_rt();
        rt.block_on(async {
            install(&bed, active_for(&run)).await;
            drop(bed.control);

            bed.runtime
                .finish_run(&run, RunOutcome::Completed, None)
                .await;

            let state = bed.shared.state.lock().await;
            let last = state.last.as_ref().expect("finalized run is retained");
            assert!(!last.terminal_delivered);
            assert!(last.truncated, "closed control delivery remains observable");
            drop(state);
            assert!(
                bed.shared
                    .outbox
                    .lock()
                    .expect("control outbox readable")
                    .is_empty(),
                "finalization clears undeliverable events after recording their status"
            );
            assert!(!bed.shared.flushing.load(Ordering::SeqCst));
        });
    }

    #[test]
    fn awaiting_without_a_current_call_returns_to_validation() {
        let mut bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
        let run = run_id("awaiting-no-current");
        let rt = test_rt();
        rt.block_on(async {
            // The approval wait observes no current call: the step hands the
            // run back to validation instead of waiting on a missing binding.
            let mut active = active_for(&run);
            active.phase = RunState::AwaitingApproval;
            install(&bed, active).await;

            let step = bed.runtime.on_awaiting(&run).await;
            assert!(matches!(step, Ok(RunState::ValidatingTools)));
            assert!(bed.control.try_recv().is_err());
        });
    }

    #[test]
    fn awaiting_a_bindingless_call_continues_to_execution() {
        let mut bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
        let run = run_id("awaiting-no-binding");
        let rt = test_rt();
        rt.block_on(async {
            // A current call with no approval binding needs no decision: the
            // wait hands straight over to execution.
            let call = read_call(&run, call_id("bindingless"));
            let mut active = active_for(&run);
            active.phase = RunState::AwaitingApproval;
            active.current = Some(current_call(&call, None));
            install(&bed, active).await;

            let step = bed.runtime.on_awaiting(&run).await;
            assert!(matches!(step, Ok(RunState::ExecutingTool)));
            assert_eq!(bed.read.execution_count(), 0);
            assert!(bed.control.try_recv().is_err());
        });
    }

    #[test]
    fn awaiting_a_zero_remaining_wait_re_measures_instead_of_sleeping() {
        let bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
        let run = run_id("awaiting-zero-wait");
        let rt = test_rt();
        rt.block_on(async {
            // The grant expires at the current reading: the wait measure is
            // zero, so the step re-measures rather than sleeping on nothing.
            let call = read_call(&run, call_id("zero-wait"));
            let approval = approval_id("zero");
            let mut active = active_for(&run);
            active.phase = RunState::AwaitingApproval;
            let now_elapsed = active.started.elapsed();
            active.current = Some(current_call(
                &call,
                Some(binding_for(&run, &call, approval, now_elapsed)),
            ));
            install(&bed, active).await;

            let step = bed.runtime.on_awaiting(&run).await;
            assert!(matches!(step, Ok(RunState::RecordingResult)));
            assert_eq!(
                inspect(&bed, recorded).await,
                Some((
                    ExecutionStatus::Denied,
                    EffectState::NotStarted,
                    "approval expired".to_owned(),
                )),
                "a zero remaining wait resolves the expiry, never a busy loop"
            );
        });
    }

    #[test]
    fn executing_a_call_with_no_recorded_tool_is_refused_before_dispatch() {
        let mut bed = make_bed(Limits::m0_test(), false, Vec::new(), None, None);
        let run = run_id("executing-no-current");
        let rt = test_rt();
        rt.block_on(async {
            // An execution step with nothing to execute hands back to
            // validation rather than inventing a call.
            install(&bed, active_for(&run)).await;

            let step = bed.runtime.on_executing(&run).await;
            assert!(matches!(step, Ok(RunState::ValidatingTools)));
            assert_eq!(bed.read.execution_count(), 0);
            assert!(bed.control.try_recv().is_err());
        });
    }

    #[test]
    fn executing_an_unregistered_or_changed_tool_is_denied_without_dispatch() {
        let bed = make_bed(Limits::m0_test(), false, Vec::new(), None, None);
        let run = run_id("executing-tool-changed");
        let rt = test_rt();
        rt.block_on(async {
            // A tool identity the registry no longer carries.
            let unknown = ToolCall::new(
                run.clone(),
                turn_id("topup"),
                call_id("unknown"),
                tool_id("host_absent"),
                NormalizedArgs::new(r#"{"path":"src"}"#).expect("valid arguments"),
            );
            let mut active = active_for(&run);
            active.current = Some(current_call(&unknown, None));
            install(&bed, active).await;

            let step = bed.runtime.on_executing(&run).await;
            assert!(matches!(step, Ok(RunState::RecordingResult)));
            assert_eq!(
                inspect(&bed, recorded).await,
                Some((
                    ExecutionStatus::Denied,
                    EffectState::NotStarted,
                    "unknown tool".to_owned(),
                )),
                "a vanished tool is denied as not started"
            );

            // A tool identity that shares the name but not the revision.
            let changed = ToolCall::new(
                run.clone(),
                turn_id("topup"),
                call_id("changed"),
                ToolId::new("host_read", M0_REVISION + 1).expect("valid tool identity"),
                NormalizedArgs::new(r#"{"path":"src"}"#).expect("valid arguments"),
            );
            let mut active = active_for(&run);
            active.current = Some(current_call(&changed, None));
            install(&bed, active).await;

            let step = bed.runtime.on_executing(&run).await;
            assert!(matches!(step, Ok(RunState::RecordingResult)));
            assert_eq!(
                inspect(&bed, recorded).await,
                Some((
                    ExecutionStatus::Denied,
                    EffectState::NotStarted,
                    "tool revision changed".to_owned(),
                )),
                "a changed tool revision is denied as not started"
            );
            assert_eq!(bed.read.execution_count(), 0);
        });
    }

    #[test]
    fn a_recording_step_with_nothing_recorded_returns_to_validation() {
        let mut bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
        let run = run_id("nothing-recorded");
        let rt = test_rt();
        rt.block_on(async {
            let call = read_call(&run, call_id("recorded"));
            let mut active = active_for(&run);
            active.current = Some(current_call(&call, None));
            install(&bed, active).await;

            let step = bed.runtime.on_recording(&run).await;
            assert!(
                matches!(step, Ok(RunState::ValidatingTools)),
                "a step with nothing to record returns to validation"
            );
            assert_eq!(
                inspect(&bed, |active| active.outcomes.len()).await,
                Some(0),
                "no outcome is fabricated for an unrecorded call"
            );
            assert_eq!(
                inspect(&bed, |active| active.conversation.len()).await,
                Some(0)
            );
            assert!(bed.control.try_recv().is_err());
            assert_eq!(
                inspect(&bed, |active| {
                    active.outcome_slot.is_none() && active.force_terminal.is_none()
                })
                .await,
                Some(true)
            );
        });
    }
}
