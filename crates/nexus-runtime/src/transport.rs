//! Event transport: two bounded channels plus consume-side staleness checks.
//!
//! The data channel carries presentation traffic (text deltas, previews,
//! progress) and may coalesce adjacent text fragments for the same turn
//! item. The control channel carries approvals, outcomes, usage, and
//! terminal events, which are never coalesced and never silently dropped: a
//! full control channel ends the run with an explicit limit outcome. The
//! runtime assigns one shared contiguous per-run sequence after its own
//! batching. Capacities track the M0 lock table through [`Limits`].

use nexus_core::{EventPayload, Limits, RunEvent, RunId};
use tokio::sync::mpsc;

/// Bounded data-channel capacity in events (lock section 1).
pub const DATA_CAPACITY: usize = Limits::M0_TEST_EVENT_DATA_CAPACITY;
/// Bounded control-channel capacity in events (lock section 1).
pub const CONTROL_CAPACITY: usize = Limits::M0_TEST_EVENT_CONTROL_CAPACITY;

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
pub(crate) fn is_control_payload(payload: &EventPayload) -> bool {
    matches!(
        payload,
        EventPayload::RunStarted { .. }
            | EventPayload::ApprovalRequired(_)
            | EventPayload::ToolStarted(_)
            | EventPayload::ToolFinished(_)
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

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::{RequestId, RunId, SessionId};

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
                    AssistantText::new(turn, "item-0", "delta").expect("valid fragment builds"),
                ),
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
        assert_eq!(payloads.len(), 9, "one fixture per EventPayload variant");

        let mut control = Vec::new();
        let mut data = Vec::new();
        for (name, payload) in &payloads {
            // Exhaustive match as the independent oracle: a new
            // `EventPayload` variant fails to compile here, forcing an
            // explicit channel decision rather than a silent fallthrough.
            let expected = match payload {
                EventPayload::RunStarted { .. }
                | EventPayload::ApprovalRequired(_)
                | EventPayload::ToolStarted(_)
                | EventPayload::ToolFinished(_)
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

        // Approvals, outcomes, usage, and the terminal travel on control;
        // text deltas, previews, and progress travel on data.
        assert_eq!(
            control,
            [
                "RunStarted",
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
