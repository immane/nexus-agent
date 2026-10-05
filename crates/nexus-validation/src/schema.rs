//! Closed M0 schema subset compilation.
//!
//! Compilation rejects everything outside the locked subset: unknown
//! keywords, `$ref`, untyped nodes, unsupported types, keywords applied to
//! the wrong type, inverted bounds, malformed structures, and budget
//! exhaustion. The compiled form is immutable and contains no schema text
//! and no references.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use nexus_core::AgentError;
use serde_json::{Map, Number, Value};

use crate::parse::{ParseFault, parse_strict};
use crate::{
    MAX_KEY_BYTES, MAX_SCHEMA_BYTES, MAX_SCHEMA_DEPTH, MAX_SCHEMA_NODES, MAX_SCHEMA_PROPERTIES,
    input_error, limit_error,
};

/// Compiled, immutable M0 tool input schema.
///
/// All structure, keyword applicability, bound ordering, and budget checks
/// happen once in [`CompiledSchema::compile`]; [`CompiledSchema::validate`]
/// only compares values against the compiled form.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledSchema {
    pub(crate) root: ObjectSchema,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ObjectSchema {
    pub(crate) properties: BTreeMap<String, NodeSchema>,
    pub(crate) required: BTreeSet<String>,
    pub(crate) additional_properties: bool,
    pub(crate) min_properties: Option<u64>,
    pub(crate) max_properties: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NodeSchema {
    Object(ObjectSchema),
    Array(ArraySchema),
    String(StringSchema),
    Number(NumberSchema),
    Boolean,
    Null,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ArraySchema {
    pub(crate) items: Box<NodeSchema>,
    pub(crate) min_items: Option<u64>,
    pub(crate) max_items: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StringSchema {
    pub(crate) min_length: Option<u64>,
    pub(crate) max_length: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NumberSchema {
    pub(crate) minimum: Option<Number>,
    pub(crate) maximum: Option<Number>,
}

impl CompiledSchema {
    /// Compiles one schema text, rejecting anything outside the closed M0
    /// subset.
    ///
    /// Malformed text, non-object roots, missing or unsupported types,
    /// unknown keywords (including `$ref`), misplaced keywords, invalid
    /// property/required declarations, and inverted bounds fail with
    /// `ErrorCategory::InvalidInput`. Byte, depth, node, and property budget
    /// exhaustion fails with `ErrorCategory::ResourceLimit`.
    pub fn compile(schema: &str) -> Result<Self, AgentError> {
        if schema.len() > MAX_SCHEMA_BYTES {
            return Err(limit_error("schema exceeds its byte budget"));
        }
        let value = parse_strict(schema, MAX_SCHEMA_DEPTH, MAX_SCHEMA_NODES)
            .map_err(ParseFault::into_schema_error)?;
        let root = value
            .as_object()
            .ok_or_else(|| input_error("schema must be an object"))?;
        match root.get("type") {
            Some(type_value) if type_value.as_str() == Some("object") => {}
            Some(_) => return Err(input_error("schema root type must be object")),
            None => return Err(input_error("schema object must declare a type")),
        }
        let mut compiler = Compiler { properties: 0 };
        let root = compile_object(&mut compiler, root)?;
        Ok(Self { root })
    }
}

/// Cross-node compile state: the property budget is shared by the whole
/// schema, not per object.
struct Compiler {
    properties: usize,
}

fn compile_node(compiler: &mut Compiler, value: &Value) -> Result<NodeSchema, AgentError> {
    let object = value
        .as_object()
        .ok_or_else(|| input_error("schema node must be an object"))?;
    let type_value = object
        .get("type")
        .ok_or_else(|| input_error("schema object must declare a type"))?;
    let type_name = type_value
        .as_str()
        .ok_or_else(|| input_error("schema type is unsupported"))?;
    match type_name {
        "object" => Ok(NodeSchema::Object(compile_object(compiler, object)?)),
        "array" => Ok(NodeSchema::Array(compile_array(compiler, object)?)),
        "string" => Ok(NodeSchema::String(compile_string(object)?)),
        "number" => Ok(NodeSchema::Number(compile_number(object)?)),
        "boolean" => {
            compile_scalar(object)?;
            Ok(NodeSchema::Boolean)
        }
        "null" => {
            compile_scalar(object)?;
            Ok(NodeSchema::Null)
        }
        _ => Err(input_error("schema type is unsupported")),
    }
}

fn compile_object(
    compiler: &mut Compiler,
    object: &Map<String, Value>,
) -> Result<ObjectSchema, AgentError> {
    let mut properties = BTreeMap::new();
    let mut required = BTreeSet::new();
    let mut additional_properties = true;
    let mut min_properties = None;
    let mut max_properties = None;
    for (keyword, value) in object {
        match keyword.as_str() {
            "type" => {}
            "properties" => {
                let declared = value
                    .as_object()
                    .ok_or_else(|| input_error("schema properties must be an object of schemas"))?;
                for (name, property) in declared {
                    if name.is_empty() || name.len() > MAX_KEY_BYTES {
                        return Err(input_error("schema property name is invalid"));
                    }
                    compiler.properties += 1;
                    if compiler.properties > MAX_SCHEMA_PROPERTIES {
                        return Err(limit_error("schema property budget exhausted"));
                    }
                    let compiled = compile_node(compiler, property)?;
                    properties.insert(name.clone(), compiled);
                }
            }
            "required" => {
                let names = value.as_array().ok_or_else(|| {
                    input_error("schema required must be an array of unique strings")
                })?;
                if names.len() > MAX_SCHEMA_PROPERTIES {
                    return Err(limit_error("schema property budget exhausted"));
                }
                for name in names {
                    let name = name.as_str().ok_or_else(|| {
                        input_error("schema required must be an array of unique strings")
                    })?;
                    if name.is_empty() || name.len() > MAX_KEY_BYTES {
                        return Err(input_error("schema required name is invalid"));
                    }
                    if !required.insert(name.to_owned()) {
                        return Err(input_error("schema required contains a duplicate name"));
                    }
                }
            }
            "additionalProperties" => {
                additional_properties = value
                    .as_bool()
                    .ok_or_else(|| input_error("schema additionalProperties must be a boolean"))?;
            }
            "minProperties" => {
                min_properties = Some(non_negative_integer(
                    value,
                    "schema minProperties must be a non-negative integer",
                )?);
            }
            "maxProperties" => {
                max_properties = Some(non_negative_integer(
                    value,
                    "schema maxProperties must be a non-negative integer",
                )?);
            }
            _ => return Err(keyword_error(keyword)),
        }
    }
    check_ordered(
        min_properties,
        max_properties,
        "schema property bound is inverted",
    )?;
    if !additional_properties {
        for name in &required {
            if !properties.contains_key(name) {
                return Err(input_error(
                    "schema required name is undeclared while additionalProperties is false",
                ));
            }
        }
    }
    Ok(ObjectSchema {
        properties,
        required,
        additional_properties,
        min_properties,
        max_properties,
    })
}

fn compile_array(
    compiler: &mut Compiler,
    object: &Map<String, Value>,
) -> Result<ArraySchema, AgentError> {
    let mut items = None;
    let mut min_items = None;
    let mut max_items = None;
    for (keyword, value) in object {
        match keyword.as_str() {
            "type" => {}
            "items" => items = Some(Box::new(compile_node(compiler, value)?)),
            "minItems" => {
                min_items = Some(non_negative_integer(
                    value,
                    "schema minItems must be a non-negative integer",
                )?);
            }
            "maxItems" => {
                max_items = Some(non_negative_integer(
                    value,
                    "schema maxItems must be a non-negative integer",
                )?);
            }
            _ => return Err(keyword_error(keyword)),
        }
    }
    let items = items.ok_or_else(|| input_error("schema array must declare items"))?;
    check_ordered(min_items, max_items, "schema array bound is inverted")?;
    Ok(ArraySchema {
        items,
        min_items,
        max_items,
    })
}

fn compile_string(object: &Map<String, Value>) -> Result<StringSchema, AgentError> {
    let mut min_length = None;
    let mut max_length = None;
    for (keyword, value) in object {
        match keyword.as_str() {
            "type" => {}
            "minLength" => {
                min_length = Some(non_negative_integer(
                    value,
                    "schema minLength must be a non-negative integer",
                )?);
            }
            "maxLength" => {
                max_length = Some(non_negative_integer(
                    value,
                    "schema maxLength must be a non-negative integer",
                )?);
            }
            _ => return Err(keyword_error(keyword)),
        }
    }
    check_ordered(min_length, max_length, "schema string bound is inverted")?;
    Ok(StringSchema {
        min_length,
        max_length,
    })
}

fn compile_number(object: &Map<String, Value>) -> Result<NumberSchema, AgentError> {
    let mut minimum = None;
    let mut maximum = None;
    for (keyword, value) in object {
        match keyword.as_str() {
            "type" => {}
            "minimum" => minimum = Some(bounded_number(value)?),
            "maximum" => maximum = Some(bounded_number(value)?),
            _ => return Err(keyword_error(keyword)),
        }
    }
    if let (Some(min), Some(max)) = (&minimum, &maximum)
        && compare_numbers(min, max) == Some(Ordering::Greater)
    {
        return Err(input_error("schema number bound is inverted"));
    }
    Ok(NumberSchema { minimum, maximum })
}

/// Boolean and null schemas carry `type` and nothing else.
fn compile_scalar(object: &Map<String, Value>) -> Result<(), AgentError> {
    for keyword in object.keys() {
        if keyword != "type" {
            return Err(keyword_error(keyword));
        }
    }
    Ok(())
}

fn non_negative_integer(value: &Value, message: &'static str) -> Result<u64, AgentError> {
    value.as_u64().ok_or_else(|| input_error(message))
}

fn bounded_number(value: &Value) -> Result<Number, AgentError> {
    let number = value
        .as_number()
        .ok_or_else(|| input_error("schema minimum/maximum must be a number"))?;
    if number.as_f64().is_some_and(|float| !float.is_finite()) {
        return Err(input_error("schema minimum/maximum must be finite"));
    }
    Ok(number.clone())
}

/// Exact ordering across serde_json's integer and float representations.
///
/// Integer/integer compares through `i128`. Integer/float compares through
/// the float's truncation and fractional part, so integers larger than f64's
/// exact range are not silently rounded. Returns `None` only for a
/// non-finite float, which serde_json parsing already rejects.
pub(crate) fn compare_numbers(left: &Number, right: &Number) -> Option<Ordering> {
    match (left.as_i128(), right.as_i128()) {
        (Some(left), Some(right)) => Some(left.cmp(&right)),
        (Some(integer), None) => compare_integer_float(integer, right.as_f64()?),
        (None, Some(integer)) => {
            compare_integer_float(integer, left.as_f64()?).map(Ordering::reverse)
        }
        (None, None) => left.as_f64()?.partial_cmp(&right.as_f64()?),
    }
}

fn compare_integer_float(integer: i128, float: f64) -> Option<Ordering> {
    if !float.is_finite() {
        return None;
    }
    // f64 can represent 2^127 exactly; i128 is [-2^127, 2^127 - 1].
    const TWO_POW_127: f64 = 170_141_183_460_469_231_731_687_303_715_884_105_728.0;
    if float >= TWO_POW_127 {
        return Some(Ordering::Less);
    }
    if float < -TWO_POW_127 {
        return Some(Ordering::Greater);
    }
    let truncated = float.trunc() as i128;
    match integer.cmp(&truncated) {
        // A positive fraction makes the float larger than the integer, so
        // the integer orders below it.
        Ordering::Equal => float.fract().partial_cmp(&0.0).map(Ordering::reverse),
        ordering => Some(ordering),
    }
}

fn check_ordered(
    min: Option<u64>,
    max: Option<u64>,
    inverted: &'static str,
) -> Result<(), AgentError> {
    if let (Some(min), Some(max)) = (min, max)
        && min > max
    {
        return Err(input_error(inverted));
    }
    Ok(())
}

/// Keywords this subset defines anywhere. A known keyword on the wrong type
/// is a different registration failure than an unknown keyword.
const KNOWN_KEYWORDS: &[&str] = &[
    "type",
    "properties",
    "required",
    "additionalProperties",
    "maxProperties",
    "minProperties",
    "items",
    "maxItems",
    "minItems",
    "maxLength",
    "minLength",
    "minimum",
    "maximum",
];

fn keyword_error(keyword: &str) -> AgentError {
    if keyword == "$ref" {
        input_error("schema $ref is unsupported")
    } else if KNOWN_KEYWORDS.contains(&keyword) {
        input_error("schema keyword is not valid for this type")
    } else {
        input_error("schema contains an unsupported keyword")
    }
}

#[cfg(test)]
mod cov_schema_private {
    use super::*;
    use nexus_core::{ErrorCategory, RetryGuidance};

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).expect("private fixture parses")
    }

    fn object(text: &str) -> Map<String, Value> {
        parse(text)
            .as_object()
            .expect("private fixture is an object")
            .clone()
    }

    fn null_schema() -> Value {
        parse(r#"{"type":"null"}"#)
    }

    #[test]
    fn known_keyword_set_is_locked() {
        assert_eq!(
            KNOWN_KEYWORDS,
            [
                "type",
                "properties",
                "required",
                "additionalProperties",
                "maxProperties",
                "minProperties",
                "items",
                "maxItems",
                "minItems",
                "maxLength",
                "minLength",
                "minimum",
                "maximum",
            ]
            .as_slice()
        );
    }

    #[test]
    fn keyword_error_distinguishes_ref_known_and_unknown() {
        let error = keyword_error("$ref");
        assert_eq!(error.category(), ErrorCategory::InvalidInput);
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        assert_eq!(error.message(), "schema $ref is unsupported");

        for &keyword in KNOWN_KEYWORDS {
            let error = keyword_error(keyword);
            assert_eq!(error.category(), ErrorCategory::InvalidInput, "{keyword}");
            assert_eq!(
                error.message(),
                "schema keyword is not valid for this type",
                "{keyword}"
            );
        }

        for keyword in [
            "$schema",
            "$id",
            "$comment",
            "description",
            "enum",
            "pattern",
        ] {
            let error = keyword_error(keyword);
            assert_eq!(error.category(), ErrorCategory::InvalidInput, "{keyword}");
            assert_eq!(
                error.message(),
                "schema contains an unsupported keyword",
                "{keyword}"
            );
        }
    }

    #[test]
    fn compare_numbers_orders_exactly_across_representations() {
        let int = |value: i64| Number::from(value);
        let uint = |value: u64| Number::from(value);
        let float = |value: f64| Number::from_f64(value).expect("finite float fixture");

        assert_eq!(compare_numbers(&int(1), &int(2)), Some(Ordering::Less));
        assert_eq!(compare_numbers(&int(2), &int(2)), Some(Ordering::Equal));
        assert_eq!(compare_numbers(&int(3), &int(2)), Some(Ordering::Greater));

        // 9007199254740993 is not representable as f64; the integer form must
        // not be rounded.
        assert_eq!(
            compare_numbers(
                &uint(9_007_199_254_740_993),
                &float(9_007_199_254_740_992.0)
            ),
            Some(Ordering::Greater)
        );
        assert_eq!(
            compare_numbers(
                &float(9_007_199_254_740_992.0),
                &uint(9_007_199_254_740_993)
            ),
            Some(Ordering::Less)
        );

        // u64::MAX is exactly 2^64 - 1; 2^64 as f64 orders above it.
        assert_eq!(
            compare_numbers(&uint(u64::MAX), &float(18_446_744_073_709_551_616.0)),
            Some(Ordering::Less)
        );

        assert_eq!(
            compare_numbers(&float(1.5), &int(1)),
            Some(Ordering::Greater)
        );
        assert_eq!(compare_numbers(&int(1), &float(1.5)), Some(Ordering::Less));
        assert_eq!(compare_numbers(&float(2.0), &int(2)), Some(Ordering::Equal));
        assert_eq!(compare_numbers(&float(-0.5), &int(0)), Some(Ordering::Less));
        assert_eq!(
            compare_numbers(&float(1.0), &float(2.0)),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_numbers(&float(-0.0), &float(0.0)),
            Some(Ordering::Equal),
            "a signed zero must not invert a bound"
        );
        assert_eq!(
            compare_numbers(&int(0), &float(-0.0)),
            Some(Ordering::Equal)
        );

        assert_eq!(
            compare_numbers(&int(i64::MIN), &float(-1e308)),
            Some(Ordering::Greater)
        );
        assert_eq!(
            compare_numbers(&uint(u64::MAX), &float(1e308)),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn compare_integer_float_handles_i128_edges_and_non_finite_floats() {
        const TWO_POW_127: f64 = 170_141_183_460_469_231_731_687_303_715_884_105_728.0;

        assert_eq!(compare_integer_float(0, 0.5), Some(Ordering::Less));
        assert_eq!(compare_integer_float(0, -0.5), Some(Ordering::Greater));
        assert_eq!(compare_integer_float(1, 1.0), Some(Ordering::Equal));
        assert_eq!(compare_integer_float(1, 1.25), Some(Ordering::Less));
        assert_eq!(compare_integer_float(1, 0.75), Some(Ordering::Greater));
        assert_eq!(
            compare_integer_float(i128::MAX, TWO_POW_127),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_integer_float(i128::MIN, -TWO_POW_127),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare_integer_float(i128::MIN, -1.7e308),
            Some(Ordering::Greater)
        );
        assert_eq!(compare_integer_float(0, f64::INFINITY), None);
        assert_eq!(compare_integer_float(0, f64::NEG_INFINITY), None);
        assert_eq!(compare_integer_float(0, f64::NAN), None);
    }

    #[test]
    fn bounded_number_accepts_exactly_json_numbers() {
        for text in ["0", "-0.0", "1.5", "9007199254740993", "1e308", "-1e308"] {
            let value = parse(text);
            let number = value.as_number().expect("fixture is a number");
            assert_eq!(
                bounded_number(&value).expect("finite number accepted"),
                number.clone()
            );
        }
        for value in [
            Value::from("1"),
            Value::Bool(true),
            Value::Null,
            Value::Array(Vec::new()),
            Value::Object(Map::new()),
        ] {
            let error = bounded_number(&value).expect_err("non-number rejected");
            assert_eq!(error.category(), ErrorCategory::InvalidInput);
            assert_eq!(error.message(), "schema minimum/maximum must be a number");
        }
    }

    #[test]
    fn non_negative_integer_rejects_negative_fractional_and_non_numeric() {
        for value in [0u64, 1, u64::MAX] {
            assert_eq!(
                non_negative_integer(&Value::from(value), "probe").expect("accepted"),
                value
            );
        }
        for value in [
            Value::from(-1i64),
            Value::from(-1.0f64),
            Value::from(1.5f64),
            Value::from("0"),
            Value::Bool(true),
            Value::Null,
        ] {
            let error = non_negative_integer(&value, "probe").expect_err("rejected");
            assert_eq!(error.category(), ErrorCategory::InvalidInput);
            assert_eq!(error.message(), "probe");
        }
    }

    #[test]
    fn check_ordered_accepts_equal_and_rejects_inverted() {
        assert!(check_ordered(None, None, "probe").is_ok());
        assert!(check_ordered(Some(0), None, "probe").is_ok());
        assert!(check_ordered(None, Some(0), "probe").is_ok());
        assert!(check_ordered(Some(1), Some(1), "probe").is_ok());
        assert!(check_ordered(Some(0), Some(1), "probe").is_ok());

        let error = check_ordered(Some(2), Some(1), "probe").expect_err("inverted rejected");
        assert_eq!(error.category(), ErrorCategory::InvalidInput);
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        assert_eq!(error.message(), "probe");
    }

    #[test]
    fn compile_node_builds_the_exact_private_variants() {
        let mut compiler = Compiler { properties: 0 };

        let string = compile_node(
            &mut compiler,
            &parse(r#"{"type":"string","minLength":1,"maxLength":2}"#),
        )
        .expect("string compiles");
        assert_eq!(
            string,
            NodeSchema::String(StringSchema {
                min_length: Some(1),
                max_length: Some(2),
            })
        );

        let number = compile_node(
            &mut compiler,
            &parse(r#"{"type":"number","minimum":-1.5,"maximum":2}"#),
        )
        .expect("number compiles");
        assert_eq!(
            number,
            NodeSchema::Number(NumberSchema {
                minimum: Some(Number::from_f64(-1.5).expect("finite")),
                maximum: Some(Number::from(2)),
            })
        );

        assert_eq!(
            compile_node(&mut compiler, &parse(r#"{"type":"boolean"}"#)).expect("boolean compiles"),
            NodeSchema::Boolean
        );
        assert_eq!(
            compile_node(&mut compiler, &parse(r#"{"type":"null"}"#)).expect("null compiles"),
            NodeSchema::Null
        );

        let array = compile_node(
            &mut compiler,
            &parse(r#"{"type":"array","items":{"type":"boolean"},"minItems":1,"maxItems":2}"#),
        )
        .expect("array compiles");
        assert_eq!(
            array,
            NodeSchema::Array(ArraySchema {
                items: Box::new(NodeSchema::Boolean),
                min_items: Some(1),
                max_items: Some(2),
            })
        );

        let object = compile_node(
            &mut compiler,
            &parse(
                r#"{"type":"object","properties":{"a":{"type":"string"}},"required":["a"],"additionalProperties":false,"minProperties":1,"maxProperties":2}"#,
            ),
        )
        .expect("object compiles");
        let NodeSchema::Object(ObjectSchema {
            properties,
            required,
            additional_properties,
            min_properties,
            max_properties,
        }) = object
        else {
            panic!("object type must compile to the object variant");
        };
        assert_eq!(properties.len(), 1);
        assert_eq!(
            properties.get("a"),
            Some(&NodeSchema::String(StringSchema {
                min_length: None,
                max_length: None,
            }))
        );
        assert_eq!(required, BTreeSet::from(["a".to_owned()]));
        assert!(!additional_properties);
        assert_eq!(min_properties, Some(1));
        assert_eq!(max_properties, Some(2));

        assert_eq!(
            compiler.properties, 1,
            "only declared properties consume the property counter"
        );
    }

    #[test]
    fn compile_object_defaults_to_an_open_object() {
        let value = parse(r#"{"type":"object"}"#);
        let mut compiler = Compiler { properties: 0 };
        let compiled = compile_object(&mut compiler, value.as_object().expect("object fixture"))
            .expect("minimal object compiles");
        assert!(compiled.properties.is_empty());
        assert!(compiled.required.is_empty());
        assert!(compiled.additional_properties);
        assert_eq!(compiled.min_properties, None);
        assert_eq!(compiled.max_properties, None);
        assert_eq!(compiler.properties, 0);
    }

    #[test]
    fn compile_object_property_budget_is_global_and_exact() {
        let with_properties = |prefix: &str, count: usize| {
            let mut properties = Map::new();
            for index in 0..count {
                properties.insert(format!("{prefix}{index}"), null_schema());
            }
            let mut object = Map::new();
            object.insert("type".to_owned(), Value::from("object"));
            object.insert("properties".to_owned(), Value::Object(properties));
            Value::Object(object)
        };

        let at_budget = with_properties("p", MAX_SCHEMA_PROPERTIES);
        let mut compiler = Compiler { properties: 0 };
        let compiled = compile_object(
            &mut compiler,
            at_budget.as_object().expect("object fixture"),
        )
        .expect("properties at the budget compile");
        assert_eq!(compiled.properties.len(), MAX_SCHEMA_PROPERTIES);
        assert_eq!(compiler.properties, MAX_SCHEMA_PROPERTIES);

        let over_budget = with_properties("p", MAX_SCHEMA_PROPERTIES + 1);
        let error = compile_object(
            &mut Compiler { properties: 0 },
            over_budget.as_object().expect("object fixture"),
        )
        .expect_err("one property over the budget is rejected");
        assert_eq!(error.category(), ErrorCategory::ResourceLimit);
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        assert_eq!(error.message(), "schema property budget exhausted");

        // The counter is shared across nested objects. The root itself
        // declares `left` and `right`, so 2 + 127 + 127 fills the budget
        // exactly and 2 + 127 + 128 is one over.
        let mut root = Map::new();
        root.insert("type".to_owned(), Value::from("object"));
        let mut properties = Map::new();
        properties.insert(
            "left".to_owned(),
            with_properties("l", MAX_SCHEMA_PROPERTIES / 2 - 1),
        );
        properties.insert(
            "right".to_owned(),
            with_properties("r", MAX_SCHEMA_PROPERTIES / 2 - 1),
        );
        root.insert("properties".to_owned(), Value::Object(properties));

        let mut compiler = Compiler { properties: 0 };
        let compiled = compile_object(&mut compiler, &root).expect("2 + 127 + 127 compile");
        assert_eq!(compiled.properties.len(), 2);
        assert_eq!(compiler.properties, MAX_SCHEMA_PROPERTIES);

        let mut over_root = root.clone();
        over_root
            .get_mut("properties")
            .and_then(Value::as_object_mut)
            .expect("properties object")
            .insert(
                "right".to_owned(),
                with_properties("r", MAX_SCHEMA_PROPERTIES / 2),
            );
        let error = compile_object(&mut Compiler { properties: 0 }, &over_root)
            .expect_err("2 + 127 + 128 is rejected");
        assert_eq!(error.category(), ErrorCategory::ResourceLimit);
        assert_eq!(error.message(), "schema property budget exhausted");
    }

    #[test]
    fn compile_object_required_rules_are_exact() {
        let with_required = |count: usize| {
            let mut object = Map::new();
            object.insert("type".to_owned(), Value::from("object"));
            object.insert(
                "required".to_owned(),
                Value::Array(
                    (0..count)
                        .map(|index| Value::from(format!("r{index}")))
                        .collect(),
                ),
            );
            Value::Object(object)
        };

        let at_budget = with_required(MAX_SCHEMA_PROPERTIES);
        let mut compiler = Compiler { properties: 0 };
        let compiled = compile_object(
            &mut compiler,
            at_budget.as_object().expect("object fixture"),
        )
        .expect("required names at the budget compile");
        assert_eq!(compiled.required.len(), MAX_SCHEMA_PROPERTIES);
        assert_eq!(
            compiler.properties, 0,
            "required names do not consume the property budget"
        );

        let over_budget = with_required(MAX_SCHEMA_PROPERTIES + 1);
        let error = compile_object(
            &mut Compiler { properties: 0 },
            over_budget.as_object().expect("object fixture"),
        )
        .expect_err("one required name over the budget is rejected");
        assert_eq!(error.category(), ErrorCategory::ResourceLimit);
        assert_eq!(error.message(), "schema property budget exhausted");

        for text in [
            r#"{"type":"object","required":"a"}"#,
            r#"{"type":"object","required":[1]}"#,
            r#"{"type":"object","required":[null]}"#,
        ] {
            let error = compile_object(&mut Compiler { properties: 0 }, &object(text))
                .expect_err("non-string required entries are rejected");
            assert_eq!(error.category(), ErrorCategory::InvalidInput);
            assert_eq!(
                error.message(),
                "schema required must be an array of unique strings"
            );
        }

        let duplicate = object(r#"{"type":"object","required":["a","a"]}"#);
        let error = compile_object(&mut Compiler { properties: 0 }, &duplicate)
            .expect_err("duplicate required name rejected");
        assert_eq!(error.message(), "schema required contains a duplicate name");

        let empty = object(r#"{"type":"object","required":[""]}"#);
        let error = compile_object(&mut Compiler { properties: 0 }, &empty)
            .expect_err("empty required name rejected");
        assert_eq!(error.message(), "schema required name is invalid");

        let long = format!(
            r#"{{"type":"object","required":["{}"]}}"#,
            "x".repeat(MAX_KEY_BYTES + 1)
        );
        let error = compile_object(&mut Compiler { properties: 0 }, &object(&long))
            .expect_err("over-long required name rejected");
        assert_eq!(error.message(), "schema required name is invalid");

        let multibyte = "é".repeat(MAX_KEY_BYTES / 2);
        assert_eq!(multibyte.len(), MAX_KEY_BYTES);
        let at_key_bound = format!(r#"{{"type":"object","required":["{multibyte}"]}}"#);
        let compiled = compile_object(&mut Compiler { properties: 0 }, &object(&at_key_bound))
            .expect("multibyte required name at the byte bound compiles");
        assert!(compiled.required.contains(&multibyte));

        // Undeclared required names are rejected only for closed objects.
        let undeclared = object(
            r#"{"type":"object","properties":{"a":{"type":"null"}},"required":["b"],"additionalProperties":false}"#,
        );
        let error = compile_object(&mut Compiler { properties: 0 }, &undeclared)
            .expect_err("undeclared required name rejected for closed objects");
        assert_eq!(
            error.message(),
            "schema required name is undeclared while additionalProperties is false"
        );
        let open = object(r#"{"type":"object","required":["b"]}"#);
        compile_object(&mut Compiler { properties: 0 }, &open)
            .expect("undeclared required names are allowed for open objects");
    }

    #[test]
    fn compile_array_requires_items_and_ordered_bounds() {
        let error = compile_array(
            &mut Compiler { properties: 0 },
            &object(r#"{"type":"array"}"#),
        )
        .expect_err("items are required");
        assert_eq!(error.category(), ErrorCategory::InvalidInput);
        assert_eq!(error.message(), "schema array must declare items");

        let error = compile_array(
            &mut Compiler { properties: 0 },
            &object(r#"{"type":"array","items":{"type":"null"},"minItems":2,"maxItems":1}"#),
        )
        .expect_err("inverted bounds rejected");
        assert_eq!(error.message(), "schema array bound is inverted");

        let mut compiler = Compiler { properties: 0 };
        let compiled = compile_array(
            &mut compiler,
            &object(r#"{"type":"array","items":{"type":"null"},"minItems":0,"maxItems":0}"#),
        )
        .expect("equal bounds compile");
        assert_eq!(compiled.min_items, Some(0));
        assert_eq!(compiled.max_items, Some(0));
        assert_eq!(compiled.items.as_ref(), &NodeSchema::Null);
        assert_eq!(compiler.properties, 0);

        for text in [
            r#"{"type":"array","items":{"type":"null"},"minItems":-1}"#,
            r#"{"type":"array","items":{"type":"null"},"maxItems":1.5}"#,
        ] {
            let error = compile_array(&mut Compiler { properties: 0 }, &object(text))
                .expect_err("invalid size keyword rejected");
            assert_eq!(error.category(), ErrorCategory::InvalidInput);
        }
    }

    #[test]
    fn compile_string_and_number_bounds_are_ordered_exactly() {
        let error = compile_string(&object(r#"{"type":"string","minLength":2,"maxLength":1}"#))
            .expect_err("inverted string bounds rejected");
        assert_eq!(error.message(), "schema string bound is inverted");

        let compiled = compile_string(&object(r#"{"type":"string","minLength":1,"maxLength":1}"#))
            .expect("equal bounds compile");
        assert_eq!(compiled.min_length, Some(1));
        assert_eq!(compiled.max_length, Some(1));

        let error = compile_number(&object(r#"{"type":"number","minimum":2,"maximum":1}"#))
            .expect_err("inverted number bounds rejected");
        assert_eq!(error.message(), "schema number bound is inverted");

        // 1 and 1.0 compare equal across representations, so these bounds are
        // not inverted.
        let compiled = compile_number(&object(r#"{"type":"number","minimum":1,"maximum":1.0}"#))
            .expect("mixed-representation equal bounds compile");
        assert_eq!(compiled.minimum.as_ref().and_then(Number::as_i128), Some(1));
        assert_eq!(
            compiled.maximum.as_ref().and_then(Number::as_f64),
            Some(1.0)
        );

        // 2^64 does not fit u64 and parses as a float; u64::MAX parses as an
        // integer. The float minimum is still exactly above the integer
        // maximum.
        let error = compile_number(&object(
            r#"{"type":"number","minimum":18446744073709551616,"maximum":18446744073709551615}"#,
        ))
        .expect_err("float/integer inversion rejected");
        assert_eq!(error.message(), "schema number bound is inverted");

        for text in [
            r#"{"type":"number","minimum":"0"}"#,
            r#"{"type":"number","maximum":null}"#,
            r#"{"type":"number","minimum":true}"#,
        ] {
            let error = compile_number(&object(text)).expect_err("non-number bound rejected");
            assert_eq!(error.message(), "schema minimum/maximum must be a number");
        }
    }

    #[test]
    fn compile_scalar_accepts_only_the_type_keyword() {
        compile_scalar(&object(r#"{"type":"boolean"}"#)).expect("boolean type-only accepted");
        compile_scalar(&object(r#"{"type":"null"}"#)).expect("null type-only accepted");

        let error = compile_scalar(&object(r#"{"type":"boolean","default":false}"#))
            .expect_err("extra keyword rejected");
        assert_eq!(error.category(), ErrorCategory::InvalidInput);
        assert_eq!(error.message(), "schema contains an unsupported keyword");

        let error = compile_scalar(&object(r##"{"type":"null","$ref":"#/x"}"##))
            .expect_err("$ref rejected");
        assert_eq!(error.message(), "schema $ref is unsupported");
    }

    #[test]
    fn compile_stores_the_exact_compiled_root() {
        let compiled = CompiledSchema::compile(
            r#"{"type":"object","properties":{"a":{"type":"string","minLength":1}},"required":["a"],"additionalProperties":false,"minProperties":1,"maxProperties":2}"#,
        )
        .expect("schema compiles");
        assert!(!compiled.root.additional_properties);
        assert_eq!(compiled.root.required, BTreeSet::from(["a".to_owned()]));
        assert_eq!(compiled.root.min_properties, Some(1));
        assert_eq!(compiled.root.max_properties, Some(2));
        assert_eq!(
            compiled.root.properties.get("a"),
            Some(&NodeSchema::String(StringSchema {
                min_length: Some(1),
                max_length: None,
            }))
        );
    }

    #[test]
    fn compile_object_counts_declared_properties_before_failure() {
        // "a" compiles and "b" is untyped; both declarations are counted
        // before the second one fails.
        let root = object(r#"{"type":"object","properties":{"a":{"type":"null"},"b":{}}}"#);
        let mut compiler = Compiler { properties: 0 };
        let error = compile_object(&mut compiler, &root).expect_err("untyped property rejected");
        assert_eq!(error.category(), ErrorCategory::InvalidInput);
        assert_eq!(error.message(), "schema object must declare a type");
        assert_eq!(compiler.properties, 2);
    }
}
