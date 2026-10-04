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
