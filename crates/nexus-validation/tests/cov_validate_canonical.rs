//! Canonicalization hardening tests for `CompiledSchema::validate`.
//!
//! Every assertion goes through the public boundary: `CompiledSchema`,
//! `validate`, and `parse_object`. Inputs are fixed literals, so the suite is
//! deterministic and touches no runtime, clock, provider, or I/O.
//!
//! Coverage focus:
//!
//! - compact, recursively key-sorted (UTF-8 byte order) canonical output,
//! - reorder-insensitive equivalence for object keys and insignificant
//!   whitespace, while array order stays significant,
//! - serde_json scalar round-trip forms (`1.0`, `-0.0`, exponent literals,
//!   escape forms) with no numeric normalization beyond that round-trip,
//! - duplicate-key rejection at every depth, so canonical output cannot
//!   contain duplicates,
//! - compiled-schema reuse and purity across repeated, cloned, and
//!   concurrent calls.

#![forbid(unsafe_code)]

use nexus_core::{AgentError, ErrorCategory, Limits, NormalizedArgs, RetryGuidance};
use nexus_validation::{CompiledSchema, parse_object};

const MAX_ARGS_BYTES: usize = Limits::M0_TEST_ARG_ASSEMBLY_BYTES;

fn compile(schema: &str) -> CompiledSchema {
    CompiledSchema::compile(schema).expect("schema compiles")
}

fn normalized(schema: &CompiledSchema, args: &str) -> NormalizedArgs {
    schema
        .validate(args, MAX_ARGS_BYTES)
        .expect("args validate")
}

fn canonical(schema: &CompiledSchema, args: &str) -> String {
    normalized(schema, args).as_str().to_owned()
}

fn assert_invalid(error: &AgentError, needle: &str) {
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert!(
        error.message().contains(needle),
        "message {:?} should contain {needle:?}",
        error.message()
    );
}

#[test]
fn canonical_output_is_compact_and_recursively_sorted() {
    let schema = compile(r#"{"type":"object"}"#);
    let args =
        r#"{ "z" : 1, "a" : { "d" : 2, "c" : [ { "y" : 1, "x" : 2 } , 3 ] }, "m" : [ 1 , 2 ] }"#;
    assert_eq!(
        normalized(&schema, args).as_str(),
        r#"{"a":{"c":[{"x":2,"y":1},3],"d":2},"m":[1,2],"z":1}"#
    );
    // Compactness is structural: whitespace is dropped everywhere except
    // inside string values, which are preserved byte-for-byte.
    assert_eq!(
        canonical(&schema, "{\n\t\"b\" : [ 1 , 2 ] ,\n\t\"a\" : \"x y\"\n}"),
        r#"{"a":"x y","b":[1,2]}"#
    );
    // Empty containers survive at every level.
    assert_eq!(
        canonical(&schema, r#"{"b":{"d":[],"c":{}},"a":{}}"#),
        r#"{"a":{},"b":{"c":{},"d":[]}}"#
    );
}

#[test]
fn object_keys_sort_by_utf8_byte_order() {
    let schema = compile(r#"{"type":"object"}"#);
    // Insertion order is the exact reverse of the expected UTF-8 byte order:
    // "" < "10" < "9" < "A" < "Z" < "a" < "aA" < "aa" < "b" < "é" < "中" < "😀".
    let args = r#"{"\ud83d\ude00":12,"\u4e2d":11,"\u00e9":10,"b":9,"aa":8,"aA":7,"a":6,"Z":5,"A":4,"9":3,"10":2,"":1}"#;
    let expected =
        r#"{"":1,"10":2,"9":3,"A":4,"Z":5,"a":6,"aA":7,"aa":8,"b":9,"é":10,"中":11,"😀":12}"#;
    assert_eq!(canonical(&schema, args), expected);
    // The same keys inserted in canonical order canonicalize identically.
    assert_eq!(canonical(&schema, expected), expected);
}

#[test]
fn reordering_object_keys_and_whitespace_is_equivalent() {
    let schema = compile(r#"{"type":"object"}"#);
    let expected = r#"{"n":-0.0,"outer":{"a":null,"b":[1,{"x":3,"y":2}]},"z":1.0}"#;
    let permutations = [
        r#"{"z":1.0,"outer":{"b":[1,{"y":2,"x":3}],"a":null},"n":-0.0}"#,
        r#"{"n":-0.0,"z":1.0,"outer":{"a":null,"b":[1,{"x":3,"y":2}]}}"#,
        r#"{
            "outer" : { "b" : [ 1 , { "x" : 3 , "y" : 2 } ] , "a" : null } ,
            "n" : -0.0 ,
            "z" : 1.0
        }"#,
        r#"{"outer":{"a":null,"b":[1,{"y":2,"x":3}]},"n":-0.0,"z":1.0}"#,
    ];
    let first = normalized(&schema, permutations[0]);
    assert_eq!(first.as_str(), expected);
    for args in &permutations[1..] {
        let other = normalized(&schema, args);
        assert_eq!(other, first, "{args}");
        assert_eq!(other.as_str(), expected, "{args}");
    }
}

#[test]
fn array_order_stays_significant() {
    let schema = compile(r#"{"type":"object"}"#);
    let forward = normalized(&schema, r#"{"v":[1,2,{"b":1,"a":2}]}"#);
    assert_eq!(forward.as_str(), r#"{"v":[1,2,{"a":2,"b":1}]}"#);
    let reversed = normalized(&schema, r#"{"v":[{"a":2,"b":1},2,1]}"#);
    assert_ne!(forward, reversed);
    assert_eq!(reversed.as_str(), r#"{"v":[{"a":2,"b":1},2,1]}"#);
    // Object key order inside array elements is still normalized.
    assert_eq!(
        normalized(&schema, r#"{"v":[{"b":1,"a":2}]}"#),
        normalized(&schema, r#"{"v":[{"a":2,"b":1}]}"#)
    );
}

#[test]
fn scalar_forms_round_trip_without_numeric_normalization() {
    let schema = compile(r#"{"type":"object"}"#);
    let args = r#"{"a":1.0,"b":-0.0,"c":0,"d":0.0,"e":-0,"f":1E2,"g":2.5e1,"h":1e-2,"i":9007199254740993,"j":18446744073709551615,"k":-9223372036854775808,"l":true,"m":null}"#;
    assert_eq!(
        canonical(&schema, args),
        r#"{"a":1.0,"b":-0.0,"c":0,"d":0.0,"e":-0.0,"f":100.0,"g":25.0,"h":0.01,"i":9007199254740993,"j":18446744073709551615,"k":-9223372036854775808,"l":true,"m":null}"#
    );
    // An integer literal is never turned into a float literal, and vice
    // versa; the 2^53 + 1 literal above stayed exact while `1` and `1.0`
    // remain distinct canonical texts.
    assert_eq!(canonical(&schema, r#"{"v":1}"#), r#"{"v":1}"#);
    assert_eq!(canonical(&schema, r#"{"v":1.0}"#), r#"{"v":1.0}"#);
    assert_ne!(
        canonical(&schema, r#"{"v":1.0}"#),
        canonical(&schema, r#"{"v":1}"#)
    );
    // Exponent spellings parse to one float and re-serialize to one text.
    for form in [r#"{"v":1e2}"#, r#"{"v":1E2}"#, r#"{"v":1.0e2}"#] {
        assert_eq!(canonical(&schema, form), r#"{"v":100.0}"#, "{form}");
    }
}

#[test]
fn zero_forms_stay_distinct() {
    let schema = compile(r#"{"type":"object"}"#);
    let integer_zero = canonical(&schema, r#"{"v":0}"#);
    let float_zero = canonical(&schema, r#"{"v":0.0}"#);
    let negative_float_zero = canonical(&schema, r#"{"v":-0.0}"#);
    // serde_json parses `-0` as the float -0.0, not as integer zero.
    let negative_integer_zero = canonical(&schema, r#"{"v":-0}"#);
    assert_eq!(integer_zero, r#"{"v":0}"#);
    assert_eq!(float_zero, r#"{"v":0.0}"#);
    assert_eq!(negative_float_zero, r#"{"v":-0.0}"#);
    assert_eq!(negative_integer_zero, r#"{"v":-0.0}"#);
    assert_ne!(integer_zero, float_zero);
    assert_ne!(integer_zero, negative_float_zero);
    assert_ne!(float_zero, negative_float_zero);
}

#[test]
fn string_escape_forms_canonicalize_to_one_text() {
    let schema = compile(r#"{"type":"object"}"#);
    let cases = [
        (r#"{"v":"\u00e9"}"#, r#"{"v":"é"}"#),
        (r#"{"v":"\ud83d\ude00"}"#, r#"{"v":"😀"}"#),
        (r#"{"v":"\/"}"#, r#"{"v":"/"}"#),
        (r#"{"v":"\n"}"#, r#"{"v":"\n"}"#),
        (r#"{"v":"\t"}"#, r#"{"v":"\t"}"#),
        (r#"{"v":"\u0001"}"#, r#"{"v":"\u0001"}"#),
        (r#"{"v":"\\"}"#, r#"{"v":"\\"}"#),
        (r#"{"v":"\""}"#, r#"{"v":"\""}"#),
        (r#"{"v":" a b "}"#, r#"{"v":" a b "}"#),
    ];
    for (args, expected) in cases {
        assert_eq!(canonical(&schema, args), expected, "{args}");
    }
    // Escaped and literal spellings of the same text are equivalent.
    assert_eq!(
        normalized(&schema, r#"{"v":"\u00e9"}"#),
        normalized(&schema, r#"{"v":"é"}"#)
    );
}

#[test]
fn duplicate_argument_keys_are_rejected_at_every_depth() {
    let schema = compile(r#"{"type":"object"}"#);
    let cases = [
        // Root level, same and differing values, differing types.
        r#"{"a":1,"a":2}"#,
        r#"{"a":1,"a":1}"#,
        r#"{"a":1,"a":"1"}"#,
        // Escape-equivalent spellings collide after decoding.
        r#"{"a":1,"\u0061":2}"#,
        // Nested object, doubly nested object, and inside array elements.
        r#"{"outer":{"a":1,"a":2}}"#,
        r#"{"outer":{"inner":{"a":1,"a":2}}}"#,
        r#"{"v":[{"a":1,"a":2}]}"#,
        r#"{"v":[1,{"a":1,"a":2}]}"#,
        // Whitespace does not hide a duplicate.
        r#"{ "a" : 1 , "a" : 2 }"#,
    ];
    for args in cases {
        let error = schema.validate(args, MAX_ARGS_BYTES).unwrap_err();
        assert_invalid(&error, "duplicate");
        assert!(parse_object(args, MAX_ARGS_BYTES).is_err(), "{args}");
    }
}

#[test]
fn duplicate_schema_keys_are_rejected() {
    for schema in [
        r#"{"type":"object","type":"object"}"#,
        r#"{"type":"object","properties":{"a":{"type":"null"},"a":{"type":"null"}}}"#,
        r#"{"type":"object","required":["a","a"]}"#,
    ] {
        let error = CompiledSchema::compile(schema).unwrap_err();
        assert_invalid(&error, "duplicate");
    }
}

#[test]
fn canonical_output_is_duplicate_free_and_round_trips() {
    let schema = compile(r#"{"type":"object"}"#);
    let args = r#"{"c":3,"a":1,"b":{"z":true,"y":null,"x":[]},"d":[{"n":1,"m":2},{}]}"#;
    let output = canonical(&schema, args);
    assert_eq!(
        output,
        r#"{"a":1,"b":{"x":[],"y":null,"z":true},"c":3,"d":[{"m":2,"n":1},{}]}"#
    );
    // Each declared key appears exactly once in the canonical text.
    for key in ["a", "b", "c", "d", "x", "y", "z", "m", "n"] {
        let needle = format!("\"{key}\":");
        assert_eq!(
            output.matches(&needle).count(),
            1,
            "key {key:?} should appear once in {output}"
        );
    }
    // The strict parser rejects duplicates at every depth, so a successful
    // reparse of the canonical text proves the output has none. It also
    // proves canonicalization preserved every key and value.
    let reparsed =
        parse_object(&output, output.len()).expect("canonical output is strictly parseable");
    let original = parse_object(args, MAX_ARGS_BYTES).expect("original parses");
    assert_eq!(reparsed, original);
}

#[test]
fn canonicalization_is_idempotent() {
    let schema = compile(
        r#"{
            "type":"object",
            "properties":{
                "path":{"type":"string","minLength":1},
                "tags":{"type":"array","items":{"type":"object","properties":{"k":{"type":"string"}},"required":["k"],"additionalProperties":false}},
                "opts":{"type":"object","additionalProperties":true}
            },
            "required":["path"],
            "additionalProperties":false
        }"#,
    );
    let args = r#"{ "tags" : [ { "k" : "a" } , { "k" : "b" } ] , "path" : "src" , "opts" : { "z" : 1 , "a" : [ true , null ] } }"#;
    let first = normalized(&schema, args);
    let second = schema
        .validate(first.as_str(), first.as_str().len())
        .expect("canonical text revalidates at its own byte length");
    assert_eq!(first, second);
    let third = schema
        .validate(second.as_str(), MAX_ARGS_BYTES)
        .expect("canonical text revalidates again");
    assert_eq!(first, third);
    assert_eq!(
        first.as_str(),
        r#"{"opts":{"a":[true,null],"z":1},"path":"src","tags":[{"k":"a"},{"k":"b"}]}"#
    );
}

#[test]
fn compiled_schema_is_reusable_and_pure_across_calls() {
    let schema =
        compile(r#"{"type":"object","properties":{"v":{"type":"string"}},"required":["v"]}"#);
    let expected = r#"{"v":"a"}"#;
    for _ in 0..4 {
        assert_eq!(canonical(&schema, r#"{ "v" : "a" }"#), expected);
        let error = schema.validate(r#"{"v":1}"#, MAX_ARGS_BYTES).unwrap_err();
        assert_invalid(&error, "type");
        assert_eq!(canonical(&schema, r#"{"v":"a"}"#), expected);
    }
    // A clone is equal and validates identically.
    let clone = schema.clone();
    assert_eq!(clone, schema);
    assert_eq!(
        clone.validate(r#"{"v":"a"}"#, MAX_ARGS_BYTES).unwrap(),
        schema.validate(r#"{"v":"a"}"#, MAX_ARGS_BYTES).unwrap()
    );
    // The caller's byte budget changes acceptance, never the canonical text.
    let compact = r#"{"v":"a"}"#;
    let tight = schema
        .validate(compact, compact.len())
        .expect("exact byte budget passes");
    let loose = schema
        .validate(compact, MAX_ARGS_BYTES)
        .expect("hard maximum passes");
    assert_eq!(tight, loose);
    assert_eq!(tight.as_str(), expected);
    // Repeated rejections leave later successes unchanged.
    for _ in 0..4 {
        assert!(schema.validate(r#"{"v":1}"#, MAX_ARGS_BYTES).is_err());
    }
    assert_eq!(canonical(&schema, compact), expected);
}

#[test]
fn compiled_schema_is_pure_across_threads() {
    let schema =
        compile(r#"{"type":"object","properties":{"v":{"type":"string"}},"required":["v"]}"#);
    let expected = r#"{"v":"a"}"#;
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..8 {
            handles.push(scope.spawn(|| {
                for _ in 0..64 {
                    let output = schema
                        .validate(r#"{ "v" : "a" }"#, MAX_ARGS_BYTES)
                        .expect("valid");
                    assert_eq!(output.as_str(), expected);
                    assert!(schema.validate(r#"{"v":1}"#, MAX_ARGS_BYTES).is_err());
                }
                expected
            }));
        }
        for handle in handles {
            assert_eq!(handle.join().expect("worker thread completes"), expected);
        }
    });
}

#[test]
fn identical_schema_text_compiles_to_equal_schemas() {
    let text = r#"{"type":"object","properties":{"v":{"type":"string"}},"required":["v"]}"#;
    let first = compile(text);
    let second = compile(text);
    assert_eq!(first, second);
    assert_eq!(
        first.validate(r#"{"v":"a"}"#, MAX_ARGS_BYTES).unwrap(),
        second.validate(r#"{"v":"a"}"#, MAX_ARGS_BYTES).unwrap()
    );
}
