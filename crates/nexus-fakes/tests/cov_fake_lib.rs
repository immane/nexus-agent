#![forbid(unsafe_code)]

//! Public-surface coverage for the `nexus-fakes` crate root.
//!
//! Every root re-export is imported once, so a missing or renamed item fails
//! compilation, then proven to alias its defining module. Every named
//! provider and tool constructor is built and driven to a terminal outcome
//! through the public ports, the gate and store are constructed both ways,
//! and each fake self-identifies as a test-only double rather than a
//! production adapter. Deterministic: fixed scripts, pre-released gates, no
//! threads, no I/O, no network.

use std::time::Duration;

use nexus_core::store::StoredMessage;
use nexus_core::{
    ApprovedScope, CallId, DEFAULT_ADAPTER_IDENTITY, EffectState, ErrorCategory, Evidence,
    ExecutionStatus, FinishReason, M0_REVISION, ModelRequest, NormalizedArgs, PersistenceState,
    ProviderContext, ProviderEvent, ProviderPort, RunId, STORE_FORMAT_REVISION, SessionCheckpoint,
    SessionId, SessionStore, ToolCall, ToolContext, ToolId, ToolIntentRecord, ToolOutcomeRecord,
    ToolPort, TurnId,
};
use nexus_fakes::{
    EphemeralStore, FRAGMENTED_TEXT, FakeGate, FakeProvider, FakeTool, FakeToolCallRecord,
    candidate, reassemble_bytes, stop_turn, tool_turn,
};

fn run_id() -> RunId {
    RunId::new("run-1").expect("valid run id")
}

fn turn_id() -> TurnId {
    TurnId::new("turn-1").expect("valid turn id")
}

fn request() -> ModelRequest {
    ModelRequest::new(run_id(), turn_id(), "fake-profile", vec![], None, 1024)
        .expect("valid request builds")
}

fn live_context() -> ProviderContext {
    ProviderContext::new(Duration::from_secs(60), false, None)
}

fn cancelled_context() -> ProviderContext {
    ProviderContext::new(Duration::from_secs(60), true, None)
}

fn tool_call(call_id: &str, tool_name: &str, args: &str) -> ToolCall {
    ToolCall::new(
        run_id(),
        turn_id(),
        CallId::new(call_id).expect("valid call id"),
        ToolId::new(tool_name, M0_REVISION).expect("valid tool id"),
        NormalizedArgs::new(args).expect("valid normalized args"),
    )
}

fn tool_context(budget: usize, scope: &str) -> ToolContext {
    ToolContext::new(
        budget,
        Duration::from_secs(60),
        false,
        ApprovedScope::new(scope).expect("valid scope builds"),
    )
    .expect("valid tool context builds")
}

/// Asserts exactly one terminal event and that it is last.
fn terminal(events: &[ProviderEvent]) -> &ProviderEvent {
    assert!(
        !events.is_empty(),
        "an invocation yields at least one event"
    );
    let terminals: Vec<&ProviderEvent> =
        events.iter().filter(|event| event.is_terminal()).collect();
    assert_eq!(terminals.len(), 1, "exactly one terminal event");
    assert_eq!(
        events.iter().position(ProviderEvent::is_terminal),
        Some(events.len() - 1),
        "terminal event is last"
    );
    terminals[0]
}

/// Compile-time pin that two public paths name the same type.
fn same_type<T>(_: &T, _: &T) {}

fn assert_provider_port<T: ProviderPort>(_: &T) {}
fn assert_tool_port<T: ToolPort>(_: &T) {}
fn assert_session_store<T: SessionStore>(_: &T) {}

#[test]
fn root_reexports_alias_their_defining_modules() {
    let provider = FakeProvider::new(vec![]);
    same_type(&provider, &nexus_fakes::provider::FakeProvider::new(vec![]));
    let gate = FakeGate::new();
    same_type(&gate, &nexus_fakes::provider::FakeGate::new());
    let store = EphemeralStore::new();
    same_type(&store, &nexus_fakes::store::EphemeralStore::new());
    let tool = FakeTool::read_only();
    same_type(&tool, &nexus_fakes::tool::FakeTool::read_only());

    let record = FakeToolCallRecord {
        call: CallId::new("call-1").expect("valid call id"),
        args: r#"{"path":"src"}"#.to_owned(),
        scope: ApprovedScope::new("project-read").expect("valid scope builds"),
    };
    let module_record = nexus_fakes::tool::FakeToolCallRecord {
        call: record.call.clone(),
        args: record.args.clone(),
        scope: record.scope.clone(),
    };
    same_type(&record, &module_record);
    assert_eq!(record, module_record);

    assert_eq!(FRAGMENTED_TEXT, nexus_fakes::provider::FRAGMENTED_TEXT);
    assert_eq!(
        candidate("item-1", "prov-ref-1", "host_read", r#"{"path":"src"}"#),
        nexus_fakes::provider::candidate("item-1", "prov-ref-1", "host_read", r#"{"path":"src"}"#)
    );
    let chunks: [&[u8]; 2] = [b"ab", b"cd"];
    assert_eq!(
        reassemble_bytes(&chunks).expect("valid UTF-8 reassembles"),
        nexus_fakes::provider::reassemble_bytes(&chunks).expect("valid UTF-8 reassembles")
    );
    assert_eq!(stop_turn("hi"), nexus_fakes::provider::stop_turn("hi"));
    assert_eq!(
        tool_turn(vec![candidate("i", "p", "t", "{}")]),
        nexus_fakes::provider::tool_turn(vec![candidate("i", "p", "t", "{}")])
    );
}

#[test]
fn every_provider_constructor_builds_a_terminal_script() {
    let constructors: Vec<(&str, FakeProvider)> = vec![
        ("new", FakeProvider::new(vec![stop_turn("one")])),
        ("new-empty", FakeProvider::new(vec![])),
        ("gated", FakeProvider::gated(vec![stop_turn("two")])),
        (
            "fragmented_text_then_two_calls",
            FakeProvider::fragmented_text_then_two_calls(),
        ),
        ("interleaved_items", FakeProvider::interleaved_items()),
        (
            "duplicate_reference_failure",
            FakeProvider::duplicate_reference_failure(),
        ),
        (
            "duplicate_reference_success",
            FakeProvider::duplicate_reference_success(),
        ),
        (
            "conflicting_reference_failure",
            FakeProvider::conflicting_reference_failure(),
        ),
        (
            "conflicting_reference_success",
            FakeProvider::conflicting_reference_success(),
        ),
        (
            "malformed_json_failure",
            FakeProvider::malformed_json_failure(),
        ),
        (
            "malformed_json_success",
            FakeProvider::malformed_json_success(),
        ),
        ("truncated_stream", FakeProvider::truncated_stream()),
        ("unfinished_success", FakeProvider::unfinished_success()),
        (
            "oversized_progress_success",
            FakeProvider::oversized_progress_success(),
        ),
        ("refusal", FakeProvider::refusal()),
        (
            "usage_provisional_then_final",
            FakeProvider::usage_provisional_then_final(),
        ),
        (
            "continuation_round_trip",
            FakeProvider::continuation_round_trip(),
        ),
    ];
    for (label, provider) in &constructors {
        assert_provider_port(provider);
        if let Some(gate) = provider.gate() {
            gate.release();
        }
        let events = provider.stream(&request(), &live_context());
        terminal(&events);
        assert_eq!(provider.call_count(), 1, "{label}: one call observed");
        assert_eq!(provider.requests().len(), 1, "{label}: request recorded");
    }
    assert_eq!(constructors.len(), 17, "every named constructor is pinned");
}

#[test]
fn provider_scripts_and_helpers_keep_their_documented_shape() {
    let built = candidate("item-1", "prov-ref-1", "host_read", r#"{"path":"src"}"#);
    assert_eq!(built.item_key(), "item-1");
    assert_eq!(built.provider_ref(), "prov-ref-1");
    assert_eq!(built.tool_name(), "host_read");
    assert_eq!(built.arguments_json(), r#"{"path":"src"}"#);

    let stop = stop_turn("answer");
    assert_eq!(stop.len(), 2);
    assert!(matches!(
        terminal(&stop),
        ProviderEvent::TurnFinished(finished) if finished.reason() == FinishReason::Stop
    ));
    let text: String = stop
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "answer");

    let calls = tool_turn(vec![built.clone()]);
    assert_eq!(calls.len(), 2);
    assert!(matches!(
        terminal(&calls),
        ProviderEvent::TurnFinished(finished) if finished.reason() == FinishReason::ToolCalls
    ));

    let provider = FakeProvider::fragmented_text_then_two_calls();
    let events = provider.stream(&request(), &live_context());
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, FRAGMENTED_TEXT);
    let refs: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ToolCallReady(candidate) => Some(candidate.provider_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(refs.len(), 2);

    let raw = FRAGMENTED_TEXT.as_bytes();
    for split in 1..raw.len() {
        let halves: [&[u8]; 2] = [&raw[..split], &raw[split..]];
        assert_eq!(
            reassemble_bytes(&halves).expect("every split reassembles"),
            FRAGMENTED_TEXT,
            "split at byte {split}"
        );
    }
    let invalid: [&[u8]; 1] = [&[0xff, 0xfe]];
    assert!(reassemble_bytes(&invalid).is_err(), "invalid UTF-8 fails");
}

#[test]
fn cancelled_context_overrides_the_script_without_consuming_it() {
    let provider = FakeProvider::new(vec![stop_turn("kept")]);
    let cancelled = provider.stream(&request(), &cancelled_context());
    assert_eq!(cancelled.len(), 1);
    assert!(matches!(
        cancelled[0],
        ProviderEvent::Failed(ref error) if error.category() == ErrorCategory::Cancelled
    ));
    assert_eq!(provider.call_count(), 1);
    assert_eq!(provider.requests().len(), 1);

    let live = provider.stream(&request(), &live_context());
    assert!(matches!(
        terminal(&live),
        ProviderEvent::TurnFinished(finished) if finished.reason() == FinishReason::Stop
    ));
}

#[test]
fn exhausted_script_fails_explicitly() {
    let provider = FakeProvider::new(vec![]);
    let events = provider.stream(&request(), &live_context());
    match terminal(&events) {
        ProviderEvent::Failed(error) => {
            assert_eq!(error.category(), ErrorCategory::Protocol);
        }
        other => panic!("exhausted script must fail explicitly, got {other:?}"),
    }
}

#[test]
fn every_tool_constructor_executes_via_the_port() {
    let budget = 1024;
    let cases: Vec<(&str, FakeTool, ExecutionStatus, EffectState, bool)> = vec![
        (
            "read_only",
            FakeTool::read_only(),
            ExecutionStatus::Succeeded,
            EffectState::KnownNotApplied,
            false,
        ),
        (
            "mutation",
            FakeTool::mutation(),
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            false,
        ),
        (
            "command",
            FakeTool::command(),
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            false,
        ),
        (
            "failing",
            FakeTool::failing("host_write"),
            ExecutionStatus::Failed,
            EffectState::Unknown,
            false,
        ),
        (
            "oversized",
            FakeTool::oversized("host_read", budget + 8),
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            true,
        ),
        (
            "delayed",
            FakeTool::delayed("host_read", Duration::from_millis(1)),
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            false,
        ),
        (
            "gated",
            FakeTool::gated("host_read"),
            ExecutionStatus::Succeeded,
            EffectState::KnownNotApplied,
            false,
        ),
        (
            "gated_mutation",
            FakeTool::gated_mutation("host_write"),
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            false,
        ),
    ];
    for (label, tool, status, effect, truncated) in &cases {
        assert_tool_port(tool);
        if let Some(gate) = tool.gate() {
            gate.release();
        }
        let call = tool_call("call-1", tool.describe().id().name(), r#"{"path":"src"}"#);
        let outcome = tool.execute(&call, &tool_context(budget, "project-read"));
        assert_eq!(outcome.status(), *status, "{label}");
        assert_eq!(outcome.effect(), *effect, "{label}");
        assert_eq!(outcome.is_truncated(), *truncated, "{label}");
        assert_eq!(tool.execution_count(), 1, "{label}");
        let log = tool.log();
        assert_eq!(log.len(), 1, "{label}");
        assert_eq!(log[0].call.as_str(), "call-1", "{label}");
        assert_eq!(log[0].args, r#"{"path":"src"}"#, "{label}");
        assert_eq!(log[0].scope.as_str(), "project-read", "{label}");
    }
    assert_eq!(cases.len(), 8, "every named tool constructor is pinned");
    assert!(
        FakeTool::read_only().gate().is_none(),
        "ungated tools expose no gate"
    );
}

#[test]
fn gate_constructs_and_coordinates_deterministically() {
    let gate = FakeGate::new();
    assert!(!gate.is_entered());
    assert!(!gate.is_released());
    assert!(
        !gate.wait_entered(Duration::ZERO),
        "an unentered gate never reports entry"
    );
    gate.release();
    assert!(gate.is_released());
    assert!(!gate.is_entered());

    let default_gate = FakeGate::default();
    assert!(!default_gate.is_released());
    default_gate.release();
    assert!(default_gate.is_released());

    let clone = gate.clone();
    assert!(clone.is_released(), "clones share gate state");
    assert_eq!(FakeGate::RELEASE_TIMEOUT, Duration::from_secs(10));
    assert!(!format!("{gate:?}").is_empty());
}

#[test]
fn ephemeral_store_constructs_self_identifies_and_round_trips() {
    let mut store = EphemeralStore::new();
    assert_session_store(&store);
    assert!(!store.is_durable(), "the double never claims durability");
    assert_eq!(store.durability(), PersistenceState::Ephemeral);
    assert_eq!(store.format_revision(), STORE_FORMAT_REVISION);
    assert_eq!(store.format_revision(), 0);
    assert!(!store.fail_next_write());
    assert!(store.intents().is_empty());
    assert!(store.outcomes().is_empty());
    assert!(
        store
            .list_sessions(0)
            .expect("bounded list works")
            .is_empty()
    );
    assert!(
        store
            .load_session(&SessionId::new("sess-missing").expect("valid session id"))
            .is_err()
    );

    let default_store = EphemeralStore::default();
    assert!(!default_store.is_durable());

    let checkpoint = SessionCheckpoint::new(
        SessionId::new("sess-1").expect("valid session id"),
        STORE_FORMAT_REVISION,
        1,
        vec![StoredMessage {
            source: "user".to_owned(),
            text: "hello".to_owned(),
            complete: true,
        }],
        "fake-profile",
    )
    .expect("valid checkpoint builds");
    store.save_checkpoint(&checkpoint).expect("save works");
    let loaded = store
        .load_session(&SessionId::new("sess-1").expect("valid session id"))
        .expect("load works");
    assert_eq!(loaded, checkpoint);
    let listed = store.list_sessions(10).expect("list works");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].session, checkpoint.session().clone());

    let intent = ToolIntentRecord {
        run: run_id(),
        call: CallId::new("call-1").expect("valid call id"),
        tool: ToolId::new("host_write", M0_REVISION).expect("valid tool id"),
        args: NormalizedArgs::new(r#"{"path":"dst"}"#).expect("valid normalized args"),
        scope: ApprovedScope::new("project-write").expect("valid scope builds"),
    };
    store.record_intent(&intent).expect("intent records");
    assert_eq!(store.intents(), std::slice::from_ref(&intent));

    let outcome = ToolOutcomeRecord {
        run: run_id(),
        call: CallId::new("call-1").expect("valid call id"),
        status: ExecutionStatus::Succeeded,
        effect: EffectState::KnownApplied,
        evidence: Evidence::HostObserved,
        truncated: false,
    };
    store.record_outcome(&outcome).expect("outcome records");
    assert_eq!(store.outcomes(), std::slice::from_ref(&outcome));

    store.set_fail_next_write(true);
    assert!(store.fail_next_write());
    let error = store.save_checkpoint(&checkpoint).unwrap_err();
    assert_eq!(error.category(), ErrorCategory::StorageFailure);
    assert!(!store.fail_next_write(), "single-shot interruption disarms");
    store
        .save_checkpoint(&checkpoint)
        .expect("retry after disarm works");
}

#[test]
fn fakes_self_identify_as_test_only_doubles() {
    let provider = FakeProvider::new(vec![]);
    assert_eq!(provider.adapter_identity(), "fake-adapter");
    assert_ne!(
        provider.adapter_identity(),
        DEFAULT_ADAPTER_IDENTITY,
        "the fake overrides the reserved production placeholder"
    );
    assert_eq!(provider.continuation_scope("profile-a"), "fake-model");
    assert_eq!(
        provider.continuation_scope("profile-b"),
        provider.continuation_scope("profile-a"),
        "scope stays stable across profiles"
    );
    let capabilities = provider.capabilities();
    assert!(capabilities.text && capabilities.streaming && capabilities.tool_calls);
    assert_eq!(capabilities.max_context_items, None);
    assert_eq!(capabilities.max_output_bytes, None);

    for (tool, name) in [
        (FakeTool::read_only(), "host_read"),
        (FakeTool::mutation(), "host_write"),
        (FakeTool::command(), "host_exec"),
    ] {
        assert_eq!(tool.describe().id().name(), name);
        assert_eq!(tool.describe().id().revision(), M0_REVISION);
        assert!(
            tool.describe().description().starts_with("fake "),
            "tool description self-identifies as a fake"
        );
    }

    assert!(!EphemeralStore::new().is_durable());
    assert_eq!(
        EphemeralStore::new().durability(),
        PersistenceState::Ephemeral
    );
    assert!(!FRAGMENTED_TEXT.is_empty());
}
