//! Value validation against a compiled schema, plus canonicalization.
//!
//! Validation is a pure comparison against the immutable compiled form. On
//! success the value is rebuilt with every object level sorted by key and
//! serialized compactly, so equivalent argument texts map to one
//! [`NormalizedArgs`] value.

use std::cmp::Ordering;

use nexus_core::{AgentError, NormalizedArgs};
use serde_json::{Map, Value};

use crate::schema::{
    ArraySchema, NodeSchema, NumberSchema, ObjectSchema, StringSchema, compare_numbers,
};
use crate::{check_arg_budget, input_error, limit_error, parse_object};

impl crate::CompiledSchema {
    /// Validates object-root argument text against this schema and returns
    /// canonical immutable arguments.
    ///
    /// `max_bytes` is the caller's argument byte budget and must lie in
    /// `1..=nexus_core::Limits::M0_TEST_ARG_ASSEMBLY_BYTES`. The core
    /// assembly budget is a hard maximum: an out-of-range value is invalid
    /// configuration and is rejected with `ErrorCategory::InvalidInput`,
    /// never silently clamped. Malformed text, duplicate keys, non-object
    /// roots, and schema mismatches also fail with
    /// `ErrorCategory::InvalidInput`; byte, depth, and node exhaustion fail
    /// with `ErrorCategory::ResourceLimit`. The returned [`NormalizedArgs`]
    /// text is compact and recursively key-sorted so it can be compared and
    /// bound exactly.
    pub fn validate(&self, args: &str, max_bytes: usize) -> Result<NormalizedArgs, AgentError> {
        check_arg_budget(max_bytes)?;
        let value = parse_object(args, max_bytes)?;
        let Value::Object(object) = &value else {
            return Err(input_error("arguments must be an object"));
        };
        validate_object(&self.root, object)?;
        let canonical = canonical_value(value);
        let serialized = serde_json::to_string(&canonical)
            .map_err(|_| input_error("arguments could not be serialized"))?;
        if serialized.len() > max_bytes {
            return Err(limit_error("argument byte budget exhausted"));
        }
        NormalizedArgs::new(serialized)
    }
}

fn validate_object(schema: &ObjectSchema, object: &Map<String, Value>) -> Result<(), AgentError> {
    let count = object.len() as u64;
    if let Some(min) = schema.min_properties
        && count < min
    {
        return Err(input_error("argument object is below its property bound"));
    }
    if let Some(max) = schema.max_properties
        && count > max
    {
        return Err(input_error("argument object exceeds its property bound"));
    }
    for required in &schema.required {
        if !object.contains_key(required) {
            return Err(input_error("argument is missing a required property"));
        }
    }
    for key in object.keys() {
        if !schema.properties.contains_key(key) && !schema.additional_properties {
            return Err(input_error("argument has an undeclared property"));
        }
    }
    for (name, child) in &schema.properties {
        if let Some(value) = object.get(name) {
            validate_node(child, value)?;
        }
    }
    Ok(())
}

fn validate_node(schema: &NodeSchema, value: &Value) -> Result<(), AgentError> {
    match schema {
        NodeSchema::Object(object) => match value.as_object() {
            Some(value) => validate_object(object, value),
            None => Err(type_mismatch()),
        },
        NodeSchema::Array(array) => match value.as_array() {
            Some(items) => validate_array(array, items),
            None => Err(type_mismatch()),
        },
        NodeSchema::String(string) => match value.as_str() {
            Some(text) => validate_string(string, text),
            None => Err(type_mismatch()),
        },
        NodeSchema::Number(number) => match value.as_number() {
            Some(value) => validate_number(number, value),
            None => Err(type_mismatch()),
        },
        NodeSchema::Boolean => {
            if value.is_boolean() {
                Ok(())
            } else {
                Err(type_mismatch())
            }
        }
        NodeSchema::Null => {
            if value.is_null() {
                Ok(())
            } else {
                Err(type_mismatch())
            }
        }
    }
}

fn validate_array(schema: &ArraySchema, items: &[Value]) -> Result<(), AgentError> {
    let count = items.len() as u64;
    if let Some(min) = schema.min_items
        && count < min
    {
        return Err(input_error("argument array is below its size bound"));
    }
    if let Some(max) = schema.max_items
        && count > max
    {
        return Err(input_error("argument array exceeds its size bound"));
    }
    for item in items {
        validate_node(&schema.items, item)?;
    }
    Ok(())
}

fn validate_string(schema: &StringSchema, text: &str) -> Result<(), AgentError> {
    // JSON Schema length semantics: Unicode code points, not bytes.
    let length = text.chars().count() as u64;
    if let Some(min) = schema.min_length
        && length < min
    {
        return Err(input_error("argument string is below its minimum length"));
    }
    if let Some(max) = schema.max_length
        && length > max
    {
        return Err(input_error("argument string exceeds its maximum length"));
    }
    Ok(())
}

fn validate_number(schema: &NumberSchema, number: &serde_json::Number) -> Result<(), AgentError> {
    if let Some(minimum) = &schema.minimum {
        match compare_numbers(number, minimum) {
            Some(Ordering::Less) => {
                return Err(input_error("argument number is below its minimum"));
            }
            None => {
                return Err(input_error("argument number comparison is inconclusive"));
            }
            _ => {}
        }
    }
    if let Some(maximum) = &schema.maximum {
        match compare_numbers(number, maximum) {
            Some(Ordering::Greater) => {
                return Err(input_error("argument number exceeds its maximum"));
            }
            None => {
                return Err(input_error("argument number comparison is inconclusive"));
            }
            _ => {}
        }
    }
    Ok(())
}

fn type_mismatch() -> AgentError {
    input_error("argument type does not match the schema")
}

/// Rebuilds `value` with every object level sorted by UTF-8 byte order.
///
/// Sorting in this crate means the result does not depend on serde_json's
/// `preserve_order` feature being off: insertion in sorted order stays
/// sorted under either map backing.
fn canonical_value(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(canonical_value).collect()),
        Value::Object(object) => {
            let mut entries: Vec<(String, Value)> = object.into_iter().collect();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            let mut sorted = Map::new();
            for (key, value) in entries {
                sorted.insert(key, canonical_value(value));
            }
            Value::Object(sorted)
        }
        scalar => scalar,
    }
}

#[cfg(test)]
mod cov_validate_private {
    use std::collections::{BTreeMap, BTreeSet};

    use nexus_core::{ErrorCategory, RetryGuidance};
    use serde_json::{Number, json};

    use super::*;

    fn integer(value: i64) -> Number {
        Number::from(value)
    }

    fn unsigned(value: u64) -> Number {
        Number::from(value)
    }

    fn float(value: f64) -> Number {
        Number::from_f64(value).expect("test float is finite")
    }

    fn number_error(schema: &NumberSchema, value: &Number) -> AgentError {
        validate_number(schema, value).expect_err("number must be rejected")
    }

    fn string_error(schema: &StringSchema, text: &str) -> AgentError {
        validate_string(schema, text).expect_err("string must be rejected")
    }

    fn empty_object() -> ObjectSchema {
        ObjectSchema {
            properties: BTreeMap::new(),
            required: BTreeSet::new(),
            additional_properties: true,
            min_properties: None,
            max_properties: None,
        }
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
    fn compare_numbers_is_exact_across_integer_and_float_forms() {
        assert_eq!(
            compare_numbers(&integer(1), &integer(1)),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare_numbers(&integer(-1), &integer(1)),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_numbers(&unsigned(u64::MAX), &unsigned(u64::MAX - 1)),
            Some(Ordering::Greater)
        );
        assert_eq!(
            compare_numbers(&integer(i64::MIN), &integer(i64::MIN + 1)),
            Some(Ordering::Less)
        );
        // 2^53 + 1 has no exact f64 form; the nearest float is 2^53, so the
        // exact integer orders above the float that would round to itself.
        assert_eq!(
            compare_numbers(
                &unsigned(9_007_199_254_740_993),
                &float(9_007_199_254_740_993.0)
            ),
            Some(Ordering::Greater)
        );
        assert_eq!(
            compare_numbers(
                &unsigned(9_007_199_254_740_992),
                &float(9_007_199_254_740_993.0)
            ),
            Some(Ordering::Equal)
        );
        // Truncation ties are broken by the fractional part.
        assert_eq!(
            compare_numbers(&integer(2), &float(2.5)),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_numbers(&integer(3), &float(2.5)),
            Some(Ordering::Greater)
        );
        assert_eq!(
            compare_numbers(&integer(-1), &float(-0.5)),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_numbers(&float(2.5), &integer(2)),
            Some(Ordering::Greater)
        );
        assert_eq!(
            compare_numbers(&float(2.5), &integer(3)),
            Some(Ordering::Less)
        );
        // Signed zeros compare equal, so an inclusive bound admits -0.0.
        assert_eq!(
            compare_numbers(&float(-0.0), &integer(0)),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare_numbers(&float(0.0), &float(-0.0)),
            Some(Ordering::Equal)
        );
        // u64::MAX is just below the f64 value 2^64.
        assert_eq!(
            compare_numbers(&unsigned(u64::MAX), &float(18_446_744_073_709_551_616.0)),
            Some(Ordering::Less)
        );
        // i64::MIN is exactly representable as f64.
        assert_eq!(
            compare_numbers(&integer(i64::MIN), &float(-9_223_372_036_854_775_808.0)),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare_numbers(&integer(i64::MIN), &float(-9_223_372_036_854_775_000.0)),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn validate_string_counts_code_points_not_bytes() {
        let schema = StringSchema {
            min_length: Some(1),
            max_length: Some(2),
        };
        for text in ["a", "é", "🦀", "e\u{301}"] {
            validate_string(&schema, text).expect("within code-point bounds");
        }
        assert_invalid(&string_error(&schema, ""), "minimum length");
        assert_invalid(&string_error(&schema, "🦀🦀🦀"), "maximum length");
        assert_invalid(&string_error(&schema, "e\u{301}\u{301}"), "maximum length");

        let unbounded = StringSchema {
            min_length: None,
            max_length: None,
        };
        validate_string(&unbounded, "").expect("no lower bound");
        validate_string(&unbounded, "🦀🦀🦀").expect("no upper bound");
    }

    #[test]
    fn validate_string_minimum_bound_counts_code_points() {
        let schema = StringSchema {
            min_length: Some(2),
            max_length: None,
        };
        for text in ["ab", "éé", "🦀🦀", "e\u{301}"] {
            validate_string(&schema, text).expect("two code points");
        }
        for text in ["", "a", "é", "🦀", "\u{301}"] {
            assert_invalid(&string_error(&schema, text), "minimum length");
        }
    }

    #[test]
    fn validate_number_bounds_are_inclusive() {
        let schema = NumberSchema {
            minimum: Some(integer(0)),
            maximum: Some(integer(10)),
        };
        for value in [
            integer(0),
            integer(10),
            integer(5),
            float(0.0),
            float(-0.0),
            float(5.5),
            float(10.0),
        ] {
            validate_number(&schema, &value).expect("within inclusive bounds");
        }
        assert_invalid(&number_error(&schema, &integer(-1)), "minimum");
        assert_invalid(&number_error(&schema, &integer(11)), "maximum");
        assert_invalid(&number_error(&schema, &float(-0.000_1)), "minimum");
        assert_invalid(&number_error(&schema, &float(10.5)), "maximum");

        let unbounded = NumberSchema {
            minimum: None,
            maximum: None,
        };
        validate_number(&unbounded, &float(-0.0)).expect("no bounds");
        validate_number(&unbounded, &unsigned(u64::MAX)).expect("no bounds");

        let min_only = NumberSchema {
            minimum: Some(float(0.5)),
            maximum: None,
        };
        validate_number(&min_only, &float(0.5)).expect("bound inclusive");
        validate_number(&min_only, &integer(1)).expect("integer above float bound");
        assert_invalid(&number_error(&min_only, &float(0.499_9)), "minimum");

        let max_only = NumberSchema {
            minimum: None,
            maximum: Some(float(0.5)),
        };
        validate_number(&max_only, &float(0.5)).expect("bound inclusive");
        validate_number(&max_only, &integer(0)).expect("integer below float bound");
        assert_invalid(&number_error(&max_only, &float(0.500_1)), "maximum");
    }

    #[test]
    fn validate_number_compares_large_integers_exactly() {
        let schema = NumberSchema {
            minimum: Some(unsigned(9_007_199_254_740_993)),
            maximum: Some(unsigned(9_007_199_254_740_995)),
        };
        validate_number(&schema, &unsigned(9_007_199_254_740_993))
            .expect("lower bound is inclusive");
        validate_number(&schema, &unsigned(9_007_199_254_740_995))
            .expect("upper bound is inclusive");
        validate_number(&schema, &float(9_007_199_254_740_994.0))
            .expect("representable float inside the window");
        assert_invalid(
            &number_error(&schema, &unsigned(9_007_199_254_740_992)),
            "minimum",
        );
        assert_invalid(
            &number_error(&schema, &unsigned(9_007_199_254_740_996)),
            "maximum",
        );
        // The source float literal rounds to 2^53, below the exact minimum.
        assert_invalid(
            &number_error(&schema, &float(9_007_199_254_740_993.0)),
            "minimum",
        );

        let lower = NumberSchema {
            minimum: Some(unsigned(u64::MAX)),
            maximum: None,
        };
        validate_number(&lower, &unsigned(u64::MAX)).expect("u64::MAX is inclusive");
        assert_invalid(&number_error(&lower, &unsigned(u64::MAX - 1)), "minimum");

        let upper = NumberSchema {
            minimum: None,
            maximum: Some(integer(i64::MIN)),
        };
        validate_number(&upper, &integer(i64::MIN)).expect("i64::MIN is inclusive");
        assert_invalid(&number_error(&upper, &integer(i64::MIN + 1)), "maximum");
        assert_invalid(
            &number_error(&upper, &float(-9_223_372_036_854_775_000.0)),
            "maximum",
        );
    }

    #[test]
    fn validate_node_accepts_scalars_of_the_declared_type() {
        validate_node(&NodeSchema::Boolean, &json!(true)).expect("true is a boolean");
        validate_node(&NodeSchema::Boolean, &json!(false)).expect("false is a boolean");
        validate_node(&NodeSchema::Null, &json!(null)).expect("null is null");
        validate_node(
            &NodeSchema::String(StringSchema {
                min_length: Some(1),
                max_length: Some(2),
            }),
            &json!("é"),
        )
        .expect("string within bounds");
        validate_node(
            &NodeSchema::Number(NumberSchema {
                minimum: Some(integer(0)),
                maximum: Some(integer(1)),
            }),
            &json!(1),
        )
        .expect("number within bounds");
    }

    #[test]
    fn validate_node_rejects_scalar_type_mismatches() {
        let string = || {
            NodeSchema::String(StringSchema {
                min_length: None,
                max_length: None,
            })
        };
        let number = || {
            NodeSchema::Number(NumberSchema {
                minimum: None,
                maximum: None,
            })
        };
        let array = || {
            NodeSchema::Array(ArraySchema {
                items: Box::new(NodeSchema::Null),
                min_items: None,
                max_items: None,
            })
        };
        let cases: &[(NodeSchema, Value)] = &[
            (string(), json!(0)),
            (string(), json!(null)),
            (string(), json!(true)),
            (number(), json!("1")),
            (number(), json!(true)),
            (number(), json!(null)),
            (NodeSchema::Boolean, json!(0)),
            (NodeSchema::Boolean, json!("false")),
            (NodeSchema::Boolean, json!(null)),
            (NodeSchema::Null, json!(false)),
            (NodeSchema::Null, json!(0)),
            (NodeSchema::Null, json!("")),
            (NodeSchema::Object(empty_object()), json!([])),
            (array(), json!({})),
        ];
        for (schema, value) in cases {
            assert_invalid(
                &validate_node(schema, value).expect_err("type mismatch"),
                "type",
            );
        }
    }

    #[test]
    fn validate_array_applies_scalar_item_schemas() {
        let schema = ArraySchema {
            items: Box::new(NodeSchema::Boolean),
            min_items: Some(1),
            max_items: Some(2),
        };
        validate_array(&schema, &[json!(true), json!(false)]).expect("booleans conform");
        assert_invalid(
            &validate_array(&schema, &[json!(true), json!(null)]).expect_err("null is not boolean"),
            "type",
        );
        assert_invalid(
            &validate_array(&schema, &[]).expect_err("empty array is below the bound"),
            "size bound",
        );
        assert_invalid(
            &validate_array(&schema, &[json!(true), json!(false), json!(true)])
                .expect_err("three items exceed the bound"),
            "size bound",
        );
    }

    #[test]
    fn validate_object_dispatches_scalar_properties() {
        let schema = ObjectSchema {
            properties: BTreeMap::from([
                ("flag".to_owned(), NodeSchema::Boolean),
                ("note".to_owned(), NodeSchema::Null),
            ]),
            required: BTreeSet::from(["flag".to_owned()]),
            additional_properties: false,
            min_properties: None,
            max_properties: Some(2),
        };
        validate_object(
            &schema,
            &Map::from_iter([
                ("flag".to_owned(), json!(true)),
                ("note".to_owned(), json!(null)),
            ]),
        )
        .expect("scalar properties conform");
        assert_invalid(
            &validate_object(&schema, &Map::from_iter([("flag".to_owned(), json!(1))]))
                .expect_err("number is not boolean"),
            "type",
        );
        assert_invalid(
            &validate_object(
                &schema,
                &Map::from_iter([
                    ("flag".to_owned(), json!(true)),
                    ("extra".to_owned(), json!(null)),
                ]),
            )
            .expect_err("undeclared property"),
            "undeclared",
        );
        assert_invalid(
            &validate_object(&schema, &Map::new()).expect_err("required property missing"),
            "required",
        );
    }

    #[test]
    fn type_mismatch_is_invalid_input_and_non_retryable() {
        assert_invalid(&type_mismatch(), "type");
    }
}
