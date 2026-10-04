# Provider Contract

Status: Draft contract, revision `draft-0`. Read the [common contract](00-common.md) first. No provider trait or adapter currently exists.

## Semantic Operations

- `capabilities(selection)`: describe known capabilities without mandatory network discovery.
- `stream(request, context)`: perform one model turn and yield normalized events.

These are operation descriptions, not compilable Rust signatures. The adapter owns protocol parsing and connections; the runtime owns whether to invoke, retry, or continue the model loop.

## Request

`ModelRequest` contains the run/turn identity, selected model profile, accepted conversation, authorized instructions, enabled tool definitions, context/output budgets, and compatible continuation state.

`ProviderContext` contains cancellation, deadlines, and implementation-owned connection/credential access. Credentials MUST NOT become ordinary conversation fields or be passed to unrelated tools.

The runtime MUST validate required capabilities and context constraints before invocation. The adapter MUST reject an unsupported tool schema or capability explicitly. A provider without tool calling cannot silently execute a prompt-based imitation of structured calls.

## Capabilities

Distinguish text generation, streaming, tool calls, structured output, multimodal content, usage reporting, and known context/output limits. Unknown or estimated limits MUST be labeled as such; do not claim exact token counts from a generic estimate.

Capabilities apply to the selected adapter/profile/model combination, not an entire vendor ecosystem. Provider-hosted tools are outside the initial host-tool contract and MUST remain disabled unless separately authorized and specified.

## Normalized Events

| Event | Meaning |
| --- | --- |
| `TextDelta` | Ordered text fragment associated with a stable turn-local item key |
| `ToolCallDelta` | Optional progress for a proposed call, including bounded argument fragments |
| `ToolCallReady` | Complete parsed call candidate: item key, original provider reference, tool name, arguments |
| `Usage` | Available usage counters, explicitly provisional or final |
| `TurnFinished` | Complete normalized assistant turn, finish reason, final available usage, continuation data |
| `Failed` | Typed terminal failure for this invocation |

For a fully consumed invocation, the adapter MUST produce exactly one terminal event: `TurnFinished` or `Failed`, followed by no further events. An unexpectedly ended transport or invalid required fragment is a failure, not a successful turn. A caller that cancels or drops the stream must still finalize its run appropriately; a dropped stream cannot promise terminal delivery.

`TurnFinished` MUST agree with prior candidate identity and assembled content. Candidate progress never authorizes execution. The baseline runtime admits calls only after the complete turn is accepted, assigns host call identities, validates registered-tool arguments, and checks authorization.

## Assembly and Finish Reasons

Adapters MUST handle arbitrary byte/chunk boundaries, split UTF-8 and JSON, interleaved protocol items, multiple calls, and bounded argument assembly. Duplicate references, conflicting fragments, malformed JSON, or invalid protocol ordering MUST NOT result in dispatch.

Finish reasons distinguish an ordinary stop, tool calls, output limit, refusal, and incomplete/error outcomes. The runtime MUST not label truncated output as a complete successful answer. Missing usage may remain unknown; it cannot be fabricated as zero.

Batching adjacent text is allowed when content and order are unchanged. Avoid mandatory whole-message cloning or wire serialization at every event; the final ownership representation remains an implementation choice.

## Continuation and Switching

Original call references and required provider continuation items MUST survive the model/tool/model cycle. Opaque state is bounded, scoped to its adapter/profile/model compatibility, and versioned for persistence.

The core MUST NOT interpret opaque state as permissions or expose it as ordinary terminal text. An incompatible provider switch or context reduction requires explicit conversion or failure; dropping required state silently is forbidden.

## Errors and Retries

Surface safe authentication, rate-limit, transport, protocol, and capability errors. Any retry must obey runtime limits and policy.

After partial output, a retry MUST not silently concatenate a new attempt onto the previous turn, reuse ambiguous tool candidates, or replay already executed calls. Make the abandoned attempt and replacement turn explicit.

## Required Tests

Cover split UTF-8/JSON, malformed and oversized frames, multiple calls, duplicate references, truncated streams, refusals, usage timing, cancellation, incompatible continuation state, and successful continuation round trips for each supported protocol.

## Related Documents

- [Model integration design](../design/03-model-integration.md)
- [Tool](02-tool.md)
- [Session store](04-session-store.md)
