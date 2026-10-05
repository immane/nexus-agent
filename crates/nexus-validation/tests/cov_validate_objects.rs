//! Object and array validation coverage for the closed M0 subset.
//!
//! These tests pin the behavior of `properties`, `required`,
//! `additionalProperties` (open and closed), `minProperties`/`maxProperties`,
//! `minItems`/`maxItems` with item schemas, nested containers, and the
//! compile-time rejection of undeclared `required` names in closed objects.
//!
//! Every assertion goes through the public boundary: `CompiledSchema::compile`
//! and `CompiledSchema::validate`. Inputs are fixed text; nothing here is
//! time-, environment-, or order-dependent.

#![forbid(unsafe_code)]

use nexus_core::{AgentError, ErrorCategory, Limits, RetryGuidance};
use nexus_validation::CompiledSchema;

const MAX_ARGS_BYTES: usize = Limits::M0_TEST_ARG_ASSEMBLY_BYTES;

fn compile(schema: &str) -> CompiledSchema {
    CompiledSchema::compile(schema).expect("schema compiles")
}

fn compile_error(schema: &str) -> AgentError {
    CompiledSchema::compile(schema).expect_err("schema must be rejected")
}

/// Validates and returns the canonical text, asserting acceptance.
fn validate_ok(schema: &str, args: &str) -> String {
    compile(schema)
        .validate(args, MAX_ARGS_BYTES)
        .unwrap_or_else(|error| panic!("{args} should validate: {}", error.message()))
        .as_str()
        .to_owned()
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

fn assert_rejected(schema: &str, args: &str, needle: &str) {
    let error = validate_error(schema, args);
    assert_invalid(&error, needle);
}

#[test]
fn declared_properties_are_validated_only_when_present() {
    let schema =
        r#"{"type":"object","properties":{"name":{"type":"string"},"count":{"type":"number"}}}"#;
    assert_eq!(validate_ok(schema, r#"{}"#), "{}");
    assert_eq!(validate_ok(schema, r#"{"name":"x"}"#), r#"{"name":"x"}"#);
    assert_eq!(validate_ok(schema, r#"{"count":1}"#), r#"{"count":1}"#);
    assert_rejected(schema, r#"{"name":1}"#, "type does not match");
    assert_rejected(schema, r#"{"count":"1"}"#, "type does not match");
}

#[test]
fn required_properties_must_be_present_even_when_false_or_null() {
    let schema = r#"{"type":"object","properties":{"a":{"type":"null"},"b":{"type":"boolean"}},"required":["a","b"]}"#;
    assert_rejected(schema, r#"{}"#, "missing a required property");
    assert_rejected(schema, r#"{"a":null}"#, "missing a required property");
    assert_rejected(schema, r#"{"b":false}"#, "missing a required property");
    // Presence, not truthiness: null and false satisfy `required`.
    assert_eq!(
        validate_ok(schema, r#"{"a":null,"b":false}"#),
        r#"{"a":null,"b":false}"#
    );
}

#[test]
fn additional_properties_default_open_and_explicit_true_accept_extras() {
    for schema in [
        r#"{"type":"object","properties":{"path":{"type":"string"}}}"#,
        r#"{"type":"object","properties":{"path":{"type":"string"}},"additionalProperties":true}"#,
    ] {
        assert_eq!(
            validate_ok(schema, r#"{"path":"src","extra":{"deep":[1,true,null]}}"#),
            r#"{"extra":{"deep":[1,true,null]},"path":"src"}"#
        );
        // Declared properties are still validated when extras are allowed.
        assert_rejected(schema, r#"{"path":1,"extra":2}"#, "type does not match");
    }
}

#[test]
fn closed_objects_accept_exact_shape_and_reject_undeclared_properties() {
    let schema = r#"{"type":"object","properties":{"path":{"type":"string"},"mode":{"type":"string"}},"required":["path"],"additionalProperties":false}"#;
    assert_eq!(
        validate_ok(schema, r#"{"path":"src"}"#),
        r#"{"path":"src"}"#
    );
    assert_eq!(
        validate_ok(schema, r#"{"mode":"fast","path":"src"}"#),
        r#"{"mode":"fast","path":"src"}"#
    );
    assert_rejected(schema, r#"{"path":"src","extra":1}"#, "undeclared property");
    // `required` is checked before `additionalProperties`.
    assert_rejected(schema, r#"{"extra":1}"#, "missing a required property");
}

#[test]
fn closed_empty_object_rejects_every_property() {
    let schema = r#"{"type":"object","additionalProperties":false}"#;
    assert_eq!(validate_ok(schema, r#"{}"#), "{}");
    assert_rejected(schema, r#"{"a":1}"#, "undeclared property");
}

#[test]
fn property_bounds_are_inclusive_and_count_every_key() {
    let schema = r#"{"type":"object","minProperties":1,"maxProperties":2}"#;
    assert_eq!(validate_ok(schema, r#"{"a":1}"#), r#"{"a":1}"#);
    assert_eq!(validate_ok(schema, r#"{"a":1,"b":2}"#), r#"{"a":1,"b":2}"#);
    assert_rejected(schema, r#"{}"#, "below its property bound");
    assert_rejected(
        schema,
        r#"{"a":1,"b":2,"c":3}"#,
        "exceeds its property bound",
    );
}

#[test]
fn zero_property_bounds_are_exact() {
    let empty_only = r#"{"type":"object","maxProperties":0}"#;
    assert_eq!(validate_ok(empty_only, r#"{}"#), "{}");
    assert_rejected(empty_only, r#"{"a":null}"#, "exceeds its property bound");

    let closed_empty_only =
        r#"{"type":"object","minProperties":0,"maxProperties":0,"additionalProperties":false}"#;
    assert_eq!(validate_ok(closed_empty_only, r#"{}"#), "{}");
    // The bound is checked before the undeclared-property check.
    assert_rejected(
        closed_empty_only,
        r#"{"a":1}"#,
        "exceeds its property bound",
    );
}

#[test]
fn property_bounds_apply_to_undeclared_keys_when_open() {
    let schema = r#"{"type":"object","properties":{"a":{"type":"null"}},"minProperties":2,"maxProperties":2}"#;
    assert_rejected(schema, r#"{"a":null}"#, "below its property bound");
    assert_eq!(
        validate_ok(schema, r#"{"a":null,"b":1}"#),
        r#"{"a":null,"b":1}"#
    );
    assert_rejected(
        schema,
        r#"{"a":null,"b":1,"c":2}"#,
        "exceeds its property bound",
    );
}

#[test]
fn required_and_bounds_interact_predictably() {
    let schema = r#"{"type":"object","properties":{"a":{"type":"number"}},"required":["a"],"minProperties":2}"#;
    // The property bound is checked before `required`.
    assert_rejected(schema, r#"{"a":1}"#, "below its property bound");
    assert_rejected(schema, r#"{"b":2,"c":3}"#, "missing a required property");
    assert_eq!(validate_ok(schema, r#"{"a":1,"b":2}"#), r#"{"a":1,"b":2}"#);
}

#[test]
fn unsatisfiable_required_with_zero_max_properties_rejects_all_arguments() {
    // Compilation does not cross-check `required` against `maxProperties`;
    // the conflict surfaces as a runtime bound failure, not a compile error.
    let schema = compile(
        r#"{"type":"object","properties":{"a":{"type":"null"}},"required":["a"],"maxProperties":0}"#,
    );
    let error = schema
        .validate(r#"{"a":null}"#, MAX_ARGS_BYTES)
        .expect_err("any argument exceeds the zero property bound");
    assert_invalid(&error, "exceeds its property bound");
    let error = schema
        .validate(r#"{}"#, MAX_ARGS_BYTES)
        .expect_err("the required property cannot be present");
    assert_invalid(&error, "missing a required property");
}

#[test]
fn array_item_schemas_apply_to_every_element() {
    let schema = r#"{"type":"object","properties":{"v":{"type":"array","items":{"type":"string","minLength":1}}}}"#;
    assert_eq!(validate_ok(schema, r#"{"v":[]}"#), r#"{"v":[]}"#);
    assert_eq!(
        validate_ok(schema, r#"{"v":["a","bb"]}"#),
        r#"{"v":["a","bb"]}"#
    );
    assert_rejected(schema, r#"{"v":["a",1]}"#, "type does not match");
    assert_rejected(schema, r#"{"v":[1,"a"]}"#, "type does not match");
    assert_rejected(schema, r#"{"v":["a",""]}"#, "minimum length");
}

#[test]
fn array_size_bounds_are_inclusive() {
    let schema = r#"{"type":"object","properties":{"v":{"type":"array","items":{"type":"null"},"minItems":1,"maxItems":2}}}"#;
    assert_eq!(validate_ok(schema, r#"{"v":[null]}"#), r#"{"v":[null]}"#);
    assert_eq!(
        validate_ok(schema, r#"{"v":[null,null]}"#),
        r#"{"v":[null,null]}"#
    );
    assert_rejected(schema, r#"{"v":[]}"#, "below its size bound");
    assert_rejected(
        schema,
        r#"{"v":[null,null,null]}"#,
        "exceeds its size bound",
    );
}

#[test]
fn zero_item_bounds_are_exact() {
    let empty_only = r#"{"type":"object","properties":{"v":{"type":"array","items":{"type":"null"},"maxItems":0}}}"#;
    assert_eq!(validate_ok(empty_only, r#"{"v":[]}"#), r#"{"v":[]}"#);
    assert_rejected(empty_only, r#"{"v":[null]}"#, "exceeds its size bound");

    let min_zero = r#"{"type":"object","properties":{"v":{"type":"array","items":{"type":"null"},"minItems":0}}}"#;
    assert_eq!(validate_ok(min_zero, r#"{"v":[]}"#), r#"{"v":[]}"#);
}

#[test]
fn array_size_is_checked_before_item_types() {
    let schema = r#"{"type":"object","properties":{"v":{"type":"array","items":{"type":"string"},"minItems":2,"maxItems":3}}}"#;
    assert_rejected(schema, r#"{"v":[1]}"#, "below its size bound");
    assert_rejected(schema, r#"{"v":[1,2,3,4]}"#, "exceeds its size bound");
    assert_rejected(schema, r#"{"v":[1,2]}"#, "type does not match");
}

#[test]
fn nested_arrays_validate_recursively() {
    let schema = r#"{"type":"object","properties":{"grid":{"type":"array","items":{"type":"array","items":{"type":"number","minimum":0},"minItems":1,"maxItems":2}}}}"#;
    assert_eq!(
        validate_ok(schema, r#"{"grid":[[0],[1,2]]}"#),
        r#"{"grid":[[0],[1,2]]}"#
    );
    assert_rejected(schema, r#"{"grid":[[]]}"#, "below its size bound");
    assert_rejected(schema, r#"{"grid":[[0,1,2]]}"#, "exceeds its size bound");
    assert_rejected(schema, r#"{"grid":[[-1]]}"#, "below its minimum");
    assert_rejected(schema, r#"{"grid":[[0],"x"]}"#, "type does not match");
}

#[test]
fn nested_objects_are_validated_at_every_level() {
    let schema = r#"{
        "type":"object",
        "properties":{
            "profile":{
                "type":"object",
                "properties":{
                    "name":{"type":"string"},
                    "address":{
                        "type":"object",
                        "properties":{"city":{"type":"string"}},
                        "required":["city"],
                        "additionalProperties":false
                    }
                },
                "required":["name"],
                "additionalProperties":false
            }
        },
        "required":["profile"],
        "additionalProperties":false
    }"#;
    let args = r#"{"profile":{"name":"ada","address":{"city":"london"}}}"#;
    assert_eq!(
        validate_ok(schema, args),
        r#"{"profile":{"address":{"city":"london"},"name":"ada"}}"#
    );
    assert_rejected(
        schema,
        r#"{"profile":{"address":{"city":"london"}}}"#,
        "missing a required property",
    );
    assert_rejected(
        schema,
        r#"{"profile":{"name":"ada","address":{}}}"#,
        "missing a required property",
    );
    assert_rejected(
        schema,
        r#"{"profile":{"name":"ada","address":{"city":"london","zip":"x"}}}"#,
        "undeclared property",
    );
    assert_rejected(
        schema,
        r#"{"profile":{"name":"ada","address":{"city":1}}}"#,
        "type does not match",
    );
    assert_rejected(
        schema,
        r#"{"profile":{"name":"ada","extra":true}}"#,
        "undeclared property",
    );
}

#[test]
fn nested_property_bounds_apply_per_level() {
    let schema = r#"{"type":"object","properties":{"child":{"type":"object","minProperties":1,"maxProperties":2}},"minProperties":1}"#;
    assert_eq!(
        validate_ok(schema, r#"{"child":{"x":1}}"#),
        r#"{"child":{"x":1}}"#
    );
    assert_rejected(schema, r#"{"child":{}}"#, "below its property bound");
    assert_rejected(
        schema,
        r#"{"child":{"x":1,"y":2,"z":3}}"#,
        "exceeds its property bound",
    );
}

#[test]
fn additional_properties_is_decided_per_object_level() {
    let closed_root_open_child = r#"{
        "type":"object",
        "properties":{"child":{"type":"object","additionalProperties":true}},
        "additionalProperties":false
    }"#;
    assert_eq!(
        validate_ok(closed_root_open_child, r#"{"child":{"anything":[1]}}"#),
        r#"{"child":{"anything":[1]}}"#
    );
    assert_rejected(
        closed_root_open_child,
        r#"{"extra":1}"#,
        "undeclared property",
    );

    let open_root_closed_child = r#"{
        "type":"object",
        "properties":{"child":{"type":"object","additionalProperties":false}},
        "additionalProperties":true
    }"#;
    assert_eq!(
        validate_ok(open_root_closed_child, r#"{"child":{},"extra":1}"#),
        r#"{"child":{},"extra":1}"#
    );
    assert_rejected(
        open_root_closed_child,
        r#"{"child":{"extra":1}}"#,
        "undeclared property",
    );
}

#[test]
fn arrays_of_closed_objects_validate_each_element() {
    let schema = r#"{"type":"object","properties":{"items":{"type":"array","items":{"type":"object","properties":{"id":{"type":"number"}},"required":["id"],"additionalProperties":false},"minItems":1,"maxItems":3}}}"#;
    assert_eq!(
        validate_ok(schema, r#"{"items":[{"id":1},{"id":2}]}"#),
        r#"{"items":[{"id":1},{"id":2}]}"#
    );
    assert_rejected(schema, r#"{"items":[]}"#, "below its size bound");
    assert_rejected(
        schema,
        r#"{"items":[{"id":1},{}]}"#,
        "missing a required property",
    );
    assert_rejected(
        schema,
        r#"{"items":[{"id":1,"extra":2}]}"#,
        "undeclared property",
    );
    assert_rejected(schema, r#"{"items":[{"id":"1"}]}"#, "type does not match");
    assert_rejected(
        schema,
        r#"{"items":[{"id":1},{"id":2},{"id":3},{"id":4}]}"#,
        "exceeds its size bound",
    );
}

#[test]
fn objects_and_arrays_mix_through_multiple_levels() {
    let schema = r#"{
        "type":"object",
        "properties":{
            "groups":{
                "type":"array",
                "items":{
                    "type":"object",
                    "properties":{
                        "name":{"type":"string"},
                        "members":{"type":"array","items":{"type":"string","minLength":1},"minItems":1}
                    },
                    "required":["name","members"],
                    "additionalProperties":false
                },
                "minItems":1,
                "maxItems":2
            }
        },
        "required":["groups"],
        "additionalProperties":false
    }"#;
    let args = r#"{"groups":[{"members":["b","a"],"name":"core"}]}"#;
    // Arrays keep element order; objects are sorted at every level.
    assert_eq!(
        validate_ok(schema, args),
        r#"{"groups":[{"members":["b","a"],"name":"core"}]}"#
    );
    assert_rejected(
        schema,
        r#"{"groups":[{"name":"core","members":[]}]}"#,
        "below its size bound",
    );
    assert_rejected(
        schema,
        r#"{"groups":[{"name":"core","members":[""]}]}"#,
        "minimum length",
    );
    assert_rejected(
        schema,
        r#"{"groups":[{"name":"core","members":["a"],"lead":null}]}"#,
        "undeclared property",
    );
}

#[test]
fn property_names_are_case_sensitive_and_exact() {
    let schema = r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}"#;
    assert_rejected(schema, r#"{"Path":"src"}"#, "missing a required property");
    assert_rejected(
        schema,
        r#"{"path":"src","Path":"src"}"#,
        "undeclared property",
    );
}

#[test]
fn deep_nesting_within_the_argument_budget_validates() {
    // Seven nested closed objects. Each object property level costs two JSON
    // schema depths (`properties` map plus schema object), so six wrappers
    // around the leaf stay inside `MAX_SCHEMA_DEPTH`; the innermost value is
    // validated and the single-key shape round-trips to the same canonical
    // text.
    let mut schema = r#"{"type":"object","additionalProperties":false,"properties":{"leaf":{"type":"number"}},"required":["leaf"]}"#.to_owned();
    let mut args = r#"{"leaf":7}"#.to_owned();
    for _ in 0..6 {
        schema = format!(
            r#"{{"type":"object","additionalProperties":false,"properties":{{"child":{schema}}},"required":["child"]}}"#
        );
        args = format!(r#"{{"child":{args}}}"#);
    }
    assert_eq!(validate_ok(&schema, &args), args);
    assert_rejected(&schema, r#"{"child":{}}"#, "missing a required property");
}

#[test]
fn canonical_output_sorts_nested_keys_and_preserves_array_order() {
    let schema = r#"{"type":"object"}"#;
    let args = r#"{"z":[{"b":2,"a":1},3],"a":{"d":4,"c":{"f":6,"e":5}}}"#;
    assert_eq!(
        validate_ok(schema, args),
        r#"{"a":{"c":{"e":5,"f":6},"d":4},"z":[{"a":1,"b":2},3]}"#
    );
    // Reordered input text yields the identical canonical value.
    let reordered = r#"{"a":{"c":{"e":5,"f":6},"d":4},"z":[{"a":1,"b":2},3]}"#;
    assert_eq!(validate_ok(schema, args), validate_ok(schema, reordered));
}

#[test]
fn object_and_array_failures_are_invalid_input_and_non_retryable() {
    let cases = [
        (r#"{"type":"object","minProperties":1}"#, r#"{}"#),
        (r#"{"type":"object","maxProperties":1}"#, r#"{"a":1,"b":2}"#),
        (r#"{"type":"object","required":["a"]}"#, r#"{}"#),
        (
            r#"{"type":"object","additionalProperties":false}"#,
            r#"{"a":1}"#,
        ),
        (
            r#"{"type":"object","properties":{"v":{"type":"array","items":{"type":"null"},"minItems":1}}}"#,
            r#"{"v":[]}"#,
        ),
        (
            r#"{"type":"object","properties":{"v":{"type":"array","items":{"type":"null"},"maxItems":1}}}"#,
            r#"{"v":[null,null]}"#,
        ),
        (
            r#"{"type":"object","properties":{"v":{"type":"array","items":{"type":"string"}}}}"#,
            r#"{"v":[1]}"#,
        ),
        (
            r#"{"type":"object","properties":{"v":{"type":"object","required":["a"]}}}"#,
            r#"{"v":{}}"#,
        ),
    ];
    for (schema, args) in cases {
        let error = validate_error(schema, args);
        assert_eq!(
            error.category(),
            ErrorCategory::InvalidInput,
            "{schema} / {args}"
        );
        assert_eq!(
            error.retry(),
            RetryGuidance::DoNotRetry,
            "{schema} / {args}"
        );
    }
}

#[test]
fn undeclared_required_names_are_rejected_for_closed_objects() {
    // Root level.
    let error = compile_error(
        r#"{"type":"object","properties":{"a":{"type":"null"}},"required":["b"],"additionalProperties":false}"#,
    );
    assert_invalid(&error, "undeclared");
    // Nested level: the check applies to every closed object, not just the root.
    let error = compile_error(
        r#"{"type":"object","properties":{"child":{"type":"object","properties":{"a":{"type":"null"}},"required":["b"],"additionalProperties":false}}}"#,
    );
    assert_invalid(&error, "undeclared");
    // Declaring the required name is accepted.
    compile(
        r#"{"type":"object","properties":{"b":{"type":"null"}},"required":["b"],"additionalProperties":false}"#,
    );
    // With additionalProperties open, an undeclared required name is legal and
    // the value must still carry it.
    let open = compile(r#"{"type":"object","required":["b"]}"#);
    let error = open
        .validate(r#"{}"#, MAX_ARGS_BYTES)
        .expect_err("required is still enforced");
    assert_invalid(&error, "missing a required property");
    assert_eq!(
        validate_ok(r#"{"type":"object","required":["b"]}"#, r#"{"b":1}"#),
        r#"{"b":1}"#
    );
}
