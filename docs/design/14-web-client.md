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
- selecting an admitted local project root when real local tools are enabled;
- adding and removing project roots through the authenticated API;
- adding, updating, or deleting provider entries; updating a stored provider
  token through the local credentials file; and adding or updating model
  entries using credential references only;
- creating one runtime session on the first accepted message;
- submitting runs, streaming events with SSE, and rendering assistant text,
  context notices, tool activity, usage, approvals, cancellation, and terminal
  outcomes;
- reconciling an uncertain stream failure with `GET /snapshot`;
- five color palettes with the selected palette kept in `localStorage`.

The client supports saved-session listing and restore when the server enables
persistence. It does not implement concurrent multi-session editing, file
browsing, syntax highlighting, background polling, offline API access, or a
release asset-compression pipeline. Its Service Worker is network-only.

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
published by the runtime; the server still validates the tuple. Configuration
forms submit credential references (an environment-variable name or stored-entry
name), never secret values. Project paths are returned only to the authenticated
page and only while local real tools are enabled.

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
  `innerHTML`); the Markdown renderer builds DOM nodes and never parses raw HTML;
- `fetch` with `ReadableStream` instead of `EventSource`, because the bearer
  header cannot be attached to a native `EventSource`;
- throttled assistant Markdown re-rendering while streaming, with a plain-text
  fallback above 40,000 characters and a final Markdown pass at completion;
- bounded display sizes (200,000 characters per assistant item and 64,000
  characters per tool item) with explicit truncation notices.

The built-in Markdown renderer supports headings, paragraphs, emphasis, strong
text, strikethrough, inline code, fenced code with a language label, ordered and
unordered lists, task markers, blockquotes, horizontal rules, links, autolinks,
and pipe tables. Link targets are limited to `http`, `https`, and `mailto`;
unsafe or malformed targets remain plain text. Raw HTML is displayed as text.
Images, footnotes, math, nested list structures, and syntax highlighting are not
implemented.

Successful tool cards collapse after completion; failed or interrupted results
stay expanded. The path-only `host_read` and `host_list` calls are suppressed
from the web transcript to avoid low-value read noise; their results still reach
the model. Other tools, approvals, usage summaries, context reductions, and
terminal errors remain visible.

## Sessions and authentication

New server session IDs use a 128-bit OS-random suffix (`sess-web-<hex>`), not a
counter reset at startup, so new sessions do not reuse earlier server identities
when opening the same persistent store. ID generation fails explicitly if the
system random source is unavailable. IDs remain opaque and are not credentials;
legacy counter-based IDs remain valid inputs.

`POST /sessions` is called lazily on the first send. The provider/model choice
is fixed at session creation, matching the server contract. The page remembers
the current session id in browser storage and asks `POST /sessions/{id}/restore`
to reopen it after a reload. Server idle cleanup can later remove the ephemeral
runtime. Reloading or closing the page does not cancel an active run, and an
active run is not reattached merely by restoring its saved history.

When the server starts with `--session-store DIRECTORY`, the sidebar lists saved
sessions from `GET /sessions`, reads bounded pages from
`GET /sessions/{id}/history`, restores context without replaying tools, and can
delete a non-live session with `DELETE /sessions/{id}`. Titles come from the
first saved user item. History rendering is bounded; filesystem effects and old
approval grants are never replayed. The provider/model/project binding is kept
in browser storage because the session checkpoint format does not carry Web
bindings; a different browser must select valid bindings before restoring.
Without `--session-store`, the list reports persistence as disabled.

The bearer token is entered in Settings and saved in same-origin `localStorage`
after a successful connection. The page reuses it on launch, sending it only in
the `Authorization` header for API calls and SSE reads, never in a URL or cookie.
Disconnect removes it, and a `401` clears the matching saved token. Network errors
do not erase it. If browser storage is blocked, the connection remains usable in
memory and the settings show that it could not be saved. The palette preference
is also persisted. Same-origin scripts can read the saved token; use only a
trusted browser profile. Different hostnames, ports, profiles, or isolated app
storage may require connecting once again.

The server permits one SSE subscriber per session. A second tab or client that
attaches while another is streaming receives `409`; the UI shows that conflict
and does not treat it as a run failure. Disconnecting an SSE read does not cancel
the run.

## Installed web app

The composer shows animated activity bars and a breathing outer border during
submission and active runs. Both stop on completion or disconnection; reduced
motion preferences disable the animation while keeping the activity indicator.

The page links a Web App Manifest with 192px and 512px icons and registers a
root-scoped Service Worker. The worker handles navigations only, fetching them
from the network; it never caches API responses or SSE frames. Installation is
therefore a standalone-window and launcher affordance, not an offline mode.

Chromium browsers may offer an install action and fire
`beforeinstallprompt`; Settings → About exposes a matching button. Other
browsers use their own Install or Add to Home Screen action. The local server
must still be running after launch. The installed app reuses its saved bearer
token where browser storage is shared, or requires connecting once if isolated.

## Projects and runtime mode

The operator must start the server with local real tools
(`--tools real --tools-root PATH`, plus `--auth-token-env`). The web client cannot
turn the scripted demo into real tools. `GET /projects` reports the mode:

- `development`: project/temp file operations and sandboxed commands are
  automatic; external file paths still require approval;
- `strict`: project reads are jailed and mutations/commands require approval;
- `fakes` and `container`: project selection is unavailable and no project paths
  are reported.

Project selection is part of the left sidebar, with the active project
highlighted. Selecting a different project while a session exists starts a new
session after confirmation; the previous session remains available in saved
history when persistence is enabled. The Settings
dialog manages project admission/removal, and a sidebar `+` shortcut opens that
section. On narrow screens the sidebar collapses and the composer exposes an
equivalent project selector.

The startup tool root is exposed as the reserved `default` project. Additional
projects are existing absolute directories admitted by `POST /projects`. Before
admission the server canonicalizes the path and refuses the filesystem root, the
user's home or any ancestor of it, common system roots, and known credential
directories. This is not secret detection; a project may still contain sensitive
files.

Project metadata is persisted in `projects.json` next to the user configuration,
with owner-only permissions on Unix. A missing configuration path keeps project
edits in memory only. `DELETE /projects/{id}` refuses the startup project and a
root still referenced by a retained session; existing sessions otherwise keep
their already-bound tool registrations. A session's project is fixed when the
session is created. The client sends the selected project id in
`POST /sessions`.

## Provider and model configuration

Settings uses a wide dialog with category navigation for Appearance, Connection,
Thinking, Projects, Providers, Models, and About. On narrow screens the navigation becomes
a horizontally scrollable row. Switching categories preserves unsaved form values.
About displays the running server's package version from the additive `version`
field in `GET /config`; it is not the configuration schema's `revision`.

The Settings dialog can add or replace entries through
`POST /config/providers` and `POST /config/models`. These writes require a
configured server bearer token even when tools are scripted; an unauthenticated
demo server refuses them. Provider credentials are references: an
environment-variable name or a stored-entry name. The configuration document
itself has no field for a secret value.

For a stored reference, the optional `api_token` field on
`POST /config/providers` writes the value to `credentials.json` beside the
server's configured document. The file is parsed strictly before replacement,
written atomically with owner-only permissions on Unix, and never returned by
the API. An empty or control-character value is refused. Tokens require a stored
reference matching the provider id. The browser uses a password field that is
cleared after a successful save and never populated from server state.

`DELETE /config/providers/{id}` removes an unreferenced provider and, when it
owned the matching stored credential, removes that entry. A provider still named
by a model is a conflict. Model removal is not provided through the web API;
removal remains an explicit configuration-file edit.

The provider form also exposes an Enabled/Disabled status. Disabled providers
remain listed and configured, but their models are omitted from new-session
choices and the server refuses a direct selection with a static error. The
summary reports the effective flag. Existing sessions keep their bound adapter.

Updates retain optional context limits when the form leaves them untouched.
Model defaults and provider ownership are validated by `nexus-config`; an
unknown provider, dangling reference, invalid endpoint, or inconsistent context
window is refused with a static error. A provider replacement revalidates models
that inherit its default-model window. Successful writes use the normal atomic
configuration save.

Thinking exposes a global task-generation default through authenticated
`POST /config/thinking`, with optional provider/model overrides in their existing
forms. Choices are Default (inherit), On (low effort), and Off. Precedence is
model, provider, then global; all inherited defaults preserve existing behavior.
Only detected Ollama Direct / Chat Completions adapters currently support an
effective explicit override. Other adapters reject it instead of silently
ignoring it. Low effort may merely enable thinking on boolean-only models.
This control does not show/hide reasoning, remove historical traces, or change
summary thinking. See the [configuration contract](../contracts/05-configuration.md#provider-task-reasoning-effort).

Changes apply to new sessions. A live session keeps the provider adapter and tool
registrations created for it. Provider deletion does not affect sessions that
are already bound.

## Event handling

The client consumes the existing event JSON without inventing additional wire
contracts:

| Event | Presentation |
| --- | --- |
| `run-started` | Running status |
| `assistant-text` | Incremental assistant text, keyed by turn and item |
| `assistant-reasoning` | Live thinking, keyed by turn and item; collapsed last-line preview, expandable scrolling text |
| `reasoning-redacted` | Compatibility status for older servers; no hidden content |
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

Live thinking is sent as plain text through the same authenticated run event
stream as assistant text. Each thinking item starts collapsed and follows its
last wrapped line. Expanding reveals the received text in a bounded scrolling
panel, which follows new output unless the reader scrolls up. Display is capped
at 200,000 characters per item with an explicit truncation notice. The display
does not enable model thinking or expose opaque provider state. Saved-history
responses still omit reasoning text; restoring a session does not restore these
live thinking panels.

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

- Saved history is available only when the server starts with
  `--session-store`; an active run is not reattached after reload. Snapshot
  recovery exposes final state, not lost event content.
- Web bindings are stored in the browser, not the server checkpoint. A different
  browser must select valid provider/model/project bindings before restoring.
- One SSE subscriber per session means multi-tab use can conflict.
- Retained event queues are bounded; saturated or evicted content is reported
  as incomplete, never reconstructed.
- Projects require local real tools. Container-mode per-project roots are not
  implemented, and the client does not browse a project's files.
- A project cannot be removed while a retained session still references it;
  sessions may remain for the idle-cleanup window after a run finishes.
- Stored project roots are revalidated at startup; a missing directory prevents
  startup until the catalog is corrected.
- The startup `--tools-root` is an operator decision and is not subjected to the
  stricter web-addition path checks.
- Provider/model removal is not exposed over HTTP. Existing sessions are not
  updated by configuration edits.
- Markdown is a deliberately bounded subset. Images, footnotes, math, nested
  list structures, raw HTML, and syntax highlighting are not rendered.
- Very long assistant items switch to plain text while streaming and may take a
  noticeable moment to format when the final Markdown pass runs.
- The demo wiring is test-only; no provider behavior is simulated beyond what
  `nexus-server` serves.
- Assets are served uncompressed. Release-size and transfer-size claims require
  a separate measured compression step.
- Browser support targets current evergreen engines; no compatibility matrix or
  accessibility audit has been completed.

## Validation

`cargo nextest run -p nexus-server --locked --offline` covers the embedded
assets, Host/Origin checks, API authentication, SSE flows, approvals, snapshot
contracts, project admission/persistence/binding, and provider/model upserts.
Browser checks during development covered model discovery, demo and
authenticated sessions, streaming text, Markdown headings/emphasis/lists/
quotes/tables/code and safe links, manual scrolling to the end during and after
streaming, hidden path-only read tools, tool cards, approve/deny, cancellation,
stream-loss retry and snapshot recovery, theme switching, disconnect/reconnect,
project add/select/remove, provider/model editing, and desktop/mobile layout.
These are local functional checks, not a security certification or browser
compatibility claim.
