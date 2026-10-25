//! Typed user configuration: providers, models, favourites, recents.
//!
//! Every value is validated at construction: bounded lengths, the lock
//! identifier charset, closed adapter kinds, endpoint shape without embedded
//! credentials, and cross-references that must resolve. References are
//! never silently repaired: removing a provider or model that is still
//! referenced fails explicitly instead of cascading.

use nexus_core::{
    ContextCompactionStrategy, ContextManagementPolicy, ContextManagementSettings, ContextWindow,
    ContextWindowSource,
};
use serde_json::{Map, Value};

use crate::store::{ConfigError, invalid};

/// Canonical configuration revision. Revision 1 loads through an explicit
/// migration and every save writes this revision.
pub const CONFIG_REVISION: u64 = 2;
/// Maximum providers in one document.
pub const MAX_PROVIDERS: usize = 16;
/// Maximum model entries in one document.
pub const MAX_MODELS: usize = 64;
/// Maximum favourite models.
pub const MAX_FAVOURITES: usize = 16;
/// Maximum recently used models (most-recent-first).
pub const MAX_RECENT: usize = 32;
/// Maximum identifier length in bytes (provider/model ids).
pub const MAX_ID_LEN: usize = 64;
/// Maximum display-name length in bytes.
pub const MAX_DISPLAY_NAME_LEN: usize = 128;
/// Maximum model name length in bytes.
pub const MAX_MODEL_NAME_LEN: usize = 128;
/// Maximum endpoint length in bytes.
pub const MAX_ENDPOINT_LEN: usize = 512;
/// Maximum environment-variable name length in bytes.
pub const MAX_ENV_VAR_LEN: usize = 64;
/// Maximum agent modes in one document (built-ins excluded from the count).
pub const MAX_MODES: usize = 16;
/// Maximum agent-mode label length in bytes.
pub const MAX_MODE_LABEL_LEN: usize = 64;

/// Built-in read-only mode id: research and planning, never executes
/// confirmation-required tools.
pub const MODE_PLAN: &str = "plan";
/// Built-in full-capability mode id: the default when no default is set.
pub const MODE_BUILD: &str = "build";

/// Returns true for the lock identifier charset: ASCII letters, digits,
/// `-`, and `_`. Empty text is rejected by the length check first.
fn is_id_text(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_ID_LEN
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Validates an identifier with a static diagnostic.
fn check_id(field: &'static str, text: &str) -> Result<(), ConfigError> {
    if is_id_text(text) {
        Ok(())
    } else {
        Err(invalid(field_message(field)))
    }
}

/// Associates a static diagnostic with a field name. Every arm is a
/// distinct literal so no caller text can reach an error message.
fn field_message(field: &str) -> &'static str {
    match field {
        "provider id" => "provider id is invalid",
        "model id" => "model id is invalid",
        "mode id" => "mode id is invalid",
        "mode label" => "mode label is invalid",
        "default mode" => "default mode is invalid",
        "display name" => "display name is invalid",
        "adapter" => "adapter kind is invalid",
        "endpoint" => "endpoint is invalid",
        "credential" => "credential reference is invalid",
        "model name" => "model name is invalid",
        "favourite" => "favourite model is invalid",
        "recent entry" => "recent model entry is invalid",
        _ => "configuration value is invalid",
    }
}

/// Validates a display name: nonempty bounded text.
fn check_display_name(text: &str) -> Result<(), ConfigError> {
    if !text.is_empty() && text.len() <= MAX_DISPLAY_NAME_LEN {
        Ok(())
    } else {
        Err(invalid("display name is invalid"))
    }
}

/// Protocol adapter of a provider profile. Closed: anything else is an
/// explicit error, never a silent fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterKind {
    /// Direct vendor API integration.
    Direct,
    /// Relay/proxy service in front of one or more vendors.
    Relay,
}

impl AdapterKind {
    /// Parses the closed adapter vocabulary.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        match text {
            "direct" => Ok(Self::Direct),
            "relay" => Ok(Self::Relay),
            _ => Err(invalid("adapter kind is invalid")),
        }
    }

    /// Returns the canonical wire name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Relay => "relay",
        }
    }
}

/// Wire protocol family. Kept separate from [`AdapterKind`], which selects
/// direct versus relay routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolKind {
    /// OpenAI Chat Completions-compatible protocol.
    ChatCompletions,
    /// OpenAI Responses protocol.
    OpenAiResponses,
    /// Anthropic Messages protocol.
    AnthropicMessages,
}

impl ProtocolKind {
    /// Parses the closed protocol vocabulary.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        match text {
            "chat_completions" => Ok(Self::ChatCompletions),
            "openai_responses" => Ok(Self::OpenAiResponses),
            "anthropic_messages" => Ok(Self::AnthropicMessages),
            _ => Err(invalid("provider protocol is invalid")),
        }
    }

    /// Returns the canonical wire name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::OpenAiResponses => "openai_responses",
            Self::AnthropicMessages => "anthropic_messages",
        }
    }
}

/// Credential reference. The only variant names an environment variable;
/// values are resolved at use time and never stored, logged, or echoed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialRef {
    /// Resolve `var` from the process environment when needed.
    EnvVar {
        /// Environment variable name (`[A-Z0-9_]`, leading letter).
        var: String,
    },
}

impl CredentialRef {
    /// Builds an environment-variable reference with name validation.
    pub fn env_var(var: impl Into<String>) -> Result<Self, ConfigError> {
        let var = var.into();
        let valid = !var.is_empty()
            && var.len() <= MAX_ENV_VAR_LEN
            && var
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            && var
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_uppercase());
        if valid {
            Ok(Self::EnvVar { var })
        } else {
            Err(invalid("credential reference is invalid"))
        }
    }

    /// Returns the referenced variable name (never a secret value).
    #[must_use]
    pub fn var_name(&self) -> &str {
        let Self::EnvVar { var } = self;
        var
    }
}

/// Provider profile: adapter, endpoint, credential reference, and the
/// default model name used when no entry is selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderProfile {
    /// Unique provider identity.
    pub id: String,
    /// Human-readable label.
    pub display_name: String,
    /// Direct or relay adapter.
    pub adapter: AdapterKind,
    /// Protocol spoken by the endpoint.
    pub protocol: ProtocolKind,
    /// Optional `http(s)` endpoint without embedded credentials.
    pub endpoint: Option<String>,
    /// Secret reference (never a value).
    pub credential: CredentialRef,
    /// Default model name for this provider.
    pub default_model: String,
    /// Configured context limit of the default model, not all provider models.
    pub context_window_tokens: Option<u32>,
}

impl ProviderProfile {
    /// Builds a validated provider profile.
    pub fn new(
        id: impl Into<String>,
        display_name: impl Into<String>,
        adapter: AdapterKind,
        endpoint: Option<String>,
        credential: CredentialRef,
        default_model: impl Into<String>,
    ) -> Result<Self, ConfigError> {
        Self::new_with_protocol(
            id,
            display_name,
            adapter,
            ProtocolKind::ChatCompletions,
            endpoint,
            credential,
            default_model,
        )
    }

    /// Builds a validated profile with an explicit protocol family.
    pub fn new_with_protocol(
        id: impl Into<String>,
        display_name: impl Into<String>,
        adapter: AdapterKind,
        protocol: ProtocolKind,
        endpoint: Option<String>,
        credential: CredentialRef,
        default_model: impl Into<String>,
    ) -> Result<Self, ConfigError> {
        let profile = Self {
            id: id.into(),
            display_name: display_name.into(),
            adapter,
            protocol,
            endpoint,
            credential,
            default_model: default_model.into(),
            context_window_tokens: None,
        };
        profile.validate()?;
        Ok(profile)
    }

    /// Validates every field, including endpoint shape.
    pub fn validate(&self) -> Result<(), ConfigError> {
        check_id("provider id", &self.id)?;
        check_display_name(&self.display_name)?;
        if let Some(endpoint) = &self.endpoint {
            let shape = !endpoint.is_empty()
                && endpoint.len() <= MAX_ENDPOINT_LEN
                && (endpoint.starts_with("https://") || endpoint.starts_with("http://"))
                && !endpoint.contains('@')
                && !endpoint
                    .bytes()
                    .any(|byte| byte.is_ascii_whitespace() || byte < 0x20);
            if !shape {
                return Err(invalid("endpoint is invalid"));
            }
        }
        if self.default_model.is_empty() || self.default_model.len() > MAX_MODEL_NAME_LEN {
            return Err(invalid("model name is invalid"));
        }
        configured_window(self.context_window_tokens)?;
        Ok(())
    }
}

/// One selectable model entry bound to exactly one provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    /// Unique model identity used by favourites and recents.
    pub id: String,
    /// Owning provider identity.
    pub provider: String,
    /// Vendor model name.
    pub name: String,
    /// Configured context limit for this model.
    pub context_window_tokens: Option<u32>,
    /// Optional positive task-generation cap, below any known context window.
    pub max_output_tokens: Option<u32>,
    /// Optional policy override; omitted fields in JSON use core defaults.
    pub context_management: Option<ContextManagementPolicy>,
}

impl ModelEntry {
    /// Builds a validated model entry. Provider existence is checked by
    /// [`UserConfig`] admission, not here.
    pub fn new(
        id: impl Into<String>,
        provider: impl Into<String>,
        name: impl Into<String>,
    ) -> Result<Self, ConfigError> {
        let entry = Self {
            id: id.into(),
            provider: provider.into(),
            name: name.into(),
            context_window_tokens: None,
            context_management: None,
            max_output_tokens: None,
        };
        entry.validate()?;
        Ok(entry)
    }

    /// Validates identifier shape and the model name bound.
    pub fn validate(&self) -> Result<(), ConfigError> {
        check_id("model id", &self.id)?;
        check_id("provider id", &self.provider)?;
        if self.name.is_empty() || self.name.len() > MAX_MODEL_NAME_LEN {
            return Err(invalid("model name is invalid"));
        }
        context_settings(
            self.context_management.unwrap_or_default(),
            self.context_window_tokens,
            self.max_output_tokens,
        )?;
        Ok(())
    }
}

/// Agent interaction mode: an identity plus its tool policy. The built-ins
/// are `plan` (read-only) and `build` (full capability); further modes may
/// be defined in configuration. `read_only` is enforced by the runtime at
/// tool dispatch: a read-only run executes automatic tools but denies
/// confirmation-required calls without prompting. The mode identity itself
/// is display metadata carried by frontends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMode {
    /// Unique mode identity (built-ins `plan`/`build` or a custom id).
    pub id: String,
    /// Human-readable label shown in the composer and CLI.
    pub label: String,
    /// True when the runtime must deny confirmation-required tools.
    pub read_only: bool,
}

impl AgentMode {
    /// Built-in read-only planning mode.
    #[must_use]
    pub fn plan() -> Self {
        Self {
            id: MODE_PLAN.to_owned(),
            label: "Plan".to_owned(),
            read_only: true,
        }
    }

    /// Built-in full-capability build mode.
    #[must_use]
    pub fn build() -> Self {
        Self {
            id: MODE_BUILD.to_owned(),
            label: "Build".to_owned(),
            read_only: false,
        }
    }

    /// Builds a validated custom mode. Built-in ids are rejected so custom
    /// definitions can never shadow or redefine them.
    pub fn custom(
        id: impl Into<String>,
        label: impl Into<String>,
        read_only: bool,
    ) -> Result<Self, ConfigError> {
        let mode = Self {
            id: id.into(),
            label: label.into(),
            read_only,
        };
        mode.validate()?;
        if mode.id == MODE_PLAN || mode.id == MODE_BUILD {
            return Err(invalid("mode id is invalid"));
        }
        Ok(mode)
    }

    /// Validates identifier shape and the label bound.
    pub fn validate(&self) -> Result<(), ConfigError> {
        check_id("mode id", &self.id)?;
        if self.label.is_empty() || self.label.len() > MAX_MODE_LABEL_LEN {
            return Err(invalid("mode label is invalid"));
        }
        Ok(())
    }
}

/// Whole user configuration document: providers, models, favourites, and
/// most-recent-first usage. Cross-references must resolve; removals that
/// would dangle a reference fail instead of cascading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserConfig {
    providers: Vec<ProviderProfile>,
    models: Vec<ModelEntry>,
    favourites: Vec<String>,
    recent: Vec<String>,
    modes: Vec<AgentMode>,
    default_mode: Option<String>,
    context_management: ContextManagementPolicy,
}

impl UserConfig {
    /// Empty configuration with no providers: provider-dependent work
    /// reports not-ready until the user adds one.
    #[must_use]
    pub fn default_config() -> Self {
        Self {
            providers: Vec::new(),
            models: Vec::new(),
            favourites: Vec::new(),
            recent: Vec::new(),
            modes: Vec::new(),
            default_mode: None,
            context_management: ContextManagementPolicy::default(),
        }
    }

    /// Returns the provider profiles in admission order.
    #[must_use]
    pub fn providers(&self) -> &[ProviderProfile] {
        &self.providers
    }

    /// Returns the model entries in admission order.
    #[must_use]
    pub fn models(&self) -> &[ModelEntry] {
        &self.models
    }

    /// Returns the global context-management policy. This is configuration
    /// only; it does not perform automatic compaction.
    #[must_use]
    pub fn context_management(&self) -> ContextManagementPolicy {
        self.context_management
    }

    /// Replaces the global policy with a validated core policy.
    pub fn set_context_management(&mut self, policy: ContextManagementPolicy) {
        self.context_management = policy;
    }

    /// Resolves configured settings for a provider and optional model entry id.
    /// A provider window applies only to its default model. No discovery or
    /// model-name inference is performed for other models.
    pub fn context_management_for(
        &self,
        provider_id: &str,
        model_id: Option<&str>,
    ) -> Result<ContextManagementSettings, ConfigError> {
        check_id("provider id", provider_id)?;
        let profile = self
            .provider(provider_id)
            .ok_or(invalid("provider is unknown"))?;
        let (policy, tokens, max_output_tokens) = if let Some(id) = model_id {
            check_id("model id", id)?;
            let entry = self.model(id).ok_or(invalid("model is unknown"))?;
            if entry.provider != provider_id {
                return Err(invalid("model does not belong to provider"));
            }
            (
                entry.context_management.unwrap_or(self.context_management),
                entry.context_window_tokens.or_else(|| {
                    (entry.name == profile.default_model)
                        .then_some(profile.context_window_tokens)
                        .flatten()
                }),
                entry.max_output_tokens,
            )
        } else {
            (self.context_management, profile.context_window_tokens, None)
        };
        context_settings(policy, tokens, max_output_tokens)
    }

    /// Returns favourite model ids in admission order.
    #[must_use]
    pub fn favourites(&self) -> &[String] {
        &self.favourites
    }

    /// Returns recently used model ids, most-recent-first.
    #[must_use]
    pub fn recent(&self) -> &[String] {
        &self.recent
    }

    /// Returns custom mode definitions in admission order. The built-in
    /// `plan` and `build` modes always exist and are never listed here.
    #[must_use]
    pub fn modes(&self) -> &[AgentMode] {
        &self.modes
    }

    /// Returns the configured default mode id, if any. `None` means the
    /// built-in `build` mode.
    #[must_use]
    pub fn default_mode(&self) -> Option<&str> {
        self.default_mode.as_deref()
    }

    /// Resolves a mode id to its definition: built-ins first, then custom
    /// modes. Unknown ids return `None` instead of falling back silently.
    #[must_use]
    pub fn resolve_mode(&self, id: &str) -> Option<AgentMode> {
        match id {
            MODE_PLAN => Some(AgentMode::plan()),
            MODE_BUILD => Some(AgentMode::build()),
            _ => self.modes.iter().find(|mode| mode.id == id).cloned(),
        }
    }

    /// Effective startup mode: the configured default when it resolves,
    /// otherwise the built-in `build` mode.
    #[must_use]
    pub fn startup_mode(&self) -> AgentMode {
        self.default_mode
            .as_deref()
            .and_then(|id| self.resolve_mode(id))
            .unwrap_or_else(AgentMode::build)
    }

    /// Admits a custom mode: unique id, bounded count, never a built-in id.
    pub fn add_mode(&mut self, mode: AgentMode) -> Result<(), ConfigError> {
        mode.validate()?;
        if mode.id == MODE_PLAN || mode.id == MODE_BUILD {
            return Err(invalid("mode id is invalid"));
        }
        if self.modes.iter().any(|existing| existing.id == mode.id) {
            return Err(invalid("mode id is already registered"));
        }
        if self.modes.len() >= MAX_MODES {
            return Err(invalid("mode list is full"));
        }
        self.modes.push(mode);
        Ok(())
    }

    /// Sets the default mode. The id must resolve (built-in or admitted
    /// custom) so startup can never select a missing mode.
    pub fn set_default_mode(&mut self, id: &str) -> Result<(), ConfigError> {
        if self.resolve_mode(id).is_none() {
            return Err(invalid("default mode is invalid"));
        }
        self.default_mode = Some(id.to_owned());
        Ok(())
    }

    /// Finds a provider by id.
    #[must_use]
    pub fn provider(&self, id: &str) -> Option<&ProviderProfile> {
        self.providers.iter().find(|profile| profile.id == id)
    }

    /// Finds a model entry by id.
    #[must_use]
    pub fn model(&self, id: &str) -> Option<&ModelEntry> {
        self.models.iter().find(|entry| entry.id == id)
    }

    /// Admits a provider: unique id, bounded count.
    pub fn add_provider(&mut self, profile: ProviderProfile) -> Result<(), ConfigError> {
        profile.validate()?;
        if self
            .providers
            .iter()
            .any(|existing| existing.id == profile.id)
        {
            return Err(invalid("provider id is already registered"));
        }
        if self.providers.len() >= MAX_PROVIDERS {
            return Err(invalid("provider list is full"));
        }
        self.providers.push(profile);
        Ok(())
    }

    /// Removes a provider. Fails while any model entry still names it
    /// instead of orphaning that entry silently.
    pub fn remove_provider(&mut self, id: &str) -> Result<(), ConfigError> {
        if self.models.iter().any(|entry| entry.provider == id) {
            return Err(invalid("provider is still referenced by a model"));
        }
        let before = self.providers.len();
        self.providers.retain(|profile| profile.id != id);
        if self.providers.len() == before {
            return Err(invalid("provider is unknown"));
        }
        Ok(())
    }

    /// Admits a model entry: unique id, bounded count, provider must exist.
    pub fn add_model(&mut self, entry: ModelEntry) -> Result<(), ConfigError> {
        entry.validate()?;
        let profile = self
            .provider(&entry.provider)
            .ok_or(invalid("model provider is unknown"))?;
        let tokens = entry.context_window_tokens.or_else(|| {
            (entry.name == profile.default_model)
                .then_some(profile.context_window_tokens)
                .flatten()
        });
        context_settings(
            entry.context_management.unwrap_or(self.context_management),
            tokens,
            entry.max_output_tokens,
        )?;
        if self.models.iter().any(|existing| existing.id == entry.id) {
            return Err(invalid("model id is already registered"));
        }
        if self.models.len() >= MAX_MODELS {
            return Err(invalid("model list is full"));
        }
        self.models.push(entry);
        Ok(())
    }

    /// Removes a model entry. Fails while a favourite or recent entry still
    /// names it instead of leaving a dangling reference.
    pub fn remove_model(&mut self, id: &str) -> Result<(), ConfigError> {
        if self.favourites.iter().any(|favourite| favourite == id) {
            return Err(invalid("model is still a favourite"));
        }
        if self.recent.iter().any(|entry| entry == id) {
            return Err(invalid("model is still recently used"));
        }
        let before = self.models.len();
        self.models.retain(|entry| entry.id != id);
        if self.models.len() == before {
            return Err(invalid("model is unknown"));
        }
        Ok(())
    }

    /// Marks a model as a favourite: idempotent, bounded, must exist.
    pub fn add_favourite(&mut self, id: &str) -> Result<(), ConfigError> {
        if self.model(id).is_none() {
            return Err(invalid("favourite model is unknown"));
        }
        if !self.favourites.iter().any(|favourite| favourite == id) {
            if self.favourites.len() >= MAX_FAVOURITES {
                return Err(invalid("favourite list is full"));
            }
            self.favourites.push(id.to_owned());
        }
        Ok(())
    }

    /// Removes a favourite; unknown ids are explicit errors, never silent.
    pub fn remove_favourite(&mut self, id: &str) -> Result<(), ConfigError> {
        let before = self.favourites.len();
        self.favourites.retain(|favourite| favourite != id);
        if self.favourites.len() == before {
            return Err(invalid("favourite is unknown"));
        }
        Ok(())
    }

    /// Records model use: moves to the front, drops the oldest past the
    /// bound. Unknown ids are explicit errors.
    pub fn record_use(&mut self, id: &str) -> Result<(), ConfigError> {
        if self.model(id).is_none() {
            return Err(invalid("recent model entry is unknown"));
        }
        self.recent.retain(|entry| entry != id);
        self.recent.insert(0, id.to_owned());
        self.recent.truncate(MAX_RECENT);
        Ok(())
    }

    /// Serializes to canonical compact JSON (sorted provider/model fields
    /// are emitted in declaration order; arrays keep admission order).
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut document = Map::new();
        document.insert("revision".to_owned(), Value::Number(CONFIG_REVISION.into()));
        document.insert(
            "providers".to_owned(),
            Value::Array(self.providers.iter().map(provider_json).collect()),
        );
        document.insert(
            "models".to_owned(),
            Value::Array(self.models.iter().map(model_json).collect()),
        );
        document.insert(
            "favourites".to_owned(),
            Value::Array(
                self.favourites
                    .iter()
                    .map(|id| Value::String(id.clone()))
                    .collect(),
            ),
        );
        document.insert(
            "recent".to_owned(),
            Value::Array(
                self.recent
                    .iter()
                    .map(|id| Value::String(id.clone()))
                    .collect(),
            ),
        );
        // Custom modes and the default are omitted when unset so documents
        // without them round-trip byte-identically to before.
        if !self.modes.is_empty() {
            document.insert(
                "modes".to_owned(),
                Value::Array(self.modes.iter().map(mode_json).collect()),
            );
        }
        if let Some(default_mode) = &self.default_mode {
            document.insert(
                "default_mode".to_owned(),
                Value::String(default_mode.clone()),
            );
        }
        // Preserve existing canonical documents when the policy is unchanged.
        if self.context_management != ContextManagementPolicy::default() {
            document.insert(
                "context_management".to_owned(),
                policy_json(self.context_management),
            );
        }
        Value::Object(document).to_string()
    }

    /// Parses and fully validates a document: strict shape, known fields
    /// only, supported revision, resolving cross-references.
    pub fn from_json(text: &str) -> Result<Self, ConfigError> {
        let value = crate::store::parse_document(text)?;
        let document = value
            .as_object()
            .ok_or(invalid("configuration is invalid"))?;
        reject_unknown(
            document,
            &[
                "revision",
                "providers",
                "models",
                "favourites",
                "recent",
                "modes",
                "default_mode",
                "context_management",
            ],
        )?;
        let revision = document
            .get("revision")
            .and_then(Value::as_u64)
            .ok_or(invalid("configuration revision is invalid"))?;
        if revision != 1 && revision != CONFIG_REVISION {
            return Err(ConfigError::unsupported());
        }
        let mut config = Self::default_config();
        if let Some(policy) = document.get("context_management") {
            config.set_context_management(parse_policy(policy)?);
        }
        for provider in object_array(document, "providers")? {
            config.add_provider(parse_provider(provider, revision)?)?;
        }
        for model in object_array(document, "models")? {
            config.add_model(parse_model(model)?)?;
        }
        for favourite in string_array(document, "favourites", MAX_FAVOURITES, "favourite")? {
            config.add_favourite(&favourite)?;
        }
        let recent = string_array(document, "recent", MAX_RECENT, "recent entry")?;
        for id in recent.iter().rev() {
            // Stored order is most-recent-first; replay oldest-first
            // through the same admission as live use so the order
            // round-trips exactly.
            config.record_use(id)?;
        }
        // Custom modes admit before the default so the default resolves
        // through the same path as live configuration.
        if let Some(modes) = document.get("modes") {
            let modes = modes
                .as_array()
                .ok_or(invalid("configuration field is invalid"))?;
            if modes.len() > MAX_MODES {
                return Err(invalid("configuration field is invalid"));
            }
            for mode in modes {
                let object = mode
                    .as_object()
                    .ok_or(invalid("configuration field is invalid"))?;
                config.add_mode(parse_mode(object)?)?;
            }
        }
        if let Some(default_mode) = document.get("default_mode") {
            let id = default_mode
                .as_str()
                .filter(|text| !text.is_empty())
                .ok_or(invalid("default mode is invalid"))?;
            config.set_default_mode(id)?;
        }
        Ok(config)
    }
}

/// Serializes one provider profile. The credential carries only the
/// variable name; values never enter the document.
fn provider_json(profile: &ProviderProfile) -> Value {
    let mut object = Map::new();
    object.insert("id".to_owned(), Value::String(profile.id.clone()));
    object.insert(
        "display_name".to_owned(),
        Value::String(profile.display_name.clone()),
    );
    object.insert(
        "adapter".to_owned(),
        Value::String(profile.adapter.name().to_owned()),
    );
    object.insert(
        "protocol".to_owned(),
        Value::String(profile.protocol.name().to_owned()),
    );
    object.insert(
        "endpoint".to_owned(),
        profile
            .endpoint
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    let mut credential = Map::new();
    credential.insert(
        "env".to_owned(),
        Value::String(profile.credential.var_name().to_owned()),
    );
    object.insert("credential".to_owned(), Value::Object(credential));
    object.insert(
        "default_model".to_owned(),
        Value::String(profile.default_model.clone()),
    );
    if let Some(tokens) = profile.context_window_tokens {
        object.insert("context_window_tokens".to_owned(), Value::from(tokens));
    }
    Value::Object(object)
}

/// Serializes one model entry.
fn model_json(entry: &ModelEntry) -> Value {
    let mut object = Map::new();
    object.insert("id".to_owned(), Value::String(entry.id.clone()));
    object.insert("provider".to_owned(), Value::String(entry.provider.clone()));
    object.insert("name".to_owned(), Value::String(entry.name.clone()));
    if let Some(tokens) = entry.context_window_tokens {
        object.insert("context_window_tokens".to_owned(), Value::from(tokens));
    }
    if let Some(policy) = entry.context_management {
        object.insert("context_management".to_owned(), policy_json(policy));
    }
    if let Some(tokens) = entry.max_output_tokens {
        object.insert("max_output_tokens".to_owned(), Value::from(tokens));
    }
    Value::Object(object)
}

/// Serializes one custom agent mode.
fn mode_json(mode: &AgentMode) -> Value {
    let mut object = Map::new();
    object.insert("id".to_owned(), Value::String(mode.id.clone()));
    object.insert("label".to_owned(), Value::String(mode.label.clone()));
    object.insert("read_only".to_owned(), Value::Bool(mode.read_only));
    Value::Object(object)
}

/// Rejects unknown object fields with a static diagnostic.
fn reject_unknown(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), ConfigError> {
    if object.keys().all(|key| allowed.contains(&key.as_str())) {
        Ok(())
    } else {
        Err(invalid("configuration field is unknown"))
    }
}

/// Extracts a required array of objects with a static diagnostic.
fn object_array<'a>(
    document: &'a Map<String, Value>,
    field: &'static str,
) -> Result<Vec<&'a Map<String, Value>>, ConfigError> {
    let values = document
        .get(field)
        .and_then(Value::as_array)
        .ok_or(invalid("configuration field is invalid"))?;
    values
        .iter()
        .map(|value| {
            value
                .as_object()
                .ok_or(invalid("configuration field is invalid"))
        })
        .collect()
}

/// Extracts an optional array of strings with count and content checks.
fn string_array(
    document: &Map<String, Value>,
    field: &'static str,
    max: usize,
    item: &'static str,
) -> Result<Vec<String>, ConfigError> {
    let values = match document.get(field) {
        None => return Ok(Vec::new()),
        Some(value) => value
            .as_array()
            .ok_or(invalid("configuration field is invalid"))?,
    };
    if values.len() > max {
        return Err(invalid("configuration field is invalid"));
    }
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
                .ok_or(invalid(field_message(item)))
        })
        .collect()
}

/// Requires a string field with a static diagnostic.
fn require_str<'a>(
    object: &'a Map<String, Value>,
    field: &'static str,
) -> Result<&'a str, ConfigError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or(invalid("configuration field is invalid"))
}

/// Parses one provider object with known fields only.
fn parse_provider(
    object: &Map<String, Value>,
    revision: u64,
) -> Result<ProviderProfile, ConfigError> {
    reject_unknown(
        object,
        &[
            "id",
            "display_name",
            "adapter",
            "protocol",
            "endpoint",
            "credential",
            "default_model",
            "context_window_tokens",
        ],
    )?;
    let credential = object
        .get("credential")
        .and_then(Value::as_object)
        .ok_or(invalid("credential reference is invalid"))?;
    reject_unknown(credential, &["env"])?;
    let var = require_str(credential, "env")?;
    let endpoint = match object.get("endpoint") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .ok_or(invalid("endpoint is invalid"))?
                .to_owned(),
        ),
    };
    let protocol = if revision == 1 {
        if object.contains_key("protocol") {
            return Err(invalid("configuration field is unknown"));
        }
        ProtocolKind::ChatCompletions
    } else {
        ProtocolKind::parse(require_str(object, "protocol")?)?
    };
    let mut profile = ProviderProfile::new_with_protocol(
        require_str(object, "id")?.to_owned(),
        require_str(object, "display_name")?.to_owned(),
        AdapterKind::parse(require_str(object, "adapter")?)?,
        protocol,
        endpoint,
        CredentialRef::env_var(var.to_owned())?,
        require_str(object, "default_model")?.to_owned(),
    )?;
    profile.context_window_tokens = parse_window_tokens(object)?;
    profile.validate()?;
    Ok(profile)
}

/// Parses one model object with known fields only.
fn parse_model(object: &Map<String, Value>) -> Result<ModelEntry, ConfigError> {
    reject_unknown(
        object,
        &[
            "id",
            "provider",
            "name",
            "context_window_tokens",
            "context_management",
            "max_output_tokens",
        ],
    )?;
    let mut entry = ModelEntry::new(
        require_str(object, "id")?.to_owned(),
        require_str(object, "provider")?.to_owned(),
        require_str(object, "name")?.to_owned(),
    )?;
    entry.context_window_tokens = parse_window_tokens(object)?;
    entry.max_output_tokens = object
        .get("max_output_tokens")
        .map(|value| {
            value
                .as_u64()
                .and_then(|tokens| u32::try_from(tokens).ok())
                .filter(|tokens| *tokens > 0)
                .ok_or(invalid("max output tokens is invalid"))
        })
        .transpose()?;
    entry.context_management = object
        .get("context_management")
        .map(parse_policy)
        .transpose()?;
    entry.validate()?;
    Ok(entry)
}

/// Validates resolved settings, including any inherited default-model window.
fn context_settings(
    policy: ContextManagementPolicy,
    tokens: Option<u32>,
    max_output_tokens: Option<u32>,
) -> Result<ContextManagementSettings, ConfigError> {
    let settings = ContextManagementSettings {
        policy,
        window: configured_window(tokens)?,
        max_output_tokens,
    };
    settings
        .validate()
        .map_err(|_| invalid("max output tokens is invalid for context window"))?;
    Ok(settings)
}

/// Creates a configured window through the domain validation boundary.
fn configured_window(tokens: Option<u32>) -> Result<Option<ContextWindow>, ConfigError> {
    tokens
        .map(|tokens| {
            ContextWindow::new(tokens, ContextWindowSource::Configured)
                .map_err(|_| invalid("context window is invalid"))
        })
        .transpose()
}

/// Missing limits are unknown; explicit null and non-positive/non-integer
/// values are errors rather than silently becoming unknown.
fn parse_window_tokens(object: &Map<String, Value>) -> Result<Option<u32>, ConfigError> {
    object
        .get("context_window_tokens")
        .map(|value| {
            value
                .as_u64()
                .and_then(|tokens| u32::try_from(tokens).ok())
                .filter(|tokens| *tokens > 0)
                .ok_or(invalid("context window is invalid"))
        })
        .transpose()
}

/// Parses a partial policy using core defaults, never an inherited policy.
fn parse_policy(value: &Value) -> Result<ContextManagementPolicy, ConfigError> {
    let object = value
        .as_object()
        .ok_or(invalid("context policy is invalid"))?;
    reject_unknown(object, &["trigger_percent", "target_percent", "strategy"])?;
    let defaults = ContextManagementPolicy::default();
    let percent = |field: &str, default: u8| -> Result<u8, ConfigError> {
        object.get(field).map_or(Ok(default), |value| {
            value
                .as_u64()
                .and_then(|number| u8::try_from(number).ok())
                .ok_or(invalid("context policy is invalid"))
        })
    };
    let strategy = match object.get("strategy") {
        None => defaults.strategy(),
        Some(Value::String(name)) if name == "summarize" => ContextCompactionStrategy::Summarize,
        Some(Value::String(name)) if name == "truncate" => ContextCompactionStrategy::Truncate,
        Some(_) => return Err(invalid("context policy strategy is invalid")),
    };
    ContextManagementPolicy::new(
        percent("trigger_percent", defaults.trigger_percent())?,
        percent("target_percent", defaults.target_percent())?,
        strategy,
    )
    .map_err(|_| invalid("context policy is invalid"))
}

fn policy_json(policy: ContextManagementPolicy) -> Value {
    serde_json::json!({
        "trigger_percent": policy.trigger_percent(),
        "target_percent": policy.target_percent(),
        "strategy": match policy.strategy() {
            ContextCompactionStrategy::Summarize => "summarize",
            ContextCompactionStrategy::Truncate => "truncate",
        },
    })
}

/// Parses one custom mode object with known fields only.
fn parse_mode(object: &Map<String, Value>) -> Result<AgentMode, ConfigError> {
    reject_unknown(object, &["id", "label", "read_only"])?;
    let read_only = object
        .get("read_only")
        .and_then(Value::as_bool)
        .ok_or(invalid("configuration field is invalid"))?;
    AgentMode::custom(
        require_str(object, "id")?.to_owned(),
        require_str(object, "label")?.to_owned(),
        read_only,
    )
}
