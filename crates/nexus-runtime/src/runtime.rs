//! Single-active-run execution loop: lifecycle, policy, ordered events.
//!
//! M0 scope: one active run (a second `Submit` gets an explicit `Busy`
//! reply), declared-order sequential tool dispatch, ephemeral persistence,
//! no session-store I/O. Core ports stay synchronous; this crate adapts them
//! at its boundary with the blocking pool, never leaking async types into
//! `nexus-core`.

// Rationale: tokio with only `rt`, `time`, `sync`, and `macros` features is
// the single async runtime for timers, deadlines, and cancellation. `time`
// provides monotonic sleeps/timeouts, `sync` provides the bounded
// data/control channels plus the mutex and cancellation notification, `rt`
// provides the executor and the blocking pool used to adapt core's
// synchronous `ProviderPort`/`ToolPort` without leaking async types into
// core (whose ports stay synchronous and data-only per the P2 review note),
// and `macros` supplies only `select!` for responsive waits. No net/io/fs
// features, no full-feature tokio, no second runtime.
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant as StdInstant};

use nexus_core::commands::{EventSequence, checked_next_sequence};
use nexus_core::{
    AgentError, ApprovalBinding, ApprovalId, ApprovalNotice, ApproveCommand, ApprovedScope,
    AssistantText, CallId, CancelCommand, Command, CommandReply, CommandResponse, ContinuationData,
    DenyCommand, EffectState, ErrorCategory, EventPayload, Evidence, ExecutionStatus, FinishReason,
    GetSnapshotCommand, Limits, ModelRequest, NormalizedArgs, OutcomeSummary, PersistenceState,
    ProviderContext, ProviderEvent, ProviderPort, RequestId, RetryGuidance, RunEvent, RunFinished,
    RunId, RunLifecycle, RunOutcome, SessionId, Snapshot, SubmitCommand, ToolCall, ToolContext,
    ToolFinishedInfo, ToolId, ToolOutcome, ToolPort, ToolStartedInfo, TurnId,
};
use tokio::sync::{Mutex, Notify, mpsc};

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
    /// Finite effective budgets; zero budgets are rejected at submit.
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
    policy: Policy,
    has_approval_handler: bool,
    provider: Arc<dyn ProviderPort + Send + Sync>,
    tools: HashMap<String, Arc<dyn ToolPort + Send + Sync>>,
    state: Mutex<State>,
    cancel: Notify,
    data_tx: mpsc::Sender<RunEvent>,
    control_tx: mpsc::Sender<RunEvent>,
    run_counter: AtomicU64,
}

struct State {
    active: Option<ActiveRun>,
    last: Option<FinishedRun>,
}

struct PendingApproval {
    binding: ApprovalBinding,
    decision: Option<bool>,
    notify: Arc<Notify>,
}

struct QueuedCall {
    call: ToolCall,
    needs_approval: bool,
}

struct CurrentCall {
    call: ToolCall,
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
    cancelled: bool,
    turn_seq: u64,
    call_seq: u64,
    approval_seq: u64,
    turns_used: u32,
    calls_used: u32,
    continuation: Option<ContinuationData>,
    queue: VecDeque<QueuedCall>,
    current: Option<CurrentCall>,
    pending: HashMap<ApprovalId, PendingApproval>,
    consumed: HashSet<ApprovalId>,
    outcomes: Vec<OutcomeSummary>,
    open_tools: HashSet<CallId>,
    outcome_slot: Option<(CallId, ToolOutcome)>,
    next_seq: EventSequence,
    last_seq: Option<EventSequence>,
    pending_text: Option<AssistantText>,
    data_dropped: bool,
    finished_sent: bool,
    terminal: Option<RunOutcome>,
}

struct FinishedRun {
    run: RunId,
    session: SessionId,
    outcome: RunOutcome,
    last_seq: Option<EventSequence>,
    outcomes: Vec<OutcomeSummary>,
    truncated: bool,
}

/// Publish-path rejection: stale identities never consume a sequence number.
/// A full control channel or sequence overflow ends the run with an explicit
/// limit outcome instead of silently dropping required events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublishReject {
    Stale,
    ControlFull,
    SeqOverflow,
}

impl From<PublishReject> for RunOutcome {
    fn from(reject: PublishReject) -> Self {
        match reject {
            PublishReject::Stale => RunOutcome::Cancelled,
            PublishReject::ControlFull | PublishReject::SeqOverflow => RunOutcome::LimitReached,
        }
    }
}

impl Runtime {
    /// Builds the runtime and hands out its two bounded event channels.
    /// There is exactly one consumer in the M0 baseline.
    pub fn new(
        config: RuntimeConfig,
        provider: Arc<dyn ProviderPort + Send + Sync>,
        tools: Vec<Arc<dyn ToolPort + Send + Sync>>,
    ) -> (Self, EventStreams) {
        let mut registry = HashMap::new();
        for tool in tools {
            registry.insert(tool.describe().id().name().to_owned(), tool);
        }
        let (data_tx, data_rx) = mpsc::channel(DATA_CAPACITY);
        let (control_tx, control_rx) = mpsc::channel(CONTROL_CAPACITY);
        let runtime = Self {
            shared: Arc::new(Shared {
                limits: config.limits,
                policy: config.policy,
                has_approval_handler: config.has_approval_handler,
                provider,
                tools: registry,
                state: Mutex::new(State {
                    active: None,
                    last: None,
                }),
                cancel: Notify::new(),
                data_tx,
                control_tx,
                run_counter: AtomicU64::new(1),
            }),
        };
        (
            runtime,
            EventStreams {
                data: data_rx,
                control: control_rx,
            },
        )
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

    /// Accepts a new run or rejects explicitly; a second submit while a run
    /// is active replies `Busy` without disturbing it.
    pub async fn submit(&self, command: SubmitCommand) -> CommandResponse {
        if self.shared.limits.validate().is_err() {
            return CommandResponse::new(command.request.clone(), CommandReply::Rejected, None);
        }
        let run_n = self.shared.run_counter.fetch_add(1, Ordering::SeqCst);
        let run = RunId::new(format!("run-{run_n}")).expect("counter id is valid");
        let mut state = self.shared.state.lock().await;
        if let Some(active) = state.active.as_ref() {
            return CommandResponse::new(
                command.request.clone(),
                CommandReply::Busy,
                Some(active.run.clone()),
            );
        }
        state.active = Some(ActiveRun {
            phase: RunState::Preparing,
            run: run.clone(),
            run_n,
            session: command.session.clone(),
            request: command.request.clone(),
            profile: command.profile.clone(),
            started: StdInstant::now(),
            cancelled: false,
            turn_seq: 0,
            call_seq: 0,
            approval_seq: 0,
            turns_used: 0,
            calls_used: 0,
            continuation: None,
            queue: VecDeque::new(),
            current: None,
            pending: HashMap::new(),
            consumed: HashSet::new(),
            outcomes: Vec::new(),
            open_tools: HashSet::new(),
            outcome_slot: None,
            next_seq: 0,
            last_seq: None,
            pending_text: None,
            data_dropped: false,
            finished_sent: false,
            terminal: None,
        });
        drop(state);
        let driver = self.clone();
        let response_run = run.clone();
        tokio::spawn(async move { driver.drive(run).await });
        CommandResponse::new(
            command.request.clone(),
            CommandReply::Accepted,
            Some(response_run),
        )
    }

    /// Stops future dispatch and signals active work. Idempotent: repeats
    /// never double-dispatch or double-record.
    pub async fn cancel(&self, command: CancelCommand) -> CommandResponse {
        let mut state = self.shared.state.lock().await;
        match state.active.as_mut() {
            Some(active) if active.run == command.run => {
                active.cancelled = true;
                self.shared.cancel.notify_one();
                for pending in active.pending.values() {
                    pending.notify.notify_one();
                }
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
    /// Duplicates and stale identities never dispatch again.
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
        let Some(pending) = active.pending.get_mut(&approval) else {
            return CommandResponse::new(request, CommandReply::StaleOrUnknownTarget, Some(run));
        };
        if pending.binding.run() != &run
            || pending.binding.call() != &call
            || pending.binding.is_expired(now)
            || pending.decision.is_some()
        {
            return CommandResponse::new(request, CommandReply::StaleOrUnknownTarget, Some(run));
        }
        pending.decision = Some(decision);
        pending.notify.notify_one();
        active.consumed.insert(approval);
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
        if let Err(outcome) = self
            .publish_control(&run, EventPayload::RunStarted { request })
            .await
        {
            self.finish_run(&run, outcome).await;
            return;
        }
        let outcome = self.run_loop(&run).await;
        self.finish_run(&run, outcome).await;
    }

    async fn run_loop(&self, run: &RunId) -> RunOutcome {
        let mut phase = RunState::Preparing;
        loop {
            let step: Result<RunState, RunOutcome> = match phase {
                RunState::Idle => Err(RunOutcome::Cancelled),
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
                Err(outcome) => {
                    self.set_terminal(run, outcome).await;
                    self.set_phase(run, RunState::Finished).await;
                    phase = RunState::Finished;
                }
            }
        }
    }

    async fn on_preparing(&self, run: &RunId) -> Result<RunState, RunOutcome> {
        let state = self.shared.state.lock().await;
        let Some(active) = state.active.as_ref() else {
            return Err(RunOutcome::Cancelled);
        };
        if &active.run != run {
            return Err(RunOutcome::Cancelled);
        }
        if active.cancelled {
            return Err(RunOutcome::Cancelled);
        }
        if self
            .shared
            .limits
            .check_run_elapsed(active.started.elapsed())
            .is_err()
        {
            return Err(RunOutcome::LimitReached);
        }
        if self
            .shared
            .limits
            .check_model_turns(active.turns_used)
            .is_err()
        {
            return Err(RunOutcome::LimitReached);
        }
        Ok(RunState::CallingModel)
    }

    async fn on_calling_model(&self, run: &RunId) -> Result<RunState, RunOutcome> {
        let (request, context, turn) = {
            let mut state = self.shared.state.lock().await;
            let Some(active) = state.active.as_mut() else {
                return Err(RunOutcome::Cancelled);
            };
            if &active.run != run || active.cancelled {
                return Err(RunOutcome::Cancelled);
            }
            let turn_n = active.turn_seq;
            active.turn_seq += 1;
            let turn = TurnId::new(format!("t{}-{turn_n}", active.run_n))
                .map_err(|_| RunOutcome::Failed)?;
            let enabled: Vec<ToolId> = self
                .shared
                .tools
                .values()
                .map(|tool| tool.describe().id().clone())
                .collect();
            let request = ModelRequest::new(
                active.run.clone(),
                turn.clone(),
                active.profile.clone(),
                enabled,
                active.continuation.clone(),
                self.shared.limits.max_tool_output_bytes,
            )
            .map_err(|_| RunOutcome::Failed)?;
            let context =
                ProviderContext::new(self.shared.limits.run_duration, active.cancelled, None);
            (request, context, turn)
        };
        let provider = self.shared.provider.clone();
        let answered = tokio::task::spawn_blocking(move || provider.stream(&request, &context));
        let events = answered.await.map_err(|_| RunOutcome::Failed)?;
        self.ingest_model_batch(run, &turn, events).await
    }

    async fn ingest_model_batch(
        &self,
        run: &RunId,
        turn: &TurnId,
        events: Vec<ProviderEvent>,
    ) -> Result<RunState, RunOutcome> {
        let terminal_count = events.iter().filter(|event| event.is_terminal()).count();
        let terminal_last = events.last().is_some_and(|event| event.is_terminal());
        if terminal_count != 1 || !terminal_last {
            return Err(RunOutcome::Failed);
        }
        let mut state = self.shared.state.lock().await;
        let Some(active) = state.active.as_mut() else {
            return Err(RunOutcome::Cancelled);
        };
        if &active.run != run {
            return Err(RunOutcome::Cancelled);
        }
        if active.cancelled {
            return Err(RunOutcome::Cancelled);
        }
        let mut candidates = Vec::new();
        let mut terminal = None;
        for event in &events {
            match event {
                ProviderEvent::TextDelta { item_key, text } => {
                    match AssistantText::new(turn.clone(), item_key.clone(), text.clone()) {
                        Ok(fragment) => {
                            buffer_text(active, run, fragment, &self.shared.data_tx);
                        }
                        Err(_) => active.data_dropped = true,
                    }
                }
                ProviderEvent::ToolCallDelta { item_key, .. } => {
                    emit_data(
                        active,
                        run,
                        &self.shared.data_tx,
                        EventPayload::ToolCallPreview {
                            item_key: item_key.clone(),
                        },
                    );
                }
                ProviderEvent::ToolCallReady(candidate) => candidates.push(candidate.clone()),
                ProviderEvent::Usage(usage) => {
                    publish_control(
                        active,
                        run,
                        &self.shared.data_tx,
                        &self.shared.control_tx,
                        EventPayload::UsageUpdated(*usage),
                    )
                    .map_err(RunOutcome::from)?;
                }
                ProviderEvent::TurnFinished(_) | ProviderEvent::Failed(_) => {
                    terminal = Some(event.clone());
                }
            }
        }
        flush_text(active, &self.shared.data_tx);
        let Some(last) = terminal else {
            return Err(RunOutcome::Failed);
        };
        match last {
            ProviderEvent::Failed(_) => Err(RunOutcome::Failed),
            ProviderEvent::TurnFinished(finished) => {
                publish_control(
                    active,
                    run,
                    &self.shared.data_tx,
                    &self.shared.control_tx,
                    EventPayload::UsageUpdated(finished.usage()),
                )
                .map_err(RunOutcome::from)?;
                active.continuation = finished.continuation().cloned();
                active.turns_used += 1;
                self.admit_candidates(active, turn, candidates)?;
                if active.queue.is_empty() {
                    match finished.reason() {
                        FinishReason::Stop => Err(RunOutcome::Completed),
                        FinishReason::ToolCalls => Ok(RunState::Preparing),
                        FinishReason::Refusal => Err(RunOutcome::Refused),
                        FinishReason::OutputLimit => Err(RunOutcome::LimitReached),
                        FinishReason::Incomplete => Err(RunOutcome::Failed),
                    }
                } else {
                    match finished.reason() {
                        FinishReason::Refusal => Err(RunOutcome::Refused),
                        FinishReason::OutputLimit => Err(RunOutcome::LimitReached),
                        FinishReason::Incomplete => Err(RunOutcome::Failed),
                        FinishReason::Stop | FinishReason::ToolCalls => {
                            Ok(RunState::ValidatingTools)
                        }
                    }
                }
            }
            _ => Err(RunOutcome::Failed),
        }
    }

    async fn on_validating(&self, run: &RunId) -> Result<RunState, RunOutcome> {
        let mut state = self.shared.state.lock().await;
        let Some(active) = state.active.as_mut() else {
            return Err(RunOutcome::Cancelled);
        };
        if &active.run != run || active.cancelled {
            return Err(RunOutcome::Cancelled);
        }
        if self
            .shared
            .limits
            .check_run_elapsed(active.started.elapsed())
            .is_err()
        {
            return Err(RunOutcome::LimitReached);
        }
        let Some(queued) = active.queue.pop_front() else {
            return Ok(RunState::Preparing);
        };
        if self
            .shared
            .limits
            .check_tool_calls_for_run(active.calls_used)
            .is_err()
        {
            return Err(RunOutcome::LimitReached);
        }
        if queued.needs_approval && !self.shared.has_approval_handler {
            let call = queued.call.call().clone();
            let outcome = denied_outcome("confirmation required and no approval handler exists");
            active.outcome_slot = Some((call, outcome));
            active.current = Some(CurrentCall {
                call: queued.call,
                binding: None,
            });
            return Ok(RunState::RecordingResult);
        }
        if !queued.needs_approval {
            active.current = Some(CurrentCall {
                call: queued.call,
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
            return Err(RunOutcome::LimitReached);
        }
        let approval_n = active.approval_seq;
        active.approval_seq += 1;
        let approval = ApprovalId::new(format!("a{}-{approval_n}", active.run_n))
            .map_err(|_| RunOutcome::Failed)?;
        let scope = ApprovedScope::new("m0-test-grant").map_err(|_| RunOutcome::Failed)?;
        let binding = ApprovalBinding::new(
            approval.clone(),
            active.run.clone(),
            queued.call.call().clone(),
            queued.call.tool().clone(),
            queued.call.args().clone(),
            scope,
            active.started.elapsed() + self.shared.limits.approval_expiry,
            self.shared.policy.revision(),
        );
        let summary = format!("run tool {}", queued.call.tool().name());
        let notice = ApprovalNotice::new(
            approval.clone(),
            queued.call.call().clone(),
            summary,
            binding.scope().as_str(),
            self.shared.limits.approval_expiry,
        )
        .map_err(|_| RunOutcome::Failed)?;
        active.pending.insert(
            approval.clone(),
            PendingApproval {
                binding: binding.clone(),
                decision: None,
                notify: Arc::new(Notify::new()),
            },
        );
        active.current = Some(CurrentCall {
            call: queued.call,
            binding: Some(binding),
        });
        publish_control(
            active,
            run,
            &self.shared.data_tx,
            &self.shared.control_tx,
            EventPayload::ApprovalRequired(notice),
        )
        .map_err(RunOutcome::from)?;
        Ok(RunState::AwaitingApproval)
    }

    async fn on_awaiting(&self, run: &RunId) -> Result<RunState, RunOutcome> {
        loop {
            let snapshot = {
                let state = self.shared.state.lock().await;
                let Some(active) = state.active.as_ref() else {
                    return Err(RunOutcome::Cancelled);
                };
                if &active.run != run {
                    return Err(RunOutcome::Cancelled);
                }
                let Some(current) = active.current.as_ref() else {
                    return Ok(RunState::ValidatingTools);
                };
                let Some(approval) = current.binding.as_ref().map(|binding| {
                    (
                        binding.approval().clone(),
                        binding.is_expired(active.started.elapsed()),
                    )
                }) else {
                    return Ok(RunState::ExecutingTool);
                };
                let pending = active
                    .pending
                    .get(&approval.0)
                    .map(|item| (item.notify.clone(), item.decision, approval.0.clone()));
                (
                    active.cancelled,
                    self.shared
                        .limits
                        .run_duration
                        .saturating_sub(active.started.elapsed()),
                    pending,
                    approval.1,
                )
            };
            let (cancelled, run_remaining, pending, expired) = snapshot;
            if cancelled {
                let mut state = self.shared.state.lock().await;
                if let Some(active) = state.active.as_mut()
                    && &active.run == run
                {
                    self.abandon_pending(active);
                    let call = current_call_id(active);
                    if let Some(call) = call {
                        active.outcome_slot = Some((
                            call,
                            cancelled_outcome("run cancelled while awaiting approval"),
                        ));
                    }
                }
                self.set_terminal(run, RunOutcome::Cancelled).await;
                return Err(RunOutcome::Cancelled);
            }
            if run_remaining.is_zero() {
                let mut state = self.shared.state.lock().await;
                if let Some(active) = state.active.as_mut()
                    && &active.run == run
                {
                    self.abandon_pending(active);
                    let call = current_call_id(active);
                    if let Some(call) = call {
                        active.outcome_slot = Some((
                            call,
                            timeout_outcome("run deadline passed while awaiting approval"),
                        ));
                    }
                }
                self.set_terminal(run, RunOutcome::LimitReached).await;
                return Err(RunOutcome::LimitReached);
            }
            let Some((notify, decision, approval_id)) = pending else {
                return Ok(RunState::ValidatingTools);
            };
            if expired {
                let mut state = self.shared.state.lock().await;
                if let Some(active) = state.active.as_mut()
                    && &active.run == run
                {
                    self.abandon_pending(active);
                    let call = current_call_id(active);
                    if let Some(call) = call {
                        active.outcome_slot = Some((call, denied_outcome("approval expired")));
                    }
                }
                return Ok(RunState::RecordingResult);
            }
            match decision {
                Some(true) => {
                    let mut state = self.shared.state.lock().await;
                    if let Some(active) = state.active.as_mut()
                        && &active.run == run
                    {
                        active.pending.remove(&approval_id);
                    }
                    return Ok(RunState::ExecutingTool);
                }
                Some(false) => {
                    let mut state = self.shared.state.lock().await;
                    if let Some(active) = state.active.as_mut()
                        && &active.run == run
                    {
                        active.pending.remove(&approval_id);
                        let call = current_call_id(active);
                        if let Some(call) = call {
                            active.outcome_slot = Some((call, denied_outcome("approval refused")));
                        }
                    }
                    return Ok(RunState::RecordingResult);
                }
                None => {
                    let approval_expiry = self.shared.limits.approval_expiry;
                    let wait_for = run_remaining.min(approval_expiry);
                    tokio::select! {
                        () = notify.notified() => {}
                        () = tokio::time::sleep(wait_for) => {}
                        () = self.shared.cancel.notified() => {}
                    }
                }
            }
        }
    }

    async fn on_executing(&self, run: &RunId) -> Result<RunState, RunOutcome> {
        let setup = {
            let mut state = self.shared.state.lock().await;
            let Some(active) = state.active.as_mut() else {
                return Err(RunOutcome::Cancelled);
            };
            if &active.run != run {
                return Err(RunOutcome::Cancelled);
            }
            let Some(current) = active.current.as_ref() else {
                return Ok(RunState::ValidatingTools);
            };
            let call = current.call.clone();
            let tool = match self.shared.tools.get(call.tool().name()) {
                Some(tool) => tool.clone(),
                None => {
                    active.outcome_slot =
                        Some((call.call().clone(), denied_outcome("unknown tool")));
                    return Ok(RunState::RecordingResult);
                }
            };
            if !tool.describe().id().is_compatible_with(call.tool()) {
                active.outcome_slot =
                    Some((call.call().clone(), denied_outcome("tool revision changed")));
                return Ok(RunState::RecordingResult);
            }
            let now = active.started.elapsed();
            if active.cancelled {
                active.outcome_slot = Some((
                    call.call().clone(),
                    cancelled_outcome("cancelled before dispatch"),
                ));
                drop(state);
                self.set_terminal(run, RunOutcome::Cancelled).await;
                return Err(RunOutcome::Cancelled);
            }
            if now >= self.shared.limits.run_duration {
                active.outcome_slot = Some((
                    call.call().clone(),
                    timeout_outcome("run deadline passed before dispatch"),
                ));
                drop(state);
                self.set_terminal(run, RunOutcome::LimitReached).await;
                return Err(RunOutcome::LimitReached);
            }
            if let Some(binding) = current.binding.clone()
                && verify_dispatch(
                    Some(&binding),
                    &call,
                    self.shared.policy.revision(),
                    now,
                    false,
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
                None => ApprovedScope::new("host-auto-read").map_err(|_| RunOutcome::Failed)?,
            };
            let tool_deadline = now + self.shared.limits.per_tool_timeout;
            let deadline = tool_deadline.min(self.shared.limits.run_duration);
            let context = ToolContext::new(
                self.shared.limits.max_tool_output_bytes,
                deadline,
                false,
                scope,
            )
            .map_err(|_| RunOutcome::Failed)?;
            publish_control(
                active,
                run,
                &self.shared.data_tx,
                &self.shared.control_tx,
                EventPayload::ToolStarted(ToolStartedInfo {
                    call: call.call().clone(),
                }),
            )
            .map_err(RunOutcome::from)?;
            active.open_tools.insert(call.call().clone());
            (call, tool, context, self.shared.limits.per_tool_timeout)
        };
        let (call, tool, context, tool_timeout) = setup;
        let call_id = call.call().clone();
        let worker = tokio::task::spawn_blocking(move || tool.execute(&call, &context));
        let outcome = match tokio::time::timeout(tool_timeout, worker).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => cancelled_outcome("tool task ended without an outcome"),
            Err(_) => timeout_outcome("tool deadline exceeded"),
        };
        let mut state = self.shared.state.lock().await;
        let Some(active) = state.active.as_mut() else {
            return Err(RunOutcome::Cancelled);
        };
        if &active.run != run {
            return Err(RunOutcome::Cancelled);
        }
        active.calls_used += 1;
        active.outcome_slot = Some((call_id, outcome));
        Ok(RunState::RecordingResult)
    }

    async fn on_recording(&self, run: &RunId) -> Result<RunState, RunOutcome> {
        let mut state = self.shared.state.lock().await;
        let Some(active) = state.active.as_mut() else {
            return Err(RunOutcome::Cancelled);
        };
        if &active.run != run {
            return Err(RunOutcome::Cancelled);
        }
        let Some((call, outcome)) = active.outcome_slot.take() else {
            return Ok(RunState::ValidatingTools);
        };
        active.open_tools.remove(&call);
        active.outcomes.push(OutcomeSummary {
            call: call.clone(),
            status: outcome.status(),
            effect: outcome.effect(),
            evidence: outcome.evidence(),
        });
        active.current = None;
        publish_control(
            active,
            run,
            &self.shared.data_tx,
            &self.shared.control_tx,
            EventPayload::ToolFinished(ToolFinishedInfo { call, outcome }),
        )
        .map_err(RunOutcome::from)?;
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

    /// Stores the terminal outcome, first recording any slotted call outcome
    /// so no decided result is lost on the way out.
    async fn set_terminal(&self, run: &RunId, outcome: RunOutcome) {
        let mut state = self.shared.state.lock().await;
        let Some(active) = state.active.as_mut() else {
            return;
        };
        if &active.run != run {
            return;
        }
        if let Some((call, decided)) = active.outcome_slot.take() {
            active.open_tools.remove(&call);
            active.outcomes.push(OutcomeSummary {
                call: call.clone(),
                status: decided.status(),
                effect: decided.effect(),
                evidence: decided.evidence(),
            });
            active.current = None;
            let _ = publish_control(
                active,
                run,
                &self.shared.data_tx,
                &self.shared.control_tx,
                EventPayload::ToolFinished(ToolFinishedInfo {
                    call,
                    outcome: decided,
                }),
            );
        }
        active.terminal = Some(outcome);
    }

    async fn take_terminal(&self, run: &RunId) -> RunOutcome {
        let state = self.shared.state.lock().await;
        match state.active.as_ref() {
            Some(active) if &active.run == run => active.terminal.unwrap_or(RunOutcome::Cancelled),
            _ => RunOutcome::Cancelled,
        }
    }

    fn abandon_pending(&self, active: &mut ActiveRun) {
        let approvals: Vec<ApprovalId> = active.pending.keys().cloned().collect();
        for approval in approvals {
            active.pending.remove(&approval);
            active.consumed.insert(approval);
        }
    }

    async fn publish_control(&self, run: &RunId, payload: EventPayload) -> Result<(), RunOutcome> {
        let mut state = self.shared.state.lock().await;
        let Some(active) = state.active.as_mut() else {
            return Err(RunOutcome::Cancelled);
        };
        publish_control(
            active,
            run,
            &self.shared.data_tx,
            &self.shared.control_tx,
            payload,
        )
        .map_err(RunOutcome::from)
    }

    /// Publishes the terminal event exactly once, then retires the run.
    /// Every `ToolStarted` still open receives an honest unknown outcome
    /// while the owner lives; nothing is fabricated after that.
    async fn finish_run(&self, run: &RunId, outcome: RunOutcome) {
        let mut state = self.shared.state.lock().await;
        let Some(mut active) = state.active.take() else {
            return;
        };
        if active.run != *run || active.finished_sent {
            state.active = Some(active);
            return;
        }
        for call in active.open_tools.drain().collect::<Vec<_>>() {
            let outcome = cancelled_outcome("run ended with tool work still open");
            active.outcomes.push(OutcomeSummary {
                call: call.clone(),
                status: outcome.status(),
                effect: outcome.effect(),
                evidence: outcome.evidence(),
            });
            let _ = publish_control(
                &mut active,
                run,
                &self.shared.data_tx,
                &self.shared.control_tx,
                EventPayload::ToolFinished(ToolFinishedInfo { call, outcome }),
            );
        }
        flush_text(&mut active, &self.shared.data_tx);
        let finished = RunFinished::new(outcome, PersistenceState::Ephemeral, None)
            .expect("ephemeral terminal record builds");
        let _ = publish_control(
            &mut active,
            run,
            &self.shared.data_tx,
            &self.shared.control_tx,
            EventPayload::RunFinished(finished),
        );
        active.finished_sent = true;
        active.phase = RunState::Finished;
        state.last = Some(FinishedRun {
            run: active.run.clone(),
            session: active.session.clone(),
            outcome,
            last_seq: active.last_seq,
            outcomes: active.outcomes.clone(),
            truncated: active.data_dropped,
        });
    }

    fn admit_candidates(
        &self,
        active: &mut ActiveRun,
        turn: &TurnId,
        candidates: Vec<nexus_core::CallCandidate>,
    ) -> Result<(), RunOutcome> {
        let run = active.run.clone();
        for (index, candidate) in candidates.into_iter().enumerate() {
            if self
                .shared
                .limits
                .check_tool_calls_for_turn(index as u32)
                .is_err()
            {
                return Err(RunOutcome::LimitReached);
            }
            let Some(registered) = self.shared.tools.get(candidate.tool_name()) else {
                let call = CallId::new(format!("c{}-{}", active.run_n, active.call_seq))
                    .map_err(|_| RunOutcome::Failed)?;
                active.call_seq += 1;
                let outcome = denied_outcome("unknown tool");
                active.outcomes.push(OutcomeSummary {
                    call: call.clone(),
                    status: outcome.status(),
                    effect: outcome.effect(),
                    evidence: outcome.evidence(),
                });
                publish_control(
                    active,
                    &run,
                    &self.shared.data_tx,
                    &self.shared.control_tx,
                    EventPayload::ToolFinished(ToolFinishedInfo { call, outcome }),
                )
                .map_err(RunOutcome::from)?;
                continue;
            };
            let tool_id = registered.describe().id().clone();
            let args = match NormalizedArgs::new(candidate.arguments_json()) {
                Ok(args) => args,
                Err(_) => {
                    let call = CallId::new(format!("c{}-{}", active.run_n, active.call_seq))
                        .map_err(|_| RunOutcome::Failed)?;
                    active.call_seq += 1;
                    let outcome = denied_outcome("invalid tool arguments");
                    active.outcomes.push(OutcomeSummary {
                        call: call.clone(),
                        status: outcome.status(),
                        effect: outcome.effect(),
                        evidence: outcome.evidence(),
                    });
                    publish_control(
                        active,
                        &run,
                        &self.shared.data_tx,
                        &self.shared.control_tx,
                        EventPayload::ToolFinished(ToolFinishedInfo { call, outcome }),
                    )
                    .map_err(RunOutcome::from)?;
                    continue;
                }
            };
            if self
                .shared
                .limits
                .check_arg_assembly_bytes(args.as_str().len())
                .is_err()
            {
                return Err(RunOutcome::LimitReached);
            }
            let call = CallId::new(format!("c{}-{}", active.run_n, active.call_seq))
                .map_err(|_| RunOutcome::Failed)?;
            active.call_seq += 1;
            active.queue.push_back(QueuedCall {
                call: ToolCall::new(
                    active.run.clone(),
                    turn.clone(),
                    call,
                    tool_id.clone(),
                    args,
                ),
                needs_approval: self.shared.policy.requires_approval(&tool_id),
            });
        }
        Ok(())
    }
}

fn current_call_id(active: &ActiveRun) -> Option<CallId> {
    active
        .current
        .as_ref()
        .map(|current| current.call.call().clone())
}

/// Dispatch-boundary check: mutable cancellation/deadline state plus the
/// exact approval tuple, rechecked immediately before execution, including
/// after any approval wait. A shorter path must never bypass this.
fn verify_dispatch(
    binding: Option<&ApprovalBinding>,
    call: &ToolCall,
    policy_revision: u32,
    now_elapsed: Duration,
    cancelled: bool,
) -> Result<(), AgentError> {
    if cancelled {
        return Err(dispatch_error(
            ErrorCategory::Cancelled,
            "cancelled before dispatch",
        ));
    }
    let Some(binding) = binding else {
        return Ok(());
    };
    binding.check_valid_for_dispatch(
        call.run(),
        call.call(),
        call.tool(),
        call.args(),
        policy_revision,
        now_elapsed,
    )
}

fn dispatch_error(category: ErrorCategory, message: &'static str) -> AgentError {
    AgentError::new(category, message, RetryGuidance::DoNotRetry)
        .expect("static safe dispatch message builds")
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

fn assign_seq(active: &mut ActiveRun) -> Result<EventSequence, PublishReject> {
    let seq = active.next_seq;
    active.next_seq = checked_next_sequence(seq).ok_or(PublishReject::SeqOverflow)?;
    active.last_seq = Some(seq);
    Ok(seq)
}

fn run_event(active: &ActiveRun, seq: EventSequence, payload: EventPayload) -> RunEvent {
    RunEvent::new(active.session.clone(), active.run.clone(), seq, payload)
}

/// Flushes coalesced text as one sequenced data event. A full data channel
/// drops presentation traffic and marks truncation; control flow continues.
fn flush_text(active: &mut ActiveRun, data_tx: &mpsc::Sender<RunEvent>) {
    let Some(fragment) = active.pending_text.take() else {
        return;
    };
    let Ok(seq) = assign_seq(active) else {
        active.data_dropped = true;
        return;
    };
    let event = run_event(active, seq, EventPayload::AssistantTextDelta(fragment));
    if data_tx.try_send(event).is_err() {
        active.data_dropped = true;
    }
}

/// Emits one data event after flushing coalesced text, preserving order.
fn emit_data(
    active: &mut ActiveRun,
    run: &RunId,
    data_tx: &mpsc::Sender<RunEvent>,
    payload: EventPayload,
) {
    if &active.run != run {
        return;
    }
    flush_text(active, data_tx);
    let Ok(seq) = assign_seq(active) else {
        active.data_dropped = true;
        return;
    };
    if data_tx.try_send(run_event(active, seq, payload)).is_err() {
        active.data_dropped = true;
    }
}

/// Buffers one text fragment, coalescing with the adjacent same-item tail.
/// Batching never crosses turn, item, or lifecycle boundaries.
fn buffer_text(
    active: &mut ActiveRun,
    run: &RunId,
    fragment: AssistantText,
    data_tx: &mpsc::Sender<RunEvent>,
) {
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
                emit_data(active, run, data_tx, EventPayload::AssistantTextDelta(tail));
                active.pending_text = Some(fragment);
            }
        }
        return;
    }
    emit_data(active, run, data_tx, EventPayload::AssistantTextDelta(tail));
    active.pending_text = Some(fragment);
}

/// Publishes one control event with a contiguous per-run sequence assigned
/// after batching. Control events are never coalesced and never silently
/// dropped: a full channel is an explicit limit outcome.
fn publish_control(
    active: &mut ActiveRun,
    run: &RunId,
    data_tx: &mpsc::Sender<RunEvent>,
    control_tx: &mpsc::Sender<RunEvent>,
    payload: EventPayload,
) -> Result<(), PublishReject> {
    if &active.run != run {
        return Err(PublishReject::Stale);
    }
    debug_assert!(crate::transport::is_control_payload(&payload));
    flush_text(active, data_tx);
    let seq = assign_seq(active)?;
    control_tx
        .try_send(run_event(active, seq, payload))
        .map_err(|_| PublishReject::ControlFull)?;
    Ok(())
}

fn snapshot_of_active(active: &ActiveRun) -> Option<Snapshot> {
    snapshot_view(
        &active.session,
        &active.run,
        active.last_seq,
        RunLifecycle::Active,
        active.pending.keys().cloned().collect(),
        active.outcomes.clone(),
        active.data_dropped,
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
    use nexus_core::Usage;
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

        fn tool_turn(candidates: Vec<nexus_core::CallCandidate>) -> Vec<ProviderEvent> {
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
        fn capabilities(&self) -> nexus_core::ProviderCapabilities {
            nexus_core::ProviderCapabilities {
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

    struct FakeTool {
        spec: nexus_core::ToolSpec,
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
                spec: nexus_core::ToolSpec::new(
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
        fn describe(&self) -> nexus_core::ToolSpec {
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
        // The terminal arrives on the control channel while presentation
        // traffic may still sit buffered in the data channel; all data
        // sends precede the terminal send, so a non-blocking drain after
        // the break collects every sequenced event without loss.
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

    fn candidate(tool: &str, args: &str) -> nexus_core::CallCandidate {
        nexus_core::CallCandidate::new("item-0", "prov-ref-0", tool, args).expect("valid")
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
    fn partial_call_deltas_never_execute() {
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
            assert_eq!(finished.outcome(), RunOutcome::Completed);
            assert!(tool_started_calls(&collected).is_empty());
            assert_eq!(bed.read_tool.executed.load(Ordering::SeqCst), 0);
            assert_eq!(bed.write_tool.executed.load(Ordering::SeqCst), 0);
            let previews = collected
                .data
                .iter()
                .filter(|event| matches!(event.payload(), EventPayload::ToolCallPreview { .. }))
                .count();
            assert_eq!(previews, 1);
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
            let (approval, call) = collected
                .control
                .iter()
                .find_map(|event| match event.payload() {
                    EventPayload::ApprovalRequired(notice) => {
                        Some((notice.approval.clone(), notice.call.clone()))
                    }
                    _ => None,
                })
                .expect("approval requested");
            assert_eq!(bed.write_tool.executed.load(Ordering::SeqCst), 0);
            let reply = bed
                .runtime
                .approve(ApproveCommand {
                    request: RequestId::new("req-decide").expect("valid"),
                    approval: approval.clone(),
                    run: run.clone(),
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
            let (approval, _) = collected
                .control
                .iter()
                .find_map(|event| match event.payload() {
                    EventPayload::ApprovalRequired(notice) => {
                        Some((notice.approval.clone(), notice.call.clone()))
                    }
                    _ => None,
                })
                .expect("approval requested");
            let other_call = CallId::new("c9-9").expect("valid");
            let reply = bed
                .runtime
                .approve(ApproveCommand {
                    request: RequestId::new("req-bad").expect("valid"),
                    approval: approval.clone(),
                    run: run.clone(),
                    call: other_call,
                })
                .await;
            assert_eq!(reply.reply(), CommandReply::StaleOrUnknownTarget);
            assert_eq!(bed.write_tool.executed.load(Ordering::SeqCst), 0);
            let (approval, call) = collected
                .control
                .iter()
                .find_map(|event| match event.payload() {
                    EventPayload::ApprovalRequired(notice) => {
                        Some((notice.approval.clone(), notice.call.clone()))
                    }
                    _ => None,
                })
                .expect("approval requested");
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
        let args = NormalizedArgs::new(r#"{"path":"src"}"#).expect("valid");
        let changed = NormalizedArgs::new(r#"{"path":"other"}"#).expect("valid");
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
        let admitted = ToolCall::new(
            run,
            TurnId::new("t1-0").expect("valid"),
            call,
            tool,
            changed,
        );
        let error = verify_dispatch(
            Some(&binding),
            &admitted,
            nexus_core::M0_REVISION,
            Duration::from_secs(1),
            false,
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
            let (approval, call) = collected
                .control
                .iter()
                .find_map(|event| match event.payload() {
                    EventPayload::ApprovalRequired(notice) => {
                        Some((notice.approval.clone(), notice.call.clone()))
                    }
                    _ => None,
                })
                .expect("approval requested");
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
    fn per_tool_timeout_records_unknown_without_rollback_claims() {
        let slow = Arc::new(FakeTool::slow("host_read", Duration::from_millis(500)));
        let provider = Arc::new(FakeProvider::new(vec![FakeProvider::tool_turn(vec![
            candidate("host_read", r#"{"path":"src"}"#),
        ])]));
        let mut limits = Limits::m0_test();
        limits.per_tool_timeout = Duration::from_millis(50);
        let tools: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![slow];
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
            let (collected, _) = collect_until_finished(&mut bed.data, &mut bed.control).await;
            let outcomes = tool_finished_map(&collected);
            assert_eq!(outcomes.len(), 1);
            let outcome = outcomes.values().next().expect("timeout recorded");
            assert_eq!(outcome.status(), ExecutionStatus::TimedOut);
            assert_eq!(outcome.effect(), EffectState::Unknown);
            assert_eq!(outcome.evidence(), Evidence::Uncertain);
        });
    }
}
