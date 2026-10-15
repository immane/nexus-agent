//! `nexus-openai`: first-party HTTP provider protocol adapters.
//!
//! This crate owns the first REAL [`nexus_core::ProviderPort`]
//! implementation: it turns a [`nexus_core::ModelRequest`] into a configured
//! Chat Completions, OpenAI Responses, or Anthropic Messages exchange and maps
//! the reply back to normalized provider events. Protocol selection is
//! explicit in the provider profile and remains separate from direct/relay
//! routing.
//!
//! # Transport honesty
//!
//! - Blocking `std` I/O with timeouts derived from the live run deadline.
//!   Plain `http` endpoints go over direct TCP; `https` endpoints go through
//!   the system `openssl s_client` helper as a TLS bridge (no TLS crate is
//!   vendored, so without an `openssl` binary on `PATH` an `https` call
//!   fails `Protocol` before any secret is used). Certificate verification
//!   is enforced (`verify_return_error` against the host name or IP); a
//!   verification failure is a `Protocol` error, never a silent downgrade.
//! - Requests send `stream: true` and parse each protocol's SSE terminal
//!   contract (Chat Completions `[DONE]`, Responses completion events, and
//!   Messages `message_stop`), including chunked framing. Plain JSON replies
//!   are also accepted. Text deltas are provisional; tool candidates are
//!   withheld until the authoritative terminal payload validates.
//! - Cancellation is observed before connecting, after connecting, and
//!   between read quanta; a cancelled call ends `Cancelled`, never
//!   half-ingested.
//! - Response bodies are capped; oversize replies fail `ResourceLimit`
//!   instead of allocating unboundedly. Malformed replies fail `Protocol`,
//!   transport failures fail `Protocol`, timeouts fail `Timeout`, and a
//!   missing credential fails `Authentication` before any socket opens.
//! - Unknown usage counters stay unknown, never zero. Invalid or incomplete
//!   tool proposals fail protocol validation rather than fabricating
//!   dispatchable work. Thinking traces that cannot be faithfully replayed
//!   by the selected protocol are rejected explicitly.
//!
//! # Item identities
//!
//! Replies carry one text block at most, published as
//! `item-0`, with calls numbered from `item-1`, so text and call keys can
//! never collide. Provider call ids round-trip verbatim as refs.
//!
//! # Dependency justification
//!
//! `nexus-core` supplies the port and event types. `nexus-config`
//! supplies provider profiles and credential references (values resolve at
//! use time). `serde_json` owns request encoding and response decoding.

#![forbid(unsafe_code)]

pub mod provider;

pub use provider::OpenAiProvider;
