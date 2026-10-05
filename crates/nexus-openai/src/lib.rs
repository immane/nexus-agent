//! `nexus-openai`: OpenAI-compatible chat provider adapter.
//!
//! This crate owns the first REAL [`nexus_core::ProviderPort`]
//! implementation: it turns a [`nexus_core::ModelRequest`] into one non-streaming Chat Completions call and maps
//! the reply back to provider events. The dialect covers OpenAI-compatible
//! endpoints (vendor API, `vLLM`, `Ollama`, and other `/chat/completions`
//! servers), not any vendor-specific extensions.
//!
//! # Transport honesty
//!
//! - Plain blocking `std` sockets with timeouts derived from the live
//!   run deadline. No TLS exists here: `https` endpoints are refused at
//!   construction with an explicit error, never attempted. Local `http`
//!   endpoints (for example Ollama's default) work directly.
//! - Cancellation is observed before connecting, after connecting, and
//!   between read chunks; a cancelled call ends `Cancelled`, never
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
//! Non-streaming replies carry one text block at most, published as
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
