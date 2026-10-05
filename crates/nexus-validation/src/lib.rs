//! Narrow, audited JSON schema and argument validation for M0 tool calls.
//!
//! This crate owns exactly one boundary: the locked M0 tool input-schema
//! subset (task `06-m0-lock.md`, section 4) and the argument text produced
//! for an admitted call. [`CompiledSchema::compile`] compiles a schema text
//! once; [`CompiledSchema::validate`] then validates argument text against
//! it and returns canonical immutable [`nexus_core::NormalizedArgs`]. [`parse_object`]
//! exposes the same strict parse for runtime policy checks. Nothing here
//! executes a tool, performs I/O, or resolves references.
//!
//! # Dependency justification
//!
//! `serde_json` is the only JSON parser in this crate. It owns lexing,
//! string and number decoding, and its internal recursion limit; this crate
//! only walks the parsed value. `serde` is a direct dependency solely for
//! the `DeserializeSeed`/`Visitor` trait hooks needed to reject duplicate
//! object keys while staying on serde_json's streaming API -- it is not a
//! second parser, and no schema framework is used. `serde_json` is
//! re-exported so callers of [`parse_object`] can name the returned value
//! without a direct dependency of their own.
//!
//! # Closed M0 subset
//!
//! A schema is an object-root JSON document, and every schema node must be
//! an object with an explicit `type` of `"object"`, `"array"`, `"string"`,
//! `"number"`, `"boolean"`, or `"null"`. The closed keyword set is:
//!
//! - object: `properties`, `required`, `additionalProperties`,
//!   `minProperties`, `maxProperties`
//! - array: `items` (required), `minItems`, `maxItems`
//! - string: `minLength`, `maxLength`
//! - number: `minimum`, `maximum`
//!
//! `$ref`, `$schema`, and every other keyword fail compilation explicitly;
//! there is no reference resolution and no implicit "any" type. An untyped
//! schema node is rejected: without `type`, keyword applicability is
//! ambiguous and an untyped node would accept any value, which the locked
//! subset does not include. The root type must be `"object"`. The existing
//! `{"type":"object"}` tool schemas compile and, following JSON Schema,
//! accept arbitrary object arguments because `additionalProperties`
//! defaults to `true`. Arrays must declare `items`, so no array element is
//! left unvalidated.
//!
//! # Budgets
//!
//! Schema compilation is bounded by [`MAX_SCHEMA_BYTES`],
//! [`MAX_SCHEMA_DEPTH`], [`MAX_SCHEMA_NODES`], [`MAX_SCHEMA_PROPERTIES`],
//! and [`MAX_KEY_BYTES`]. Argument parsing and validation are bounded by the
//! caller's `max_bytes` plus [`MAX_ARGS_DEPTH`] and [`MAX_ARGS_NODES`]. The
//! caller's `max_bytes` must lie in `1..=MAX_ARG_BYTES`, where `MAX_ARG_BYTES`
//! is the core M0 argument-assembly budget: an out-of-range budget is invalid
//! configuration and maps to `ErrorCategory::InvalidInput`, never to a silent
//! clamp. serde_json's own recursion limit remains a backstop behind these
//! tighter budgets. Exhaustion maps to `ErrorCategory::ResourceLimit`, never
//! to a silent fallback.
//!
//! # Canonical arguments
//!
//! [`CompiledSchema::validate`] returns [`nexus_core::NormalizedArgs`] whose text is the
//! compact, recursively key-sorted (UTF-8 byte order) serialization of the
//! validated value. Key order and insignificant whitespace therefore cannot
//! change an approval binding. Numbers and strings keep exactly the
//! representation serde_json parses and re-serializes (for example `1.0`
//! stays `1.0` and `-0.0` stays distinct from `0`); there is no numeric
//! normalization beyond that round-trip. Duplicate object keys are rejected
//! while parsing, in schemas and in arguments, so serde_json's default
//! last-key-wins behavior is never reached and canonical output cannot
//! contain duplicates.
//!
//! # Example
//!
//! ```
//! use nexus_validation::CompiledSchema;
//!
//! let schema = CompiledSchema::compile(
//!     r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}"#,
//! )
//! .expect("closed M0 schema compiles");
//! let args = schema
//!     .validate(r#"{ "path" : "src" }"#, 65_536)
//!     .expect("arguments validate");
//! assert_eq!(args.as_str(), r#"{"path":"src"}"#);
//! ```

#![forbid(unsafe_code)]

mod parse;
mod schema;
mod validate;

pub use schema::CompiledSchema;
// Re-exported for `parse_object` consumers (runtime policy checks) so they
// can name the returned value without adding serde_json directly.
pub use serde_json;

use nexus_core::{AgentError, ErrorCategory, Limits, RetryGuidance};

/// Maximum schema text in bytes. Equal to the core `ToolSpec` bound so this
/// validator can never accept a schema the carrier rejects.
pub const MAX_SCHEMA_BYTES: usize = nexus_core::tool::MAX_SCHEMA_BYTES;

/// Maximum JSON nesting depth accepted while compiling a schema (M0-TEST
/// choice, not a product default).
pub const MAX_SCHEMA_DEPTH: usize = 16;

/// Maximum JSON value count accepted while compiling a schema (M0-TEST
/// choice, not a product default).
pub const MAX_SCHEMA_NODES: usize = 2_048;

/// Maximum total declared properties accepted in one schema (M0-TEST
/// choice, not a product default).
pub const MAX_SCHEMA_PROPERTIES: usize = 256;

/// Maximum property or `required` name length in bytes (M0-TEST choice, not
/// a product default).
pub const MAX_KEY_BYTES: usize = 128;

/// Maximum JSON nesting depth accepted in argument text (M0-TEST choice,
/// not a product default).
pub const MAX_ARGS_DEPTH: usize = 32;

/// Maximum JSON value count accepted in argument text (M0-TEST choice, not
/// a product default).
pub const MAX_ARGS_NODES: usize = 4_096;

/// Hard maximum argument text in bytes: the core M0 tool-argument assembly
/// budget. [`CompiledSchema::validate`] and [`parse_object`] accept caller
/// byte budgets only in `1..=MAX_ARG_BYTES`; an out-of-range budget is
/// invalid configuration, never silently narrowed to this maximum.
const MAX_ARG_BYTES: usize = Limits::M0_TEST_ARG_ASSEMBLY_BYTES;

/// Rejects caller byte budgets outside `1..=MAX_ARG_BYTES` as invalid
/// configuration.
fn check_arg_budget(max_bytes: usize) -> Result<(), AgentError> {
    if max_bytes == 0 {
        return Err(input_error("argument byte budget must be nonzero"));
    }
    if max_bytes > MAX_ARG_BYTES {
        return Err(input_error("argument byte budget exceeds the hard maximum"));
    }
    Ok(())
}

/// Strictly parses object-root argument JSON within a byte budget.
///
/// `max_bytes` must lie in `1..=MAX_ARG_BYTES`, the core assembly budget; an
/// out-of-range budget is invalid configuration and fails with
/// `ErrorCategory::InvalidInput` rather than being clamped. Malformed JSON,
/// trailing content, duplicate object keys, and non-object roots fail with
/// `ErrorCategory::InvalidInput`; byte, depth, and node exhaustion fail with
/// `ErrorCategory::ResourceLimit`. The returned [`serde_json::Value`] is the
/// plain parsed value (no canonicalization); this is the policy-inspection
/// path, while [`CompiledSchema::validate`] is the admission path.
pub fn parse_object(args: &str, max_bytes: usize) -> Result<serde_json::Value, AgentError> {
    check_arg_budget(max_bytes)?;
    if args.len() > max_bytes {
        return Err(limit_error("argument byte budget exhausted"));
    }
    let value = parse::parse_strict(args, MAX_ARGS_DEPTH, MAX_ARGS_NODES)
        .map_err(parse::ParseFault::into_argument_error)?;
    if !value.is_object() {
        return Err(input_error("arguments must be an object"));
    }
    Ok(value)
}

pub(crate) fn input_error(message: &'static str) -> AgentError {
    AgentError::new(
        ErrorCategory::InvalidInput,
        message,
        RetryGuidance::DoNotRetry,
    )
    .expect("static safe validation message builds")
}

pub(crate) fn limit_error(message: &'static str) -> AgentError {
    AgentError::new(
        ErrorCategory::ResourceLimit,
        message,
        RetryGuidance::DoNotRetry,
    )
    .expect("static safe validation message builds")
}

#[cfg(test)]
mod cov_lib_private {
    use nexus_core::{ErrorCategory, Limits, RetryGuidance};

    use super::{MAX_ARG_BYTES, check_arg_budget, input_error, limit_error};

    #[test]
    fn hard_arg_maximum_equals_the_core_m0_assembly_budget() {
        assert_eq!(MAX_ARG_BYTES, Limits::M0_TEST_ARG_ASSEMBLY_BYTES);
        assert_eq!(MAX_ARG_BYTES, 65_536);
    }

    #[test]
    fn check_arg_budget_accepts_exactly_one_through_the_hard_maximum() {
        assert!(check_arg_budget(1).is_ok());
        assert!(check_arg_budget(MAX_ARG_BYTES).is_ok());
        for budget in [0, MAX_ARG_BYTES + 1, usize::MAX] {
            let error = check_arg_budget(budget).expect_err("budget must be rejected");
            assert_eq!(error.category(), ErrorCategory::InvalidInput);
            assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
        }
        assert!(
            check_arg_budget(0)
                .unwrap_err()
                .message()
                .contains("nonzero")
        );
        assert!(
            check_arg_budget(MAX_ARG_BYTES + 1)
                .unwrap_err()
                .message()
                .contains("hard maximum")
        );
    }

    #[test]
    fn static_error_helpers_pin_their_categories() {
        let invalid = input_error("invalid");
        let limit = limit_error("limit");
        assert_eq!(invalid.category(), ErrorCategory::InvalidInput);
        assert_eq!(limit.category(), ErrorCategory::ResourceLimit);
        assert_eq!(invalid.retry(), RetryGuidance::DoNotRetry);
        assert_eq!(limit.retry(), RetryGuidance::DoNotRetry);
    }
}
