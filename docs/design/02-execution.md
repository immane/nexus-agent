# Execution and Lifecycle

Status: Draft design. State names describe proposed semantics, not existing Rust enums.

## Startup

The critical startup path should perform only bounded local work: read and validate the selected configuration, establish runtime ownership and limits, initialize the terminal when applicable, and accept input.

Do not block the first interactive state on model discovery, credentials over the network, plugin processes, repository indexing, package installation, or loading all session history. Deferred initialization must report its actual readiness; a visible spinner with blocked input does not count as interactive startup.

Only the selected provider and enabled tools become operational on demand. Load a chosen historical session separately and bound the amount materialized in memory.

## Proposed Baseline

One active run is supported initially. A second submission receives an explicit busy result rather than silently replacing the active run or creating an unbounded queue. Broader concurrency requires a bounded scheduling design and updated contracts.

```text
Idle -> Preparing -> CallingModel
CallingModel -> Completed
CallingModel -> ValidatingTools -> AwaitingApproval -> ExecutingTool
ExecutingTool -> RecordingResult -> Preparing
Any active state -> cancellation, failure, or limit handling -> Finished
```

The runtime validates complete tool calls before dispatch. The baseline waits for a valid completed model turn before executing its tools. Partial argument previews are presentation data, not executable instructions.

Multiple calls from a turn execute in declared order initially. Safe bounded parallelism may be added after measuring a concrete workload; conflicting mutations must not race by default.

## Run Loop

1. Capture the effective configuration and validate the submission.
2. Construct context within limits, including enabled tool definitions and required provider continuation state.
3. Request a normalized model stream and forward bounded presentation deltas.
4. Accept a complete assistant turn or preserve an explicitly incomplete outcome.
5. Resolve each tool, validate its arguments, and check current authorization.
6. Obtain approval if required; record side-effecting intent when persistence requires it.
7. Execute within limits, record the actual outcome, and append the tool result.
8. Continue the model loop or finalize the run exactly once within the live runtime lifecycle.

Context reduction must not silently discard required tool-call associations or provider continuation data. If the selected policy cannot fit a valid request, report a limit failure instead of sending corrupted history.

## Cancellation and Deadlines

Control traffic must remain responsive under heavy output. Cancellation stops new dispatches and signals active work. Late events remain associated with their original run and cannot modify a replacement run.

Cancellation is not rollback. A completed write remains a completed write even if the run is cancelled afterward. An interrupted operation with uncertain effects must not be automatically retried.

Cooperative cancellation does not preempt arbitrary blocking code. Built-in tools must use bounded operations or independently terminable work. External process termination must follow platform-specific ownership rules; detached descendants may require reconciliation.

Do not start conflicting work while an earlier operation may still be running. If termination cannot be established, report the uncertainty and block unsafe reuse rather than claiming cleanup succeeded.

## Output and Backpressure

Use bounded event buffers and batch text deltas without changing content or ordering. Keep control handling independent from saturated output delivery. Define a bounded failure path for disconnected or persistently stalled consumers; do not deadlock the agent loop on a terminal event send.

The frontend updates its own state and renders at a controlled rate. It should not trigger full-history processing for every delta.

## Shutdown and Recovery

Stop accepting submissions, cancel active work, reconcile child processes, flush required records, and restore terminal state. Cleanup failures must not prevent terminal restoration or be reported as successful rollback.

After an unexpected process exit, recovered runs are interrupted, not automatically resumed. Resolve outstanding tool effects before any replay. Normal lifecycle guarantees do not imply an event was delivered before a crash.

## Related Documents

- [Commands and events](../contracts/03-command-events.md)
- [Tool contract](../contracts/02-tool.md)
- [Session store](../contracts/04-session-store.md)
- [Configuration](../contracts/05-configuration.md)
