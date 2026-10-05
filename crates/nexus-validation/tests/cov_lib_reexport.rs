//! Crate-surface hardening for `nexus-validation`.
//!
//! These tests pin the public boundary itself: the `serde_json` re-export is
//! usable without a direct dependency, every `MAX_*` budget constant keeps
//! its exact value, the `CompiledSchema` and `parse_object` entry points stay
//! strict and deterministic, and the hard argument-byte maximum stays aligned
//! with the core M0 assembly budget. No tool, executor, or runtime is
//! involved, and every assertion is deterministic.

#![forbid(unsafe_code)]

use nexus_core::{AgentError, ErrorCategory, Limits, RetryGuidance};
use nexus_validation::serde_json::{self, Value};
use nexus_validation::{
    CompiledSchema, MAX_ARGS_DEPTH, MAX_ARGS_NODES, MAX_KEY_BYTES, MAX_SCHEMA_BYTES,
    MAX_SCHEMA_DEPTH, MAX_SCHEMA_NODES, MAX_SCHEMA_PROPERTIES, parse_object,
};

/// The core M0 tool-argument assembly budget: the validator's hard maximum.
const HARD_ARG_BYTES: usize = Limits::M0_TEST_ARG_ASSEMBLY_BYTES;

fn compile(schema: &str) -> CompiledSchema {
    CompiledSchema::compile(schema).expect("schema compiles")
}

fn assert_invalid(error: &AgentError, needle: &str) {
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert!(
        error.message().contains(needle),
        "message {:?} should contain {needle:?}",
        error.message()
    );
}

#[test]
fn serde_json_reexport_is_usable_without_a_direct_dependency() {
    let text = r#"{"a":[1,2.5,"x",true,null,{}],"b":{"c":false}}"#;
    let parsed: Value = serde_json::from_str(text).expect("re-export parses");
    assert!(parsed.is_object());
    assert_eq!(parsed["a"].as_array().map(Vec::len), Some(6));
    assert_eq!(parsed["a"][1].as_f64(), Some(2.5));
    assert_eq!(parsed["b"]["c"].as_bool(), Some(false));
    assert_eq!(
        serde_json::to_string(&parsed).expect("re-export serializes"),
        text
    );
    // `parse_object` returns the same re-exported value type.
    assert_eq!(parse_object(text, HARD_ARG_BYTES).expect("parses"), parsed);
    // The `json!` macro from the re-export builds the same shape.
    let built = serde_json::json!({"a":[1,2.5,"x",true,null,{}],"b":{"c":false}});
    assert_eq!(built, parsed);
}

#[test]
fn serde_json_reexport_types_interoperate_with_the_entry_points() {
    fn object_len(value: &Value) -> usize {
        value.as_object().expect("object value").len()
    }
    let value = parse_object(r#"{"x":1,"y":2}"#, HARD_ARG_BYTES).expect("parses");
    assert_eq!(object_len(&value), 2);
    let round_trip = serde_json::to_value(serde_json::json!({"x":1,"y":2})).expect("to_value");
    assert_eq!(round_trip, value);
    assert_eq!(
        serde_json::from_value::<Value>(value.clone()).expect("from_value"),
        value
    );
}

#[test]
fn budget_constants_are_pinned() {
    assert_eq!(MAX_SCHEMA_BYTES, 65_536);
    assert_eq!(MAX_SCHEMA_DEPTH, 16);
    assert_eq!(MAX_SCHEMA_NODES, 2_048);
    assert_eq!(MAX_SCHEMA_PROPERTIES, 256);
    assert_eq!(MAX_KEY_BYTES, 128);
    assert_eq!(MAX_ARGS_DEPTH, 32);
    assert_eq!(MAX_ARGS_NODES, 4_096);
}

#[test]
fn schema_byte_budget_matches_the_core_tool_spec_bound() {
    assert_eq!(MAX_SCHEMA_BYTES, nexus_core::tool::MAX_SCHEMA_BYTES);
    let base = r#"{"type":"object"}"#;
    let at_bound = format!("{base}{}", " ".repeat(MAX_SCHEMA_BYTES - base.len()));
    assert_eq!(at_bound.len(), MAX_SCHEMA_BYTES);
    CompiledSchema::compile(&at_bound).expect("schema at the bound compiles");
    let error = CompiledSchema::compile(&format!("{at_bound} ")).unwrap_err();
    assert_eq!(error.category(), ErrorCategory::ResourceLimit);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
}

#[test]
fn hard_argument_maximum_matches_the_core_m0_assembly_budget() {
    assert_eq!(HARD_ARG_BYTES, 65_536);
    let schema = compile(r#"{"type":"object"}"#);
    // A one-byte budget is valid configuration but too small for any object
    // text, so the failure must be exhaustion, not invalid configuration.
    let error = schema.validate("{}", 1).unwrap_err();
    assert_eq!(error.category(), ErrorCategory::ResourceLimit);
    let error = parse_object("{}", 1).unwrap_err();
    assert_eq!(error.category(), ErrorCategory::ResourceLimit);
    // The smallest object fits the smallest useful budget exactly.
    schema.validate("{}", "{}".len()).expect("budget accepted");
    parse_object("{}", "{}".len()).expect("budget accepted");
    // The hard maximum itself is accepted configuration.
    schema
        .validate("{}", HARD_ARG_BYTES)
        .expect("budget accepted");
    parse_object("{}", HARD_ARG_BYTES).expect("budget accepted");
    for budget in [0, HARD_ARG_BYTES + 1, usize::MAX] {
        for error in [
            schema.validate("{}", budget).unwrap_err(),
            parse_object("{}", budget).unwrap_err(),
        ] {
            assert_eq!(error.category(), ErrorCategory::InvalidInput, "{budget}");
            assert_eq!(error.retry(), RetryGuidance::DoNotRetry, "{budget}");
        }
    }
    assert_invalid(&schema.validate("{}", 0).unwrap_err(), "nonzero");
    assert_invalid(
        &parse_object("{}", HARD_ARG_BYTES + 1).unwrap_err(),
        "hard maximum",
    );
}

#[test]
fn compiled_schema_entry_point_is_deterministic_and_reusable() {
    let text = r#"{"type":"object","properties":{"v":{"type":"string"}},"required":["v"],"additionalProperties":false}"#;
    let first = compile(text);
    let second = compile(text);
    assert_eq!(first, second);
    assert_eq!(first.clone(), first);
    assert_eq!(format!("{first:?}"), format!("{second:?}"));
    let args = r#"{"v":"x"}"#;
    assert_eq!(
        first.validate(args, HARD_ARG_BYTES).expect("valid"),
        second.validate(args, HARD_ARG_BYTES).expect("valid")
    );
    let left = first.validate(r#"{"v":1}"#, HARD_ARG_BYTES).unwrap_err();
    let right = second.validate(r#"{"v":1}"#, HARD_ARG_BYTES).unwrap_err();
    assert_eq!(left.category(), ErrorCategory::InvalidInput);
    assert_eq!(left.message(), right.message());
}

#[test]
fn parse_object_entry_point_is_strict_about_objects_and_duplicates() {
    for args in ["[]", "\"x\"", "1", "true", "null", ""] {
        let error = parse_object(args, HARD_ARG_BYTES).unwrap_err();
        assert_eq!(error.category(), ErrorCategory::InvalidInput, "{args:?}");
    }
    assert_invalid(
        &parse_object(r#"{"a":1,"a":2}"#, HARD_ARG_BYTES).unwrap_err(),
        "duplicate",
    );
    // This entry point returns the plain parsed value, not the canonical
    // form: the re-export's own parser yields an equal value for
    // duplicate-free object text.
    let text = r#"{ "z" : [1, 2], "a" : { "y" : true } }"#;
    assert_eq!(
        parse_object(text, HARD_ARG_BYTES).expect("parses"),
        serde_json::from_str::<Value>(text).expect("parses")
    );
}

#[test]
fn argument_depth_and_node_budgets_use_the_pinned_constants() {
    let nested = format!(
        r#"{{"a":{}{}}}"#,
        "[".repeat(MAX_ARGS_DEPTH + 1),
        "]".repeat(MAX_ARGS_DEPTH + 1)
    );
    let error = parse_object(&nested, HARD_ARG_BYTES).unwrap_err();
    assert_eq!(error.category(), ErrorCategory::ResourceLimit);
    assert!(error.message().contains("depth"), "{:?}", error.message());

    let elements = vec!["0"; MAX_ARGS_NODES - 1].join(",");
    let wide = format!(r#"{{"a":[{elements}]}}"#);
    let error = parse_object(&wide, HARD_ARG_BYTES).unwrap_err();
    assert_eq!(error.category(), ErrorCategory::ResourceLimit);
    assert!(error.message().contains("node"), "{:?}", error.message());
}

#[test]
fn schema_node_budget_uses_the_pinned_constant() {
    let schema_with = |properties: usize, array_levels: usize| {
        let mut node = String::from(r#"{"type":"null"}"#);
        for _ in 0..array_levels {
            node = format!(r#"{{"type":"array","items":{node}}}"#);
        }
        let entries: Vec<String> = (0..properties)
            .map(|index| format!(r#""p{index}":{node}"#))
            .collect();
        format!(
            r#"{{"type":"object","properties":{{{}}}}}"#,
            entries.join(",")
        )
    };
    // Each property contributes a fixed node count well inside every other
    // budget, so the wide case can only fail on the schema node budget.
    compile(&schema_with(16, 8));
    let error = CompiledSchema::compile(&schema_with(200, 8)).unwrap_err();
    assert_eq!(error.category(), ErrorCategory::ResourceLimit);
    assert!(error.message().contains("node"), "{:?}", error.message());
}
