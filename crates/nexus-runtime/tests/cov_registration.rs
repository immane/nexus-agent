#![forbid(unsafe_code)]

//! Coverage hardening for `Runtime::try_new` registration.
//!
//! Exercises the public constructor boundary only: duplicate tool names,
//! non-M0 tool revisions, schemas outside the closed M0 subset, the provider
//! text/tool-call capability gates, and an accepted valid registration.
//! Deterministic: fixed inputs and minimal in-file doubles; no randomness,
//! threads, wall-clock reads, or tool dispatch.

use std::sync::Arc;

use nexus_core::{
    AgentError, ErrorCategory, Limits, M0_REVISION, ModelRequest, ProviderCapabilities,
    ProviderContext, ProviderEvent, ProviderPort, RetryGuidance, ToolCall, ToolContext, ToolId,
    ToolOutcome, ToolPort, ToolSpec,
};
use nexus_runtime::{Policy, Runtime, RuntimeConfig};

/// Open object schema: the smallest text the closed subset accepts.
const OBJECT_SCHEMA: &str = r#"{"type":"object"}"#;

/// Closed schema with one required string property.
const PATH_OBJECT_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}"#;

/// Closed empty object schema; proves duplicates are keyed by name, not by
/// descriptor equality.
const CLOSED_EMPTY_SCHEMA: &str = r#"{"type":"object","additionalProperties":false}"#;

fn config() -> RuntimeConfig {
    RuntimeConfig {
        limits: Limits::m0_test(),
        policy: Policy::m0_test(),
        has_approval_handler: false,
    }
}

fn assert_error(error: &AgentError, category: ErrorCategory, message: &str, case: &str) {
    assert_eq!(error.category(), category, "{case}");
    assert_eq!(error.message(), message, "{case}");
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry, "{case}");
    assert!(error.correlation().is_empty(), "{case}");
}

fn spec(name: &str, revision: u32, schema: &str) -> ToolSpec {
    ToolSpec::new(
        ToolId::new(name, revision).expect("valid tool id"),
        format!("{name} registration double"),
        schema,
    )
    .expect("object-root schema shape builds")
}

/// Minimal tool double: a fixed descriptor. `execute` is unreachable because
/// registration tests never dispatch a call.
struct StubTool {
    spec: ToolSpec,
}

impl ToolPort for StubTool {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn execute(&self, _call: &ToolCall, _context: &ToolContext) -> ToolOutcome {
        unreachable!("registration tests never execute a tool")
    }
}

fn tool_from_spec(spec: ToolSpec) -> Arc<dyn ToolPort + Send + Sync> {
    Arc::new(StubTool { spec })
}

fn tool(name: &str, revision: u32, schema: &str) -> Arc<dyn ToolPort + Send + Sync> {
    tool_from_spec(spec(name, revision, schema))
}

fn capabilities(text: bool, tool_calls: bool) -> ProviderCapabilities {
    ProviderCapabilities {
        text,
        streaming: false,
        tool_calls,
        structured_output: false,
        usage_reporting: false,
        max_context_items: None,
        max_output_bytes: None,
    }
}

/// Minimal provider double: fixed capabilities and an empty stream. The
/// constructor only reads capabilities, so the stream is never invoked here.
struct StubProvider {
    capabilities: ProviderCapabilities,
}

impl StubProvider {
    fn text_only() -> Self {
        Self {
            capabilities: capabilities(true, false),
        }
    }

    fn text_and_tools() -> Self {
        Self {
            capabilities: capabilities(true, true),
        }
    }
}

impl ProviderPort for StubProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities.clone()
    }

    fn stream(&self, _request: &ModelRequest, _context: &ProviderContext) -> Vec<ProviderEvent> {
        Vec::new()
    }
}

#[test]
fn duplicate_tool_names_are_rejected_by_name_not_descriptor() {
    let tools = vec![
        tool("alpha", M0_REVISION, PATH_OBJECT_SCHEMA),
        tool("beta", M0_REVISION, OBJECT_SCHEMA),
        // Same name, different valid descriptor: registration is keyed by
        // name, so the third tool is a duplicate.
        tool("alpha", M0_REVISION, CLOSED_EMPTY_SCHEMA),
    ];
    let error = Runtime::try_new(config(), Arc::new(StubProvider::text_and_tools()), tools)
        .err()
        .expect("duplicate tool names are rejected");
    assert_error(
        &error,
        ErrorCategory::InvalidInput,
        "duplicate tool registration",
        "duplicate names",
    );
}

#[test]
fn non_m0_tool_revisions_are_rejected() {
    let tools = vec![
        tool("alpha", M0_REVISION, OBJECT_SCHEMA),
        tool("beta", M0_REVISION + 1, OBJECT_SCHEMA),
    ];
    let error = Runtime::try_new(config(), Arc::new(StubProvider::text_and_tools()), tools)
        .err()
        .expect("non-M0 revisions are rejected");
    assert_error(
        &error,
        ErrorCategory::InvalidInput,
        "tool revision is not the M0 revision",
        "revision mismatch",
    );
}

#[test]
fn schemas_outside_the_closed_subset_are_rejected_at_registration() {
    let cases = [
        (
            "array root",
            r#"{"type":"array"}"#,
            "schema root type must be object",
        ),
        (
            "unsupported node type",
            r#"{"type":"object","properties":{"n":{"type":"integer"}}}"#,
            "schema type is unsupported",
        ),
        (
            "untyped node",
            r#"{"type":"object","properties":{"n":{}}}"#,
            "schema object must declare a type",
        ),
        (
            "$ref keyword",
            r##"{"type":"object","$ref":"#/definitions/x"}"##,
            "schema $ref is unsupported",
        ),
        (
            "duplicate key",
            r#"{"type":"object","type":"object"}"#,
            "schema contains a duplicate object key",
        ),
        (
            "malformed JSON",
            r#"{"type":"object",}"#,
            "schema is not valid JSON",
        ),
    ];
    for (case, schema, message) in cases {
        // Every case passes the `ToolSpec` object-root shape check: rejection
        // must come from registration compiling the schema.
        let shape_ok = spec("bad_schema", M0_REVISION, schema);
        assert_eq!(shape_ok.input_schema_json(), schema, "{case}");
        let error = Runtime::try_new(
            config(),
            Arc::new(StubProvider::text_and_tools()),
            vec![tool_from_spec(shape_ok)],
        )
        .err()
        .expect("schema outside the closed subset is rejected");
        assert_error(&error, ErrorCategory::InvalidInput, message, case);
    }
}

#[test]
fn provider_without_text_capability_is_rejected() {
    let tools = vec![tool("alpha", M0_REVISION, OBJECT_SCHEMA)];
    let provider = StubProvider {
        capabilities: capabilities(false, false),
    };
    let error = Runtime::try_new(config(), Arc::new(provider), tools)
        .err()
        .expect("text capability is required");
    assert_error(
        &error,
        ErrorCategory::UnsupportedCapability,
        "provider cannot generate text",
        "no text, no tool calls",
    );

    // The text gate is checked before the tool gate, so a tool-capable
    // provider that cannot generate text is still rejected.
    let provider = StubProvider {
        capabilities: capabilities(false, true),
    };
    let error = Runtime::try_new(config(), Arc::new(provider), Vec::new())
        .err()
        .expect("text capability is required regardless of tool support");
    assert_error(
        &error,
        ErrorCategory::UnsupportedCapability,
        "provider cannot generate text",
        "no text, tool calls",
    );
}

#[test]
fn tool_call_capability_is_required_only_when_tools_are_registered() {
    let tools = vec![tool("alpha", M0_REVISION, OBJECT_SCHEMA)];
    let error = Runtime::try_new(config(), Arc::new(StubProvider::text_only()), tools)
        .err()
        .expect("registered tools require the tool-call capability");
    assert_error(
        &error,
        ErrorCategory::UnsupportedCapability,
        "provider cannot call tools",
        "tools without tool calls",
    );

    // No tools registered: the text-only provider is accepted, because the
    // gate is on tools actually being registered, not on the capability bit.
    let (_runtime, mut streams) =
        Runtime::try_new(config(), Arc::new(StubProvider::text_only()), Vec::new())
            .expect("text-only provider with no tools is valid");
    assert!(
        streams.data.try_recv().is_err(),
        "fresh data channel carries no events"
    );
    assert!(
        streams.control.try_recv().is_err(),
        "fresh control channel carries no events"
    );
}

#[test]
fn valid_registration_is_accepted() {
    let tools = vec![
        tool("alpha", M0_REVISION, PATH_OBJECT_SCHEMA),
        tool(
            "beta",
            M0_REVISION,
            r#"{"type":"object","properties":{"text":{"type":"string","maxLength":64}},"required":["text"],"additionalProperties":false}"#,
        ),
    ];
    let (_runtime, mut streams) =
        Runtime::try_new(config(), Arc::new(StubProvider::text_and_tools()), tools)
            .expect("valid registration builds");
    assert!(
        streams.data.try_recv().is_err(),
        "fresh data channel carries no events"
    );
    assert!(
        streams.control.try_recv().is_err(),
        "fresh control channel carries no events"
    );
}
