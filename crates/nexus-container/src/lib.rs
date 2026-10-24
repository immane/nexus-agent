//! Opt-in container-backed tool environment and bounded worker protocol.
//!
//! `nexus-container` owns gateway/workload lifecycles, Docker transports,
//! container-specific tool composition, and the worker/keeper executables.
//! Ordinary host tools remain in `nexus-tools`; this crate depends on that
//! executor layer and never introduces a dependency in the opposite direction.

#![forbid(unsafe_code)]

mod exec;
pub mod file_tools;
pub mod gateway;
mod ownership;
mod session;
pub mod worker_client;
pub mod worker_protocol;
mod worker_transport;
mod workload;

pub use gateway::{
    ContainerEnvironment, ContainerError, GatewayIdentity, discover_host_ipv4_cidrs,
    validate_image_reference,
};
pub use workload::tool_registrations;
