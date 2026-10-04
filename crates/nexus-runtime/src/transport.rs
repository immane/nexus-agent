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
