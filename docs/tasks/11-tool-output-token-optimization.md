# Tool-output and cumulative token-cost optimization

Date: 2026-11-11.

Status: follow-up proposal requested by the owner. Eager old-output previews are
implemented in the current working tree; the additional changes below are not.
This document does not authorize implementation, live inference, external costs,
configuration changes, or another server restart.

## Scope and evidence

Reduce repeated transmission of large tool results without losing execution
evidence or encouraging unsafe command retries. Context-window capacity,
cumulative input tokens, billed cost, and latency are separate objectives.

The inspected cancelled Web session contained 88 tool calls, 336,496 tool-result
bytes, 100,052 reasoning bytes, and 55,915 argument bytes. One failed PHP result
contained 131,092 bytes and 494 occurrences of `Deprecated`; it was followed by
16 more tool calls. These are archived payload bytes, not provider token counts.
The external screenshot's 116 requests and 4.2M cumulative input were not proven
to have exactly the same session scope.

An offline counterfactual projection shortened 27 older results: result-body
bytes fell from 336,496 to 89,421 (73.4%), and the largest result fell to 2,277
bytes. This is not measured token, billing, cache, or repair-quality improvement.
The local source artifacts are not portable published benchmark evidence.

Related documents:

- [Context management](../design/12-context-management.md).
- [Small-context optimization](10-small-context-optimization.md).
- [Development tests](../testing.md).

## Current implementation boundary

`crates/nexus-runtime/src/runtime/context_compaction.rs` applies deterministic
previews before pressure thresholds, including unknown context windows:

- Older completed result bodies retain at most 2,048 bytes of UTF-8-safe
  head/tail excerpts plus an omission and uncertainty notice.
- The latest complete tool group normally remains full. Existing emergency
  headroom handling can shorten it when necessary.
- Original records, exact call arguments, required reasoning, pairing,
  identities, and status/effect/evidence are preserved. Opaque continuations and
  incompletely paired histories are not rewritten.
- Candidate selection, protected-floor checks, summary acceptance, and dispatch
  estimates use projected costs. Summary content still comes from original
  removed sources; emergency previews start from raw requests to preserve
  original truncation provenance.
- Previews require no extra model request. They do not add a model-facing archive
  retrieval tool or remove existing persistence/history bounds.

Validation of this working-tree change: 675 affected runtime/headless/integration
tests passed, four were skipped under the default configuration; scoped Clippy
with `-D warnings`, formatting, and diff checks passed. The debug server was
rebuilt with container support and restarted using the same configuration and
session store; its health check passed. No matched live cost evaluation was run.

## Reference-agent findings

These observations describe OpenCode V2 documentation and Pi/Codex main branches
inspected on the date above, not every installed release or provider.

| Agent | Output handling | Context and cost handling |
| --- | --- | --- |
| OpenCode V2 | Configurable line/byte bounds; documentation example uses 2,000 lines / 50 KiB. Shell output can be shortened with complete output in managed storage; reads are paginated. | Summary plus a recent token-budget tail (15k by default); current compaction guide gives a 15% buffer with a 16k minimum. Summary requests reuse normal prompt prefixes where possible. Supported OpenAI Responses routes can opt into native compaction. Code Mode can filter intermediate results before returning them to the model. |
| Pi | Default 2,000 lines / 50 KiB; head truncation for reads, tail truncation for Bash. Truncated Bash output has a full-output temporary-file reference. | Default reserve 16,384 tokens and recent retention 20k. Summary serialization truncates tool results to 2,000 characters and omits thinking blocks. Optional Code Mode/tool discovery reduces intermediate output and eager schema exposure. |
| Codex | Byte or approximate-token budgets, including middle omission. Live-history truncation is separate from original rollout payloads. | Configurable tool-history and auto-compaction budgets, local/remote compaction paths, and provider usage plus estimates for unreported additions. Eligible WebSocket requests reuse prior response state for incremental transport. |

Codex's unified-exec default return budget of 10,000 tokens is not a universal
history limit: execution return budgets, model truncation policies, and
`tool_output_token_limit` are different controls. Pi's 2,000-character summary
serialization limit is not its normal tool-output limit. Omitting thinking from
summary serialization does not authorize dropping provider-required reasoning
from ordinary requests.

Native checkpoints are provider/model-bound and are not a generic replacement
for portable summaries. Code Mode and tool discovery availability/defaults vary.
Background summaries and cache warming add requests and can increase cost when
their results are unused; neither is proposed as a default here.

## Proposed priorities

### P0: Retrieve archived output without re-execution

Reuse source-record identities and existing storage boundaries. Return an opaque
output reference, original size, truncation/completeness information, and outcome
metadata alongside a bounded preview. Add bounded search and range reads over
the original captured result, with explicit missing/expired outcomes.

Retrieval must enforce session/run authorization, work across restart when the
underlying record is durable, and never expose arbitrary host paths. Specify
retention and byte limits; do not promise a complete process log when capture
was already truncated. A current file reread is not historical evidence, and an
unknown-effect command must never be rerun just to recover omitted output.

### P0: Bound first exposure by tool type

Old-output previews reduce replay, but do not prevent the first large result
from entering a request. Separate capture/storage limits from model-visible
return budgets, reusing existing tool controls where possible:

- Reads: explicit ranges and byte-bounded pages with continuation information.
- Searches: bounded matches, per-line length, and surrounding context.
- Tests/builds/commands: bounded diagnostic excerpts and terminal summary,
  original exit/timeout/cancellation metadata, and a retrievable output reference.
- Mutations: preserve operation evidence and uncertain effects, not just a
  generic success sentence.

Do not prescribe one universal budget before evaluating information loss. Head,
tail, and selected-diagnostic policies need tool-specific tests.

### P1: Preserve reusable request prefixes

Audit the actual provider-encoded request, not just internal message sizes.
Keep stable instructions and tool ordering stable; check whether changing budget
notes or historical previews invalidates otherwise reusable prefixes. Consider
stable, deterministic projection boundaries without repeatedly rewriting older
content. Respect provider continuation and reasoning-binding constraints.

An output's transition from full content to a preview changes the prefix. Measure
that cache tradeoff rather than assuming fewer bytes always means a cheaper
request. Incremental transport reduces transmission, not necessarily billed
context or occupancy. Provider cache support is adapter-specific.

### P1: Observe actual usage and cost

Correlate requests by run/turn and purpose: task, summary, retry/recovery, and any
future warming. Distinguish latest-request usage, cumulative usage, and active
occupancy estimates. Record provider-reported input, cache read/write, output,
and reasoning where available; unknown counters remain unknown.

Normalize provider accounting before computing uncached input or cost: cached
tokens may already be included in input. Use applicable prices and retain missing
price/accounting uncertainty. Diagnostics must not contain private payload text.

### P2: Deterministic filtering and cost-aware compaction

Deduplicate repetitive warnings into bounded representative diagnostics with
counts and original-output references. Preserve distinct failures and exit
metadata; do not suppress all deprecations or infer success from a quiet preview.
Prefer host-side structured processing over another LLM summary for each result.

Only after measurement, consider a working-context cost target independent of
the advertised maximum window. Earlier compaction should account for summary
input/output, cache invalidation, likely future reuse, and repair quality. Do not
simply lower the threshold or disable thinking globally. Lazy schema discovery
is useful if tool definitions are a demonstrated material cost, not a reason to
introduce a new orchestration subsystem now.

## Validation and acceptance

Use deterministic local fixtures first: warning floods, Unicode, one oversized
line, failures hidden in the middle, parallel tool groups, incomplete capture,
unknown effects, unavailable output references, unauthorized retrieval, durable
restart, opaque continuations, and required reasoning. Preserve existing source
integrity and call/result pairing assertions. Follow affected-package sequential
Cargo validation in [testing.md](../testing.md).

Compare baseline and candidates on the same captured histories for payload and
projection behavior. Separately evaluate authorized matched live tasks using the
same model/provider/settings and report all attempts, summary/retry calls, actual
usage, cache hits, estimated cost, latency, completion, and repair correctness.
Offline replay cannot establish the model's changed trajectory or actual cache
hits. Include log-middle recovery so savings cannot be obtained by silently
making necessary evidence inaccessible.

Accept a candidate only when evidence remains recoverable within explicit
retention bounds, unsafe repeats do not increase, and measured cost improves
without unacceptable correctness or latency regressions. No universal percentage
savings target is justified by the current single-session simulation.

## Sources and version notes

- OpenCode V2: [tools](https://opencode.ai/v2/docs/tools),
  [configuration](https://opencode.ai/v2/docs/config),
  [compaction](https://opencode.ai/v2/docs/compaction),
  [prompt caching](https://opencode.ai/v2/docs/models#prompt-caching).
  The inspected Context7 excerpt differed from the live compaction page on the
  default buffer; the table above follows the live V2 page. Recheck before
  translating these observations into configuration advice.
- Pi snapshot: `4e2a11dec29ae70cf4cc1dd221283d82004f47ba`:
  [truncation](https://github.com/earendil-works/pi/blob/4e2a11dec29ae70cf4cc1dd221283d82004f47ba/packages/coding-agent/src/core/tools/truncate.ts),
  [Bash](https://github.com/earendil-works/pi/blob/4e2a11dec29ae70cf4cc1dd221283d82004f47ba/packages/coding-agent/src/core/tools/bash.ts),
  [compaction](https://github.com/earendil-works/pi/blob/4e2a11dec29ae70cf4cc1dd221283d82004f47ba/packages/coding-agent/docs/compaction.md),
  [CLI/Code Mode](https://github.com/earendil-works/pi/blob/4e2a11dec29ae70cf4cc1dd221283d82004f47ba/packages/coding-agent/docs/cli.md).
- Codex snapshot: `e58bf8f5233b51e7cd1679d947e3028c1ed91b06`:
  [truncation](https://github.com/openai/codex/blob/e58bf8f5233b51e7cd1679d947e3028c1ed91b06/codex-rs/utils/output-truncation/src/lib.rs),
  [history](https://github.com/openai/codex/blob/e58bf8f5233b51e7cd1679d947e3028c1ed91b06/codex-rs/core/src/context_manager/history.rs),
  [execution budgets](https://github.com/openai/codex/blob/e58bf8f5233b51e7cd1679d947e3028c1ed91b06/codex-rs/core/src/unified_exec/mod.rs),
  [client](https://github.com/openai/codex/blob/e58bf8f5233b51e7cd1679d947e3028c1ed91b06/codex-rs/core/src/client.rs).
