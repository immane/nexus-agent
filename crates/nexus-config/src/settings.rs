//! Layered operational settings for frontends and live providers.
//!
//! These settings are separate from the user-owned provider/model document so
//! frontend saves cannot flatten a system base file or its overlays.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nexus_core::Limits;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::store::{ConfigError, MAX_DOCUMENT_BYTES, invalid, parse_document};
use crate::tui_settings::TuiSettings;

const SETTINGS_REVISION: u64 = 1;
const BASE_CONFIG_PATH: &str = "/etc/nexus-agent/base.jsonc";

/// Execution backend selected by startup configuration or a command-line flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Local,
    Container,
}

/// Server's default tool registration mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerToolsMode {
    Fake,
    Real,
}

/// Existing command-line startup defaults, without credentials or policy bypasses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default = "LaunchSettings::empty", deny_unknown_fields)]
pub struct LaunchSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict_tools: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_tools: Option<ServerToolsMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_store: Option<String>,
    /// Name of the environment variable containing the bearer token, never the token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_token_env: Option<String>,
}

impl Default for LaunchSettings {
    fn default() -> Self {
        Self {
            execution: Some(ExecutionMode::Local),
            strict_tools: Some(false),
            tools_root: None,
            server_port: Some(8471),
            server_tools: Some(ServerToolsMode::Fake),
            session_store: None,
            auth_token_env: None,
        }
    }
}

impl LaunchSettings {
    fn empty() -> Self {
        Self {
            execution: None,
            strict_tools: None,
            tools_root: None,
            server_port: None,
            server_tools: None,
            session_store: None,
            auth_token_env: None,
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        for path in [self.tools_root.as_deref(), self.session_store.as_deref()] {
            if path.is_some_and(|value| {
                value.is_empty() || value.len() > 4096 || value.bytes().any(|byte| byte == 0)
            }) {
                return Err(invalid("startup path is invalid"));
            }
        }
        if self.auth_token_env.as_deref().is_some_and(|name| {
            name.is_empty()
                || name.len() > 128
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        }) {
            return Err(invalid(
                "authentication environment variable name is invalid",
            ));
        }
        Ok(())
    }
}

/// Selects the established frontend budget profile. Defaults match current
/// TUI and ordinary-server behavior exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsProfile {
    /// Interactive local coding sessions.
    Tui,
    /// Ordinary loopback server sessions.
    Server,
    /// Benchmark execution defaults; external files use the ordinary schema.
    Benchmark,
}

impl SettingsProfile {
    fn key(self) -> &'static str {
        match self {
            Self::Tui => "tui",
            Self::Server => "server",
            Self::Benchmark => "server",
        }
    }

    fn defaults(self) -> EffectiveSettings {
        let mut limits = Limits::m0_test();
        match self {
            Self::Tui => {
                limits.max_model_turns_per_run = 64;
                limits.max_tool_calls_per_run = 128;
                limits.run_duration = Duration::from_secs(60 * 60);
                limits.per_tool_timeout = Duration::from_secs(10 * 60);
                limits.retained_context_items = 512;
            }
            Self::Server => {
                limits.run_duration = Duration::from_secs(60 * 60);
                limits.per_tool_timeout = Duration::from_secs(10 * 60);
            }
            Self::Benchmark => return benchmark_defaults(),
        }
        EffectiveSettings {
            limits,
            provider: ProviderSettings::default(),
            server: ServerSettings::default(),
            tools: ToolsSettings::default(),
            launch: LaunchSettings::default(),
            tui: TuiSettings::default(),
        }
    }
}

fn benchmark_defaults() -> EffectiveSettings {
    let mut limits = Limits::m0_test();
    limits.max_model_turns_per_run = 500;
    limits.max_tool_calls_per_run = 4_096;
    limits.max_tool_calls_per_turn = 64;
    limits.run_duration = Limits::M0_TEST_MAX_DURATION;
    limits.per_tool_timeout = Duration::from_secs(60 * 60);
    limits.retained_context_items = 8_192;
    EffectiveSettings {
        limits,
        provider: ProviderSettings::default(),
        server: ServerSettings::default(),
        tools: ToolsSettings::default(),
        launch: LaunchSettings::default(),
        tui: TuiSettings::default(),
    }
}

/// Provider deadlines and retry delays, in seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSettings {
    pub connect_timeout_seconds: u64,
    pub context_discovery_timeout_seconds: u64,
    pub fallback_call_timeout_seconds: u64,
    pub retry_backoff_seconds: Vec<u64>,
}

impl Default for ProviderSettings {
    fn default() -> Self {
        Self {
            connect_timeout_seconds: 10,
            context_discovery_timeout_seconds: 2,
            fallback_call_timeout_seconds: 600,
            retry_backoff_seconds: vec![1, 2, 5, 10, 30, 60],
        }
    }
}

impl ProviderSettings {
    /// Validates the same deadlines and backoff bounds as the provider adapter.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(1..=86_400).contains(&self.connect_timeout_seconds)
            || !(1..=86_400).contains(&self.context_discovery_timeout_seconds)
            || !(1..=86_400).contains(&self.fallback_call_timeout_seconds)
        {
            return Err(invalid("provider timeout is invalid"));
        }
        let delays = &self.retry_backoff_seconds;
        if delays.is_empty()
            || delays.len() > 8
            || delays.iter().any(|seconds| !(1..=3_600).contains(seconds))
            || delays.windows(2).any(|pair| pair[0] > pair[1])
        {
            return Err(invalid("provider retry backoff is invalid"));
        }
        Ok(())
    }
}

/// Server admission ceilings and idle maintenance, in seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSettings {
    pub max_connections: usize,
    pub max_sessions: usize,
    pub session_idle_seconds: u64,
    pub session_cleanup_interval_seconds: u64,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            max_connections: 64,
            max_sessions: 128,
            session_idle_seconds: 1_800,
            session_cleanup_interval_seconds: 60,
        }
    }
}

impl ServerSettings {
    /// Validates reductions of the hard admission caps and bounded idle timing.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(1..=64).contains(&self.max_connections)
            || !(1..=128).contains(&self.max_sessions)
            || !(1..=86_400).contains(&self.session_idle_seconds)
            || !(1..=86_400).contains(&self.session_cleanup_interval_seconds)
        {
            return Err(invalid("server settings are invalid"));
        }
        Ok(())
    }
}

/// Filesystem tool ceilings. Settings may reduce, but never raise, hard caps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolsSettings {
    pub list_entry_limit: usize,
    pub search_file_bytes: u64,
}

impl Default for ToolsSettings {
    fn default() -> Self {
        Self {
            list_entry_limit: 10_000,
            search_file_bytes: 1_048_576,
        }
    }
}

impl ToolsSettings {
    /// Validates reductions of the hard listing and per-file search caps.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(1..=10_000).contains(&self.list_entry_limit)
            || !(1..=1_048_576).contains(&self.search_file_bytes)
        {
            return Err(invalid("tools settings are invalid"));
        }
        Ok(())
    }
}

/// Validated settings after built-in defaults, system base, and overlays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveSettings {
    limits: Limits,
    provider: ProviderSettings,
    server: ServerSettings,
    tools: ToolsSettings,
    launch: LaunchSettings,
    tui: TuiSettings,
}

impl EffectiveSettings {
    /// Returns the effective run limits.
    #[must_use]
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Returns provider retry delays in seconds. The last delay is retained
    /// for subsequent retries, matching the existing adapter behavior.
    #[must_use]
    pub fn provider_retry_backoff(&self) -> &[u64] {
        &self.provider.retry_backoff_seconds
    }

    /// Returns provider deadlines and retry settings.
    #[must_use]
    pub fn provider(&self) -> &ProviderSettings {
        &self.provider
    }

    /// Returns server admission and idle maintenance settings.
    #[must_use]
    pub fn server(&self) -> &ServerSettings {
        &self.server
    }

    /// Returns filesystem tool operational ceilings.
    #[must_use]
    pub fn tools(&self) -> &ToolsSettings {
        &self.tools
    }

    /// Returns optional defaults for existing frontend startup options.
    #[must_use]
    pub fn launch(&self) -> &LaunchSettings {
        &self.launch
    }

    /// Returns frontend presentation settings.
    #[must_use]
    pub fn tui(&self) -> &TuiSettings {
        &self.tui
    }
}

/// Resolves settings with optional system base and ordered partial overlays.
/// An absent system base is normal; an explicitly named missing file is an
/// error so operators do not silently run with unexpected defaults.
pub fn load_settings(
    profile: SettingsProfile,
    base: Option<&Path>,
    overlays: &[PathBuf],
) -> Result<EffectiveSettings, ConfigError> {
    let mut values = profile.defaults();
    let env_base_set = std::env::var_os("NEXUS_BASE_CONFIG").is_some();
    let base_path = base.map(Path::to_path_buf).or_else(default_base_path);
    if let Some(path) = base_path {
        match std::fs::File::open(path) {
            Ok(file) => apply_layer(&mut values, &read_settings(file)?, profile)?,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && base.is_none()
                    && !env_base_set => {}
            Err(_) => return Err(invalid("base settings could not be read")),
        }
    }
    for path in overlays {
        let file =
            std::fs::File::open(path).map_err(|_| invalid("settings overlay could not be read"))?;
        let text = read_settings(file)?;
        apply_layer(&mut values, &text, profile)?;
    }
    values
        .limits
        .validate()
        .map_err(|_| invalid("runtime settings are invalid"))?;
    Ok(values)
}

fn read_settings(file: std::fs::File) -> Result<String, ConfigError> {
    // Read at most one byte beyond the budget, including for growing files and pipes.
    let mut text = String::new();
    file.take((MAX_DOCUMENT_BYTES + 1) as u64)
        .read_to_string(&mut text)
        .map_err(|_| invalid("settings document could not be read"))?;
    if text.len() > MAX_DOCUMENT_BYTES {
        return Err(invalid("settings document is too large"));
    }
    Ok(text)
}

/// Returns the optional machine-wide base file path.
#[must_use]
pub fn default_base_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("NEXUS_BASE_CONFIG") {
        return Some(PathBuf::from(path));
    }
    Some(PathBuf::from(BASE_CONFIG_PATH))
}

/// Renders a stable, secret-free representation for `--print-effective-config`.
/// The output is plain pretty JSON, without comments.
#[must_use]
pub fn settings_json(settings: &EffectiveSettings) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "runtime": {
            "max_model_turns_per_run": settings.limits.max_model_turns_per_run,
            "max_tool_calls_per_run": settings.limits.max_tool_calls_per_run,
            "max_tool_calls_per_turn": settings.limits.max_tool_calls_per_turn,
            "run_timeout_seconds": settings.limits.run_duration.as_secs(),
            "tool_timeout_seconds": settings.limits.per_tool_timeout.as_secs(),
            "retained_context_items": settings.limits.retained_context_items,
            "max_arg_assembly_bytes": settings.limits.max_arg_assembly_bytes,
            "max_tool_output_bytes": settings.limits.max_tool_output_bytes,
            "event_data_capacity": settings.limits.event_data_capacity,
            "event_control_capacity": settings.limits.event_control_capacity,
            "max_concurrent_ops": settings.limits.max_concurrent_ops,
            "approval_expiry_seconds": settings.limits.approval_expiry.as_secs(),
        },
        "provider": settings.provider,
        "server": settings.server,
        "tools": settings.tools,
        "launch": settings.launch,
        "tui": settings.tui,
    }))
    .expect("validated operational settings are JSON serializable")
}

fn parse_settings(text: &str) -> Result<Value, ConfigError> {
    parse_document(&strip_comments(text)?)
}

fn strip_comments(text: &str) -> Result<String, ConfigError> {
    if text.len() > MAX_DOCUMENT_BYTES {
        return Err(invalid("settings document is too large"));
    }
    let mut bytes = text.as_bytes().to_vec();
    let mut index = 0;
    let mut in_string = false;
    while index < bytes.len() {
        if in_string {
            match bytes[index] {
                b'\\' => {
                    // Leave escape validation to the strict JSON parser.
                    index += 1;
                }
                b'"' => in_string = false,
                _ => {}
            }
        } else if bytes[index] == b'"' {
            in_string = true;
        } else if bytes[index..].starts_with(b"//") {
            while index < bytes.len() && !matches!(bytes[index], b'\n' | b'\r') {
                bytes[index] = b' ';
                index += 1;
            }
            continue;
        } else if bytes[index..].starts_with(b"/*") {
            bytes[index..index + 2].fill(b' ');
            index += 2;
            loop {
                if index >= bytes.len() {
                    return Err(invalid("settings block comment is unterminated"));
                }
                if bytes[index..].starts_with(b"*/") {
                    bytes[index..index + 2].fill(b' ');
                    index += 2;
                    break;
                }
                if !matches!(bytes[index], b'\n' | b'\r') {
                    bytes[index] = b' ';
                }
                index += 1;
            }
            continue;
        }
        index += 1;
    }
    // Entire comments become ASCII spaces; all other UTF-8 bytes stay intact.
    String::from_utf8(bytes).map_err(|_| invalid("settings document is invalid"))
}

fn apply_layer(
    settings: &mut EffectiveSettings,
    text: &str,
    profile: SettingsProfile,
) -> Result<(), ConfigError> {
    let root = parse_settings(text)?;
    let object = root
        .as_object()
        .ok_or(invalid("settings document is invalid"))?;
    reject_unknown(object, &["revision", "defaults", "profiles", "tui"])?;
    if object.get("revision").and_then(Value::as_u64) != Some(SETTINGS_REVISION) {
        return Err(invalid("settings revision is unsupported"));
    }
    if let Some(defaults) = object.get("defaults") {
        apply_profile(settings, section(defaults)?)?;
    }
    if let Some(tui) = object.get("tui") {
        apply_tui(settings, tui)?;
    }
    if let Some(profiles) = object.get("profiles") {
        let profiles = profiles
            .as_object()
            .ok_or(invalid("settings profiles are invalid"))?;
        if profiles
            .keys()
            .any(|key| !["tui", "server"].contains(&key.as_str()))
        {
            return Err(invalid("settings profile name is invalid"));
        }
        let inherited = settings.clone();
        for (key, value) in profiles {
            if key == profile.key() {
                apply_profile(settings, section(value)?)?;
            } else {
                // Invalid inactive profiles must not become latent startup failures.
                apply_profile(&mut inherited.clone(), section(value)?)?;
            }
        }
    }
    Ok(())
}

fn section(value: &Value) -> Result<&Map<String, Value>, ConfigError> {
    value
        .as_object()
        .ok_or(invalid("settings section is invalid"))
}

fn apply_profile(
    settings: &mut EffectiveSettings,
    object: &Map<String, Value>,
) -> Result<(), ConfigError> {
    reject_unknown(
        object,
        &["runtime", "provider", "server", "tools", "launch", "tui"],
    )?;
    if let Some(runtime) = object.get("runtime") {
        let runtime = section(runtime)?;
        reject_unknown(
            runtime,
            &[
                "max_model_turns_per_run",
                "max_tool_calls_per_run",
                "max_tool_calls_per_turn",
                "run_timeout_seconds",
                "tool_timeout_seconds",
                "retained_context_items",
                "max_arg_assembly_bytes",
                "max_tool_output_bytes",
                "event_data_capacity",
                "event_control_capacity",
                "max_concurrent_ops",
                "approval_expiry_seconds",
            ],
        )?;
        set_u32(
            runtime,
            "max_model_turns_per_run",
            &mut settings.limits.max_model_turns_per_run,
        )?;
        set_u32(
            runtime,
            "max_tool_calls_per_run",
            &mut settings.limits.max_tool_calls_per_run,
        )?;
        set_u32(
            runtime,
            "max_tool_calls_per_turn",
            &mut settings.limits.max_tool_calls_per_turn,
        )?;
        set_duration(
            runtime,
            "run_timeout_seconds",
            &mut settings.limits.run_duration,
        )?;
        set_duration(
            runtime,
            "tool_timeout_seconds",
            &mut settings.limits.per_tool_timeout,
        )?;
        set_usize(
            runtime,
            "retained_context_items",
            &mut settings.limits.retained_context_items,
        )?;
        set_usize(
            runtime,
            "max_arg_assembly_bytes",
            &mut settings.limits.max_arg_assembly_bytes,
        )?;
        set_usize(
            runtime,
            "max_tool_output_bytes",
            &mut settings.limits.max_tool_output_bytes,
        )?;
        set_usize(
            runtime,
            "event_data_capacity",
            &mut settings.limits.event_data_capacity,
        )?;
        set_usize(
            runtime,
            "event_control_capacity",
            &mut settings.limits.event_control_capacity,
        )?;
        set_usize(
            runtime,
            "max_concurrent_ops",
            &mut settings.limits.max_concurrent_ops,
        )?;
        set_duration(
            runtime,
            "approval_expiry_seconds",
            &mut settings.limits.approval_expiry,
        )?;
    }
    if let Some(provider) = object.get("provider") {
        let provider = section(provider)?;
        reject_unknown(
            provider,
            &[
                "connect_timeout_seconds",
                "context_discovery_timeout_seconds",
                "fallback_call_timeout_seconds",
                "retry_backoff_seconds",
            ],
        )?;
        set_seconds(
            provider,
            "connect_timeout_seconds",
            &mut settings.provider.connect_timeout_seconds,
        )?;
        set_seconds(
            provider,
            "context_discovery_timeout_seconds",
            &mut settings.provider.context_discovery_timeout_seconds,
        )?;
        set_seconds(
            provider,
            "fallback_call_timeout_seconds",
            &mut settings.provider.fallback_call_timeout_seconds,
        )?;
        if let Some(delays) = provider.get("retry_backoff_seconds") {
            let delays = delays
                .as_array()
                .ok_or(invalid("provider retry backoff is invalid"))?;
            let mut parsed = Vec::with_capacity(delays.len());
            for value in delays {
                let seconds = value
                    .as_u64()
                    .ok_or(invalid("provider retry backoff is invalid"))?;
                parsed.push(seconds);
            }
            settings.provider.retry_backoff_seconds = parsed;
        }
        settings.provider.validate()?;
    }
    if let Some(server) = object.get("server") {
        let mut merged = serde_json::to_value(&settings.server)
            .map_err(|_| invalid("server settings are invalid"))?;
        merge_partial(&mut merged, server)?;
        let server: ServerSettings =
            serde_json::from_value(merged).map_err(|_| invalid("server settings are invalid"))?;
        server.validate()?;
        settings.server = server;
    }
    if let Some(tools) = object.get("tools") {
        let mut merged = serde_json::to_value(&settings.tools)
            .map_err(|_| invalid("tools settings are invalid"))?;
        merge_partial(&mut merged, tools)?;
        let tools: ToolsSettings =
            serde_json::from_value(merged).map_err(|_| invalid("tools settings are invalid"))?;
        tools.validate()?;
        settings.tools = tools;
    }
    if let Some(launch) = object.get("launch") {
        let overlay: LaunchSettings = serde_json::from_value(launch.clone())
            .map_err(|_| invalid("startup settings are invalid"))?;
        let mut merged = settings.launch.clone();
        if overlay.execution.is_some() {
            merged.execution = overlay.execution;
        }
        if overlay.strict_tools.is_some() {
            merged.strict_tools = overlay.strict_tools;
        }
        if overlay.tools_root.is_some() {
            merged.tools_root = overlay.tools_root;
        }
        if overlay.server_port.is_some() {
            merged.server_port = overlay.server_port;
        }
        if overlay.server_tools.is_some() {
            merged.server_tools = overlay.server_tools;
        }
        if overlay.session_store.is_some() {
            merged.session_store = overlay.session_store;
        }
        if overlay.auth_token_env.is_some() {
            merged.auth_token_env = overlay.auth_token_env;
        }
        merged.validate()?;
        settings.launch = merged;
    }
    if let Some(tui) = object.get("tui") {
        apply_tui(settings, tui)?;
    }
    settings
        .limits
        .validate()
        .map_err(|_| invalid("runtime settings are invalid"))?;
    Ok(())
}

fn apply_tui(settings: &mut EffectiveSettings, overlay: &Value) -> Result<(), ConfigError> {
    let mut merged =
        serde_json::to_value(&settings.tui).map_err(|_| invalid("TUI settings are invalid"))?;
    merge_partial(&mut merged, overlay)?;
    let tui: TuiSettings =
        serde_json::from_value(merged).map_err(|_| invalid("TUI settings are invalid"))?;
    tui.validate()?;
    settings.tui = tui;
    Ok(())
}

fn merge_partial(target: &mut Value, overlay: &Value) -> Result<(), ConfigError> {
    if let Value::Object(current) = target {
        let overlay = section(overlay)?;
        for (key, value) in overlay {
            let target = current
                .get_mut(key)
                .ok_or(invalid("settings document contains an unknown field"))?;
            merge_partial(target, value)?;
        }
    } else {
        *target = overlay.clone();
    }
    Ok(())
}

fn set_seconds(
    object: &Map<String, Value>,
    key: &str,
    target: &mut u64,
) -> Result<(), ConfigError> {
    if let Some(value) = object.get(key) {
        *target = value
            .as_u64()
            .filter(|value| *value > 0 && *value <= 86_400)
            .ok_or(invalid("provider timeout is invalid"))?;
    }
    Ok(())
}

fn set_u32(object: &Map<String, Value>, key: &str, target: &mut u32) -> Result<(), ConfigError> {
    if let Some(value) = object.get(key) {
        *target = value
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0 && *value <= 1_000_000)
            .ok_or(invalid("runtime settings are invalid"))?;
    }
    Ok(())
}

fn set_usize(
    object: &Map<String, Value>,
    key: &str,
    target: &mut usize,
) -> Result<(), ConfigError> {
    if let Some(value) = object.get(key) {
        *target = value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value > 0 && *value <= 1_073_741_824)
            .ok_or(invalid("runtime settings are invalid"))?;
    }
    Ok(())
}

fn set_duration(
    object: &Map<String, Value>,
    key: &str,
    target: &mut Duration,
) -> Result<(), ConfigError> {
    if let Some(value) = object.get(key) {
        let seconds = value
            .as_u64()
            .filter(|value| *value > 0 && *value <= 86_400)
            .ok_or(invalid("runtime settings are invalid"))?;
        *target = Duration::from_secs(seconds);
    }
    Ok(())
}

fn reject_unknown(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), ConfigError> {
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(invalid("settings document contains an unknown field"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_are_whitespace_without_changing_offsets_or_utf8() {
        let text = "// 日本語 🦀\r\n{ /* café\n雪 */ \"revision\": /*1*/ 1 } // fin";
        let stripped = strip_comments(text).unwrap();
        assert_eq!(stripped.len(), text.len());
        for (original, replaced) in text.bytes().zip(stripped.bytes()) {
            if matches!(original, b'\r' | b'\n') {
                assert_eq!(original, replaced);
            }
        }
        assert!(!stripped.contains("日本語"));
        assert!(!stripped.contains("café"));
        assert!(!stripped.contains("雪"));
        assert_eq!(
            parse_settings(text).unwrap(),
            serde_json::json!({"revision": 1})
        );
        apply_layer(
            &mut SettingsProfile::Tui.defaults(),
            text,
            SettingsProfile::Tui,
        )
        .unwrap();
        assert_eq!(
            parse_settings("/*one*//*two*/{\"revision\":1}//EOF").unwrap(),
            serde_json::json!({"revision": 1})
        );
    }

    #[test]
    fn scanner_preserves_strings_urls_quotes_backslashes_and_unicode() {
        let text = r#"{"url":"https://example.test/a/*b*/?q=//", "quote":"escaped \" // /* intact", "slash":"\/", "backslash":"\\", "unicode":"雪🦀", "end":"\\"} // removed"#;
        let stripped = strip_comments(text).unwrap();
        let plain = text.strip_suffix(" // removed").unwrap();
        assert_eq!(&stripped[..plain.len()], plain);
        assert_eq!(
            parse_settings(text).unwrap(),
            parse_document(plain).unwrap()
        );
        assert_eq!(
            strip_comments(r#"{"x":"/*unterminated"}"#).unwrap(),
            r#"{"x":"/*unterminated"}"#
        );
    }

    #[test]
    fn jsonc_keeps_strict_json_syntax_and_duplicate_key_checks() {
        for text in [
            r#"{"revision":1,}"#,
            r#"{"revision":1,/*comment*/}"#,
            r#"{"values":[1,/*comment*/]}"#,
            r#"{"revision":1,/*comment*/"revision":1}"#,
            r#"{"revision":1,"revi\u0073ion":1}"#,
            r#"{"nested":{"x":1,/*comment*/"x":2}}"#,
            r#"{"revision":1/*comment*/2}"#,
            r#"{"revision":1} # unsupported comment"#,
            r#"{'revision':1}"#,
            r#"{"revision":1}/"#,
            r#"{"revision":1}/*"#,
            r#"{"revision":1}/*/"#,
            "/* never closed\n雪",
            "// comment only",
            "[]",
            "null",
            "{} {}",
            r#"{"x":"bad\q"}"#,
            r#"{"x":"unterminated // /*"#,
        ] {
            assert!(parse_settings(text).is_err(), "accepted {text}");
        }
        let error = parse_settings("{} /* private text").unwrap_err();
        assert_eq!(error.message(), "settings block comment is unterminated");
        assert!(!error.to_string().contains("private text"));
    }

    #[test]
    fn jsonc_reuses_strict_depth_and_node_budgets() {
        let nested = format!("{{\"x\":{}0{}}}", "[".repeat(64), "]".repeat(64));
        assert!(nested.len() < MAX_DOCUMENT_BYTES);
        assert!(parse_settings(&nested).is_err());
        let at_budget = format!("{{\"x\":[{}]}}", vec!["0"; 4_094].join(","));
        assert!(parse_settings(&at_budget).is_ok());
        let over_budget = format!("{{\"x\":[{}]}}", vec!["0"; 4_095].join(","));
        assert!(over_budget.len() < MAX_DOCUMENT_BYTES);
        assert!(parse_settings(&over_budget).is_err());
    }

    #[test]
    fn profiles_preserve_existing_defaults() {
        let tui = SettingsProfile::Tui.defaults();
        assert_eq!(tui.limits.max_model_turns_per_run, 64);
        assert_eq!(tui.limits.max_tool_calls_per_run, 128);
        assert_eq!(tui.limits.run_duration, Duration::from_secs(3600));
        let server = SettingsProfile::Server.defaults();
        assert_eq!(server.limits.max_model_turns_per_run, 8);
        assert_eq!(server.limits.max_tool_calls_per_run, 64);
        let benchmark = benchmark_defaults();
        assert_eq!(benchmark.limits.max_model_turns_per_run, 500);
        assert_eq!(benchmark.limits.max_tool_calls_per_run, 4096);
        for settings in [tui, server, benchmark] {
            assert_eq!(settings.provider(), &ProviderSettings::default());
            assert_eq!(settings.server(), &ServerSettings::default());
            assert_eq!(settings.tools(), &ToolsSettings::default());
            assert_eq!(settings.tui(), &TuiSettings::default());
            assert_eq!(settings.provider_retry_backoff(), &[1, 2, 5, 10, 30, 60]);
        }
    }

    #[test]
    fn overlay_layers_merge_partial_profile_values_in_order() {
        let dir = std::env::temp_dir().join(format!("nexus-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("arbitrary.data");
        let overlay = dir.join("overrides.without-jsonc-extension");
        std::fs::write(&base, r#"{"revision": 1, "defaults": {"runtime": {"max_model_turns_per_run": 100}}, "profiles": {"tui": {"runtime": {"tool_timeout_seconds": 900}}}}"#).unwrap();
        std::fs::write(
            &overlay,
            r#"{"revision": 1, "profiles": {"tui": {"runtime": {"max_model_turns_per_run": 200}}}}"#,
        )
        .unwrap();
        let actual = load_settings(SettingsProfile::Tui, Some(&base), &[overlay]).unwrap();
        assert_eq!(actual.limits.max_model_turns_per_run, 200);
        assert_eq!(actual.limits.per_tool_timeout, Duration::from_secs(900));
        assert_eq!(actual.limits.max_tool_calls_per_run, 128);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_or_unknown_settings_fail_closed_including_inactive_profiles() {
        for text in [
            r#"{"revision":2}"#,
            r#"{"revision":1,"unknown":true}"#,
            r#"{"revision":1,"revision":1}"#,
            r#"{"revision":1,"defaults":{},"defaults":{}}"#,
            r#"{"revision":1,"profiles":{"other":{}}}"#,
            r#"{"revision":1,"profiles":{"server":{"runtime":{"unknown":1}}}}"#,
            r#"{"revision":1,"profiles":{"server":{"runtime":{"max_tool_calls_per_run":0}}}}"#,
            r#"{"revision":1,"profiles":{"server":{"provider":{"connect_timeout_seconds":0}}}}"#,
            r#"{"revision":1,"profiles":{"server":{"tui":{"unknown":true}}}}"#,
            r#"{"revision":1,"profiles":{"server":{"tui":{"display":{"tool_preview_lines":0}}}}}"#,
            r#"{"revision":1,"tui":{"theme":{"accent":123}}}"#,
            r#"{"revision":1,"tui":{"theme":{"accent":"not_a_color"}}}"#,
            r#"{"revision":1,"tui":{"theme":"green"}}"#,
            r#"{"revision":1,"defaults":{"runtime":{"max_tool_calls_per_run":-1}}}"#,
            r#"{"revision":1,"defaults":{"runtime":{"max_tool_calls_per_run":1.5}}}"#,
            r#"{"revision":1,"defaults":{"runtime":{"max_tool_calls_per_run":"3"}}}"#,
            r#"{"revision":1,"defaults":{"provider":{"retry_backoff_seconds":[]}}}"#,
            r#"{"revision":1,"defaults":{"provider":{"retry_backoff_seconds":[2,1]}}}"#,
            r#"{"revision":1,"defaults":{"provider":{"retry_backoff_seconds":[true]}}}"#,
            r#"{"revision":1,"defaults":{"provider":{"context_discovery_timeout_seconds":86401}}}"#,
            r#"{"revision":1,"defaults":{"provider":{"connect_timeout_seconds":86401}}}"#,
            r#"{"revision":1,"defaults":{"provider":{"fallback_call_timeout_seconds":"5"}}}"#,
            r#"{"revision":1,"defaults":{"tui":1}}"#,
            r#"{"revision":1,"defaults":{"runtime":[]}}"#,
            "revision = 1",
        ] {
            assert!(
                apply_layer(
                    &mut SettingsProfile::Tui.defaults(),
                    text,
                    SettingsProfile::Tui
                )
                .is_err(),
                "accepted {text}"
            );
        }
    }

    #[test]
    fn checked_in_base_jsonc_matches_all_default_fields() {
        let text = include_str!("../../../config/base.jsonc");
        for profile in [SettingsProfile::Tui, SettingsProfile::Server] {
            let mut settings = profile.defaults();
            apply_layer(&mut settings, text, profile).unwrap();
            settings.limits.validate().unwrap();
            assert_eq!(settings, profile.defaults());
            let layer = parse_settings(text).unwrap();
            let printed: Value = serde_json::from_str(&settings_json(&settings)).unwrap();
            assert_eq!(
                layer["profiles"][profile.key()]["runtime"],
                printed["runtime"]
            );
            assert_eq!(layer["defaults"]["provider"], printed["provider"]);
            assert_eq!(layer["defaults"]["server"], printed["server"]);
            assert_eq!(layer["defaults"]["tools"], printed["tools"]);
            assert_eq!(layer["defaults"]["launch"], printed["launch"]);
            assert_eq!(layer["tui"], printed["tui"]);
        }
    }

    #[test]
    fn checked_in_benchmark_jsonc_is_an_ordinary_layer() {
        let text = include_str!("../../../config/benchmark.jsonc");
        for profile in [
            SettingsProfile::Tui,
            SettingsProfile::Server,
            SettingsProfile::Benchmark,
        ] {
            let mut settings = profile.defaults();
            apply_layer(&mut settings, text, profile).unwrap();
            assert_eq!(settings, benchmark_defaults());
        }
        let dir =
            std::env::temp_dir().join(format!("nexus-renamed-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("anything.custom");
        std::fs::write(&path, text).unwrap();
        assert_eq!(
            load_settings(SettingsProfile::Server, Some(&path), &[]).unwrap(),
            benchmark_defaults()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn settings_size_budget_applies_before_parsing_and_during_reads() {
        let prefix = "{\"revision\": 1}\n//";
        let boundary = format!("{prefix}{}", "x".repeat(MAX_DOCUMENT_BYTES - prefix.len()));
        apply_layer(
            &mut SettingsProfile::Tui.defaults(),
            &boundary,
            SettingsProfile::Tui,
        )
        .unwrap();
        let text = format!("{prefix}{}", "x".repeat(MAX_DOCUMENT_BYTES));
        assert!(
            apply_layer(
                &mut SettingsProfile::Tui.defaults(),
                &text,
                SettingsProfile::Tui
            )
            .is_err()
        );
        let dir = std::env::temp_dir().join(format!("nexus-large-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("large.jsonc");
        std::fs::write(&path, text).unwrap();
        assert!(load_settings(SettingsProfile::Tui, Some(&path), &[]).is_err());
        let base = dir.join("base.jsonc");
        std::fs::write(&base, r#"{"revision":1}"#).unwrap();
        assert!(load_settings(SettingsProfile::Tui, Some(&base), &[path]).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn output_is_plain_pretty_json_with_complete_effective_sections() {
        let settings = SettingsProfile::Tui.defaults();
        let text = settings_json(&settings);
        assert!(text.starts_with("{\n  "));
        let output: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            output["provider"],
            serde_json::to_value(settings.provider()).unwrap()
        );
        assert_eq!(output["tui"], serde_json::to_value(settings.tui()).unwrap());
        assert_eq!(
            output["server"],
            serde_json::to_value(settings.server()).unwrap()
        );
        assert_eq!(output["runtime"].as_object().unwrap().len(), 12);
        assert_eq!(
            output["tools"],
            serde_json::to_value(settings.tools()).unwrap()
        );
    }

    #[test]
    fn tools_layers_merge_defaults_profiles_and_overlays_in_order() {
        for profile in [
            SettingsProfile::Tui,
            SettingsProfile::Server,
            SettingsProfile::Benchmark,
        ] {
            let mut settings = profile.defaults();
            apply_layer(&mut settings, r#"{"revision":1,"defaults":{"tools":{"list_entry_limit":100,"search_file_bytes":1000}},"profiles":{"tui":{"tools":{"list_entry_limit":50}},"server":{"tools":{"list_entry_limit":25}}}}"#, profile).unwrap();
            assert_eq!(
                settings.tools().list_entry_limit,
                if profile == SettingsProfile::Tui {
                    50
                } else {
                    25
                }
            );
            assert_eq!(settings.tools().search_file_bytes, 1000);
            apply_layer(
                &mut settings,
                r#"{"revision":1,"defaults":{"tools":{"search_file_bytes":500}}}"#,
                profile,
            )
            .unwrap();
            assert_eq!(settings.tools().search_file_bytes, 500);
            assert_eq!(
                settings.tools().list_entry_limit,
                if profile == SettingsProfile::Tui {
                    50
                } else {
                    25
                }
            );
            let output: Value = serde_json::from_str(&settings_json(&settings)).unwrap();
            assert_eq!(
                output["tools"],
                serde_json::to_value(settings.tools()).unwrap()
            );
        }
    }

    #[test]
    fn tools_bounds_types_and_unknown_fields_fail_closed_in_all_profiles() {
        assert_eq!(ToolsSettings::default().list_entry_limit, 10_000);
        assert_eq!(ToolsSettings::default().search_file_bytes, 1_048_576);
        ToolsSettings::default().validate().unwrap();
        for (key, upper) in [
            ("list_entry_limit", 10_000),
            ("search_file_bytes", 1_048_576),
        ] {
            for value in [serde_json::json!(1), serde_json::json!(upper)] {
                let mut settings = SettingsProfile::Tui.defaults();
                let document = serde_json::json!({"revision":1,"defaults":{"tools":{key:value}}});
                apply_layer(&mut settings, &document.to_string(), SettingsProfile::Tui).unwrap();
                settings.tools().validate().unwrap();
            }
            for value in [
                serde_json::json!(0),
                serde_json::json!(upper + 1),
                serde_json::json!(-1),
                serde_json::json!(1.5),
                serde_json::json!(true),
                serde_json::json!(null),
                serde_json::json!("1"),
            ] {
                for location in ["defaults", "active", "inactive"] {
                    let tools = serde_json::json!({key:value});
                    let document = match location {
                        "defaults" => serde_json::json!({"revision":1,"defaults":{"tools":tools}}),
                        "active" => {
                            serde_json::json!({"revision":1,"profiles":{"tui":{"tools":tools}}})
                        }
                        _ => {
                            serde_json::json!({"revision":1,"profiles":{"server":{"tools":tools}}})
                        }
                    };
                    assert!(
                        apply_layer(
                            &mut SettingsProfile::Tui.defaults(),
                            &document.to_string(),
                            SettingsProfile::Tui
                        )
                        .is_err(),
                        "accepted {document}"
                    );
                }
            }
        }
        for tools in [
            serde_json::json!({"unknown":1}),
            serde_json::json!(null),
            serde_json::json!(1),
        ] {
            let document = serde_json::json!({"revision":1,"profiles":{"server":{"tools":tools}}});
            assert!(
                apply_layer(
                    &mut SettingsProfile::Tui.defaults(),
                    &document.to_string(),
                    SettingsProfile::Tui
                )
                .is_err()
            );
        }
        assert!(
            serde_json::from_value::<ToolsSettings>(
                serde_json::json!({"list_entry_limit":1,"search_file_bytes":1,"unknown":1})
            )
            .is_err()
        );
    }

    #[test]
    fn server_layers_merge_all_fields_in_profile_and_overlay_order() {
        let mut settings = SettingsProfile::Server.defaults();
        assert_eq!(
            settings.server(),
            &ServerSettings {
                max_connections: 64,
                max_sessions: 128,
                session_idle_seconds: 1_800,
                session_cleanup_interval_seconds: 60,
            }
        );
        apply_layer(&mut settings, r#"{"revision":1,"defaults":{"server":{"max_connections":32,"max_sessions":64,"session_idle_seconds":900,"session_cleanup_interval_seconds":30}},"profiles":{"server":{"server":{"max_connections":16,"session_idle_seconds":600}},"tui":{"server":{"max_sessions":2}}}}"#, SettingsProfile::Server).unwrap();
        assert_eq!(
            settings.server(),
            &ServerSettings {
                max_connections: 16,
                max_sessions: 64,
                session_idle_seconds: 600,
                session_cleanup_interval_seconds: 30,
            }
        );
        apply_layer(&mut settings, r#"{"revision":1,"defaults":{"server":{"max_sessions":8}},"profiles":{"server":{"server":{"session_cleanup_interval_seconds":5}}}}"#, SettingsProfile::Server).unwrap();
        assert_eq!(
            settings.server(),
            &ServerSettings {
                max_connections: 16,
                max_sessions: 8,
                session_idle_seconds: 600,
                session_cleanup_interval_seconds: 5,
            }
        );
    }

    #[test]
    fn launch_defaults_merge_and_validate_references_without_exposing_secrets() {
        let mut settings = SettingsProfile::Tui.defaults();
        apply_layer(
            &mut settings,
            r#"{"revision":1,"defaults":{"launch":{"execution":"container","tools_root":"workspace","auth_token_env":"NEXUS_TOKEN","server_port":9000}},"profiles":{"tui":{"launch":{"strict_tools":true}}}}"#,
            SettingsProfile::Tui,
        )
        .unwrap();
        assert_eq!(settings.launch.execution, Some(ExecutionMode::Container));
        assert_eq!(settings.launch.strict_tools, Some(true));
        assert_eq!(settings.launch.tools_root.as_deref(), Some("workspace"));
        assert_eq!(settings.launch.server_port, Some(9000));
        assert_eq!(
            settings.launch.auth_token_env.as_deref(),
            Some("NEXUS_TOKEN")
        );
        let output: Value = serde_json::from_str(&settings_json(&settings)).unwrap();
        assert_eq!(output["launch"]["auth_token_env"], "NEXUS_TOKEN");
        assert!(output["launch"].get("token").is_none());
        apply_layer(
            &mut settings,
            r#"{"revision":1,"profiles":{"tui":{"launch":{"execution":null,"strict_tools":null,"server_port":null}}}}"#,
            SettingsProfile::Tui,
        )
        .unwrap();
        assert_eq!(settings.launch.execution, Some(ExecutionMode::Container));
        assert_eq!(settings.launch.strict_tools, Some(true));
        assert_eq!(settings.launch.server_port, Some(9000));

        for invalid in [
            r#"{"revision":1,"defaults":{"launch":{"execution":"remote"}}}"#,
            r#"{"revision":1,"defaults":{"launch":{"server_tools":"admin"}}}"#,
            r#"{"revision":1,"defaults":{"launch":{"server_port":65536}}}"#,
            r#"{"revision":1,"defaults":{"launch":{"tools_root":""}}}"#,
            r#"{"revision":1,"defaults":{"launch":{"auth_token_env":"BAD-NAME"}}}"#,
            r#"{"revision":1,"profiles":{"server":{"launch":{"unexpected":true}}}}"#,
        ] {
            assert!(
                apply_layer(
                    &mut SettingsProfile::Tui.defaults(),
                    invalid,
                    SettingsProfile::Tui
                )
                .is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn server_bounds_types_and_unknown_fields_are_checked_in_inactive_profiles() {
        for (key, upper) in [
            ("max_connections", 64),
            ("max_sessions", 128),
            ("session_idle_seconds", 86_400),
            ("session_cleanup_interval_seconds", 86_400),
        ] {
            for value in [
                "0".to_owned(),
                (upper + 1).to_string(),
                "-1".into(),
                "1.5".into(),
                "true".into(),
                "null".into(),
                "\"1\"".into(),
            ] {
                for location in ["defaults", "active", "inactive"] {
                    let server =
                        serde_json::from_str::<Value>(&format!(r#"{{"{key}":{value}}}"#)).unwrap();
                    let document = match location {
                        "defaults" => {
                            serde_json::json!({"revision":1,"defaults":{"server":server}})
                        }
                        "active" => {
                            serde_json::json!({"revision":1,"profiles":{"tui":{"server":server}}})
                        }
                        _ => {
                            serde_json::json!({"revision":1,"profiles":{"server":{"server":server}}})
                        }
                    };
                    assert!(
                        apply_layer(
                            &mut SettingsProfile::Tui.defaults(),
                            &document.to_string(),
                            SettingsProfile::Tui
                        )
                        .is_err(),
                        "accepted {document}"
                    );
                }
            }
            for value in [1, upper] {
                let document =
                    format!(r#"{{"revision":1,"defaults":{{"server":{{"{key}":{value}}}}}}}"#);
                apply_layer(
                    &mut SettingsProfile::Tui.defaults(),
                    &document,
                    SettingsProfile::Tui,
                )
                .unwrap();
            }
        }
        for server in [
            serde_json::json!({"unknown":1}),
            serde_json::json!(1),
            serde_json::json!(null),
        ] {
            let document =
                serde_json::json!({"revision":1,"profiles":{"server":{"server":server}}});
            assert!(
                apply_layer(
                    &mut SettingsProfile::Tui.defaults(),
                    &document.to_string(),
                    SettingsProfile::Tui
                )
                .is_err()
            );
        }
    }

    #[test]
    fn provider_validation_matches_adapter_deadline_bounds() {
        let mut settings = ProviderSettings::default();
        settings.validate().unwrap();
        settings.connect_timeout_seconds = 86_400;
        settings.context_discovery_timeout_seconds = 86_400;
        settings.fallback_call_timeout_seconds = 86_400;
        settings.validate().unwrap();
        for key in [
            "connect_timeout_seconds",
            "context_discovery_timeout_seconds",
        ] {
            let text = format!(r#"{{"revision":1,"defaults":{{"provider":{{"{key}":86401}}}}}}"#);
            assert!(
                apply_layer(
                    &mut SettingsProfile::Tui.defaults(),
                    &text,
                    SettingsProfile::Tui
                )
                .is_err()
            );
        }
        settings.fallback_call_timeout_seconds = 86_401;
        assert!(settings.validate().is_err());
        for delays in [vec![], vec![0], vec![3_601], vec![2, 1], vec![1; 9]] {
            let settings = ProviderSettings {
                retry_backoff_seconds: delays,
                ..ProviderSettings::default()
            };
            assert!(settings.validate().is_err());
        }
    }

    #[test]
    fn tui_nested_partial_layers_preserve_prior_overrides() {
        let mut settings = SettingsProfile::Tui.defaults();
        apply_layer(
            &mut settings,
            r##"{"revision":1,"tui":{"theme":{"accent":"#123456"},"display":{"tool_preview_lines":20}}}"##,
            SettingsProfile::Tui,
        )
        .unwrap();
        apply_layer(&mut settings, r#"{"revision":1,"defaults":{"tui":{"theme":{"warning":"red"}}},"profiles":{"tui":{"tui":{"display":{"picker_visible_rows":12}}}}}"#, SettingsProfile::Tui).unwrap();
        let mut expected = TuiSettings::default();
        expected.theme.accent = "#123456".into();
        expected.theme.warning = "red".into();
        expected.display.tool_preview_lines = 20;
        expected.display.picker_visible_rows = 12;
        assert_eq!(settings.tui(), &expected);
    }

    #[test]
    fn deep_partial_merge_preserves_nested_defaults_and_rejects_unknown_shapes() {
        let mut value = serde_json::json!({"theme": {"a": "old", "b": "keep"}, "count": 1});
        merge_partial(&mut value, &serde_json::json!({"theme": {"a": "new"}})).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"theme": {"a": "new", "b": "keep"}, "count": 1})
        );
        assert!(merge_partial(&mut value, &serde_json::json!({"theme": {"unknown": 1}})).is_err());
        assert!(merge_partial(&mut value, &serde_json::json!({"theme": 1})).is_err());
    }
}
