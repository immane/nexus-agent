//! `nexus-openai`: OpenAI-compatible chat provider adapter.
//!
//! This crate owns the first REAL [`nexus_core::ProviderPort`]
//! implementation: it turns a [`nexus_core::ModelRequest`] into one Chat
//! Completions call and maps the reply back to provider events. The dialect
//! covers OpenAI-compatible endpoints (vendor API, `vLLM`, `Ollama`, and
//! other `/chat/completions` servers), not any vendor-specific extensions.
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
//! - Requests send `stream: true` and parse the SSE wire (`data:` chunks
//!   plus `data: [DONE]`), including `Transfer-Encoding: chunked` framing.
//!   A plain single-JSON reply is still accepted for servers that ignore the
//!   flag. Either way the adapter returns one validated batch: text is
//!   aggregated to a single `item-0` delta and streamed tool-argument
//!   fragments are assembled per index before any candidate is built.
//! - Cancellation is observed before connecting, after connecting, and
//!   between read quanta; a cancelled call ends `Cancelled`, never
//!   half-ingested.
//! - Response bodies are capped; oversize replies fail `ResourceLimit`
//!   instead of allocating unboundedly. Malformed replies fail `Protocol`,
//!   transport failures fail `Protocol`, timeouts fail `Timeout`, and a
//!   missing credential fails `Authentication` before any socket opens.
//! - Unknown usage counters stay unknown, never zero. A claimed
//!   `tool_calls` finish without any parsed call is a provider-contract
//!   violation and fails `Protocol` instead of fabricating dispatchable
//!   work.
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
