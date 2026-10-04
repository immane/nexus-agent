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
