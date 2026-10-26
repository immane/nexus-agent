//! Bounded single-request JSON framing for the opt-in file worker.
//! This is not a container boundary and does not provide active cancellation.

use std::io::{self, Read, Write};

use serde_json::Value;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_REQUEST_BYTES: usize = 1_048_576;
pub const MAX_OUTPUT_BYTES: usize = 262_144;
// A JSON string can expand each input byte to six ASCII bytes (`\\u00XX`).
// Leave room for the response object and its metadata as well.
pub const MAX_RESPONSE_BYTES: usize = MAX_OUTPUT_BYTES * 6 + 256;
pub const MAX_ARGUMENT_BYTES: usize = 65_536;

const OPERATIONS: [&str; 5] = [
    "host_read",
    "host_list",
    "host_search",
    "host_write",
    "host_patch",
];

#[derive(Debug)]
pub struct Request {
    pub version: u16,
    pub operation: String,
    pub arguments: Value,
    pub output_limit: usize,
}

#[derive(Debug)]
pub struct Response {
    pub version: u16,
    pub result: Value,
}

#[derive(Debug)]
pub enum ProtocolError {
    Io(io::Error),
    InvalidRequest,
    ResponseTooLarge,
}

impl From<io::Error> for ProtocolError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Io(_) => "protocol I/O failed",
            Self::InvalidRequest => "request frame is invalid",
            Self::ResponseTooLarge => "response exceeds its bound",
        })
    }
}

impl std::error::Error for ProtocolError {}

/// Reads exactly one JSON document, rejecting excess bytes, trailing JSON,
/// unknown operations, and requests that exceed the output allowance.
pub fn read_request<R: Read>(reader: &mut R) -> Result<Request, ProtocolError> {
    let mut bytes = Vec::new();
    reader
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(ProtocolError::InvalidRequest);
    }
    let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
    let value = UniqueValue::deserialize(&mut deserializer)
        .map_err(|_| ProtocolError::InvalidRequest)?
        .0;
    deserializer
        .end()
        .map_err(|_| ProtocolError::InvalidRequest)?;
    let object = value.as_object().ok_or(ProtocolError::InvalidRequest)?;
    if object.len() != 4 {
        return Err(ProtocolError::InvalidRequest);
    }
    let request = Request {
        version: object
            .get("version")
            .and_then(Value::as_u64)
            .and_then(|n| u16::try_from(n).ok())
            .ok_or(ProtocolError::InvalidRequest)?,
        operation: object
            .get("operation")
            .and_then(Value::as_str)
            .ok_or(ProtocolError::InvalidRequest)?
            .to_owned(),
        arguments: object
            .get("arguments")
            .cloned()
            .ok_or(ProtocolError::InvalidRequest)?,
        output_limit: object
            .get("output_limit")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or(ProtocolError::InvalidRequest)?,
    };
    if request.version != PROTOCOL_VERSION
        || !OPERATIONS.contains(&request.operation.as_str())
        || request.output_limit == 0
        || request.output_limit > MAX_OUTPUT_BYTES
        || !request.arguments.is_object()
        || serde_json::to_vec(&request.arguments).map_or(true, |v| v.len() > MAX_ARGUMENT_BYTES)
        || !valid_arguments(&request.operation, &request.arguments)
    {
        return Err(ProtocolError::InvalidRequest);
    }
    Ok(request)
}

pub(crate) fn valid_arguments(operation: &str, value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if operation == "host_read" {
        return nexus_tools::fs::valid_read_arguments(value);
    }
    let fields: &[(&str, bool)] = match operation {
        "host_list" => &[("path", true)],
        "host_search" => &[("path", true), ("query", true)],
        "host_write" => &[("path", true), ("content", true)],
        "host_patch" => &[("path", true), ("old_text", true), ("new_text", true)],
        _ => return false,
    };
    object.len() == fields.len()
        && fields
            .iter()
            .all(|(name, _)| object.get(*name).is_some_and(Value::is_string))
}

/// Require every path-bearing argument to remain lexically beneath `/workspace`
/// before the rooted filesystem adapters perform canonical resolution.
pub fn validate_workspace_paths(operation: &str, arguments: &Value) -> Result<(), ProtocolError> {
    use std::path::{Component, Path};
    let object = arguments.as_object().ok_or(ProtocolError::InvalidRequest)?;
    for key in ["path"] {
        let path = object
            .get(key)
            .and_then(Value::as_str)
            .ok_or(ProtocolError::InvalidRequest)?;
        let path = Path::new(path);
        let mut normalized = Path::new("").to_path_buf();
        for component in path.components() {
            match component {
                Component::RootDir if normalized.as_os_str().is_empty() => normalized.push("/"),
                Component::Normal(part) => normalized.push(part),
                Component::CurDir => {}
                _ => return Err(ProtocolError::InvalidRequest),
            }
        }
        if path.is_absolute() && !normalized.starts_with("/workspace") {
            return Err(ProtocolError::InvalidRequest);
        }
        if !path.is_absolute()
            && normalized
                .components()
                .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(ProtocolError::InvalidRequest);
        }
    }
    let _ = operation;
    Ok(())
}

// Deserialize JSON recursively while refusing duplicate keys before Value's
// map representation can silently collapse them.
use serde::Deserialize;
use serde::de::{MapAccess, SeqAccess, Visitor};
use std::fmt;
struct UniqueValue(Value);
pub(crate) fn parse_unique(bytes: &[u8]) -> Result<Value, ProtocolError> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = UniqueValue::deserialize(&mut deserializer)
        .map_err(|_| ProtocolError::InvalidRequest)?
        .0;
    deserializer
        .end()
        .map_err(|_| ProtocolError::InvalidRequest)?;
    Ok(value)
}
impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = UniqueValue;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("JSON value without duplicate keys")
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Bool(v)))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::from(v)))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::from(v)))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(|number| UniqueValue(Value::Number(number)))
                    .ok_or_else(|| E::custom("invalid number"))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::String(v.to_owned())))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::String(v)))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                self.visit_unit()
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(v) = seq.next_element::<UniqueValue>()? {
                    out.push(v.0)
                }
                Ok(UniqueValue(Value::Array(out)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, UniqueValue>()? {
                    if out.insert(key, value.0).is_some() {
                        return Err(serde::de::Error::custom("duplicate key"));
                    }
                }
                Ok(UniqueValue(Value::Object(out)))
            }
        }
        d.deserialize_any(V)
    }
}

/// Emits one compact response followed by EOF; response serialization is
/// bounded before any bytes are written.
pub fn write_response<W: Write>(writer: &mut W, result: Value) -> Result<(), ProtocolError> {
    let response = serde_json::json!({"version": PROTOCOL_VERSION, "result": result});
    let mut bytes = Vec::with_capacity(MAX_RESPONSE_BYTES.min(4096));
    let mut bounded = BoundedWriter {
        output: &mut bytes,
        limit: MAX_RESPONSE_BYTES,
    };
    serde_json::to_writer(&mut bounded, &response).map_err(|error| {
        if error.is_io() {
            ProtocolError::ResponseTooLarge
        } else {
            ProtocolError::InvalidRequest
        }
    })?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

struct BoundedWriter<'a> {
    output: &'a mut Vec<u8>,
    limit: usize,
}
impl Write for BoundedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.output.len().saturating_add(bytes.len()) > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "response frame limit",
            ));
        }
        self.output.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn frame(body: &str) -> Result<Request, ProtocolError> {
        read_request(&mut Cursor::new(body.as_bytes()))
    }

    #[test]
    fn accepts_each_supported_file_operation() {
        for operation in OPERATIONS {
            let arguments = match operation {
                "host_read" | "host_list" => r#"{"path":"a"}"#,
                "host_search" => r#"{"path":"a","query":"q"}"#,
                "host_write" => r#"{"path":"a","content":"x"}"#,
                "host_patch" => r#"{"path":"a","old_text":"x","new_text":"y"}"#,
                _ => unreachable!(),
            };
            let input = format!(
                r#"{{"version":1,"operation":"{operation}","arguments":{arguments},"output_limit":64}}"#
            );
            assert_eq!(frame(&input).unwrap().operation, operation);
        }
    }

    #[test]
    fn rejects_unknown_version_operation_and_fields() {
        for input in [
            r#"{"version":2,"operation":"host_read","arguments":{},"output_limit":1}"#,
            r#"{"version":1,"operation":"exec","arguments":{},"output_limit":1}"#,
            r#"{"version":1,"operation":"host_read","arguments":{},"output_limit":1,"extra":true}"#,
            r#"{"version":1,"operation":"host_read","arguments":{},"output_limit":1} {}"#,
        ] {
            assert!(matches!(frame(input), Err(ProtocolError::InvalidRequest)));
        }
    }

    #[test]
    fn rejects_oversized_frame_and_output_allowance() {
        let oversized = vec![b' '; MAX_REQUEST_BYTES + 1];
        assert!(matches!(
            read_request(&mut Cursor::new(oversized)),
            Err(ProtocolError::InvalidRequest)
        ));
        assert!(matches!(
            frame(&format!(
                r#"{{"version":1,"operation":"host_read","arguments":{{}},"output_limit":{}}}"#,
                MAX_OUTPUT_BYTES + 1
            )),
            Err(ProtocolError::InvalidRequest)
        ));
    }

    #[test]
    fn response_is_versioned_json() {
        let mut output = Vec::new();
        write_response(&mut output, serde_json::json!({"ok": true})).unwrap();
        let response: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(response["version"], PROTOCOL_VERSION);
        assert_eq!(response["result"]["ok"], true);
    }

    #[test]
    fn maximum_output_survives_worst_case_json_escaping() {
        let mut output = Vec::new();
        write_response(
            &mut output,
            serde_json::json!({"content": "\u{0000}".repeat(MAX_OUTPUT_BYTES)}),
        )
        .unwrap();
        assert!(output.len() <= MAX_RESPONSE_BYTES);
        let response: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            response["result"]["content"].as_str().unwrap().len(),
            MAX_OUTPUT_BYTES
        );
    }
}
