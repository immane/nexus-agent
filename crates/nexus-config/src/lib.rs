//! `nexus-config`: typed user configuration with file persistence.
//!
//! This crate owns exactly one of the representations
//! [05-configuration](../docs/contracts/05-configuration.md) requires:
//! provider profiles, model entries, favourite models, and recently used
//! models, stored as one versioned JSON document. It does NOT cover tool
//! registrations, plugin profiles, runtime policy, or session policy; those
//! stay with their owning slices until each selects its representation.
//!
//! # Secrets
//!
//! Credentials are references, never values. A provider carries
//! [`CredentialRef::EnvVar`], resolved only for the selected integration
//! when needed via [`resolve_credential`]. The file format has no field
//! that can hold a secret value, so none can be written, logged, or echoed
//! by construction; error messages are static and never interpolate
//! caller text.
//!
//! # File format (revision 2, with revision 1 migration)
//!
//! ```json
//! {
//!   "revision": 2,
//!   "providers": [
//!     {
//!       "id": "anthropic-main",
//!       "display_name": "Anthropic (direct)",
//!       "adapter": "direct",
//!       "protocol": "anthropic_messages",
//!       "endpoint": "https://api.anthropic.com",
//!       "credential": {"env": "ANTHROPIC_API_KEY"},
//!       "default_model": "claude-sonnet"
//!     }
//!   ],
//!   "models": [
//!     {"id": "sonnet", "provider": "anthropic-main", "name": "claude-sonnet-4-5"}
//!   ],
//!   "favourites": ["sonnet"],
//!   "recent": ["sonnet"]
//! }
//! ```
//!
//! Revision 1 documents migrate to `chat_completions`; unknown fields,
//! duplicate keys, unsupported revisions, oversize documents, and dangling
//! references are explicit errors, never silent repairs.
//! A missing file is NOT an error: [`load`] returns `None` and the caller
//! falls back to [`UserConfig::default_config`].
//!
//! Optional global `context_management` configures `trigger_percent` (65),
//! `target_percent` (10), and `strategy` (`"summarize"` or `"truncate"`).
//! Omitted policy fields use those defaults; percentages must satisfy
//! `0 < target_percent < trigger_percent < 100`. Model entries may override
//! the policy with their own `context_management` object. Providers and models
//! may set a positive `context_window_tokens` (`u32`); the provider limit is
//! only for its `default_model`, never a blanket limit for all its models.
//! [`UserConfig::context_management_for`] resolves these settings without
//! discovery, token estimation, or automatic compaction. Absent optional fields
//! preserve existing behavior and canonical output; explicit null is invalid.
//!
//! # Dependency justification
//!
//! `nexus-core` supplies validated context-management types. `nexus-validation`
//! supplies the strict duplicate-rejecting parse. `serde_json` owns lexing
//! and the in-memory value model. Filesystem access is confined to
//! [`load`]/[`save`] with atomic writes; no network, no discovery, no
//! plugin launch during loading.

#![forbid(unsafe_code)]

pub mod model;
pub mod store;

pub use model::{
    AdapterKind, AgentMode, CredentialRef, MODE_BUILD, MODE_PLAN, ModelEntry, ProtocolKind,
    ProviderProfile, UserConfig,
};
pub use store::{
    CONFIG_ENV_VAR, CONFIG_FILE_NAME, ConfigError, ConfigErrorKind, default_path,
    default_path_with, load, resolve_credential, resolve_path, resolve_path_with, resolve_with,
    save, summary,
};
