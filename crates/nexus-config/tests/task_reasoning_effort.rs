#![forbid(unsafe_code)]

//! Offline layered task-reasoning configuration regressions.

use nexus_config::{
    AdapterKind, ConfigErrorKind, CredentialRef, ModelEntry, ProtocolKind, ProviderProfile,
    UserConfig, summary,
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

fn document_with_model() -> Value {
    let mut doc = document();
    doc["models"] = json!([{"id": "model", "provider": "local", "name": "local-model"}]);
    doc
}

#[test]
fn default_document_and_summary_remain_unchanged() {
    let config = UserConfig::default_config();
    let expected = r#"{"favourites":[],"models":[],"providers":[],"recent":[],"revision":2}"#;
    assert_eq!(config.task_reasoning_effort(), None);
    assert_eq!(config.to_json(), expected);
    assert_eq!(summary(&config).to_string(), expected);

    let doc = document_with_model();
    let config = UserConfig::from_json(&doc.to_string()).unwrap();
    assert_eq!(config.to_json(), doc.to_string());
    assert_eq!(config.models()[0].task_reasoning_effort, None);
    assert_eq!(
        ModelEntry::new("new", "local", "name")
            .unwrap()
            .task_reasoning_effort,
        None
    );
    let mut expected_summary = doc;
    expected_summary["providers"][0]["context_window_tokens"] = Value::Null;
    expected_summary["providers"][0]["enabled"] = Value::Bool(true);
    expected_summary["models"][0]["context_window_tokens"] = Value::Null;
    expected_summary["models"][0]["max_output_tokens"] = Value::Null;
    assert_eq!(summary(&config), expected_summary);
}

#[test]
fn every_layer_round_trips_inherit_low_and_none_canonically() {
    for path in [
        "/task_reasoning_effort",
        "/providers/0/task_reasoning_effort",
        "/models/0/task_reasoning_effort",
    ] {
        for (wire, effort) in [
            ("inherit", None),
            ("low", Some(ReasoningEffort::Low)),
            ("none", Some(ReasoningEffort::Disabled)),
        ] {
            let mut doc = document_with_model();
            let (parent, _) = path.rsplit_once('/').unwrap();
            doc.pointer_mut(parent).unwrap()["task_reasoning_effort"] = json!(wire);
            let config = UserConfig::from_json(&doc.to_string()).unwrap();
            let actual = match parent {
                "" => config.task_reasoning_effort(),
                "/providers/0" => config.providers()[0].task_reasoning_effort,
                _ => config.models()[0].task_reasoning_effort,
            };
            assert_eq!(actual, effort);
            assert_eq!(
                config
                    .task_reasoning_effort_for("local", Some("model"))
                    .unwrap(),
                effort
            );
            let encoded = config.to_json();
            let canonical: Value = serde_json::from_str(&encoded).unwrap();
            let redacted = summary(&config);
            if effort.is_some() {
                assert_eq!(canonical.pointer(path), Some(&json!(wire)));
                assert_eq!(redacted.pointer(path), Some(&json!(wire)));
            } else {
                assert!(canonical.pointer(path).is_none());
                assert!(redacted.pointer(path).is_none());
                assert_eq!(canonical, document_with_model());
            }
            assert_eq!(UserConfig::from_json(&encoded).unwrap(), config);
        }
    }
}

#[test]
fn model_provider_global_precedence_and_reset_to_inherit() {
    for global in [
        None,
        Some(ReasoningEffort::Low),
        Some(ReasoningEffort::Disabled),
    ] {
        for provider in [
            None,
            Some(ReasoningEffort::Low),
            Some(ReasoningEffort::Disabled),
        ] {
            for model in [
                None,
                Some(ReasoningEffort::Low),
                Some(ReasoningEffort::Disabled),
            ] {
                let mut config = UserConfig::from_json(&document_with_model().to_string()).unwrap();
                config.set_task_reasoning_effort(global);
                let mut profile = config.providers()[0].clone();
                profile.task_reasoning_effort = provider;
                config.upsert_provider(profile).unwrap();
                let mut entry = config.models()[0].clone();
                entry.task_reasoning_effort = model;
                config.upsert_model(entry.clone()).unwrap();
                assert_eq!(
                    config
                        .task_reasoning_effort_for("local", Some("model"))
                        .unwrap(),
                    model.or(provider).or(global)
                );
                assert_eq!(
                    config.task_reasoning_effort_for("local", None).unwrap(),
                    provider.or(global)
                );
                entry.task_reasoning_effort = None;
                config.upsert_model(entry).unwrap();
                let mut profile = config.providers()[0].clone();
                profile.task_reasoning_effort = None;
                config.upsert_provider(profile).unwrap();
                config.set_task_reasoning_effort(None);
                assert_eq!(
                    config
                        .task_reasoning_effort_for("local", Some("model"))
                        .unwrap(),
                    None
                );
            }
        }
    }
}

#[test]
fn invalid_efforts_are_rejected_at_every_layer() {
    for parent in ["", "/providers/0", "/models/0"] {
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
            json!(" inherit"),
            json!("private-invalid-value"),
        ] {
            let mut doc = document_with_model();
            doc.pointer_mut(parent).unwrap()["task_reasoning_effort"] = value;
            let error = UserConfig::from_json(&doc.to_string()).unwrap_err();
            assert_eq!(error.kind(), ConfigErrorKind::Invalid);
            assert_eq!(error.message(), "task reasoning effort is invalid");
        }
    }
}

#[test]
fn resolution_validates_ids_existence_and_provider_membership() {
    let mut config = UserConfig::from_json(&document_with_model().to_string()).unwrap();
    let mut other = config.providers()[0].clone();
    other.id = "other".into();
    config.add_provider(other).unwrap();
    config.set_task_reasoning_effort(Some(ReasoningEffort::Low));
    for (provider, model, message) in [
        ("bad id", None, "provider id is invalid"),
        ("missing", None, "provider is unknown"),
        ("local", Some("bad id"), "model id is invalid"),
        ("local", Some("missing"), "model is unknown"),
        ("other", Some("model"), "model does not belong to provider"),
    ] {
        let error = config
            .task_reasoning_effort_for(provider, model)
            .unwrap_err();
        assert_eq!(error.kind(), ConfigErrorKind::Invalid);
        assert_eq!(error.message(), message);
    }
}

#[test]
fn unsupported_model_overrides_and_provider_updates_fail_atomically() {
    for (adapter, protocol) in [
        (AdapterKind::Relay, ProtocolKind::ChatCompletions),
        (AdapterKind::Direct, ProtocolKind::OpenAiResponses),
        (AdapterKind::Direct, ProtocolKind::AnthropicMessages),
    ] {
        for effort in [ReasoningEffort::Low, ReasoningEffort::Disabled] {
            let mut doc = document_with_model();
            doc["providers"][0]["adapter"] = json!(adapter.name());
            doc["providers"][0]["protocol"] = json!(protocol.name());
            doc["task_reasoning_effort"] = json!(effort.as_str());
            doc["providers"][0]["task_reasoning_effort"] = json!("inherit");
            doc["models"][0]["task_reasoning_effort"] = json!("inherit");
            let mut config = UserConfig::from_json(&doc.to_string()).unwrap();
            // Global settings may coexist with unsupported providers; selection owns support checks.
            assert_eq!(
                config
                    .task_reasoning_effort_for("local", Some("model"))
                    .unwrap(),
                Some(effort)
            );
            let before = config.clone();
            let mut entry = config.models()[0].clone();
            entry.task_reasoning_effort = Some(effort);
            assert!(config.upsert_model(entry.clone()).is_err());
            assert_eq!(config, before);
            entry.id = "new".into();
            assert!(config.add_model(entry.clone()).is_err());
            assert_eq!(config, before);
            assert!(config.upsert_model(entry).is_err());
            assert_eq!(config, before);
            doc["models"][0]["task_reasoning_effort"] = json!(effort.as_str());
            assert!(UserConfig::from_json(&doc.to_string()).is_err());

            let mut supported = UserConfig::from_json(&document_with_model().to_string()).unwrap();
            let mut entry = supported.models()[0].clone();
            entry.task_reasoning_effort = Some(effort);
            supported.upsert_model(entry.clone()).unwrap();
            let before = supported.clone();
            let mut profile = supported.providers()[0].clone();
            profile.adapter = adapter;
            profile.protocol = protocol;
            assert!(supported.upsert_provider(profile).is_err());
            assert_eq!(supported, before);

            // Moving an existing override to an unsupported provider also fails without mutation.
            let mut other = config.providers()[0].clone();
            other.id = "other".into();
            supported.add_provider(other).unwrap();
            let before = supported.clone();
            entry.provider = "other".into();
            assert!(supported.upsert_model(entry).is_err());
            assert_eq!(supported, before);
        }
    }
}
