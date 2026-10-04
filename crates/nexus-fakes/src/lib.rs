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
