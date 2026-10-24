#![forbid(unsafe_code)]

//! Offline configuration-only context-management regression tests.

use nexus_config::{
    AdapterKind, ConfigErrorKind, CredentialRef, ModelEntry, ProviderProfile, UserConfig,
};
use nexus_core::{ContextCompactionStrategy, ContextManagementPolicy, ContextWindowSource};
use serde_json::{Value, json};

fn document() -> Value {
    json!({
        "revision": 2,
        "providers": [{
            "id": "acme", "display_name": "Acme", "adapter": "direct",
            "protocol": "chat_completions", "credential": {"env": "ACME_KEY"},
            "default_model": "default-model"
        }],
        "models": [
            {"id": "default", "provider": "acme", "name": "default-model"},
            {"id": "other", "provider": "acme", "name": "other-model"}
        ],
        "favourites": [], "recent": []
    })
}

fn parse(value: &Value) -> UserConfig {
    UserConfig::from_json(&value.to_string()).expect("valid configuration")
}

fn policy(trigger: u8, target: u8, strategy: ContextCompactionStrategy) -> ContextManagementPolicy {
    ContextManagementPolicy::new(trigger, target, strategy).expect("valid policy")
}

#[test]
fn legacy_documents_and_constructors_keep_default_settings() {
    for revision in [1, 2] {
        let mut value = document();
        value["revision"] = json!(revision);
        if revision == 1 {
            value["providers"][0]
                .as_object_mut()
                .unwrap()
                .remove("protocol");
        }
        let config = parse(&value);
        assert_eq!(
            config.context_management(),
            ContextManagementPolicy::default()
        );
        let settings = config.context_management_for("acme", None).unwrap();
        assert_eq!(settings.policy.trigger_percent(), 65);
        assert_eq!(settings.policy.target_percent(), 10);
        assert_eq!(
            settings.policy.strategy(),
            ContextCompactionStrategy::Summarize
        );
        assert!(settings.window.is_none());
        let encoded: Value = serde_json::from_str(&config.to_json()).unwrap();
        assert!(encoded.get("context_management").is_none());
        assert!(
            encoded["providers"][0]
                .get("context_window_tokens")
                .is_none()
        );
        assert!(encoded["models"][0].get("context_window_tokens").is_none());
        assert!(encoded["models"][0].get("context_management").is_none());
        assert_eq!(parse(&encoded), config);
    }
    let profile = ProviderProfile::new(
        "acme",
        "Acme",
        AdapterKind::Direct,
        None,
        CredentialRef::env_var("ACME_KEY").unwrap(),
        "default-model",
    )
    .unwrap();
    assert!(profile.context_window_tokens.is_none());
    let entry = ModelEntry::new("default", "acme", "default-model").unwrap();
    assert!(entry.context_window_tokens.is_none());
    assert!(entry.context_management.is_none());
}

#[test]
fn partial_policies_use_core_defaults_and_round_trip() {
    for (value, expected) in [
        (json!({}), ContextManagementPolicy::default()),
        (
            json!({"trigger_percent": 80}),
            policy(80, 10, ContextCompactionStrategy::Summarize),
        ),
        (
            json!({"target_percent": 20}),
            policy(65, 20, ContextCompactionStrategy::Summarize),
        ),
        (
            json!({"strategy": "truncate"}),
            policy(65, 10, ContextCompactionStrategy::Truncate),
        ),
        (
            json!({"trigger_percent": 99, "target_percent": 1}),
            policy(99, 1, ContextCompactionStrategy::Summarize),
        ),
    ] {
        let mut doc = document();
        doc["context_management"] = value.clone();
        doc["models"][0]["context_management"] = value;
        let config = parse(&doc);
        assert_eq!(config.context_management(), expected);
        assert_eq!(
            config.model("default").unwrap().context_management,
            Some(expected)
        );
        assert_eq!(UserConfig::from_json(&config.to_json()).unwrap(), config);
    }
}

#[test]
fn resolution_preserves_window_ownership_and_policy_precedence() {
    let mut doc = document();
    doc["context_management"] = json!({"trigger_percent": 80, "target_percent": 20});
    doc["providers"][0]["context_window_tokens"] = json!(100_000);
    doc["models"][1]["context_management"] = json!({"strategy": "truncate"});
    let config = parse(&doc);
    for model in [None, Some("default")] {
        let settings = config.context_management_for("acme", model).unwrap();
        assert_eq!(settings.policy, config.context_management());
        let window = settings.window.unwrap();
        assert_eq!(window.tokens(), 100_000);
        assert_eq!(window.source(), ContextWindowSource::Configured);
    }
    let other = config
        .context_management_for("acme", Some("other"))
        .unwrap();
    assert!(
        other.window.is_none(),
        "provider default limit must not leak"
    );
    assert_eq!(
        other.policy,
        policy(65, 10, ContextCompactionStrategy::Truncate)
    );

    doc["models"][0]["context_window_tokens"] = json!(40_000);
    doc["models"][1]["context_window_tokens"] = json!(u32::MAX);
    let config = parse(&doc);
    assert_eq!(
        config
            .context_management_for("acme", Some("default"))
            .unwrap()
            .window
            .unwrap()
            .tokens(),
        40_000
    );
    assert_eq!(
        config
            .context_management_for("acme", Some("other"))
            .unwrap()
            .window
            .unwrap()
            .tokens(),
        u32::MAX
    );
    assert_eq!(UserConfig::from_json(&config.to_json()).unwrap(), config);
}

#[test]
fn resolution_rejects_invalid_unknown_and_foreign_identities() {
    let mut doc = document();
    let mut second = doc["providers"][0].clone();
    second["id"] = json!("second");
    doc["providers"].as_array_mut().unwrap().push(second);
    let config = parse(&doc);
    for (provider, model) in [
        ("", None),
        ("bad id", None),
        ("missing", None),
        ("acme", Some("")),
        ("acme", Some("bad id")),
        ("acme", Some("missing")),
        ("second", Some("default")),
    ] {
        assert_eq!(
            config
                .context_management_for(provider, model)
                .unwrap_err()
                .kind(),
            ConfigErrorKind::Invalid
        );
    }
}

#[test]
fn policy_parsing_is_strict_at_both_scopes() {
    let invalid = [
        json!(null),
        json!([]),
        json!(true),
        json!("summarize"),
        json!({"unknown": 1}),
        json!({"strategy": "Summarize"}),
        json!({"strategy": "other"}),
        json!({"strategy": null}),
        json!({"strategy": 1}),
        json!({"trigger_percent": 0}),
        json!({"trigger_percent": 100}),
        json!({"trigger_percent": 256}),
        json!({"trigger_percent": -1}),
        json!({"trigger_percent": 65.0}),
        json!({"trigger_percent": "65"}),
        json!({"trigger_percent": true}),
        json!({"trigger_percent": null}),
        json!({"target_percent": 0}),
        json!({"target_percent": 65}),
        json!({"target_percent": 66}),
        json!({"target_percent": 256}),
        json!({"target_percent": -1}),
        json!({"target_percent": 10.0}),
        json!({"target_percent": "10"}),
        json!({"target_percent": null}),
        json!({"trigger_percent": 20, "target_percent": 20}),
        json!({"trigger_percent": 10, "target_percent": 20}),
    ];
    for value in invalid {
        for model_scope in [false, true] {
            let mut doc = document();
            if model_scope {
                doc["models"][0]["context_management"] = value.clone();
            } else {
                doc["context_management"] = value.clone();
            }
            assert_eq!(
                UserConfig::from_json(&doc.to_string()).unwrap_err().kind(),
                ConfigErrorKind::Invalid,
                "{value}"
            );
        }
    }
}

#[test]
fn windows_reject_null_zero_wrong_types_and_overflow_at_both_scopes() {
    for value in [
        json!(null),
        json!(0),
        json!(-1),
        json!(1.0),
        json!("100"),
        json!(true),
        json!([]),
        json!({}),
        json!(4_294_967_296_u64),
    ] {
        for scope in ["providers", "models"] {
            let mut doc = document();
            doc[scope][0]["context_window_tokens"] = value.clone();
            assert_eq!(
                UserConfig::from_json(&doc.to_string()).unwrap_err().kind(),
                ConfigErrorKind::Invalid,
                "{scope}: {value}"
            );
        }
    }
    let mut doc = document();
    doc["providers"][0]["context_window_tokens"] = json!(1);
    doc["models"][0]["context_window_tokens"] = json!(1);
    assert_eq!(
        parse(&doc)
            .context_management_for("acme", Some("default"))
            .unwrap()
            .window
            .unwrap()
            .tokens(),
        1
    );
}

#[test]
fn public_field_admission_validates_windows_and_setter_round_trips() {
    let mut config = parse(&document());
    let mut profile = config.provider("acme").unwrap().clone();
    profile.id = "second".to_owned();
    profile.context_window_tokens = Some(0);
    assert!(profile.validate().is_err());
    assert!(config.add_provider(profile).is_err());
    let mut entry = ModelEntry::new("invalid", "acme", "model").unwrap();
    entry.context_window_tokens = Some(0);
    assert!(entry.validate().is_err());
    assert!(config.add_model(entry).is_err());
    let custom = policy(75, 15, ContextCompactionStrategy::Truncate);
    config.set_context_management(custom);
    assert_eq!(config.context_management(), custom);
    assert_eq!(UserConfig::from_json(&config.to_json()).unwrap(), config);
}

#[test]
fn duplicate_context_fields_are_rejected() {
    for fragment in [
        r#""context_management":{},"context_management":{}"#,
        r#""context_management":{"trigger_percent":65,"trigger_percent":70}"#,
        r#""context_management":{"target_percent":10,"target_percent":20}"#,
        r#""context_management":{"strategy":"summarize","strategy":"truncate"}"#,
    ] {
        let text = format!(r#"{{"revision":2,"providers":[],"models":[],{fragment}}}"#);
        assert!(UserConfig::from_json(&text).is_err());
    }
    let text = document().to_string().replace(
        "\"default_model\":\"default-model\"",
        "\"default_model\":\"default-model\",\"context_window_tokens\":1,\"context_window_tokens\":2",
    );
    assert!(UserConfig::from_json(&text).is_err());
}
