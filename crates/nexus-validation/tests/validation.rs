//! Adversarial and boundary tests for the closed M0 schema subset.
//!
//! Every assertion goes through the public boundary. No tool, executor, or
//! runtime is involved: validation is a pure function of schema text and
//! argument text, and execution stays the runtime's concern.

use nexus_core::{AgentError, ErrorCategory, Limits};
use nexus_validation::{
    CompiledSchema, MAX_ARGS_DEPTH, MAX_ARGS_NODES, MAX_KEY_BYTES, MAX_SCHEMA_BYTES,
    MAX_SCHEMA_DEPTH, MAX_SCHEMA_PROPERTIES, parse_object,
};

const MAX_ARGS_BYTES: usize = Limits::M0_TEST_ARG_ASSEMBLY_BYTES;

fn compile(schema: &str) -> CompiledSchema {
    CompiledSchema::compile(schema).expect("schema compiles")
}

fn compile_error(schema: &str) -> AgentError {
    CompiledSchema::compile(schema).expect_err("schema must be rejected")
}

fn validate_error(schema: &str, args: &str) -> AgentError {
    compile(schema)
        .validate(args, MAX_ARGS_BYTES)
        .expect_err("arguments must be rejected")
}

fn assert_invalid(error: &AgentError, needle: &str) {
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert!(
        error.message().contains(needle),
        "message {:?} should contain {needle:?}",
        error.message()
    );
}

fn assert_resource_limit(error: &AgentError) {
    assert_eq!(error.category(), ErrorCategory::ResourceLimit);
}

#[test]
fn minimal_object_schema_accepts_arbitrary_object_arguments() {
    let schema = compile(r#"{"type":"object"}"#);
    for args in [
        r#"{}"#,
        r#"{"path":"src"}"#,
        r#"{"a":1,"b":[true,null,{"c":"x"}],"d":{"e":[]}}"#,
    ] {
        let normalized = schema
            .validate(args, MAX_ARGS_BYTES)
            .expect("args validate");
        assert!(normalized.as_str().starts_with('{'));
    }
}

#[test]
fn full_subset_schema_validates_conforming_arguments() {
    let schema = compile(
        r#"{
            "type":"object",
            "properties":{
                "path":{"type":"string","minLength":1,"maxLength":64},
                "count":{"type":"number","minimum":0,"maximum":100},
                "force":{"type":"boolean"},
                "note":{"type":"null"},
                "tags":{"type":"array","items":{"type":"string","maxLength":8},"minItems":0,"maxItems":4},
                "nested":{
                    "type":"object",
                    "properties":{"ok":{"type":"boolean"}},
                    "required":["ok"],
                    "additionalProperties":false,
                    "minProperties":1,
                    "maxProperties":2
                }
            },
            "required":["path"],
            "additionalProperties":false,
            "minProperties":1,
            "maxProperties":6
        }"#,
    );
    let args = r#"{"path":"src","count":100,"force":true,"note":null,"tags":["a","b"],"nested":{"ok":false}}"#;
    let normalized = schema
        .validate(args, MAX_ARGS_BYTES)
        .expect("args validate");
    assert_eq!(
        normalized.as_str(),
        r#"{"count":100,"force":true,"nested":{"ok":false},"note":null,"path":"src","tags":["a","b"]}"#
    );
}

#[test]
fn canonical_form_is_sorted_compact_and_reorder_insensitive() {
    let schema = compile(r#"{"type":"object"}"#);
    let left = schema
        .validate(r#"{ "b" : 1, "a" : { "d" : 2, "c" : 3 } }"#, MAX_ARGS_BYTES)
        .expect("valid");
    let right = schema
        .validate(r#"{"a":{"c":3,"d":2},"b":1}"#, MAX_ARGS_BYTES)
        .expect("valid");
    assert_eq!(left.as_str(), r#"{"a":{"c":3,"d":2},"b":1}"#);
    assert_eq!(left, right);
}

#[test]
fn canonical_form_keeps_serde_json_scalar_forms() {
    let schema = compile(r#"{"type":"object"}"#);
    let normalized = schema
        .validate(r#"{"z":1.0,"a":"\u00e9","n":-0.0}"#, MAX_ARGS_BYTES)
        .expect("valid");
    assert_eq!(normalized.as_str(), "{\"a\":\"é\",\"n\":-0.0,\"z\":1.0}");
}

#[test]
fn duplicate_argument_keys_are_rejected_not_last_key_wins() {
    let schema = compile(r#"{"type":"object"}"#);
    let error = schema
        .validate(r#"{"a":1,"a":2}"#, MAX_ARGS_BYTES)
        .unwrap_err();
    assert_invalid(&error, "duplicate");
    assert!(parse_object(r#"{"a":1,"a":2}"#, MAX_ARGS_BYTES).is_err());
    assert!(
        schema
            .validate(r#"{"outer":{"x":1,"x":2}}"#, MAX_ARGS_BYTES)
            .is_err()
    );
}

#[test]
fn duplicate_schema_keys_are_rejected() {
    let error = compile_error(r#"{"type":"object","type":"object"}"#);
    assert_invalid(&error, "duplicate");
    assert!(
        CompiledSchema::compile(
            r#"{"type":"object","properties":{"v":{"type":"null","type":"null"}}}"#
        )
        .is_err()
    );
}

#[test]
fn malformed_and_trailing_argument_text_is_rejected() {
    let schema = compile(r#"{"type":"object"}"#);
    for args in [
        "",
        "{",
        r#"{"a":}"#,
        r#"{"a":1,}"#,
        r#"{"a":1} trailing"#,
        r#"{"a":1}{"b":2}"#,
    ] {
        let error = schema.validate(args, MAX_ARGS_BYTES).unwrap_err();
        assert_eq!(error.category(), ErrorCategory::InvalidInput, "{args:?}");
    }
}

#[test]
fn non_object_argument_roots_are_rejected() {
    let schema = compile(r#"{"type":"object"}"#);
    for args in ["[]", "[1,2]", "\"text\"", "1", "true", "null"] {
        let error = schema.validate(args, MAX_ARGS_BYTES).unwrap_err();
        assert_eq!(error.category(), ErrorCategory::InvalidInput, "{args:?}");
    }
    for args in ["[]", "1", "null"] {
        assert!(parse_object(args, MAX_ARGS_BYTES).is_err(), "{args:?}");
    }
}

#[test]
fn argument_types_must_match_declared_types() {
    let cases = [
        (
            r#"{"type":"object","properties":{"v":{"type":"string"}}}"#,
            r#"{"v":1}"#,
        ),
        (
            r#"{"type":"object","properties":{"v":{"type":"number"}}}"#,
            r#"{"v":"1"}"#,
        ),
        (
            r#"{"type":"object","properties":{"v":{"type":"boolean"}}}"#,
            r#"{"v":null}"#,
        ),
        (
            r#"{"type":"object","properties":{"v":{"type":"null"}}}"#,
            r#"{"v":false}"#,
        ),
        (
            r#"{"type":"object","properties":{"v":{"type":"array","items":{"type":"null"}}}}"#,
            r#"{"v":{}}"#,
        ),
        (
            r#"{"type":"object","properties":{"v":{"type":"object"}}}"#,
            r#"{"v":[]}"#,
        ),
    ];
    for (schema, args) in cases {
        let error = validate_error(schema, args);
        assert_invalid(&error, "type");
    }
}

#[test]
fn required_and_additional_properties_are_enforced() {
    let schema = compile(
        r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}"#,
    );
    let error = schema.validate(r#"{}"#, MAX_ARGS_BYTES).unwrap_err();
    assert_invalid(&error, "required");
    let error = schema
        .validate(r#"{"path":"src","extra":1}"#, MAX_ARGS_BYTES)
        .unwrap_err();
    assert_invalid(&error, "undeclared");
    schema
        .validate(r#"{"path":"src"}"#, MAX_ARGS_BYTES)
        .expect("closed object accepts its exact shape");
}

#[test]
fn string_length_bounds_count_unicode_code_points() {
    let schema = compile(
        r#"{"type":"object","properties":{"v":{"type":"string","minLength":1,"maxLength":2}}}"#,
    );
    schema
        .validate(r#"{"v":"é"}"#, MAX_ARGS_BYTES)
        .expect("one code point is within bounds");
    schema
        .validate(r#"{"v":"éé"}"#, MAX_ARGS_BYTES)
        .expect("two code points are within bounds");
    assert!(schema.validate(r#"{"v":""}"#, MAX_ARGS_BYTES).is_err());
    assert!(schema.validate(r#"{"v":"ééé"}"#, MAX_ARGS_BYTES).is_err());
}

#[test]
fn number_bounds_are_inclusive() {
    let schema = compile(
        r#"{"type":"object","properties":{"v":{"type":"number","minimum":0,"maximum":10}}}"#,
    );
    for args in [r#"{"v":0}"#, r#"{"v":10}"#, r#"{"v":5.5}"#, r#"{"v":0.0}"#] {
        schema.validate(args, MAX_ARGS_BYTES).expect(args);
    }
    for args in [r#"{"v":-1}"#, r#"{"v":10.5}"#, r#"{"v":-0.0001}"#] {
        let error = schema.validate(args, MAX_ARGS_BYTES).unwrap_err();
        assert_eq!(error.category(), ErrorCategory::InvalidInput, "{args}");
    }
}

#[test]
fn large_integer_bounds_compare_exactly() {
    let schema = compile(
        r#"{"type":"object","properties":{"v":{"type":"number","minimum":9007199254740993,"maximum":9007199254740995}}}"#,
    );
    schema
        .validate(r#"{"v":9007199254740993}"#, MAX_ARGS_BYTES)
        .expect("lower bound is inclusive");
    schema
        .validate(r#"{"v":9007199254740995}"#, MAX_ARGS_BYTES)
        .expect("upper bound is inclusive");
    assert!(
        schema
            .validate(r#"{"v":9007199254740992}"#, MAX_ARGS_BYTES)
            .is_err()
    );
    assert!(
        schema
            .validate(r#"{"v":9007199254740996}"#, MAX_ARGS_BYTES)
            .is_err()
    );
    // The float literal rounds to 9007199254740992.0, which is below the
    // exact integer minimum; naive f64 comparison would accept it.
    assert!(
        schema
            .validate(r#"{"v":9007199254740993.0}"#, MAX_ARGS_BYTES)
            .is_err()
    );
}

#[test]
fn array_bounds_and_item_schemas_are_enforced() {
    let schema = compile(
        r#"{"type":"object","properties":{"v":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":2}}}"#,
    );
    schema
        .validate(r#"{"v":["a"]}"#, MAX_ARGS_BYTES)
        .expect("ok");
    schema
        .validate(r#"{"v":["a","b"]}"#, MAX_ARGS_BYTES)
        .expect("ok");
    assert!(schema.validate(r#"{"v":[]}"#, MAX_ARGS_BYTES).is_err());
    assert!(
        schema
            .validate(r#"{"v":["a","b","c"]}"#, MAX_ARGS_BYTES)
            .is_err()
    );
    assert!(schema.validate(r#"{"v":["a",1]}"#, MAX_ARGS_BYTES).is_err());
}

#[test]
fn object_property_bounds_are_enforced() {
    let schema = compile(r#"{"type":"object","minProperties":1,"maxProperties":2}"#);
    schema.validate(r#"{"a":1}"#, MAX_ARGS_BYTES).expect("ok");
    schema
        .validate(r#"{"a":1,"b":2}"#, MAX_ARGS_BYTES)
        .expect("ok");
    assert!(schema.validate(r#"{}"#, MAX_ARGS_BYTES).is_err());
    assert!(
        schema
            .validate(r#"{"a":1,"b":2,"c":3}"#, MAX_ARGS_BYTES)
            .is_err()
    );
}

#[test]
fn byte_budget_boundary_is_exact() {
    let schema = compile(r#"{"type":"object"}"#);
    let args = r#"{"path":"src"}"#;
    schema
        .validate(args, args.len())
        .expect("exact budget passes");
    let error = schema.validate(args, args.len() - 1).unwrap_err();
    assert_resource_limit(&error);
    let error = schema.validate(args, 0).unwrap_err();
    assert_invalid(&error, "nonzero");
    let error = parse_object(args, args.len() - 1).unwrap_err();
    assert_resource_limit(&error);
}

#[test]
fn byte_budget_configuration_outside_the_hard_maximum_is_rejected() {
    let schema = compile(r#"{"type":"object"}"#);
    for budget in [0, MAX_ARGS_BYTES + 1, usize::MAX] {
        for error in [
            schema.validate("{}", budget).unwrap_err(),
            parse_object("{}", budget).unwrap_err(),
        ] {
            assert_eq!(error.category(), ErrorCategory::InvalidInput, "{budget}");
            assert_eq!(error.retry(), nexus_core::RetryGuidance::DoNotRetry);
        }
    }
    schema
        .validate("{}", MAX_ARGS_BYTES)
        .expect("the hard maximum is accepted");
    parse_object("{}", MAX_ARGS_BYTES).expect("the hard maximum is accepted");
    schema
        .validate("{}", "{}".len())
        .expect("a minimal nonzero budget is accepted");
}

#[test]
fn byte_budget_hard_maximum_boundary_is_exact() {
    let schema = compile(r#"{"type":"object"}"#);
    let base = r#"{"path":"src"}"#;
    let at_max = format!("{base}{}", " ".repeat(MAX_ARGS_BYTES - base.len()));
    assert_eq!(at_max.len(), MAX_ARGS_BYTES);
    schema
        .validate(&at_max, MAX_ARGS_BYTES)
        .expect("argument text at the hard maximum is accepted");
    let error = schema.validate(&at_max, MAX_ARGS_BYTES - 1).unwrap_err();
    assert_resource_limit(&error);
    let error = schema.validate(&at_max, MAX_ARGS_BYTES + 1).unwrap_err();
    assert_invalid(&error, "hard maximum");
}

#[test]
fn canonical_growth_counts_against_the_byte_budget() {
    let schema = compile(r#"{"type":"object"}"#);
    let args = r#"{"n":1E2}"#;
    assert_eq!(args.len(), 9);
    let normalized = schema
        .validate(args, 11)
        .expect("canonical form fits the budget");
    assert_eq!(normalized.as_str(), r#"{"n":100.0}"#);
    let error = schema.validate(args, 10).unwrap_err();
    assert_resource_limit(&error);
}

#[test]
fn argument_depth_budget_boundary() {
    let schema = compile(r#"{"type":"object"}"#);
    let nested = |levels: usize| format!(r#"{{"a":{}{}}}"#, "[".repeat(levels), "]".repeat(levels));
    schema
        .validate(&nested(MAX_ARGS_DEPTH), MAX_ARGS_BYTES)
        .expect("depth at the budget passes");
    let error = schema
        .validate(&nested(MAX_ARGS_DEPTH + 1), MAX_ARGS_BYTES)
        .unwrap_err();
    assert_resource_limit(&error);
    let error = parse_object(&nested(MAX_ARGS_DEPTH + 1), MAX_ARGS_BYTES).unwrap_err();
    assert_resource_limit(&error);
}

#[test]
fn argument_node_budget_boundary() {
    let schema = compile(r#"{"type":"object"}"#);
    let build = |elements: usize| format!(r#"{{"a":[{}]}}"#, vec!["0"; elements].join(","));
    // Root object + array + elements share the node budget.
    let at_budget = build(MAX_ARGS_NODES - 2);
    assert!(parse_object(&at_budget, MAX_ARGS_BYTES).is_ok());
    schema
        .validate(&at_budget, MAX_ARGS_BYTES)
        .expect("nodes at the budget pass");
    let over_budget = build(MAX_ARGS_NODES - 1);
    let error = parse_object(&over_budget, MAX_ARGS_BYTES).unwrap_err();
    assert_resource_limit(&error);
    let error = schema.validate(&over_budget, MAX_ARGS_BYTES).unwrap_err();
    assert_resource_limit(&error);
}

#[test]
fn parse_object_returns_plain_values_within_budgets() {
    let value = parse_object(
        r#"{"a":[1,2.5,"x",true,null,{}],"b":{"c":false}}"#,
        MAX_ARGS_BYTES,
    )
    .expect("parses");
    assert!(value.is_object());
    assert_eq!(value["a"].as_array().map(Vec::len), Some(6));
    assert_eq!(value["a"][3].as_bool(), Some(true));
    assert_eq!(value["b"]["c"].as_bool(), Some(false));
}

#[test]
fn schema_root_must_be_a_typed_object() {
    for schema in [
        "{}",
        r#"{"properties":{}}"#,
        r#"{"type":"string"}"#,
        r#"{"type":"array","items":{"type":"null"}}"#,
        "[]",
        "\"text\"",
        "null",
    ] {
        assert!(CompiledSchema::compile(schema).is_err(), "{schema}");
    }
}

#[test]
fn malformed_schema_text_is_rejected() {
    for schema in [
        "",
        "{",
        r#"{"type":"object""#,
        r#"{"type":"object",}"#,
        r#"{"type":"object"} trailing"#,
    ] {
        let error = CompiledSchema::compile(schema).unwrap_err();
        assert_eq!(error.category(), ErrorCategory::InvalidInput, "{schema:?}");
    }
}

#[test]
fn unsupported_or_untyped_schema_nodes_are_rejected() {
    for schema in [
        r#"{"type":"object","properties":{"v":{"type":"integer"}}}"#,
        r#"{"type":"object","properties":{"v":{"type":"any"}}}"#,
        r#"{"type":"object","properties":{"v":{"type":42}}}"#,
        r#"{"type":"object","properties":{"v":{}}}"#,
        r#"{"type":"object","properties":{"v":{"properties":{}}}}"#,
    ] {
        assert!(CompiledSchema::compile(schema).is_err(), "{schema}");
    }
}

#[test]
fn refs_and_unknown_keywords_are_rejected() {
    for schema in [
        r##"{"type":"object","$ref":"#/definitions/x"}"##,
        r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object"}"#,
        r#"{"type":"object","description":"read"}"#,
        r#"{"type":"object","enum":[1]}"#,
        r#"{"type":"string","pattern":"^a$"}"#,
        r#"{"type":"number","format":"float"}"#,
        r#"{"type":"object","properties":{"v":{"type":"string","const":"x"}}}"#,
        r#"{"type":"object","oneOf":[]}"#,
        r#"{"type":"object","properties":{"v":{"type":"string","default":"x"}}}"#,
    ] {
        assert!(CompiledSchema::compile(schema).is_err(), "{schema}");
    }
    let ref_error = compile_error(r##"{"type":"object","$ref":"#/x"}"##);
    assert_invalid(&ref_error, "$ref");
}

#[test]
fn known_keywords_on_the_wrong_type_are_rejected() {
    for schema in [
        r#"{"type":"string","properties":{}}"#,
        r#"{"type":"object","items":{"type":"null"}}"#,
        r#"{"type":"array","items":{"type":"null"},"minLength":1}"#,
        r#"{"type":"number","maxItems":1}"#,
        r#"{"type":"boolean","minProperties":0}"#,
        r#"{"type":"null","maximum":0}"#,
        r#"{"type":"string","required":["a"]}"#,
    ] {
        assert!(CompiledSchema::compile(schema).is_err(), "{schema}");
    }
}

#[test]
fn property_and_required_declarations_are_validated() {
    for schema in [
        r#"{"type":"object","properties":[]}"#,
        r#"{"type":"object","properties":{"v":1}}"#,
        r#"{"type":"object","properties":{"":{"type":"null"}}}"#,
        r#"{"type":"object","required":"v"}"#,
        r#"{"type":"object","required":[1]}"#,
        r#"{"type":"object","required":["v","v"]}"#,
        r#"{"type":"object","required":[""]}"#,
    ] {
        assert!(CompiledSchema::compile(schema).is_err(), "{schema}");
    }
    let long_name = "x".repeat(MAX_KEY_BYTES + 1);
    let long_property =
        format!(r#"{{"type":"object","properties":{{"{long_name}":{{"type":"null"}}}}}}"#);
    assert!(CompiledSchema::compile(&long_property).is_err());
    let long_required = format!(r#"{{"type":"object","required":["{long_name}"]}}"#);
    assert!(CompiledSchema::compile(&long_required).is_err());
    let name = "x".repeat(MAX_KEY_BYTES);
    let at_bound = format!(r#"{{"type":"object","properties":{{"{name}":{{"type":"null"}}}}}}"#);
    CompiledSchema::compile(&at_bound).expect("name at the bound compiles");
}

#[test]
fn additional_properties_must_be_a_boolean() {
    assert!(
        CompiledSchema::compile(r#"{"type":"object","additionalProperties":{"type":"string"}}"#)
            .is_err()
    );
    let open = compile(r#"{"type":"object","additionalProperties":true}"#);
    open.validate(r#"{"anything":1}"#, MAX_ARGS_BYTES)
        .expect("open object accepts extras");
    let closed = compile(r#"{"type":"object","additionalProperties":false}"#);
    closed
        .validate(r#"{}"#, MAX_ARGS_BYTES)
        .expect("closed object accepts an empty object");
    assert!(
        closed
            .validate(r#"{"anything":1}"#, MAX_ARGS_BYTES)
            .is_err()
    );
}

#[test]
fn size_keywords_require_non_negative_integers() {
    for schema in [
        r#"{"type":"string","minLength":-1}"#,
        r#"{"type":"string","maxLength":1.5}"#,
        r#"{"type":"string","maxLength":"4"}"#,
        r#"{"type":"array","items":{"type":"null"},"minItems":true}"#,
        r#"{"type":"array","items":{"type":"null"},"maxItems":-2}"#,
        r#"{"type":"object","minProperties":-1}"#,
        r#"{"type":"object","maxProperties":2.5}"#,
    ] {
        assert!(CompiledSchema::compile(schema).is_err(), "{schema}");
    }
}

#[test]
fn inverted_bounds_are_rejected() {
    for schema in [
        r#"{"type":"string","minLength":2,"maxLength":1}"#,
        r#"{"type":"array","items":{"type":"null"},"minItems":2,"maxItems":1}"#,
        r#"{"type":"object","minProperties":2,"maxProperties":1}"#,
        r#"{"type":"number","minimum":2,"maximum":1}"#,
    ] {
        assert!(CompiledSchema::compile(schema).is_err(), "{schema}");
    }
}

#[test]
fn number_bounds_must_be_json_numbers() {
    assert!(CompiledSchema::compile(r#"{"type":"number","minimum":"0"}"#).is_err());
    assert!(CompiledSchema::compile(r#"{"type":"number","maximum":null}"#).is_err());
    // serde_json rejects numbers outside the finite f64 range during parsing.
    assert!(CompiledSchema::compile(r#"{"type":"number","minimum":1e999}"#).is_err());
}

#[test]
fn arrays_require_an_explicit_item_schema() {
    for schema in [
        r#"{"type":"object","properties":{"v":{"type":"array"}}}"#,
        r#"{"type":"object","properties":{"v":{"type":"array","items":[]}}}"#,
        r#"{"type":"object","properties":{"v":{"type":"array","items":1}}}"#,
    ] {
        assert!(CompiledSchema::compile(schema).is_err(), "{schema}");
    }
}

#[test]
fn undeclared_required_names_with_closed_objects_are_rejected() {
    let error = compile_error(
        r#"{"type":"object","properties":{"a":{"type":"null"}},"required":["b"],"additionalProperties":false}"#,
    );
    assert_invalid(&error, "required");
    compile(r#"{"type":"object","required":["b"],"additionalProperties":true}"#);
}

#[test]
fn schema_depth_budget_boundary() {
    let nested = |levels: usize| {
        let mut node = r#"{"type":"null"}"#.to_owned();
        for _ in 0..levels {
            node = format!(r#"{{"type":"array","items":{node}}}"#);
        }
        format!(r#"{{"type":"object","properties":{{"v":{node}}}}}"#)
    };
    // Root object sits at JSON depth 0; each array level adds one object and
    // the innermost `{"type":"null"}` adds another object plus its "type"
    // string, so `MAX_SCHEMA_DEPTH - 3` items levels exactly fill the budget.
    let at_budget = nested(MAX_SCHEMA_DEPTH - 3);
    CompiledSchema::compile(&at_budget).expect("depth at the budget compiles");
    let over_budget = nested(MAX_SCHEMA_DEPTH - 2);
    let error = CompiledSchema::compile(&over_budget).unwrap_err();
    assert_resource_limit(&error);
}

#[test]
fn schema_byte_budget_boundary() {
    let base = r#"{"type":"object"}"#;
    let padded = format!("{base}{}", " ".repeat(MAX_SCHEMA_BYTES - base.len()));
    assert_eq!(padded.len(), MAX_SCHEMA_BYTES);
    CompiledSchema::compile(&padded).expect("schema at the byte budget compiles");
    let over_budget = format!("{padded} ");
    let error = CompiledSchema::compile(&over_budget).unwrap_err();
    assert_resource_limit(&error);
}

#[test]
fn schema_property_budget_boundary() {
    let schema_with = |count: usize| {
        let properties: Vec<String> = (0..count)
            .map(|index| format!(r#""p{index}":{{"type":"null"}}"#))
            .collect();
        format!(
            r#"{{"type":"object","properties":{{{}}}}}"#,
            properties.join(",")
        )
    };
    CompiledSchema::compile(&schema_with(MAX_SCHEMA_PROPERTIES))
        .expect("properties at the budget compile");
    let error = CompiledSchema::compile(&schema_with(MAX_SCHEMA_PROPERTIES + 1)).unwrap_err();
    assert_resource_limit(&error);
}

#[test]
fn schema_required_count_is_bounded() {
    let names: Vec<String> = (0..=MAX_SCHEMA_PROPERTIES)
        .map(|index| format!(r#""p{index}""#))
        .collect();
    let schema = format!(r#"{{"type":"object","required":[{}]}}"#, names.join(","));
    let error = CompiledSchema::compile(&schema).unwrap_err();
    assert_resource_limit(&error);
}

#[test]
fn compiled_schema_is_reusable_and_validation_is_pure() {
    let schema =
        compile(r#"{"type":"object","properties":{"v":{"type":"string"}},"required":["v"]}"#);
    let first = schema
        .validate(r#"{"v":"a"}"#, MAX_ARGS_BYTES)
        .expect("valid");
    let second = schema
        .validate(r#"{"v":"a"}"#, MAX_ARGS_BYTES)
        .expect("valid");
    assert_eq!(first, second);
    assert!(schema.validate(r#"{"v":1}"#, MAX_ARGS_BYTES).is_err());
    let third = schema
        .validate(r#"{"v":"a"}"#, MAX_ARGS_BYTES)
        .expect("valid after rejection");
    assert_eq!(first, third);
}

#[test]
fn all_failures_are_non_retryable() {
    let schema_error = compile_error(r#"{"type":"object","unknown":1}"#);
    assert_eq!(schema_error.retry(), nexus_core::RetryGuidance::DoNotRetry);
    let argument_error = validate_error(
        r#"{"type":"object","properties":{"v":{"type":"string"}}}"#,
        r#"{"v":1}"#,
    );
    assert_eq!(
        argument_error.retry(),
        nexus_core::RetryGuidance::DoNotRetry
    );
    let budget_error = compile(r#"{"type":"object"}"#)
        .validate("{}", 0)
        .unwrap_err();
    assert_eq!(budget_error.retry(), nexus_core::RetryGuidance::DoNotRetry);
}
