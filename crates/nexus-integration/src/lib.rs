#![forbid(unsafe_code)]

//! P6 cross-crate integration tests (M0).
//!
//! This crate ships no production code: the library root is intentionally
//! empty and every proof lives in `tests/*.rs`, driven against the real
//! runtime and the real fakes. Shared deterministic helpers live in
//! `tests/common/mod.rs` so no test file grows its own mini-framework.
