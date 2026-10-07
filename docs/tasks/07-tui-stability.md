# TUI Stability and Interaction Backlog

Status: Proposed implementation plan; not part of the M0 lock and not evidence
that any item below is implemented. This is a follow-up to the existing TUI,
not a change to runtime policy, tool contracts, or the M0 acceptance checklist.

## Goal

Make the existing Ratatui TUI feel responsive during editing, streaming, and
scrolling, then add bounded model selection and composer suggestions for `/`
commands and `@` paths. Borrow the interaction shape of OpenCode's searchable
dialogs and prompt autocomplete, not its keymap or broader product behavior.
Keep the current single-renderer architecture, composer-first workflow, exact
approval semantics, and terminal restoration guarantees.

The work is deliberately staged: measure before optimizing, stabilize the
render/input foundation before overlays, and keep filesystem discovery outside
the draw path.

## Current implementation observations

These are code-based hypotheses to validate with measurements, not established
user-facing performance claims:

- `crates/nexus-tui/src/main.rs`: `interactive_loop` consumes terminal input
  from `handle_key_batch` on the 50 ms timer tick. Input can therefore wait for
  a tick even when it is already available. Stream events can also trigger a
  draw before the normal redraw gate is consulted.
- `crates/nexus-tui/src/main.rs`: `read_input` polls synchronously and drains
  non-key/non-mouse events, including resize and paste reports. There is no
  dedicated paste editing path.
- `crates/nexus-tui/src/state.rs`: `visible_lines` derives total retained
  height and materializes `viewport height + scrollback` rows before removing
  the scrollback rows. Large manual scroll offsets may do avoidable allocation
  and rendering work. Total height still visits retained entries, even when
  cached per-entry heights avoid rewrapping.
- `crates/nexus-tui/src/state.rs`: assistant Markdown is rendered to measure
  height and again to materialize visible rows. The entry height cache does
  not cache the parsed/styled Markdown representation.
- `crates/nexus-tui/src/main.rs`: pointer hit mapping calls `window_lines`,
  which reconstructs visible text for mouse events instead of reusing the last
  rendered window.
- `crates/nexus-tui/src/render.rs`: composer height follows logical line count;
  wrapped visual rows, caret placement, and hit testing should share one layout
  model before adding inline suggestions.
- `crates/nexus-tui/src/main.rs`: recent-model persistence occurs in the run
  outcome handling path. Measure this path before moving work; configuration
  write failures must remain visible and must not block terminal cleanup.

Ratatui already diffs the current frame against the prior frame. Preserve that
behavior; avoid adding another renderer or assuming that raising the draw rate
alone improves responsiveness.

## Non-goals and invariants

- Do not expand M0 semantics or change the runtime's authorization decisions.
- Do not make file completion read file contents, grant access, or execute a
  tool. Selecting a path inserts a quoted/escaped `@relative/path` reference
  into the draft only. Any later file access remains an ordinary model-directed
  tool call subject to the current runtime policy and approval flow.
- Do not scan outside the configured project root, follow symlinks outside it,
  start network discovery, or load arbitrary large directory trees on the UI
  thread.
- Do not let a popup consume or synthesize an approval decision. Modal focus
  must prevent keys such as `a`, `s`, and `d` from falling through to the
  approval card.
- `Esc` closes the topmost suggestion/dialog and never cancels a run. Closing
  restores the draft and caret. The existing `Ctrl+C` cancel/quit escalation
  is unchanged; quitting from the keyboard is `q`/`:q` in the viewport.
- All interactions remain keyboard usable; mouse support is additive.
- Preserve bounded state, stale-run rejection, terminal cleanup, and explicit
  error/empty/loading states.

## Stage 0 — establish a reproducible baseline

Before changing the event loop or renderer, add/use deterministic fixtures and
record the same measurements for a cold start and a warm run where applicable:

1. Build optimized TUI (`cargo build -p nexus-tui --release --locked --offline`)
   and record build profile, target, terminal, OS, and binary size.
2. Measure key-to-visible-frame latency (p50/p95/max), frame render duration,
   idle CPU over a fixed interval, and time to apply a batch of stream events.
3. Exercise a short conversation, a retention-limit conversation, long
   Markdown/code blocks, narrow and wide terminal sizes, and scroll positions
   at the live tail and near the oldest retained entry.
4. Separate terminal/PTY overhead from application render time. Do not present
   a synthetic renderer benchmark as an end-to-end interactive result.

Keep benchmark fixtures and measurement scripts deterministic, opt-in if they
need a PTY or wall-clock sleeps, and documented alongside the results. Avoid
clearing Cargo caches as part of normal measurements.

## Stage 1 — input, scheduling, and rendering hardening

### Input and redraw scheduling

- Separate terminal input readiness from the 50 ms animation/maintenance tick.
  Prefer one input owner feeding a bounded queue or an equivalent event-driven
  mechanism; never call blocking terminal reads on the async event loop.
- Do not drop key presses when the queue is full. Define bounded backpressure
  and shutdown behavior; coalesce replaceable resize notifications while
  preserving key/paste ordering.
- Handle resize explicitly and recompute layout before subsequent hit testing.
- Handle bracketed paste as one bounded composer edit, sanitize control and bidi
  characters using the existing input rules, and never submit pasted text
  implicitly.
- Route stream updates through the redraw gate. Coalesce adjacent visual
  updates, but never delay approval, completion, failure, or cancellation state
  beyond the documented responsiveness bound.
- Keep caret/spinner animation on timer ticks. Stop requesting animation frames
  when the relevant animation is inactive or the app is not drawing that slot.

### Visible-window and Markdown work

- Profile first. Optimize only measured hot paths and keep the current bounded
  retention as the hard memory limit.
- Change viewport extraction to materialize only rows needed for the requested
  visible slice; scrolling far from the tail must not build and discard all
  rows between the tail and the viewport.
- Cache parsed/styled Markdown per assistant entry revision, then wrap for the
  current width. Invalidate on append, truncation, or content replacement; cap
  cache memory and do not retain duplicate unbounded rendered strings.
- Reuse the latest visible-row/hit-test snapshot for mouse gestures when its
  layout generation still matches. Invalidate on content, scroll, resize, fold,
  or viewport geometry changes.
- If cached height lookup remains O(number of entries) and profiling shows it
  matters at the retention bound, evaluate a simple cumulative-height index.
  Do not add a tree/index speculatively; test invalidation across append,
  truncation, fold, and terminal-width changes first.
- Move config writes or other potentially slow side effects off the draw path
  only if profiling demonstrates a stall. Report errors through the transcript
  and join/cancel workers deterministically during shutdown.

### Acceptance

- Key handling is no longer quantized to the 50 ms animation tick; report
  before/after p50/p95/max key-to-frame latency on the same terminal setup.
- Under an output flood, input, approval, cancellation, and terminal events
  make progress; no unbounded queue or lost control event is observed.
- Far-scroll render work is proportional to the visible window plus bounded
  metadata work, not the number of intervening wrapped rows.
- Cached and uncached renderer output is byte/cell equivalent across resize,
  CJK, combining marks, emoji, folded entries, truncation, and selection.
- Terminal resize, paste, normal exit, error exit, and panic/error cleanup keep
  terminal state recoverable.

## Stage 2 — shared picker/dialog foundation and model picker

Add one small overlay state machine in the TUI presentation layer. Suggested
shape: a single active overlay enum (model picker, command suggestions, file
suggestions), selected index, filter text, scroll offset, and a generation or
query identity for asynchronous results. Do not let render code perform I/O or
mutate runtime state.

The shared list widget should provide:

- centered, viewport-clamped geometry; a clear title, filter field, selected
  row, empty state, and bounded list scrolling;
- keyboard selection (`Up`/`Down`, paging, `Enter`, `Esc`) and optional mouse
  selection;
- correct clipping in narrow/short terminals and stable selection when results
  change;
- explicit focus routing, so overlay input is handled before composer,
  viewport, or approval shortcuts;
- tests using Ratatui's deterministic test backend; no terminal-specific
  dependency beyond the current crossterm backend.

Model picker behavior:

- Open from the existing model-cycle action and `:m`/`:model` in the desktop
  command line.
- Filter locally against configured model IDs, provider names, and display
  labels; mark the active selection and expose provider identity.
- Confirm updates the same active model binding used by `:model set <name>` and
  persists recency through the existing configuration owner. Cancel has no
  side effect.
- A selection only affects future submissions. Do not switch the provider for
  an in-flight run.
- Keep `:model set <name>` as a direct exact configured-ID selection for scripts
  and power users; report unknown IDs as today.
- The viewport additionally offers vim-style keys: `i` focuses the composer,
  `o` focuses it on a fresh line, and `:` opens an editable command line.
  Commands execute only on Enter; `:m` opens the picker, `:model set <id>`
  selects an exact configured model, `:s` lists sessions, and `:q` quits.
  A bare `q` is not a quit key. Composer text beginning with `/` is ordinary
  prompt input. The footer keeps the command line at left and version/status
  at right.

### Acceptance

- Open/filter/navigate/confirm/cancel works with keyboard alone and with mouse.
- Empty configuration, duplicate display names, long IDs, resize, and stale
  selection cases are explicit and deterministic.
- Model switching does not alter an active run or accidentally submit the
  composer; save failure is surfaced without losing the in-memory selection.
- While the picker is open, approval keys cannot reach the approval handler.

## Stage 3 — `:` command suggestions

- Derive suggestions from one command metadata registry containing canonical
  name, short description, and argument/help metadata. Use that registry for
  `:help`, parser validation where practical, and interactive suggestions so
  names cannot drift between three lists.
- Show suggestions when the viewport command line is active;
  filter as the user types and preserve all text after the caret.
- Support navigation, `Enter`/`Tab` completion, and `Esc` dismissal
  without stealing normal multiline editing keys when no suggestion is active.
- `Enter` or `Tab` completes the highlighted command; while a candidate is
  selected, the first `Enter` only inserts it. A subsequent `Enter` executes
  the completed command through the local parser and never submits a task.
- Every candidate displays a short explanation; file candidates identify
  directories versus literal reference-only files.
- Offer subcommand/argument suggestions only where values are known locally
  (for example session labels and configured model IDs); do not guess unknown
  command semantics.

### Acceptance

- Empty `/`, partial names, no-match queries, caret-in-middle edits, and
  multiline drafts behave predictably.
- Completion preserves suffix text and places the caret at the end of the
  inserted completion.
- `Esc` dismisses only the suggestion list without clearing, submitting, or
  changing focus; a second `Esc` may dismiss the command line itself.
- Suggestion actions cannot fall through into approval, scroll, or submit
  handlers.

## Stage 4 — `@` project-path suggestions

- Detect the `@query` token around the composer caret; do not rewrite unrelated
  text or search a different token when the caret is elsewhere.
- Search only beneath the canonical configured project root. Use a bounded,
  cancellable worker with maximum depth, result count, query length, and time
  budget. Exclude `.git`, build output, dependency caches, and other known
  generated directories by default; make exclusions explicit and testable.
- Do not follow symlinks by default. If symlink support is later required,
  canonicalize every result and prove it remains beneath the root before
  presenting it. Never use completion as an authorization check.
- Rank exact/prefix filename matches before path-substring matches; return
  deterministic ordering for ties. Show relative paths and distinguish
  loading, no matches, scan limit, and filesystem error states.
- Use a monotonically increasing query generation/cancellation token. A late
  result from an old query must not replace results for the current composer.
- Selection inserts an escaped relative `@path` reference at the token and
  keeps the rest of the draft. It does not read or attach the file. The model
  may request `host_read` later, subject to existing policy, protected-path
  checks, and approval.

Initial implementation uses a synchronous best-effort scan with a 12ms time
budget, 1,000-entry cap, depth 5, and 10 results; it never follows symlinks
and excludes common generated directories. A background cancellable scanner
remains a follow-up if measurements show that this bounded scan can still
interrupt interactive input on slow filesystems.

### Acceptance

- Tests cover spaces and punctuation in names, Unicode, hidden paths, large
  directories, permission errors, symlinks, stale results, cancellation,
  resize, and caret-in-middle replacement.
- Search never blocks drawing and never traverses outside the configured
  project root.
- Inserted paths are unambiguous to the prompt parser and do not alter adjacent
  user text. If the runtime/provider contract cannot preserve the reference
  safely, ship path completion as literal text only and document that it does
  not attach file contents.

## Cross-stage stability tests

Register TUI behavior tests in `crates/nexus-tui/tests/behavior.rs`, following
`docs/testing.md`. Keep UI state/logic tests deterministic and use test doubles
for filesystem discovery and delayed results. Required regression scenarios:

- sustained stream output plus key, resize, paste, and mouse input;
- overlay open/close while an approval is pending, including every approval
  decision key;
- fast query changes where workers finish out of order;
- switching sessions/models while a stream is active;
- tiny terminal geometry, CJK/wide characters, combining marks, emoji, and
  long unbroken paths;
- modal cleanup and terminal restoration on I/O error and normal quit.

Routine validation: `cargo fmt --all --check`, affected `nexus-tui` unit and
behavior tests sequentially, then affected-package Clippy. Do not run the
workspace-wide suite unless explicitly requested. Platform acceptance requires
separate Linux and macOS terminal/PTY checks; test-backend coverage alone does
not prove terminal protocol behavior.

## Open decisions before implementation

1. Which terminals/PTYs and OS versions define the key-latency acceptance
   environment? Record the terminal emulator, dimensions, and multiplexers.
2. Should `Tab` accept file/command completion or remain focus navigation when
   no suggestion is visible? Resolve against existing composer and focus
   bindings before freezing the key behavior.
3. Which generated directories should the default file matcher exclude, and
   should users be able to override exclusions in a later configuration
   change? Initial implementation should use a conservative fixed list.
4. What exact `@path` quoting/escaping syntax is accepted by the model prompt?
   Until a parser/contract is specified, insertion must remain literal and
   must not claim that file contents are attached.

## References

- [OpenCode CLI keybindings and dialog/autocomplete interaction](https://opencode.ai/v2/docs/cli/keybinds)
- [Current TUI design](../design/07-tui.md)
- [Performance design](../design/05-performance.md)
- [M0 gates](04-m0-gates.md)
- [Development tests](../testing.md)
