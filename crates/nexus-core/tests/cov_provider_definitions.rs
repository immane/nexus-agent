#![forbid(unsafe_code)]

//! Coverage hardening: `ModelRequest` construction bounds, tool-definition
//! accounting, provider compatibility defaults, and credential references.
//!
//! Pinned from the public API only:
//! - a model output budget is required and finite: zero (the "missing" value)
//!   and effectively infinite values are rejected, while one byte and the
//!   documented cap are accepted;
//! - the enabled-tool count follows the per-turn declared-call bound, and the
//!   tool-definition count follows `MAX_TOOL_DEFINITIONS` inclusively;
//! - the definition aggregate accounts identity name, description, and schema
//!   bytes; M0 per-spec and count bounds keep every valid set below
//!   `MAX_TOOL_DEFINITION_BYTES`, so the reachable maximum is pinned as a
//!   drift detector for the otherwise unreachable aggregate rejection;
//! - the `ProviderPort` identity and scope defaults are stable exact-equality
//!   placeholders that compose with `ContinuationData::is_compatible_with`;
//! - `CredentialRef` carries a bounded reference name only, and the provider
//!   context exposes that name and never a secret value.
//!
//! Everything is deterministic: no sleeps, threads, I/O, or wall-clock reads.

use std::time::Duration;

use nexus_core::ids::MAX_ID_LEN;
use nexus_core::provider::{MAX_CREDENTIAL_REF_LEN, MAX_PROFILE_LEN};
use nexus_core::tool::{MAX_SCHEMA_BYTES, MAX_TOOL_DESCRIPTION_LEN};
use nexus_core::{
    AgentError, ContinuationData, CredentialRef, DEFAULT_ADAPTER_IDENTITY, ErrorCategory, Limits,
    M0_REVISION, MAX_TOOL_DEFINITION_BYTES, MAX_TOOL_DEFINITIONS, ModelRequest,
    ProviderCapabilities, ProviderContext, ProviderEvent, ProviderPort, RetryGuidance, RunId,
    ToolId, ToolSpec, TurnId,
};

const BUDGET: usize = 1024;

fn run() -> RunId {
    RunId::new("run-1").expect("valid run id")
}

fn turn() -> TurnId {
    TurnId::new("turn-1").expect("valid turn id")
}

fn tool_id(name: impl Into<String>) -> ToolId {
    ToolId::new(name, M0_REVISION).expect("valid tool id")
}

fn spec(name: impl Into<String>) -> ToolSpec {
    ToolSpec::new(tool_id(name), "read files", r#"{"type":"object"}"#).expect("valid spec")
}

fn request() -> ModelRequest {
    ModelRequest::new(
        run(),
        turn(),
        "profile-a",
        vec![tool_id("host_read")],
        None,
        BUDGET,
    )
    .expect("valid request")
}

fn request_with_budget(budget: usize) -> Result<ModelRequest, AgentError> {
    ModelRequest::new(run(), turn(), "profile-a", vec![], None, budget)
}

fn max_object_schema() -> String {
    let mut schema = String::with_capacity(MAX_SCHEMA_BYTES);
    schema.push('{');
    schema.push_str(&"x".repeat(MAX_SCHEMA_BYTES - 2));
    schema.push('}');
    schema
}

#[test]
fn missing_budget_is_rejected_as_invalid_input() {
    let error = request_with_budget(0).expect_err("zero is the missing budget, never infinity");
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
}

#[test]
fn infinite_and_above_cap_budgets_are_rejected() {
    let cap = Limits::M0_TEST_TOOL_OUTPUT_BYTES;
    for budget in [usize::MAX, cap + 1, cap.saturating_mul(2)] {
        let error =
            request_with_budget(budget).expect_err("an unbounded output budget must be rejected");
        assert_eq!(
            error.category(),
            ErrorCategory::InvalidInput,
            "budget {budget}"
        );
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry, "budget {budget}");
    }
}

#[test]
fn budget_boundaries_are_finite_and_inclusive() {
    let minimum = request_with_budget(1).expect("one byte is a finite budget");
    assert_eq!(minimum.output_budget_bytes(), 1);

    let cap = Limits::M0_TEST_TOOL_OUTPUT_BYTES;
    let maximum = request_with_budget(cap).expect("the documented cap is inclusive");
    assert_eq!(maximum.output_budget_bytes(), cap);
}

#[test]
fn profile_boundaries_are_enforced() {
    let max_profile = "p".repeat(MAX_PROFILE_LEN);
    let request = ModelRequest::new(run(), turn(), max_profile.clone(), vec![], None, BUDGET)
        .expect("profile at the bound is accepted");
    assert_eq!(request.profile(), max_profile);

    let error = ModelRequest::new(run(), turn(), "", vec![], None, BUDGET)
        .expect_err("an empty profile has no compatibility scope");
    assert_eq!(error.category(), ErrorCategory::InvalidInput);

    let error = ModelRequest::new(
        run(),
        turn(),
        "p".repeat(MAX_PROFILE_LEN + 1),
        vec![],
        None,
        BUDGET,
    )
    .expect_err("an overlong profile is rejected");
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
}

#[test]
fn enabled_tools_follow_the_per_turn_call_bound() {
    assert_eq!(
        MAX_TOOL_DEFINITIONS,
        Limits::M0_TEST_TOOL_CALLS_PER_TURN as usize,
        "definition and per-turn call bounds are the same M0 value"
    );

    let exact: Vec<ToolId> = (0..MAX_TOOL_DEFINITIONS)
        .map(|index| tool_id(format!("tool_{index}")))
        .collect();
    ModelRequest::new(run(), turn(), "profile-a", exact, None, BUDGET)
        .expect("exactly the per-turn bound is accepted");

    let too_many: Vec<ToolId> = (0..=MAX_TOOL_DEFINITIONS)
        .map(|index| tool_id(format!("tool_{index}")))
        .collect();
    let error = ModelRequest::new(run(), turn(), "profile-a", too_many, None, BUDGET)
        .expect_err("one above the per-turn bound is rejected");
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
}

#[test]
fn request_identities_round_trip() {
    let request = request();
    assert_eq!(request.run(), &run());
    assert_eq!(request.turn(), &turn());
    assert_eq!(request.profile(), "profile-a");
    assert_eq!(request.enabled_tools(), [tool_id("host_read")].as_slice());
    assert_eq!(request.continuation(), None);
    assert_eq!(request.output_budget_bytes(), BUDGET);
    assert!(request.tool_definitions().is_empty());
}

#[test]
fn definition_count_bound_is_inclusive_and_replaces() {
    let exact: Vec<ToolSpec> = (0..MAX_TOOL_DEFINITIONS)
        .map(|index| spec(format!("tool_{index}")))
        .collect();
    let installed = request()
        .with_tool_definitions(exact)
        .expect("exactly the definition bound is accepted");
    assert_eq!(installed.tool_definitions().len(), MAX_TOOL_DEFINITIONS);

    let too_many: Vec<ToolSpec> = (0..=MAX_TOOL_DEFINITIONS)
        .map(|index| spec(format!("tool_{index}")))
        .collect();
    let error = request()
        .with_tool_definitions(too_many)
        .expect_err("one above the definition bound is rejected");
    assert_eq!(error.category(), ErrorCategory::ResourceLimit);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);

    let cleared = installed
        .with_tool_definitions(Vec::new())
        .expect("a later install replaces the previous definitions");
    assert!(cleared.tool_definitions().is_empty());
}

#[test]
fn definition_aggregate_counts_identity_description_and_schema() {
    let first = spec("host_read");
    let second = ToolSpec::new(tool_id("host_write"), "write files", r#"{"type":"object"}"#)
        .expect("valid spec");
    let installed = request()
        .with_tool_definitions(vec![first.clone(), second.clone()])
        .expect("small definition set is accepted");
    assert_eq!(installed.tool_definitions(), [first, second].as_slice());

    let accounted: usize = installed
        .tool_definitions()
        .iter()
        .map(|spec| {
            spec.id().name().len() + spec.description().len() + spec.input_schema_json().len()
        })
        .sum();
    assert_eq!(
        accounted,
        "host_read".len()
            + "read files".len()
            + r#"{"type":"object"}"#.len()
            + "host_write".len()
            + "write files".len()
            + r#"{"type":"object"}"#.len(),
        "identity name, description, and schema bytes all count"
    );
}

#[test]
fn reachable_definition_maximum_accounts_identity_name() {
    let max_name = "t".repeat(MAX_ID_LEN);
    let max_description = "d".repeat(MAX_TOOL_DESCRIPTION_LEN);
    let max_schema = max_object_schema();

    let specs: Vec<ToolSpec> = (0..MAX_TOOL_DEFINITIONS)
        .map(|_| {
            ToolSpec::new(
                tool_id(&max_name),
                max_description.clone(),
                max_schema.clone(),
            )
            .expect("maximum-length spec builds")
        })
        .collect();

    let accounted: usize = specs
        .iter()
        .map(|spec| {
            spec.id().name().len() + spec.description().len() + spec.input_schema_json().len()
        })
        .sum();
    let reachable_max =
        MAX_TOOL_DEFINITIONS * (MAX_ID_LEN + MAX_TOOL_DESCRIPTION_LEN + MAX_SCHEMA_BYTES);
    assert_eq!(
        accounted, reachable_max,
        "the aggregate is identity name plus description plus schema"
    );
    assert!(
        reachable_max < MAX_TOOL_DEFINITION_BYTES,
        "M0 per-spec and count bounds keep every valid set below the aggregate cap \
         ({reachable_max} < {MAX_TOOL_DEFINITION_BYTES}); if the cap becomes reachable, \
         add a rejection test at the cap plus one byte"
    );

    let installed = request()
        .with_tool_definitions(specs)
        .expect("the maximum reachable definition set is accepted");
    assert_eq!(installed.tool_definitions().len(), MAX_TOOL_DEFINITIONS);
    for spec in installed.tool_definitions() {
        assert_eq!(spec.id().name().len(), MAX_ID_LEN);
        assert_eq!(spec.description().len(), MAX_TOOL_DESCRIPTION_LEN);
        assert_eq!(spec.input_schema_json().len(), MAX_SCHEMA_BYTES);
    }
}

fn text_capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        text: true,
        streaming: false,
        tool_calls: false,
        structured_output: false,
        usage_reporting: false,
        max_context_items: None,
        max_output_bytes: None,
    }
}

/// Implements only the two required methods; identity and scope come from the
/// trait defaults.
struct DefaultsOnlyProvider;

impl ProviderPort for DefaultsOnlyProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        text_capabilities()
    }

    fn stream(&self, _request: &ModelRequest, _context: &ProviderContext) -> Vec<ProviderEvent> {
        Vec::new()
    }
}

/// Overrides both compatibility labels, as an adapter emitting continuation
/// state must.
struct OverridingProvider;

impl ProviderPort for OverridingProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        text_capabilities()
    }

    fn stream(&self, _request: &ModelRequest, _context: &ProviderContext) -> Vec<ProviderEvent> {
        Vec::new()
    }

    fn adapter_identity(&self) -> &str {
        "acme-adapter"
    }

    fn continuation_scope(&self, profile: &str) -> String {
        format!("acme:{profile}")
    }
}

#[test]
fn adapter_identity_default_is_the_reserved_placeholder() {
    let provider = DefaultsOnlyProvider;
    assert_eq!(provider.adapter_identity(), DEFAULT_ADAPTER_IDENTITY);
    assert_eq!(
        DEFAULT_ADAPTER_IDENTITY, "unknown-adapter",
        "the placeholder is a compatibility-matching label, not a secret or a claim"
    );
}

#[test]
fn continuation_scope_default_is_the_exact_profile_name() {
    let provider = DefaultsOnlyProvider;
    for profile in ["profile-a", "profile-b", "Profile-A", " profile-a "] {
        assert_eq!(
            provider.continuation_scope(profile),
            profile,
            "the default scope copies the profile exactly"
        );
    }
    assert_ne!(
        provider.continuation_scope("profile-a"),
        provider.continuation_scope("profile-b"),
        "distinct profiles never share a default scope"
    );
}

#[test]
fn default_scope_matches_continuation_data_exactly() {
    let provider = DefaultsOnlyProvider;
    let profile = "profile-a";
    let continuation = ContinuationData::new(
        provider.adapter_identity(),
        provider.continuation_scope(profile),
        vec![1, 2, 3],
    )
    .expect("continuation for the default scope builds");
    let request = ModelRequest::new(
        run(),
        turn(),
        profile,
        vec![],
        Some(continuation.clone()),
        BUDGET,
    )
    .expect("request carries the continuation");
    let carried = request.continuation().expect("continuation is carried");

    assert_eq!(carried, &continuation);
    assert!(carried.is_compatible_with(
        provider.adapter_identity(),
        &provider.continuation_scope(request.profile())
    ));
    assert!(!carried.is_compatible_with("acme-adapter", &provider.continuation_scope(profile)));
    assert!(!carried.is_compatible_with(provider.adapter_identity(), "profile-b"));
}

#[test]
fn overrides_replace_defaults_for_compatibility_matching() {
    let defaults = DefaultsOnlyProvider;
    let overriding = OverridingProvider;
    assert_ne!(
        overriding.adapter_identity(),
        defaults.adapter_identity(),
        "an override is not the reserved placeholder"
    );
    assert_ne!(
        overriding.continuation_scope("profile-a"),
        defaults.continuation_scope("profile-a"),
        "an override is not the bare profile name"
    );

    let default_scoped = ContinuationData::new(
        defaults.adapter_identity(),
        defaults.continuation_scope("profile-a"),
        vec![],
    )
    .expect("continuation builds");
    assert!(
        !default_scoped.is_compatible_with(
            overriding.adapter_identity(),
            &overriding.continuation_scope("profile-a")
        ),
        "a default-scoped continuation never matches an overridden adapter"
    );

    let overridden = ContinuationData::new(
        overriding.adapter_identity(),
        overriding.continuation_scope("profile-a"),
        vec![],
    )
    .expect("continuation builds");
    assert!(overridden.is_compatible_with(
        overriding.adapter_identity(),
        &overriding.continuation_scope("profile-a")
    ));
}

#[test]
fn credential_reference_stores_only_a_bounded_name() {
    let name = "payments-prod-key";
    let credential = CredentialRef::new(name).expect("valid reference name");
    assert_eq!(
        credential.as_str(),
        name,
        "only the reference name crosses the boundary"
    );
    assert_eq!(
        CredentialRef::new(name).expect("valid reference name"),
        credential,
        "references compare by name"
    );
    assert_ne!(
        CredentialRef::new("payments-staging-key").expect("valid reference name"),
        credential
    );
}

#[test]
fn credential_reference_boundaries_are_enforced() {
    let maximum = "r".repeat(MAX_CREDENTIAL_REF_LEN);
    assert_eq!(
        CredentialRef::new(maximum.clone())
            .expect("maximum-length reference name is accepted")
            .as_str(),
        maximum
    );

    for invalid in [String::new(), "r".repeat(MAX_CREDENTIAL_REF_LEN + 1)] {
        let error =
            CredentialRef::new(invalid).expect_err("empty and overlong reference names fail");
        assert_eq!(error.category(), ErrorCategory::InvalidInput);
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    }
}

#[test]
fn provider_context_exposes_the_reference_name_without_a_value() {
    let credential = CredentialRef::new("payments-prod-key").expect("valid reference name");
    let configured = ProviderContext::new(Duration::ZERO, false, Some(credential.clone()));
    assert_eq!(configured.credential(), Some(&credential));
    assert_eq!(
        configured
            .credential()
            .expect("configured credential")
            .as_str(),
        "payments-prod-key",
        "the context exposes the name only; CredentialRef has no value accessor"
    );

    let anonymous = ProviderContext::new(Duration::ZERO, false, None);
    assert_eq!(
        anonymous.credential(),
        None,
        "no credential is fabricated when none is configured"
    );
}
