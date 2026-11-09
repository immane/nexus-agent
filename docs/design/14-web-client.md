# Built-in web client

Status: **partial, implemented**. This document describes the embedded browser
client in `crates/nexus-server/web/`. It is a live-session client, not a complete
web workspace, and it does not extend the server's trust boundary.

## Scope

The client supports:

- same-origin connection to the loopback server and model discovery from
  `GET /config`;
- selecting a configured provider/model, or the test-only demo script when no
  provider is configured;
- creating one runtime session on the first accepted message;
- submitting runs, streaming events with SSE, and rendering assistant text,
  context notices, tool activity, usage, approvals, cancellation, and terminal
  outcomes;
- reconciling an uncertain stream failure with `GET /snapshot`;
- five color palettes with the selected palette kept in `localStorage`.

The client does not implement session listing, transcript restore, multi-session
switching, file browsing, diffs, Markdown, syntax highlighting, background
polling, service workers, or a release asset-compression pipeline.

## Boundaries

```text
Browser: presentation, drafts, in-memory token, selected theme
   |  same-origin HTTP + SSE
nexus-server: authentication, sessions, snapshots, configuration
   |  existing runtime command/event port
nexus-runtime: execution, approvals, policy, persistence
```

The browser never receives provider credentials or tool authorization
decisions it can grant locally. Approval cards carry only the exact identities
published by the runtime; the server still validates the tuple.

The static shell (`/`, `/assets/app.css`, `/assets/app.js`) is public by design.
All API routes keep their existing bearer, Host, and Origin checks. The shell
contains no runtime data or credential values.

## Assets and rendering

Assets are fixed paths embedded into the binary with `include_bytes!`. Paths are
matched exactly; there is no filesystem lookup, directory listing, or MIME
guessing.

The shell uses:

- native HTML, CSS, and ES modules with no third-party runtime;
- a same-origin CSP with no inline scripts or styles and `frame-ancestors
  'none'`;
- text nodes for all model, tool, approval, and error content (never
  `innerHTML`);
- `fetch` with `ReadableStream` instead of `EventSource`, because the bearer
  header cannot be attached to a native `EventSource`;
- batched assistant-text appends on animation frames;
- bounded display sizes (200,000 characters per assistant item and 64,000
  characters per tool item) with explicit truncation notices.

Successful tool cards collapse after completion; failed or interrupted results
stay expanded. Markdown and code highlighting are intentionally absent from
this slice.

## Sessions and authentication

`POST /sessions` is called lazily on the first send. The provider/model choice
is fixed at session creation, matching the server contract. The page keeps the
session token in memory only; reloading loses the transcript and the session
view. Server idle cleanup can later remove the ephemeral runtime. Reloading or
closing the page does not cancel an active run.

The bearer token is entered manually in Settings. It stays only in the page's
JavaScript memory and is sent in the `Authorization` header for API calls and
SSE reads. It is never placed in a URL, cookie, `localStorage`, or
`sessionStorage`. Disconnecting or receiving `401` clears it. The palette
preference is the only persisted browser state.

The server permits one SSE subscriber per session. A second tab or client that
attaches while another is streaming receives `409`; the UI shows that conflict
and does not treat it as a run failure. Disconnecting an SSE read does not cancel
the run.

## Event handling

The client consumes the existing event JSON without inventing additional wire
contracts:

| Event | Presentation |
| --- | --- |
| `run-started` | Running status |
| `assistant-text` | Incremental assistant text, keyed by turn and item |
| `reasoning-redacted` | Running status; no hidden content |
| `model-request-started/finished` | Waiting/status transitions |
| `model-response-discarded` | Warning notice |
| `context-reduced` | Informational notice |
| `tool-call-preview` | Preparing-tool status |
| `tool-started` | Tool card with safe arguments preview |
| `tool-output` | Incremental bounded output |
| `tool-finished` | Typed status, effect, evidence, output, truncation |
| `approval-required` | Approval card with exact approval/call identities |
| `usage` | Inline usage counters; unknown stays unknown |
| `run-finished` | Terminal outcome and optional typed error |

Sequence numbers are tracked per run. Duplicate sequence numbers are ignored;
gaps produce an explicit incomplete-transcript notice. A terminal event always
marks the run finished even if another UI update is in flight. Late cancellation
or approval responses cannot overwrite a terminal status.

Approvals are sent only with the identities from the live event. The optional
`session-directory` scope is offered only when the runtime published a directory.
After a decision request whose acceptance could be uncertain, the card's
controls stay disabled to prevent a duplicate grant. A known `401` rejection is
the exception: it did not apply a decision, so controls become available again
after reconnecting. A run that finishes before a decision disables the
remaining approval controls.

## Recovery and uncertainty

The server stream is not a durable replay log. On a stream failure the client:

1. retries a read-only SSE attachment at most twice with a short backoff;
2. queries the run snapshot;
3. if active, keeps the run active and offers an explicit reconnect;
4. if finalized, renders the snapshot outcome and marks that the terminal event
   was not received;
5. if the run is evicted or the snapshot cannot be read, shows an unknown state
   and blocks sending rather than assuming completion or failure.

Submission, approval, cancellation, and stream errors are not retried blindly.
A network failure after a submit request may mean the server accepted a run; the
UI says the outcome is unknown and does not silently submit again. Starting a
new session never claims to cancel work that may still be executing.

## Known limits

- No source-history API is consumed, so a reload cannot reconstruct a
  transcript. Snapshot recovery exposes final state, not lost content.
- One SSE subscriber per session means multi-tab use can conflict.
- Retained event queues are bounded; saturated or evicted content is reported
  as incomplete, never reconstructed.
- The demo wiring is test-only; no provider behavior is simulated beyond what
  `nexus-server` serves.
- Assets are served uncompressed. Release-size and transfer-size claims require
  a separate measured compression step.
- Browser support targets current evergreen engines; no compatibility matrix or
  accessibility audit has been completed.

## Validation

`cargo nextest run -p nexus-server --locked --offline` covers the embedded
assets, Host/Origin checks, API authentication, SSE flows, approvals, and
snapshot contracts. Browser checks during development covered model discovery,
demo and authenticated sessions, streaming text, tool cards, approve/deny,
cancellation, stream-loss retry and snapshot recovery, theme switching,
disconnect/reconnect, and desktop/mobile layout. These are local functional
checks, not a security certification or browser compatibility claim.
