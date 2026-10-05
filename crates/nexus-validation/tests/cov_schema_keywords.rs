//! Coverage hardening for the closed M0 keyword applicability matrix.
//!
//! These tests pin the externally observable compile-time contract of
//! `src/schema.rs`: which keyword belongs to which node type, that a known
//! keyword on the wrong type is a distinct rejection from an unknown
//! keyword, and that `additionalProperties`, `items`, size keywords, and
//! number bounds are validated exactly. Every assertion goes through the
//! public [`CompiledSchema::compile`] boundary and `AgentError` accessors;
//! schemas are fixed literals, so the suite is deterministic and touches no
//! runtime, tool, clock, or I/O. No private module or helper is named, so
//! the tests keep compiling if the internal layout changes.

#![forbid(unsafe_code)]

use nexus_core::{ErrorCategory, Limits, RetryGuidance};
use nexus_validation::CompiledSchema;

/// Every node type the closed subset accepts.
const TYPES: [&str; 6] = ["object", "array", "string", "number", "boolean", "null"];

/// `(keyword, the only type it belongs to, a member fragment declaring it
/// with a valid value)` for every keyword except `type`, which is universal.
const KEYWORD_CLAUSES: &[(&str, &str, &str)] = &[
    ("properties", "object", r#""properties":{}"#),
    ("required", "object", r#""required":[]"#),
    (
        "additionalProperties",
        "object",
        r#""additionalProperties":true"#,
    ),
    ("minProperties", "object", r#""minProperties":0"#),
    ("maxProperties", "object", r#""maxProperties":0"#),
    ("items", "array", r#""items":{"type":"null"}"#),
    ("minItems", "array", r#""minItems":0"#),
    ("maxItems", "array", r#""maxItems":0"#),
    ("minLength", "string", r#""minLength":0"#),
    ("maxLength", "string", r#""maxLength":0"#),
    ("minimum", "number", r#""minimum":0"#),
    ("maximum", "number", r#""maximum":0"#),
];

const WRONG_TYPE: &str = "schema keyword is not valid for this type";
const UNKNOWN_KEYWORD: &str = "schema contains an unsupported keyword";
const REF_UNSUPPORTED: &str = "schema $ref is unsupported";

fn assert_compiles(schema: &str) {
    if let Err(error) = CompiledSchema::compile(schema) {
        panic!("schema should compile: {schema}: {error:?}");
    }
}

fn assert_rejected(schema: &str, message: &str) {
    let error = match CompiledSchema::compile(schema) {
        Ok(_) => panic!("schema should be rejected: {schema}"),
        Err(error) => error,
    };
    assert_eq!(
        error.category(),
        ErrorCategory::InvalidInput,
        "schema: {schema}"
    );
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry, "schema: {schema}");
    assert_eq!(error.message(), message, "schema: {schema}");
}

/// Builds one node of `ty` carrying `clause`.
///
/// Array nodes declare `items` unless the clause itself is `items`, so a
/// misplaced keyword stays the only fault regardless of key iteration order.
fn node_with_clause(ty: &str, clause: &str) -> String {
    let items = if ty == "array" && !clause.starts_with(r#""items""#) {
        r#","items":{"type":"null"}"#
    } else {
        ""
    };
    format!(r#"{{"type":"{ty}"{items},{clause}}}"#)
}

/// Wraps a node as `properties.v` of an object root, the only legal root.
fn nested(node: &str) -> String {
    format!(r#"{{"type":"object","properties":{{"v":{node}}}}}"#)
}

/// Builds the smallest complete node of `ty`; arrays need `items`.
fn valid_node(ty: &str) -> String {
    if ty == "array" {
        r#"{"type":"array","items":{"type":"null"}}"#.to_owned()
    } else {
        format!(r#"{{"type":"{ty}"}}"#)
    }
}

#[test]
fn every_keyword_compiles_on_its_own_type() {
    for &(_, valid, clause) in KEYWORD_CLAUSES {
        assert_compiles(&nested(&node_with_clause(valid, clause)));
    }
    for &(_, valid, clause) in KEYWORD_CLAUSES {
        let root = format!(r#"{{"type":"{valid}",{clause}}}"#);
        if valid == "object" {
            assert_compiles(&root);
        } else {
            // The root gate, not the keyword, owns this rejection.
            assert_rejected(&root, "schema root type must be object");
        }
    }
}

#[test]
fn known_keywords_on_every_wrong_type_are_rejected() {
    let mut checked = 0;
    for &(_, valid, clause) in KEYWORD_CLAUSES {
        for ty in TYPES {
            if ty == valid {
                continue;
            }
            let schema = nested(&node_with_clause(ty, clause));
            assert_rejected(&schema, WRONG_TYPE);
            checked += 1;
        }
    }
    assert_eq!(checked, KEYWORD_CLAUSES.len() * (TYPES.len() - 1));
}

#[test]
fn keyword_applicability_is_decided_before_value_shape() {
    for node in [
        r#"{"type":"string","properties":[]}"#,
        r#"{"type":"string","minItems":-1}"#,
        r#"{"type":"array","items":{"type":"null"},"minProperties":"x"}"#,
        r#"{"type":"number","items":[]}"#,
        r#"{"type":"boolean","additionalProperties":"no"}"#,
        r#"{"type":"null","minimum":true}"#,
        r#"{"type":"object","items":{}}"#,
    ] {
        assert_rejected(&nested(node), WRONG_TYPE);
    }
}

#[test]
fn misplaced_known_keywords_are_rejected_at_any_nesting_depth() {
    for schema in [
        r#"{"type":"object","properties":{"v":{"type":"array","items":{"type":"string","minimum":0}}}}"#,
        r#"{"type":"object","properties":{"v":{"type":"object","properties":{"w":{"type":"number","maxLength":1}}}}}"#,
        r#"{"type":"object","properties":{"v":{"type":"array","items":{"type":"array","items":{"type":"boolean","minItems":0}}}}}"#,
    ] {
        assert_rejected(schema, WRONG_TYPE);
    }
}

#[test]
fn unknown_and_reserved_keywords_are_distinct_from_misplaced_known_keywords() {
    for ty in TYPES {
        assert_rejected(
            &nested(&node_with_clause(ty, r#""description":"read""#)),
            UNKNOWN_KEYWORD,
        );
        assert_rejected(
            &nested(&node_with_clause(ty, r##""$ref":"#/x""##)),
            REF_UNSUPPORTED,
        );
    }
    assert_rejected(r##"{"type":"object","$ref":"#/x"}"##, REF_UNSUPPORTED);
    for keyword in [
        "Properties",
        "properties ",
        "minlength",
        "minItems ",
        "additionalproperties",
        "$schema",
        "pattern",
        "enum",
        "const",
        "oneOf",
        "definitions",
    ] {
        assert_rejected(
            &nested(&format!(r#"{{"type":"string","{keyword}":0}}"#)),
            UNKNOWN_KEYWORD,
        );
    }
}

#[test]
fn additional_properties_must_be_a_json_boolean() {
    const MESSAGE: &str = "schema additionalProperties must be a boolean";
    for value in [
        r#""true""#,
        r#""false""#,
        "0",
        "1",
        "-1",
        "1.5",
        "null",
        "[]",
        "[true]",
        "{}",
        r#"{"type":"boolean"}"#,
        r#"{"type":"null"}"#,
    ] {
        let clause = format!(r#""additionalProperties":{value}"#);
        assert_rejected(&format!(r#"{{"type":"object",{clause}}}"#), MESSAGE);
        assert_rejected(&nested(&node_with_clause("object", &clause)), MESSAGE);
    }
    assert_compiles(r#"{"type":"object","additionalProperties":true}"#);
    assert_compiles(r#"{"type":"object","additionalProperties":false}"#);
    assert_compiles(
        r#"{"type":"object","properties":{"v":{"type":"object","additionalProperties":false}}}"#,
    );
    assert_compiles(
        r#"{"type":"object","properties":{"v":{"type":"object","additionalProperties":true}}}"#,
    );
}

#[test]
fn additional_properties_boolean_controls_argument_validation() {
    const MAX_ARGS_BYTES: usize = Limits::M0_TEST_ARG_ASSEMBLY_BYTES;
    let open = CompiledSchema::compile(r#"{"type":"object","additionalProperties":true}"#)
        .expect("open object compiles");
    let closed = CompiledSchema::compile(r#"{"type":"object","additionalProperties":false}"#)
        .expect("closed object compiles");
    open.validate(r#"{"extra":1}"#, MAX_ARGS_BYTES)
        .expect("open object accepts undeclared properties");
    closed
        .validate(r#"{}"#, MAX_ARGS_BYTES)
        .expect("closed object accepts an empty object");
    let error = closed
        .validate(r#"{"extra":1}"#, MAX_ARGS_BYTES)
        .expect_err("closed object rejects undeclared properties");
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
}

#[test]
fn closed_objects_reject_undeclared_required_names() {
    assert_rejected(
        r#"{"type":"object","properties":{"a":{"type":"null"}},"required":["b"],"additionalProperties":false}"#,
        "schema required name is undeclared while additionalProperties is false",
    );
    assert_compiles(
        r#"{"type":"object","properties":{"a":{"type":"null"}},"required":["b"],"additionalProperties":true}"#,
    );
    assert_compiles(
        r#"{"type":"object","properties":{"a":{"type":"null"}},"required":["a"],"additionalProperties":false}"#,
    );
}

#[test]
fn arrays_must_declare_a_valid_items_schema() {
    const MISSING: &str = "schema array must declare items";
    for (node, message) in [
        (r#"{"type":"array"}"#, MISSING),
        (r#"{"type":"array","minItems":0}"#, MISSING),
        (r#"{"type":"array","maxItems":0}"#, MISSING),
        (
            r#"{"type":"array","items":null}"#,
            "schema node must be an object",
        ),
        (
            r#"{"type":"array","items":1}"#,
            "schema node must be an object",
        ),
        (
            r#"{"type":"array","items":"null"}"#,
            "schema node must be an object",
        ),
        (
            r#"{"type":"array","items":true}"#,
            "schema node must be an object",
        ),
        (
            r#"{"type":"array","items":[]}"#,
            "schema node must be an object",
        ),
        (
            r#"{"type":"array","items":{}}"#,
            "schema object must declare a type",
        ),
        (
            r#"{"type":"array","items":{"properties":{}}}"#,
            "schema object must declare a type",
        ),
        (
            r#"{"type":"array","items":{"type":"integer"}}"#,
            "schema type is unsupported",
        ),
        (
            r#"{"type":"array","items":{"type":"any"}}"#,
            "schema type is unsupported",
        ),
    ] {
        assert_rejected(&nested(node), message);
    }
    // Every node type is a valid item schema.
    for ty in TYPES {
        let item = valid_node(ty);
        assert_compiles(&nested(&format!(r#"{{"type":"array","items":{item}}}"#)));
    }
    // Arrays nested in arrays must declare items at every level.
    assert_rejected(
        r#"{"type":"object","properties":{"v":{"type":"array","items":{"type":"array"}}}}"#,
        MISSING,
    );
    // A duplicated items key is a parse-level rejection.
    assert_rejected(
        r#"{"type":"object","properties":{"v":{"type":"array","items":{"type":"null"},"items":{"type":"null"}}}}"#,
        "schema contains a duplicate object key",
    );
}

#[test]
fn size_keywords_require_the_non_negative_integer_representation() {
    let owners = [
        ("minLength", "string"),
        ("maxLength", "string"),
        ("minItems", "array"),
        ("maxItems", "array"),
        ("minProperties", "object"),
        ("maxProperties", "object"),
    ];
    for (keyword, ty) in owners {
        let message = format!("schema {keyword} must be a non-negative integer");
        // `-0`, `0.0`, `1.0`, and `1e2` parse as JSON floats, so they are
        // rejected even when numerically integral: the subset accepts only
        // the integer representation (`Number::as_u64`).
        for value in [
            "-1",
            "-0",
            "0.5",
            "1.5",
            "0.0",
            "1.0",
            "1e2",
            "1E2",
            r#""0""#,
            r#""4""#,
            "true",
            "false",
            "null",
            "[]",
            "{}",
            "18446744073709551616",
        ] {
            let clause = format!(r#""{keyword}":{value}"#);
            assert_rejected(&nested(&node_with_clause(ty, &clause)), &message);
        }
        for value in ["0", "1", "9007199254740993", "18446744073709551615"] {
            let clause = format!(r#""{keyword}":{value}"#);
            assert_compiles(&nested(&node_with_clause(ty, &clause)));
        }
    }
}

#[test]
fn inverted_bounds_are_rejected_with_type_specific_messages() {
    for (node, message) in [
        (
            r#"{"type":"string","minLength":2,"maxLength":1}"#,
            "schema string bound is inverted",
        ),
        (
            r#"{"type":"array","items":{"type":"null"},"minItems":2,"maxItems":1}"#,
            "schema array bound is inverted",
        ),
        (
            r#"{"type":"object","minProperties":2,"maxProperties":1}"#,
            "schema property bound is inverted",
        ),
        (
            r#"{"type":"number","minimum":2,"maximum":1}"#,
            "schema number bound is inverted",
        ),
    ] {
        assert_rejected(&nested(node), message);
    }
    // Object bounds at the root exercise the same check on the root node.
    assert_rejected(
        r#"{"type":"object","minProperties":2,"maxProperties":1}"#,
        "schema property bound is inverted",
    );
}

#[test]
fn equal_bounds_are_accepted_for_every_bounded_keyword() {
    for node in [
        r#"{"type":"string","minLength":3,"maxLength":3}"#,
        r#"{"type":"array","items":{"type":"null"},"minItems":3,"maxItems":3}"#,
        r#"{"type":"object","minProperties":3,"maxProperties":3}"#,
        r#"{"type":"number","minimum":3,"maximum":3}"#,
    ] {
        assert_compiles(&nested(node));
    }
    assert_compiles(r#"{"type":"object","minProperties":0,"maxProperties":0}"#);
}

#[test]
fn number_bound_ordering_is_exact_across_representations() {
    for node in [
        r#"{"type":"number","minimum":1.5,"maximum":1}"#,
        r#"{"type":"number","minimum":1,"maximum":0.5}"#,
        r#"{"type":"number","minimum":9007199254740993,"maximum":9007199254740992}"#,
        r#"{"type":"number","minimum":18446744073709551615,"maximum":18446744073709551614}"#,
        r#"{"type":"number","minimum":1.7976931348623157e308,"maximum":18446744073709551615}"#,
        r#"{"type":"number","minimum":-0.5,"maximum":-1}"#,
        r#"{"type":"number","minimum":1e308,"maximum":1e307}"#,
    ] {
        assert_rejected(&nested(node), "schema number bound is inverted");
    }
    for node in [
        r#"{"type":"number","minimum":1.0,"maximum":1}"#,
        r#"{"type":"number","minimum":1,"maximum":1.0}"#,
        r#"{"type":"number","minimum":9007199254740993,"maximum":9007199254740993}"#,
        r#"{"type":"number","minimum":18446744073709551615,"maximum":18446744073709551616}"#,
        r#"{"type":"number","minimum":-0.0,"maximum":0}"#,
        r#"{"type":"number","minimum":1e308,"maximum":1.7976931348623157e308}"#,
    ] {
        assert_compiles(&nested(node));
    }
}

#[test]
fn number_bounds_must_be_json_numbers() {
    const MESSAGE: &str = "schema minimum/maximum must be a number";
    for keyword in ["minimum", "maximum"] {
        for value in [
            r#""0""#,
            r#""1.5""#,
            "null",
            "true",
            "false",
            "[]",
            "[0]",
            "{}",
            r#"{"type":"number"}"#,
        ] {
            let clause = format!(r#""{keyword}":{value}"#);
            assert_rejected(&nested(&node_with_clause("number", &clause)), MESSAGE);
        }
        for value in [
            "0",
            "-1",
            "1.5",
            "-0.0",
            "1e2",
            "1E-2",
            "9007199254740993",
            "18446744073709551615",
            "18446744073709551616",
            "1e308",
        ] {
            let clause = format!(r#""{keyword}":{value}"#);
            assert_compiles(&nested(&node_with_clause("number", &clause)));
        }
        // A literal outside the finite f64 range fails while parsing the
        // schema text, before the bound kind is inspected.
        let clause = format!(r#""{keyword}":1e999"#);
        assert_rejected(
            &nested(&node_with_clause("number", &clause)),
            "schema is not valid JSON",
        );
    }
}

#[test]
fn type_keyword_accepts_exactly_the_six_subset_names() {
    for ty in TYPES {
        assert_compiles(&nested(&valid_node(ty)));
    }
    for value in [
        r#""integer""#,
        r#""any""#,
        r#""Object""#,
        r#""OBJECT""#,
        r#""String""#,
        r#""""#,
        r#""string ""#,
        r#"" string""#,
        r#""number_""#,
        "0",
        "1.5",
        "null",
        "true",
        "false",
        "[]",
        "{}",
    ] {
        assert_rejected(
            &nested(&format!(r#"{{"type":{value}}}"#)),
            "schema type is unsupported",
        );
    }
    assert_rejected(
        r#"{"type":"object","properties":{"v":{}}}"#,
        "schema object must declare a type",
    );
    assert_rejected(
        r#"{"type":"object","properties":{"v":{"properties":{}}}}"#,
        "schema object must declare a type",
    );
}

#[test]
fn non_object_root_types_are_rejected_before_keyword_checks() {
    for schema in [
        r#"{"type":"array","items":{"type":"null"}}"#,
        r#"{"type":"array","items":{"type":"null"},"minItems":0}"#,
        r#"{"type":"string","minLength":0}"#,
        r#"{"type":"number","minimum":0}"#,
        r#"{"type":"boolean"}"#,
        r#"{"type":"null"}"#,
        r#"{"type":"integer"}"#,
        r#"{"type":42}"#,
    ] {
        assert_rejected(schema, "schema root type must be object");
    }
    assert_rejected("{}", "schema object must declare a type");
    assert_rejected(r#"{"properties":{}}"#, "schema object must declare a type");
}
