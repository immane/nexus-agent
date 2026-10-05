//! Deterministic test-only doubles over the synchronous core ports.
//!
//! [`FakeProvider`] serves scripted normalized event streams, records every
//! observed [`ModelRequest`](nexus_core::ModelRequest) for inspection, and
//! fails explicitly when its script is exhausted. [`FakeTool`] records
//! admitted calls (entry before any delay or gate wait) and replays scripted
//! outcomes. [`FakeGate`] is the entered/release coordination primitive for
//! deterministic cancellation worker ownership tests. [`EphemeralStore`] is
//! an explicitly non-durable in-memory [`SessionStore`](nexus_core::SessionStore).
//! No randomness, no I/O, no network; every script is a named constructor.

#![forbid(unsafe_code)]

pub mod provider;
pub mod store;
pub mod tool;

pub use provider::{
    FRAGMENTED_TEXT, FakeGate, FakeProvider, candidate, reassemble_bytes, stop_turn, tool_turn,
};
pub use store::EphemeralStore;
pub use tool::{FakeTool, FakeToolCallRecord};

#[cfg(test)]
mod cov_lib_private {
    //! Crate-private coverage for the fakes surface.
    //!
    //! [`FakeGate::enter_and_wait`] is `pub(crate)` and unreachable from the
    //! integration tests, so its hand-off contract is pinned here.
    //! Deterministic: entry is observed before release, and the wait is
    //! bounded.

    use super::*;
    use std::time::Duration;

    #[test]
    fn gate_enter_and_wait_returns_immediately_once_released() {
        let gate = FakeGate::new();
        gate.release();
        assert!(gate.enter_and_wait(), "a pre-released gate never blocks");
        assert!(gate.is_entered());
        assert!(gate.is_released());
    }

    #[test]
    fn gate_enter_and_wait_announces_entry_then_release_unblocks() {
        let gate = FakeGate::new();
        let worker_gate = gate.clone();
        std::thread::scope(|scope| {
            let worker = scope.spawn(move || worker_gate.enter_and_wait());
            assert!(
                gate.wait_entered(Duration::from_secs(5)),
                "entry is announced before the gate blocks"
            );
            assert!(!gate.is_released());
            gate.release();
            assert!(worker.join().expect("gate worker joins"));
        });
        assert!(gate.is_entered());
        assert!(gate.is_released());
    }
}
