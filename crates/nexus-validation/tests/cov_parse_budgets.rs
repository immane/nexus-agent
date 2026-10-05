//! Public-boundary hardening for the `parse_object` parse budgets.
//!
//! `parse_object` is the policy-inspection entry point of `nexus-validation`.
//! These tests pin the budget behavior at its exact boundaries using only the
//! public API:
//!
//! - the caller's byte budget is exact and counts UTF-8 bytes;
//! - the depth budget accepts depth `MAX_ARGS_DEPTH` and rejects one deeper;
//! - the node budget counts values (not object keys), accepts
//!   `MAX_ARGS_NODES` values, and rejects one more;
//! - out-of-range configuration (`0` or above the hard maximum) is
//!   `InvalidInput`, while in-range exhaustion is `ResourceLimit`.
//!
//! No clocks, randomness, I/O, or environment are involved, so every check is
//! deterministic.

#![forbid(unsafe_code)]

use nexus_core::{AgentError, ErrorCategory, Limits, RetryGuidance};
use nexus_validation::{MAX_ARGS_DEPTH, MAX_ARGS_NODES, parse_object};

/// The hard maximum argument byte budget: the core M0 assembly budget.
const MAX_ARG_BYTES: usize = Limits::M0_TEST_ARG_ASSEMBLY_BYTES;

fn assert_resource_limit(error: &AgentError, needle: &str) {
    assert_eq!(
        error.category(),
        ErrorCategory::ResourceLimit,
        "message: {error}"
    );
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert!(
        error.message().contains(needle),
        "message {:?} should contain {needle:?}",
        error.message()
    );
}

fn assert_invalid_input(error: &AgentError, needle: &str) {
    assert_eq!(
        error.category(),
        ErrorCategory::InvalidInput,
        "message: {error}"
    );
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert!(
        error.message().contains(needle),
        "message {:?} should contain {needle:?}",
        error.message()
    );
}

#[test]
fn byte_budget_boundary_is_exact_and_counts_utf8_bytes() {
    let args = r#"{"a":1}"#;
    parse_object(args, args.len()).expect("text at the exact byte budget parses");
    let error = parse_object(args, args.len() - 1).unwrap_err();
    assert_resource_limit(&error, "byte");

    // A budget of 1 is valid configuration but cannot fit the two-byte "{}".
    let error = parse_object("{}", 1).unwrap_err();
    assert_resource_limit(&error, "byte");
    parse_object("{}", 2).expect("the smallest fitting budget parses");

    // The budget counts bytes, not characters: this text is 9 characters but
    // 10 bytes, so a 9-byte budget must be exhausted.
    let multibyte = r#"{"a":"é"}"#;
    assert_eq!(multibyte.chars().count(), 9);
    assert_eq!(multibyte.len(), 10);
    parse_object(multibyte, multibyte.len()).expect("byte-exact budget parses");
    let error = parse_object(multibyte, multibyte.len() - 1).unwrap_err();
    assert_resource_limit(&error, "byte");
}

#[test]
fn byte_budget_at_the_hard_maximum_is_exact() {
    let base = r#"{"a":1}"#;
    let at_max = format!("{base}{}", " ".repeat(MAX_ARG_BYTES - base.len()));
    assert_eq!(at_max.len(), MAX_ARG_BYTES);
    parse_object(&at_max, MAX_ARG_BYTES).expect("text at the hard maximum parses");
    let error = parse_object(&at_max, MAX_ARG_BYTES - 1).unwrap_err();
    assert_resource_limit(&error, "byte");

    // One byte over the hard maximum is in-range exhaustion, not invalid
    // configuration: the budget itself is still MAX_ARG_BYTES.
    let oversize = format!("{at_max} ");
    assert_eq!(oversize.len(), MAX_ARG_BYTES + 1);
    let error = parse_object(&oversize, MAX_ARG_BYTES).unwrap_err();
    assert_resource_limit(&error, "byte");
}

#[test]
fn byte_budget_over_the_hard_maximum_is_invalid_input() {
    for budget in [MAX_ARG_BYTES + 1, usize::MAX] {
        let error = parse_object("{}", budget).unwrap_err();
        assert_invalid_input(&error, "hard maximum");
    }

    // The range check runs before the byte-length check, so an over-cap
    // budget stays invalid configuration even for oversized text.
    let oversize = "x".repeat(MAX_ARG_BYTES + 1);
    let error = parse_object(&oversize, MAX_ARG_BYTES + 1).unwrap_err();
    assert_invalid_input(&error, "hard maximum");
}

#[test]
fn zero_byte_budget_is_invalid_input() {
    let error = parse_object("{}", 0).unwrap_err();
    assert_invalid_input(&error, "nonzero");
    // Zero is rejected before parsing, even for empty text.
    let error = parse_object("", 0).unwrap_err();
    assert_invalid_input(&error, "nonzero");
}

#[test]
fn byte_exhaustion_is_checked_before_parsing() {
    // An in-range budget smaller than the text wins over a syntax error:
    // exhaustion is a ResourceLimit, not InvalidInput.
    let error = parse_object("{not json", 1).unwrap_err();
    assert_resource_limit(&error, "byte");
}

#[test]
fn depth_budget_boundary_through_objects() {
    let nested = |levels: usize| {
        let mut text = String::from("{}");
        for _ in 1..levels {
            text = format!(r#"{{"a":{text}}}"#);
        }
        text
    };
    // The root object sits at depth 0, so `levels` objects reach depth
    // `levels - 1`; MAX_ARGS_DEPTH + 1 objects exactly fill the budget.
    let at_budget = nested(MAX_ARGS_DEPTH + 1);
    let value = parse_object(&at_budget, MAX_ARG_BYTES).expect("depth at the budget parses");
    assert!(value.is_object());
    let error = parse_object(&nested(MAX_ARGS_DEPTH + 2), MAX_ARG_BYTES).unwrap_err();
    assert_resource_limit(&error, "depth");
}

#[test]
fn depth_budget_boundary_through_arrays() {
    let nested = |levels: usize| format!(r#"{{"a":{}{}}}"#, "[".repeat(levels), "]".repeat(levels));
    // The root object sits at depth 0 and the "a" array at depth 1, so
    // MAX_ARGS_DEPTH nested arrays exactly fill the budget.
    parse_object(&nested(MAX_ARGS_DEPTH), MAX_ARG_BYTES).expect("depth at the budget parses");
    let error = parse_object(&nested(MAX_ARGS_DEPTH + 1), MAX_ARG_BYTES).unwrap_err();
    assert_resource_limit(&error, "depth");
}

#[test]
fn node_budget_boundary_with_array_elements() {
    let build = |elements: usize| format!(r#"{{"a":[{}]}}"#, vec!["0"; elements].join(","));
    // The root object, the array, and every element share the node budget.
    let below = build(MAX_ARGS_NODES - 3);
    let value = parse_object(&below, MAX_ARG_BYTES).expect("nodes below the budget parse");
    assert_eq!(
        value["a"].as_array().map(Vec::len),
        Some(MAX_ARGS_NODES - 3)
    );

    let at_budget = build(MAX_ARGS_NODES - 2);
    let value = parse_object(&at_budget, MAX_ARG_BYTES).expect("nodes at the budget parse");
    assert_eq!(
        value["a"].as_array().map(Vec::len),
        Some(MAX_ARGS_NODES - 2)
    );

    let error = parse_object(&build(MAX_ARGS_NODES - 1), MAX_ARG_BYTES).unwrap_err();
    assert_resource_limit(&error, "node");
}

#[test]
fn node_budget_counts_values_not_object_keys() {
    let build = |keys: usize| {
        let entries: Vec<String> = (0..keys).map(|index| format!(r#""k{index}":0"#)).collect();
        format!("{{{}}}", entries.join(","))
    };
    // Only the root object and one scalar per key count as nodes, so
    // MAX_ARGS_NODES - 1 keys exactly fill the budget and one more key
    // exhausts it. If keys were counted, the accepted case would hold
    // 2 * MAX_ARGS_NODES - 1 nodes and fail.
    let at_budget = build(MAX_ARGS_NODES - 1);
    let value = parse_object(&at_budget, MAX_ARG_BYTES).expect("nodes at the budget parse");
    assert_eq!(
        value.as_object().map(|object| object.len()),
        Some(MAX_ARGS_NODES - 1)
    );
    let error = parse_object(&build(MAX_ARGS_NODES), MAX_ARG_BYTES).unwrap_err();
    assert_resource_limit(&error, "node");
}

#[test]
fn in_budget_syntax_and_shape_failures_stay_invalid_input() {
    // Malformed text exactly at the hard maximum is a syntax failure, not a
    // byte-budget exhaustion.
    let malformed = format!("{{{}", " ".repeat(MAX_ARG_BYTES - 1));
    assert_eq!(malformed.len(), MAX_ARG_BYTES);
    let error = parse_object(&malformed, MAX_ARG_BYTES).unwrap_err();
    assert_invalid_input(&error, "valid JSON");

    // A non-object root within budget is likewise InvalidInput.
    let error = parse_object("[]", 2).unwrap_err();
    assert_invalid_input(&error, "object");
}
