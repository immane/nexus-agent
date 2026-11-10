#![forbid(unsafe_code)]

//! Offline provider task-reasoning configuration regressions.

use nexus_config::{
    AdapterKind, ConfigErrorKind, CredentialRef, ProtocolKind, ProviderProfile, UserConfig,
};
use nexus_core::ReasoningEffort;
use serde_json::{Value, json};

fn document() -> Value {
    json!({
        "revision": 2,
        "providers": [{
            "id": "local", "display_name": "Local", "adapter": "direct",
            "protocol": "chat_completions", "endpoint": "http://localhost:11434/v1",
            "credential": {"env": "LOCAL_KEY"}, "default_model": "local-model"
        }],
        "models": [], "favourites": [], "recent": []
    })
}

#[test]
fn omitted_effort_preserves_legacy_defaults_and_canonical_json() {
    let profile = ProviderProfile::new(
        "local",
        "Local",
        AdapterKind::Direct,
        None,
        CredentialRef::env_var("LOCAL_KEY").unwrap(),
        "local-model",
    )
    .unwrap();
    assert_eq!(profile.task_reasoning_effort, None);
    for revision in [1, 2] {
        let mut doc = document();
        doc["revision"] = json!(revision);
        if revision == 1 {
            doc["providers"][0]
                .as_object_mut()
                .unwrap()
                .remove("protocol");
        }
        let config = UserConfig::from_json(&doc.to_string()).unwrap();
        assert_eq!(config.providers()[0].task_reasoning_effort, None);
        let encoded = config.to_json();
        assert!(!encoded.contains("task_reasoning_effort"));
        assert!(!encoded.contains("show_reasoning"));
        assert_eq!(UserConfig::from_json(&encoded).unwrap(), config);
        if revision == 2 {
            assert_eq!(serde_json::from_str::<Value>(&encoded).unwrap(), doc);
        }
    }
}

#[test]
fn supported_efforts_round_trip_independently_of_reasoning_visibility() {
    for (wire, effort) in [
        ("none", ReasoningEffort::Disabled),
        ("low", ReasoningEffort::Low),
    ] {
        for show in [false, true] {
            let mut doc = document();
            doc["providers"][0]["task_reasoning_effort"] = json!(wire);
            doc["show_reasoning"] = json!(show);
            let config = UserConfig::from_json(&doc.to_string()).unwrap();
            assert_eq!(config.providers()[0].task_reasoning_effort, Some(effort));
            assert_eq!(config.show_reasoning(), show);
            let encoded = config.to_json();
            let value: Value = serde_json::from_str(&encoded).unwrap();
            assert_eq!(value["providers"][0]["task_reasoning_effort"], json!(wire));
            assert_eq!(UserConfig::from_json(&encoded).unwrap(), config);
        }
    }
}

#[test]
fn invalid_efforts_fail_with_static_diagnostics() {
    for value in [
        json!(null),
        json!(false),
        json!(1),
        json!([]),
        json!({}),
        json!(""),
        json!("disabled"),
        json!("medium"),
        json!("high"),
        json!("LOW"),
        json!(" none"),
        json!("private-invalid-value"),
    ] {
        let mut doc = document();
        doc["providers"][0]["task_reasoning_effort"] = value;
        let error = UserConfig::from_json(&doc.to_string()).unwrap_err();
        assert_eq!(error.kind(), ConfigErrorKind::Invalid);
        assert_eq!(error.message(), "task reasoning effort is invalid");
    }
}

#[test]
fn explicit_efforts_require_direct_chat_completions_in_json_and_admission() {
    for (adapter, protocol) in [
        (AdapterKind::Relay, ProtocolKind::ChatCompletions),
        (AdapterKind::Direct, ProtocolKind::OpenAiResponses),
        (AdapterKind::Direct, ProtocolKind::AnthropicMessages),
    ] {
        let mut doc = document();
        doc["providers"][0]["adapter"] = json!(adapter.name());
        doc["providers"][0]["protocol"] = json!(protocol.name());
        // Unsupported protocols/routing remain valid when no effort is configured.
        assert!(UserConfig::from_json(&doc.to_string()).is_ok());
        for (wire, effort) in [
            ("none", ReasoningEffort::Disabled),
            ("low", ReasoningEffort::Low),
        ] {
            doc["providers"][0]["task_reasoning_effort"] = json!(wire);
            let error = UserConfig::from_json(&doc.to_string()).unwrap_err();
            assert_eq!(error.kind(), ConfigErrorKind::Invalid);
            assert_eq!(
                error.message(),
                "task reasoning effort requires direct chat completions"
            );

            let mut profile = ProviderProfile::new_with_protocol(
                "local",
                "Local",
                adapter,
                protocol,
                None,
                CredentialRef::env_var("LOCAL_KEY").unwrap(),
                "local-model",
            )
            .unwrap();
            profile.task_reasoning_effort = Some(effort);
            let mut config = UserConfig::default_config();
            assert_eq!(config.add_provider(profile.clone()).unwrap_err(), error);
            assert_eq!(config.upsert_provider(profile).unwrap_err(), error);
        }
    }
}
