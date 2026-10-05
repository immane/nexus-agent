//! Coverage hardening for [`nexus_validation::parse_object`].
//!
//! These tests drive the public boundary only and use fixed literals, so the
//! suite is deterministic. They pin four behaviors:
//!
//! - duplicate object keys are rejected at every level (never last-wins),
//!   including duplicates written with an escape sequence and duplicates
//!   whose values are identical;
//! - non-object roots are rejected with a root-specific error that is
//!   distinct from a syntax failure;
//! - malformed text and trailing content are rejected, while surrounding
//!   whitespace is not treated as trailing content;
//! - well-formed values are returned as plain parsed values (no
//!   canonicalization) within the byte, depth, and node budgets.
//!
//! Budget rejection boundaries themselves are pinned by
//! `cov_parse_budgets.rs`; this file asserts the accepted side of those
//! budgets and the exact error taxonomy of the parse failures it exercises.

#![forbid(unsafe_code)]

use nexus_core::{AgentError, ErrorCategory, Limits, RetryGuidance};
use nexus_validation::serde_json::{self, Value};
use nexus_validation::{MAX_ARGS_DEPTH, MAX_ARGS_NODES, parse_object};

/// The hard maximum argument byte budget: the core M0 assembly budget.
const MAX_ARG_BYTES: usize = Limits::M0_TEST_ARG_ASSEMBLY_BYTES;

fn parse(args: &str) -> Value {
    parse_object(args, MAX_ARG_BYTES).expect("arguments parse")
}

fn parse_error(args: &str) -> AgentError {
    parse_object(args, MAX_ARG_BYTES).expect_err("arguments must be rejected")
}

fn assert_invalid(error: &AgentError, needle: &str) {
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

#[test]
fn duplicate_keys_are_rejected_never_last_wins() {
    for args in [
        // The second value must not overwrite the first.
        r#"{"a":1,"a":2}"#,
        r#"{"a":2,"a":1}"#,
        // Identical values are still a duplicate, not a harmless repetition.
        r#"{"a":1,"a":1}"#,
        r#"{"a":1,"a":2,"a":3}"#,
        r#"{ "a" : 1 , "a" : 2 }"#,
        // Duplicates nested in objects, deep chains, and array items reject
        // the whole document.
        r#"{"outer":{"x":1,"x":2}}"#,
        r#"{"a":{"b":{"c":1,"c":2}}}"#,
        r#"{"list":[{"k":1,"k":2}]}"#,
        // Key equality is by decoded text: `\u0061` is `a`.
        r#"{"a":1,"\u0061":2}"#,
        r#"{"\u0061":1,"a":2}"#,
        // The empty key is a key like any other.
        r#"{"":1,"":2}"#,
    ] {
        let error = parse_error(args);
        assert_invalid(&error, "duplicate");
    }
}

#[test]
fn duplicate_detection_is_scoped_to_one_object_level() {
    // The same key may appear in sibling objects and in sibling array items;
    // only one map level is checked at a time.
    let value = parse(r#"{"a":{"x":1},"b":{"x":2},"c":[{"k":1},{"k":2}],"x":3}"#);
    assert_eq!(
        value,
        serde_json::json!({"a":{"x":1},"b":{"x":2},"c":[{"k":1},{"k":2}],"x":3})
    );
}

#[test]
fn case_sensitive_and_padded_keys_are_distinct() {
    let value = parse(r#"{"a":1,"A":2,"a ":3," a":4,"":5}"#);
    assert_eq!(value, serde_json::json!({"a":1,"A":2,"a ":3," a":4,"":5}));
}

#[test]
fn non_object_roots_are_rejected_with_a_root_specific_error() {
    for args in [
        "[]", "[1,2]", "[{}]", "\"text\"", "\"\"", "\"{}[]\"", "1", "-1", "0", "1.5", "-0.0",
        "1e3", "true", "false", "null",
    ] {
        let error = parse_error(args);
        // A parsed non-object is a shape failure, not a syntax failure.
        assert_eq!(error.category(), ErrorCategory::InvalidInput, "{args:?}");
        assert_eq!(error.message(), "arguments must be an object", "{args:?}");
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    }
}

#[test]
fn non_object_values_are_allowed_inside_an_object_root() {
    let value = parse(r#"{"a":[],"b":null,"c":1,"d":false,"e":"x","f":{}}"#);
    assert!(value.is_object());
    assert_eq!(value["a"].as_array().map(Vec::len), Some(0));
    assert_eq!(value["c"].as_i64(), Some(1));
}

#[test]
fn malformed_text_is_rejected_as_invalid_json() {
    for args in [
        "",
        "   ",
        "\n\t ",
        "{",
        "}",
        "[",
        "{\"a\"}",
        "{\"a\":}",
        "{\"a\":1,}",
        "{,}",
        "{\"a\",}",
        "{\"a\" 1}",
        "{\"a\"::1}",
        "{\"a\":01}",
        "{\"a\":+1}",
        "{\"a\":.5}",
        "{\"a\":1.}",
        "{\"a\":1e}",
        "{\"a\":1e999}",
        "{\"a\":NaN}",
        "{\"a\":Infinity}",
        "{\"a\":-Infinity}",
        "{\"a\":undefined}",
        "{\"a\":\"unterminated}",
        "{\"a\":\"bad\\qescape\"}",
        "{\"a\":\"raw\ncontrol\"}",
        "{\"a\":\"\\ud800\"}",
        "{'a':1}",
        "{a:1}",
        "nul",
        "tru",
        "fals",
    ] {
        let error = parse_error(args);
        assert_invalid(&error, "not valid JSON");
    }
}

#[test]
fn trailing_content_after_a_valid_value_is_rejected() {
    for args in [
        r#"{"a":1} x"#,
        r#"{"a":1}{}"#,
        r#"{"a":1} []"#,
        r#"{"a":1},"#,
        r#"{"a":1}]"#,
        r#"{"a":1}}"#,
        r#"{"a":1}0"#,
        r#"{} true"#,
        "{\"a\":1}\n{\"b\":2}",
    ] {
        let error = parse_error(args);
        assert_invalid(&error, "not valid JSON");
    }
}

#[test]
fn surrounding_whitespace_is_not_trailing_content() {
    for args in [" {} ", "\n{\"a\":1}\t", "  {\"a\":1}  ", "\r\n{}\r\n"] {
        let value = parse(args);
        assert!(value.is_object(), "{args:?}");
    }
}

#[test]
fn plain_values_are_returned_without_canonicalization() {
    let value = parse(r#"{"z":1,"a":[1,2.5,"x",true,null,{}],"b":{"c":false},"u":"\u00e9"}"#);
    assert_eq!(
        value,
        serde_json::json!({"z":1,"a":[1,2.5,"x",true,null,{}],"b":{"c":false},"u":"é"})
    );
    assert_eq!(value["a"].as_array().map(Vec::len), Some(6));
    assert_eq!(value["a"][3].as_bool(), Some(true));
    assert_eq!(value["b"]["c"].as_bool(), Some(false));
    assert_eq!(value["u"].as_str(), Some("é"));
}

#[test]
fn scalar_forms_are_preserved() {
    let value = parse(
        r#"{"i":1,"n":-1,"f":1.0,"neg":-0.0,"big":9007199254740993,"exp":1e2,"u":"\u00e9","e":""}"#,
    );
    assert!(value["i"].is_i64());
    assert_eq!(value["i"].as_i64(), Some(1));
    assert_eq!(value["n"].as_i64(), Some(-1));
    assert!(value["f"].is_f64());
    assert_eq!(value["f"].as_f64(), Some(1.0));
    // `1.0` stays a float; plain parsing does not fold it to an integer.
    assert_eq!(value["f"].as_i64(), None);
    assert!(value["neg"].is_f64());
    assert!(value["neg"].as_f64().expect("float").is_sign_negative());
    assert!(value["big"].is_u64());
    assert_eq!(value["big"].as_u64(), Some(9_007_199_254_740_993));
    assert_eq!(value["exp"].as_f64(), Some(100.0));
    assert_eq!(value["u"].as_str(), Some("é"));
    assert_eq!(value["e"].as_str(), Some(""));
}

#[test]
fn plain_values_parse_at_the_exact_byte_budget() {
    let args = r#"{"é":"é"}"#;
    assert_eq!(args.chars().count(), 9);
    assert_eq!(args.len(), 11);
    let value = parse_object(args, args.len()).expect("byte-exact budget parses");
    assert_eq!(value["é"].as_str(), Some("é"));
    // The budget counts UTF-8 bytes, not characters.
    let error = parse_object(args, args.chars().count()).unwrap_err();
    assert_resource_limit(&error, "byte");
}

#[test]
fn plain_values_parse_at_the_exact_depth_budget() {
    let at_budget = format!(
        r#"{{"a":{}{}}}"#,
        "[".repeat(MAX_ARGS_DEPTH),
        "]".repeat(MAX_ARGS_DEPTH)
    );
    let value = parse(&at_budget);
    let mut innermost = &value["a"];
    for _ in 1..MAX_ARGS_DEPTH {
        innermost = &innermost[0];
    }
    assert_eq!(innermost.as_array().map(Vec::len), Some(0));
}

#[test]
fn plain_values_parse_at_the_exact_node_budget() {
    let elements = MAX_ARGS_NODES - 2;
    let at_budget = format!(r#"{{"a":[{}]}}"#, vec!["0"; elements].join(","));
    let value = parse(&at_budget);
    assert_eq!(value["a"].as_array().map(Vec::len), Some(elements));
}

#[test]
fn shape_and_syntax_failures_at_the_hard_maximum_stay_invalid_input() {
    // A generous byte budget never turns a duplicate, a non-object root, or
    // malformed text into a resource-limit failure.
    for args in [r#"{"a":1,"a":2}"#, "[]", "null", "{not json"] {
        let error = parse_object(args, MAX_ARG_BYTES).unwrap_err();
        assert_eq!(error.category(), ErrorCategory::InvalidInput, "{args:?}");
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    }
}
