#![forbid(unsafe_code)]

//! User-configuration coverage: validation matrices, cross-reference
//! rules, favourite/recent semantics, strict document parsing, file
//! persistence, credential resolution, and summary redaction.

use nexus_config::{
    AdapterKind, ConfigErrorKind, CredentialRef, ModelEntry, ProviderProfile, UserConfig, load,
    resolve_credential, save, summary,
};

fn provider(id: &str) -> ProviderProfile {
    ProviderProfile::new(
        id,
        format!("{id} display"),
        AdapterKind::Direct,
        Some("https://api.example.com".to_owned()),
        CredentialRef::env_var("NEXUS_TEST_KEY").expect("valid credential"),
        "default-model",
    )
    .expect("valid provider builds")
}

fn model(id: &str, provider: &str) -> ModelEntry {
    ModelEntry::new(id, provider, format!("{id}-model")).expect("valid model builds")
}

fn configured() -> UserConfig {
    let mut config = UserConfig::default_config();
    config
        .add_provider(provider("acme"))
        .expect("provider admits");
    config
        .add_model(model("fast", "acme"))
        .expect("model admits");
    config
        .add_model(model("strong", "acme"))
        .expect("model admits");
    config
}

#[test]
fn identifiers_accept_the_lock_charset_at_the_boundary() {
    for valid in ["a".to_owned(), "A-9_z".to_owned(), "x".repeat(64)] {
        assert!(
            ProviderProfile::new(
                valid.clone(),
                "Display",
                AdapterKind::Direct,
                None,
                CredentialRef::env_var("K").expect("valid"),
                "m"
            )
            .is_ok()
        );
        assert!(ModelEntry::new(valid.clone(), "acme", "m").is_ok());
    }
    let invalid = [
        "".to_owned(),
        "has space".to_owned(),
        "uni-é".to_owned(),
        "dot.name".to_owned(),
        "slash/a".to_owned(),
        "x".repeat(65),
    ];
    for text in &invalid {
        assert!(
            ProviderProfile::new(
                text.clone(),
                "Display",
                AdapterKind::Direct,
                None,
                CredentialRef::env_var("K").expect("valid"),
                "m"
            )
            .is_err(),
            "{text:?} rejected"
        );
        assert!(ModelEntry::new(text.clone(), "acme", "m").is_err());
    }
}

#[test]
fn adapter_vocabulary_is_closed() {
    assert_eq!(AdapterKind::parse("direct"), Ok(AdapterKind::Direct));
    assert_eq!(AdapterKind::parse("relay"), Ok(AdapterKind::Relay));
    for invalid in ["", "Direct", "DIRECT", "openai", "rest", "direct "] {
        assert!(AdapterKind::parse(invalid).is_err());
    }
    assert_eq!(AdapterKind::Direct.name(), "direct");
    assert_eq!(AdapterKind::Relay.name(), "relay");
}

#[test]
fn endpoints_require_http_shape_without_embedded_credentials() {
    let valid = [
        "https://api.example.com",
        "http://localhost:8080/v1",
        "https://relay.example.com:443/deep/path?q=1",
    ];
    for endpoint in valid {
        assert!(
            provider_with_endpoint(Some(endpoint)).is_ok(),
            "{endpoint} accepted"
        );
    }
    let invalid = [
        "".to_owned(),
        "api.example.com".to_owned(),
        "ftp://files.example.com".to_owned(),
        "https://user:pass@api.example.com".to_owned(),
        "https://api.example.com@evil.example.com".to_owned(),
        "https://api.example.com/has space".to_owned(),
        "https://api.example.com/\ttab".to_owned(),
        format!("https://api.example.com/{}", "p".repeat(512)),
    ];
    for endpoint in &invalid {
        assert!(
            provider_with_endpoint(Some(endpoint.as_str())).is_err(),
            "{endpoint:?} rejected"
        );
    }
    assert!(provider_with_endpoint(None).is_ok(), "endpoint is optional");
}

fn provider_with_endpoint(
    endpoint: Option<&str>,
) -> Result<ProviderProfile, nexus_config::ConfigError> {
    ProviderProfile::new(
        "acme",
        "Display",
        AdapterKind::Relay,
        endpoint.map(str::to_owned),
        CredentialRef::env_var("K").expect("valid"),
        "m",
    )
}

#[test]
fn credential_names_are_restricted_and_never_carry_values() {
    assert!(CredentialRef::env_var("A").is_ok());
    assert!(CredentialRef::env_var("NEXUS_TEST_KEY_2").is_ok());
    for invalid in [
        "",
        "lowercase",
        "has space",
        "DASH-NAME",
        "9LEADING",
        &"K".repeat(65),
    ] {
        assert!(CredentialRef::env_var(invalid).is_err());
    }
    let credential = CredentialRef::env_var("NEXUS_TEST_KEY").expect("valid");
    assert_eq!(credential.var_name(), "NEXUS_TEST_KEY");
}

#[test]
fn duplicates_capacity_and_dangling_references_fail_explicitly() {
    let mut config = configured();
    assert!(
        config.add_provider(provider("acme")).is_err(),
        "duplicate provider"
    );
    assert!(
        config.add_model(model("fast", "acme")).is_err(),
        "duplicate model"
    );
    assert!(
        config
            .add_model(model("ghost", "no-such-provider"))
            .is_err()
    );
    assert!(
        config.remove_provider("acme").is_err(),
        "referenced provider stays"
    );
    assert!(config.remove_provider("no-such").is_err());
    config.add_favourite("fast").expect("favourite admits");
    assert!(
        config.remove_model("fast").is_err(),
        "favourited model stays"
    );
    config.record_use("strong").expect("use records");
    assert!(config.remove_model("strong").is_err(), "recent model stays");
    assert!(config.remove_model("no-such").is_err());
    assert!(config.remove_favourite("no-such").is_err());
}

#[test]
fn favourites_deduplicate_and_stay_bounded() {
    let mut config = configured();
    config.add_favourite("fast").expect("first admits");
    config.add_favourite("fast").expect("repeat is idempotent");
    assert_eq!(config.favourites(), &["fast".to_owned()]);
    assert!(
        config.add_favourite("ghost").is_err(),
        "unknown model rejected"
    );
    config.remove_favourite("fast").expect("removal works");
    assert!(config.favourites().is_empty());
}

#[test]
fn recent_is_most_recent_first_with_a_bounded_tail() {
    let mut config = configured();
    for index in 0..40 {
        let id = format!("model-{index:02}");
        config
            .add_model(ModelEntry::new(&id, "acme", &id).expect("valid"))
            .expect("capacity admits");
        config.record_use(&id).expect("use records");
    }
    assert_eq!(config.recent().len(), nexus_config::model::MAX_RECENT);
    assert_eq!(config.recent()[0], "model-39");
    assert_eq!(
        config.recent()[nexus_config::model::MAX_RECENT - 1],
        "model-08"
    );
    config.record_use("model-10").expect("reuse records");
    assert_eq!(config.recent()[0], "model-10");
    assert_eq!(
        config
            .recent()
            .iter()
            .filter(|id| *id == "model-10")
            .count(),
        1,
        "reuse moves instead of duplicating"
    );
    assert!(config.record_use("ghost").is_err(), "unknown use rejected");
}

#[test]
fn documents_round_trip_and_reject_bad_shapes() {
    let mut config = configured();
    config.add_favourite("strong").expect("favourite admits");
    config.record_use("fast").expect("use records");
    config.record_use("strong").expect("use records");
    let text = config.to_json();
    let back = UserConfig::from_json(&text).expect("round trip parses");
    assert_eq!(back, config);
    assert_eq!(back.recent(), &["strong".to_owned(), "fast".to_owned()]);

    for bad in [
        "",
        "{",
        "[1,2]",
        r#"{"revision":2,"providers":[],"models":[]}"#,
        r#"{"revision":"1","providers":[],"models":[]}"#,
        r#"{"revision":1,"providers":[],"models":[],"extra":[]}"#,
        r#"{"revision":1,"providers":[]}"#,
        r#"{"revision":1,"providers":[],"models":[],"models":[]}"#,
        r#"{"revision":1,"providers":[{"id":"a"}],"models":[]}"#,
        r#"{"revision":1,"providers":[],"models":[{"id":"m","provider":"p","name":"n","x":1}]}"#,
        r#"{"revision":1,"providers":[],"models":[{"id":"m","provider":"p","name":"n"}],"favourites":["ghost"]}"#,
        r#"{"revision":1,"providers":[],"models":[{"id":"m","provider":"p","name":"n"}],"recent":["ghost"]}"#,
    ] {
        let error = UserConfig::from_json(bad).expect_err("rejected");
        assert_ne!(
            error.kind(),
            ConfigErrorKind::Io,
            "shape failures are never I/O"
        );
    }
    let version = UserConfig::from_json(r#"{"revision":99,"providers":[],"models":[]}"#)
        .expect_err("unsupported revision");
    assert_eq!(version.kind(), ConfigErrorKind::UnsupportedRevision);
    let oversized = format!(
        r#"{{"revision":1,"providers":[],"models":[],"favourites":[],"recent":[],"pad":"{}"}}"#,
        "x".repeat(70_000)
    );
    assert!(
        UserConfig::from_json(&oversized).is_err(),
        "oversize rejected"
    );
}

#[test]
fn files_persist_atomically_and_absent_means_default() {
    let dir = std::env::temp_dir().join(format!("nexus-config-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir builds");
    let path = dir.join("config.json");

    assert!(
        load(&path).expect("absent loads").is_none(),
        "missing is default"
    );

    let mut config = configured();
    config.add_favourite("fast").expect("favourite admits");
    config.record_use("strong").expect("use records");
    save(&config, &path).expect("save writes");
    let back = load(&path).expect("present loads").expect("document found");
    assert_eq!(back, config);

    std::fs::write(&path, "{broken").expect("corrupt writes");
    let error = load(&path).expect_err("corrupt is explicit");
    assert_eq!(error.kind(), ConfigErrorKind::Invalid);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path)
            .expect("metadata reads")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "owner-only permissions");
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .expect("dir reads")
            .map(|entry| entry.expect("entry reads").file_name())
            .collect();
        assert!(
            entries
                .iter()
                .all(|name| !name.to_string_lossy().ends_with(".tmp")),
            "no temporary file survives a save"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn credentials_resolve_without_leaking_values() {
    let present = CredentialRef::env_var("NEXUS_CONFIG_TEST_PRESENT").expect("valid");
    assert_eq!(
        nexus_config::resolve_with(&present, |_| Some("s3cret-value".to_owned()))
            .expect("present resolves"),
        "s3cret-value"
    );
    for lookup in [
        (|_: &str| None) as fn(&str) -> Option<String>,
        (|_: &str| Some(String::new())) as fn(&str) -> Option<String>,
    ] {
        let error = nexus_config::resolve_with(&present, lookup).expect_err("unusable fails");
        assert_eq!(error.kind(), ConfigErrorKind::MissingCredential);
        assert!(
            !error.message().contains("s3cret"),
            "diagnostics never carry values"
        );
    }
    let absent = CredentialRef::env_var("NEXUS_CONFIG_TEST_ABSENT").expect("valid");
    assert_eq!(
        resolve_credential(&absent)
            .expect_err("absent fails")
            .kind(),
        ConfigErrorKind::MissingCredential
    );
}

#[test]
fn summaries_carry_no_secret_values() {
    let mut config = configured();
    config.add_favourite("fast").expect("favourite admits");
    let text = summary(&config).to_string();
    let value: serde_json::Value = serde_json::from_str(&text).expect("summary encodes");
    assert_eq!(value["revision"], 1);
    assert_eq!(value["providers"][0]["credential"]["env"], "NEXUS_TEST_KEY");
    assert_eq!(value["favourites"][0], "fast");
    for key in ["s3cret", "password", "api_key", "BEGIN"] {
        assert!(!text.contains(key), "no secret material in summaries");
    }
}

#[test]
fn error_display_is_static_and_total() {
    let error = UserConfig::from_json("{}").expect_err("empty rejected");
    assert_eq!(
        error.to_string(),
        "[config] configuration revision is invalid"
    );
    let boxed: Box<dyn std::error::Error> = Box::new(error);
    assert!(boxed.source().is_none());
}

#[test]
fn builtin_modes_resolve_without_configuration() {
    use nexus_config::{AgentMode, MODE_BUILD, MODE_PLAN};

    let config = UserConfig::default_config();
    assert!(config.modes().is_empty());
    assert_eq!(config.default_mode(), None);
    let plan = config.resolve_mode(MODE_PLAN).expect("plan resolves");
    assert_eq!(plan, AgentMode::plan());
    assert!(plan.read_only);
    let build = config.resolve_mode(MODE_BUILD).expect("build resolves");
    assert_eq!(build, AgentMode::build());
    assert!(!build.read_only);
    assert_eq!(config.startup_mode(), AgentMode::build());
    assert_eq!(config.resolve_mode("nope"), None);
}

#[test]
fn custom_modes_admit_resolve_and_round_trip() {
    use nexus_config::AgentMode;

    let mut config = UserConfig::default_config();
    config
        .add_mode(AgentMode::custom("review", "Review", true).expect("valid mode"))
        .expect("mode admits");
    config.set_default_mode("review").expect("default admits");
    assert_eq!(config.startup_mode().id, "review");
    assert!(config.startup_mode().read_only);

    let text = config.to_json();
    let reloaded = UserConfig::from_json(&text).expect("modes round-trip");
    assert_eq!(reloaded, config);

    // Documents without modes still load and keep the old shape.
    let legacy = UserConfig::from_json(
        r#"{"revision":1,"providers":[],"models":[],"favourites":[],"recent":[]}"#,
    )
    .expect("legacy loads");
    assert_eq!(legacy, UserConfig::default_config());
    assert!(!legacy.to_json().contains("modes"));
}

#[test]
fn mode_admission_is_closed_and_explicit() {
    use nexus_config::AgentMode;

    let mut config = UserConfig::default_config();
    assert!(AgentMode::custom("plan", "Shadow", false).is_err());
    assert!(AgentMode::custom("build", "Shadow", false).is_err());
    assert!(AgentMode::custom("bad id!", "X", false).is_err());
    assert!(AgentMode::custom("ok", "", false).is_err());
    config
        .add_mode(AgentMode::custom("review", "Review", true).expect("valid"))
        .expect("admits");
    assert!(
        config
            .add_mode(AgentMode::custom("review", "Again", false).expect("valid"))
            .is_err(),
        "duplicate custom ids are rejected"
    );
    assert!(config.set_default_mode("missing").is_err());
    assert!(UserConfig::from_json(
        r#"{"revision":1,"providers":[],"models":[],"favourites":[],"recent":[],"modes":[{"id":"plan","label":"Shadow","read_only":false}]}"#,
    )
    .is_err());
    assert!(UserConfig::from_json(
        r#"{"revision":1,"providers":[],"models":[],"favourites":[],"recent":[],"default_mode":"missing"}"#,
    )
    .is_err());
}
