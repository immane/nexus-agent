# Model Integration

Status: Draft design. All protocol families below are coverage targets, not implemented or verified integrations.

## Protocols Before Vendor SDKs

Use a small set of adapters sharing HTTP, streaming, and serialization infrastructure. A provider profile selects its protocol, endpoint, model identifier, authentication source, and compatibility settings.

| Protocol family | Intended coverage |
| --- | --- |
| OpenAI Chat Completions-compatible | Services and local endpoints implementing the relevant chat/tool protocol |
| OpenAI Responses | Responses events, function calls, and continuation requirements |
| Anthropic Messages | Content blocks, tool use/results, and Messages streaming |
| Gemini native APIs | Native content/function representations and continuation requirements |

Do not create a dependency on a vendor SDK merely to serialize a small protocol. A dedicated SDK remains an option when it provides a demonstrated correctness or maintenance benefit. Pi's unified `pi-ai` layer with dynamic key resolution is a supporting precedent for one shared protocol/HTTP infrastructure with per-profile credentials. See the [Pi reference](10-pi-reference.md).

Cloud platforms with additional signing, routing, or authentication requirements may need separate adapters. A configurable base URL alone does not make those platforms compatible.

## Authentication and Relay Plugins

The first release uses API configuration with external credential references, such as explicitly named environment variables. Browser account login/OAuth is deferred. Do not resolve credentials for every profile, prompt for account login before rendering, or contact endpoints during ordinary startup.

An API relay is a provider plugin using the same normalized request/stream contract, not a tool offered to the model. First integrate existing relay services through explicit endpoint/protocol/model profiles and isolated credential references. Reuse shared protocol clients rather than starting a local proxy or importing a new SDK per relay.

Keep an optional boundary for later local forwarding, routing, or protocol conversion. Local relay execution must be on demand, bounded, and absent from the default startup path. Tool-oriented MCP transport is not a provider-plugin protocol. See the [relay design](08-api-relay.md).

## Capability Model

Resolve capabilities for the configured adapter/profile/model combination. Streaming, tool calls, structured outputs, multimodal inputs, usage reporting, and context limits are distinct capabilities.

Configuration may provide conservative overrides. Network discovery must be optional and deferred. Unknown capabilities must not be assumed available just because an endpoint accepts a familiar request shape.

Unsupported required capabilities produce explicit failures. Any fallback must be selected deliberately, preserve tool validation, and avoid duplicate operations.

## Normalized Streams

Adapters assemble bounded protocol fragments, preserve item and call identity, and emit the [provider contract](../contracts/01-provider.md). Transport chunks are not necessarily UTF-8, JSON, SSE, or tool-call boundaries.

Only complete, valid model turns become accepted conversation records. Visible partial text can remain in the frontend as incomplete output without being labeled a completed answer.

Tool schemas remain host-validated. Provider-specific schema conversion must not silently weaken runtime validation or advertise a tool the adapter cannot represent correctly.

## Continuation Fidelity

Preserve original tool-call references and all protocol-required continuation data. Some APIs require opaque reasoning items, signatures, or server-side continuation identifiers across turns.

The core may retain bounded, adapter-scoped continuation data without interpreting it. Adapters must preserve its semantics and reject incompatible reuse. Context compaction and provider switching require an explicit policy; they must not silently manufacture a valid history after dropping required state.

Opaque data is neither a permission grant nor user-facing output. Apply privacy and retention rules before storing or forwarding it.

## Testing and Coverage Claims

Use recorded synthetic fixtures for fragmented streams, multiple calls, malformed arguments, truncation, usage, errors, and continuation round trips. Keep fixtures free of credentials and private conversation data.

Live compatibility checks require explicit authorization and record adapter version, endpoint family, model, capabilities, and tested scope. Do not describe an entire compatible ecosystem as supported after testing a single service.

## References

Consult current official documentation when implementing a selected API version:

- [OpenAI API documentation](https://developers.openai.com/api/docs)
- [Anthropic API documentation](https://platform.claude.com/docs/en/api/overview)
- [Gemini API documentation](https://ai.google.dev/gemini-api/docs)

## Related Documents

- [Common contract](../contracts/00-common.md)
- [Provider contract](../contracts/01-provider.md)
- [Performance](05-performance.md)
- [API relay](08-api-relay.md)
- [First-release defaults](decisions/01-first-release-defaults.md)
