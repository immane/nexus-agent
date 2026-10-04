//! Deterministic test-only doubles over the synchronous core ports.
//!
//! [`FakeProvider`] serves scripted normalized event streams, [`FakeTool`]
//! records admitted calls and replays scripted outcomes, and
//! [`EphemeralStore`] is an explicitly non-durable in-memory
//! [`SessionStore`](nexus_core::SessionStore). No randomness, no I/O, no
//! network; every script is a named constructor.

#![forbid(unsafe_code)]

pub mod provider;
pub mod store;
pub mod tool;

pub use provider::{
    FRAGMENTED_TEXT, FakeProvider, candidate, reassemble_bytes, stop_turn, tool_turn,
};
pub use store::EphemeralStore;
pub use tool::{FakeTool, FakeToolCallRecord};
