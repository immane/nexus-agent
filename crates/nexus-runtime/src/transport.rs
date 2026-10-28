//! Event transport: two bounded channels plus consume-side staleness checks.
//!
//! The data channel carries presentation traffic (text deltas, previews,
//! progress) and may coalesce adjacent text fragments for the same turn
//! item. The control channel carries approvals, outcomes, usage, and
//! terminal events, which are never coalesced and never silently dropped: a
//! full control channel ends the run with an explicit limit outcome. The
//! runtime assigns one shared contiguous per-run sequence after its own
//! batching. Capacities track the M0 lock table through [`Limits`].

use std::collections::BTreeMap;

use nexus_core::{EventPayload, Limits, RunEvent, RunId};
use tokio::sync::mpsc;

/// Bounded data-channel capacity in events (lock section 1).
pub const DATA_CAPACITY: usize = Limits::M0_TEST_EVENT_DATA_CAPACITY;
/// Bounded control-channel capacity in events (lock section 1).
pub const CONTROL_CAPACITY: usize = Limits::M0_TEST_EVENT_CONTROL_CAPACITY;
/// Default event-count budget for convenience consumers. Saturation returns
/// the unaccepted event; it is not evidence that a predecessor was lost.
pub const ORDERED_EVENT_BUFFER_CAPACITY: usize = DATA_CAPACITY + CONTROL_CAPACITY + 256;

/// Receivers for the two authoritative event channels. The runtime is the
/// sole publisher; there is exactly one consumer in the M0 baseline.
pub struct EventStreams {
    /// Presentation traffic; adjacent same-item text may be coalesced.
    pub data: mpsc::Receiver<RunEvent>,
    /// Approvals, outcomes, terminal events; never coalesced or dropped.
    pub control: mpsc::Receiver<RunEvent>,
}

/// Returns true when `payload` travels on the control channel.
#[must_use]
pub fn is_control_payload(payload: &EventPayload) -> bool {
    matches!(
        payload,
        EventPayload::RunStarted { .. }
            | EventPayload::ContextReduced { .. }
            | EventPayload::ApprovalRequired(_)
            | EventPayload::ToolStarted(_)
            | EventPayload::ToolFinished(_)
            | EventPayload::ModelRequestStarted { .. }
            | EventPayload::ModelRequestFinished { .. }
            | EventPayload::ModelResponseDiscarded { .. }
            | EventPayload::UsageUpdated(_)
            | EventPayload::RunFinished(_)
    )
}

/// Consume-path staleness check: frontends must reject updates whose owning
/// run is not the live one instead of applying them to the current run.
#[must_use]
pub fn event_is_live(event: &RunEvent, active_run: Option<&RunId>) -> bool {
    match active_run {
        Some(run) => event.run() == run,
        None => false,
    }
}

/// Bounded cross-channel sequence consumer shared by frontends.
///
/// It rejects foreign, duplicate, stale, and post-terminal traffic; emits
/// contiguous events immediately; and advances over missing sequence numbers
/// only when the caller explicitly flushes (for example after draining both
/// input channels on terminal or closure). Saturation returns ownership to
/// the caller, without advancing over a missing sequence.
/// Rendering, retention, reconnect, and closure policy remain caller-owned.
#[derive(Debug)]
pub struct OrderedEventConsumer {
    active: Option<RunId>,
    expected: u64,
    buffered: BTreeMap<u64, RunEvent>,
    capacity: usize,
    terminal_seq: Option<u64>,
    finalized: bool,
    gaps: u64,
    rejected: u64,
}

impl OrderedEventConsumer {
    /// Creates a consumer with a finite reorder budget and one additional
    /// slot reserved for its next contiguous predecessor.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            active: None,
            expected: 0,
            buffered: BTreeMap::new(),
            capacity: capacity.max(1),
            terminal_seq: None,
            finalized: false,
            gaps: 0,
            rejected: 0,
        }
    }

    /// Starts consuming one accepted run; previous buffered state is retired.
    pub fn adopt(&mut self, run: &RunId) {
        self.adopt_from(run, 0);
    }

    /// Starts consuming from a known first available sequence, used when a
    /// reconnect intentionally does not replay earlier stream records.
    pub fn adopt_from(&mut self, run: &RunId, first_sequence: u64) {
        self.active = Some(run.clone());
        self.expected = first_sequence;
        self.buffered.clear();
        self.terminal_seq = None;
        self.finalized = false;
    }

    /// Returns the currently adopted run, if any.
    #[must_use]
    pub fn active_run(&self) -> Option<&RunId> {
        self.active.as_ref()
    }

    /// Whether this run's terminal event has been emitted.
    #[must_use]
    pub fn is_finalized(&self) -> bool {
        self.finalized
    }

    /// Whether a terminal has arrived but is waiting for a sequence gap.
    #[must_use]
    pub fn has_terminal(&self) -> bool {
        self.terminal_seq.is_some() && !self.finalized
    }

    /// Whether out-of-order events are currently buffered.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.buffered.is_empty()
    }

    /// Number of currently buffered out-of-order events.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.buffered.len()
    }

    /// Number of missing sequence values explicitly skipped.
    #[must_use]
    pub fn gaps(&self) -> u64 {
        self.gaps
    }

    /// Number of rejected events.
    #[must_use]
    pub fn rejected(&self) -> u64 {
        self.rejected
    }

    /// Retains an event without advancing delivery. On saturation the caller
    /// retains ownership and must stop receiving or report a transport error.
    /// One extra slot is reserved for the missing predecessor.
    pub fn enqueue(&mut self, event: RunEvent) -> Result<(), Box<RunEvent>> {
        if self.finalized || self.active.as_ref() != Some(event.run()) {
            self.rejected = self.rejected.saturating_add(1);
            return Ok(());
        }
        let seq = event.seq();
        if seq < self.expected || self.terminal_seq.is_some_and(|terminal| seq > terminal) {
            self.rejected = self.rejected.saturating_add(1);
            return Ok(());
        }
        if self.buffered.contains_key(&seq) {
            self.rejected = self.rejected.saturating_add(1);
            return Ok(());
        }
        if self.buffered.len() >= self.capacity && seq != self.expected {
            return Err(Box::new(event));
        }
        if event.is_terminal() {
            if self.terminal_seq.is_some_and(|terminal| terminal != seq) {
                self.rejected = self.rejected.saturating_add(1);
                return Ok(());
            }
            self.terminal_seq = Some(seq);
            let after_terminal: Vec<u64> = self
                .buffered
                .range((seq.saturating_add(1))..)
                .map(|(buffered_seq, _)| *buffered_seq)
                .collect();
            for buffered_seq in after_terminal {
                self.buffered.remove(&buffered_seq);
                self.rejected = self.rejected.saturating_add(1);
            }
        }
        self.buffered.insert(seq, event);
        Ok(())
    }

    /// The next contiguous event, retained until acknowledged by `pop_ready`.
    pub fn peek_ready(&self) -> Option<&RunEvent> {
        self.buffered.get(&self.expected)
    }

    /// Lowest retained event, used to select the other FIFO channel when a
    /// bounded consumer must wait for a predecessor instead of reading ahead.
    pub fn first_pending(&self) -> Option<&RunEvent> {
        self.buffered.first_key_value().map(|(_, event)| event)
    }

    /// Next contiguous sequence required before delivery may advance.
    pub fn expected_sequence(&self) -> u64 {
        self.expected
    }

    /// Acknowledges delivery of exactly one contiguous event.
    pub fn pop_ready(&mut self) -> Option<RunEvent> {
        let event = self.buffered.remove(&self.expected)?;
        self.mark_emitted(&event);
        self.expected = self.expected.saturating_add(1);
        Some(event)
    }

    /// Adds an event and returns all newly deliverable events in sequence order.
    pub fn push(&mut self, event: RunEvent) -> Result<Vec<RunEvent>, Box<RunEvent>> {
        self.enqueue(event)?;
        Ok(self.take_ready())
    }

    /// Flushes buffered events in order and records any intervening gaps.
    pub fn flush(&mut self) -> Vec<RunEvent> {
        let mut ready = Vec::new();
        while let Some((&seq, _)) = self.buffered.iter().next() {
            if seq > self.expected {
                self.gaps = self.gaps.saturating_add(seq - self.expected);
                self.expected = seq;
            }
            let event = self.buffered.remove(&seq).expect("peeked key exists");
            self.mark_emitted(&event);
            ready.push(event);
            self.expected = self.expected.saturating_add(1);
            if self.finalized {
                self.rejected = self.rejected.saturating_add(self.buffered.len() as u64);
                self.buffered.clear();
                break;
            }
        }
        ready
    }

    fn take_ready(&mut self) -> Vec<RunEvent> {
        let mut ready = Vec::new();
        while let Some(event) = self.buffered.remove(&self.expected) {
            self.mark_emitted(&event);
            ready.push(event);
            self.expected = self.expected.saturating_add(1);
            if self.finalized {
                self.rejected = self.rejected.saturating_add(self.buffered.len() as u64);
                self.buffered.clear();
                break;
            }
        }
        ready
    }

    fn mark_emitted(&mut self, event: &RunEvent) {
        if event.is_terminal() {
            self.finalized = true;
            self.terminal_seq = Some(event.seq());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::{PersistenceState, RequestId, RunFinished, RunId, RunOutcome, SessionId};

    fn event(run: &str, seq: u64) -> RunEvent {
        RunEvent::new(
            SessionId::new("sess-1").expect("valid"),
            RunId::new(run).expect("valid"),
            seq,
            EventPayload::RunStarted {
                request: RequestId::new("req-1").expect("valid"),
            },
        )
    }

    fn terminal(run: &str, seq: u64) -> RunEvent {
        RunEvent::new(
            SessionId::new("sess-1").expect("valid"),
            RunId::new(run).expect("valid"),
            seq,
            EventPayload::RunFinished(
                RunFinished::new(RunOutcome::Completed, PersistenceState::Ephemeral, None)
                    .expect("valid terminal"),
            ),
        )
    }

    #[test]
    fn capacities_track_the_lock_table() {
        assert_eq!(DATA_CAPACITY, 1_024);
        assert_eq!(CONTROL_CAPACITY, 128);
    }

    #[test]
    fn stale_run_updates_are_not_live() {
        let current = RunId::new("run-2").expect("valid");
        assert!(event_is_live(&event("run-2", 0), Some(&current)));
        assert!(!event_is_live(&event("run-1", 9), Some(&current)));
        assert!(!event_is_live(&event("run-1", 9), None));
    }

    #[test]
    fn terminal_and_approval_payloads_are_control() {
        let started = event("run-1", 0);
        assert!(is_control_payload(started.payload()));
    }

    #[test]
    fn ordered_consumer_merges_channels_and_rejects_stale_duplicate_and_post_terminal() {
        let run = RunId::new("run-live").expect("valid");
        let mut consumer = OrderedEventConsumer::new(8);
        consumer.adopt(&run);

        assert!(consumer.push(event("run-live", 1)).unwrap().is_empty());
        let ready = consumer.push(event("run-live", 0)).unwrap();
        assert_eq!(ready.iter().map(RunEvent::seq).collect::<Vec<_>>(), [0, 1]);
        assert!(consumer.push(event("run-live", 1)).unwrap().is_empty());
        assert!(consumer.push(event("run-stale", 2)).unwrap().is_empty());

        assert!(consumer.push(terminal("run-live", 3)).unwrap().is_empty());
        assert!(consumer.has_terminal());
        let ready = consumer.push(event("run-live", 2)).unwrap();
        assert_eq!(ready.iter().map(RunEvent::seq).collect::<Vec<_>>(), [2, 3]);
        assert!(consumer.is_finalized());
        assert!(consumer.push(event("run-live", 4)).unwrap().is_empty());
        assert_eq!(consumer.rejected(), 3);
        assert_eq!(consumer.gaps(), 0);
    }

    #[test]
    fn ordered_consumer_flushes_missing_sequences_without_fabricating_events() {
        let run = RunId::new("run-flush").expect("valid");
        let mut consumer = OrderedEventConsumer::new(8);
        consumer.adopt(&run);
        assert!(consumer.push(event("run-flush", 2)).unwrap().is_empty());
        let ready = consumer.flush();
        assert_eq!(ready.iter().map(RunEvent::seq).collect::<Vec<_>>(), [2]);
        assert_eq!(consumer.gaps(), 2);
        assert!(consumer.push(event("run-flush", 1)).unwrap().is_empty());
        assert_eq!(consumer.rejected(), 1);
    }

    #[test]
    fn a_full_reorder_buffer_waits_for_a_predecessor_in_the_other_channel() {
        let run = RunId::new("run-full-buffer").expect("valid");
        let mut consumer = OrderedEventConsumer::new(2);
        consumer.adopt(&run);
        assert!(
            consumer
                .push(event("run-full-buffer", 1))
                .unwrap()
                .is_empty()
        );
        assert!(
            consumer
                .push(event("run-full-buffer", 2))
                .unwrap()
                .is_empty()
        );
        let ready = consumer.push(event("run-full-buffer", 0)).unwrap();
        assert_eq!(
            ready.iter().map(RunEvent::seq).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(consumer.gaps(), 0);
        assert!(!consumer.has_pending());
    }

    #[test]
    fn full_enqueue_preserves_ownership_and_reserves_room_for_the_predecessor() {
        let run = RunId::new("run-saturated").expect("valid");
        let mut consumer = OrderedEventConsumer::new(2);
        consumer.adopt(&run);
        consumer.enqueue(event("run-saturated", 1)).unwrap();
        consumer.enqueue(event("run-saturated", 2)).unwrap();

        let refused = terminal("run-saturated", 3);
        let returned = consumer.enqueue(refused.clone()).unwrap_err();
        assert_eq!(*returned, refused);
        assert_eq!(consumer.pending_len(), 2);
        assert!(!consumer.has_terminal());
        assert_eq!(consumer.gaps(), 0);
        assert_eq!(consumer.rejected(), 0);

        consumer.enqueue(event("run-saturated", 0)).unwrap();
        assert_eq!(consumer.pending_len(), 3, "missing predecessor reserve");
        for seq in 0..=2 {
            assert_eq!(consumer.pop_ready().unwrap().seq(), seq);
        }
        let ready = consumer.push(*returned).unwrap();
        assert_eq!(ready.iter().map(RunEvent::seq).collect::<Vec<_>>(), [3]);
        assert!(consumer.is_finalized());
        assert_eq!(consumer.gaps(), 0);
        assert_eq!(consumer.rejected(), 0);
    }

    #[test]
    fn saturated_push_returns_the_event_without_advancing_delivery() {
        let run = RunId::new("run-push-full").expect("valid");
        let mut consumer = OrderedEventConsumer::new(1);
        consumer.adopt(&run);
        assert!(consumer.push(event("run-push-full", 1)).unwrap().is_empty());
        let refused = event("run-push-full", 2);
        let returned = consumer.push(refused.clone()).unwrap_err();
        assert_eq!(*returned, refused);
        assert_eq!(consumer.pending_len(), 1);
        assert_eq!(consumer.gaps(), 0);
        let ready = consumer.push(event("run-push-full", 0)).unwrap();
        assert_eq!(ready.iter().map(RunEvent::seq).collect::<Vec<_>>(), [0, 1]);
        assert_eq!(consumer.push(*returned).unwrap()[0].seq(), 2);
        assert_eq!(consumer.rejected(), 0);
    }

    #[test]
    fn ordered_consumer_starts_reconnect_at_the_first_available_sequence() {
        let run = RunId::new("run-reconnect").expect("valid");
        let mut consumer = OrderedEventConsumer::new(8);
        consumer.adopt_from(&run, 7);
        let ready = consumer.push(event("run-reconnect", 7)).unwrap();
        assert_eq!(ready.iter().map(RunEvent::seq).collect::<Vec<_>>(), [7]);
        assert!(consumer.push(event("run-reconnect", 6)).unwrap().is_empty());
        assert_eq!(consumer.rejected(), 1);
    }
}

#[cfg(test)]
mod cov_transport_private {
    //! Unit coverage for transport internals: the exhaustive control/data
    //! payload split, the lock-table capacity cross-checks, and the proof
    //! that control classification never bypasses the consume-path staleness
    //! check.

    use std::time::Duration;

    use super::*;
    use nexus_core::{
        ApprovalId, ApprovalNotice, AssistantText, CallId, EffectState, Evidence, ExecutionStatus,
        PersistenceState, RequestId, RunFinished, RunOutcome, SessionId, ToolFinishedInfo,
        ToolOutcome, ToolProgress, ToolStartedInfo, TurnId, Usage, UsageFinality,
    };

    fn session() -> SessionId {
        SessionId::new("sess-cov").expect("valid session id")
    }

    fn call(raw: &str) -> CallId {
        CallId::new(raw).expect("valid call id")
    }

    /// One valid payload per `EventPayload` variant, in declaration order.
    fn all_payloads() -> Vec<(&'static str, EventPayload)> {
        let request = RequestId::new("req-cov").expect("valid request id");
        let turn = TurnId::new("turn-cov").expect("valid turn id");
        let notice = ApprovalNotice::new(
            ApprovalId::new("approval-cov").expect("valid approval id"),
            call("call-cov"),
            "run tool host_write",
            "project scope",
            Duration::from_secs(120),
        )
        .expect("safe approval notice builds");
        let outcome = ToolOutcome::new(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            "ok",
            false,
        )
        .expect("bounded tool outcome builds");
        vec![
            ("RunStarted", EventPayload::RunStarted { request }),
            (
                "AssistantTextDelta",
                EventPayload::AssistantTextDelta(
                    AssistantText::new(turn.clone(), "item-0", "delta")
                        .expect("valid fragment builds"),
                ),
            ),
            (
                "ContextReduced",
                EventPayload::ContextReduced {
                    method: nexus_core::ContextReductionMethod::Truncate,
                    removed_items: 2,
                    retained_items: 4,
                    item_limit: 8,
                    retained_bytes: 128,
                    byte_limit: 1024,
                },
            ),
            (
                "ModelRequestStarted",
                EventPayload::ModelRequestStarted {
                    turn: turn.clone(),
                    purpose: nexus_core::ModelRequestPurpose::Task,
                    estimated_input_tokens: Some(123),
                    raw_estimated_input_tokens: Some(100),
                    estimate_scale_millis: 1250,
                    context_window_tokens: Some(94_208),
                    context_trigger_percent: 65,
                    context_target_percent: 10,
                    compaction_strategy: nexus_core::ContextCompactionStrategy::Truncate,
                    max_output_tokens: Some(2048),
                    projected_tool_outputs: 1,
                },
            ),
            (
                "ModelRequestFinished",
                EventPayload::ModelRequestFinished {
                    turn: turn.clone(),
                    response_validated: true,
                },
            ),
            (
                "ModelResponseDiscarded",
                EventPayload::ModelResponseDiscarded { turn: turn.clone() },
            ),
            (
                "ToolCallPreview",
                EventPayload::ToolCallPreview {
                    item_key: "item-0".to_owned(),
                },
            ),
            ("ApprovalRequired", EventPayload::ApprovalRequired(notice)),
            (
                "ToolStarted",
                EventPayload::ToolStarted(ToolStartedInfo {
                    call: call("call-cov"),
                    tool: nexus_core::ToolId::new("host_read", nexus_core::M0_REVISION).unwrap(),
                    args_preview: None,
                }),
            ),
            (
                "ToolOutput",
                EventPayload::ToolOutput(
                    ToolProgress::new(call("call-cov"), "progress", false)
                        .expect("valid progress builds"),
                ),
            ),
            (
                "ToolFinished",
                EventPayload::ToolFinished(ToolFinishedInfo {
                    call: call("call-cov"),
                    outcome,
                }),
            ),
            (
                "UsageUpdated",
                EventPayload::UsageUpdated(Usage::new(
                    Some(1),
                    Some(2),
                    UsageFinality::Provisional,
                )),
            ),
            (
                "RunFinished",
                EventPayload::RunFinished(
                    RunFinished::new(RunOutcome::Completed, PersistenceState::Ephemeral, None)
                        .expect("terminal record builds"),
                ),
            ),
        ]
    }

    #[test]
    fn control_classification_covers_every_payload_variant() {
        let payloads = all_payloads();
        assert_eq!(payloads.len(), 13, "one fixture per EventPayload variant");

        let mut control = Vec::new();
        let mut data = Vec::new();
        for (name, payload) in &payloads {
            // Exhaustive match as the independent oracle: a new
            // `EventPayload` variant fails to compile here, forcing an
            // explicit channel decision rather than a silent fallthrough.
            let expected = match payload {
                EventPayload::RunStarted { .. }
                | EventPayload::ContextReduced { .. }
                | EventPayload::ApprovalRequired(_)
                | EventPayload::ToolStarted(_)
                | EventPayload::ToolFinished(_)
                | EventPayload::ModelRequestStarted { .. }
                | EventPayload::ModelRequestFinished { .. }
                | EventPayload::ModelResponseDiscarded { .. }
                | EventPayload::UsageUpdated(_)
                | EventPayload::RunFinished(_) => true,
                EventPayload::AssistantTextDelta(_)
                | EventPayload::ToolCallPreview { .. }
                | EventPayload::ToolOutput(_) => false,
            };
            assert_eq!(is_control_payload(payload), expected, "{name}");
            if expected {
                control.push(*name);
            } else {
                data.push(*name);
            }
        }

        // Request lifecycle, committed reductions, approvals, outcomes, usage,
        // and terminal travel on control; text, previews, and progress use data.
        assert_eq!(
            control,
            [
                "RunStarted",
                "ContextReduced",
                "ModelRequestStarted",
                "ModelRequestFinished",
                "ModelResponseDiscarded",
                "ApprovalRequired",
                "ToolStarted",
                "ToolFinished",
                "UsageUpdated",
                "RunFinished",
            ]
        );
        assert_eq!(
            data,
            ["AssistantTextDelta", "ToolCallPreview", "ToolOutput"]
        );
    }

    #[test]
    fn declared_capacities_are_the_lock_table_exhaustion_boundaries() {
        let limits = Limits::m0_test();
        assert_eq!(DATA_CAPACITY, limits.event_data_capacity);
        assert_eq!(CONTROL_CAPACITY, limits.event_control_capacity);

        // One buffered event below the bound still fits; the bound itself is
        // exhaustion, matching the channel that rejects the next send.
        assert!(limits.check_event_data_buffered(DATA_CAPACITY - 1).is_ok());
        assert!(limits.check_event_data_buffered(DATA_CAPACITY).is_err());
        assert!(
            limits
                .check_event_control_buffered(CONTROL_CAPACITY - 1)
                .is_ok()
        );
        assert!(
            limits
                .check_event_control_buffered(CONTROL_CAPACITY)
                .is_err()
        );
    }

    #[test]
    fn control_payloads_never_bypass_the_consume_path_staleness_check() {
        let live = RunId::new("run-live").expect("valid run id");
        let stale = RunId::new("run-stale").expect("valid run id");

        for (name, payload) in all_payloads() {
            let control = is_control_payload(&payload);
            // Even approval and terminal events from a stale run stay
            // rejected; classification is not permission to apply.
            let event = RunEvent::new(session(), stale.clone(), u64::MAX, payload);
            assert!(!event_is_live(&event, Some(&live)), "{name}");
            assert!(!event_is_live(&event, None), "{name}");
            assert_eq!(is_control_payload(event.payload()), control, "{name}");
        }
    }
}
