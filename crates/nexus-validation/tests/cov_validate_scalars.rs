//! Coverage hardening for scalar validation: string bounds measured in
//! Unicode code points, inclusive number bounds, exact comparison of large
//! integers, boolean and null acceptance, and scalar type mismatches.
//!
//! Every assertion goes through the public boundary and uses fixed literals,
//! so the suite is deterministic: no clock, randomness, environment, or
//! external I/O is involved.

#![forbid(unsafe_code)]

use nexus_core::{AgentError, ErrorCategory, Limits, RetryGuidance};
use nexus_validation::CompiledSchema;

const MAX_ARGS_BYTES: usize = Limits::M0_TEST_ARG_ASSEMBLY_BYTES;

fn compile(schema: &str) -> CompiledSchema {
    CompiledSchema::compile(schema).expect("schema compiles")
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

fn accepts(schema: &CompiledSchema, args: &str) {
    schema
        .validate(args, MAX_ARGS_BYTES)
        .unwrap_or_else(|error| panic!("{args} should validate: {error:?}"));
}

fn rejects(schema: &CompiledSchema, args: &str, needle: &str) {
    assert_invalid(&schema.validate(args, MAX_ARGS_BYTES).unwrap_err(), needle);
}

#[test]
fn string_min_length_counts_code_points() {
    let schema = compile(
        r#"{"type":"object","properties":{"v":{"type":"string","minLength":2}},"required":["v"]}"#,
    );
    for text in ["ab", "éé", "🦀🦀", "e\u{301}", "🦀a"] {
        accepts(&schema, &format!(r#"{{"v":"{text}"}}"#));
    }
    for text in ["", "a", "é", "🦀", "\u{301}"] {
        rejects(&schema, &format!(r#"{{"v":"{text}"}}"#), "minimum length");
    }
}

#[test]
fn string_max_length_counts_code_points_not_bytes() {
    // "🦀🦀" is 8 UTF-8 bytes but only 2 code points; byte counting would
    // reject it against maxLength 2.
    assert_eq!("🦀🦀".len(), 8);
    assert_eq!("🦀🦀".chars().count(), 2);
    let schema = compile(r#"{"type":"object","properties":{"v":{"type":"string","maxLength":2}}}"#);
    for text in ["", "a", "éé", "🦀🦀", "e\u{301}", "🦀a"] {
        accepts(&schema, &format!(r#"{{"v":"{text}"}}"#));
    }
    for text in ["abc", "ééé", "🦀🦀🦀", "e\u{301}\u{301}"] {
        rejects(&schema, &format!(r#"{{"v":"{text}"}}"#), "maximum length");
    }
}

#[test]
fn string_exact_length_window_and_zero_bounds() {
    let exact = compile(
        r#"{"type":"object","properties":{"v":{"type":"string","minLength":2,"maxLength":2}}}"#,
    );
    accepts(&exact, r#"{"v":"🦀🦀"}"#);
    accepts(&exact, r#"{"v":"e\u0301"}"#);
    rejects(&exact, r#"{"v":"🦀"}"#, "minimum length");
    rejects(&exact, r#"{"v":"🦀🦀🦀"}"#, "maximum length");

    let empty_only = compile(
        r#"{"type":"object","properties":{"v":{"type":"string","minLength":0,"maxLength":0}}}"#,
    );
    accepts(&empty_only, r#"{"v":""}"#);
    rejects(&empty_only, r#"{"v":"a"}"#, "maximum length");
    rejects(&empty_only, r#"{"v":"🦀"}"#, "maximum length");
}

#[test]
fn string_without_bounds_accepts_any_code_point_count() {
    let schema = compile(r#"{"type":"object","properties":{"v":{"type":"string"}}}"#);
    let long = "x".repeat(4_096);
    for text in ["", "a", "🦀🦀🦀", long.as_str()] {
        accepts(&schema, &format!(r#"{{"v":"{text}"}}"#));
    }
}

#[test]
fn number_bounds_are_inclusive_at_both_ends() {
    let schema = compile(
        r#"{"type":"object","properties":{"v":{"type":"number","minimum":0,"maximum":10}}}"#,
    );
    for args in [
        r#"{"v":0}"#,
        r#"{"v":0.0}"#,
        r#"{"v":-0.0}"#,
        r#"{"v":5.5}"#,
        r#"{"v":10}"#,
        r#"{"v":10.0}"#,
    ] {
        accepts(&schema, args);
    }
    for args in [r#"{"v":-1}"#, r#"{"v":-0.0001}"#] {
        rejects(&schema, args, "minimum");
    }
    for args in [r#"{"v":10.0001}"#, r#"{"v":11}"#] {
        rejects(&schema, args, "maximum");
    }
}

#[test]
fn fractional_and_negative_bounds_are_inclusive() {
    let fraction = compile(
        r#"{"type":"object","properties":{"v":{"type":"number","minimum":0.5,"maximum":1.5}}}"#,
    );
    for args in [r#"{"v":0.5}"#, r#"{"v":0.75}"#, r#"{"v":1.5}"#] {
        accepts(&fraction, args);
    }
    rejects(&fraction, r#"{"v":0.4999}"#, "minimum");
    rejects(&fraction, r#"{"v":1.5001}"#, "maximum");

    let negative = compile(
        r#"{"type":"object","properties":{"v":{"type":"number","minimum":-5,"maximum":-1}}}"#,
    );
    for args in [r#"{"v":-5}"#, r#"{"v":-3.25}"#, r#"{"v":-1}"#] {
        accepts(&negative, args);
    }
    rejects(&negative, r#"{"v":-5.0001}"#, "minimum");
    rejects(&negative, r#"{"v":-0.9999}"#, "maximum");
}

#[test]
fn one_sided_bounds_are_enforced_in_the_open_direction() {
    let min_only =
        compile(r#"{"type":"object","properties":{"v":{"type":"number","minimum":-5}}}"#);
    accepts(&min_only, r#"{"v":-5}"#);
    accepts(&min_only, r#"{"v":1000000}"#);
    rejects(&min_only, r#"{"v":-5.0001}"#, "minimum");

    let max_only =
        compile(r#"{"type":"object","properties":{"v":{"type":"number","maximum":-1}}}"#);
    accepts(&max_only, r#"{"v":-1}"#);
    accepts(&max_only, r#"{"v":-1000000}"#);
    rejects(&max_only, r#"{"v":-0.9999}"#, "maximum");
}

#[test]
fn mixed_integer_and_float_forms_compare_at_bounds() {
    let float_bound = compile(
        r#"{"type":"object","properties":{"v":{"type":"number","minimum":1.0,"maximum":2.0}}}"#,
    );
    accepts(&float_bound, r#"{"v":1}"#);
    accepts(&float_bound, r#"{"v":2}"#);
    rejects(&float_bound, r#"{"v":0}"#, "minimum");
    rejects(&float_bound, r#"{"v":3}"#, "maximum");

    let int_bound = compile(
        r#"{"type":"object","properties":{"v":{"type":"number","minimum":1,"maximum":2}}}"#,
    );
    accepts(&int_bound, r#"{"v":1.0}"#);
    accepts(&int_bound, r#"{"v":2.0}"#);
    rejects(&int_bound, r#"{"v":0.5}"#, "minimum");
    rejects(&int_bound, r#"{"v":2.5}"#, "maximum");
}

#[test]
fn large_integer_bounds_compare_exactly_above_f64_exact_range() {
    let schema = compile(
        r#"{"type":"object","properties":{"v":{"type":"number","minimum":9007199254740993,"maximum":9007199254740995}}}"#,
    );
    for args in [
        r#"{"v":9007199254740993}"#,
        r#"{"v":9007199254740994}"#,
        r#"{"v":9007199254740995}"#,
        r#"{"v":9007199254740994.0}"#,
    ] {
        accepts(&schema, args);
    }
    rejects(&schema, r#"{"v":9007199254740992}"#, "minimum");
    rejects(&schema, r#"{"v":9007199254740996}"#, "maximum");
    // The float literal rounds down to 9007199254740992.0, below the exact
    // integer minimum; a naive f64 comparison would admit it.
    rejects(&schema, r#"{"v":9007199254740993.0}"#, "minimum");
}

#[test]
fn integer_bounds_at_signed_and_unsigned_extremes_compare_exactly() {
    let wide = compile(
        r#"{"type":"object","properties":{"v":{"type":"number","minimum":-9223372036854775807,"maximum":18446744073709551614}}}"#,
    );
    accepts(&wide, r#"{"v":-9223372036854775807}"#);
    accepts(&wide, r#"{"v":0}"#);
    accepts(&wide, r#"{"v":18446744073709551614}"#);
    rejects(&wide, r#"{"v":-9223372036854775808}"#, "minimum");
    rejects(&wide, r#"{"v":18446744073709551615}"#, "maximum");
    // u64::MAX + 1 is parsed as the f64 value 2^64, which exceeds the bound.
    rejects(&wide, r#"{"v":18446744073709551616}"#, "maximum");
}

#[test]
fn boolean_schema_accepts_only_booleans() {
    let schema = compile(r#"{"type":"object","properties":{"v":{"type":"boolean"}}}"#);
    for args in [r#"{"v":true}"#, r#"{"v":false}"#] {
        accepts(&schema, args);
    }
    for args in [
        r#"{"v":0}"#,
        r#"{"v":1}"#,
        r#"{"v":"true"}"#,
        r#"{"v":null}"#,
        r#"{"v":{}}"#,
        r#"{"v":[]}"#,
    ] {
        rejects(&schema, args, "type");
    }
}

#[test]
fn null_schema_accepts_only_null() {
    let schema = compile(r#"{"type":"object","properties":{"v":{"type":"null"}}}"#);
    accepts(&schema, r#"{"v":null}"#);
    for args in [
        r#"{"v":false}"#,
        r#"{"v":0}"#,
        r#"{"v":""}"#,
        r#"{"v":{}}"#,
        r#"{"v":[]}"#,
    ] {
        rejects(&schema, args, "type");
    }
}

#[test]
fn scalar_type_mismatch_matrix_is_exhaustive() {
    let cases: &[(&str, &[&str], &[&str])] = &[
        (
            "string",
            &[r#""x""#, r#""""#],
            &["0", "-1", "1.5", "true", "false", "null", "{}", "[]"],
        ),
        (
            "number",
            &["0", "-1", "1.5", "1e2"],
            &[r#""1""#, "true", "false", "null", "{}", "[]"],
        ),
        (
            "boolean",
            &["true", "false"],
            &["0", "1", r#""true""#, "null", "{}", "[]"],
        ),
        (
            "null",
            &["null"],
            &["0", r#""null""#, "true", "false", "{}", "[]"],
        ),
    ];
    for (kind, accepted, rejected) in cases {
        let schema = compile(&format!(
            r#"{{"type":"object","properties":{{"v":{{"type":"{kind}"}}}}}}"#
        ));
        for value in *accepted {
            accepts(&schema, &format!(r#"{{"v":{value}}}"#));
        }
        for value in *rejected {
            rejects(&schema, &format!(r#"{{"v":{value}}}"#), "type");
        }
    }
}

#[test]
fn scalar_items_inside_arrays_are_validated() {
    let booleans = compile(
        r#"{"type":"object","properties":{"v":{"type":"array","items":{"type":"boolean"},"minItems":1,"maxItems":2}}}"#,
    );
    accepts(&booleans, r#"{"v":[true,false]}"#);
    rejects(&booleans, r#"{"v":[true,null]}"#, "type");
    rejects(&booleans, r#"{"v":[]}"#, "size bound");

    let nulls =
        compile(r#"{"type":"object","properties":{"v":{"type":"array","items":{"type":"null"}}}}"#);
    accepts(&nulls, r#"{"v":[null,null]}"#);
    rejects(&nulls, r#"{"v":[null,0]}"#, "type");

    let strings = compile(
        r#"{"type":"object","properties":{"v":{"type":"array","items":{"type":"string","maxLength":2}}}}"#,
    );
    accepts(&strings, r#"{"v":["🦀🦀","e\u0301"]}"#);
    rejects(&strings, r#"{"v":["🦀🦀🦀"]}"#, "maximum length");
    rejects(&strings, r#"{"v":["ok",1]}"#, "type");
}

#[test]
fn canonicalization_preserves_scalar_json_types() {
    let schema = compile(
        r#"{"type":"object","properties":{"b":{"type":"boolean"},"n":{"type":"null"},"i":{"type":"number"},"f":{"type":"number"},"s":{"type":"string"}}}"#,
    );
    let normalized = schema
        .validate(
            r#"{"s":"é","f":1.0,"i":-7,"n":null,"b":true}"#,
            MAX_ARGS_BYTES,
        )
        .expect("valid");
    assert_eq!(
        normalized.as_str(),
        r#"{"b":true,"f":1.0,"i":-7,"n":null,"s":"é"}"#
    );
}
