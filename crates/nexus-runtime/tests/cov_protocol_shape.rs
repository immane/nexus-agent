#![forbid(unsafe_code)]

//! Public-surface coverage for `nexus_runtime::protocol`.
//!
//! `validate_batch` is crate-private, so its turn-shape contract is covered
//! directly by the `cov_protocol_private` unit module in `src/protocol.rs`.
//! This file pins what an external caller can observe: the module path and the
//! finite batch bounds the validator enforces over every adapter batch.

use nexus_runtime::protocol::{MAX_BATCH_BYTES, MAX_BATCH_EVENTS};

#[test]
fn protocol_bounds_are_public_and_finite() {
    // Documented M0 representation choices: one finite event-count bound over
    // the adapter's whole list (terminal included) and one aggregate payload
    // byte bound. Changing either is a deliberate contract change.
    assert_eq!(MAX_BATCH_EVENTS, 4_096);
    assert_eq!(MAX_BATCH_BYTES, 1_048_576);
}
