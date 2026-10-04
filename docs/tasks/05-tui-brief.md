# TUI Research Brief (P5B input)

Status: Draft research brief for P5B; no implementation.

This note records layout and interaction patterns observed in two upstream
references for the future P5B TUI. It introduces no requirements and changes
nothing in `docs/design/07-tui.md`. Concrete bindings remain open pending the
terminal compatibility tests already required by that design.

## Sources

- Grok Build user guide (getting started):
  <https://raw.githubusercontent.com/xai-org/grok-build/main/crates/codegen/xai-grok-pager/docs/user-guide/01-getting-started.md>
- Grok Build keyboard shortcuts:
  <https://raw.githubusercontent.com/xai-org/grok-build/main/crates/codegen/xai-grok-pager/docs/user-guide/03-keyboard-shortcuts.md>
- Grok Build permissions and safety:
  <https://raw.githubusercontent.com/xai-org/grok-build/main/crates/codegen/xai-grok-pager/docs/user-guide/22-permissions-and-safety.md>
- Pi TUI component doc (via GitHub page render):
  <https://github.com/earendil-works/pi/tree/main/packages/coding-agent/docs/tui.md>

## 1. Layout elements worth copying

Grok Build (getting-started, shortcuts):

- Two-pane fullscreen: **scrollback** (conversation history: prompts as sticky
  headers, agent markdown, thinking blocks, tool calls with inline diffs, task
  lists) plus a fixed **bottom prompt**. `Tab` moves focus between them.
- Expandable entries: collapse/expand the selected entry (`Left`/`Right` in
  simple mode), toggle fold, expand/collapse all, thinking-block toggle, raw
  markdown view, copy content vs. copy metadata (e.g. the shell command), and
  fullscreen viewer for one entry.
- Compact contextual **shortcuts bar** showing keys for the current
  focus/state, with the focus hint naming the card that owns the keyboard.
- Blocking **cards** (question, MCP elicitation, permission prompt,
  cancel-turn panel) with a shared contract: `Tab`/`Shift+Tab` walk the card's
  rows only, `Esc` steps back one rung (parking focus in the scrollback where
  applicable), priority order permission > cancel-turn > question > elicitation.
- Bounded presentation habits: pinned manual folds (`respect_manual_folds`),
  batched stream updates, and stash/restore of the composer draft. These align
  with our batching/bounding rules; details stay P5B-internal.

Pi (`tui.md`; a component-system doc, not an end-user keymap):

- Single-renderer rule: extensions use `ctx.ui` integration points
  (`select`/`confirm`/`input`/`editor`, `notify`/`setStatus`, `setWidget`,
  `custom`, extension renderers) and never create a second terminal renderer.
- Component model: render line arrays for a supplied width using
  `visibleWidth`/`truncateToWidth`/`sliceByColumn`/`wrapTextWithAnsi`;
  reapply styles per line; `invalidate()` plus coalesced `requestRender()`.
- Prefer built-ins (`Text`/`Markdown`/`TruncatedText`, layout containers,
  `Input`/`Editor`, `SelectList`/`SettingsList`, bounded `ScrollView`,
  `Loader`/`CancellableLoader`, `MouseRegion`) over rebuilding selection,
  scrolling, or width handling.
- Keyboard/mouse/focus rules that match our design: `matchesKey`/`Key` plus a
  `KeybindingsManager` for app actions; `Focusable` + cursor marker for IME;
  custom editors must preserve app shortcuts and forward unowned keys; mouse
  is complement-only with a keyboard path for every interaction; unhandled
  wheel scrolls the nearest `ScrollView`; regular (non-fullscreen) mode leaves
  scrollback to the terminal.
- Diagnostics practice: `PI_TUI_WRITE_LOG` captures the raw ANSI stream; test
  narrow widths, wide characters, resize, theme change, focus transitions, and
  both regular and fullscreen modes.

## 2. Approval interaction patterns

Grok permission prompt (shortcuts + permissions docs):

- Choices offered: **allow once**, **reject once** (optionally with a message
  back to the model), session-scoped allow-edits, per-command
  "always allow `<prefix>`" persisted per project, and an always-approve
  toggle. Our contract maps to the first two only: `Approve` binds the exact
  live call identity/arguments, `Deny` refuses without executing.
- Prompt shows the exact operation, affected scope, and full arguments
  (`Ctrl+F` expands/collapses args); scope widen/narrow and hand-edited
  patterns exist upstream. Copy the preview/scope display, not the
  persistence or the always-approve row.
- In-card keys: move between options, direct numeric choice, `Enter` to
  confirm, `Esc` parks focus without answering or dismissing, `Ctrl+C`
  cancels the request. Typing on the "No" row starts a message to the agent.
- Authorization pipeline worth mirroring in principle: hooks first, then
  deny > ask > allow rules, then remembered grants, then built-in
  read-only approvals, then mode policy. Our accepted subset is narrower
  (scoped reads automatic, mutations/commands confirmed); the severity
  ordering (deny wins) is the part to keep.

## 3. Cancellation keys

Grok Build (shortcuts doc; do not copy values blindly, see section 4):

- `Ctrl+C` cancels the turn once the composer is empty; with a draft, the
  first press clears the draft and the second cancels.
- `Esc` never cancels: mid-turn it shows a "use `Ctrl+C`" reminder; idle
  double-`Esc` clears a non-empty prompt (stashed) or opens rewind on an
  empty prompt. While cancelling, `Esc` is a no-op and `Ctrl+C` escalates
  toward quit.
- Cancel-turn panel choices are confirmed with numbers/`Enter`; its `Esc`
  means "keep running" and resolves the panel.
- Mid-turn `Enter` with text queues a follow-up; a terminal-dependent
  send-now chord cancels the current turn and runs the message next. The
  chord varies by terminal (see conflicts below), which is exactly why our
  design defers concrete bindings.
- Control-path principle (matches our command-events backpressure section):
  cancellation and approval responses must stay responsive under output
  saturation; Pi's RPC practice (stream events with backpressure before
  reading the next command) is the headless analogue.

## 4. Keyboard-conflict notes across the two references

- Grok's own map is context-dependent and therefore collision-prone:
  `Ctrl+M` (model picker vs. multiline toggle), `Ctrl+G` (tasks pane vs.
  external editor in minimal mode), `Ctrl+L` (extensions modal vs. mid-turn
  interject on VS Code family), `Ctrl+O` (always-approve vs. Apple Terminal
  send-now), `Enter` (send vs. queue vs. send-now-on-empty), `Tab` (focus
  switch vs. card-row walk), `Ctrl+C` (clear vs. cancel vs. quit
  escalation). Each collision is resolved by focus/state context, not by
  distinct keys. Any borrowed pattern must preserve that context gating.
- Terminal capability splits (Grok shortcuts doc): `Ctrl+.` needs the Kitty
  keyboard protocol (fallback `Ctrl+X`); modified-`Enter` chords need Kitty
  or tmux `extended-keys` (fallback `Ctrl+I`; VS Code family uses `Ctrl+L`);
  `Cmd+Enter` is excluded as a send chord; `Cmd+A` works only on Ghostty;
  Windows needs `Alt+V` for image paste; VS Code family remaps quit to
  `Ctrl+D` and half-page to `Shift+D`. Grok ships `/doctor` for this. Lesson:
  no static keymap is portable; P5B needs capability detection plus
  documented fallbacks, not a copied table.
- Pi vs. Grok: Pi routes app actions through a configurable
  `KeybindingsManager` and mandates a keyboard path for every mouse action;
  Grok bindings are built in and explicitly non-remappable. Our design takes
  Pi's side on test-before-freeze and avoids Grok's fixed-map approach.
- `Esc` handling differs by layer: Grok reserves it for clear/rewind/park
  (never cancel, never focus-switch); Pi overlays use explicit focus
  release/redirect through the overlay handle. Do not merge these into one
  `Esc` rule without the per-card rung table.

## 5. DO-NOT-COPY list

- **Always-approve controls** as features: `Ctrl+O` toggle, `--yolo` /
  `--always-approve`, `/always-approve`, `Shift+Tab` mode cycling into
  always-approve, `acceptEdits`/`auto`/`bypassPermissions` modes, and
  per-command "always allow" persistence. Decision 01 requires confirmation
  for file mutations and command execution; headless without a handler
  denies. (Source: permissions doc, "Permission modes".)
- **Broad theming/marketplace panels**: command-palette extension tabs
  (Hooks, Plugins, Marketplace, Skills, Workflows, MCP Servers), rich theme
  palettes, and Pi's full extension-reaches-everything API. Both our TUI
  design and the Pi reference note exclude these for the first release.
- **Multi-agent panels and extras**: agent dashboard, subagent fullscreen
  views, tasks/todos panes, prompt-queue pane, plan/auto-review modes,
  background-command management. Deferred or excluded upstream references;
  not P5B scope.
- **Auth/session conveniences that conflict with our defaults**: browser
  login/OAuth first-run, continue-latest-session (`-c`) as the norm,
  auto-restored sessions, MCP elicitation auto-approval flows.
- **A fixed non-remappable keymap**: Grok states bindings cannot be
  remapped. Our bindings stay undecided until Linux/macOS terminal
  compatibility tests; copying the table verbatim would import the
  contradictions Section 4 documents.
- **Fail-open hooks as a security boundary** and remembered grants that
  bypass `deny`: the permissions doc notes hooks fail open and deny always
  wins. Any future hook/grant mechanism must preserve deny-wins and must
  not treat hook success as authorization.
