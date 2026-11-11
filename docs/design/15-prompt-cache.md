# Prompt Cache Stability

Date: 2026-11-12.

Status: owner-confirmed fix direction; offline repair implemented. Matched live
cache-rate comparison remains pending explicit authorization. This document
records the request-shape invariant, the diagnosed causes of low cache reuse,
the staged repair, and the validation needed before claiming a cache-rate
improvement. It makes no provider-compatibility or cost guarantee.

## 1. Problem and Target

A long session served by the same model and OpenCode Go endpoint should reuse a
large stable prompt prefix across turns. Observed console accounting for one
instrumented session showed roughly 6.6% cached input against an expected
steady-state range of 95%–99%. The target is a stable **weighted** cache ratio
(cache-read tokens divided by total input tokens) for ordinary long-session
turns, not a percentage for every request.

Cold starts, genuinely new large content, context reduction, recovery after a
truncated stream, and configuration changes may legitimately lower reuse.
Those requests must be identifiable instead of being averaged into a failure or
hidden behind a favorable total.

## 2. Request-Shape Invariant

Normal turns of a session must be append-only from the provider's perspective:

> Everything already sent as model input for a turn stays byte-identical on the
> next turn; new input is appended. Only an explicit context reduction,
> configuration change, protocol correction, or recovery may rewrite the older
> prefix.

Providers that cache automatically, including DeepSeek, reuse only an identical
prefix starting from the first token. No cache marker or key can repair a
changed prefix. Preserve correctness invariants while enforcing this rule:

- Do not remove required reasoning, call/result identity, or pairing data.
- Do not disable context limits, output reserves, cancellation, or compaction.
- Do not rewrite durable source records; projections affect only the model view.
- Do not report unknown cache counters as zero.
- Do not add provider-specific cache options without checking that the serving
  API accepts and needs them.

## 3. Diagnosed Causes

### 3.1 Dynamic estimate in the system prefix

`nexus-context::render_context_note` appended the current heuristic input
occupancy to the runtime note. With a discovered window configured,
`rendered_instructions` placed that changing number in the system message ahead
of the whole transcript. A fixed-width number is stable in length but not in
content, so the shared history behind it lost prefix matches on every turn.

Session logs confirm the shape: a 1,000,000-token window, empty host
instructions, a 321-byte runtime note on every dispatch, and estimates changing
from 1,972 to 116,351 tokens across 86 calls.

### 3.2 Age-based rewriting of complete tool results

Command output with archive retrieval is projected from its first request and is
therefore stable. Other tool results were shortened only after a newer group
arrived. That changed a prefix that had already been accepted. It also appeared
as shrinking aggregate bytes between adjacent turns, which an append-only
request cannot do.

### 3.3 Accounting and observability gaps

The adapter reads `prompt_tokens_details.cached_tokens` and
`cache_read_input_tokens` but not DeepSeek's `prompt_cache_hit_tokens` /
`prompt_cache_miss_tokens`. Local display can therefore undercount cache reads.
It does not explain low reuse reported by the service, so accounting is fixed
separately from the request shape.

There is no regression test asserting that two successive normal requests share
their system content, tool definitions, and older messages. Prefix drift was
therefore invisible to the suite.

## 4. Repair Stages

### Stage 1: remove changing content from the system prefix

Render only stable policy facts in the model-visible note: the known window and
the configured compaction policy. Keep per-turn estimates, occupancy
percentages, calibration, and headroom in runtime diagnostics. Budget
enforcement remains unchanged.

State that must legitimately change, such as "history was omitted" or an output
limit recovery notice, still changes the note when it occurs. It must not be
regenerated on every turn.

### Stage 2: stop age-based tool-output rewriting

Keep the existing first-exposure projection for archived command output. Do not
shorten a complete result merely because a newer tool group followed it. Normal
turns retain the full bounded capture already accepted from the tool. Explicit
compaction and emergency headroom paths may still project; those reasons are
recorded in diagnostics.

This may raise input usage and trigger compaction earlier. Validation must
compare total and uncached input, not only the cache ratio.

### Stage 3: request-prefix regression coverage

Compare the actual encoded provider request after `request_body`, not only
runtime domain objects: system content, ordered tool definitions, message shape,
call/result identities, required reasoning fields, and the Go session header.
Successive normal turns must be identical in the earlier portion. A failure
should identify the first changed section and whether the change was declared.

Cover normal text turns, reasoning and tool calls, multiple calls per turn,
large command/read/search results, retries, cancellation, context reduction, and
restored sessions. Do not compare whole JSON strings: wrappers and generation
parameters are not the cached model prefix.

### Stage 4: cache accounting and diagnostics

Prefer one counter for each meaning without double counting; total prompt
tokens may already include cached tokens. Read DeepSeek's `prompt_cache_hit_tokens`
as an alternate representation of the cache-read counter; native detail fields
take precedence and representations are never summed. Keep unknown counters
unknown. `prompt_cache_miss_tokens` is not yet carried by the shared `Usage`
type, so uncached input is only derived where the stored semantics are known.

Recording one final usage sample per model invocation with run, turn, input,
cache counters, output, finality, and a prefix-change reason remains proposed
diagnostics work. Such diagnostics must not contain prompts, tool bodies,
reasoning text, or tokens.

### Stage 5: verify OpenCode Go session stability

The adapter already sends a stable `User-Agent` and `x-opencode-session` for the
configured Go endpoint. Verify that every normal call in a session, including
retries and auxiliary requests, keeps that identity and that separate sessions
differ. Do not impersonate OpenCode or add unverified cache parameters.

## 5. Validation

Offline checks must establish:

- Per-turn estimates no longer alter normal system content.
- Already-sent tool results do not change on later normal turns.
- Tool pairing, reasoning replay, projection provenance, and source records are
  unchanged.
- Budgeting, compaction, cancellation, and recovery tests still pass.
- Unidentified prefix changes fail the new regression checks.

A paid live comparison needs explicit authorization. Use the same model,
endpoint, and task shape to compare per-request cache ratios, weighted steady
state, uncached input, cost, latency, compaction frequency, and task outcome.
Cold, recovery, and reduction requests are reported separately.

## 6. Implementation Notes (2026-11-12)

- `nexus-context::render_context_note` now emits only the known window and
  compaction policy. Per-turn estimates remain available in runtime budget
  diagnostics; the two-pass note estimation was removed because the note no
  longer depends on token counts.
- `project_tool_outputs_for_request` projects only archived command output, and
  always from its first exposure. Complete non-command results stay as accepted.
  Emergency headroom and explicit reduction paths are unchanged.
- The Chat Completions and Responses usage readers accept DeepSeek's
  `prompt_cache_hit_tokens` as an alternate cache-read counter. Native detail
  fields take precedence; representations are never summed.
- Regression coverage:
  `nexus-openai::provider::prompt_cache_tests` compares encoded system content,
  tool definitions, message prefixes, and replayed reasoning across growing
  transcripts for Chat Completions and Anthropic Messages;
  `nexus-context` asserts the note is stable and non-command request projections
  do not change; `nexus-runtime` asserts normal requests never rewrite prior
  tool output while explicit compaction still preserves pinned evidence.
- No live Go comparison has been run. The console's reported cache ratio must be
  re-measured on the same model, endpoint, and task shape before claiming the
  target range.

## 7. Non-Goals

- Cache markers or keys that the serving API does not need.
- Re-sending history only to raise a percentage.
- Weakening context limits, evidence retention, or execution guarantees.
- Treating provider cache reuse as a correctness or cost guarantee.
