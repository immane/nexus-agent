//! Minimal TUI shell over the nexus runtime command/event port (P5B).
//!
//! The frontend owns presentation state only: a bounded conversation
//! viewport, a fixed composer, entry folding, an approval card, and a
//! controlled refresh gate. It submits typed [`nexus_core::Command`] values
//! to the runtime and never invokes providers or tools directly, so there is
//! exactly one policy path (the runtime's).
//!
//! // Rationale: `ratatui` 0.26 is the single terminal UI library for M0.
//! Immediate-mode rendering over its one `crossterm` 0.27 backend covers the
//! fixed composer, bounded viewport, approval card, and footer without a
//! larger fullscreen framework (no side panels, theming, marketplaces, or
//! multi-agent views per the DO-NOT-COPY list). Version 0.26 matches the
//! offline-cached crates, `default-features = false` keeps only the
//! `crossterm` backend, and its `TestBackend` makes renderer tests
//! deterministic without a PTY.
//!
//! Untrusted runtime output is sanitized at the presentation boundary
//! ([`sanitize`]) so model/tool text can never inject terminal control
//! sequences.

#![forbid(unsafe_code)]

pub mod decisions;
pub mod keys;
pub mod markdown;
pub mod render;
pub mod sanitize;
pub mod slash;
pub mod state;
pub mod terminal;

pub use decisions::{approve_command, cancel_command, deny_command, submit_command};
pub use keys::{Action, Focus, map_key, next_focus};
pub use markdown::{MdStyle, StyledRun};
pub use render::render;
pub use slash::{SessionArgs, SlashCommand};
pub use state::{
    AppState, EntryKind, MAX_PICKER_VISIBLE_ROWS, ModelChoice, Overlay, PendingApprovalCard,
    PointerGeometry, RefreshGate, SelPoint, TOAST_TTL, TextSelection, VisibleView,
    cell_to_char_col,
};
pub use terminal::{TerminalGuard, install_panic_hook, restore_for_panic};
