#![forbid(unsafe_code)]

//! Coverage hardening for the [`CallCandidate`] provider-proposal path.
//!
//! All assertions use the exported `nexus-core` API only. A candidate
//! validates provider-scoped identity bounds (item key, provider reference,
//! tool name) and the argument assembly budget, carries raw argument text
//! without shape validation, and never carries host identity: admission binds
//! [`CallId`], [`RunId`], and [`TurnId`] explicitly through [`ToolCall::new`].

use nexus_core::content::MAX_TOOL_NAME_LEN;
use nexus_core::{
    AgentError, CallCandidate, CallId, ContentBlock, ErrorCategory, Limits, M0_REVISION,
    MAX_ITEM_KEY_LEN, MAX_PROVIDER_REF_LEN, NormalizedArgs, RetryGuidance, RunId, ToolCall, ToolId,
    TurnId,
};

fn candidate(
    item_key: &str,
    provider_ref: &str,
    tool_name: &str,
    arguments_json: &str,
) -> CallCandidate {
    CallCandidate::new(item_key, provider_ref, tool_name, arguments_json)
        .expect("valid candidate builds")
}

fn rejected(
    item_key: &str,
    provider_ref: &str,
    tool_name: &str,
    arguments_json: &str,
) -> AgentError {
    CallCandidate::new(item_key, provider_ref, tool_name, arguments_json)
        .expect_err("candidate must be rejected")
}

fn assert_invalid_input(error: &AgentError, needle: &str) {
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert!(
        error.message().contains(needle),
        "message {:?} should contain {needle:?}",
        error.message()
    );
}

#[test]
fn constants_pin_documented_m0_test_bounds() {
    assert_eq!(MAX_ITEM_KEY_LEN, 128);
    assert_eq!(MAX_PROVIDER_REF_LEN, 256);
    assert_eq!(MAX_TOOL_NAME_LEN, 64);
    assert_eq!(Limits::M0_TEST_ARG_ASSEMBLY_BYTES, 65_536);
    // Candidate tool names follow the identifier charset and length exactly.
    assert_eq!(MAX_TOOL_NAME_LEN, nexus_core::ids::MAX_ID_LEN);
    const {
        assert!(Limits::M0_TEST_ARG_ASSEMBLY_BYTES < Limits::M0_TEST_TOOL_OUTPUT_BYTES);
    }
}

#[test]
fn item_key_rejects_empty_and_oversize_by_byte_length() {
    let error = rejected("", "prov-ref-0", "host_read", "{}");
    assert_invalid_input(&error, "identity is invalid");

    let at_bound = "k".repeat(MAX_ITEM_KEY_LEN);
    let built = candidate(&at_bound, "prov-ref-0", "host_read", "{}");
    assert_eq!(built.item_key(), at_bound);

    let oversize = "k".repeat(MAX_ITEM_KEY_LEN + 1);
    let error = rejected(&oversize, "prov-ref-0", "host_read", "{}");
    assert_invalid_input(&error, "identity is invalid");

    // Bounds are bytes, not characters: 64 two-byte characters sit exactly at
    // the 128-byte limit, and one extra byte rejects.
    let multibyte_at_bound = "é".repeat(MAX_ITEM_KEY_LEN / 2);
    assert_eq!(multibyte_at_bound.len(), MAX_ITEM_KEY_LEN);
    let built = candidate(&multibyte_at_bound, "prov-ref-0", "host_read", "{}");
    assert_eq!(built.item_key(), multibyte_at_bound);
    let multibyte_over = multibyte_at_bound + "x";
    assert_eq!(multibyte_over.len(), MAX_ITEM_KEY_LEN + 1);
    let error = rejected(&multibyte_over, "prov-ref-0", "host_read", "{}");
    assert_invalid_input(&error, "identity is invalid");
}

#[test]
fn provider_ref_rejects_empty_and_oversize_by_byte_length() {
    let error = rejected("item-0", "", "host_read", "{}");
    assert_invalid_input(&error, "identity is invalid");

    let at_bound = "r".repeat(MAX_PROVIDER_REF_LEN);
    let built = candidate("item-0", &at_bound, "host_read", "{}");
    assert_eq!(built.provider_ref(), at_bound);

    let oversize = "r".repeat(MAX_PROVIDER_REF_LEN + 1);
    let error = rejected("item-0", &oversize, "host_read", "{}");
    assert_invalid_input(&error, "identity is invalid");

    // The provider reference is byte-bounded and never host-bounded.
    let multibyte_at_bound = "é".repeat(MAX_PROVIDER_REF_LEN / 2);
    assert_eq!(multibyte_at_bound.len(), MAX_PROVIDER_REF_LEN);
    let built = candidate("item-0", &multibyte_at_bound, "host_read", "{}");
    assert_eq!(built.provider_ref(), multibyte_at_bound);
    let multibyte_over = multibyte_at_bound + "x";
    assert_eq!(multibyte_over.len(), MAX_PROVIDER_REF_LEN + 1);
    let error = rejected("item-0", &multibyte_over, "host_read", "{}");
    assert_invalid_input(&error, "identity is invalid");
}

#[test]
fn tool_name_rejects_empty_oversize_and_illegal_charset() {
    let error = rejected("item-0", "prov-ref-0", "", "{}");
    assert_invalid_input(&error, "identity is invalid");

    let at_bound = "AZaz09-_".repeat(8);
    assert_eq!(at_bound.len(), MAX_TOOL_NAME_LEN);
    let built = candidate("item-0", "prov-ref-0", &at_bound, "{}");
    assert_eq!(built.tool_name(), at_bound);

    let oversize = "a".repeat(MAX_TOOL_NAME_LEN + 1);
    let error = rejected("item-0", "prov-ref-0", &oversize, "{}");
    assert_invalid_input(&error, "identity is invalid");

    for name in [
        "host read",
        "host/read",
        "host.read",
        "host:read",
        "host@read",
        "ünïcode",
        "a\nb",
        "a\tb",
        "a\u{0}b",
        "a+b",
        "a=b",
    ] {
        let error = rejected("item-0", "prov-ref-0", name, "{}");
        assert_invalid_input(&error, "identity is invalid");
    }

    // Charset is checked independently of length: a full-length name with a
    // space is still rejected.
    let full_with_space = format!("{} ", "a".repeat(MAX_TOOL_NAME_LEN - 1));
    assert_eq!(full_with_space.len(), MAX_TOOL_NAME_LEN);
    let error = rejected("item-0", "prov-ref-0", &full_with_space, "{}");
    assert_invalid_input(&error, "identity is invalid");

    // Each allowed byte class is accepted on its own.
    for name in ["A", "z", "0", "-", "_"] {
        let built = candidate("item-0", "prov-ref-0", name, "{}");
        assert_eq!(built.tool_name(), name);
    }
}

#[test]
fn provider_fields_are_opaque_and_round_trip_verbatim() {
    // Item keys and provider references are provider-scoped opaque strings:
    // only non-empty and the byte bound apply here, with no charset rule.
    let item_key = "item key/0.ünïcode";
    let provider_ref = r#" {"ref": "0"} "#;
    let arguments_json = r#"  {"path": "a"}  "#;
    let built = candidate(item_key, provider_ref, "host_read", arguments_json);
    assert_eq!(built.item_key(), item_key);
    assert_eq!(built.provider_ref(), provider_ref);
    assert_eq!(built.tool_name(), "host_read");
    assert_eq!(built.arguments_json(), arguments_json);

    // Values that are illegal host identities remain legal provider fields,
    // so no host identity can be derived from a candidate.
    assert!(CallId::new(built.item_key()).is_err());
    assert!(CallId::new(built.provider_ref()).is_err());
    assert!(RunId::new(built.provider_ref()).is_err());
    assert!(TurnId::new(built.provider_ref()).is_err());
}

#[test]
fn arguments_are_bounded_by_the_assembly_budget_exactly() {
    let at_bound = "x".repeat(Limits::M0_TEST_ARG_ASSEMBLY_BYTES);
    let built = candidate("item-0", "prov-ref-0", "host_read", &at_bound);
    assert_eq!(
        built.arguments_json().len(),
        Limits::M0_TEST_ARG_ASSEMBLY_BYTES
    );
    assert_eq!(
        built.arguments_json(),
        at_bound,
        "accepted arguments are never truncated"
    );

    let oversize = "x".repeat(Limits::M0_TEST_ARG_ASSEMBLY_BYTES + 1);
    let error = rejected("item-0", "prov-ref-0", "host_read", &oversize);
    assert_invalid_input(&error, "assembly budget");

    // The budget is bytes, not characters: 32_768 two-byte characters sit
    // exactly at 65_536 bytes and one extra byte rejects.
    let multibyte_at_bound = "é".repeat(Limits::M0_TEST_ARG_ASSEMBLY_BYTES / 2);
    assert_eq!(multibyte_at_bound.len(), Limits::M0_TEST_ARG_ASSEMBLY_BYTES);
    let built = candidate("item-0", "prov-ref-0", "host_read", &multibyte_at_bound);
    assert_eq!(built.arguments_json(), multibyte_at_bound);
    let multibyte_over = multibyte_at_bound + "x";
    assert_eq!(multibyte_over.len(), Limits::M0_TEST_ARG_ASSEMBLY_BYTES + 1);
    let error = rejected("item-0", "prov-ref-0", "host_read", &multibyte_over);
    assert_invalid_input(&error, "assembly budget");

    // The candidate budget is the assembly budget, not the larger tool-output
    // budget: a payload that fits tool output still rejects here.
    const {
        assert!(Limits::M0_TEST_ARG_ASSEMBLY_BYTES < Limits::M0_TEST_TOOL_OUTPUT_BYTES);
    }
    let output_sized = "x".repeat(Limits::M0_TEST_TOOL_OUTPUT_BYTES);
    let error = rejected("item-0", "prov-ref-0", "host_read", &output_sized);
    assert_invalid_input(&error, "assembly budget");
}

#[test]
fn owned_string_inputs_are_accepted_and_preserved() {
    // The constructor takes `impl Into<String>`; owned inputs are preserved
    // without re-encoding.
    let built = CallCandidate::new(
        String::from("item-0"),
        String::from("prov-ref-0"),
        String::from("host_read"),
        String::from(r#"{"a":1}"#),
    )
    .expect("owned inputs build");
    assert_eq!(built.item_key(), "item-0");
    assert_eq!(built.provider_ref(), "prov-ref-0");
    assert_eq!(built.tool_name(), "host_read");
    assert_eq!(built.arguments_json(), r#"{"a":1}"#);
}

#[test]
fn candidate_carries_raw_arguments_while_admission_validates_shape() {
    // The candidate enforces only the assembly bound; it does not parse or
    // shape-check argument text. Admission (NormalizedArgs) rejects what is
    // not object-root text, so a proposal is never mistaken for validated
    // arguments.
    for raw in ["", "not json", "{unclosed", "[1,2,3]"] {
        let built = candidate("item-0", "prov-ref-0", "host_read", raw);
        assert_eq!(built.arguments_json(), raw);
        assert!(
            NormalizedArgs::new(built.arguments_json()).is_err(),
            "admission rejects raw argument text {raw:?}"
        );
    }

    let object_root = candidate("item-0", "prov-ref-0", "host_read", r#"{"path":"a"}"#);
    let admitted = NormalizedArgs::new(object_root.arguments_json()).expect("object root admits");
    assert_eq!(admitted.as_str(), r#"{"path":"a"}"#);
}

#[test]
fn identity_fields_compare_by_value_and_clone_independently() {
    let original = candidate("item-0", "prov-ref-0", "host_read", r#"{"a":1}"#);
    let same = candidate("item-0", "prov-ref-0", "host_read", r#"{"a":1}"#);
    assert_eq!(original, same);
    assert_eq!(original, original.clone());
    assert_eq!(original.item_key(), original.clone().item_key());
    assert_eq!(original.provider_ref(), original.clone().provider_ref());
    assert_eq!(original.tool_name(), original.clone().tool_name());
    assert_eq!(original.arguments_json(), original.clone().arguments_json());

    // Any single differing field breaks value equality.
    for other in [
        candidate("item-1", "prov-ref-0", "host_read", r#"{"a":1}"#),
        candidate("item-0", "prov-ref-1", "host_read", r#"{"a":1}"#),
        candidate("item-0", "prov-ref-0", "host_write", r#"{"a":1}"#),
        candidate("item-0", "prov-ref-0", "host_read", r#"{"a":2}"#),
    ] {
        assert_ne!(original, other);
    }
}

#[test]
fn call_proposal_block_round_trips_the_candidate() {
    let proposal = candidate("item-0", "prov-ref-0", "host_read", r#"{"a":1}"#);
    let block = ContentBlock::CallProposal(proposal.clone());
    match block {
        ContentBlock::CallProposal(carried) => assert_eq!(carried, proposal),
        other => panic!("expected a proposal block, got {other:?}"),
    }

    // A proposal block is not an admitted call block even with equal text.
    let admitted = ToolCall::new(
        RunId::new("run-1").expect("valid"),
        TurnId::new("turn-1").expect("valid"),
        CallId::new("call-1").expect("valid"),
        ToolId::new("host_read", M0_REVISION).expect("valid"),
        NormalizedArgs::new(r#"{"a":1}"#).expect("valid"),
    );
    assert_ne!(
        ContentBlock::CallProposal(proposal),
        ContentBlock::Call(admitted)
    );
}

#[test]
fn candidate_has_no_host_identity_and_admission_binds_it_explicitly() {
    let proposal = candidate("item key 0", "prov ref/0", "host_read", r#"{"a":1}"#);

    // Provider-scoped fields can be illegal as host identities, so no host id
    // can be derived from the candidate.
    assert!(CallId::new(proposal.item_key()).is_err());
    assert!(CallId::new(proposal.provider_ref()).is_err());
    assert!(RunId::new(proposal.item_key()).is_err());
    assert!(RunId::new(proposal.provider_ref()).is_err());
    assert!(TurnId::new(proposal.provider_ref()).is_err());

    let run = RunId::new("run-1").expect("valid");
    let turn = TurnId::new("turn-1").expect("valid");
    let tool = ToolId::new(proposal.tool_name(), M0_REVISION).expect("valid");
    let args = NormalizedArgs::new(proposal.arguments_json()).expect("valid");

    let first = ToolCall::new(
        run.clone(),
        turn.clone(),
        CallId::new("call-a").expect("valid"),
        tool.clone(),
        args.clone(),
    );
    let second = ToolCall::new(
        run.clone(),
        turn.clone(),
        CallId::new("call-b").expect("valid"),
        tool.clone(),
        args,
    );

    // Identity comes from the explicit admission parameters only: the same
    // proposal admits to distinct calls and stays untouched.
    assert_eq!(first.call().as_str(), "call-a");
    assert_eq!(second.call().as_str(), "call-b");
    assert_ne!(first, second);
    assert_eq!(first.run(), &run);
    assert_eq!(first.turn(), &turn);
    assert_eq!(first.tool(), &tool);
    assert_eq!(proposal.item_key(), "item key 0");
    assert_eq!(proposal.provider_ref(), "prov ref/0");
}

#[test]
fn identity_failure_is_reported_before_oversize_arguments() {
    // The constructor checks provider identity first, so an oversize payload
    // cannot mask an invalid identity.
    let oversize = "x".repeat(Limits::M0_TEST_ARG_ASSEMBLY_BYTES + 1);
    let error = rejected("", "prov-ref-0", "host_read", &oversize);
    assert_invalid_input(&error, "identity is invalid");
}
