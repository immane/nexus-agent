//! `nexus-web`: loopback HTTP frontend for the agent runtime (M0-test scope).
//!
//! TEST-ONLY. This crate transports the same runtime command/event port the
//! TUI uses over plain HTTP/1.1 on loopback, with server-sent events for the
//! live stream. It is never real configuration: the demo wiring uses
//! scripted [`nexus_fakes`] doubles (no provider credentials, no network
//! egress beyond loopback, no stored sessions), and the server binds
//! `127.0.0.1` only. There is no authentication: any local process can
//! submit, approve, and cancel. Do not expose this server to a network.
//!
//! # Protocol (v1, unstable, test-only)
//!
//! - `GET /health` -> `{"status":"ok"}`.
//! - `POST /sessions` -> `201 {"session": "<id>"}`. One [`Runtime`] per
//!   session: sessions never alias runs, calls, or approvals, and a second
//!   session is never blocked by the first.
//! - `POST /sessions/{sid}/runs` with `{"input": "...", "profile?": "..."}`
//!   -> `201 {"reply":"Accepted","run":"..."}` on acceptance,
//!   `409` while the session's run slot is busy, `400` on invalid input.
//! - `GET /sessions/{sid}/snapshot?run=<rid>` -> `200` with the bounded
//!   snapshot, or the reply status (`404` for unknown runs).
//! - `POST /sessions/{sid}/runs/{rid}/cancel` -> the runtime reply.
//! - `POST /sessions/{sid}/runs/{rid}/approve` (or `/deny`) with
//!   `{"approval": "<aid>", "call": "<cid>"}`: the exact runtime identity
//!   collected from the `approval-required` event. The server never infers
//!   or completes identities; mismatches are rejected (`404`) or reported
//!   (`409`) by the runtime, exactly as over the in-process port.
//! - `GET /sessions/{sid}/runs/{rid}/events` -> `text/event-stream`. One
//!   subscriber per session at a time (`409` while one is attached); each
//!   event is emitted once as `data: {...}` with the per-run sequence in
//!   `id:`, a `: ping` comment every idle window, and the connection closes
//!   after that run's terminal event. Sync history first via `/snapshot`:
//!   the stream only carries events from attach time.
//!
//! Enum names in JSON are the Rust `Debug` forms and are NOT a stable
//! contract. Error bodies are static diagnostics; rejected input is never
//! echoed.
//!
//! # Dependency justification
//!
//! `serde_json` only encodes/decodes the HTTP API. `tokio` only drives the
//! runtime port from blocking connection threads (`rt`, `rt-multi-thread`,
//! `time`, `sync`, `macros`); sockets stay plain blocking `std::net` I/O,
//! so there is intentionally no `net`/`io-util` async I/O and no HTTP
//! framework. `nexus-fakes` supplies the test-only demo wiring.

#![forbid(unsafe_code)]

pub mod http;
pub mod json;
pub mod server;

pub use server::{Server, Session};
