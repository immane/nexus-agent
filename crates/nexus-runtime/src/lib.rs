//! Nexus runtime: single-active-run execution loop.
//!
//! M0 scope: one active run with explicit `Busy` rejection, exact-tuple
//! approval binding, headless denial without a handler, bounded two-channel
//! event transport with contiguous per-run sequencing, honest cancellation
//! (unknown effects, never rollback claims, never blind retry), and
//! ephemeral-only persistence. Core ports stay synchronous; async adaptation
//! lives at this crate's boundary.

#![forbid(unsafe_code)]

pub mod policy;
pub mod runtime;
pub mod transport;

pub use policy::Policy;
pub use runtime::{RunState, Runtime, RuntimeConfig};
pub use transport::{CONTROL_CAPACITY, DATA_CAPACITY, EventStreams, event_is_live};
