//! Single-active-run execution loop: lifecycle, policy, ordered events.
//!
//! M0 scope: one active run (a second `Submit` gets an explicit `Busy`
//! reply), declared-order sequential tool dispatch, and opt-in durable tool
//! operation records. Core ports stay synchronous; this crate adapts them
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
//! - model context retains exact input, call, result, item-key, and provider
//!   references; reduction removes only whole completed exchanges.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant as StdInstant, SystemTime, UNIX_EPOCH};

#[cfg(test)]
use nexus_context::CONTEXT_NOTICE_ITEM_KEY;
use nexus_context::{
    CONTEXT_NOTICE_TEXT, ContextBounds, TokenEstimator, context_calls_are_paired,
    history_exchange_effects_certain, is_context_notice, is_retained_evidence, memory_bytes,
    project_bounded_history,
};
use nexus_core::commands::{EventSequence, MAX_TEXT_FRAGMENT_BYTES};
use nexus_core::{
    AgentError, ApprovalBinding, ApprovalId, ApprovalNotice, ApproveCommand, AssistantText,
    CallCandidate, CallId, CancelCommand, CancellationToken, Command, CommandReply,
    CommandResponse, ContinuationData, DenyCommand, EffectState, ErrorCategory, EventPayload,
    Evidence, ExecutionStatus, FinishReason, GetSnapshotCommand, ItemKey, Limits,
    ListSessionsCommand, ModelContextItem, ModelInstructions, ModelRequest, OutcomeSummary,
    PersistenceState, ProviderContext, ProviderEvent, ProviderPort, RequestId,
    RestoreSessionCommand, RetryGuidance, RunEvent, RunFinished, RunId, RunLifecycle, RunOutcome,
    STORE_FORMAT_REVISION, SessionCheckpoint, SessionId, SessionStore, Snapshot, SubmitCommand,
    ToolAccess, ToolCall, ToolContext, ToolFinishedInfo, ToolId, ToolIntentRecord, ToolOutcome,
    ToolOutcomeRecord, ToolPort, ToolRegistration, ToolSpec, ToolStartedInfo, TurnId, Usage,
    UsageFinality,
};
use nexus_session::recover_session;
use nexus_validation::CompiledSchema;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::task::JoinHandle;

use crate::policy::Policy;
use crate::transport::EventStreams;
mod context_compaction;
#[cfg(test)]
use crate::transport::{CONTROL_CAPACITY, DATA_CAPACITY};
use context_compaction::{PendingCompaction, prepare_context_request};

// Observation time for cooperative cleanup, not additional execution authority.
const TOOL_CLEANUP_GRACE: Duration = Duration::from_secs(2);

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
    local_reasoning: AtomicBool,
    diagnostic_writer: StdMutex<Option<Box<dyn std::io::Write + Send>>>,
    instructions: ModelInstructions,
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
    session_store: Option<Arc<StdMutex<Box<dyn SessionStore + Send>>>>,
    /// An interrupted write must be reconciled by reopening, never retried.
    store_uncertain: AtomicBool,
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

/// Cloneable owner for one session store shared by multiple runtime instances.
/// The store's own single-writer lock remains held until every clone drops.
#[derive(Clone)]
pub struct SessionStoreHandle(Arc<StdMutex<Box<dyn SessionStore + Send>>>);

impl SessionStoreHandle {
    /// Wraps an opened store so runtimes can share one serialized owner.
    #[must_use]
    pub fn new(store: Box<dyn SessionStore + Send>) -> Self {
        Self(Arc::new(StdMutex::new(store)))
    }

    /// Deletes one explicitly selected persisted conversation.
    pub fn delete_session(&self, session: &SessionId) -> Result<(), AgentError> {
        self.0
            .lock()
            .map_err(|_| runtime_error(ErrorCategory::Internal, "session store is unavailable"))?
            .delete_session(session)
    }

    /// Explicitly reads and imports legacy sessions through the shared store.
    pub fn import_legacy_sessions(&self) -> Result<usize, AgentError> {
        self.0
            .lock()
            .map_err(|_| runtime_error(ErrorCategory::Internal, "session store is unavailable"))?
            .import_legacy_sessions()
    }
}

struct RegisteredTool {
    spec: ToolSpec,
    schema: CompiledSchema,
    port: Arc<dyn ToolPort + Send + Sync>,
    access: ToolAccess,
}

struct State {
    active: Option<ActiveRun>,
    restore_busy: bool,
    last: Option<FinishedRun>,
    /// Whole completed exchanges, globally bounded by the context budgets.
    history: VecDeque<RetainedExchange>,
    /// Sessions whose completed cache lost one or more exchanges and need a
    /// model-visible notice on their next accepted run.
    history_omissions: Vec<HistoryOmission>,
    /// Bounded fallback for overflowed per-session omission markers. Kept
    /// pending for the runtime lifetime so no session silently loses notice.
    history_omission_fallback_items: usize,
    /// Once uncertain evidence cannot be retained within the finite cache,
    /// refuse further model work rather than silently authorizing blind retries.
    history_safety_exhausted: bool,
    /// Sessions initialized or explicitly loaded through this runtime's store.
    durable_sessions: HashSet<SessionId>,
    /// Workers whose termination is not yet established. A non-empty list
    /// blocks new runs and further dispatch; at most one entry exists in the
    /// sequential M0 baseline.
    quarantine: Vec<QuarantineMeta>,
}

struct RetainedExchange {
    session: SessionId,
    items: Vec<ModelContextItem>,
    derived_memory: Option<String>,
}

struct HistoryOmission {
    session: SessionId,
    removed_items: usize,
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
    access: ToolAccess,
}

struct CurrentCall {
    call: ToolCall,
    item_key: String,
    provider_ref: String,
    binding: Option<ApprovalBinding>,
    dispatched: bool,
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
    reasoning: String,
    /// Preview item keys already announced as `ToolCallPreview`.
    preview_keys: HashSet<String>,
    retained_bytes: usize,
    identity_bytes: usize,
    event_count: usize,
    usage_updates: usize,
    usage_prefix: Vec<Usage>,
    usage_replayed: Vec<Usage>,
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
const MAX_HISTORY_OMISSION_SESSIONS: usize = 128;

struct ActiveRun {
    context_management: nexus_core::ContextManagementSettings,
    derived_memory: Option<String>,
    pending_compaction: Option<PendingCompaction>,
    input_estimate: u64,
    token_estimator: TokenEstimator,
    summary_token_estimator: TokenEstimator,
    instruction_bytes: usize,
    context_note: String,
    context_note_bytes: usize,
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
    /// Fresh task requests may replace discarded responses, never resume them.
    output_limit_recovery_used: bool,
    task_output_limit_recovery_attempts: u8,
    response_recovery_note: Option<&'static str>,
    access_failure_attempts: usize,
    continuation: Option<ContinuationData>,
    conversation: Vec<ModelContextItem>,
    /// Index of the first context item not yet committed to source history.
    source_cursor: usize,
    history_len: usize,
    inherited_history: bool,
    initial_context_reduction: Option<ContextReduction>,
    pending_omission_items: usize,
    queue: VecDeque<QueuedCall>,
    current: Option<CurrentCall>,
    dispatched_calls: HashSet<CallId>,
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
    persistence_error: Option<AgentError>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContextReduction {
    method: nexus_core::ContextReductionMethod,
    removed_items: usize,
    retained_items: usize,
    retained_bytes: usize,
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

/// Provider access classification is deliberately narrower than a blanket
/// retry of protocol/internal errors. Access failures must opt in; local
/// input, credential-safety, integrity, and resource failures stay terminal.
fn provider_access_failure(error: &AgentError) -> bool {
    matches!(
        error.category(),
        ErrorCategory::Authentication
            | ErrorCategory::PermissionDenied
            | ErrorCategory::RateLimited
            | ErrorCategory::Timeout
            | ErrorCategory::Protocol
    ) && error.retry() != nexus_core::RetryGuidance::DoNotRetry
}

fn provider_recovery_delay(attempt: usize) -> Duration {
    Duration::from_secs([1, 2, 5, 10, 30, 60][attempt.min(5)])
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

// A terminal may be owned by a full-screen frontend. Uncoordinated writes
// invalidate its screen buffer; retain diagnostics only for redirected stderr.
fn diagnostic(message: std::fmt::Arguments<'_>) {
    diagnostic_to(
        message,
        &mut std::io::stderr().lock(),
        std::io::IsTerminal::is_terminal(&std::io::stderr()),
    );
}

fn diagnostic_to(
    message: std::fmt::Arguments<'_>,
    writer: &mut impl std::io::Write,
    terminal: bool,
) {
    if !terminal {
        let _ = writeln!(writer, "{message}");
    }
}

impl Shared {
    fn diagnostic(&self, message: std::fmt::Arguments<'_>) {
        use std::io::Write;
        if let Ok(mut writer) = self.diagnostic_writer.lock()
            && let Some(sink) = writer.as_mut()
        {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let line = format!("timestamp_unix_s={timestamp} {message}\n");
            if sink
                .write_all(line.as_bytes())
                .and_then(|()| sink.flush())
                .is_ok()
            {
                return;
            }
            // Do not repeatedly retry a failed sink or write into a TUI terminal.
            *writer = None;
            diagnostic(format_args!(
                "nexus-runtime: local diagnostic writer failed"
            ));
        }
        diagnostic(message);
    }
}

impl Runtime {
    /// Routes metadata-only runtime diagnostics to a host-owned writer instead
    /// of stderr. Install before submitting runs; the host owns file retention.
    /// Write failure falls back to redirected (never terminal) stderr.
    #[must_use]
    pub fn with_diagnostic_writer(self, writer: impl std::io::Write + Send + 'static) -> Self {
        *self
            .shared
            .diagnostic_writer
            .lock()
            .expect("diagnostic writer lock") = Some(Box::new(writer));
        self.shared.diagnostic(format_args!(
            "nexus-runtime: context budget configured_items={} effective_items={} max_bytes={}",
            self.shared.limits.retained_context_items,
            self.shared.context_items_bound,
            nexus_core::provider::MAX_CONVERSATION_BYTES,
        ));
        self
    }

    /// Fallible production constructor: validates limits, provider
    /// capabilities, and every host registration (one `describe()` call per
    /// tool, exact M0 revision, no duplicate names, closed-schema compile)
    /// before any run can start. The access classification is taken from the
    /// host [`ToolRegistration`], never from the tool's own descriptor.
    pub fn try_new(
        config: RuntimeConfig,
        provider: Arc<dyn ProviderPort + Send + Sync>,
        tools: Vec<ToolRegistration>,
    ) -> Result<(Self, EventStreams), AgentError> {
        Self::try_new_with_instructions(config, provider, tools, ModelInstructions::default())
    }

    /// Constructs a runtime with an immutable host-authorized instruction snapshot.
    /// Instructions are not conversation history and never authorize tools.
    /// Existing callers opt out through the empty default in [`Self::try_new`].
    pub fn try_new_with_instructions(
        config: RuntimeConfig,
        provider: Arc<dyn ProviderPort + Send + Sync>,
        tools: Vec<ToolRegistration>,
        instructions: ModelInstructions,
    ) -> Result<(Self, EventStreams), AgentError> {
        Self::try_new_inner(config, provider, tools, instructions, None)
    }

    /// Constructs a runtime with an explicit session store. Tool intents are
    /// durably recorded before dispatch and observed outcomes and accepted
    /// source items are recorded afterward. Restore commands and terminal
    /// checkpoints are not yet wired; terminal persistence therefore remains
    /// `Ephemeral` unless a required write failed.
    pub fn try_new_with_session_store(
        config: RuntimeConfig,
        provider: Arc<dyn ProviderPort + Send + Sync>,
        tools: Vec<ToolRegistration>,
        instructions: ModelInstructions,
        store: Box<dyn SessionStore + Send>,
    ) -> Result<(Self, EventStreams), AgentError> {
        Self::try_new_inner(
            config,
            provider,
            tools,
            instructions,
            Some(Arc::new(StdMutex::new(store))),
        )
    }

    /// Constructs a runtime using a store handle shared with other runtimes.
    /// Each instance retains independent run/history state while store access
    /// is serialized through the common owner.
    pub fn try_new_with_session_store_handle(
        config: RuntimeConfig,
        provider: Arc<dyn ProviderPort + Send + Sync>,
        tools: Vec<ToolRegistration>,
        instructions: ModelInstructions,
        store: SessionStoreHandle,
    ) -> Result<(Self, EventStreams), AgentError> {
        Self::try_new_inner(config, provider, tools, instructions, Some(store.0))
    }

    fn try_new_inner(
        config: RuntimeConfig,
        provider: Arc<dyn ProviderPort + Send + Sync>,
        tools: Vec<ToolRegistration>,
        instructions: ModelInstructions,
        session_store: Option<Arc<StdMutex<Box<dyn SessionStore + Send>>>>,
    ) -> Result<(Self, EventStreams), AgentError> {
        config.limits.validate()?;
        if config.policy.is_container() {
            config.policy.validate()?;
        }
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
        for registration in tools {
            let (tool, access) = registration.into_parts();
            let spec = tool.describe();
            let id = spec.id().clone();
            if !config
                .policy
                .accepts_tool_revision(id.name(), id.revision())
            {
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
                    access,
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
        diagnostic(format_args!(
            "nexus-runtime: context budget configured_items={} effective_items={} max_bytes={}",
            config.limits.retained_context_items,
            context_items_bound,
            nexus_core::provider::MAX_CONVERSATION_BYTES,
        ));
        let (data_tx, data_rx) = mpsc::channel(config.limits.event_data_capacity);
        let (control_tx, control_rx) = mpsc::channel(config.limits.event_control_capacity);
        let shared = Arc::new(Shared {
            local_reasoning: AtomicBool::new(false),
            diagnostic_writer: StdMutex::new(None),
            instructions,
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
            session_store,
            store_uncertain: AtomicBool::new(false),
            state: Mutex::new(State {
                active: None,
                restore_busy: false,
                last: None,
                history: VecDeque::new(),
                history_omissions: Vec::new(),
                history_omission_fallback_items: 0,
                history_safety_exhausted: false,
                durable_sessions: HashSet::new(),
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
        tools: Vec<ToolRegistration>,
    ) -> (Self, EventStreams) {
        Self::try_new(config, provider, tools)
            .expect("test-only runtime wiring must be valid; use Runtime::try_new in production")
    }

    /// Opt in to sensitive reasoning events on this runtime's local data stream.
    /// Call only when that stream is owned by a trusted local presentation.
    pub fn enable_local_reasoning(&self) {
        self.shared.local_reasoning.store(true, Ordering::SeqCst);
    }

    /// Handles one typed frontend command, including explicit bounded history
    /// listing and restoration when a store has been injected.
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
            Command::ListSessions(command) => (self.list_sessions(command).await, None),
            Command::RestoreSession(command) => (self.restore_session(command).await, None),
        }
    }

    async fn list_sessions(&self, command: ListSessionsCommand) -> CommandResponse {
        if command.validate().is_err() || self.shared.store_uncertain.load(Ordering::SeqCst) {
            return CommandResponse::new(command.request, CommandReply::Rejected, None);
        }
        let Some(store) = self.shared.session_store.clone() else {
            return CommandResponse::new(command.request, CommandReply::Rejected, None);
        };
        let limit = command.limit;
        let result = tokio::task::spawn_blocking(move || {
            let store = store.lock().map_err(|_| {
                runtime_error(
                    ErrorCategory::StorageFailure,
                    "session store is unavailable",
                )
            })?;
            store.list_sessions(limit)
        })
        .await;
        match result {
            Ok(Ok(sessions)) if sessions.len() <= limit => {
                CommandResponse::new(command.request, CommandReply::Accepted, None)
                    .with_sessions(sessions)
            }
            _ => CommandResponse::new(command.request, CommandReply::Rejected, None),
        }
    }

    async fn restore_session(&self, command: RestoreSessionCommand) -> CommandResponse {
        let runtime = self.clone();
        let request = command.request.clone();
        tokio::spawn(async move { runtime.restore_session_inner(command).await })
            .await
            .unwrap_or_else(|_| CommandResponse::new(request, CommandReply::Rejected, None))
    }

    async fn restore_session_inner(&self, command: RestoreSessionCommand) -> CommandResponse {
        let Some(store) = self.shared.session_store.clone() else {
            return CommandResponse::new(command.request, CommandReply::Rejected, None);
        };
        if self.shared.store_uncertain.load(Ordering::SeqCst) {
            return CommandResponse::new(command.request, CommandReply::Rejected, None);
        }
        {
            let mut state = self.shared.state.lock().await;
            if state.active.is_some()
                || state.restore_busy
                || !state.quarantine.is_empty()
                || self.shared.outbox_events.load(Ordering::SeqCst) != 0
            {
                return CommandResponse::new(command.request, CommandReply::Busy, None);
            }
            state.restore_busy = true;
        }

        let session = command.session.clone();
        let recovered = tokio::task::spawn_blocking(move || {
            let store = store.lock().map_err(|_| {
                runtime_error(
                    ErrorCategory::StorageFailure,
                    "session store is unavailable",
                )
            })?;
            recover_session(store.as_ref(), &session)
        })
        .await;
        let recovered = match recovered {
            Ok(Ok(recovered)) => recovered,
            _ => {
                self.shared.state.lock().await.restore_busy = false;
                return CommandResponse::new(command.request, CommandReply::Rejected, None);
            }
        };
        let items = match restore_items(recovered.items().cloned().collect()) {
            Ok(items) => items,
            Err(()) => {
                self.shared.state.lock().await.restore_busy = false;
                return CommandResponse::new(command.request, CommandReply::Rejected, None);
            }
        };
        let Some((items, omitted_items)) =
            bounded_restored_history(items, self.shared.context_items_bound)
        else {
            self.shared.state.lock().await.restore_busy = false;
            return CommandResponse::new(command.request, CommandReply::Rejected, None);
        };
        let mut state = self.shared.state.lock().await;
        if state.active.is_some() {
            state.restore_busy = false;
            return CommandResponse::new(command.request, CommandReply::Busy, None);
        }
        let discarded: Vec<_> = state
            .history
            .iter()
            .filter(|exchange| exchange.session != command.session)
            .map(|exchange| (exchange.session.clone(), exchange.items.len()))
            .collect();
        state.history.clear();
        state
            .history_omissions
            .retain(|omission| omission.session != command.session);
        for (session, removed) in discarded {
            mark_history_omitted(&mut state, session, removed);
        }
        for exchange in split_history_exchanges(&items) {
            state.history.push_back(RetainedExchange {
                session: command.session.clone(),
                items: exchange,
                derived_memory: None,
            });
        }
        if omitted_items > 0 {
            mark_history_omitted(&mut state, command.session.clone(), omitted_items);
        }
        state.durable_sessions.insert(command.session.clone());
        state.restore_busy = false;
        CommandResponse::new(command.request, CommandReply::Accepted, None)
            .with_restored_session(command.session)
            .with_restored_history(items, omitted_items)
    }

    /// Accepts a new run or rejects explicitly. A second submit while a run
    /// is active, a worker termination is unconfirmed, or a committed
    /// control outbox is still draining replies `Busy` without disturbing
    /// the retained ownership.
    pub async fn submit(&self, command: SubmitCommand) -> CommandResponse {
        if command.validate().is_err() {
            return CommandResponse::new(command.request.clone(), CommandReply::Rejected, None);
        }
        if self.shared.store_uncertain.load(Ordering::SeqCst) {
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
        if state.restore_busy {
            return CommandResponse::new(command.request.clone(), CommandReply::Busy, None);
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
        // The cache is already bounded. Include it whole, then use the same
        // transactional projection as every later model turn: admission must
        // not bypass uncertain-effect protection or notice accounting.
        for exchange in state
            .history
            .iter()
            .filter(|exchange| exchange.session == command.session)
        {
            conversation.extend(exchange.items.iter().cloned());
        }
        let session_omitted_items = state
            .history_omissions
            .iter()
            .filter(|omission| omission.session == command.session)
            .map(|omission| omission.removed_items)
            .sum::<usize>();
        let history_omitted_items =
            session_omitted_items.saturating_add(state.history_omission_fallback_items);
        let history_omitted = history_omitted_items > 0;
        let history_len = conversation.len();
        conversation.push(user_item);
        let source_cursor = conversation.len();
        let already_initialized = state.durable_sessions.contains(&command.session);
        let source_input = conversation
            .last()
            .expect("the user item was appended")
            .clone();
        let context_note = if history_omitted {
            CONTEXT_NOTICE_TEXT.to_owned()
        } else {
            String::new()
        };
        let context_note_bytes =
            runtime_note_bytes(&context_note, self.shared.instructions.payload_bytes() > 0);
        let initial_context_reduction = history_omitted.then(|| ContextReduction {
            method: nexus_core::ContextReductionMethod::InitialHistoryOmission,
            removed_items: history_omitted_items,
            retained_items: conversation.len(),
            retained_bytes: conversation_bytes(&conversation)
                .saturating_add(self.shared.instructions.payload_bytes())
                .saturating_add(context_note_bytes),
        });
        let derived_memory = state
            .history
            .iter()
            .rev()
            .filter(|exchange| exchange.session == command.session)
            .find_map(|exchange| exchange.derived_memory.clone());
        let (context_management, discovery) = self.shared.provider.context_management_for_run();
        state.active = Some(ActiveRun {
            context_management,
            derived_memory,
            pending_compaction: None,
            input_estimate: 0,
            token_estimator: TokenEstimator::default(),
            summary_token_estimator: TokenEstimator::default(),
            instruction_bytes: self.shared.instructions.payload_bytes(),
            context_note,
            context_note_bytes,
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
            output_limit_recovery_used: false,
            task_output_limit_recovery_attempts: 0,
            response_recovery_note: None,
            access_failure_attempts: 0,
            continuation: None,
            conversation,
            source_cursor,
            history_len,
            inherited_history: history_len > 0,
            initial_context_reduction,
            pending_omission_items: history_omitted_items,
            queue: VecDeque::new(),
            current: None,
            dispatched_calls: HashSet::new(),
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
            persistence_error: None,
            delivery_closed: false,
            finished_sent: false,
        });
        drop(state);
        // A caller abandoning its reply must not abandon the reserved slot or
        // detach the storage worker. Admission stays owned by the runtime.
        let admitting = self.clone();
        let request = command.request.clone();
        tokio::spawn(async move {
            admitting
                .complete_admission(command, run, source_input, already_initialized, discovery)
                .await
        })
        .await
        .unwrap_or_else(|_| CommandResponse::new(request, CommandReply::Rejected, None))
    }

    async fn complete_admission(
        &self,
        command: SubmitCommand,
        run: RunId,
        source_input: ModelContextItem,
        already_initialized: bool,
        discovery: Option<nexus_core::ContextWindowDiscovery>,
    ) -> CommandResponse {
        // Reserve the active slot before I/O, but never hold the state lock
        // while waiting for the blocking store. Concurrent commands stay live.
        if let Err(_error) = self
            .persist_session_input(
                &command.session,
                &command.profile,
                &run,
                &source_input,
                already_initialized,
            )
            .await
        {
            let mut state = self.shared.state.lock().await;
            if state
                .active
                .as_ref()
                .is_some_and(|active| active.run == run)
            {
                state.active = None;
            }
            return CommandResponse::new(command.request.clone(), CommandReply::Rejected, None);
        }
        if self.shared.session_store.is_some() {
            self.shared
                .state
                .lock()
                .await
                .durable_sessions
                .insert(command.session.clone());
        }
        let driver = self.clone();
        let run_for_task = run.clone();
        // The supervisor owns the driver's join handle so a panic or an
        // abrupt return still finalizes the slot exactly once.
        tokio::spawn(async move {
            let inner = {
                let driver = driver.clone();
                let run = run_for_task.clone();
                tokio::spawn(async move { driver.drive(run, discovery).await })
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

    /// Reports whether the runtime has active or unconfirmed worker
    /// ownership. A finalized snapshot alone is insufficient for safe
    /// session reclamation because timed-out workers may remain quarantined.
    pub async fn has_unconfirmed_work(&self) -> bool {
        let state = self.shared.state.lock().await;
        state.active.is_some() || !state.quarantine.is_empty()
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

    async fn drive(&self, run: RunId, discovery: Option<nexus_core::ContextWindowDiscovery>) {
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
        // The operation owns its admission snapshot. Retain the run until it
        // returns so cancellation cannot release a slot with work still active.
        if let Some(discovery) = discovery {
            let started = StdInstant::now();
            let window = match tokio::task::spawn_blocking(discovery).await {
                Ok(settings) => settings,
                Err(_) => {
                    let terminal = Terminal::failed_internal("context discovery worker failed");
                    self.finish_run(&run, terminal.outcome, terminal.error)
                        .await;
                    return;
                }
            };
            self.shared.diagnostic(format_args!(
                "nexus-runtime: context discovery outcome={} elapsed_ms={} window_tokens={:?}",
                if window.is_some() {
                    "discovered"
                } else {
                    "unknown"
                },
                started.elapsed().as_millis(),
                window.map(|window| window.tokens()),
            ));
            let mut state = self.shared.state.lock().await;
            let Some(active) = state.active.as_mut().filter(|active| active.run == run) else {
                return;
            };
            if active.context_management.window.is_none() {
                active.context_management.window = window;
            }
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
        if state.history_safety_exhausted {
            return Err(Terminal::limit(
                "uncertain history retention budget exhausted",
            ));
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
        let (mut request, context, mut turn, deadline, mut started_payload) = {
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
            if self
                .shared
                .limits
                .check_model_turns(active.turns_used)
                .is_err()
            {
                return Err(Terminal::limit("model turn budget exhausted"));
            }
            let compacted = compact_context(active, self.shared.context_items_bound)?;
            let initial = active.initial_context_reduction.take();
            let combined_reduction = compacted.is_some() && initial.is_some();
            let mut reduction = match (compacted, initial) {
                (Some(mut compacted), Some(initial)) => {
                    compacted.removed_items = compacted
                        .removed_items
                        .saturating_add(initial.removed_items);
                    Some(compacted)
                }
                (Some(compacted), None) | (None, Some(compacted)) => Some(compacted),
                (None, None) => None,
            };
            if let Some(reduction) = reduction.as_mut() {
                if combined_reduction {
                    reduction.method = nexus_core::ContextReductionMethod::Combined;
                }
                reduction.retained_items = active.conversation.len();
                reduction.retained_items += usize::from(active.derived_memory.is_some());
                reduction.retained_bytes = conversation_bytes(&active.conversation)
                    .saturating_add(active.instruction_bytes)
                    .saturating_add(active.context_note_bytes)
                    .saturating_add(context_compaction::projection_overhead(
                        active,
                        &active.context_note,
                    ));
            }
            if let Some(reduction) = reduction {
                publish_context_reduction(&self.shared, active, run, reduction);
            }
            let turn_n = active.turn_seq;
            active.turn_seq += 1;
            let turn = TurnId::new(format!(
                "t{:x}-{}-{turn_n}",
                self.shared.incarnation, active.run_n
            ))
            .map_err(|_| Terminal::failed_internal("turn identity is invalid"))?;
            let request = prepare_context_request(&self.shared, active, &turn)?;
            let purpose = if active.pending_compaction.is_some() {
                nexus_core::ModelRequestPurpose::Summary
            } else {
                nexus_core::ModelRequestPurpose::Task
            };
            let source_results: HashMap<CallId, &str> = active
                .conversation
                .iter()
                .filter_map(|item| match item {
                    ModelContextItem::ToolResult { call, outcome, .. } => {
                        Some((call.clone(), outcome.content()))
                    }
                    _ => None,
                })
                .collect();
            let projected_tool_outputs = request
                .conversation()
                .iter()
                .filter(|item| match item {
                    ModelContextItem::ToolResult { call, outcome, .. } => {
                        source_results.get(call).is_some_and(|source| {
                            *source != outcome.content()
                                && outcome.content().contains("Nexus model-context projection")
                        })
                    }
                    _ => false,
                })
                .count();
            let window = active
                .context_management
                .window
                .map(|window| window.tokens());
            let max_output_tokens = request.max_output_tokens();
            let estimator = if active.pending_compaction.is_some() {
                &active.summary_token_estimator
            } else {
                &active.token_estimator
            };
            let raw_estimated_input_tokens = Some(estimator.raw_estimate(&request));
            let estimated_input_tokens = Some(estimator.estimate(&request));
            let estimate_scale_millis = estimator.scale_millis();
            active.last_usage = None;
            active.published_usage = None;
            let started_payload = EventPayload::ModelRequestStarted {
                turn: turn.clone(),
                purpose,
                estimated_input_tokens,
                raw_estimated_input_tokens,
                estimate_scale_millis,
                context_window_tokens: window,
                context_trigger_percent: active.context_management.policy.trigger_percent(),
                context_target_percent: active.context_management.policy.target_percent(),
                compaction_strategy: active.context_management.policy.strategy(),
                max_output_tokens,
                projected_tool_outputs,
            };
            let outcome = emit_control(&self.shared, active, run, started_payload.clone());
            apply_control_outcome(active, outcome);
            if let Some(outcome) = active.terminal {
                return Err(Terminal {
                    outcome,
                    error: active.terminal_error.clone(),
                });
            }
            let context = ProviderContext::new(active.started.elapsed(), active.cancelled, None)
                .with_control(active.token.clone(), active.deadline);
            (request, context, turn, active.deadline, started_payload)
        };
        let (wake, token) = {
            let state = self.shared.state.lock().await;
            match state.active.as_ref().filter(|active| &active.run == run) {
                Some(active) => (active.wake.clone(), active.token.clone()),
                None => return Err(Terminal::cancelled()),
            }
        };
        loop {
            if token.is_cancelled() {
                return Err(Terminal::cancelled());
            }
            if StdInstant::now() >= deadline {
                return Err(Terminal::limit(
                    "run duration exhausted while awaiting provider",
                ));
            }
            let provider = self.shared.provider.clone();
            let incremental = provider.supports_incremental_streaming();
            let (stream_tx, mut stream_rx) =
                mpsc::channel::<ProviderEvent>(PROVISIONAL_STREAM_CAPACITY);
            let stream_overflow = Arc::new(AtomicBool::new(false));
            let producer_overflow = stream_overflow.clone();
            let producer_token = token.clone();
            let stream_bytes = Arc::new(AtomicUsize::new(0));
            let producer_bytes = stream_bytes.clone();
            let worker_context = context.clone();
            let worker_request = request.clone();
            let mut handle = tokio::task::spawn_blocking(move || {
                if incremental {
                    let sink = |event: ProviderEvent| {
                        if !matches!(
                            &event,
                            ProviderEvent::TextDelta { .. }
                                | ProviderEvent::ReasoningDelta { .. }
                                | ProviderEvent::ToolCallDelta { .. }
                        ) && !matches!(&event, ProviderEvent::Usage(usage) if usage.finality() == UsageFinality::Provisional)
                        {
                            return;
                        }
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
                    provider.stream_with_sink(&worker_request, &worker_context, &sink)
                } else {
                    provider.stream(&worker_request, &worker_context)
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
            return match joined {
                Some(Ok(events)) => {
                    let mut effective_limits = self.shared.limits;
                    effective_limits.max_tool_output_bytes = self.shared.effective_output_budget;
                    let response_validated =
                        crate::protocol::validate_batch(&events, &effective_limits).is_ok();
                    self.publish_model_request_finished(run, &turn, response_validated)
                        .await;
                    if stream_overflow.load(Ordering::SeqCst) || prefix.exceeded {
                        self.check_completed_stream_limits(
                            run,
                            &events,
                            &mut prefix,
                            stream_overflow.load(Ordering::SeqCst),
                        )
                        .await?;
                    }
                    // The join can become ready after the deadline while the
                    // deadline branch is also ready. Because the join branch is
                    // biased, do not accept the provider batch without checking
                    // the invocation's live control state again.
                    if let Err(error) = context.check_active() {
                        let mut state = self.shared.state.lock().await;
                        if let Some(active) =
                            state.active.as_mut().filter(|active| &active.run == run)
                            && active.pending_compaction.is_some()
                        {
                            context_compaction::record_summary_usage(
                                &self.shared,
                                active,
                                run,
                                &events,
                                &mut prefix,
                            );
                            if let Some(outcome) = active.terminal {
                                return Err(Terminal {
                                    outcome,
                                    error: active.terminal_error.clone(),
                                });
                            }
                            if stream_overflow.load(Ordering::SeqCst) || prefix.exceeded {
                                return Err(Terminal::limit(
                                    "summary provider stream exceeded its bound",
                                ));
                            }
                        }
                        return Err(match error.category() {
                            ErrorCategory::Cancelled => Terminal::cancelled_with(error),
                            ErrorCategory::Timeout => {
                                Terminal::limit("run duration exhausted while awaiting provider")
                            }
                            _ => Terminal::failed(error),
                        });
                    }
                    // The join may win the race while provisional events are
                    // still buffered: drain them first so every streamed prefix
                    // publishes before the authoritative batch ingests.
                    self.drain_stream(run, &turn, &mut stream_rx, &stream_bytes, &mut prefix)
                        .await;
                    self.check_completed_stream_limits(
                        run,
                        &events,
                        &mut prefix,
                        stream_overflow.load(Ordering::SeqCst),
                    )
                    .await?;
                    // Only joined access failures may replay this prepared
                    // request. Task output must be absent; summary text is
                    // private staging and can be discarded without replaying
                    // client-visible text or admitting any proposed tool call.
                    if response_validated
                        && let Some(ProviderEvent::Failed(error)) = events.last()
                        && provider_access_failure(error)
                    {
                        let summary = {
                            let mut state = self.shared.state.lock().await;
                            let active = state
                                .active
                                .as_mut()
                                .filter(|active| &active.run == run)
                                .ok_or_else(Terminal::cancelled)?;
                            if active.pending_compaction.is_some() {
                                context_compaction::record_summary_usage(
                                    &self.shared,
                                    active,
                                    run,
                                    &events,
                                    &mut prefix,
                                );
                                if let Some(outcome) = active.terminal {
                                    return Err(Terminal {
                                        outcome,
                                        error: active.terminal_error.clone(),
                                    });
                                }
                                true
                            } else {
                                false
                            }
                        };
                        if summary || (events.len() == 1 && prefix.event_count == 0) {
                            self.wait_provider_recovery(run, error.category()).await?;
                            let mut state = self.shared.state.lock().await;
                            let active = state
                                .active
                                .as_mut()
                                .filter(|active| &active.run == run)
                                .ok_or_else(Terminal::cancelled)?;
                            if active.cancelled || token.is_cancelled() {
                                return Err(Terminal::cancelled());
                            }
                            if StdInstant::now() >= deadline {
                                return Err(Terminal::limit(
                                    "run duration exhausted while recovering provider access",
                                ));
                            }
                            let turn_n = active.turn_seq;
                            active.turn_seq += 1;
                            turn = TurnId::new(format!(
                                "t{:x}-{}-{turn_n}",
                                self.shared.incarnation, active.run_n
                            ))
                            .map_err(|_| Terminal::failed_internal("turn identity is invalid"))?;
                            request = request.with_turn(turn.clone());
                            // Usage belongs to an invocation, not its replacement.
                            active.last_usage = None;
                            active.published_usage = None;
                            if let EventPayload::ModelRequestStarted { turn: identity, .. } =
                                &mut started_payload
                            {
                                *identity = turn.clone();
                            }
                            let outcome =
                                emit_control(&self.shared, active, run, started_payload.clone());
                            apply_control_outcome(active, outcome);
                            continue;
                        }
                    }
                    let result = self
                        .ingest_model_batch(run, &turn, events, &mut prefix)
                        .await;
                    if let Err(error) = self.persist_pending_source(run).await {
                        let mut state = self.shared.state.lock().await;
                        if let Some(active) =
                            state.active.as_mut().filter(|active| &active.run == run)
                        {
                            active.persistence_error = Some(error.clone());
                        }
                        return Err(Terminal::failed(error));
                    }
                    if matches!(&result, Ok(RunState::ValidatingTools)) {
                        let reduction = {
                            let mut state = self.shared.state.lock().await;
                            let Some(active) =
                                state.active.as_mut().filter(|active| &active.run == run)
                            else {
                                return Err(Terminal::cancelled());
                            };
                            compact_context(active, self.shared.context_items_bound)?
                        };
                        if let Some(reduction) = reduction {
                            let mut state = self.shared.state.lock().await;
                            if let Some(active) =
                                state.active.as_mut().filter(|active| &active.run == run)
                            {
                                publish_context_reduction(&self.shared, active, run, reduction);
                            }
                        }
                    }
                    result
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
            };
        }
    }

    async fn wait_provider_recovery(
        &self,
        run: &RunId,
        category: ErrorCategory,
    ) -> Result<(), Terminal> {
        let (token, wake, deadline, context, attempt) = {
            let mut state = self.shared.state.lock().await;
            let active = state
                .active
                .as_mut()
                .filter(|active| &active.run == run)
                .ok_or_else(Terminal::cancelled)?;
            if active.cancelled {
                return Err(Terminal::cancelled());
            }
            let context = ProviderContext::new(active.started.elapsed(), false, None)
                .with_control(active.token.clone(), active.deadline);
            let attempt = active.access_failure_attempts;
            active.access_failure_attempts = attempt.saturating_add(1);
            (
                active.token.clone(),
                active.wake.clone(),
                active.deadline,
                context,
                attempt,
            )
        };
        let check = || {
            context
                .check_active()
                .map_err(|error| match error.category() {
                    ErrorCategory::Cancelled => Terminal::cancelled_with(error),
                    ErrorCategory::Timeout => {
                        Terminal::limit("run duration exhausted while recovering provider access")
                    }
                    _ => Terminal::failed(error),
                })
        };
        check()?;
        let delay = provider_recovery_delay(attempt);
        self.shared.diagnostic(format_args!(
            "nexus-runtime: provider access recovery run={} attempt={} delay_seconds={} error_category={category:?}",
            run.as_str(), attempt.saturating_add(1), delay.as_secs(),
        ));
        let retry_at = tokio::time::Instant::now() + delay;
        loop {
            if token.is_cancelled() {
                return Err(Terminal::cancelled());
            }
            tokio::select! {
                biased;
                () = wake.notified() => { check()?; }
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    return Err(Terminal::limit("run duration exhausted while recovering provider access"));
                }
                () = tokio::time::sleep_until(retry_at) => break,
            }
        }
        check()
    }

    async fn publish_model_request_finished(
        &self,
        run: &RunId,
        turn: &TurnId,
        response_validated: bool,
    ) {
        let mut state = self.shared.state.lock().await;
        if let Some(active) = state.active.as_mut().filter(|active| &active.run == run) {
            let outcome = emit_control(
                &self.shared,
                active,
                run,
                EventPayload::ModelRequestFinished {
                    turn: turn.clone(),
                    response_validated,
                },
            );
            apply_control_outcome(active, outcome);
        }
    }

    /// Account an available summary batch even when draining its provisional
    /// prefixes exhausted a stream bound. Limit handling never commits memory.
    async fn check_completed_stream_limits(
        &self,
        run: &RunId,
        events: &[ProviderEvent],
        prefix: &mut StreamedPrefix,
        overflow: bool,
    ) -> Result<(), Terminal> {
        let message = if overflow {
            "provisional provider stream exceeded its bound"
        } else if prefix.exceeded {
            "provisional output budget exhausted"
        } else {
            return Ok(());
        };
        let mut state = self.shared.state.lock().await;
        if let Some(active) = state.active.as_mut().filter(|active| &active.run == run)
            && active.pending_compaction.is_some()
        {
            context_compaction::record_summary_usage(&self.shared, active, run, events, prefix);
            if let Some(outcome) = active.terminal {
                return Err(Terminal {
                    outcome,
                    error: active.terminal_error.clone(),
                });
            }
        }
        Err(Terminal::limit(message))
    }

    /// Publishes one provisional provider event while its turn is still in
    /// flight. Text, call previews and usage estimates publish immediately;
    /// sensitive reasoning publishes only with explicit local opt-in.
    /// Candidates and terminals are
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
            ProviderEvent::ReasoningDelta { text } => {
                if !record_provisional(prefix, text.len(), self.shared.effective_output_budget) {
                    active.data_dropped = true;
                    prefix.exceeded = true;
                    active.token.cancel();
                    return;
                }
                prefix.reasoning.push_str(&text);
                if active.pending_compaction.is_none() {
                    emit_reasoning(&self.shared, active, run, turn, &text);
                }
            }
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
                if active.pending_compaction.is_none() {
                    buffer_text_fragments(&self.shared, active, run, turn, &item_key, &text);
                    flush_text(&self.shared, active);
                }
            }
            ProviderEvent::ToolCallDelta { item_key, .. } => {
                if !record_identity(prefix, item_key.len()) {
                    active.data_dropped = true;
                    prefix.exceeded = true;
                    active.token.cancel();
                    return;
                }
                prefix.preview_keys.insert(item_key.clone());
                if active.pending_compaction.is_none() {
                    emit_data(
                        &self.shared,
                        active,
                        run,
                        EventPayload::ToolCallPreview { item_key },
                    );
                }
            }
            ProviderEvent::Usage(usage) => {
                if usage.finality() == UsageFinality::Provisional {
                    prefix.usage_prefix.push(usage);
                }
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
        {
            let mut state = self.shared.state.lock().await;
            if let Some(active) = state.active.as_mut().filter(|active| &active.run == run)
                && active.pending_compaction.is_some()
            {
                if matches!(events.last(), Some(ProviderEvent::TurnFinished(finished)) if finished.reason() == FinishReason::OutputLimit)
                {
                    self.shared.diagnostic(format_args!(
                        "nexus-runtime: summary output limit run={} turn={} event_count={}",
                        run.as_str(),
                        turn.as_str(),
                        events.len(),
                    ));
                }
                return context_compaction::ingest_summary(
                    &self.shared,
                    active,
                    run,
                    events,
                    prefix,
                );
            }
        }
        // Full-turn atomic validation: duplicate references, partial/final
        // disagreement, and finish-reason conflicts fail before any
        // admission, identity consumption, or queue mutation. A malformed
        // failure remains terminal even if its primary provider cause is an
        // otherwise recoverable access category.
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
        if StdInstant::now() >= active.deadline {
            return Err(Terminal::limit(
                "run duration exhausted before accepting provider response",
            ));
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
        context_compaction::calibrate_estimate(active, &events);
        let mut streamed_index = 0;
        let mut usage_prefix_matches = true;
        let mut unmatched_usage = prefix.usage_prefix.clone();
        for usage in events.iter().filter_map(|event| match event {
            ProviderEvent::Usage(usage) if usage.finality() == UsageFinality::Provisional => {
                Some(*usage)
            }
            _ => None,
        }) {
            if successful_turn {
                if streamed_index < prefix.usage_prefix.len()
                    && prefix.usage_prefix[streamed_index] != usage
                {
                    usage_prefix_matches = false;
                    break;
                }
                if streamed_index < prefix.usage_prefix.len() {
                    prefix.usage_replayed.push(usage);
                    streamed_index += 1;
                }
            } else if let Some(index) = unmatched_usage.iter().position(|item| *item == usage) {
                unmatched_usage.remove(index);
                prefix.usage_replayed.push(usage);
            }
        }
        if successful_turn && !usage_prefix_matches {
            return Err(Terminal::failed(runtime_error(
                ErrorCategory::Protocol,
                "authoritative provider usage does not match its streamed prefix",
            )));
        }
        if successful_turn {
            let authoritative_reasoning: String = events
                .iter()
                .filter_map(|event| match event {
                    ProviderEvent::ReasoningDelta { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            if !authoritative_reasoning.starts_with(&prefix.reasoning) {
                return Err(Terminal::failed(runtime_error(
                    ErrorCategory::Protocol,
                    "authoritative provider reasoning does not match its streamed prefix",
                )));
            }
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
        // Keep the authoritative trace for replay, while publishing only the
        // suffix not already delivered provisionally.
        let mut reasoning = String::new();
        let mut reasoning_skip = if successful_turn || prefix.reasoning.is_empty() {
            prefix.reasoning.len()
        } else {
            usize::MAX
        };
        for event in &events {
            if active.terminal.is_some() {
                break;
            }
            match event {
                ProviderEvent::ReasoningDelta { text } => {
                    reasoning.push_str(text);
                    let mut skip = reasoning_skip.min(text.len());
                    while !text.is_char_boundary(skip) {
                        skip -= 1;
                    }
                    reasoning_skip = reasoning_skip.saturating_sub(skip);
                    if skip < text.len() {
                        emit_reasoning(&self.shared, active, run, turn, &text[skip..]);
                    }
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
                    if usage.finality() == UsageFinality::Provisional
                        && let Some(index) =
                            prefix.usage_replayed.iter().position(|item| item == usage)
                    {
                        prefix.usage_replayed.remove(index);
                        continue;
                    }
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
        if StdInstant::now() >= active.deadline {
            return Err(Terminal::limit(
                "run duration exhausted before accepting provider response",
            ));
        }
        flush_text(&self.shared, active);
        let Some(terminal) = terminal else {
            return Err(Terminal::failed(runtime_error(
                ErrorCategory::Protocol,
                "provider batch ended without a terminal event",
            )));
        };
        let discarded = match &terminal {
            Err(_) => true,
            Ok(finished) => matches!(
                finished.reason(),
                FinishReason::Incomplete | FinishReason::OutputLimit
            ),
        };
        if discarded {
            // Both the drained streamed prefix and any authoritative batch
            // suffix must precede this marker, even for non-streaming providers.
            let outcome = emit_control(
                &self.shared,
                active,
                run,
                EventPayload::ModelResponseDiscarded { turn: turn.clone() },
            );
            apply_control_outcome(active, outcome);
            if active.terminal.is_some() {
                return Ok(RunState::Finished);
            }
        }
        match terminal {
            Err(error) => {
                if error.category() == ErrorCategory::Cancelled {
                    Err(Terminal::cancelled_with(error))
                } else if provider_access_failure(&error) && active.continuation.is_none() {
                    // Output may already be visible, so this is a fresh model
                    // turn, not a replay. No failed candidate or partial text
                    // enters accepted context; the host notice is bounded.
                    active.response_recovery_note = Some(
                        "The previous model invocation failed before a complete response was accepted. Its partial response and proposed tool calls were discarded; no proposed action executed. Choose the next small step from the accepted conversation, without reconstructing incomplete tool calls.",
                    );
                    drop(state);
                    self.wait_provider_recovery(run, error.category()).await?;
                    Ok(RunState::CallingModel)
                } else {
                    Err(Terminal::failed(error))
                }
            }
            Ok(finished) => {
                active.turns_used += 1;
                let continuation_compatible = finished.continuation().is_none_or(|continuation| {
                    let adapter = self.shared.provider.adapter_identity();
                    let scope = self.shared.provider.continuation_scope(&active.profile);
                    continuation.is_compatible_with(adapter, &scope)
                });
                if finished.reason() == FinishReason::OutputLimit {
                    let recovery_attempted = active.task_output_limit_recovery_attempts < 1
                        && active.continuation.is_none()
                        && finished.continuation().is_none()
                        && continuation_compatible
                        && !active.cancelled
                        && !active.token.is_cancelled()
                        && StdInstant::now() < active.deadline
                        && self
                            .shared
                            .limits
                            .check_model_turns(active.turns_used)
                            .is_ok();
                    let mut argument_progress = HashMap::<&str, usize>::new();
                    for event in &events {
                        if let ProviderEvent::ToolCallDelta {
                            item_key,
                            assembled_bytes,
                        } = event
                        {
                            let bytes = argument_progress.entry(item_key).or_default();
                            *bytes = (*bytes).max(*assembled_bytes);
                        }
                    }
                    let argument_progress_bytes = argument_progress
                        .values()
                        .fold(0usize, |sum, bytes| sum.saturating_add(*bytes));
                    self.shared.diagnostic(format_args!(
                        "model output limit: run={} turn={} text_bytes={} reasoning_bytes={} candidate_count={} argument_progress_bytes={} recovery_attempted={}",
                        run.as_str(),
                        turn.as_str(),
                        text_items
                            .iter()
                            .fold(0usize, |sum, (_, text)| sum.saturating_add(text.len())),
                        reasoning.len(),
                        candidates.len(),
                        argument_progress_bytes,
                        recovery_attempted,
                    ));
                    if !continuation_compatible {
                        return Err(Terminal::failed(runtime_error(
                            ErrorCategory::UnsupportedCapability,
                            "provider continuation is incompatible with this profile",
                        )));
                    }
                    if active.cancelled || active.token.is_cancelled() {
                        return Err(Terminal::cancelled());
                    }
                    if StdInstant::now() >= active.deadline {
                        return Err(Terminal::limit("run duration exhausted"));
                    }
                    if recovery_attempted {
                        // Drop the entire response: no candidate identity is allocated,
                        // and no text, reasoning, arguments, or opaque state is replayed.
                        // Request projection owns the bounded host recovery instruction.
                        active.output_limit_recovery_used = true;
                        active.task_output_limit_recovery_attempts += 1;
                        return Ok(RunState::CallingModel);
                    }
                    return Err(Terminal::limit("model output limit reached"));
                }
                if !continuation_compatible {
                    return Err(Terminal::failed(runtime_error(
                        ErrorCategory::UnsupportedCapability,
                        "provider continuation is incompatible with this profile",
                    )));
                }
                if finished.reason() == FinishReason::Incomplete {
                    if active.continuation.is_some() || finished.continuation().is_some() {
                        return Err(Terminal::failed(runtime_error(
                            ErrorCategory::Protocol,
                            "incomplete response cannot safely resume opaque continuation",
                        )));
                    }
                    self.shared
                        .limits
                        .check_model_turns(active.turns_used)
                        .map_err(|_| Terminal::limit("model turn budget exhausted"))?;
                    active.response_recovery_note = Some(
                        "The previous model response ended incomplete and was discarded, including proposed tool calls. No proposed action executed. Choose the next small step from the accepted conversation; do not reconstruct or continue incomplete tool calls.",
                    );
                    return Ok(RunState::CallingModel);
                }
                active.access_failure_attempts = 0;
                active.response_recovery_note = None;
                if let Some(continuation) = finished.continuation().cloned() {
                    active.continuation = Some(continuation);
                }
                match finished.reason() {
                    FinishReason::Stop => {
                        active.task_output_limit_recovery_attempts = 0;
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
                    FinishReason::OutputLimit => {
                        unreachable!("output limit handled before continuation adoption")
                    }
                    FinishReason::Incomplete => {
                        unreachable!("incomplete response handled before continuation adoption")
                    }
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
                        active.task_output_limit_recovery_attempts = 0;
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
            let tool_access = registered.access;
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
                needs_approval: self.shared.policy.requires_approval(tool_access, &tool_id),
                access: tool_access,
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
        if self.shared.policy.validate().is_err() {
            // An invalid policy can neither authorize an automatic call nor
            // mint a scoped approval. Fail closed with the same recorded
            // denial the automatic path would produce, without dispatching.
            let call = queued.call.call().clone();
            active.outcome_slot =
                Some((call, denied_outcome("policy no longer authorizes dispatch")));
            active.current = Some(CurrentCall {
                call: queued.call,
                item_key: queued.item_key,
                provider_ref: queued.provider_ref,
                binding: None,
                dispatched: false,
            });
            return Ok(RunState::RecordingResult);
        }
        if self.shared.policy.is_development() || self.shared.policy.is_container() {
            let approval = self.shared.policy.call_requires_approval(
                &queued.call,
                self.shared
                    .session_directories
                    .lock()
                    .expect("session permissions lock")
                    .get(&active.session)
                    .map_or(&[], Vec::as_slice),
                queued.access,
            );
            queued.needs_approval = match approval {
                Ok(needs_approval) => needs_approval,
                Err(error) => {
                    active.outcome_slot = Some((
                        queued.call.call().clone(),
                        ToolOutcome::new(
                            ExecutionStatus::Denied,
                            EffectState::NotStarted,
                            Evidence::HostObserved,
                            error.message(),
                            false,
                        )
                        .expect("validated policy error builds a denial"),
                    ));
                    active.current = Some(CurrentCall {
                        call: queued.call,
                        item_key: queued.item_key,
                        provider_ref: queued.provider_ref,
                        binding: None,
                        dispatched: false,
                    });
                    return Ok(RunState::RecordingResult);
                }
            };
        }
        if active.read_only && (queued.needs_approval || queued.access != ToolAccess::Read) {
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
                dispatched: false,
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
                dispatched: false,
            });
            return Ok(RunState::RecordingResult);
        }
        if !queued.needs_approval {
            active.current = Some(CurrentCall {
                call: queued.call,
                item_key: queued.item_key,
                provider_ref: queued.provider_ref,
                binding: None,
                dispatched: false,
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
            dispatched: false,
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
                    registered.access,
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
            if (self.shared.policy.is_development() || self.shared.policy.is_container())
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
                scope.clone(),
            )
            .map_err(|_| Terminal::failed_internal("tool context is invalid"))?
            .with_control(active.token.clone(), tool_deadline);
            let intent = ToolIntentRecord {
                run: call.run().clone(),
                call: call.call().clone(),
                tool: call.tool().clone(),
                args: call.args().clone(),
                scope,
            };
            let tool = registered.port.clone();
            let wake = active.wake.clone();
            let token = active.token.clone();
            (
                active.session.clone(),
                call,
                tool,
                context,
                tool_deadline,
                active.deadline,
                wake,
                token,
                intent,
            )
        };
        let (session, call, tool, context, tool_deadline, run_deadline, wake, token, intent) =
            setup;
        let call_id = call.call().clone();
        if let Err(error) = self.persist_tool_intent(&session, intent.clone()).await {
            let mut state = self.shared.state.lock().await;
            if let Some(active) = state.active.as_mut()
                && &active.run == run
            {
                active.persistence_error = Some(error.clone());
                if active.terminal.is_none() {
                    active.terminal = Some(RunOutcome::Failed);
                    active.terminal_error = Some(error);
                }
                active.outcome_slot = Some((
                    call_id,
                    ToolOutcome::new(
                        ExecutionStatus::Failed,
                        EffectState::NotStarted,
                        Evidence::HostObserved,
                        "tool intent could not be persisted",
                        false,
                    )
                    .expect("bounded not-started persistence outcome builds"),
                ));
                return Ok(RunState::RecordingResult);
            }
            return Err(Terminal::cancelled());
        }

        // Persistence may block long enough for cancellation, deadline, or a
        // scoped approval to change. Revalidate all dispatch authority after
        // the durable intent and immediately before marking the call started.
        {
            let mut state = self.shared.state.lock().await;
            let worker_quarantined = !state.quarantine.is_empty();
            let Some(active) = state.active.as_mut() else {
                return Err(Terminal::cancelled());
            };
            if &active.run != run {
                return Err(Terminal::cancelled());
            }
            if active.cancelled || active.token.is_cancelled() {
                active.outcome_slot =
                    Some((call_id, cancelled_outcome("cancelled before dispatch")));
                active.force_terminal = Some(RunOutcome::Cancelled);
                return Ok(RunState::RecordingResult);
            }
            if StdInstant::now() >= active.deadline {
                active.outcome_slot = Some((
                    call_id,
                    timeout_outcome("run deadline passed before dispatch"),
                ));
                active.force_terminal = Some(RunOutcome::LimitReached);
                return Ok(RunState::RecordingResult);
            }
            if StdInstant::now() >= tool_deadline {
                active.outcome_slot = Some((
                    call_id,
                    ToolOutcome::new(
                        ExecutionStatus::TimedOut,
                        EffectState::NotStarted,
                        Evidence::HostObserved,
                        "tool deadline passed before dispatch",
                        false,
                    )
                    .expect("bounded not-started timeout outcome builds"),
                ));
                return Ok(RunState::RecordingResult);
            }
            if worker_quarantined {
                active.outcome_slot = Some((
                    call_id,
                    denied_outcome(
                        "tool dispatch unavailable while a previous worker remains quarantined; termination and effects are unconfirmed; do not retry the previous operation",
                    ),
                ));
                return Ok(RunState::RecordingResult);
            }
            let Some(current) = active.current.as_ref() else {
                return Ok(RunState::ValidatingTools);
            };
            let authorization = if let Some(binding) = current.binding.as_ref() {
                binding
                    .check_valid_for_dispatch(
                        call.run(),
                        call.call(),
                        call.tool(),
                        call.args(),
                        self.shared.policy.revision(),
                        active.started.elapsed(),
                    )
                    .ok()
                    .map(|()| binding.scope().clone())
            } else {
                let Some(registered) = self.shared.tools.get(call.tool().name()) else {
                    active.outcome_slot = Some((
                        call_id,
                        denied_outcome("tool registration changed before dispatch"),
                    ));
                    return Ok(RunState::RecordingResult);
                };
                self.shared
                    .policy
                    .authorize_with_directories(
                        &call,
                        self.shared
                            .session_directories
                            .lock()
                            .expect("session permissions lock")
                            .get(&active.session)
                            .map_or(&[], Vec::as_slice),
                        registered.access,
                    )
                    .ok()
            };
            if authorization.as_ref() != Some(&intent.scope) {
                active.outcome_slot = Some((
                    call_id,
                    denied_outcome("dispatch authority changed during persistence"),
                ));
                return Ok(RunState::RecordingResult);
            }
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
            if let Some(current) = active.current.as_mut() {
                current.dispatched = true;
            }
        }
        let mut handle = tokio::task::spawn_blocking(move || tool.execute(&call, &context));
        // The context retains the original deadline. A cooperative tool may
        // need bounded time to stop its work and report the actual effects.
        let cleanup_deadline = tool_deadline
            .checked_add(TOOL_CLEANUP_GRACE)
            .map_or(run_deadline, |deadline| deadline.min(run_deadline));
        let mut cancelled_first = false;
        let joined = loop {
            if token.is_cancelled() {
                cancelled_first = true;
                break None;
            }
            if StdInstant::now() >= run_deadline {
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
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(cleanup_deadline)) => break None,
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
                } else if StdInstant::now() >= run_deadline {
                    timeout_outcome("tool deadline exceeded; termination unconfirmed")
                } else {
                    timeout_outcome(
                        "tool deadline exceeded; termination and effects are unconfirmed; worker remains quarantined in the background; new tool dispatch is unavailable until termination; do not retry this operation",
                    )
                };
                active.outcome_slot = Some((call_id.clone(), outcome));
                if active.terminal.is_none() {
                    active.force_terminal = if cancelled {
                        Some(RunOutcome::Cancelled)
                    } else if StdInstant::now() >= run_deadline {
                        Some(RunOutcome::LimitReached)
                    } else {
                        None
                    };
                }
                drop(state);
                // Ownership is retained: the worker stays quarantined until
                // it actually terminates, and no next call or new run can
                // start meanwhile. The model may still receive the outcome
                // and explain the uncertainty without dispatching tools.
                self.quarantine_tool(run, call_id, handle).await;
                Ok(RunState::RecordingResult)
            }
        }
    }

    async fn on_recording(&self, run: &RunId) -> Result<RunState, Terminal> {
        let pending_record = {
            let state = self.shared.state.lock().await;
            state
                .active
                .as_ref()
                .filter(|active| &active.run == run)
                .and_then(|active| {
                    active
                        .outcome_slot
                        .as_ref()
                        .map(|(call, outcome)| {
                            let outcome = bound_outcome(&self.shared, outcome.clone());
                            ToolOutcomeRecord {
                                run: active.run.clone(),
                                call: call.clone(),
                                status: outcome.status(),
                                effect: outcome.effect(),
                                evidence: outcome.evidence(),
                                truncated: outcome.is_truncated(),
                            }
                        })
                        .map(|record| (active.session.clone(), record))
                })
        };
        let persistence_error = if let Some((session, record)) = pending_record {
            self.persist_tool_outcome(&session, record).await.err()
        } else {
            None
        };
        let mut state = self.shared.state.lock().await;
        let Some(active) = state.active.as_mut() else {
            return Err(Terminal::cancelled());
        };
        if &active.run != run {
            return Err(Terminal::cancelled());
        }
        if let Some(error) = persistence_error {
            active.persistence_error = Some(error.clone());
            if active.terminal.is_none() {
                active.terminal = Some(RunOutcome::Failed);
                active.terminal_error = Some(error);
            }
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
            if current.dispatched {
                active.dispatched_calls.insert(call.clone());
            }
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
        let forced_terminal = active.force_terminal.take();
        if let Some(outcome) = forced_terminal {
            active.terminal = Some(outcome);
        }
        drop(state);
        if let Err(error) = self.persist_pending_source(run).await {
            let mut state = self.shared.state.lock().await;
            if let Some(active) = state.active.as_mut().filter(|active| &active.run == run) {
                active.persistence_error = Some(error.clone());
                if active.terminal.is_none() {
                    active.terminal = Some(RunOutcome::Failed);
                    active.terminal_error = Some(error);
                }
            }
            return Ok(RunState::Finished);
        }
        let mut state = self.shared.state.lock().await;
        let Some(active) = state.active.as_mut().filter(|active| &active.run == run) else {
            return Err(Terminal::cancelled());
        };
        if active.terminal.is_some() {
            return Ok(RunState::Finished);
        }
        if let Some(reduction) = compact_context(active, self.shared.context_items_bound)? {
            publish_context_reduction(&self.shared, active, run, reduction);
        }
        active.source_cursor = active.conversation.len();
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

    async fn persist_tool_intent(
        &self,
        session: &SessionId,
        intent: ToolIntentRecord,
    ) -> Result<(), AgentError> {
        let Some(store) = self.shared.session_store.clone() else {
            return Ok(());
        };
        if self.shared.store_uncertain.load(Ordering::SeqCst) {
            return Err(runtime_error(
                ErrorCategory::StorageFailure,
                "session store requires reconciliation",
            ));
        }
        let run = intent.run.clone();
        let session = session.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let mut store = store.lock().map_err(|_| {
                runtime_error(
                    ErrorCategory::StorageFailure,
                    "session store is unavailable",
                )
            })?;
            store.record_session_intent(&session, &intent)
        });
        self.await_store(&run, worker).await
    }

    async fn persist_tool_outcome(
        &self,
        session: &SessionId,
        outcome: ToolOutcomeRecord,
    ) -> Result<(), AgentError> {
        let Some(store) = self.shared.session_store.clone() else {
            return Ok(());
        };
        if self.shared.store_uncertain.load(Ordering::SeqCst) {
            return Err(runtime_error(
                ErrorCategory::StorageFailure,
                "session store requires reconciliation",
            ));
        }
        let run = outcome.run.clone();
        let session = session.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let mut store = store.lock().map_err(|_| {
                runtime_error(
                    ErrorCategory::StorageFailure,
                    "session store is unavailable",
                )
            })?;
            store.record_session_outcome(&session, &outcome)
        });
        self.await_store(&run, worker).await
    }

    async fn persist_session_input(
        &self,
        session: &SessionId,
        profile: &str,
        run: &RunId,
        item: &ModelContextItem,
        already_initialized: bool,
    ) -> Result<(), AgentError> {
        let Some(store) = self.shared.session_store.clone() else {
            return Ok(());
        };
        if self.shared.store_uncertain.load(Ordering::SeqCst) {
            return Err(runtime_error(
                ErrorCategory::StorageFailure,
                "session store requires reconciliation",
            ));
        }
        let owner = run.clone();
        let session = session.clone();
        let profile = profile.to_owned();
        let run = run.clone();
        let item = item.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let mut store = store.lock().map_err(|_| {
                runtime_error(
                    ErrorCategory::StorageFailure,
                    "session store is unavailable",
                )
            })?;
            let checkpoint = SessionCheckpoint::new(
                session.clone(),
                STORE_FORMAT_REVISION,
                0,
                Vec::<nexus_core::store::StoredMessage>::new(),
                profile,
            )?;
            let created = store.initialize_session(&checkpoint)?;
            if !created && !already_initialized {
                let existing = store.load_session(&session)?;
                if !existing.messages().is_empty() || store.last_source_sequence(&session)? > 0 {
                    return Err(runtime_error(
                        ErrorCategory::StorageFailure,
                        "session history must be explicitly restored before submitting",
                    ));
                }
            }
            store.append_source_items(&session, &run, &[item])?;
            Ok(())
        });
        self.await_store(&owner, worker).await
    }

    /// Commits every newly accepted context item before the runtime advances
    /// to another phase. Source history is append-only even when the active
    /// model context is later compacted.
    async fn persist_pending_source(&self, run: &RunId) -> Result<(), AgentError> {
        let Some(store) = self.shared.session_store.clone() else {
            return Ok(());
        };
        if self.shared.store_uncertain.load(Ordering::SeqCst) {
            return Err(runtime_error(
                ErrorCategory::StorageFailure,
                "session store requires reconciliation",
            ));
        }
        let (session, items, cursor) = {
            let state = self.shared.state.lock().await;
            let Some(active) = state.active.as_ref().filter(|active| &active.run == run) else {
                return Err(runtime_error(
                    ErrorCategory::StorageFailure,
                    "active session disappeared before source history commit",
                ));
            };
            if active.source_cursor > active.conversation.len() {
                return Err(runtime_error(
                    ErrorCategory::StorageFailure,
                    "active source history cursor is invalid",
                ));
            }
            (
                active.session.clone(),
                active.conversation[active.source_cursor..].to_vec(),
                active.source_cursor,
            )
        };
        if items.is_empty() {
            return Ok(());
        }
        let store_session = session.clone();
        let store_run = run.clone();
        let store_items = items.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let mut store = store.lock().map_err(|_| {
                runtime_error(
                    ErrorCategory::StorageFailure,
                    "session store is unavailable",
                )
            })?;
            store.append_source_items(&store_session, &store_run, &store_items)
        });
        self.await_store(run, worker).await?;

        let mut state = self.shared.state.lock().await;
        let Some(active) = state.active.as_mut().filter(|active| &active.run == run) else {
            return Err(runtime_error(
                ErrorCategory::StorageFailure,
                "active session changed after source history commit",
            ));
        };
        if active.source_cursor != cursor {
            return Err(runtime_error(
                ErrorCategory::StorageFailure,
                "active source history cursor changed during commit",
            ));
        }
        active.source_cursor = cursor.saturating_add(items.len());
        Ok(())
    }

    /// Blocking writes cannot be preempted. Stop waiting at live cancellation
    /// or the run deadline, retain the worker, and refuse further store use
    /// until the host reopens/reconciles it.
    async fn await_store<T: Send + 'static>(
        &self,
        run: &RunId,
        mut worker: JoinHandle<Result<T, AgentError>>,
    ) -> Result<T, AgentError> {
        let control = {
            let state = self.shared.state.lock().await;
            state
                .active
                .as_ref()
                .filter(|active| &active.run == run)
                .map(|active| (active.token.clone(), active.wake.clone(), active.deadline))
        };
        let mut interrupted = RunOutcome::Cancelled;
        if let Some((token, wake, deadline)) = control {
            loop {
                if token.is_cancelled() {
                    break;
                }
                if StdInstant::now() >= deadline {
                    interrupted = RunOutcome::LimitReached;
                    break;
                }
                tokio::select! {
                    result = &mut worker => {
                        return match result {
                            Ok(result) => result,
                            Err(_) => {
                                self.shared.store_uncertain.store(true, Ordering::SeqCst);
                                Err(runtime_error(ErrorCategory::StorageFailure, "session store worker failed; reopen required"))
                            }
                        };
                    }
                    () = wake.notified() => {}
                    () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {}
                }
            }
        }
        let error = runtime_error(
            ErrorCategory::StorageFailure,
            "session store write interrupted; reopen required",
        );
        self.shared.store_uncertain.store(true, Ordering::SeqCst);
        {
            let mut state = self.shared.state.lock().await;
            state.quarantine.push(QuarantineMeta {
                run: run.clone(),
                call: None,
            });
            if let Some(active) = state.active.as_mut().filter(|active| &active.run == run) {
                active.persistence_error = Some(error.clone());
                if active.terminal.is_none() {
                    active.terminal = Some(interrupted);
                }
            }
        }
        let shared = self.shared.clone();
        let run = run.clone();
        tokio::spawn(async move {
            let _ = worker.await;
            shared
                .state
                .lock()
                .await
                .quarantine
                .retain(|meta| !(meta.run == run && meta.call.is_none()));
        });
        Err(error)
    }

    async fn set_terminal(&self, run: &RunId, outcome: RunOutcome, error: Option<AgentError>) {
        let mut state = self.shared.state.lock().await;
        let Some(active) = state.active.as_mut() else {
            return;
        };
        if &active.run != run {
            return;
        }
        if active.cancelled {
            active.terminal = Some(RunOutcome::Cancelled);
            active.terminal_error = error
                .filter(|error| error.category() == ErrorCategory::Cancelled)
                .or_else(|| {
                    active
                        .terminal_error
                        .take()
                        .filter(|error| error.category() == ErrorCategory::Cancelled)
                });
            return;
        }
        if active.terminal.is_some() {
            return;
        }
        active.terminal = Some(outcome);
        active.terminal_error = error;
    }

    async fn take_terminal(&self, run: &RunId) -> (RunOutcome, Option<AgentError>) {
        let state = self.shared.state.lock().await;
        match state.active.as_ref() {
            Some(active) if &active.run == run && active.cancelled => (
                RunOutcome::Cancelled,
                active
                    .terminal_error
                    .clone()
                    .filter(|error| error.category() == ErrorCategory::Cancelled),
            ),
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
        // Only explicit user cancellation dominates terminal arbitration.
        // Internal token cancellation (e.g. stream overflow) retains its cause.
        let (outcome, error) = if active.cancelled {
            (
                RunOutcome::Cancelled,
                error
                    .filter(|error| error.category() == ErrorCategory::Cancelled)
                    .or_else(|| {
                        active
                            .terminal_error
                            .clone()
                            .filter(|error| error.category() == ErrorCategory::Cancelled)
                    }),
            )
        } else {
            (outcome, error)
        };
        if let Some((call, decided)) = active.outcome_slot.take() {
            let dispatched = active
                .current
                .as_ref()
                .is_some_and(|current| current.dispatched && current.call.call() == &call);
            if dispatched
                && !active.conversation.iter().any(|item| {
                    matches!(item, ModelContextItem::ToolResult { call: recorded, .. } if recorded == &call)
                })
                && let Some(current) = active.current.as_ref()
            {
                match ModelContextItem::tool_result(
                    call.clone(),
                    current.item_key.clone(),
                    current.provider_ref.clone(),
                    current.call.tool().clone(),
                    decided.clone(),
                ) {
                    Ok(result) => active.conversation.push(result),
                    Err(_) => state.history_safety_exhausted = true,
                }
            }
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
        let mut unrecorded_current_call = false;
        if let Some(current) = active.current.take() {
            let call = current.call.call().clone();
            if current.dispatched {
                active.dispatched_calls.insert(call.clone());
            }
            if !active.outcomes.iter().any(|summary| summary.call == call) {
                unrecorded_current_call = current.dispatched;
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
        let persistence_error = active.persistence_error.clone();
        let persistence = if persistence_error.is_some() {
            PersistenceState::SaveFailed
        } else if self.shared.session_store.is_some() {
            PersistenceState::Saved
        } else {
            PersistenceState::Ephemeral
        };
        let finished = RunFinished::new(outcome, persistence, persistence_error)
            .expect("terminal persistence record builds");
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
        self.shared.diagnostic(format_args!(
            "nexus-runtime: run finished run={} outcome={outcome:?} error_category={:?} source_persistence={persistence:?} turns={} calls={} elapsed_ms={} output_limit_recovery_used={} data_dropped={} terminal_delivery={delivery:?}",
            run.as_str(),
            error.as_ref().map(AgentError::category),
            active.turns_used,
            active.calls_used,
            active.started.elapsed().as_millis(),
            active.output_limit_recovery_used,
            active.data_dropped,
        ));
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
        // Even an uncommitted run can have executed a tool whose effects are
        // unknown. Without durable outcome history, fail closed for this
        // runtime rather than letting a later task repeat the action blindly.
        let has_uncertain_outcome = active
            .outcomes
            .iter()
            .any(|outcome| !outcome_summary_is_certain(outcome));
        let has_uncertain_result = active.conversation[active.history_len..]
            .iter()
            .any(|item| {
                matches!(item, ModelContextItem::ToolResult { call, outcome, .. }
                if active.dispatched_calls.contains(call)
                    && (outcome.effect() == EffectState::Unknown
                        || outcome.evidence() == Evidence::Uncertain))
            });
        let has_projected_evidence = active.conversation[active.history_len..]
            .iter()
            .any(is_retained_evidence);
        let preserve_uncertain_exchange = has_projected_evidence
            || (has_uncertain_outcome && (has_uncertain_result || unrecorded_current_call));
        if unrecorded_current_call {
            // The tool may have been dispatched, but no result was recorded;
            // there is no complete exchange to carry forward safely.
            state.history_safety_exhausted = true;
        }
        // Failed/cancelled/limited runs are not committed: their partial
        // calls or provisional text must never contaminate the next request.
        // The exception is a narrowly retained exchange containing an
        // uncertain tool effect: it is required evidence against blind retry.
        if outcome == RunOutcome::Completed || preserve_uncertain_exchange {
            let mut items = if outcome == RunOutcome::Completed {
                active.conversation.clone()
            } else {
                active.conversation[active.history_len..].to_vec()
            };
            if outcome != RunOutcome::Completed {
                let complete_dispatched_calls: HashSet<CallId> = items
                    .iter()
                    .filter_map(|item| match item {
                        ModelContextItem::ToolResult { call, .. }
                            if active.dispatched_calls.contains(call) =>
                        {
                            Some(call.clone())
                        }
                        _ => None,
                    })
                    .collect();
                items.retain(|item| match item {
                    ModelContextItem::UserText(_) => true,
                    ModelContextItem::AssistantCall { call, .. } => {
                        complete_dispatched_calls.contains(call.call())
                    }
                    ModelContextItem::ToolResult { call, .. } => {
                        complete_dispatched_calls.contains(call)
                    }
                    _ => is_context_notice(item) || is_retained_evidence(item),
                });
                if history_exchange_effects_certain(&items) {
                    // The terminal summary says the effect was uncertain but
                    // no corresponding result can be carried forward.
                    state.history_safety_exhausted = true;
                }
            }
            // The completed request can contain inherited exchanges plus the
            // new answer. Admit the current exchange as before, then let the
            // cache's whole-exchange eviction enforce its aggregate ceiling.
            // Checking the entire projected transcript here would silently
            // keep stale history whenever appending an answer crosses the cap.
            let current_items = if outcome == RunOutcome::Completed {
                &items[active.history_len..]
            } else {
                &items
            };
            if !context_calls_are_paired(&items) {
                // Never cache orphan results or incomplete calls, including
                // defensive terminal paths that bypass normal recording.
                state.history_safety_exhausted = true;
            } else if current_items.len() <= self.shared.context_items_bound
                && conversation_bytes(current_items) <= nexus_core::provider::MAX_CONVERSATION_BYTES
            {
                if outcome == RunOutcome::Completed && active.pending_omission_items > 0 {
                    state
                        .history_omissions
                        .retain(|omission| omission.session != active.session);
                }
                if outcome == RunOutcome::Completed && active.inherited_history {
                    // Commit the selected projection, not the original cache:
                    // omitted summary sources must not reappear next submit.
                    state
                        .history
                        .retain(|exchange| exchange.session != active.session);
                }
                let mut exchanges: Vec<Vec<ModelContextItem>> = Vec::new();
                for item in items {
                    if exchanges.is_empty() || matches!(item, ModelContextItem::UserText(_)) {
                        exchanges.push(Vec::new());
                    }
                    exchanges.last_mut().expect("exchange exists").push(item);
                }
                let last = exchanges.len().saturating_sub(1);
                for (index, items) in exchanges.into_iter().enumerate() {
                    state.history.push_back(RetainedExchange {
                        session: active.session.clone(),
                        items,
                        derived_memory: if outcome == RunOutcome::Completed && index == last {
                            active.derived_memory.clone()
                        } else {
                            None
                        },
                    });
                }
                while state
                    .history
                    .iter()
                    .map(|exchange| {
                        exchange.items.len() + usize::from(exchange.derived_memory.is_some())
                    })
                    .sum::<usize>()
                    > self.shared.context_items_bound
                    || state
                        .history
                        .iter()
                        .map(|exchange| {
                            conversation_bytes(&exchange.items)
                                .saturating_add(memory_bytes(exchange.derived_memory.as_deref()))
                        })
                        .sum::<usize>()
                        > nexus_core::provider::MAX_CONVERSATION_BYTES
                {
                    if let Some(index) = state
                        .history
                        .iter()
                        .position(|exchange| history_exchange_effects_certain(&exchange.items))
                    {
                        let removed = &state.history[index];
                        let session = removed.session.clone();
                        let removed_items = removed.items.len();
                        state.history.remove(index);
                        mark_history_omitted(&mut state, session, removed_items);
                    } else {
                        // Do not rewrite the completed outcome. Bound the
                        // cache, but latch a fail-closed state before losing
                        // evidence: no later run in this runtime may call the
                        // model or dispatch tools based on incomplete history.
                        state.history_safety_exhausted = true;
                        if let Some(discarded) = state.history.pop_back() {
                            mark_history_omitted(
                                &mut state,
                                discarded.session,
                                discarded.items.len(),
                            );
                        }
                    }
                }
            } else {
                let omitted_items = if outcome == RunOutcome::Completed && active.inherited_history
                {
                    // Cache rejection must not resurrect source exchanges
                    // removed from this completed run's selected projection.
                    // Keep uncertain effects even when their replacement
                    // cannot be admitted into the finite cache.
                    let mut removed_items = current_items.len();
                    state.history.retain(|exchange| {
                        let remove = exchange.session == active.session
                            && history_exchange_effects_certain(&exchange.items);
                        if remove {
                            removed_items = removed_items.saturating_add(exchange.items.len());
                        }
                        !remove
                    });
                    removed_items
                } else {
                    items.len()
                };
                mark_history_omitted(&mut state, active.session.clone(), omitted_items);
                if preserve_uncertain_exchange || !history_exchange_effects_certain(&items) {
                    state.history_safety_exhausted = true;
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

fn outcome_summary_is_certain(outcome: &OutcomeSummary) -> bool {
    outcome.effect != EffectState::Unknown && outcome.evidence != Evidence::Uncertain
}

fn mark_history_omitted(state: &mut State, session: SessionId, removed_items: usize) {
    if removed_items == 0 {
        return;
    }
    if let Some(omission) = state
        .history_omissions
        .iter_mut()
        .find(|omission| omission.session == session)
    {
        omission.removed_items = omission.removed_items.saturating_add(removed_items);
    } else if state.history_omissions.len() < MAX_HISTORY_OMISSION_SESSIONS {
        state.history_omissions.push(HistoryOmission {
            session,
            removed_items,
        });
    } else {
        state.history_omission_fallback_items = state
            .history_omission_fallback_items
            .saturating_add(removed_items);
    }
}

/// Bounded request projection, also used after accepting a turn/result so
/// active state cannot grow into an unbounded source transcript. A single
/// bounded provider batch/tool result may transiently exceed the budget.
/// Inherited user exchanges go first, then oldest complete tool exchanges;
/// the current user task, newest complete exchange and pending calls stay.
fn compact_context(
    active: &mut ActiveRun,
    bound: usize,
) -> Result<Option<ContextReduction>, Terminal> {
    // Stage reductions so a context that ultimately cannot fit is not left
    // partially altered before its explicit limit terminal.
    let items = &active.conversation;
    let memory_items = usize::from(active.derived_memory.is_some());
    let item_bound = bound.saturating_sub(memory_items);
    let bytes = conversation_bytes(items)
        .saturating_add(active.instruction_bytes)
        .saturating_add(active.context_note_bytes)
        .saturating_add(context_compaction::projection_overhead(
            active,
            &active.context_note,
        ));
    let notice_count = items.iter().filter(|item| is_context_notice(item)).count();
    if items.len() <= item_bound
        && bytes <= nexus_core::provider::MAX_CONVERSATION_BYTES
        && notice_count <= 1
    {
        return Ok(None);
    }
    // No adapter contract currently establishes that opaque continuation
    // remains valid after transcript reduction. Never discard it or guess.
    if active.continuation.is_some() {
        return Err(context_limit(active, bound));
    }

    let note = if active.context_note.is_empty() {
        CONTEXT_NOTICE_TEXT
    } else {
        active.context_note.as_str()
    };
    let note_bytes = runtime_note_bytes(note, active.instruction_bytes > 0);
    let reserved_bytes = active
        .instruction_bytes
        .saturating_add(note_bytes)
        .saturating_add(context_compaction::projection_overhead(active, note));
    let projection = project_bounded_history(
        items,
        active.history_len,
        ContextBounds {
            max_items: bound,
            max_bytes: nexus_core::provider::MAX_CONVERSATION_BYTES,
            reserved_items: memory_items,
            reserved_bytes,
        },
    )
    .ok_or_else(|| context_limit(active, bound))?;

    active.conversation = projection.items;
    // All original items have been archived before any context projection.
    // The cursor is an index in this new view, not the archival sequence.
    active.source_cursor = active.conversation.len();
    active.history_len = projection.history_len;
    if projection.removed_items > 0 {
        active.context_note = note.to_owned();
        active.context_note_bytes = note_bytes;
    }
    Ok(Some(ContextReduction {
        method: nexus_core::ContextReductionMethod::BoundedHistory,
        removed_items: projection.removed_items,
        retained_items: active.conversation.len() + memory_items,
        retained_bytes: projection.retained_bytes,
    }))
}

#[cfg(test)]
fn context_notice_item() -> Result<ModelContextItem, Terminal> {
    ModelContextItem::assistant_text(CONTEXT_NOTICE_ITEM_KEY, CONTEXT_NOTICE_TEXT)
        .map_err(Terminal::failed)
}

fn restore_items(source: Vec<ModelContextItem>) -> Result<Vec<ModelContextItem>, ()> {
    let mut results = HashSet::new();
    for item in &source {
        if let ModelContextItem::ToolResult { call, .. } = item {
            results.insert(call.clone());
        }
    }
    let mut restored = Vec::with_capacity(source.len());
    for item in source {
        match &item {
            ModelContextItem::AssistantCall {
                item_key,
                provider_ref,
                call,
                ..
            } if !results.contains(call.call()) => {
                let call_id = call.call().clone();
                let item_key = item_key.as_str().to_owned();
                let provider_ref = provider_ref.as_str().to_owned();
                let tool = call.tool().clone();
                let uncertain = ToolOutcome::new(
                    ExecutionStatus::Interrupted,
                    EffectState::Unknown,
                    Evidence::Uncertain,
                    "Session ended before this tool call recorded an outcome; effects are unknown.",
                    false,
                )
                .map_err(|_| ())?;
                restored.push(item);
                restored.push(
                    ModelContextItem::tool_result(call_id, item_key, provider_ref, tool, uncertain)
                        .map_err(|_| ())?,
                );
            }
            _ => restored.push(item),
        }
    }
    if !context_calls_are_paired(&restored)
        || restored
            .first()
            .is_some_and(|item| !matches!(item, ModelContextItem::UserText(_)))
    {
        return Err(());
    }
    Ok(restored)
}

fn bounded_restored_history(
    mut items: Vec<ModelContextItem>,
    max_items: usize,
) -> Option<(Vec<ModelContextItem>, usize)> {
    if items.is_empty() {
        return Some((items, 0));
    }
    let original_len = items.len();
    // The projection API selects inherited exchanges before a current user
    // turn. A temporary sentinel provides that boundary and is removed before
    // anything is installed into runtime state.
    items.push(ModelContextItem::user_text("restore-boundary").ok()?);
    let projection = project_bounded_history(
        &items,
        original_len,
        ContextBounds {
            max_items,
            max_bytes: nexus_core::provider::MAX_CONVERSATION_BYTES,
            reserved_items: 0,
            reserved_bytes: 0,
        },
    )?;
    let mut retained = projection.items;
    if !matches!(retained.pop(), Some(ModelContextItem::UserText(_))) {
        return None;
    }
    let removed = original_len.saturating_sub(retained.len());
    Some((retained, removed))
}

fn split_history_exchanges(items: &[ModelContextItem]) -> Vec<Vec<ModelContextItem>> {
    let mut exchanges: Vec<Vec<ModelContextItem>> = Vec::new();
    for item in items {
        if matches!(item, ModelContextItem::UserText(_)) {
            exchanges.push(Vec::new());
        }
        if let Some(exchange) = exchanges.last_mut() {
            exchange.push(item.clone());
        }
    }
    exchanges
}

fn runtime_note_bytes(note: &str, has_host_instructions: bool) -> usize {
    if note.is_empty() {
        return 0;
    }
    note.len()
        .saturating_add("Runtime context note:\n".len())
        .saturating_add(usize::from(has_host_instructions) * 2)
}

fn publish_context_reduction(
    shared: &Arc<Shared>,
    active: &mut ActiveRun,
    run: &RunId,
    reduction: ContextReduction,
) {
    shared.diagnostic(format_args!(
        "nexus-runtime: context reduced: removed_items={} retained_items={} max_items={} retained_bytes={} max_bytes={}",
        reduction.removed_items,
        reduction.retained_items,
        shared.context_items_bound,
        reduction.retained_bytes,
        nexus_core::provider::MAX_CONVERSATION_BYTES,
    ));
    let outcome = emit_control(
        shared,
        active,
        run,
        EventPayload::ContextReduced {
            method: reduction.method,
            removed_items: reduction.removed_items,
            retained_items: reduction.retained_items,
            item_limit: shared.context_items_bound,
            retained_bytes: reduction.retained_bytes,
            byte_limit: nexus_core::provider::MAX_CONVERSATION_BYTES,
        },
    );
    apply_control_outcome(active, outcome);
}

fn context_limit(active: &ActiveRun, bound: usize) -> Terminal {
    // Only numeric state enters diagnostics, never transcript or opaque data.
    Terminal {
        outcome: RunOutcome::LimitReached,
        error: Some(AgentError::new(
            ErrorCategory::ResourceLimit,
            format!(
                "context budget exhausted: items={} max_items={} bytes={} max_bytes={} continuation={}",
                active.conversation.len(),
                bound,
                conversation_bytes(&active.conversation)
                    .saturating_add(active.instruction_bytes)
                    .saturating_add(active.context_note_bytes),
                nexus_core::provider::MAX_CONVERSATION_BYTES,
                u8::from(active.continuation.is_some()),
            ),
            RetryGuidance::DoNotRetry,
        )
        .expect("numeric context diagnostic is safe")),
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
        ProviderEvent::ToolCallDelta {
            item_key,
            assembled_bytes,
        } => item_key
            .len()
            .saturating_add(std::mem::size_of_val(assembled_bytes)),
        ProviderEvent::ToolCallReady(candidate) => candidate
            .item_key()
            .len()
            .saturating_add(candidate.provider_ref().len())
            .saturating_add(candidate.tool_name().len())
            .saturating_add(candidate.arguments_json().len()),
        ProviderEvent::ReasoningDelta { text } => text.len(),
        ProviderEvent::Failed(error) => error.message().len(),
        ProviderEvent::Usage(_) => std::mem::size_of::<Usage>(),
        ProviderEvent::TurnFinished(finished) => finished
            .continuation()
            .map_or(0, |continuation| continuation.bytes().len()),
    }
}

fn control_event_bytes(event: &RunEvent) -> usize {
    let payload_bytes = match event.payload() {
        EventPayload::RunStarted { request } => request.as_str().len(),
        EventPayload::AssistantTextDelta(text) | EventPayload::AssistantReasoningDelta(text) => {
            text.turn.as_str().len() + text.item_key.len() + text.text.len()
        }
        EventPayload::ContextReduced {
            method,
            removed_items: _,
            retained_items: _,
            item_limit: _,
            retained_bytes: _,
            byte_limit: _,
        } => method.as_str().len() + 5 * std::mem::size_of::<usize>(),
        EventPayload::ModelRequestStarted { turn, .. } => turn.as_str().len() + 64,
        EventPayload::ModelRequestFinished { turn, .. }
        | EventPayload::ModelResponseDiscarded { turn } => turn.as_str().len() + 16,
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

fn emit_reasoning(
    shared: &Arc<Shared>,
    active: &mut ActiveRun,
    run: &RunId,
    turn: &TurnId,
    text: &str,
) {
    if !shared.local_reasoning.load(Ordering::SeqCst) {
        return;
    }
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + MAX_TEXT_FRAGMENT_BYTES).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if let Ok(fragment) = AssistantText::new(turn.clone(), "reasoning", &text[start..end]) {
            emit_data(
                shared,
                active,
                run,
                EventPayload::AssistantReasoningDelta(fragment),
            );
        } else {
            active.data_dropped = true;
            return;
        }
        start = end;
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
    if shared.control_closed.load(Ordering::SeqCst) || shared.control_tx.is_closed() {
        shared.control_closed.store(true, Ordering::SeqCst);
        return ControlOutcome::Closed;
    }
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
                shared.control_closed.store(true, Ordering::SeqCst);
                {
                    let mut outbox = shared.outbox.lock().expect("control outbox readable");
                    let queued_bytes = outbox.iter().map(control_event_bytes).sum::<usize>();
                    let queued_events = outbox.len();
                    outbox.clear();
                    shared.outbox_bytes.fetch_sub(
                        queued_bytes.saturating_add(control_event_bytes(&event)),
                        Ordering::SeqCst,
                    );
                    shared
                        .outbox_events
                        .fetch_sub(queued_events.saturating_add(1), Ordering::SeqCst);
                }
                let mut state = shared.state.lock().await;
                if let Some(last) = state.last.as_mut() {
                    last.terminal_delivered = false;
                    last.truncated = true;
                }
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
    let truncated = if known.len() > Limits::MAX_SNAPSHOT_KNOWN_OUTCOMES {
        let excess = known.len() - Limits::MAX_SNAPSHOT_KNOWN_OUTCOMES;
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

    struct ContextCheckingProvider {
        inner: FakeProvider,
        bound: usize,
        largest: AtomicUsize,
    }

    impl ProviderPort for ContextCheckingProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            self.inner.capabilities()
        }

        fn stream(&self, request: &ModelRequest, context: &ProviderContext) -> Vec<ProviderEvent> {
            let items = request.conversation();
            assert!(items.len() <= self.bound);
            assert!(conversation_bytes(items) <= nexus_core::MAX_CONVERSATION_BYTES);
            assert!(matches!(&items[0], ModelContextItem::UserText(_)));
            self.largest.fetch_max(items.len(), Ordering::SeqCst);
            let mut calls = HashMap::new();
            for item in items {
                match item {
                    ModelContextItem::AssistantCall {
                        call,
                        item_key,
                        provider_ref,
                        ..
                    } => {
                        assert!(
                            calls
                                .insert(call.call(), (item_key, provider_ref, call.tool()))
                                .is_none()
                        );
                    }
                    ModelContextItem::ToolResult {
                        call,
                        item_key,
                        provider_ref,
                        tool,
                        ..
                    } => {
                        assert_eq!(calls.remove(call), Some((item_key, provider_ref, tool)));
                    }
                    _ => {}
                }
            }
            assert!(calls.is_empty(), "requests never contain unmatched calls");
            self.inner.stream(request, context)
        }
    }

    #[test]
    fn active_tool_run_exceeds_256_source_items_with_bounded_paired_context() {
        for bound in [256, 8192] {
            let mut script = Vec::new();
            for turn in 0..10 {
                script.push(FakeProvider::tool_turn(
                    (0..16)
                        .map(|n| {
                            CallCandidate::new(
                                format!("item-{n}"),
                                format!("ref-{turn}-{n}"),
                                "host_read",
                                r#"{"path":"src"}"#,
                            )
                            .expect("candidate")
                        })
                        .collect(),
                ));
            }
            script.push(FakeProvider::stop_turn("done"));
            let provider = Arc::new(ContextCheckingProvider {
                inner: FakeProvider::new(script),
                bound,
                largest: AtomicUsize::new(0),
            });
            let tool = Arc::new(FakeTool::succeeding("host_read", nexus_core::M0_REVISION));
            let mut config = quick_config();
            config.limits.max_model_turns_per_run = 11;
            config.limits.max_tool_calls_per_run = 160;
            config.limits.retained_context_items = bound;
            let (runtime, mut streams) = Runtime::new(
                config,
                provider.clone(),
                vec![ToolRegistration::read(tool.clone())],
            );
            test_rt().block_on(async {
                assert_eq!(
                    runtime.submit(submit_cmd("long-context")).await.reply(),
                    CommandReply::Accepted
                );
                let (_, finished) =
                    collect_until_finished(&mut streams.data, &mut streams.control).await;
                assert_eq!(finished.outcome(), RunOutcome::Completed);
                assert_eq!(tool.executed.load(Ordering::SeqCst), 160);
                assert_eq!(provider.inner.calls.load(Ordering::SeqCst), 11);
                let state = runtime.shared.state.lock().await;
                let retained = &state
                    .history
                    .back()
                    .expect("completed context retained")
                    .items;
                assert!(retained.len() <= bound);
                assert!(conversation_bytes(retained) <= nexus_core::MAX_CONVERSATION_BYTES);
            });
            if bound > 256 {
                assert_eq!(
                    provider.largest.load(Ordering::SeqCst),
                    321,
                    "representation is not clamped to the M0 default"
                );
            } else {
                assert!(provider.largest.load(Ordering::SeqCst) <= 256);
            }
        }
    }

    #[test]
    fn final_answer_completes_even_when_it_cannot_be_retained_in_context() {
        let provider = Arc::new(FakeProvider::new(vec![FakeProvider::stop_turn("answer")]));
        let mut config = quick_config();
        config.limits.retained_context_items = 1;
        let (runtime, mut streams) = Runtime::new(config, provider, Vec::new());
        test_rt().block_on(async {
            assert_eq!(
                runtime.submit(submit_cmd("task")).await.reply(),
                CommandReply::Accepted
            );
            let (_, finished) =
                collect_until_finished(&mut streams.data, &mut streams.control).await;
            assert_eq!(finished.outcome(), RunOutcome::Completed);
            assert!(finished.error().is_none());
            let state = runtime.shared.state.lock().await;
            assert!(
                state.history.is_empty(),
                "oversized completed history is not retained"
            );
        });
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

    struct BlockingAfterToolProvider {
        entered: std::sync::mpsc::Sender<()>,
        release: StdMutex<std::sync::mpsc::Receiver<()>>,
        calls: AtomicUsize,
    }

    impl BlockingAfterToolProvider {
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

    impl ProviderPort for BlockingAfterToolProvider {
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
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return FakeProvider::tool_turn(vec![candidate("host_read", r#"{"path":"src"}"#)]);
            }
            let _ = self.entered.send(());
            let release = self.release.lock().expect("release lock");
            let _ = release.recv_timeout(Duration::from_secs(5));
            FakeProvider::stop_turn("after tool")
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
        let tools = vec![
            ToolRegistration::read(read_tool.clone()),
            ToolRegistration::mutation(write_tool.clone()),
        ];
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

    #[test]
    fn host_instructions_are_fixed_and_sent_on_every_model_turn() {
        struct InstructionProvider {
            seen: AtomicUsize,
            expected: ModelInstructions,
        }
        impl ProviderPort for InstructionProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                FakeProvider::new(vec![]).capabilities()
            }

            fn stream(&self, request: &ModelRequest, _: &ProviderContext) -> Vec<ProviderEvent> {
                assert_eq!(request.instructions(), &self.expected);
                if self.seen.fetch_add(1, Ordering::SeqCst).is_multiple_of(2) {
                    FakeProvider::tool_turn(vec![
                        CallCandidate::new(
                            "item-0",
                            "ref-0",
                            "host_read",
                            r#"{"path":"src/main.rs"}"#,
                        )
                        .unwrap(),
                    ])
                } else {
                    FakeProvider::stop_turn("done")
                }
            }
        }
        test_rt().block_on(async {
            let instructions = ModelInstructions::new("host rule", "workspace fact").unwrap();
            let provider = Arc::new(InstructionProvider {
                seen: AtomicUsize::new(0),
                expected: instructions.clone(),
            });
            let (runtime, mut streams) = Runtime::try_new_with_instructions(
                quick_config(),
                provider.clone(),
                vec![ToolRegistration::read(Arc::new(FakeTool::succeeding(
                    "host_read",
                    nexus_core::M0_REVISION,
                )))],
                instructions,
            )
            .unwrap();
            for tag in ["first", "second"] {
                runtime.submit(submit_cmd(tag)).await;
                let (_, finished) =
                    collect_until_finished(&mut streams.data, &mut streams.control).await;
                assert_eq!(finished.outcome(), RunOutcome::Completed);
            }
            assert_eq!(provider.seen.load(Ordering::SeqCst), 4);
        });
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
        let duplicate = vec![
            ToolRegistration::read(Arc::new(FakeTool::succeeding(
                "host_read",
                nexus_core::M0_REVISION,
            ))),
            ToolRegistration::read(Arc::new(FakeTool::succeeding(
                "host_read",
                nexus_core::M0_REVISION,
            ))),
        ];
        assert!(
            Runtime::try_new(quick_config(), provider.clone(), duplicate).is_err(),
            "duplicate registrations fail explicitly"
        );
        let wrong_revision = vec![ToolRegistration::read(Arc::new(FakeTool::succeeding(
            "host_read",
            nexus_core::M0_REVISION + 1,
        )))];
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
            assert_eq!(finished.outcome(), RunOutcome::Completed, "{finished:?}");
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

    #[test]
    fn development_unresolvable_path_is_reported_without_terminating_the_run() {
        for has_approval_handler in [true, false] {
            let root = std::env::temp_dir();
            let missing = format!("nexus-missing-parent-{}/file.txt", std::process::id());
            assert!(!root.join(&missing).parent().unwrap().exists());
            let mut config = quick_config();
            config.policy = Policy::development(&root).unwrap();
            config.has_approval_handler = has_approval_handler;
            let mut bed = bed_with(
                vec![
                    FakeProvider::tool_turn(vec![candidate(
                        "host_write",
                        &format!(r#"{{"path":"{missing}"}}"#),
                    )]),
                    FakeProvider::stop_turn("The parent directory does not exist."),
                ],
                config,
                Duration::ZERO,
            );
            test_rt().block_on(async {
                bed.runtime.submit(submit_cmd("missing-parent")).await;
                let (events, finished) =
                    collect_until_finished(&mut bed.data, &mut bed.control).await;
                assert_eq!(finished.outcome(), RunOutcome::Completed);
                assert_eq!(bed.write_tool.executed.load(Ordering::SeqCst), 0);
                assert_eq!(bed.provider.calls.load(Ordering::SeqCst), 2);
                let outcomes = tool_finished_map(&events);
                assert_eq!(outcomes.len(), 1);
                let outcome = outcomes.values().next().unwrap();
                assert_eq!(outcome.status(), ExecutionStatus::Denied);
                assert_eq!(outcome.effect(), EffectState::NotStarted);
                assert_eq!(outcome.content(), "tool path cannot be resolved");
                assert!(!events.control.iter().any(|event| matches!(
                    event.payload(),
                    EventPayload::ApprovalRequired(_) | EventPayload::ToolStarted(_)
                )));
            });
        }
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
        let tools = vec![ToolRegistration::read(slow_tool.clone())];
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
        let (blocked_tool, _entered, release) = BlockingTool::new();
        let blocked_tool = Arc::new(blocked_tool);
        let provider = Arc::new(FakeProvider::new(vec![FakeProvider::tool_turn(vec![
            candidate("host_read", r#"{"path":"src"}"#),
        ])]));
        let mut limits = Limits::m0_test();
        limits.per_tool_timeout = Duration::from_millis(50);
        let tools = vec![ToolRegistration::read(blocked_tool.clone())];
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
            read_tool: Arc::new(FakeTool::succeeding("host_read", nexus_core::M0_REVISION)),
            write_tool: Arc::new(FakeTool::succeeding("host_write", nexus_core::M0_REVISION)),
        };
        let rt = test_rt();
        rt.block_on(async {
            let submit = bed.runtime.submit(submit_cmd("timeout")).await;
            assert_eq!(submit.reply(), CommandReply::Accepted);
            let run = submit.run().cloned().expect("run issued");
            let (collected, finished) =
                collect_until_finished(&mut bed.data, &mut bed.control).await;
            assert_eq!(finished.outcome(), RunOutcome::Completed);
            assert!(finished.error().is_none());
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
            release
                .send(())
                .expect("release after cleanup grace expires");
            tokio::time::timeout(Duration::from_secs(1), async {
                while bed.runtime.has_unconfirmed_work().await {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("late worker terminates and clears quarantine");

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
        let tools = Vec::new();
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
        let tools = vec![ToolRegistration::read(Arc::new(FakeTool::succeeding(
            "host_read",
            nexus_core::M0_REVISION,
        )))];
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
        let tools = vec![ToolRegistration::read(Arc::new(PanicTool::new(
            "host_read",
        )))];
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
        let (provider, next_turn_entered, next_turn_release) = BlockingAfterToolProvider::new();
        let provider = Arc::new(provider);
        let tools = vec![ToolRegistration::read(tool.clone())];
        let (runtime, streams) = Runtime::new(quick_config(), provider, tools);
        let mut data = streams.data;
        let mut control = streams.control;
        let rt = test_rt();
        rt.block_on(async {
            let submit = runtime.submit(submit_cmd("blocked-tool-cancel")).await;
            assert_eq!(submit.reply(), CommandReply::Accepted);
            assert!(runtime.has_unconfirmed_work().await);
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
            assert!(
                runtime.has_unconfirmed_work().await,
                "logical finalization does not clear worker quarantine"
            );

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
            tokio::task::spawn_blocking(move || {
                next_turn_entered
                    .recv_timeout(Duration::from_secs(5))
                    .expect("new run reaches its deliberately blocked provider turn");
            })
            .await
            .expect("provider entry waiter completes");
            assert!(
                runtime.has_unconfirmed_work().await,
                "the newly admitted run is active"
            );
            assert_eq!(accepted.reply(), CommandReply::Accepted);
            next_turn_release
                .send(())
                .expect("release the provider turn");
            let _ = collect_until_finished(&mut data, &mut control).await;
            assert_eq!(tool.executions.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn cancelled_blocked_provider_finalizes_promptly_and_quarantines_until_termination() {
        let (provider, entered, release) = BlockingProvider::new();
        let provider = Arc::new(provider);
        let tools = Vec::new();
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
    fn configured_event_capacities_are_the_effective_channel_bounds() {
        let mut limits = Limits::m0_test();
        limits.event_data_capacity = 3;
        limits.event_control_capacity = 2;
        let (runtime, _) = Runtime::new(
            RuntimeConfig {
                limits,
                policy: Policy::m0_test(),
                has_approval_handler: false,
            },
            Arc::new(NoopProvider),
            Vec::new(),
        );
        assert_eq!(runtime.shared.data_tx.capacity(), 3);
        assert_eq!(runtime.shared.control_tx.capacity(), 2);
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

    pub(super) fn shared_with_data() -> (Arc<Shared>, mpsc::Receiver<RunEvent>) {
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

    pub(super) fn shared_with_control() -> (Arc<Shared>, mpsc::Receiver<RunEvent>) {
        let (runtime, streams) = Runtime::new(
            RuntimeConfig {
                limits: Limits::m0_test(),
                policy: Policy::m0_test(),
                has_approval_handler: false,
            },
            Arc::new(NoopProvider),
            Vec::new(),
        );
        (runtime.shared, streams.control)
    }

    fn active_for(run: &RunId) -> ActiveRun {
        ActiveRun {
            context_management: nexus_core::ContextManagementSettings::default(),
            derived_memory: None,
            pending_compaction: None,
            input_estimate: 0,
            token_estimator: TokenEstimator::default(),
            summary_token_estimator: TokenEstimator::default(),
            instruction_bytes: 0,
            context_note: String::new(),
            context_note_bytes: 0,
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
            output_limit_recovery_used: false,
            task_output_limit_recovery_attempts: 0,
            response_recovery_note: None,
            access_failure_attempts: 0,
            continuation: None,
            conversation: Vec::new(),
            source_cursor: 0,
            history_len: 0,
            inherited_history: false,
            initial_context_reduction: None,
            pending_omission_items: 0,
            queue: VecDeque::new(),
            current: None,
            dispatched_calls: HashSet::new(),
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
            persistence_error: None,
            delivery_closed: false,
            finished_sent: false,
        }
    }

    pub(super) fn context_exchange(
        run: &RunId,
        n: usize,
        output_bytes: usize,
    ) -> Vec<ModelContextItem> {
        let call = ToolCall::new(
            run.clone(),
            TurnId::new(format!("turn-{n}")).expect("valid turn"),
            CallId::new(format!("call-{n}")).expect("valid call"),
            ToolId::new("host_read", nexus_core::M0_REVISION).expect("valid tool"),
            nexus_core::NormalizedArgs::new("{}").expect("valid args"),
        );
        let item = format!("item-{n}");
        let reference = format!("reference-{n}");
        vec![
            ModelContextItem::assistant_call(&item, &reference, call.clone()).expect("call item"),
            ModelContextItem::tool_result(
                call.call().clone(),
                item,
                reference,
                call.tool().clone(),
                ToolOutcome::new(
                    ExecutionStatus::Succeeded,
                    EffectState::KnownApplied,
                    Evidence::HostObserved,
                    "x".repeat(output_bytes),
                    false,
                )
                .expect("bounded outcome"),
            )
            .expect("result item"),
        ]
    }

    pub(super) fn context_run() -> ActiveRun {
        let mut active = active_for(&RunId::new("run-context").expect("valid run"));
        active
            .conversation
            .push(ModelContextItem::user_text("current task").expect("user"));
        active
    }

    #[test]
    fn context_projection_prunes_oldest_complete_exchanges_and_bounds_active_memory() {
        let mut active = context_run();
        for n in 0..200 {
            active
                .conversation
                .extend(context_exchange(&active.run, n, 1));
            assert!(compact_context(&mut active, 256).is_ok());
            assert!(active.conversation.len() <= 256);
        }
        assert_eq!(active.conversation.len(), 255);
        assert!(
            matches!(&active.conversation[0], ModelContextItem::UserText(text) if text.as_str() == "current task")
        );
        assert!(
            matches!(&active.conversation[1], ModelContextItem::AssistantCall { provider_ref, .. } if provider_ref.as_str() == "reference-73")
        );
        let expected = context_exchange(&active.run, 199, 1);
        assert_eq!(
            &active.conversation[active.conversation.len() - expected.len()..],
            expected.as_slice()
        );
        let before = active.conversation.clone();
        assert!(compact_context(&mut active, 256).is_ok());
        assert_eq!(
            active.conversation, before,
            "projection is deterministic and idempotent"
        );
    }

    #[test]
    fn context_projection_enforces_bytes_including_identity_and_reasoning() {
        let mut active = context_run();
        for n in 0..4 {
            active.conversation.push(
                ModelContextItem::assistant_text_with_reasoning(
                    format!("text-{n}"),
                    "answer",
                    Some("r".repeat(100)),
                )
                .expect("reasoning item"),
            );
            active.conversation.extend(context_exchange(
                &active.run,
                n,
                Limits::M0_TEST_TOOL_OUTPUT_BYTES,
            ));
        }
        assert!(conversation_bytes(&active.conversation) > nexus_core::MAX_CONVERSATION_BYTES);
        assert!(compact_context(&mut active, 8192).is_ok());
        assert_eq!(
            active.conversation.len(),
            10,
            "the omission note does not consume a conversation item"
        );
        assert!(!active.context_note.is_empty());
        assert!(
            conversation_bytes(&active.conversation) + active.context_note_bytes
                <= nexus_core::MAX_CONVERSATION_BYTES
        );
        assert!(
            matches!(&active.conversation[1], ModelContextItem::AssistantText { item_key, reasoning: Some(trace), .. } if item_key.as_str() == "text-1" && trace.len() == 100)
        );
    }

    #[test]
    fn context_projection_reserves_instruction_bytes_when_evicting_history() {
        let mut active = context_run();
        for n in 0..4 {
            active.conversation.extend(context_exchange(
                &active.run,
                n,
                Limits::M0_TEST_TOOL_OUTPUT_BYTES - 1024,
            ));
        }
        assert!(conversation_bytes(&active.conversation) < nexus_core::MAX_CONVERSATION_BYTES);
        active.instruction_bytes = nexus_core::MAX_INSTRUCTIONS_BYTES;
        let reduction = compact_context(&mut active, 8192)
            .unwrap_or_else(|_| panic!("context fits after eviction"))
            .expect("instruction pressure evicts an old exchange");
        assert_eq!(
            reduction.retained_bytes,
            conversation_bytes(&active.conversation)
                + active.instruction_bytes
                + active.context_note_bytes
        );
        assert!(reduction.retained_bytes <= nexus_core::MAX_CONVERSATION_BYTES);
        assert_eq!(active.instruction_bytes, nexus_core::MAX_INSTRUCTIONS_BYTES);
    }

    #[test]
    fn context_projection_keeps_parallel_calls_and_denials_with_their_results() {
        let mut active = context_run();
        let old = context_exchange(&active.run, 0, 1);
        let mut first = context_exchange(&active.run, 1, 1);
        let second = context_exchange(&active.run, 2, 1);
        let denied =
            CallCandidate::new("denied", "denied-ref", "unknown", "{}").expect("candidate");
        first[0] = ModelContextItem::assistant_denied_call(
            CallId::new("call-1").expect("call"),
            denied,
            None,
        )
        .expect("denied item");
        // Use the exact correlation of the denied candidate on its result.
        if let ModelContextItem::ToolResult {
            item_key,
            provider_ref,
            tool,
            outcome,
            ..
        } = &mut first[1]
        {
            *item_key = ItemKey::new("denied").expect("key");
            *provider_ref = nexus_core::ProviderRef::new("denied-ref").expect("ref");
            *tool = ToolId::new("unknown", nexus_core::M0_REVISION).expect("tool");
            *outcome = denied_outcome("unknown tool");
        }
        active.conversation.extend(old);
        active
            .conversation
            .extend([first[0].clone(), second[0].clone(), first[1].clone()]);
        // The newest turn is incomplete: it is never a removable boundary.
        assert!(compact_context(&mut active, 4).is_err());
        active.conversation.push(second[1].clone());
        assert!(compact_context(&mut active, 6).is_ok());
        assert_eq!(
            &active.conversation[1..],
            &[
                first[0].clone(),
                second[0].clone(),
                first[1].clone(),
                second[1].clone()
            ]
        );
        assert!(
            compact_context(&mut active, 4).is_err(),
            "newest complete turn is protected"
        );
    }

    #[test]
    fn context_projection_rejects_opaque_continuation_without_mutation() {
        let mut active = context_run();
        active
            .conversation
            .extend(context_exchange(&active.run, 0, 1));
        active
            .conversation
            .extend(context_exchange(&active.run, 1, 1));
        active.continuation =
            Some(ContinuationData::new("adapter", "profile", vec![1, 2, 3]).expect("continuation"));
        let before = active.conversation.clone();
        let continuation = active.continuation.clone();
        assert!(compact_context(&mut active, 5).is_ok());
        let terminal = compact_context(&mut active, 3).expect_err("explicit limit");
        assert_eq!(terminal.outcome, RunOutcome::LimitReached);
        let error = terminal.error.expect("numeric diagnostic");
        assert_eq!(error.category(), ErrorCategory::ResourceLimit);
        assert!(error.message().contains("max_items=3"));
        assert!(error.message().contains("continuation=1"));
        assert!(!error.message().contains("current task"));
        assert_eq!(active.conversation, before);
        assert_eq!(active.continuation, continuation);
    }

    #[test]
    fn context_projection_never_evicts_unknown_or_uncertain_effects() {
        for (effect, evidence) in [
            (EffectState::Unknown, Evidence::HostObserved),
            (EffectState::KnownApplied, Evidence::Uncertain),
        ] {
            let mut active = context_run();
            let mut uncertain = context_exchange(&active.run, 0, 1);
            if let ModelContextItem::ToolResult { outcome, .. } = &mut uncertain[1] {
                *outcome = ToolOutcome::new(
                    ExecutionStatus::Succeeded,
                    effect,
                    evidence,
                    "outcome could not establish retry safety",
                    false,
                )
                .expect("bounded uncertain outcome");
            } else {
                panic!("exchange ends in its tool result");
            }
            active.conversation.extend(uncertain);
            active
                .conversation
                .extend(context_exchange(&active.run, 1, 1));
            let before = active.conversation.clone();
            let terminal = compact_context(&mut active, 4)
                .expect_err("uncertain oldest exchange cannot be evicted");
            assert_eq!(terminal.outcome, RunOutcome::LimitReached);
            assert_eq!(active.conversation, before);
        }
    }

    #[test]
    fn context_projection_never_evicts_uncertain_inherited_exchanges() {
        let mut active = active_for(&RunId::new("run-context").expect("valid run"));
        let mut history = context_exchange(&active.run, 0, 1);
        if let ModelContextItem::ToolResult { outcome, .. } = &mut history[1] {
            *outcome = ToolOutcome::new(
                ExecutionStatus::TimedOut,
                EffectState::Unknown,
                Evidence::Uncertain,
                "tool outcome is uncertain",
                false,
            )
            .expect("bounded uncertain outcome");
        }
        active.conversation.extend(history);
        active.history_len = active.conversation.len();
        active
            .conversation
            .push(ModelContextItem::user_text("new task").expect("user"));
        let before = active.conversation.clone();

        let terminal = compact_context(&mut active, 2)
            .expect_err("uncertain inherited effects cannot be evicted");
        assert_eq!(terminal.outcome, RunOutcome::LimitReached);
        assert_eq!(active.conversation, before);
    }

    #[test]
    fn inherited_notices_are_deduplicated_before_budget_accounting() {
        let mut active = context_run();
        active
            .conversation
            .push(context_notice_item().unwrap_or_else(|_| panic!("valid notice")));
        active
            .conversation
            .push(ModelContextItem::user_text("older task").expect("user"));
        active
            .conversation
            .push(context_notice_item().unwrap_or_else(|_| panic!("valid notice")));
        active.history_len = active.conversation.len();
        active
            .conversation
            .push(ModelContextItem::user_text("new task").expect("user"));
        let reduction = compact_context(&mut active, 4)
            .unwrap_or_else(|_| panic!("duplicate notices alone make room"))
            .expect("reduction is visible");
        assert_eq!(active.history_len, 2);
        assert_eq!(
            active
                .conversation
                .iter()
                .filter(|item| is_context_notice(item))
                .count(),
            0
        );
        assert!(!active.context_note.is_empty());
        assert_eq!(reduction.retained_items, active.conversation.len());
        assert_eq!(
            reduction.retained_bytes,
            conversation_bytes(&active.conversation) + active.context_note_bytes
        );
        assert_eq!(
            reduction.removed_items, 2,
            "legacy notices are represented by one out-of-history note"
        );
    }

    #[test]
    fn omission_note_does_not_consume_a_retained_context_item() {
        let mut active = context_run();
        active.history_len = 1;
        active
            .conversation
            .push(ModelContextItem::user_text("new task").expect("user"));
        let before = active.conversation.clone();
        assert!(compact_context(&mut active, 1).is_ok());
        assert_ne!(active.conversation, before);
        assert_eq!(active.conversation.len(), 1);
        assert!(!active.context_note.is_empty());
    }

    #[test]
    fn submit_does_not_bypass_uncertain_history_protection() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let mut limits = Limits::m0_test();
            limits.retained_context_items = 3;
            let (runtime, mut streams) = Runtime::new(
                RuntimeConfig {
                    limits,
                    policy: Policy::m0_test(),
                    has_approval_handler: false,
                },
                Arc::new(NoopProvider),
                Vec::new(),
            );
            let mut previous = context_run();
            let mut exchange = context_exchange(&previous.run, 0, 1);
            if let ModelContextItem::ToolResult { outcome, .. } = &mut exchange[1] {
                *outcome = ToolOutcome::new(
                    ExecutionStatus::Succeeded,
                    EffectState::Unknown,
                    Evidence::Uncertain,
                    "unknown effects",
                    false,
                )
                .expect("outcome");
            }
            previous.conversation.extend(exchange);
            let session = previous.session.clone();
            let before = previous.conversation.clone();
            runtime
                .shared
                .state
                .lock()
                .await
                .history
                .push_back(RetainedExchange {
                    session: session.clone(),
                    items: previous.conversation,
                    derived_memory: None,
                });
            let response = runtime
                .submit(
                    SubmitCommand::new(
                        RequestId::new("req-next").expect("request"),
                        session,
                        "next task",
                        "m0-test",
                    )
                    .expect("submit"),
                )
                .await;
            assert_eq!(response.reply(), CommandReply::Accepted);
            while let Some(event) = streams.control.recv().await {
                if let EventPayload::RunFinished(finished) = event.payload() {
                    assert_eq!(
                        finished.outcome(),
                        RunOutcome::LimitReached,
                        "NoopProvider would fail if called"
                    );
                    break;
                }
            }
            assert_eq!(runtime.shared.state.lock().await.history[0].items, before);
        });
    }

    #[test]
    fn completed_uncertain_history_overflow_blocks_future_model_work() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let mut limits = Limits::m0_test();
            limits.retained_context_items = 3;
            let (runtime, _streams) = Runtime::new(
                RuntimeConfig {
                    limits,
                    policy: Policy::m0_test(),
                    has_approval_handler: false,
                },
                Arc::new(NoopProvider),
                Vec::new(),
            );
            let mut active = context_run();
            let mut exchange = context_exchange(&active.run, 0, 1);
            if let ModelContextItem::ToolResult { outcome, .. } = &mut exchange[1] {
                *outcome = ToolOutcome::new(
                    ExecutionStatus::Succeeded,
                    EffectState::Unknown,
                    Evidence::Uncertain,
                    "unknown effects",
                    false,
                )
                .expect("outcome");
            }
            active.conversation.extend(exchange);
            active
                .dispatched_calls
                .insert(CallId::new("call-0").expect("call"));
            let run = active.run.clone();
            {
                let mut state = runtime.shared.state.lock().await;
                state.history.push_back(RetainedExchange {
                    session: active.session.clone(),
                    items: active.conversation.clone(),
                    derived_memory: None,
                });
                state.active = Some(active);
            }
            runtime.finish_run(&run, RunOutcome::Completed, None).await;
            {
                let mut state = runtime.shared.state.lock().await;
                assert!(state.history_safety_exhausted);
                assert_eq!(state.history.len(), 1, "cache remains bounded");
                assert_eq!(
                    state.last.as_ref().expect("finished").outcome,
                    RunOutcome::Completed
                );
                state.active = Some(context_run());
            }
            assert_eq!(
                runtime
                    .on_preparing(&run)
                    .await
                    .expect_err("unsafe history cannot authorize model work")
                    .outcome,
                RunOutcome::LimitReached
            );
        });
    }

    #[test]
    fn history_omission_markers_are_bounded_with_a_global_fallback() {
        let mut state = State {
            active: None,
            restore_busy: false,
            last: None,
            history: VecDeque::new(),
            history_omissions: Vec::new(),
            history_omission_fallback_items: 0,
            history_safety_exhausted: false,
            durable_sessions: HashSet::new(),
            quarantine: Vec::new(),
        };
        for index in 0..=MAX_HISTORY_OMISSION_SESSIONS {
            mark_history_omitted(
                &mut state,
                SessionId::new(format!("session-{index}")).expect("session"),
                3,
            );
        }
        assert_eq!(state.history_omissions.len(), MAX_HISTORY_OMISSION_SESSIONS);
        assert_eq!(state.history_omission_fallback_items, 3);
    }

    #[test]
    fn omission_marker_survives_failed_run_and_clears_after_completed_cache() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let (runtime, _streams) = Runtime::new(
                RuntimeConfig {
                    limits: Limits::m0_test(),
                    policy: Policy::m0_test(),
                    has_approval_handler: false,
                },
                Arc::new(NoopProvider),
                Vec::new(),
            );
            let mut active = context_run();
            let session = active.session.clone();
            let failed_run = active.run.clone();
            active.pending_omission_items = 3;
            active
                .conversation
                .push(context_notice_item().unwrap_or_else(|_| panic!("notice")));
            {
                let mut state = runtime.shared.state.lock().await;
                state.history_omissions.push(HistoryOmission {
                    session: session.clone(),
                    removed_items: 3,
                });
                state.active = Some(active);
            }
            runtime
                .finish_run(&failed_run, RunOutcome::Failed, None)
                .await;
            assert_eq!(
                runtime.shared.state.lock().await.history_omissions[0].removed_items,
                3,
                "failed runs do not acknowledge an omission notice"
            );

            let mut completed = context_run();
            completed.pending_omission_items = 3;
            completed
                .conversation
                .push(context_notice_item().unwrap_or_else(|_| panic!("notice")));
            let completed_run = completed.run.clone();
            runtime.shared.state.lock().await.active = Some(completed);
            runtime
                .finish_run(&completed_run, RunOutcome::Completed, None)
                .await;
            assert!(
                runtime
                    .shared
                    .state
                    .lock()
                    .await
                    .history_omissions
                    .is_empty(),
                "a completed cached exchange carries the notice forward"
            );
        });
    }

    #[test]
    fn failed_run_preserves_projected_uncertainty_without_native_tool_records() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (runtime, _streams) = Runtime::new(
                    RuntimeConfig {
                        limits: Limits::m0_test(),
                        policy: Policy::m0_test(),
                        has_approval_handler: false,
                    },
                    Arc::new(NoopProvider),
                    Vec::new(),
                );
                let mut active = context_run();
                let evidence = ModelContextItem::assistant_text(
                    nexus_context::EVIDENCE_KEY,
                    format!(
                        "{}call=old-call effect=Unknown",
                        nexus_context::EVIDENCE_LABEL
                    ),
                )
                .unwrap();
                active.conversation.push(evidence.clone());
                active.conversation.push(
                    ModelContextItem::assistant_text("partial", "uncommitted analysis").unwrap(),
                );
                let run = active.run.clone();
                runtime.shared.state.lock().await.active = Some(active);
                runtime.finish_run(&run, RunOutcome::Failed, None).await;
                let state = runtime.shared.state.lock().await;
                assert!(!state.history_safety_exhausted);
                let retained = &state.history.back().expect("retained evidence").items;
                assert_eq!(retained.len(), 2);
                assert_eq!(retained[1], evidence);
            });
    }

    #[test]
    fn failed_run_with_uncertain_effect_retains_evidence_for_future_model_work() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let (runtime, _streams) = Runtime::new(
                RuntimeConfig {
                    limits: Limits::m0_test(),
                    policy: Policy::m0_test(),
                    has_approval_handler: false,
                },
                Arc::new(NoopProvider),
                Vec::new(),
            );
            let mut active = context_run();
            active.outcomes.push(OutcomeSummary {
                call: CallId::new("call-0").expect("call"),
                status: ExecutionStatus::TimedOut,
                effect: EffectState::Unknown,
                evidence: Evidence::Uncertain,
            });
            let mut exchange = context_exchange(&active.run, 0, 1);
            if let ModelContextItem::ToolResult { outcome, .. } = &mut exchange[1] {
                *outcome = ToolOutcome::new(
                    ExecutionStatus::TimedOut,
                    EffectState::Unknown,
                    Evidence::Uncertain,
                    "tool outcome is uncertain",
                    false,
                )
                .expect("outcome");
            }
            active.conversation.extend(exchange);
            let queued = context_exchange(&active.run, 1, 1);
            active.conversation.push(queued[0].clone());
            active
                .dispatched_calls
                .insert(CallId::new("call-0").expect("call"));
            let run = active.run.clone();
            runtime.shared.state.lock().await.active = Some(active);

            runtime.finish_run(&run, RunOutcome::Failed, None).await;
            let state = runtime.shared.state.lock().await;
            assert!(!state.history_safety_exhausted);
            assert!(
                state.history.iter().any(|exchange| {
                    exchange.items.iter().any(|item| {
                        matches!(
                            item,
                            ModelContextItem::ToolResult { outcome, .. }
                                if outcome.effect() == EffectState::Unknown
                                    && outcome.evidence() == Evidence::Uncertain
                        )
                    })
                }),
                "failed-run uncertain evidence is retained for the next request"
            );
            let retained = &state.history.back().expect("retained exchange").items;
            let assistant_calls: Vec<CallId> = retained
                .iter()
                .filter_map(|item| match item {
                    ModelContextItem::AssistantCall { call, .. } => Some(call.call().clone()),
                    _ => None,
                })
                .collect();
            let results: Vec<CallId> = retained
                .iter()
                .filter_map(|item| match item {
                    ModelContextItem::ToolResult { call, .. } => Some(call.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(assistant_calls, vec![CallId::new("call-0").expect("call")]);
            assert_eq!(
                results, assistant_calls,
                "no incomplete queued call is retained"
            );
            assert_eq!(
                state.last.as_ref().expect("finished").outcome,
                RunOutcome::Failed
            );
        });
    }

    #[test]
    fn terminal_outcome_slot_for_dispatched_call_becomes_retained_tool_result() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let (runtime, _streams) = Runtime::new(
                RuntimeConfig {
                    limits: Limits::m0_test(),
                    policy: Policy::m0_test(),
                    has_approval_handler: false,
                },
                Arc::new(NoopProvider),
                Vec::new(),
            );
            let mut active = context_run();
            let run = active.run.clone();
            let call = ToolCall::new(
                run.clone(),
                TurnId::new("turn-terminal-slot").expect("turn"),
                CallId::new("call-terminal-slot").expect("call"),
                ToolId::new("host_read", nexus_core::M0_REVISION).expect("tool"),
                nexus_core::NormalizedArgs::new("{}").expect("args"),
            );
            active.conversation.push(
                ModelContextItem::assistant_call(
                    "item-terminal-slot",
                    "provider-terminal-slot",
                    call.clone(),
                )
                .expect("call item"),
            );
            active.current = Some(CurrentCall {
                call: call.clone(),
                item_key: "item-terminal-slot".to_owned(),
                provider_ref: "provider-terminal-slot".to_owned(),
                binding: None,
                dispatched: true,
            });
            active.outcome_slot = Some((
                call.call().clone(),
                ToolOutcome::new(
                    ExecutionStatus::TimedOut,
                    EffectState::Unknown,
                    Evidence::Uncertain,
                    "tool termination is unconfirmed",
                    false,
                )
                .expect("outcome"),
            ));
            runtime.shared.state.lock().await.active = Some(active);

            runtime.finish_run(&run, RunOutcome::Failed, None).await;
            let state = runtime.shared.state.lock().await;
            let retained = &state
                .history
                .back()
                .expect("uncertain exchange retained")
                .items;
            assert!(retained.iter().any(|item| matches!(
                item,
                ModelContextItem::ToolResult { call: retained_call, outcome, .. }
                    if retained_call == call.call()
                        && outcome.effect() == EffectState::Unknown
            )));
            assert!(!state.history_safety_exhausted);
            assert!(context_calls_are_paired(retained));
        });
    }

    #[test]
    fn retained_context_rejects_orphan_results_and_incomplete_calls() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            for (outcome, orphan_result) in [
                (RunOutcome::Completed, false),
                (RunOutcome::Completed, true),
                (RunOutcome::Failed, true),
            ] {
                let (runtime, _streams) = Runtime::new(
                    RuntimeConfig {
                        limits: Limits::m0_test(),
                        policy: Policy::m0_test(),
                        has_approval_handler: false,
                    },
                    Arc::new(NoopProvider),
                    Vec::new(),
                );
                let mut active = context_run();
                let run = active.run.clone();
                let mut exchange = context_exchange(&run, 0, 1);
                if outcome == RunOutcome::Failed {
                    if let ModelContextItem::ToolResult { outcome, .. } = &mut exchange[1] {
                        *outcome = timeout_outcome("uncertain orphan result");
                    }
                    let call = CallId::new("call-0").expect("call");
                    active.dispatched_calls.insert(call.clone());
                    active.outcomes.push(OutcomeSummary {
                        call,
                        status: ExecutionStatus::TimedOut,
                        effect: EffectState::Unknown,
                        evidence: Evidence::Uncertain,
                    });
                }
                active
                    .conversation
                    .push(exchange[usize::from(orphan_result)].clone());
                runtime.shared.state.lock().await.active = Some(active);
                runtime.finish_run(&run, outcome, None).await;
                let state = runtime.shared.state.lock().await;
                assert!(state.history.is_empty(), "malformed context is not cached");
                assert!(state.history_safety_exhausted);
                assert_eq!(state.last.as_ref().expect("finished").outcome, outcome);
            }
        });
    }

    #[test]
    fn context_projection_byte_limit_is_inclusive_and_uncompactable_text_fails() {
        let mut active = context_run();
        let remaining =
            nexus_core::MAX_CONVERSATION_BYTES - conversation_bytes(&active.conversation);
        for n in 0..4 {
            active.conversation.push(
                ModelContextItem::assistant_reasoning(
                    "r".repeat(remaining / 4 + usize::from(n < remaining % 4)),
                )
                .expect("bounded reasoning"),
            );
        }
        assert_eq!(
            conversation_bytes(&active.conversation),
            nexus_core::MAX_CONVERSATION_BYTES
        );
        assert!(compact_context(&mut active, 256).is_ok());
        active
            .conversation
            .push(ModelContextItem::assistant_text("k", "x").expect("text"));
        let before = active.conversation.clone();
        let terminal = compact_context(&mut active, 256).expect_err("cannot prune current text");
        assert_eq!(terminal.outcome, RunOutcome::LimitReached);
        assert!(
            terminal
                .error
                .expect("diagnostic")
                .message()
                .contains("max_bytes=1048576")
        );
        assert_eq!(active.conversation, before);
    }

    #[test]
    fn context_projection_evicts_inherited_exchanges_before_current_tools() {
        let mut active = context_run();
        active
            .conversation
            .extend(context_exchange(&active.run, 0, 1));
        active.history_len = active.conversation.len();
        active
            .conversation
            .push(ModelContextItem::user_text("new task").expect("user"));
        active
            .conversation
            .extend(context_exchange(&active.run, 1, 1));
        assert!(compact_context(&mut active, 4).is_ok());
        assert_eq!(active.history_len, 0);
        assert_eq!(active.conversation.len(), 3);
        assert!(
            matches!(&active.conversation[0], ModelContextItem::UserText(text) if text.as_str() == "new task")
        );
    }

    #[test]
    fn cancelled_run_does_not_project_or_dispatch_another_request() {
        let (runtime, _streams) = Runtime::new(
            RuntimeConfig {
                limits: Limits::m0_test(),
                policy: Policy::m0_test(),
                has_approval_handler: false,
            },
            Arc::new(NoopProvider),
            Vec::new(),
        );
        let mut active = context_run();
        for n in 0..130 {
            active
                .conversation
                .extend(context_exchange(&active.run, n, 1));
        }
        active.cancelled = true;
        active.token.cancel();
        let run = active.run.clone();
        let before = active.conversation.clone();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            runtime.shared.state.lock().await.active = Some(active);
            let terminal = runtime.on_calling_model(&run).await.expect_err("cancelled");
            assert_eq!(terminal.outcome, RunOutcome::Cancelled);
            assert_eq!(
                runtime
                    .shared
                    .state
                    .lock()
                    .await
                    .active
                    .as_ref()
                    .expect("active")
                    .conversation,
                before
            );
        });
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
        requests: StdMutex<Vec<ModelRequest>>,
        script: StdMutex<VecDeque<Vec<ProviderEvent>>>,
        gate: Option<Gate>,
    }

    impl ScriptedProvider {
        fn new(script: Vec<Vec<ProviderEvent>>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                requests: StdMutex::new(Vec::new()),
                script: StdMutex::new(script.into()),
                gate: None,
            }
        }

        fn gated(script: Vec<Vec<ProviderEvent>>, gate: Gate) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                requests: StdMutex::new(Vec::new()),
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

        fn stream(&self, request: &ModelRequest, _context: &ProviderContext) -> Vec<ProviderEvent> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().unwrap().push(request.clone());
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
        let tools = vec![
            ToolRegistration::read(read.clone()),
            ToolRegistration::mutation(write.clone()),
        ];
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
            dispatched: false,
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
            context_management: nexus_core::ContextManagementSettings::default(),
            derived_memory: None,
            pending_compaction: None,
            input_estimate: 0,
            token_estimator: TokenEstimator::default(),
            summary_token_estimator: TokenEstimator::default(),
            instruction_bytes: 0,
            context_note: String::new(),
            context_note_bytes: 0,
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
            output_limit_recovery_used: false,
            task_output_limit_recovery_attempts: 0,
            response_recovery_note: None,
            access_failure_attempts: 0,
            continuation: None,
            conversation: Vec::new(),
            source_cursor: 0,
            history_len: 0,
            inherited_history: false,
            initial_context_reduction: None,
            pending_omission_items: 0,
            queue: VecDeque::new(),
            current: None,
            dispatched_calls: HashSet::new(),
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
            persistence_error: None,
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

    fn output_limit_turn() -> Vec<ProviderEvent> {
        vec![ProviderEvent::TurnFinished(nexus_core::TurnFinished::new(
            FinishReason::OutputLimit,
            Usage::new(Some(3), Some(4), UsageFinality::Final),
            None,
        ))]
    }

    #[test]
    fn access_recovery_reuses_prepared_request_beyond_turn_budget() {
        let mut limits = Limits::m0_test();
        limits.max_model_turns_per_run = 1;
        let failure = |category| {
            vec![ProviderEvent::Failed(
                AgentError::new(
                    category,
                    "access unavailable",
                    RetryGuidance::RetryAfterBackoff,
                )
                .unwrap(),
            )]
        };
        let bed = make_bed(
            limits,
            true,
            vec![
                failure(ErrorCategory::Authentication),
                failure(ErrorCategory::RateLimited),
                stop_turn("recovered"),
            ],
            None,
            None,
        );
        let run = run_id("access-recovery");
        test_rt().block_on(async {
            install(&bed, active_for(&run)).await;
            let terminal = bed
                .runtime
                .on_calling_model(&run)
                .await
                .expect_err("completed");
            assert_eq!(terminal.outcome, RunOutcome::Completed);
            assert_eq!(inspect(&bed, |active| active.turns_used).await, Some(1));
            assert!(bed.shared.state.lock().await.quarantine.is_empty());
        });
        let requests = bed.provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_ne!(requests[0].turn(), requests[1].turn());
        assert_ne!(requests[1].turn(), requests[2].turn());
        assert_eq!(
            requests[0].clone().with_turn(requests[1].turn().clone()),
            requests[1]
        );
        assert_eq!(
            requests[1].clone().with_turn(requests[2].turn().clone()),
            requests[2]
        );
    }

    #[test]
    fn access_recovery_classification_does_not_retry_integrity_or_local_failures() {
        for category in [
            ErrorCategory::InvalidInput,
            ErrorCategory::Internal,
            ErrorCategory::ResourceLimit,
            ErrorCategory::StorageFailure,
            ErrorCategory::PermissionDenied,
            ErrorCategory::Protocol,
            ErrorCategory::Cancelled,
            ErrorCategory::Authentication,
            ErrorCategory::RateLimited,
            ErrorCategory::Timeout,
        ] {
            assert!(!provider_access_failure(&runtime_error(
                category,
                "terminal failure"
            )));
        }
        let transport = AgentError::new(
            ErrorCategory::Protocol,
            "transport unavailable",
            RetryGuidance::RetryAfterBackoff,
        )
        .unwrap();
        assert!(provider_access_failure(&transport));
    }

    #[test]
    fn access_recovery_backoff_observes_cancel_and_deadline_without_dispatch() {
        for cancelled in [true, false] {
            let bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
            let run = run_id("backoff-controls");
            test_rt().block_on(async {
                let mut active = active_for(&run);
                active.access_failure_attempts = 5;
                if !cancelled {
                    active.deadline = StdInstant::now() + Duration::from_millis(10);
                }
                let token = active.token.clone();
                let wake = active.wake.clone();
                install(&bed, active).await;
                let wait = bed
                    .runtime
                    .wait_provider_recovery(&run, ErrorCategory::Timeout);
                let cancel = async {
                    tokio::task::yield_now().await;
                    if cancelled {
                        token.cancel();
                        wake.notify_one();
                    }
                };
                let (result, ()) = tokio::join!(wait, cancel);
                let terminal = result.expect_err("backoff interrupted");
                assert_eq!(
                    terminal.outcome,
                    if cancelled {
                        RunOutcome::Cancelled
                    } else {
                        RunOutcome::LimitReached
                    }
                );
                assert_eq!(bed.provider.call_count(), 0);
            });
        }
    }

    #[test]
    fn task_responses_after_deadline_never_complete_or_admit_tools() {
        for events in [
            stop_turn("late answer"),
            vec![
                ProviderEvent::ToolCallReady(
                    CallCandidate::new("candidate", "reference", "host_read", "{}").unwrap(),
                ),
                ProviderEvent::TurnFinished(TurnFinished::new(
                    FinishReason::ToolCalls,
                    Usage::new(None, None, UsageFinality::Final),
                    None,
                )),
            ],
        ] {
            let bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
            let run = run_id("late-provider-response");
            test_rt().block_on(async {
                let mut active = active_for(&run);
                active.deadline = StdInstant::now();
                let original = active.conversation.clone();
                install(&bed, active).await;
                let terminal = bed
                    .runtime
                    .ingest_model_batch(
                        &run,
                        &turn_id("late"),
                        events,
                        &mut StreamedPrefix::default(),
                    )
                    .await
                    .expect_err("the run deadline cannot lose to response acceptance");
                assert_eq!(terminal.outcome, RunOutcome::LimitReached);
                assert_eq!(
                    inspect(&bed, |active| active.conversation.clone()).await,
                    Some(original)
                );
                assert_eq!(
                    inspect(&bed, |active| (
                        active.turns_used,
                        active.calls_used,
                        active.queue.len()
                    ))
                    .await,
                    Some((0, 0, 0))
                );
                assert_eq!(bed.read.execution_count(), 0);
            });
        }
    }

    #[test]
    fn discarded_response_marker_follows_all_text_and_precedes_replacement_request() {
        struct Provider {
            inner: ScriptedProvider,
            incremental: bool,
        }
        impl ProviderPort for Provider {
            fn capabilities(&self) -> ProviderCapabilities {
                self.inner.capabilities()
            }

            fn stream(
                &self,
                request: &ModelRequest,
                context: &ProviderContext,
            ) -> Vec<ProviderEvent> {
                self.inner.stream(request, context)
            }

            fn supports_incremental_streaming(&self) -> bool {
                self.incremental
            }

            fn stream_with_sink(
                &self,
                request: &ModelRequest,
                context: &ProviderContext,
                sink: &(dyn Fn(ProviderEvent) + Send + Sync),
            ) -> Vec<ProviderEvent> {
                let events = self.stream(request, context);
                for event in &events {
                    if matches!(event, ProviderEvent::TextDelta { .. }) {
                        sink(event.clone());
                    }
                }
                events
            }
        }

        for incremental in [false, true] {
            for reason in [
                None,
                Some(FinishReason::Incomplete),
                Some(FinishReason::OutputLimit),
            ] {
                let terminal = reason.map_or_else(
                    || {
                        ProviderEvent::Failed(
                            AgentError::new(
                                ErrorCategory::Protocol,
                                "transport interrupted",
                                RetryGuidance::RetryAfterBackoff,
                            )
                            .unwrap(),
                        )
                    },
                    |reason| {
                        ProviderEvent::TurnFinished(TurnFinished::new(
                            reason,
                            Usage::new(None, None, UsageFinality::Final),
                            None,
                        ))
                    },
                );
                let provider = Arc::new(Provider {
                    inner: ScriptedProvider::new(vec![
                        vec![
                            ProviderEvent::TextDelta {
                                item_key: "partial".into(),
                                text: "first ".into(),
                            },
                            ProviderEvent::TextDelta {
                                item_key: "partial".into(),
                                text: "last".into(),
                            },
                            terminal,
                        ],
                        stop_turn("replacement"),
                    ]),
                    incremental,
                });
                let (runtime, mut streams) = Runtime::new(
                    RuntimeConfig {
                        limits: Limits::m0_test(),
                        policy: Policy::m0_test(),
                        has_approval_handler: false,
                    },
                    provider,
                    Vec::new(),
                );
                let run = run_id("discard-order");
                test_rt().block_on(async {
                    runtime.shared.state.lock().await.active = Some(active_for(&run));
                    assert!(matches!(
                        runtime.on_calling_model(&run).await,
                        Ok(RunState::CallingModel)
                    ));
                    assert_eq!(
                        runtime
                            .on_calling_model(&run)
                            .await
                            .expect_err("completed")
                            .outcome,
                        RunOutcome::Completed
                    );
                });
                let mut events = Vec::new();
                while let Ok(event) = streams.control.try_recv() {
                    events.push(event);
                }
                while let Ok(event) = streams.data.try_recv() {
                    events.push(event);
                }
                events.sort_by_key(RunEvent::seq);
                let discarded_turn = events
                    .iter()
                    .find_map(|event| match event.payload() {
                        EventPayload::ModelResponseDiscarded { turn } => Some(turn.clone()),
                        _ => None,
                    })
                    .expect("discard marker");
                let marker = events
                    .iter()
                    .position(|event| {
                        matches!(event.payload(), EventPayload::ModelResponseDiscarded { .. })
                    })
                    .unwrap();
                let mut text = String::new();
                for (index, event) in events.iter().enumerate() {
                    if let EventPayload::AssistantTextDelta(fragment) = event.payload()
                        && fragment.turn == discarded_turn
                    {
                        assert!(index < marker, "discarded text must precede its marker");
                        text.push_str(&fragment.text);
                    }
                }
                assert_eq!(text, "first last");
                let replacement = events.iter().position(|event| matches!(event.payload(), EventPayload::ModelRequestStarted { turn, .. } if *turn != discarded_turn)).expect("replacement request");
                assert!(marker < replacement);
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(
                            event.payload(),
                            EventPayload::ModelResponseDiscarded { .. }
                        ))
                        .count(),
                    1
                );
            }
        }
    }

    #[test]
    fn incomplete_and_partial_failed_responses_feed_host_note_without_candidates() {
        for incomplete in [true, false] {
            let mut limits = Limits::m0_test();
            if !incomplete {
                limits.max_model_turns_per_run = 1;
            }
            let bed = make_bed(limits, true, vec![stop_turn("recovered")], None, None);
            let run = run_id("partial-recovery");
            test_rt().block_on(async {
                let active = active_for(&run);
                let original = active.conversation.clone();
                install(&bed, active).await;
                let mut events = vec![
                    ProviderEvent::TextDelta {
                        item_key: "partial".into(),
                        text: "unfinished response".into(),
                    },
                    ProviderEvent::ToolCallReady(
                        CallCandidate::new("candidate", "reference", "host_read", "{}").unwrap(),
                    ),
                ];
                events.push(if incomplete {
                    ProviderEvent::TurnFinished(TurnFinished::new(
                        FinishReason::Incomplete,
                        Usage::new(None, None, UsageFinality::Final),
                        None,
                    ))
                } else {
                    ProviderEvent::Failed(
                        AgentError::new(
                            ErrorCategory::Timeout,
                            "provider response interrupted",
                            RetryGuidance::RetryAfterBackoff,
                        )
                        .unwrap(),
                    )
                });
                let mut prefix = StreamedPrefix::default();
                bed.runtime
                    .publish_provisional(&run, &turn_id("partial"), events[0].clone(), &mut prefix)
                    .await;
                assert!(matches!(
                    bed.runtime
                        .ingest_model_batch(&run, &turn_id("partial"), events, &mut prefix)
                        .await,
                    Ok(RunState::CallingModel)
                ));
                assert_eq!(
                    inspect(&bed, |active| active.conversation.clone()).await,
                    Some(original)
                );
                assert_eq!(
                    inspect(&bed, |active| (
                        active.calls_used,
                        active.call_seq,
                        active.queue.len()
                    ))
                    .await,
                    Some((0, 0, 0))
                );
                assert_eq!(
                    bed.runtime
                        .on_calling_model(&run)
                        .await
                        .expect_err("completed")
                        .outcome,
                    RunOutcome::Completed
                );
                assert_eq!(bed.read.execution_count(), 0);
                assert_eq!(
                    inspect(&bed, |active| active.response_recovery_note).await,
                    Some(None),
                    "an accepted response clears the abandoned-invocation notice"
                );
                if !incomplete {
                    assert_eq!(
                        inspect(&bed, |active| (
                            active.turns_used,
                            active.access_failure_attempts
                        ))
                        .await,
                        Some((1, 0))
                    );
                }
            });
            let requests = bed.provider.requests.lock().unwrap();
            assert_eq!(requests.len(), 1, "no replay of the failed invocation");
            assert!(requests[0].context_note().contains("discarded"));
            assert!(!format!("{:?}", requests[0].conversation()).contains("unfinished response"));
        }
    }

    #[test]
    fn repeated_partial_access_failures_share_increasing_backoff_without_spending_turns() {
        assert_eq!(
            (0..8)
                .map(|attempt| provider_recovery_delay(attempt).as_secs())
                .collect::<Vec<_>>(),
            vec![1, 2, 5, 10, 30, 60, 60, 60]
        );
        assert_eq!(provider_recovery_delay(usize::MAX).as_secs(), 60);
        let failure = || {
            vec![
                ProviderEvent::TextDelta {
                    item_key: "partial".into(),
                    text: "private partial output".into(),
                },
                ProviderEvent::Failed(
                    AgentError::new(
                        ErrorCategory::Protocol,
                        "private transport detail",
                        RetryGuidance::RetryAfterBackoff,
                    )
                    .unwrap(),
                ),
            ]
        };
        let mut limits = Limits::m0_test();
        limits.max_model_turns_per_run = 1;
        let bed = make_bed(
            limits,
            true,
            vec![failure(), failure(), stop_turn("recovered")],
            None,
            None,
        );
        let capture = DiagnosticCapture::default();
        let runtime = bed.runtime.clone().with_diagnostic_writer(capture.clone());
        let run = run_id("partial-backoff");
        test_rt().block_on(async {
            install(&bed, active_for(&run)).await;
            for attempt in 1..=2 {
                assert!(matches!(
                    runtime.on_calling_model(&run).await,
                    Ok(RunState::CallingModel)
                ));
                assert_eq!(
                    inspect(&bed, |active| (
                        active.turns_used,
                        active.access_failure_attempts
                    ))
                    .await,
                    Some((0, attempt))
                );
            }
            assert_eq!(
                runtime
                    .on_calling_model(&run)
                    .await
                    .expect_err("completed")
                    .outcome,
                RunOutcome::Completed
            );
            assert_eq!(
                inspect(&bed, |active| (
                    active.turns_used,
                    active.access_failure_attempts
                ))
                .await,
                Some((1, 0))
            );
        });
        let log = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(log.contains("attempt=1 delay_seconds=1 error_category=Protocol"));
        assert!(log.contains("attempt=2 delay_seconds=2 error_category=Protocol"));
        assert!(!log.contains("private"));
        let requests = bed.provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_ne!(
            requests[0], requests[1],
            "recovery is a fresh request, not a replay"
        );
        assert!(requests[1].context_note().contains("discarded"));
    }

    #[test]
    fn explicit_cancel_dominates_terminal_until_finalization_but_internal_cancel_does_not() {
        for explicit in [true, false] {
            let mut bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
            let run = run_id("terminal-arbitration");
            test_rt().block_on(async {
                let mut active = active_for(&run);
                active.cancelled = explicit;
                active.token.cancel();
                active.terminal = Some(RunOutcome::Completed);
                install(&bed, active).await;
                bed.runtime
                    .set_terminal(
                        &run,
                        RunOutcome::LimitReached,
                        Some(runtime_error(
                            ErrorCategory::ResourceLimit,
                            "stream overflow",
                        )),
                    )
                    .await;
                if explicit {
                    assert_eq!(
                        bed.runtime.take_terminal(&run).await,
                        (RunOutcome::Cancelled, None)
                    );
                }
                // Cancellation arriving after take_terminal must still win in finish_run.
                bed.runtime
                    .finish_run(
                        &run,
                        RunOutcome::LimitReached,
                        Some(runtime_error(
                            ErrorCategory::ResourceLimit,
                            "stream overflow",
                        )),
                    )
                    .await;
                let event = bed.control.recv().await.unwrap();
                let EventPayload::RunFinished(finished) = event.payload() else {
                    panic!("terminal event")
                };
                assert_eq!(
                    finished.outcome(),
                    if explicit {
                        RunOutcome::Cancelled
                    } else {
                        RunOutcome::LimitReached
                    }
                );
                if !explicit {
                    assert_eq!(
                        finished.error().unwrap().category(),
                        ErrorCategory::ResourceLimit
                    );
                }
            });
        }
    }

    #[test]
    fn explicit_cancellation_preserves_unconfirmed_worker_diagnostic_through_finalization() {
        let mut bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
        let run = run_id("cancel-diagnostic");
        test_rt().block_on(async {
            let mut active = active_for(&run);
            active.cancelled = true;
            install(&bed, active).await;
            let error = runtime_error(ErrorCategory::Cancelled, "provider termination unconfirmed");
            bed.runtime
                .set_terminal(&run, RunOutcome::Cancelled, Some(error.clone()))
                .await;
            assert_eq!(
                bed.runtime.take_terminal(&run).await,
                (RunOutcome::Cancelled, Some(error.clone()))
            );
            // A stale non-cancellation result cannot replace either the user's
            // cancellation or the retained diagnostic about the live worker.
            bed.runtime
                .finish_run(
                    &run,
                    RunOutcome::Failed,
                    Some(runtime_error(ErrorCategory::Internal, "stale failure")),
                )
                .await;
            let event = bed.control.recv().await.unwrap();
            let EventPayload::RunFinished(finished) = event.payload() else {
                panic!("terminal event")
            };
            assert_eq!(finished.outcome(), RunOutcome::Cancelled);
            assert_eq!(finished.error(), Some(&error));
        });
    }

    #[test]
    fn repeated_output_limit_recovery_stops_after_one_retry() {
        let mut limits = Limits::m0_test();
        limits.max_model_turns_per_run = 3;
        let bed = make_bed(limits, true, Vec::new(), None, None);
        let run = run_id("repeated-output-limit");
        test_rt().block_on(async {
            install(&bed, active_for(&run)).await;
            for used in 1..=2 {
                let result = bed
                    .runtime
                    .ingest_model_batch(
                        &run,
                        &turn_id("limited"),
                        output_limit_turn(),
                        &mut StreamedPrefix::default(),
                    )
                    .await;
                if used == 1 {
                    assert!(matches!(result, Ok(RunState::CallingModel)));
                } else {
                    assert_limit(result, "model output limit reached");
                }
                assert_eq!(inspect(&bed, |active| active.turns_used).await, Some(used));
                assert_eq!(inspect(&bed, |active| active.calls_used).await, Some(0));
            }
        });
    }

    #[derive(Clone, Default)]
    struct DiagnosticCapture(Arc<StdMutex<Vec<u8>>>);

    impl std::io::Write for DiagnosticCapture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn diagnostic_routing_protects_terminal_and_tolerates_failed_writer() {
        let mut bytes = Vec::new();
        diagnostic_to(format_args!("metadata"), &mut bytes, true);
        assert!(bytes.is_empty());
        diagnostic_to(format_args!("metadata"), &mut bytes, false);
        assert_eq!(bytes, b"metadata\n");
        diagnostic_to(format_args!("metadata"), &mut std::io::sink(), true);

        struct Broken;
        impl std::io::Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("private failure detail"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        diagnostic_to(format_args!("metadata"), &mut Broken, false);
        let bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
        let _runtime = bed.runtime.clone().with_diagnostic_writer(Broken);
        assert!(bed.shared.diagnostic_writer.lock().unwrap().is_none());
    }

    #[test]
    fn diagnostic_writers_are_runtime_local() {
        let a = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
        let b = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
        let capture_a = DiagnosticCapture::default();
        let capture_b = DiagnosticCapture::default();
        let _a = a.runtime.clone().with_diagnostic_writer(capture_a.clone());
        let _b = b.runtime.clone().with_diagnostic_writer(capture_b.clone());
        a.shared.diagnostic(format_args!("runtime-a"));
        b.shared.diagnostic(format_args!("runtime-b"));
        let a = String::from_utf8(capture_a.0.lock().unwrap().clone()).unwrap();
        let b = String::from_utf8(capture_b.0.lock().unwrap().clone()).unwrap();
        assert!(a.contains("runtime-a") && !a.contains("runtime-b"));
        assert!(b.contains("runtime-b") && !b.contains("runtime-a"));
    }

    #[test]
    fn output_limit_diagnostics_correlate_run_turn_and_terminal_without_content() {
        let bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
        let capture = DiagnosticCapture::default();
        let runtime = bed.runtime.clone().with_diagnostic_writer(capture.clone());
        let run = run_id("diagnostic-run");
        test_rt().block_on(async {
            let mut active = active_for(&run);
            active
                .conversation
                .push(ModelContextItem::user_text("private-prompt").unwrap());
            install(&bed, active).await;
            let mut events = vec![
                ProviderEvent::TextDelta {
                    item_key: "private-item".into(),
                    text: "private-response".into(),
                },
                ProviderEvent::ReasoningDelta {
                    text: "private-reasoning".into(),
                },
                ProviderEvent::ToolCallReady(
                    CallCandidate::new(
                        "private-candidate",
                        "private-provider-reference",
                        "host_read",
                        r#"{"value":"private-payload-and-credential"}"#,
                    )
                    .unwrap(),
                ),
            ];
            events.extend(output_limit_turn());
            assert!(matches!(
                runtime
                    .ingest_model_batch(
                        &run,
                        &turn_id("diagnostic-turn"),
                        events,
                        &mut StreamedPrefix::default()
                    )
                    .await,
                Ok(RunState::CallingModel)
            ));
            runtime
                .finish_run(
                    &run,
                    RunOutcome::LimitReached,
                    Some(runtime_error(
                        ErrorCategory::ResourceLimit,
                        "model output limit reached",
                    )),
                )
                .await;
        });
        let log = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(log.contains("model output limit: run=run-diagnostic-run turn=turn-diagnostic-turn text_bytes=16 reasoning_bytes=17"), "{log}");
        assert!(log.contains("recovery_attempted=true"));
        assert!(log.contains("run finished run=run-diagnostic-run outcome=LimitReached error_category=Some(ResourceLimit)"));
        assert!(log.contains("source_persistence=Ephemeral turns=1 calls=0 elapsed_ms="));
        assert!(
            log.contains(
                "output_limit_recovery_used=true data_dropped=false terminal_delivery=Sent"
            )
        );
        for private in [
            "private-item",
            "private-response",
            "private-reasoning",
            "private-prompt",
            "private-candidate",
            "private-provider-reference",
            "private-payload-and-credential",
            "model output limit reached",
        ] {
            assert!(!log.contains(private), "diagnostics must exclude content");
        }
    }

    #[test]
    fn output_limit_recovery_never_allocates_calls_or_replays_partial_context() {
        let bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
        let run = run_id("output-limit-discard");
        test_rt().block_on(async {
            let active = active_for(&run);
            let conversation = active.conversation.clone();
            install(&bed, active).await;
            let mut events = vec![
                ProviderEvent::TextDelta {
                    item_key: "partial".into(),
                    text: "unfinished".into(),
                },
                ProviderEvent::ReasoningDelta {
                    text: "private unfinished reasoning".into(),
                },
                ProviderEvent::ToolCallDelta {
                    item_key: "partial-call".into(),
                    assembled_bytes: 9,
                },
                ProviderEvent::ToolCallReady(
                    CallCandidate::new("candidate", "ref", "host_read", "{\"partial\":").unwrap(),
                ),
            ];
            events.extend(output_limit_turn());
            assert!(matches!(
                bed.runtime
                    .ingest_model_batch(
                        &run,
                        &turn_id("truncated"),
                        events,
                        &mut StreamedPrefix::default()
                    )
                    .await,
                Ok(RunState::CallingModel)
            ));
            let state = bed.shared.state.lock().await;
            let active = state.active.as_ref().unwrap();
            assert!(active.output_limit_recovery_used);
            assert_eq!(active.turns_used, 1);
            assert_eq!(active.call_seq, 0);
            assert_eq!(active.calls_used, 0);
            assert!(active.queue.is_empty());
            assert!(active.current.is_none());
            assert!(active.pending.is_empty());
            assert_eq!(active.conversation, conversation);
            assert_eq!(active.source_cursor, 0);
            assert!(active.context_note.is_empty());
            assert!(active.continuation.is_none());
            assert_eq!(
                active.last_usage,
                Some(Usage::new(Some(3), Some(4), UsageFinality::Final))
            );
            assert_eq!(bed.read.execution_count(), 0);
        });
    }

    #[test]
    fn consecutive_task_output_limits_stop_after_one_recovery() {
        let bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
        let run = run_id("output-limit-consecutive");
        test_rt().block_on(async {
            install(&bed, active_for(&run)).await;
            assert!(matches!(
                bed.runtime
                    .ingest_model_batch(
                        &run,
                        &turn_id("first-limit"),
                        output_limit_turn(),
                        &mut StreamedPrefix::default(),
                    )
                    .await,
                Ok(RunState::CallingModel)
            ));
            let terminal = bed
                .runtime
                .ingest_model_batch(
                    &run,
                    &turn_id("second-limit"),
                    output_limit_turn(),
                    &mut StreamedPrefix::default(),
                )
                .await
                .expect_err("second consecutive limit must terminate the run");
            assert_eq!(terminal.outcome, RunOutcome::LimitReached);
            assert_eq!(
                terminal.error.unwrap().category(),
                ErrorCategory::ResourceLimit
            );
            assert_eq!(
                inspect(&bed, |active| active.task_output_limit_recovery_attempts).await,
                Some(1)
            );
            assert!(
                inspect(&bed, |active| active.output_limit_recovery_used)
                    .await
                    .unwrap()
            );
        });
    }

    #[test]
    fn output_limit_recovery_respects_opaque_state_turn_budget_and_controls() {
        for guard in [
            "opaque",
            "response-opaque",
            "incompatible",
            "turns",
            "cancelled",
            "deadline",
        ] {
            let limits = Limits::m0_test();
            let bed = make_bed(limits, true, Vec::new(), None, None);
            let run = run_id("output-limit-guard");
            let mut active = active_for(&run);
            match guard {
                "opaque" => {
                    active.continuation = Some(
                        ContinuationData::new(
                            nexus_core::DEFAULT_ADAPTER_IDENTITY,
                            "cov-profile",
                            vec![1, 2],
                        )
                        .unwrap(),
                    )
                }
                "turns" => active.turns_used = limits.max_model_turns_per_run - 1,
                "cancelled" => active.token.cancel(),
                "deadline" => active.deadline = StdInstant::now() - Duration::from_secs(1),
                "response-opaque" | "incompatible" => {}
                _ => unreachable!(),
            }
            let continuation = active.continuation.clone();
            test_rt().block_on(async {
                install(&bed, active).await;
                let events = if matches!(guard, "response-opaque" | "incompatible") {
                    vec![ProviderEvent::TurnFinished(nexus_core::TurnFinished::new(
                        FinishReason::OutputLimit,
                        Usage::new(Some(3), Some(4), UsageFinality::Final),
                        Some(
                            ContinuationData::new(
                                if guard == "incompatible" {
                                    "wrong-adapter"
                                } else {
                                    nexus_core::DEFAULT_ADAPTER_IDENTITY
                                },
                                "cov-profile",
                                vec![3, 4],
                            )
                            .unwrap(),
                        ),
                    ))]
                } else {
                    output_limit_turn()
                };
                let result = bed
                    .runtime
                    .ingest_model_batch(
                        &run,
                        &turn_id("guard"),
                        events,
                        &mut StreamedPrefix::default(),
                    )
                    .await;
                match guard {
                    "cancelled" => assert_cancelled(result),
                    "deadline" => assert_limit(
                        result,
                        "run duration exhausted before accepting provider response",
                    ),
                    "incompatible" => {
                        let Err(terminal) = result else {
                            panic!("incompatible continuation must fail");
                        };
                        assert_eq!(terminal.outcome, RunOutcome::Failed);
                        assert_eq!(
                            terminal.error.unwrap().category(),
                            ErrorCategory::UnsupportedCapability
                        );
                    }
                    _ => assert_limit(result, "model output limit reached"),
                }
                assert_eq!(
                    inspect(&bed, |active| active.continuation.clone()).await,
                    Some(continuation)
                );
                assert_eq!(inspect(&bed, |active| active.calls_used).await, Some(0));
                assert_eq!(bed.provider.call_count(), 0);
            });
        }
    }

    #[test]
    fn recovered_output_limit_rechecks_controls_before_next_request() {
        for cancel in [true, false] {
            let bed = make_bed(
                Limits::m0_test(),
                true,
                vec![stop_turn("unreachable")],
                None,
                None,
            );
            let run = run_id("output-limit-next-request");
            test_rt().block_on(async {
                install(&bed, active_for(&run)).await;
                assert!(matches!(
                    bed.runtime
                        .ingest_model_batch(
                            &run,
                            &turn_id("recovery"),
                            output_limit_turn(),
                            &mut StreamedPrefix::default()
                        )
                        .await,
                    Ok(RunState::CallingModel)
                ));
                {
                    let mut state = bed.shared.state.lock().await;
                    let active = state.active.as_mut().unwrap();
                    if cancel {
                        active.token.cancel();
                    } else {
                        active.deadline = StdInstant::now() - Duration::from_secs(1);
                    }
                }
                let result = bed.runtime.on_calling_model(&run).await;
                if cancel {
                    assert_cancelled(result);
                } else {
                    assert_limit(result, "run duration exhausted");
                }
                assert_eq!(bed.provider.call_count(), 0);
            });
        }
    }

    #[test]
    fn invalid_output_limit_batch_does_not_enable_recovery() {
        let mut limits = Limits::m0_test();
        limits.max_arg_assembly_bytes = 8;
        let bed = make_bed(limits, true, Vec::new(), None, None);
        let run = run_id("invalid-output-limit");
        test_rt().block_on(async {
            install(&bed, active_for(&run)).await;
            let mut events = vec![ProviderEvent::ToolCallDelta {
                item_key: "oversized".into(),
                assembled_bytes: 9,
            }];
            events.extend(output_limit_turn());
            let result = bed
                .runtime
                .ingest_model_batch(
                    &run,
                    &turn_id("invalid"),
                    events,
                    &mut StreamedPrefix::default(),
                )
                .await;
            assert!(matches!(
                result,
                Err(Terminal {
                    outcome: RunOutcome::Failed,
                    ..
                })
            ));
            assert_eq!(
                inspect(&bed, |active| active.output_limit_recovery_used).await,
                Some(false)
            );
            assert_eq!(inspect(&bed, |active| active.turns_used).await, Some(0));
            assert_eq!(bed.provider.call_count(), 0);
        });
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
                access: ToolAccess::Mutation,
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
            let model_request = bed
                .control
                .try_recv()
                .expect("request metadata is published before provider dispatch");
            assert!(matches!(
                model_request.payload(),
                EventPayload::ModelRequestStarted { .. }
            ));
            let response = bed
                .control
                .try_recv()
                .expect("response validation metadata follows the provider batch");
            assert!(matches!(
                response.payload(),
                EventPayload::ModelRequestFinished { response_validated: true, .. }
            ));
            let usage = bed
                .control
                .try_recv()
                .expect("the ingested turn published its usage update");
            assert!(matches!(usage.payload(), EventPayload::UsageUpdated(_)));
            assert!(bed.control.try_recv().is_err());
            assert_eq!(
                inspect(&bed, |active| (active.next_seq, active.last_seq, active.data_dropped))
                    .await,
                Some((4, Some(3), false)),
                "request metadata, one text delta, usage, and response metadata hold sequences 0..3: \
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
    fn a_vanished_control_consumer_retires_the_undeliverable_event() {
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
            bed.shared.outbox_events.store(1, Ordering::SeqCst);
            bed.shared
                .outbox_bytes
                .store(control_event_bytes(&buffered), Ordering::SeqCst);

            // The only consumer disappears while an event is committed to the
            // outbox. The flusher retires undeliverable ownership and classifies
            // the channel as closed rather than as saturation.
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
            assert!(
                outbox.is_empty(),
                "permanently undeliverable events are retired"
            );
            assert!(!bed.shared.flushing.load(Ordering::SeqCst));
            drop(outbox);
            assert_eq!(bed.shared.outbox_events.load(Ordering::SeqCst), 0);
            assert_eq!(bed.shared.outbox_bytes.load(Ordering::SeqCst), 0);
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
    fn closed_control_transport_discards_undeliverable_events_without_rescheduling() {
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
            bed.shared.outbox_events.store(2, Ordering::SeqCst);
            bed.shared.outbox_bytes.store(
                control_event_bytes(&first) + control_event_bytes(&second),
                Ordering::SeqCst,
            );
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
            assert!(
                outbox.is_empty(),
                "undeliverable events are retired on permanent closure"
            );
            drop(outbox);
            assert_eq!(bed.shared.outbox_events.load(Ordering::SeqCst), 0);
            assert_eq!(bed.shared.outbox_bytes.load(Ordering::SeqCst), 0);
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
    fn closing_during_finalization_retires_in_flight_and_queued_control_events() {
        let mut limits = Limits::m0_test();
        limits.event_control_capacity = 1;
        let bed = make_bed(limits, true, Vec::new(), None, None);
        let run = run_id("close-during-finalization");
        let rt = test_rt();
        rt.block_on(async {
            install(&bed, active_for(&run)).await;
            let filler = RunEvent::new(session_id(), run.clone(), 0, usage_payload());
            bed.shared
                .control_tx
                .try_reserve()
                .expect("single channel slot accepts filler")
                .send(filler);

            let inflight = RunEvent::new(session_id(), run.clone(), 1, usage_payload());
            bed.shared
                .outbox
                .lock()
                .expect("outbox is readable")
                .push_back(inflight.clone());
            bed.shared.outbox_events.store(1, Ordering::SeqCst);
            bed.shared
                .outbox_bytes
                .store(control_event_bytes(&inflight), Ordering::SeqCst);

            let flusher = tokio::spawn(flush_control(bed.shared.clone()));
            tokio::time::timeout(FLUSH_WAIT, async {
                while !bed
                    .shared
                    .outbox
                    .lock()
                    .expect("outbox is readable")
                    .is_empty()
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("flusher took ownership of the in-flight event");

            bed.runtime
                .finish_run(&run, RunOutcome::Completed, None)
                .await;
            assert_eq!(bed.shared.outbox_events.load(Ordering::SeqCst), 2);
            drop(bed.control);
            tokio::time::timeout(FLUSH_WAIT, flusher)
                .await
                .expect("flusher observes closure")
                .expect("flusher task completes");

            assert!(
                bed.shared
                    .outbox
                    .lock()
                    .expect("outbox readable")
                    .is_empty()
            );
            assert_eq!(bed.shared.outbox_events.load(Ordering::SeqCst), 0);
            assert_eq!(bed.shared.outbox_bytes.load(Ordering::SeqCst), 0);
            let state = bed.shared.state.lock().await;
            let finished = state.last.as_ref().expect("logical terminal is retained");
            assert!(!finished.terminal_delivered);
            assert!(
                finished.truncated,
                "undeliverable control data is observable"
            );
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

    #[test]
    fn tool_deadline_expiring_during_intent_persistence_never_dispatches_or_ends_run() {
        struct IntentStore(Gate);
        impl SessionStore for IntentStore {
            fn list_sessions(
                &self,
                _: usize,
            ) -> Result<Vec<nexus_core::SessionMetadata>, AgentError> {
                Ok(Vec::new())
            }
            fn load_session(&self, _: &SessionId) -> Result<SessionCheckpoint, AgentError> {
                unreachable!("test installs an active run")
            }
            fn save_checkpoint(&mut self, _: &SessionCheckpoint) -> Result<(), AgentError> {
                Ok(())
            }
            fn record_intent(&mut self, _: &ToolIntentRecord) -> Result<(), AgentError> {
                self.0.wait();
                Ok(())
            }
            fn record_outcome(&mut self, _: &ToolOutcomeRecord) -> Result<(), AgentError> {
                Ok(())
            }
        }
        let (gate, mut handle) = gate();
        let read = Arc::new(ScriptedTool::new("host_read", None));
        let mut limits = Limits::m0_test();
        limits.per_tool_timeout = Duration::from_millis(50);
        let (runtime, mut streams) = Runtime::try_new_with_session_store(
            RuntimeConfig {
                limits,
                policy: Policy::m0_test(),
                has_approval_handler: false,
            },
            Arc::new(ScriptedProvider::new(Vec::new())),
            vec![ToolRegistration::read(read.clone())],
            ModelInstructions::default(),
            Box::new(IntentStore(gate)),
        )
        .unwrap();
        let run = run_id("intent-tool-timeout");
        let call = read_call(&run, call_id("not-dispatched"));
        let mut active = active_for(&run);
        active.current = Some(current_call(&call, None));
        test_rt().block_on(async {
            runtime.shared.state.lock().await.active = Some(active);
            let (step, ()) = tokio::join!(runtime.on_executing(&run), async {
                handle.entered.recv().await.unwrap();
                tokio::time::sleep(Duration::from_millis(100)).await;
                handle.release.send(()).unwrap();
            });
            assert!(matches!(step, Ok(RunState::RecordingResult)));
            assert_eq!(read.execution_count(), 0);
            assert!(
                streams.control.try_recv().is_err(),
                "no ToolStarted before dispatch"
            );
            let state = runtime.shared.state.lock().await;
            let active = state.active.as_ref().unwrap();
            let (_, outcome) = active.outcome_slot.as_ref().unwrap();
            assert_eq!(outcome.status(), ExecutionStatus::TimedOut);
            assert_eq!(outcome.effect(), EffectState::NotStarted);
            assert_eq!(outcome.evidence(), Evidence::HostObserved);
            assert!(
                active.force_terminal.is_none(),
                "only the call deadline expired"
            );
            assert!(!active.current.as_ref().unwrap().dispatched);
            assert!(state.quarantine.is_empty());
        });
    }

    #[test]
    fn session_store_records_intent_before_dispatch_and_actual_outcome_afterward() {
        let directory = std::env::temp_dir().join(format!(
            "nexus-runtime-session-store-{}-{}",
            std::process::id(),
            next_incarnation()
        ));
        let mut store = nexus_session::FileSessionStore::open(&directory).unwrap();
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let read = Arc::new(ScriptedTool::new("host_read", None));
        let run = run_id("persistent-tool");
        let mut active = active_for(&run);
        let checkpoint = SessionCheckpoint::new(
            active.session.clone(),
            STORE_FORMAT_REVISION,
            0,
            Vec::new(),
            active.profile.clone(),
        )
        .unwrap();
        store.initialize_session(&checkpoint).unwrap();
        // This fixture starts after model admission; its context is outside
        // this operation-record-focused test.
        active.source_cursor = 1;
        let (runtime, _streams) = Runtime::try_new_with_session_store(
            RuntimeConfig {
                limits: Limits::m0_test(),
                policy: Policy::m0_test(),
                has_approval_handler: true,
            },
            provider,
            vec![ToolRegistration::read(read.clone())],
            ModelInstructions::default(),
            Box::new(store),
        )
        .unwrap();
        let call = read_call(&run, call_id("persistent-tool"));
        active.current = Some(current_call(&call, None));
        test_rt().block_on(async {
            runtime.shared.state.lock().await.active = Some(active);
            assert!(matches!(
                runtime.on_executing(&run).await,
                Ok(RunState::RecordingResult)
            ));
            assert_eq!(read.execution_count(), 1);
            assert!(matches!(
                runtime.on_recording(&run).await,
                Ok(RunState::Preparing)
            ));
        });
        drop(runtime);

        let reopened = nexus_session::FileSessionStore::open(&directory).unwrap();
        let snapshot = std::fs::read_to_string(directory.join("session-store.json")).unwrap();
        assert!(snapshot.contains("run-persistent-tool"));
        assert!(snapshot.contains("call-persistent-tool"));
        assert!(snapshot.contains("succeeded"));
        assert!(snapshot.contains("known_applied"));
        drop(reopened);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn runtime_persists_user_and_completed_assistant_source_items() {
        let directory = std::env::temp_dir().join(format!(
            "nexus-runtime-session-history-{}-{}",
            std::process::id(),
            next_incarnation()
        ));
        let store = nexus_session::FileSessionStore::open(&directory).unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![stop_turn("durable answer")]));
        let (runtime, mut streams) = Runtime::try_new_with_session_store(
            RuntimeConfig {
                limits: Limits::m0_test(),
                policy: Policy::m0_test(),
                has_approval_handler: true,
            },
            provider,
            Vec::new(),
            ModelInstructions::default(),
            Box::new(store),
        )
        .unwrap();

        let command = SubmitCommand::new(
            RequestId::new("req-durable-history").unwrap(),
            SessionId::new("sess-1").unwrap(),
            "do work",
            "test-profile",
        )
        .unwrap();
        test_rt().block_on(async {
            assert_eq!(
                runtime.submit(command).await.reply(),
                CommandReply::Accepted
            );
            let finished = loop {
                let event = streams.control.recv().await.unwrap();
                if let EventPayload::RunFinished(finished) = event.payload() {
                    break finished.clone();
                }
            };
            assert_eq!(finished.outcome(), RunOutcome::Completed);
            assert_eq!(finished.persistence(), PersistenceState::Saved);
        });
        drop(runtime);

        let reopened = nexus_session::FileSessionStore::open(&directory).unwrap();
        let session = SessionId::new("sess-1").unwrap();
        let records = reopened.load_source_records(&session, 0, 10).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(
            records[0].item(),
            &ModelContextItem::user_text("do work").unwrap()
        );
        assert_eq!(
            records[1].item(),
            &ModelContextItem::assistant_text("item-0", "durable answer").unwrap()
        );
        drop(reopened);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn explicit_list_and_restore_load_bounded_history_without_execution() {
        let directory = std::env::temp_dir().join(format!(
            "nexus-runtime-restore-{}-{}",
            std::process::id(),
            next_incarnation()
        ));
        let session = SessionId::new("sess-recover").unwrap();
        let old_run = run_id("interrupted");
        let call = read_call(&old_run, call_id("unfinished"));
        let source = vec![
            ModelContextItem::user_text("perform a read").unwrap(),
            ModelContextItem::assistant_call("item-call", "provider-ref", call.clone()).unwrap(),
        ];
        let mut store = nexus_session::FileSessionStore::open(&directory).unwrap();
        store
            .initialize_session(
                &SessionCheckpoint::new(
                    session.clone(),
                    STORE_FORMAT_REVISION,
                    0,
                    Vec::new(),
                    "old-profile",
                )
                .unwrap(),
            )
            .unwrap();
        store
            .append_source_items(&session, &old_run, &source)
            .unwrap();

        let provider = Arc::new(ScriptedProvider::new(vec![stop_turn("continued")]));
        let tool = Arc::new(ScriptedTool::new("host_read", None));
        let (runtime, mut streams) = Runtime::try_new_with_session_store(
            RuntimeConfig {
                limits: Limits::m0_test(),
                policy: Policy::m0_test(),
                has_approval_handler: true,
            },
            provider.clone(),
            vec![ToolRegistration::read(tool.clone())],
            ModelInstructions::default(),
            Box::new(store),
        )
        .unwrap();
        test_rt().block_on(async {
            let listed = runtime
                .handle(Command::ListSessions(
                    ListSessionsCommand::new(RequestId::new("req-list-recovered").unwrap(), 4)
                        .unwrap(),
                ))
                .await
                .0;
            assert_eq!(listed.reply(), CommandReply::Accepted);
            assert!(listed.has_sessions());
            assert_eq!(listed.sessions().len(), 1);
            assert_eq!(listed.sessions()[0].session, session);
            assert_eq!(provider.call_count(), 0);

            let restored = runtime
                .handle(Command::RestoreSession(RestoreSessionCommand {
                    request: RequestId::new("req-restore-recovered").unwrap(),
                    session: session.clone(),
                }))
                .await
                .0;
            assert_eq!(restored.reply(), CommandReply::Accepted);
            assert_eq!(restored.restored_session(), Some(&session));
            assert!(restored.restored_history().starts_with(&source));
            assert!(matches!(
                restored.restored_history().last(),
                Some(ModelContextItem::ToolResult { outcome, .. })
                    if outcome.effect() == EffectState::Unknown
            ));
            assert_eq!(restored.restored_omitted_items(), 0);
            assert_eq!(provider.call_count(), 0, "restore must not call the model");
            assert_eq!(tool.execution_count(), 0, "restore must not execute tools");
            {
                let state = runtime.shared.state.lock().await;
                assert_eq!(state.history.len(), 1);
                let restored_items = &state.history.front().unwrap().items;
                assert_eq!(restored_items.len(), 3);
                assert!(matches!(
                    restored_items[1],
                    ModelContextItem::AssistantCall { .. }
                ));
                assert!(matches!(
                    &restored_items[2],
                    ModelContextItem::ToolResult { outcome, .. }
                        if outcome.status() == ExecutionStatus::Interrupted
                            && outcome.effect() == EffectState::Unknown
                            && outcome.evidence() == Evidence::Uncertain
                ));
            }

            let submitted = runtime
                .submit(
                    SubmitCommand::new(
                        RequestId::new("req-continue-recovered").unwrap(),
                        session.clone(),
                        "continue safely",
                        "current-profile",
                    )
                    .unwrap(),
                )
                .await;
            assert_eq!(submitted.reply(), CommandReply::Accepted);
            while let Some(event) = streams.control.recv().await {
                if event.is_terminal() {
                    break;
                }
            }
            assert_eq!(provider.call_count(), 1);
            assert_eq!(tool.execution_count(), 0, "old call is never replayed");
        });
        drop(runtime);
        let reopened = nexus_session::FileSessionStore::open(&directory).unwrap();
        assert_eq!(reopened.last_source_sequence(&session).unwrap(), 4);
        drop(reopened);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn runtimes_share_one_serialized_session_store_handle() {
        let directory = std::env::temp_dir().join(format!(
            "nexus-runtime-shared-store-{}-{}",
            std::process::id(),
            next_incarnation()
        ));
        let store = nexus_session::FileSessionStore::open(&directory).unwrap();
        let handle = SessionStoreHandle::new(Box::new(store));
        let (first, mut streams) = Runtime::try_new_with_session_store_handle(
            RuntimeConfig {
                limits: Limits::m0_test(),
                policy: Policy::m0_test(),
                has_approval_handler: true,
            },
            Arc::new(ScriptedProvider::new(vec![stop_turn("saved")])),
            Vec::new(),
            ModelInstructions::default(),
            handle.clone(),
        )
        .unwrap();
        let (second, _streams) = Runtime::try_new_with_session_store_handle(
            RuntimeConfig {
                limits: Limits::m0_test(),
                policy: Policy::m0_test(),
                has_approval_handler: true,
            },
            Arc::new(ScriptedProvider::new(Vec::new())),
            Vec::new(),
            ModelInstructions::default(),
            handle.clone(),
        )
        .unwrap();
        test_rt().block_on(async {
            let response = first
                .submit(
                    SubmitCommand::new(
                        RequestId::new("req-shared-store").unwrap(),
                        SessionId::new("sess-shared").unwrap(),
                        "persist me",
                        "profile",
                    )
                    .unwrap(),
                )
                .await;
            assert_eq!(response.reply(), CommandReply::Accepted);
            while let Some(event) = streams.control.recv().await {
                if event.is_terminal() {
                    break;
                }
            }
            let listed = second
                .handle(Command::ListSessions(
                    ListSessionsCommand::new(RequestId::new("req-shared-list").unwrap(), 10)
                        .unwrap(),
                ))
                .await
                .0;
            assert_eq!(listed.reply(), CommandReply::Accepted);
            assert_eq!(listed.sessions().len(), 1);
            assert_eq!(listed.sessions()[0].session.as_str(), "sess-shared");
        });
        drop(first);
        drop(second);
        drop(handle);
        let reopened = nexus_session::FileSessionStore::open(&directory).unwrap();
        assert_eq!(
            reopened
                .last_source_sequence(&SessionId::new("sess-shared").unwrap())
                .unwrap(),
            2
        );
        drop(reopened);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stored_source_survives_summary_truncation_and_hard_context_projection() {
        for mode in 0..3 {
            let directory = std::env::temp_dir().join(format!(
                "nexus-runtime-store-compaction-{}-{}",
                std::process::id(),
                next_incarnation()
            ));
            let run = run_id("store-compaction");
            let mut active = active_for(&run);
            active.conversation = vec![
                ModelContextItem::user_text("old task").unwrap(),
                ModelContextItem::assistant_text("old-answer", "old source history ".repeat(400))
                    .unwrap(),
                ModelContextItem::user_text("current task").unwrap(),
            ];
            active.history_len = 2;
            active.inherited_history = true;
            active.source_cursor = active.conversation.len();
            let original = active.conversation.clone();
            let session = active.session.clone();
            let mut store = nexus_session::FileSessionStore::open(&directory).unwrap();
            store
                .initialize_session(
                    &SessionCheckpoint::new(
                        session.clone(),
                        STORE_FORMAT_REVISION,
                        0,
                        Vec::new(),
                        "profile",
                    )
                    .unwrap(),
                )
                .unwrap();
            store
                .append_source_items(&session, &run, &original)
                .unwrap();
            let provider = Arc::new(ScriptedProvider::new(if mode == 0 {
                vec![stop_turn("Earlier task facts."), stop_turn("new answer")]
            } else {
                vec![stop_turn("new answer")]
            }));
            let (runtime, _streams) = Runtime::try_new_with_session_store(
                RuntimeConfig {
                    limits: Limits::m0_test(),
                    policy: Policy::m0_test(),
                    has_approval_handler: true,
                },
                provider,
                Vec::new(),
                ModelInstructions::default(),
                Box::new(store),
            )
            .unwrap();
            if mode < 2 {
                active.context_management.window = Some(
                    nexus_core::ContextWindow::new(
                        if mode == 0 { 16_384 } else { 4096 },
                        nexus_core::ContextWindowSource::Configured,
                    )
                    .unwrap(),
                );
                active.context_management.policy = nexus_core::ContextManagementPolicy::new(
                    if mode == 0 { 20 } else { 65 },
                    10,
                    if mode == 0 {
                        nexus_core::ContextCompactionStrategy::Summarize
                    } else {
                        nexus_core::ContextCompactionStrategy::Truncate
                    },
                )
                .unwrap();
            } else {
                assert!(compact_context(&mut active, 2).is_ok());
            }
            test_rt().block_on(async {
                runtime.shared.state.lock().await.active = Some(active);
                if mode == 0 {
                    assert!(matches!(
                        runtime.on_calling_model(&run).await,
                        Ok(RunState::CallingModel)
                    ));
                }
                assert!(matches!(
                    runtime.on_calling_model(&run).await,
                    Err(Terminal {
                        outcome: RunOutcome::Completed,
                        ..
                    })
                ));
                assert!(
                    runtime
                        .shared
                        .state
                        .lock()
                        .await
                        .active
                        .as_ref()
                        .unwrap()
                        .persistence_error
                        .is_none()
                );
            });
            drop(runtime);
            let reopened = nexus_session::FileSessionStore::open(&directory).unwrap();
            let records = reopened.load_source_records(&session, 0, 10).unwrap();
            assert_eq!(records.len(), 4, "mode {mode}");
            assert_eq!(
                records[..3]
                    .iter()
                    .map(|record| record.item().clone())
                    .collect::<Vec<_>>(),
                original
            );
            assert_eq!(
                records[3].item(),
                &ModelContextItem::assistant_text("item-0", "new answer").unwrap()
            );
            drop(reopened);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn stored_unknown_exec_groups_are_summarized_without_changing_archived_sources() {
        let directory = std::env::temp_dir().join(format!(
            "nexus-runtime-store-unknown-summary-{}-{}",
            std::process::id(),
            next_incarnation()
        ));
        let run = run_id("stored-unknown-summary");
        let mut active = active_for(&run);
        active.conversation = vec![ModelContextItem::user_text("continue safely").unwrap()];
        let mut groups = Vec::new();
        for n in 0..5 {
            let old = n < 2;
            let call = ToolCall::new(
                run.clone(),
                turn_id(&format!("stored-exec-{n}")),
                call_id(&format!("stored-exec-{n}")),
                tool_id("host_exec"),
                NormalizedArgs::new(format!(r#"{{"command":"perform-operation-{n}"}}"#)).unwrap(),
            );
            let item = format!("stored-exec-item-{n}");
            let reference = format!("stored-exec-reference-{n}");
            let group = vec![
                ModelContextItem::assistant_reasoning(
                    format!("original reasoning {n}; ").repeat(if old { 400 } else { 1 }),
                )
                .unwrap(),
                ModelContextItem::assistant_call(&item, &reference, call.clone()).unwrap(),
                ModelContextItem::tool_result(
                    call.call().clone(),
                    item,
                    reference,
                    call.tool().clone(),
                    ToolOutcome::new(
                        if old {
                            ExecutionStatus::TimedOut
                        } else {
                            ExecutionStatus::Succeeded
                        },
                        if old {
                            EffectState::Unknown
                        } else {
                            EffectState::KnownApplied
                        },
                        if old {
                            Evidence::Uncertain
                        } else {
                            Evidence::HostObserved
                        },
                        format!("original output {n}; ").repeat(if old { 800 } else { 4 }),
                        false,
                    )
                    .unwrap(),
                )
                .unwrap(),
            ];
            active.conversation.extend(group.clone());
            groups.push(group);
        }
        active.source_cursor = active.conversation.len();
        active.context_management.window = Some(
            nexus_core::ContextWindow::new(32768, nexus_core::ContextWindowSource::Configured)
                .unwrap(),
        );
        active.context_management.policy = nexus_core::ContextManagementPolicy::new(
            50,
            10,
            nexus_core::ContextCompactionStrategy::Summarize,
        )
        .unwrap();
        let original = active.conversation.clone();
        let recent = groups[2..].concat();
        let session = active.session.clone();
        let mut store = nexus_session::FileSessionStore::open(&directory).unwrap();
        store
            .initialize_session(
                &SessionCheckpoint::new(
                    session.clone(),
                    STORE_FORMAT_REVISION,
                    0,
                    Vec::new(),
                    "profile",
                )
                .unwrap(),
            )
            .unwrap();
        store
            .append_source_items(&session, &run, &original)
            .unwrap();
        // Intentionally omit uncertainty from the model's summary: retained
        // execution evidence must be host-derived, not dependent on this text.
        let summary = "Earlier operations were attempted.";
        let provider = Arc::new(ScriptedProvider::new(
            (0..3)
                .map(|_| stop_turn(summary))
                .chain(std::iter::once(stop_turn("new answer")))
                .collect(),
        ));
        let (runtime, _streams) = Runtime::try_new_with_session_store(
            RuntimeConfig {
                limits: Limits::m0_test(),
                policy: Policy::m0_test(),
                has_approval_handler: true,
            },
            provider.clone(),
            Vec::new(),
            ModelInstructions::default(),
            Box::new(store),
        )
        .unwrap();
        test_rt().block_on(async {
            runtime.shared.state.lock().await.active = Some(active);
            loop {
                assert!(matches!(
                    runtime.on_calling_model(&run).await,
                    Ok(RunState::CallingModel)
                ));
                let pending = runtime
                    .shared
                    .state
                    .lock()
                    .await
                    .active
                    .as_ref()
                    .unwrap()
                    .pending_compaction
                    .is_some();
                if !pending {
                    break;
                }
                assert!(
                    provider.call_count() < 16,
                    "summary is bounded by turn budget"
                );
            }
            assert!(provider.call_count() > 1);
            {
                let mut state = runtime.shared.state.lock().await;
                let active = state.active.as_mut().unwrap();
                assert_eq!(active.derived_memory.as_deref(), Some(summary));
                assert_eq!(active.conversation[0], original[0]);
                assert_eq!(
                    &active.conversation[active.conversation.len() - recent.len()..],
                    recent.as_slice(),
                    "the newest three completed groups, including reasoning, stay verbatim"
                );
                assert!(active.conversation.iter().all(|item| !matches!(
                    item,
                    ModelContextItem::AssistantCall { call, .. }
                        if call.call() == &call_id("stored-exec-0")
                            || call.call() == &call_id("stored-exec-1")
                )));
                let evidence = active
                    .conversation
                    .iter()
                    .filter(|item| {
                        matches!(
                            item,
                            ModelContextItem::AssistantText { text, .. }
                                if text.as_str().contains("Unknown")
                                    && text.as_str().contains("do not blindly retry")
                        )
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                assert_eq!(evidence.len(), 2);
                for n in 0..2 {
                    assert!(evidence.iter().any(|item| matches!(
                        item,
                        ModelContextItem::AssistantText { text, .. }
                            if text.as_str().contains(&format!("stored-exec-{n}"))
                                && text.as_str().contains("host_exec")
                                && text.as_str().contains(&format!("perform-operation-{n}"))
                                && text.as_str().contains("TimedOut")
                                && text.as_str().contains("Uncertain")
                    )));
                }
                let request = prepare_context_request(
                    &runtime.shared,
                    active,
                    &turn_id("after-stored-summary"),
                )
                .unwrap_or_else(|terminal| panic!("request failed: {:?}", terminal.error));
                for item in &evidence {
                    assert!(request.conversation().contains(item));
                }
                assert_eq!(
                    &request.conversation()[request.conversation().len() - recent.len()..],
                    recent.as_slice()
                );
                assert!(active.pending_compaction.is_none());
                assert!(active.persistence_error.is_none());
                assert_eq!(active.source_cursor, active.conversation.len());
            }
            assert!(matches!(
                runtime.on_calling_model(&run).await,
                Err(Terminal {
                    outcome: RunOutcome::Completed,
                    ..
                })
            ));
            assert!(provider.call_count() > 2);
            assert!(
                runtime
                    .shared
                    .state
                    .lock()
                    .await
                    .active
                    .as_ref()
                    .unwrap()
                    .persistence_error
                    .is_none()
            );
        });
        drop(runtime);
        let reopened = nexus_session::FileSessionStore::open(&directory).unwrap();
        let records = reopened.load_source_records(&session, 0, 100).unwrap();
        assert_eq!(records.len(), original.len() + 1);
        assert_eq!(
            records[..original.len()]
                .iter()
                .map(|record| record.item().clone())
                .collect::<Vec<_>>(),
            original,
            "reopening preserves exact calls, large reasoning and full uncertain outputs"
        );
        assert_eq!(
            records.last().unwrap().item(),
            &ModelContextItem::assistant_text("item-0", "new answer").unwrap()
        );
        drop(reopened);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn interrupted_store_worker_is_owned_and_does_not_block_runtime_state() {
        for cancel in [true, false] {
            let bed = make_bed(Limits::m0_test(), true, Vec::new(), None, None);
            let run = run_id("blocked-store");
            let mut active = active_for(&run);
            let (gate, mut handle) = gate();
            test_rt().block_on(async {
                if !cancel {
                    active.deadline = StdInstant::now() + Duration::from_millis(100);
                }
                let token = active.token.clone();
                let wake = active.wake.clone();
                install(&bed, active).await;
                let worker = tokio::task::spawn_blocking(move || {
                    gate.wait();
                    Ok::<(), AgentError>(())
                });
                handle.entered.recv().await.unwrap();
                if cancel {
                    token.cancel();
                    wake.notify_one();
                }
                let result = tokio::time::timeout(
                    Duration::from_secs(1),
                    bed.runtime.await_store(&run, worker),
                )
                .await
                .unwrap();
                assert!(result.is_err());
                {
                    let state = bed.shared.state.lock().await;
                    assert_eq!(state.quarantine.len(), 1);
                    let active = state.active.as_ref().unwrap();
                    assert_eq!(
                        active.terminal,
                        Some(if cancel {
                            RunOutcome::Cancelled
                        } else {
                            RunOutcome::LimitReached
                        })
                    );
                    assert!(active.persistence_error.is_some());
                }
                assert!(bed.shared.store_uncertain.load(Ordering::SeqCst));
                handle.release.send(()).unwrap();
                tokio::time::timeout(Duration::from_secs(1), async {
                    while !bed.shared.state.lock().await.quarantine.is_empty() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                assert!(
                    bed.shared.store_uncertain.load(Ordering::SeqCst),
                    "late completion does not make an interrupted write safe to retry"
                );
            });
        }
    }

    #[test]
    fn blocked_input_save_reserves_admission_without_holding_state_lock() {
        struct AdmissionStore(Gate);
        impl SessionStore for AdmissionStore {
            fn list_sessions(
                &self,
                _: usize,
            ) -> Result<Vec<nexus_core::SessionMetadata>, AgentError> {
                Ok(Vec::new())
            }
            fn load_session(&self, _: &SessionId) -> Result<SessionCheckpoint, AgentError> {
                Err(runtime_error(
                    ErrorCategory::StorageFailure,
                    "session not found",
                ))
            }
            fn save_checkpoint(&mut self, _: &SessionCheckpoint) -> Result<(), AgentError> {
                Ok(())
            }
            fn initialize_session(&mut self, _: &SessionCheckpoint) -> Result<bool, AgentError> {
                self.0.wait();
                Ok(true)
            }
            fn append_source_items(
                &mut self,
                _: &SessionId,
                _: &RunId,
                _: &[ModelContextItem],
            ) -> Result<u64, AgentError> {
                Ok(1)
            }
            fn record_intent(&mut self, _: &ToolIntentRecord) -> Result<(), AgentError> {
                Ok(())
            }
            fn record_outcome(&mut self, _: &ToolOutcomeRecord) -> Result<(), AgentError> {
                Ok(())
            }
        }
        let (gate, mut gate_handle) = gate();
        let (runtime, _streams) = Runtime::try_new_with_session_store(
            RuntimeConfig {
                limits: Limits::m0_test(),
                policy: Policy::m0_test(),
                has_approval_handler: true,
            },
            Arc::new(ScriptedProvider::new(Vec::new())),
            Vec::new(),
            ModelInstructions::default(),
            Box::new(AdmissionStore(gate)),
        )
        .unwrap();
        test_rt().block_on(async {
            let submitter = runtime.clone();
            let command = SubmitCommand::new(
                RequestId::new("req-admission").unwrap(),
                session_id(),
                "input",
                "profile",
            )
            .unwrap();
            let duplicate = command.clone();
            let submitting = tokio::spawn(async move { submitter.submit(command).await });
            tokio::time::timeout(Duration::from_secs(1), gate_handle.entered.recv())
                .await
                .unwrap()
                .unwrap();
            let busy = tokio::time::timeout(Duration::from_secs(1), runtime.submit(duplicate))
                .await
                .unwrap();
            assert_eq!(busy.reply(), CommandReply::Busy);
            let run = busy.run().unwrap().clone();
            let cancelled = tokio::time::timeout(
                Duration::from_secs(1),
                runtime.cancel(CancelCommand {
                    request: RequestId::new("req-cancel-admission").unwrap(),
                    run,
                }),
            )
            .await
            .unwrap();
            assert_eq!(cancelled.reply(), CommandReply::Accepted);
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), submitting)
                    .await
                    .unwrap()
                    .unwrap()
                    .reply(),
                CommandReply::Rejected
            );
            assert!(runtime.shared.state.lock().await.active.is_none());
            assert!(runtime.shared.store_uncertain.load(Ordering::SeqCst));
            gate_handle.release.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(1), async {
                while !runtime.shared.state.lock().await.quarantine.is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        });
    }

    #[test]
    fn runtime_persists_tool_call_and_result_before_continuing() {
        let directory = std::env::temp_dir().join(format!(
            "nexus-runtime-session-tool-history-{}-{}",
            std::process::id(),
            next_incarnation()
        ));
        let store = nexus_session::FileSessionStore::open(&directory).unwrap();
        let read = Arc::new(ScriptedTool::new("host_read", None));
        let provider = Arc::new(ScriptedProvider::new(vec![
            {
                let mut events = vec![ProviderEvent::ToolCallReady(
                    CallCandidate::new(
                        "item-call",
                        "provider-ref",
                        "host_read",
                        r#"{"path":"src"}"#,
                    )
                    .unwrap(),
                )];
                events.push(ProviderEvent::TurnFinished(TurnFinished::new(
                    FinishReason::ToolCalls,
                    Usage::new(None, None, UsageFinality::Final),
                    None,
                )));
                events
            },
            stop_turn("finished after read"),
        ]));
        let (runtime, mut streams) = Runtime::try_new_with_session_store(
            RuntimeConfig {
                limits: Limits::m0_test(),
                policy: Policy::m0_test(),
                has_approval_handler: true,
            },
            provider,
            vec![ToolRegistration::read(read.clone())],
            ModelInstructions::default(),
            Box::new(store),
        )
        .unwrap();
        let command = SubmitCommand::new(
            RequestId::new("req-durable-tool-history").unwrap(),
            SessionId::new("sess-1").unwrap(),
            "read something",
            "test-profile",
        )
        .unwrap();

        test_rt().block_on(async {
            assert_eq!(
                runtime.submit(command).await.reply(),
                CommandReply::Accepted
            );
            let finished = loop {
                let event = streams.control.recv().await.unwrap();
                if let EventPayload::RunFinished(finished) = event.payload() {
                    break finished.clone();
                }
            };
            assert_eq!(finished.outcome(), RunOutcome::Completed);
        });
        assert_eq!(read.execution_count(), 1);
        drop(runtime);

        let reopened = nexus_session::FileSessionStore::open(&directory).unwrap();
        let session = SessionId::new("sess-1").unwrap();
        let records = reopened.load_source_records(&session, 0, 10).unwrap();
        assert_eq!(records.len(), 4);
        assert!(matches!(
            records[1].item(),
            ModelContextItem::AssistantCall { .. }
        ));
        assert!(matches!(
            records[2].item(),
            ModelContextItem::ToolResult { .. }
        ));
        assert_eq!(
            records[3].item(),
            &ModelContextItem::assistant_text("item-0", "finished after read").unwrap()
        );
        drop(reopened);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn session_store_intent_failure_prevents_tool_dispatch() {
        let directory = std::env::temp_dir().join(format!(
            "nexus-runtime-session-store-failure-{}-{}",
            std::process::id(),
            next_incarnation()
        ));
        let store = nexus_session::FileSessionStore::open(&directory).unwrap();
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let read = Arc::new(ScriptedTool::new("host_read", None));
        let (runtime, _streams) = Runtime::try_new_with_session_store(
            RuntimeConfig {
                limits: Limits::m0_test(),
                policy: Policy::m0_test(),
                has_approval_handler: true,
            },
            provider,
            vec![ToolRegistration::read(read.clone())],
            ModelInstructions::default(),
            Box::new(store),
        )
        .unwrap();

        std::fs::remove_dir_all(&directory).unwrap();
        std::fs::write(&directory, "block store directory creation").unwrap();
        let run = run_id("persistent-intent-failure");
        let call = read_call(&run, call_id("persistent-intent-failure"));
        let mut active = active_for(&run);
        active.current = Some(current_call(&call, None));
        test_rt().block_on(async {
            runtime.shared.state.lock().await.active = Some(active);
            assert!(matches!(
                runtime.on_executing(&run).await,
                Ok(RunState::RecordingResult)
            ));
            assert_eq!(read.execution_count(), 0);
            assert!(
                runtime
                    .shared
                    .state
                    .lock()
                    .await
                    .active
                    .as_ref()
                    .unwrap()
                    .persistence_error
                    .is_some()
            );
        });
        drop(runtime);
        std::fs::remove_file(directory).unwrap();
    }

    #[test]
    fn session_store_outcome_failure_preserves_observation_and_marks_save_failed() {
        let directory = std::env::temp_dir().join(format!(
            "nexus-runtime-session-outcome-failure-{}-{}",
            std::process::id(),
            next_incarnation()
        ));
        let mut store = nexus_session::FileSessionStore::open(&directory).unwrap();
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let read = Arc::new(ScriptedTool::new("host_read", None));
        let run = run_id("persistent-outcome-failure");
        let mut active = active_for(&run);
        let checkpoint = SessionCheckpoint::new(
            active.session.clone(),
            STORE_FORMAT_REVISION,
            0,
            Vec::new(),
            active.profile.clone(),
        )
        .unwrap();
        store.initialize_session(&checkpoint).unwrap();
        active.source_cursor = 1;
        let (runtime, mut streams) = Runtime::try_new_with_session_store(
            RuntimeConfig {
                limits: Limits::m0_test(),
                policy: Policy::m0_test(),
                has_approval_handler: true,
            },
            provider,
            vec![ToolRegistration::read(read.clone())],
            ModelInstructions::default(),
            Box::new(store),
        )
        .unwrap();

        let call = read_call(&run, call_id("persistent-outcome-failure"));
        active.current = Some(current_call(&call, None));
        test_rt().block_on(async {
            runtime.shared.state.lock().await.active = Some(active);
            assert!(matches!(
                runtime.on_executing(&run).await,
                Ok(RunState::RecordingResult)
            ));
            assert_eq!(read.execution_count(), 1);

            std::fs::remove_dir_all(&directory).unwrap();
            std::fs::write(&directory, "block store directory creation").unwrap();
            assert!(matches!(
                runtime.on_recording(&run).await,
                Ok(RunState::Finished)
            ));
            let state = runtime.shared.state.lock().await;
            let active = state.active.as_ref().unwrap();
            assert_eq!(active.terminal, Some(RunOutcome::Failed));
            assert!(active.persistence_error.is_some());
            assert_eq!(active.outcomes.len(), 1);
            assert_eq!(active.outcomes[0].effect, EffectState::KnownApplied);
            let terminal_error = active.terminal_error.clone();
            drop(state);
            runtime
                .finish_run(&run, RunOutcome::Failed, terminal_error)
                .await;
            let finished = loop {
                let event = streams
                    .control
                    .recv()
                    .await
                    .expect("terminal control event is delivered");
                if let EventPayload::RunFinished(finished) = event.payload() {
                    break finished.clone();
                }
            };
            assert_eq!(finished.persistence(), PersistenceState::SaveFailed);
            assert!(finished.persistence_error().is_some());
        });
        drop(runtime);
        std::fs::remove_file(directory).unwrap();
    }
}
