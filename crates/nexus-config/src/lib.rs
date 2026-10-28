//! `nexus-config`: typed user configuration with file persistence.
//!
//! This crate owns the user and operational representations
//! [05-configuration](../docs/contracts/05-configuration.md) requires:
//! provider profiles, model entries, favourite models, and recently used
//! models, stored as one versioned JSON document. Operational settings use
//! separate layered JSONC documents (see [`settings`]). It does NOT cover tool
//! registrations, plugin profiles, or session policy; those
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
//! Model entries alone may set `max_output_tokens` to a positive `u32`, strictly
//! below their resolved context window when known. This task-generation cap also
//! raises output headroom above the legacy reserve when necessary; omission
//! leaves the generation cap unset and preserves the legacy reserve.
//! [`UserConfig::context_management_for`] resolves these settings without
//! discovery, token estimation, or automatic compaction. Absent optional fields
//! preserve existing behavior and canonical output; explicit null is invalid.
//!
//! # Dependency justification
//!
//! `nexus-core` supplies validated context-management types. `nexus-validation`
//! supplies strict duplicate-rejecting user-JSON parsing. `serde_json` owns
//! the JSON representation; typed operational settings also accept JSON comments.
//! Filesystem access is confined to loading and saving; user saves are atomic.
//! No network, discovery, or plugin launch occurs during loading.

#![forbid(unsafe_code)]

pub mod model;
pub mod settings;
pub mod store;
pub mod tui_settings;

pub use model::{
    AdapterKind, AgentMode, CredentialRef, MODE_BUILD, MODE_PLAN, ModelEntry, ProtocolKind,
    ProviderProfile, UserConfig,
};
pub use settings::{
    EffectiveSettings, ProviderSettings, SettingsProfile, default_base_path, load_settings,
    settings_json,
};
pub use store::{
    CONFIG_ENV_VAR, CONFIG_FILE_NAME, ConfigError, ConfigErrorKind, default_path,
    default_path_with, load, resolve_credential, resolve_path, resolve_path_with, resolve_with,
    save, summary,
};
pub use tui_settings::{ThemeSettings, TuiSettings};
