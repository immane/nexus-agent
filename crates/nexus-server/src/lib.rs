//! `nexus-server`: loopback HTTP frontend for the agent runtime (M0-test scope).
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
//! - `POST /sessions` -> `201 {"session": "<id>"}`. One
//!   [`nexus_runtime::Runtime`] per
//!   session: sessions never alias runs, calls, or approvals, and a second
//!   session is never blocked by the first. An optional
//!   `{"provider": "<id>", "model": "<id>"}` body binds the session to a
//!   configured provider served by the real OpenAI-compatible adapter;
//!   without it the session serves the demo script. Unknown identities
//!   fail before any run is minted, and a missing credential fails
//!   without network touch.
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
//!   Approval additionally accepts `"scope":"session-directory"` for
//!   the runtime-published directory in development mode (default: once).
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
//! # User configuration
//!
//! The server holds one [`nexus_config::UserConfig`], installed by
//! [`Server::set_config`] together with the file it is saved to.
//! [`Server::new`] alone starts from the empty default document with no
//! path, so every configuration-dependent route behaves as not-ready
//! rather than guessing an identity.
//!
//! - `GET /config` -> `200` with the redacted summary: providers, models,
//!   favourites, and recents. Credential *references* (variable names) are
//!   included because the document cannot hold a value.
//! - `POST /sessions/{sid}/runs` accepts optional `"provider"` and
//!   `"model"`. An unknown identity, or a pair that disagrees about
//!   ownership, is `400`. A selected provider whose credential does not
//!   resolve is `503`; the diagnostic names only the provider id.
//!   An accepted run records its model and, at its SSE terminal event,
//!   marks that model recently used and saves the file. A failed save is
//!   logged and never changes the run's outcome.
//! - `POST /config/favourites` with `{"id": "<model id>"}` -> `200` with
//!   the resulting list; `400` for an unknown model, `409` for a full list,
//!   `500` when the save fails.
//! - `DELETE /config/favourites/{mid}` -> `200` with the resulting list;
//!   `404` for an id that is not a favourite. The transport admits
//!   `DELETE` alongside `GET` and `POST`, so this route is reachable.
//!
//! # Dependency justification
//!
//! `serde_json` only encodes/decodes the HTTP API. `tokio` only drives the
//! runtime port from blocking connection threads (`rt`, `rt-multi-thread`,
//! `time`, `sync`, `macros`); sockets stay plain blocking `std::net` I/O,
//! so there is intentionally no `net`/`io-util` async I/O and no HTTP
//! framework. `nexus-fakes` supplies the test-only demo wiring.
//! `nexus-tools` supplies opt-in real filesystem and sandboxed exec tools.
//! `--tools real` defaults to development mode with `--tools-root` as the
//! project (default: working directory): project/temp operations are automatic,
//! external file access requires approval/session directory grants, and exec
//! has broad non-protected reads but bounded writes and no network.
//! `--strict-tools` retains jailed reads and approval-required writes/exec.
//! `nexus-config` owns the configuration document, its strict parse, its
//! atomic save, and credential *references*; the server resolves a
//! reference at submit time and never handles the value beyond that
//! single lookup.

#![forbid(unsafe_code)]

pub mod http;
pub mod json;
pub mod server;

pub use server::{Server, Session, ToolsMode};
