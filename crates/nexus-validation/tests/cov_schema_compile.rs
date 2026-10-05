#![forbid(unsafe_code)]

//! Coverage hardening for [`CompiledSchema::compile`].
//!
//! Every assertion goes through the `nexus-validation` public boundary. The
//! tests pin the closed M0 compilation contract: the root must be a typed
//! object; each type accepts exactly its locked keyword set and rejects the
//! other types' keywords as misplaced; `$ref`, `$schema`, and unknown
//! keywords are rejected at every node; untyped or non-object nodes are
//! rejected; and the byte, depth, node, property, and key budgets are exact
//! (the bound value compiles, one over fails with
//! `ErrorCategory::ResourceLimit`). Inputs are fixed strings only: no
//! randomness, clocks, or I/O are involved.

use nexus_core::{AgentError, ErrorCategory, RetryGuidance};
use nexus_validation::{
    CompiledSchema, MAX_KEY_BYTES, MAX_SCHEMA_BYTES, MAX_SCHEMA_DEPTH, MAX_SCHEMA_NODES,
    MAX_SCHEMA_PROPERTIES,
};

fn compile_error(schema: &str) -> AgentError {
    CompiledSchema::compile(schema).expect_err("schema must be rejected")
}

fn assert_invalid(error: &AgentError, needle: &str) {
    assert_eq!(error.category(), ErrorCategory::InvalidInput, "{error}");
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry, "{error}");
    assert!(
        error.message().contains(needle),
        "message {:?} should contain {needle:?}",
        error.message()
    );
}

fn assert_limit(error: &AgentError, needle: &str) {
    assert_eq!(error.category(), ErrorCategory::ResourceLimit, "{error}");
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry, "{error}");
    assert!(
        error.message().contains(needle),
        "message {:?} should contain {needle:?}",
        error.message()
    );
}

#[test]
fn root_must_be_a_typed_object() {
    CompiledSchema::compile(r#"{"type":"object"}"#).expect("minimal typed object root compiles");
    CompiledSchema::compile("  { \"type\" : \"object\" }  ")
        .expect("whitespace around a typed object root compiles");

    let cases = [
        ("empty object", "{}", "must declare a type"),
        (
            "untyped properties",
            r#"{"properties":{}}"#,
            "must declare a type",
        ),
        (
            "string root",
            r#"{"type":"string"}"#,
            "root type must be object",
        ),
        (
            "array root",
            r#"{"type":"array","items":{"type":"null"}}"#,
            "root type must be object",
        ),
        (
            "number root",
            r#"{"type":"number"}"#,
            "root type must be object",
        ),
        (
            "boolean root",
            r#"{"type":"boolean"}"#,
            "root type must be object",
        ),
        (
            "null root",
            r#"{"type":"null"}"#,
            "root type must be object",
        ),
        ("numeric type", r#"{"type":42}"#, "root type must be object"),
        ("null type", r#"{"type":null}"#, "root type must be object"),
        (
            "case-sensitive type",
            r#"{"type":"Object"}"#,
            "root type must be object",
        ),
        ("array document", "[{}]", "must be an object"),
        ("scalar document", r#""object""#, "must be an object"),
        ("number document", "1", "must be an object"),
        ("true document", "true", "must be an object"),
        ("null document", "null", "must be an object"),
        ("empty text", "", "not valid JSON"),
        ("whitespace only", "   ", "not valid JSON"),
        (
            "trailing content",
            r#"{"type":"object"} trailing"#,
            "not valid JSON",
        ),
        (
            "duplicate type key",
            r#"{"type":"object","type":"object"}"#,
            "duplicate",
        ),
    ];
    for (case, schema, needle) in cases {
        let error = compile_error(schema);
        assert_invalid(&error, needle);
        assert!(!error.message().is_empty(), "{case}");
    }
}

#[test]
fn every_type_accepts_its_full_locked_keyword_set() {
    let cases = [
        (
            "object",
            r#"{"type":"object","properties":{"v":{"type":"object","properties":{"a":{"type":"null"}},"required":["a"],"additionalProperties":false,"minProperties":0,"maxProperties":1}}}"#,
        ),
        (
            "array",
            r#"{"type":"object","properties":{"v":{"type":"array","items":{"type":"null"},"minItems":0,"maxItems":1}}}"#,
        ),
        (
            "string",
            r#"{"type":"object","properties":{"v":{"type":"string","minLength":0,"maxLength":1}}}"#,
        ),
        (
            "number",
            r#"{"type":"object","properties":{"v":{"type":"number","minimum":-1,"maximum":1}}}"#,
        ),
        (
            "boolean",
            r#"{"type":"object","properties":{"v":{"type":"boolean"}}}"#,
        ),
        (
            "null",
            r#"{"type":"object","properties":{"v":{"type":"null"}}}"#,
        ),
    ];
    for (type_name, schema) in cases {
        CompiledSchema::compile(schema)
            .unwrap_or_else(|error| panic!("{type_name} schema must compile: {error}"));
    }
}

#[test]
fn keywords_from_other_types_are_rejected_as_misplaced() {
    let fragments = [
        ("properties", r#""properties":{}"#),
        ("required", r#""required":[]"#),
        ("additionalProperties", r#""additionalProperties":true"#),
        ("minProperties", r#""minProperties":0"#),
        ("maxProperties", r#""maxProperties":1"#),
        ("items", r#""items":{"type":"null"}"#),
        ("minItems", r#""minItems":0"#),
        ("maxItems", r#""maxItems":1"#),
        ("minLength", r#""minLength":0"#),
        ("maxLength", r#""maxLength":1"#),
        ("minimum", r#""minimum":0"#),
        ("maximum", r#""maximum":1"#),
    ];
    let allowed: &[(&str, &[&str])] = &[
        (
            "object",
            &[
                "properties",
                "required",
                "additionalProperties",
                "minProperties",
                "maxProperties",
            ],
        ),
        ("array", &["items", "minItems", "maxItems"]),
        ("string", &["minLength", "maxLength"]),
        ("number", &["minimum", "maximum"]),
        ("boolean", &[]),
        ("null", &[]),
    ];
    for &(type_name, allowed_keywords) in allowed {
        for &(keyword, fragment) in &fragments {
            if allowed_keywords.contains(&keyword) {
                continue;
            }
            // Non-object types are only legal below the object root, so each
            // type's node is exercised as a property value.
            let schema = format!(
                r#"{{"type":"object","properties":{{"v":{{"type":"{type_name}",{fragment}}}}}}}"#
            );
            let error = compile_error(&schema);
            assert_invalid(&error, "not valid for this type");
            assert!(
                !error.message().contains("unsupported"),
                "{type_name} + {keyword} must be a placement error, not an unknown keyword"
            );
        }
    }
}

#[test]
fn ref_schema_and_unknown_keywords_are_rejected_at_every_node() {
    for schema in [
        r##"{"type":"object","$ref":"#/definitions/x"}"##,
        r##"{"type":"object","properties":{"v":{"type":"boolean","$ref":"#/x"}}}"##,
        r##"{"type":"object","properties":{"v":{"type":"string","$ref":"#/x"}}}"##,
        r##"{"type":"object","properties":{"v":{"type":"array","items":{"type":"null","$ref":"#/x"}}}}"##,
        r##"{"type":"object","properties":{"v":{"type":"object","properties":{"w":{"type":"null"}},"$ref":"#/x"}}}"##,
    ] {
        let error = compile_error(schema);
        assert_invalid(&error, "$ref");
    }

    // `$schema` is an ordinary unknown keyword for this closed subset.
    let unknown_keywords = [
        (
            "$schema",
            r#""$schema":"https://json-schema.org/draft/2020-12/schema""#,
        ),
        ("$id", r#""$id":"x""#),
        ("$comment", r#""$comment":"x""#),
        ("$defs", r#""$defs":{}"#),
        ("definitions", r#""definitions":{}"#),
        ("title", r#""title":"x""#),
        ("description", r#""description":"x""#),
        ("default", r#""default":null"#),
        ("examples", r#""examples":[]"#),
        ("enum", r#""enum":[1]"#),
        ("const", r#""const":1"#),
        ("oneOf", r#""oneOf":[]"#),
        ("anyOf", r#""anyOf":[]"#),
        ("allOf", r#""allOf":[]"#),
        ("not", r#""not":{}"#),
        ("if", r#""if":{}"#),
        ("then", r#""then":{}"#),
        ("else", r#""else":{}"#),
        ("pattern", r#""pattern":"^a$""#),
        ("format", r#""format":"date""#),
        ("patternProperties", r#""patternProperties":{}"#),
        ("propertyNames", r#""propertyNames":{}"#),
        ("uniqueItems", r#""uniqueItems":true"#),
        ("multipleOf", r#""multipleOf":2"#),
        ("exclusiveMinimum", r#""exclusiveMinimum":0"#),
        ("exclusiveMaximum", r#""exclusiveMaximum":1"#),
        ("contains", r#""contains":{}"#),
        ("prefixItems", r#""prefixItems":[]"#),
        ("unevaluatedProperties", r#""unevaluatedProperties":false"#),
        ("additionalItems", r#""additionalItems":false"#),
        ("dependencies", r#""dependencies":{}"#),
        ("dependentRequired", r#""dependentRequired":{}"#),
        ("dependentSchemas", r#""dependentSchemas":{}"#),
        ("contentEncoding", r#""contentEncoding":"base64""#),
        ("contentMediaType", r#""contentMediaType":"text/plain""#),
        ("deprecated", r#""deprecated":true"#),
        ("readOnly", r#""readOnly":true"#),
        ("writeOnly", r#""writeOnly":true"#),
    ];
    for (keyword, fragment) in unknown_keywords {
        for schema in [
            format!(r#"{{"type":"object",{fragment}}}"#),
            format!(r#"{{"type":"object","properties":{{"v":{{"type":"string",{fragment}}}}}}}"#),
            format!(
                r#"{{"type":"object","properties":{{"v":{{"type":"array","items":{{"type":"null",{fragment}}}}}}}}}"#
            ),
            format!(
                r#"{{"type":"object","properties":{{"v":{{"type":"object","properties":{{"w":{{"type":"null",{fragment}}}}}}}}}}}"#
            ),
        ] {
            let error = compile_error(&schema);
            assert_invalid(&error, "unsupported keyword");
            assert!(
                !error.message().contains("not valid"),
                "{keyword} must be an unknown-keyword error"
            );
        }
    }
}

#[test]
fn untyped_or_unsupported_nodes_are_rejected_at_every_position() {
    let untyped = [
        (
            "empty property",
            r#"{"type":"object","properties":{"v":{}}}"#,
        ),
        (
            "object keywords only",
            r#"{"type":"object","properties":{"v":{"properties":{"w":{"type":"null"}}}}}"#,
        ),
        (
            "array keywords only",
            r#"{"type":"object","properties":{"v":{"items":{"type":"null"}}}}"#,
        ),
        (
            "string keywords only",
            r#"{"type":"object","properties":{"v":{"minLength":1}}}"#,
        ),
        (
            "unknown keyword only",
            r#"{"type":"object","properties":{"v":{"description":"x"}}}"#,
        ),
        (
            "untyped items",
            r#"{"type":"object","properties":{"v":{"type":"array","items":{}}}}"#,
        ),
        (
            "nested object property",
            r#"{"type":"object","properties":{"v":{"type":"object","properties":{"w":{}}}}}"#,
        ),
        (
            "nested array items",
            r#"{"type":"object","properties":{"v":{"type":"array","items":{"type":"object","properties":{"w":{}}}}}}"#,
        ),
    ];
    for (case, schema) in untyped {
        let error = compile_error(schema);
        assert_invalid(&error, "must declare a type");
        assert!(!error.message().is_empty(), "{case}");
    }

    let bad_types = [
        ("null", "null"),
        ("number", "42"),
        ("boolean", "true"),
        ("array", "[]"),
        ("object", "{}"),
        ("empty string", r#""""#),
        ("integer", r#""integer""#),
        ("any", r#""any""#),
        ("mixed case", r#""Object""#),
        ("padded", r#""object ""#),
    ];
    for (case, type_value) in bad_types {
        let schema = format!(r#"{{"type":"object","properties":{{"v":{{"type":{type_value}}}}}}}"#);
        let error = compile_error(&schema);
        assert_invalid(&error, "type is unsupported");
        assert!(!error.message().is_empty(), "{case}");
    }

    for schema in [
        r#"{"type":"object","properties":{"v":1}}"#,
        r#"{"type":"object","properties":{"v":[]}}"#,
        r#"{"type":"object","properties":{"v":{"type":"array","items":1}}}"#,
        r#"{"type":"object","properties":{"v":{"type":"array","items":[]}}}"#,
    ] {
        let error = compile_error(schema);
        assert_invalid(&error, "node must be an object");
    }

    // The root takes the root-typing path, not the node path.
    for schema in [r#"{"type":42}"#, r#"{"type":"integer"}"#] {
        let error = compile_error(schema);
        assert_invalid(&error, "root type must be object");
    }
}

#[test]
fn schema_byte_budget_is_exact_and_measured_in_bytes() {
    let base = r#"{"type":"object"}"#;
    let at_budget = format!("{base}{}", " ".repeat(MAX_SCHEMA_BYTES - base.len()));
    assert_eq!(at_budget.len(), MAX_SCHEMA_BYTES);
    CompiledSchema::compile(&at_budget).expect("schema at the byte budget compiles");

    let one_byte_over = format!("{at_budget} ");
    assert_eq!(one_byte_over.len(), MAX_SCHEMA_BYTES + 1);
    assert_limit(&compile_error(&one_byte_over), "byte budget");

    // Over-budget text is rejected before it is parsed at all.
    let garbage_over = format!("{}garbage", " ".repeat(MAX_SCHEMA_BYTES));
    assert_limit(&compile_error(&garbage_over), "byte budget");

    // The budget counts bytes: a multibyte property name near the bound is
    // measured by its encoded length, not its character count.
    let name = "é".repeat(MAX_KEY_BYTES / 2);
    assert_eq!(name.len(), MAX_KEY_BYTES);
    assert_eq!(name.chars().count(), MAX_KEY_BYTES / 2);
    let core = format!(r#"{{"type":"object","properties":{{"{name}":{{"type":"null"}}}}}}"#);
    let padded = format!("{core}{}", " ".repeat(MAX_SCHEMA_BYTES - core.len()));
    assert_eq!(padded.len(), MAX_SCHEMA_BYTES);
    assert!(padded.chars().count() < padded.len());
    CompiledSchema::compile(&padded).expect("multibyte schema at the byte budget compiles");

    // One extra two-byte character is over the byte budget even though the
    // character count grows by only one.
    let over = format!("{padded}é");
    assert_eq!(over.chars().count(), padded.chars().count() + 1);
    assert!(over.len() > MAX_SCHEMA_BYTES);
    assert_limit(&compile_error(&over), "byte budget");
}

#[test]
fn key_byte_bound_is_exact_for_properties_and_required() {
    let name = "x".repeat(MAX_KEY_BYTES);
    let property = format!(r#"{{"type":"object","properties":{{"{name}":{{"type":"null"}}}}}}"#);
    CompiledSchema::compile(&property).expect("property name at the bound compiles");
    let required = format!(r#"{{"type":"object","required":["{name}"]}}"#);
    CompiledSchema::compile(&required).expect("required name at the bound compiles");

    let over = "x".repeat(MAX_KEY_BYTES + 1);
    let property = format!(r#"{{"type":"object","properties":{{"{over}":{{"type":"null"}}}}}}"#);
    assert_invalid(&compile_error(&property), "property name is invalid");
    let required = format!(r#"{{"type":"object","required":["{over}"]}}"#);
    assert_invalid(&compile_error(&required), "required name is invalid");

    // The key bound is bytes, not characters.
    let multibyte = "é".repeat(MAX_KEY_BYTES / 2 + 1);
    assert_eq!(multibyte.len(), MAX_KEY_BYTES + 2);
    let property =
        format!(r#"{{"type":"object","properties":{{"{multibyte}":{{"type":"null"}}}}}}"#);
    assert_invalid(&compile_error(&property), "property name is invalid");
}

#[test]
fn schema_depth_budget_is_exact() {
    let nested = |levels: usize| {
        let mut node = r#"{"type":"null"}"#.to_owned();
        for _ in 0..levels {
            node = format!(r#"{{"type":"array","items":{node}}}"#);
        }
        format!(r#"{{"type":"object","properties":{{"v":{node}}}}}"#)
    };
    // The root object sits at depth 0, the `properties` object at depth 1,
    // the property value at depth 2; each array adds one level and the
    // innermost `"type"` string is one deeper than its object.
    let at_budget = nested(MAX_SCHEMA_DEPTH - 3);
    CompiledSchema::compile(&at_budget).expect("depth at the budget compiles");
    let over_budget = nested(MAX_SCHEMA_DEPTH - 2);
    assert_limit(&compile_error(&over_budget), "depth budget");

    // Parse budgets are checked before any schema semantics.
    let deep_with_unknown = {
        let mut node = r#"{"type":"null"}"#.to_owned();
        for _ in 0..(MAX_SCHEMA_DEPTH - 2) {
            node = format!(r#"{{"type":"array","items":{node}}}"#);
        }
        format!(r#"{{"type":"object","unknown":1,"properties":{{"v":{node}}}}}"#)
    };
    assert_limit(&compile_error(&deep_with_unknown), "depth budget");

    let deep_wrong_root = {
        let mut node = r#"{"type":"null"}"#.to_owned();
        for _ in 0..(MAX_SCHEMA_DEPTH - 2) {
            node = format!(r#"{{"type":"array","items":{node}}}"#);
        }
        format!(r#"{{"type":"string","properties":{{"v":{node}}}}}"#)
    };
    assert_limit(&compile_error(&deep_wrong_root), "depth budget");
}

#[test]
fn schema_node_budget_is_exact() {
    // Each property is a `MAX_SCHEMA_DEPTH - 3`-level array chain: two nodes
    // per array plus a two-node null leaf. The root contributes three nodes
    // and the `required` array one, so `4 + properties * chain == 2048`.
    let levels = MAX_SCHEMA_DEPTH - 3;
    let chain = {
        let mut node = r#"{"type":"null"}"#.to_owned();
        for _ in 0..levels {
            node = format!(r#"{{"type":"array","items":{node}}}"#);
        }
        node
    };
    let chain_nodes = 2 * levels + 2;
    assert_eq!(
        (MAX_SCHEMA_NODES - 4) % chain_nodes,
        0,
        "the node budget must divide evenly for this construction"
    );
    let property_count = (MAX_SCHEMA_NODES - 4) / chain_nodes;
    assert!(property_count <= MAX_SCHEMA_PROPERTIES);

    let build = |properties: usize, required: &str| {
        let declared: Vec<String> = (0..properties)
            .map(|index| format!(r#""p{index}":{chain}"#))
            .collect();
        format!(
            r#"{{"type":"object","properties":{{{}}},"required":[{required}]}}"#,
            declared.join(",")
        )
    };
    let at_budget = build(property_count, "");
    CompiledSchema::compile(&at_budget).expect("nodes at the budget compile");

    // One extra `required` string pushes the node count to 2049.
    let over_budget = build(property_count, r#""x""#);
    assert_limit(&compile_error(&over_budget), "node budget");
}

#[test]
fn schema_property_budget_is_global_and_exact() {
    let null = r#"{"type":"null"}"#;
    let object_with = |prefix: &str, count: usize| {
        let properties: Vec<String> = (0..count)
            .map(|index| format!(r#""{prefix}{index}":{null}"#))
            .collect();
        format!(
            r#"{{"type":"object","properties":{{{}}}}}"#,
            properties.join(",")
        )
    };

    CompiledSchema::compile(&object_with("p", MAX_SCHEMA_PROPERTIES))
        .expect("properties at the budget compile");
    assert_limit(
        &compile_error(&object_with("p", MAX_SCHEMA_PROPERTIES + 1)),
        "property budget",
    );

    // The budget is shared across nested objects. The root itself declares
    // `left` and `right`, so 2 + 127 + 127 fills the budget exactly and
    // 2 + 127 + 128 is one over.
    let nested = |right: usize| {
        format!(
            r#"{{"type":"object","properties":{{"left":{},"right":{}}}}}"#,
            object_with("l", 127),
            object_with("r", right)
        )
    };
    CompiledSchema::compile(&nested(127)).expect("2 + 127 + 127 properties compile");
    assert_limit(&compile_error(&nested(128)), "property budget");

    // `required` names have their own count budget and do not consume the
    // property budget: 256 declared properties plus 256 required names
    // compile together.
    let properties: Vec<String> = (0..MAX_SCHEMA_PROPERTIES)
        .map(|index| format!(r#""p{index}":{null}"#))
        .collect();
    let required: Vec<String> = (0..MAX_SCHEMA_PROPERTIES)
        .map(|index| format!(r#""r{index}""#))
        .collect();
    let combined = format!(
        r#"{{"type":"object","properties":{{{}}},"required":[{}]}}"#,
        properties.join(","),
        required.join(",")
    );
    CompiledSchema::compile(&combined).expect("256 properties plus 256 required names compile");

    // Required names alone are bounded by the same numeric budget.
    let at_budget = format!(r#"{{"type":"object","required":[{}]}}"#, required.join(","));
    CompiledSchema::compile(&at_budget).expect("required names at the budget compile");
    let over_required = format!(
        r#"{{"type":"object","required":[{},"r-over"]}}"#,
        required.join(",")
    );
    assert_limit(&compile_error(&over_required), "property budget");
}

#[test]
fn structural_failures_report_their_specific_reason() {
    let cases = [
        (
            r#"{"type":"object","properties":[]}"#,
            "properties must be an object",
        ),
        (
            r#"{"type":"object","properties":{"v":1}}"#,
            "node must be an object",
        ),
        (
            r#"{"type":"object","properties":{"":{"type":"null"}}}"#,
            "property name is invalid",
        ),
        (
            r#"{"type":"object","required":"v"}"#,
            "required must be an array",
        ),
        (
            r#"{"type":"object","required":[1]}"#,
            "required must be an array",
        ),
        (
            r#"{"type":"object","required":["v","v"]}"#,
            "duplicate name",
        ),
        (
            r#"{"type":"object","required":[""]}"#,
            "required name is invalid",
        ),
        (
            r#"{"type":"object","additionalProperties":{"type":"string"}}"#,
            "additionalProperties must be a boolean",
        ),
        (
            r#"{"type":"object","minProperties":-1}"#,
            "minProperties must be a non-negative integer",
        ),
        (
            r#"{"type":"object","maxProperties":1.5}"#,
            "maxProperties must be a non-negative integer",
        ),
        (
            r#"{"type":"object","minProperties":2,"maxProperties":1}"#,
            "property bound is inverted",
        ),
        (
            r#"{"type":"object","properties":{"a":{"type":"null"}},"required":["b"],"additionalProperties":false}"#,
            "undeclared",
        ),
        (
            r#"{"type":"object","properties":{"v":{"type":"array"}}}"#,
            "array must declare items",
        ),
        (
            r#"{"type":"object","properties":{"v":{"type":"array","items":{"type":"null"},"minItems":-1}}}"#,
            "minItems must be a non-negative integer",
        ),
        (
            r#"{"type":"object","properties":{"v":{"type":"array","items":{"type":"null"},"maxItems":true}}}"#,
            "maxItems must be a non-negative integer",
        ),
        (
            r#"{"type":"object","properties":{"v":{"type":"array","items":{"type":"null"},"minItems":2,"maxItems":1}}}"#,
            "array bound is inverted",
        ),
        (
            r#"{"type":"object","properties":{"v":{"type":"string","minLength":-1}}}"#,
            "minLength must be a non-negative integer",
        ),
        (
            r#"{"type":"object","properties":{"v":{"type":"string","maxLength":2.5}}}"#,
            "maxLength must be a non-negative integer",
        ),
        (
            r#"{"type":"object","properties":{"v":{"type":"string","minLength":2,"maxLength":1}}}"#,
            "string bound is inverted",
        ),
        (
            r#"{"type":"object","properties":{"v":{"type":"number","minimum":"0"}}}"#,
            "minimum/maximum must be a number",
        ),
        (
            r#"{"type":"object","properties":{"v":{"type":"number","maximum":null}}}"#,
            "minimum/maximum must be a number",
        ),
        (
            r#"{"type":"object","properties":{"v":{"type":"number","minimum":2,"maximum":1}}}"#,
            "number bound is inverted",
        ),
        (
            r#"{"type":"object","properties":{"v":{"type":"number","minimum":1e999}}}"#,
            "not valid JSON",
        ),
    ];
    for (schema, needle) in cases {
        let error = compile_error(schema);
        assert_invalid(&error, needle);
    }
}

#[test]
fn accepted_schemas_compile_deterministically() {
    let schema_text = r#"{
        "type":"object",
        "properties":{
            "a":{"type":"string","minLength":1,"maxLength":8},
            "b":{"type":"array","items":{"type":"number","minimum":-1,"maximum":1},"minItems":1,"maxItems":3},
            "c":{"type":"object","properties":{"d":{"type":"boolean"}},"required":["d"],"additionalProperties":false},
            "e":{"type":"null"}
        },
        "required":["a"],
        "additionalProperties":true,
        "minProperties":1,
        "maxProperties":8
    }"#;
    let first = CompiledSchema::compile(schema_text).expect("full subset schema compiles");
    let second = CompiledSchema::compile(schema_text).expect("recompilation succeeds");
    assert_eq!(
        first, second,
        "compilation is deterministic and value-based"
    );

    let reordered = r#"{"maxProperties":8,"minProperties":1,"additionalProperties":true,"required":["a"],"properties":{"e":{"type":"null"},"c":{"additionalProperties":false,"required":["d"],"properties":{"d":{"type":"boolean"}},"type":"object"},"b":{"maxItems":3,"minItems":1,"items":{"maximum":1,"minimum":-1,"type":"number"},"type":"array"},"a":{"maxLength":8,"minLength":1,"type":"string"}},"type":"object"}"#;
    assert_eq!(
        first,
        CompiledSchema::compile(reordered).expect("reordered schema compiles"),
        "compiled identity ignores JSON key order and whitespace"
    );
}
