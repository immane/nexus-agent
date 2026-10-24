# Provider Contract

Status: Draft contract, revision `draft-1`, with implemented subsets identified below. Read the [common contract](00-common.md) first. API configuration and relay scope are accepted in [Decision 01](../design/decisions/01-first-release-defaults.md). The Rust provider port and HTTP adapters exist; draft requirements beyond the implemented subsets are not implementation claims.

## Semantic Operations

- `capabilities(selection)`: describe known capabilities without mandatory network discovery.
- `stream(request, context)`: perform one model turn and yield normalized events.

These are operation descriptions, not compilable Rust signatures. The adapter owns protocol parsing and connections; the runtime owns whether to invoke, retry, or continue the model loop.

## API Profiles and Relay Plugins

Direct providers and API relay plugins implement the same semantic port. A relay MUST NOT become a model-invoked tool or bypass normalized stream validation, limits, or host policy.

First-release profiles use an explicit API endpoint, protocol/model selection, and external credential references. Resolve only credentials assigned to the selected integration when needed. Browser login/OAuth is outside initial scope, and neither authentication nor relay discovery may block interactive startup.

Existing relay services are the first path. Future local relay implementations remain optional and on demand; no local proxy is mandatory. Cross-language provider plugins need their own supported HTTP or versioned process interface, not an assumed Rust ABI or MCP tool protocol.

A relay MUST preserve call references and required continuation state, declare truthful capabilities, and isolate destination/credential selection. Do not silently forward a request to multiple destinations, merge retries into one turn, or reinterpret unsupported protocol state as valid history.

## Request

`ModelRequest` contains the run/turn identity, selected model profile, accepted conversation, authorized instructions, enabled tool definitions, context/output budgets, and compatible continuation state.

### Implemented token-budget metadata

`ProviderPort::context_management` returns local settings; the runtime snapshots
them for a run. The OpenAI-compatible adapter additionally makes one bounded,
best-effort read-only Ollama `/api/show` request for Chat Completions profiles
on port `11434` or a hostname containing `ollama`, when no configured window
exists. It prefers `parameters.num_ctx`, then a
`model_info.*.context_length` value. Such values are marked `ModelMetadata`, not
treated as exact active serving limits. Explicit configured windows take
precedence. Unsupported endpoints, protocols, and discovery failures leave the
window unknown; no model-name inference is performed. Other API protocols do not
define a common model-window discovery contract.

`ModelRequest::max_output_tokens` is an optional positive generation ceiling,
independent of `output_budget_bytes`. Chat Completions and Anthropic Messages
encode it as `max_tokens`; Responses encodes it as `max_output_tokens`. Without
an explicit ceiling, adapters retain the legacy byte-derived default. A token
ceiling does not disable response-byte validation or imply a successful finish.

`nexus_context::estimated_input_tokens` includes instruction/note payloads,
conversation and reasoning, call/result metadata, tool definitions, and framing
allowances. Its three-UTF-8-bytes-per-token heuristic is not a tokenizer or an
upper bound, and must not be emitted as provider-reported `Usage`. Runtime
headroom and calibration do not establish a guarantee for every tokenizer.

### Implemented instruction placeholder

The Rust API now carries a separate `ModelInstructions` value, with `agent` and
`environment` text supplied by the trusted application composition layer. Both
default to empty; no built-in coding rules, environment detection, file loading,
CLI configuration, or benchmark-specific commands are enabled by this API.
Task descriptions, repository files, and tool output remain conversation data;
they must not be promoted to host instructions merely because they contain
instruction-like text. Instructions do not grant tool permissions.

`Runtime::try_new_with_instructions` installs a fixed snapshot for that runtime's
lifetime and attaches it to every model request. Existing constructors retain
the empty default. Changing instructions or execution environment requires a new
runtime; there is no hot-update API. Applications must supply environment facts
appropriate to the actual backend rather than sending container paths in local
mode.

Instructions have a 16 KiB rendered UTF-8 limit, including the blank-line
separator between nonempty fields. Their bytes also consume the request's
1 MiB aggregate conversation/instruction budget, regardless of builder order.
Runtime history projection reserves these bytes before pruning whole exchanges;
instructions are not ordinary history items and cannot be evicted or cached as
an earlier task exchange. Reduction events' retained byte counts include the
reserved instruction bytes; item counts still describe conversation items only.

Runtime-generated context notes (for example, that earlier completed exchanges
were omitted) are also separate from conversation history and are sent through
the same native instruction field with an explicit `Runtime context note:`
label. They are bounded to 4 KiB of note text; the rendered label, separator,
host instructions, note, and conversation all count toward the same 1 MiB
request budget. Notes do not consume retained-item slots or become part of a
later cached exchange. If a note must be emitted because history was reduced,
its rendered bytes are reserved before eviction. Reduction-event retained byte
counts include it.

The adapter renders nonempty fields in agent/environment order. Chat Completions
uses a leading `system` message, Responses uses the top-level `instructions`
field, and Anthropic Messages uses the top-level `system` field. Empty
instructions omit those fields/messages, preserving existing wire behavior.
Adapters must encode instructions natively or explicitly reject them as
unsupported; silently dropping them or converting them to ordinary user text is
not permitted. This section describes implemented behavior; other draft
proposals in this document are not implied to be implemented.

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

Cover split UTF-8/JSON, malformed and oversized frames, multiple calls, duplicate references, truncated streams, refusals, usage timing, cancellation, incompatible continuation state, and successful continuation round trips for each supported protocol. Apply the same cases to relay profiles, including credential isolation and no initialization of unused endpoints or processes.

## Related Documents

- [Model integration design](../design/03-model-integration.md)
- [Tool](02-tool.md)
- [Session store](04-session-store.md)
- [API relay design](../design/08-api-relay.md)
