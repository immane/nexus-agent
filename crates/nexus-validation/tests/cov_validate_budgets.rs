//! Public-boundary hardening for `CompiledSchema::validate` byte, depth, and
//! node budgets.
//!
//! Every assertion goes through the `nexus-validation` public API plus the
//! `nexus-core` category/retry vocabulary; no private items, tools, or I/O are
//! involved. The tests pin:
//!
//! - the hard argument byte maximum as exactly 65,536 bytes, with text at the
//!   exact boundary accepted and one byte beyond it exhausted;
//! - `max_bytes == 0` and `max_bytes > Limits::M0_TEST_ARG_ASSEMBLY_BYTES` as
//!   invalid configuration (`InvalidInput` with exact diagnostics), rejected
//!   before parsing and never clamped to the hard maximum;
//! - in-range byte exhaustion, UTF-8 byte counting, and canonical-serialization
//!   growth as `ResourceLimit` at the exact byte;
//! - depth and node budgets as inclusive at the exact boundary and
//!   `ResourceLimit` one value beyond, for arrays, objects, and mixed nesting;
//! - every failure being non-retryable.
//!
//! All inputs are built deterministically from public constants; no clocks,
//! randomness, or I/O are involved.

#![forbid(unsafe_code)]

use nexus_core::{AgentError, ErrorCategory, Limits, RetryGuidance};
use nexus_validation::{CompiledSchema, MAX_ARGS_DEPTH, MAX_ARGS_NODES, parse_object};

/// The core M0 argument-assembly budget, which the validator documents as its
/// hard `max_bytes` maximum.
const MAX_ARG_BYTES: usize = Limits::M0_TEST_ARG_ASSEMBLY_BYTES;

fn compile(schema: &str) -> CompiledSchema {
    CompiledSchema::compile(schema).expect("schema compiles")
}

/// Asserts category, exact diagnostic, and non-retryable guidance together,
/// so no failure path can regress to a retryable hint unnoticed.
fn assert_invalid(error: &AgentError, message: &str) {
    assert_eq!(error.category(), ErrorCategory::InvalidInput, "{error}");
    assert_eq!(error.message(), message, "{error}");
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry, "{error}");
}

fn assert_limit(error: &AgentError, message: &str) {
    assert_eq!(error.category(), ErrorCategory::ResourceLimit, "{error}");
    assert_eq!(error.message(), message, "{error}");
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry, "{error}");
}

/// Nested arrays under the root object's `a` key; the innermost array sits at
/// JSON depth `depth` (the root object is depth 0).
fn arrays_at_depth(depth: usize) -> String {
    format!(r#"{{"a":{}{}}}"#, "[".repeat(depth), "]".repeat(depth))
}

/// Nested objects under the root object's `a` key with a terminal scalar; the
/// scalar sits at JSON depth `depth`. Requires `depth >= 1`.
fn objects_at_depth(depth: usize) -> String {
    let opens = r#"{"a":"#.repeat(depth - 1);
    let closes = "}".repeat(depth - 1);
    format!(r#"{{"a":{opens}0{closes}}}"#)
}

/// Alternating array/object nesting under the root object's `a` key with a
/// terminal scalar; the scalar sits at JSON depth `depth`. Requires
/// `depth >= 1`.
fn mixed_at_depth(depth: usize) -> String {
    let mut opens = String::new();
    let mut closes = String::new();
    for level in 0..depth - 1 {
        if level % 2 == 0 {
            opens.push('[');
            closes.insert(0, ']');
        } else {
            opens.push_str(r#"{"a":"#);
            closes.insert(0, '}');
        }
    }
    format!(r#"{{"a":{opens}null{closes}}}"#)
}

/// Root object plus an array of `elements` scalar zeros: `elements + 2` JSON
/// values in total.
fn array_args(elements: usize) -> String {
    format!(r#"{{"a":[{}]}}"#, vec!["0"; elements].join(","))
}

/// Root object with `count` null members: `count + 1` JSON values in total
/// (member keys are not values).
fn object_args(count: usize) -> String {
    let entries: Vec<String> = (0..count)
        .map(|index| format!(r#""k{index}":null"#))
        .collect();
    format!(r#"{{{}}}"#, entries.join(","))
}

#[test]
fn hard_maximum_is_exactly_65536_bytes() {
    assert_eq!(MAX_ARG_BYTES, 65_536);
}

#[test]
fn zero_budget_is_invalid_configuration_before_parsing() {
    let open = compile(r#"{"type":"object"}"#);
    let closed = compile(
        r#"{"type":"object","properties":{"v":{"type":"null"}},"additionalProperties":false}"#,
    );
    // The budget is checked before any text is parsed or validated, so
    // malformed, non-object, and schema-violating text all take the same path.
    for args in ["{}", "", "{", "not json", "[]", r#"{"v":1}"#] {
        let error = open.validate(args, 0).unwrap_err();
        assert_invalid(&error, "argument byte budget must be nonzero");
        let error = parse_object(args, 0).unwrap_err();
        assert_invalid(&error, "argument byte budget must be nonzero");
    }
    let error = closed.validate(r#"{"v":1}"#, 0).unwrap_err();
    assert_invalid(&error, "argument byte budget must be nonzero");
}

#[test]
fn over_hard_maximum_budget_is_invalid_configuration_never_clamped() {
    let schema = compile(r#"{"type":"object"}"#);
    for budget in [
        MAX_ARG_BYTES + 1,
        MAX_ARG_BYTES + 2,
        MAX_ARG_BYTES * 2,
        usize::MAX / 2,
        usize::MAX,
    ] {
        let error = schema.validate("{}", budget).unwrap_err();
        assert_invalid(&error, "argument byte budget exceeds the hard maximum");
        let error = parse_object("{}", budget).unwrap_err();
        assert_invalid(&error, "argument byte budget exceeds the hard maximum");
    }

    // Configuration is checked before argument length: oversized text under an
    // oversized budget is still invalid configuration, not a resource limit,
    // and the budget is never narrowed to the hard maximum.
    let oversized = format!(r#"{{"pad":"{}"}}"#, "x".repeat(MAX_ARG_BYTES));
    let error = schema.validate(&oversized, MAX_ARG_BYTES + 1).unwrap_err();
    assert_invalid(&error, "argument byte budget exceeds the hard maximum");
}

#[test]
fn exact_hard_maximum_budget_accepts_text_at_the_boundary() {
    let schema = compile(r#"{"type":"object"}"#);

    // Whitespace padding keeps the raw text at exactly the hard maximum while
    // the canonical form stays minimal.
    let canonical = r#"{"path":"src"}"#;
    let padded = format!("{canonical}{}", " ".repeat(MAX_ARG_BYTES - canonical.len()));
    assert_eq!(padded.len(), MAX_ARG_BYTES);
    let normalized = schema
        .validate(&padded, MAX_ARG_BYTES)
        .expect("argument text exactly at the hard maximum is accepted");
    assert_eq!(normalized.as_str(), canonical);
    parse_object(&padded, MAX_ARG_BYTES).expect("parse accepts the exact boundary");

    // A value whose canonical serialization is itself exactly the hard
    // maximum: `{"a":"` + 65,528 bytes + `"}`.
    let tight = format!(r#"{{"a":"{}"}}"#, "x".repeat(MAX_ARG_BYTES - 8));
    assert_eq!(tight.len(), MAX_ARG_BYTES);
    let normalized = schema
        .validate(&tight, MAX_ARG_BYTES)
        .expect("canonical form exactly at the hard maximum is accepted");
    assert_eq!(normalized.as_str().len(), MAX_ARG_BYTES);
    parse_object(&tight, MAX_ARG_BYTES).expect("parse accepts the exact canonical boundary");

    // One raw byte beyond the boundary exhausts the budget.
    let over = format!("{padded} ");
    assert_eq!(over.len(), MAX_ARG_BYTES + 1);
    let error = schema.validate(&over, MAX_ARG_BYTES).unwrap_err();
    assert_limit(&error, "argument byte budget exhausted");
    let error = parse_object(&over, MAX_ARG_BYTES).unwrap_err();
    assert_limit(&error, "argument byte budget exhausted");

    // The same text with one byte less budget also exhausts it.
    let error = schema.validate(&padded, MAX_ARG_BYTES - 1).unwrap_err();
    assert_limit(&error, "argument byte budget exhausted");
    let error = schema.validate(&tight, MAX_ARG_BYTES - 1).unwrap_err();
    assert_limit(&error, "argument byte budget exhausted");
}

#[test]
fn in_range_byte_exhaustion_is_exact() {
    let schema = compile(r#"{"type":"object"}"#);
    let args = r#"{"a":1}"#;
    assert_eq!(args.len(), 7);

    for budget in 1..args.len() {
        let error = schema.validate(args, budget).unwrap_err();
        assert_limit(&error, "argument byte budget exhausted");
        let error = parse_object(args, budget).unwrap_err();
        assert_limit(&error, "argument byte budget exhausted");
    }
    for budget in [args.len(), args.len() + 1, MAX_ARG_BYTES] {
        schema
            .validate(args, budget)
            .expect("budget at or above the text length admits it");
        parse_object(args, budget).expect("budget at or above the text length admits it");
    }

    // Budget 1 is valid configuration but cannot fit even the minimal object
    // root, so it exhausts as a resource limit rather than misreporting.
    let error = schema.validate("{}", 1).unwrap_err();
    assert_limit(&error, "argument byte budget exhausted");
    let error = parse_object("{}", 1).unwrap_err();
    assert_limit(&error, "argument byte budget exhausted");
}

#[test]
fn byte_budget_counts_utf8_bytes_not_characters() {
    let schema = compile(r#"{"type":"object"}"#);
    let args = r#"{"a":"é"}"#;
    assert_eq!(args.chars().count(), 9);
    assert_eq!(args.len(), 10);

    schema
        .validate(args, 10)
        .expect("the byte budget admits the two-byte code point");
    let error = schema.validate(args, 9).unwrap_err();
    assert_limit(&error, "argument byte budget exhausted");
    let error = parse_object(args, 9).unwrap_err();
    assert_limit(&error, "argument byte budget exhausted");
}

#[test]
fn canonical_growth_counts_against_the_byte_budget() {
    let schema = compile(r#"{"type":"object"}"#);
    let cases = [
        (r#"{"n":1E2}"#, r#"{"n":100.0}"#),
        (r#"{"n":1e10}"#, r#"{"n":10000000000.0}"#),
        (r#"{"b":1E2,"a":0}"#, r#"{"a":0,"b":100.0}"#),
    ];
    for (args, canonical) in cases {
        assert!(
            args.len() < canonical.len(),
            "case must grow: {args} -> {canonical}"
        );

        // The raw text fits one byte below the canonical length, so only the
        // post-canonicalization byte check can reject it.
        let error = schema.validate(args, canonical.len() - 1).unwrap_err();
        assert_limit(&error, "argument byte budget exhausted");

        let normalized = schema
            .validate(args, canonical.len())
            .expect("canonical form exactly at the budget is accepted");
        assert_eq!(normalized.as_str(), canonical);

        schema
            .validate(args, canonical.len() + 1)
            .expect("headroom above the canonical length is accepted");
    }
}

#[test]
fn depth_boundary_is_inclusive_for_arrays_objects_and_mixed_nesting() {
    let schema = compile(r#"{"type":"object"}"#);
    let builders = [
        ("arrays", arrays_at_depth as fn(usize) -> String),
        ("objects", objects_at_depth as fn(usize) -> String),
        ("mixed", mixed_at_depth as fn(usize) -> String),
    ];
    for (label, build) in builders {
        let at_budget = build(MAX_ARGS_DEPTH);
        schema
            .validate(&at_budget, MAX_ARG_BYTES)
            .unwrap_or_else(|error| panic!("{label} depth at budget: {error}"));
        parse_object(&at_budget, MAX_ARG_BYTES)
            .unwrap_or_else(|error| panic!("{label} parse depth at budget: {error}"));

        let over_budget = build(MAX_ARGS_DEPTH + 1);
        let error = schema.validate(&over_budget, MAX_ARG_BYTES).unwrap_err();
        assert_limit(&error, "argument depth budget exhausted");
        let error = parse_object(&over_budget, MAX_ARG_BYTES).unwrap_err();
        assert_limit(&error, "argument depth budget exhausted");
    }
}

#[test]
fn node_boundary_is_inclusive_and_counts_arrays_objects_and_members() {
    let schema = compile(r#"{"type":"object"}"#);

    // Root object + array + 4,094 scalars = 4,096 values, exactly the budget.
    let arrays_at_budget = array_args(MAX_ARGS_NODES - 2);
    schema
        .validate(&arrays_at_budget, MAX_ARG_BYTES)
        .expect("array nodes exactly at the budget pass");
    parse_object(&arrays_at_budget, MAX_ARG_BYTES)
        .expect("array nodes exactly at the budget parse");

    let arrays_over_budget = array_args(MAX_ARGS_NODES - 1);
    let error = schema
        .validate(&arrays_over_budget, MAX_ARG_BYTES)
        .unwrap_err();
    assert_limit(&error, "argument node budget exhausted");
    let error = parse_object(&arrays_over_budget, MAX_ARG_BYTES).unwrap_err();
    assert_limit(&error, "argument node budget exhausted");

    // Root object + 4,095 null members = 4,096 values; keys are not values.
    let objects_at_budget = object_args(MAX_ARGS_NODES - 1);
    schema
        .validate(&objects_at_budget, MAX_ARG_BYTES)
        .expect("object nodes exactly at the budget pass");
    parse_object(&objects_at_budget, MAX_ARG_BYTES)
        .expect("object nodes exactly at the budget parse");

    let objects_over_budget = object_args(MAX_ARGS_NODES);
    let error = schema
        .validate(&objects_over_budget, MAX_ARG_BYTES)
        .unwrap_err();
    assert_limit(&error, "argument node budget exhausted");
    let error = parse_object(&objects_over_budget, MAX_ARG_BYTES).unwrap_err();
    assert_limit(&error, "argument node budget exhausted");
}

#[test]
fn every_validate_and_parse_failure_is_non_retryable() {
    let open = compile(r#"{"type":"object"}"#);
    let closed = compile(
        r#"{"type":"object","properties":{"v":{"type":"string","minLength":2}},"required":["v"],"additionalProperties":false}"#,
    );

    let failures: Vec<AgentError> = vec![
        // Invalid budget configuration.
        open.validate("{}", 0).unwrap_err(),
        open.validate("{}", MAX_ARG_BYTES + 1).unwrap_err(),
        parse_object("{}", 0).unwrap_err(),
        parse_object("{}", usize::MAX).unwrap_err(),
        // In-range byte exhaustion and canonical growth.
        open.validate(r#"{"a":1}"#, 6).unwrap_err(),
        open.validate(r#"{"n":1E2}"#, 10).unwrap_err(),
        // Depth and node exhaustion.
        open.validate(&arrays_at_depth(MAX_ARGS_DEPTH + 1), MAX_ARG_BYTES)
            .unwrap_err(),
        open.validate(&array_args(MAX_ARGS_NODES - 1), MAX_ARG_BYTES)
            .unwrap_err(),
        parse_object(&objects_at_depth(MAX_ARGS_DEPTH + 1), MAX_ARG_BYTES).unwrap_err(),
        parse_object(&object_args(MAX_ARGS_NODES), MAX_ARG_BYTES).unwrap_err(),
        // Malformed text, duplicate keys, and non-object roots.
        open.validate("{", MAX_ARG_BYTES).unwrap_err(),
        open.validate(r#"{"a":1,"a":2}"#, MAX_ARG_BYTES)
            .unwrap_err(),
        open.validate("[]", MAX_ARG_BYTES).unwrap_err(),
        parse_object("null", MAX_ARG_BYTES).unwrap_err(),
        // Schema mismatches.
        closed.validate(r#"{"v":1}"#, MAX_ARG_BYTES).unwrap_err(),
        closed.validate(r#"{"v":"x"}"#, MAX_ARG_BYTES).unwrap_err(),
        closed
            .validate(r#"{"v":"xy","extra":1}"#, MAX_ARG_BYTES)
            .unwrap_err(),
        closed.validate("{}", MAX_ARG_BYTES).unwrap_err(),
    ];

    for error in &failures {
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry, "{error}");
        assert!(
            matches!(
                error.category(),
                ErrorCategory::InvalidInput | ErrorCategory::ResourceLimit
            ),
            "unexpected category for {error}"
        );
    }
}
