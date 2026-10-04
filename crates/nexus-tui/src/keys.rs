//! Test-only keyset mapping raw key events to UI intents.
//!
//! Concrete bindings are UNDECIDED pending the Linux/macOS terminal
//! compatibility tests required by the TUI design: the table below is a
//! test-only stand-in, not a product keymap, and must not be copied
//! verbatim (upstream maps contradict each other and collide by focus
//! context; see the TUI research brief sections 4-5).
//!
//! Test-only keyset (context-gated by [`Focus`]; `Esc` never cancels and no
//! shortcut silently approves):
//!
//! | Focus | Key | Intent |
//! | --- | --- | --- |
//! | Composer | `Enter` | Submit the draft |
//! | Composer | `Alt+Enter` or `Ctrl+J` | Multiline newline |
//! | Composer | `Tab` | Switch focus |
//! | Composer | `Esc` | Park focus in the viewport (never cancel) |
//! | Composer | printable char | Type into the draft |
//! | Composer | `Backspace` | Delete last draft char |
//! | Viewport | `Up`/`k`, `Down`/`j` | Move selection (scrolls) |
//! | Viewport | `PageUp`/`PageDown` | Scroll one page |
//! | Viewport | `Left`/`h`, `Right`/`l` | Fold/unfold the selected entry |
//! | Viewport | `Tab` | Switch focus |
//! | Viewport | `q` | Quit (test-only convenience) |
//! | Approval | `a` | Allow once (exact live call only) |
//! | Approval | `d` | Deny without executing |
//! | Approval | `Tab` | Switch focus |
//! | Approval | `Esc` | Park focus without answering or dismissing |
//! | Any | `Ctrl+C` | Cancel while cancellable (caller checks
//! [`crate::AppState::can_cancel`]), else quit |
//! | Any | `Ctrl+D` | Quit |
//!
//! Approval/deny intents are only produced while [`Focus::ApprovalCard`]
//! owns the keyboard; the DO-NOT-COPY list (always-approve toggles,
//! persisted grants, fail-open hooks) is excluded by construction: no such
//! intent exists here.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Which card owns the keyboard (mirrors the upstream blocking-card
/// contract: one focused card, `Esc` steps back one rung, never cancels).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// Fixed composer input.
    Composer,
    /// Conversation viewport.
    Viewport,
    /// Pending approval card.
    ApprovalCard,
}

/// UI intent decoded from one key event. Intents that reach the runtime
/// become typed commands via [`crate::decisions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Submit the composer draft as a new run.
    Submit,
    /// Insert a newline into the draft.
    Newline,
    /// Cycle keyboard focus.
    FocusSwitch,
    /// Park focus in the viewport (never answers or cancels).
    ParkFocus,
    /// Move the viewport selection up.
    ScrollUp,
    /// Move the viewport selection down.
    ScrollDown,
    /// Scroll one viewport page up.
    PageUp,
    /// Scroll one viewport page down.
    PageDown,
    /// Fold or unfold the selected entry.
    FoldToggle,
    /// Allow the exact live approval once.
    ApproveOnce,
    /// Refuse the live approval without executing.
    Deny,
    /// Cancel streaming work or a pending approval.
    Cancel,
    /// Leave the TUI.
    Quit,
    /// Type a char into the composer.
    Type(char),
    /// Delete the last composer char.
    Backspace,
}

/// Maps one key event to an intent for the focused card. Returns `None`
/// for unbound keys. The mapping is total over the table above and emits
/// [`Action::Cancel`] only for `Ctrl+C`, never for `Esc`.
#[must_use]
pub fn map_key(focus: Focus, key: KeyEvent) -> Option<Action> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Some(Action::Cancel),
            KeyCode::Char('d') => Some(Action::Quit),
            KeyCode::Char('j') if focus == Focus::Composer => Some(Action::Newline),
            _ => None,
        };
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        return match (focus, key.code) {
            (Focus::Composer, KeyCode::Enter) => Some(Action::Newline),
            _ => None,
        };
    }
    match (focus, key.code) {
        (Focus::Composer, KeyCode::Enter) => Some(Action::Submit),
        (Focus::Composer, KeyCode::Tab) => Some(Action::FocusSwitch),
        (Focus::Composer, KeyCode::Esc) => Some(Action::ParkFocus),
        (Focus::Composer, KeyCode::Backspace) => Some(Action::Backspace),
        (Focus::Composer, KeyCode::Char(char)) => Some(Action::Type(char)),
        (Focus::Viewport, KeyCode::Up | KeyCode::Char('k')) => Some(Action::ScrollUp),
        (Focus::Viewport, KeyCode::Down | KeyCode::Char('j')) => Some(Action::ScrollDown),
        (Focus::Viewport, KeyCode::PageUp) => Some(Action::PageUp),
        (Focus::Viewport, KeyCode::PageDown) => Some(Action::PageDown),
        (
            Focus::Viewport,
            KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l'),
        ) => Some(Action::FoldToggle),
        (Focus::Viewport, KeyCode::Tab) => Some(Action::FocusSwitch),
        (Focus::Viewport, KeyCode::Char('q')) => Some(Action::Quit),
        (Focus::ApprovalCard, KeyCode::Char('a')) => Some(Action::ApproveOnce),
        (Focus::ApprovalCard, KeyCode::Char('d')) => Some(Action::Deny),
        (Focus::ApprovalCard, KeyCode::Tab) => Some(Action::FocusSwitch),
        (Focus::ApprovalCard, KeyCode::Esc) => Some(Action::ParkFocus),
        _ => None,
    }
}

/// Cycles focus: composer, viewport, then the approval card only while a
/// decision is actually pending.
#[must_use]
pub fn next_focus(current: Focus, approval_pending: bool) -> Focus {
    match current {
        Focus::Composer => Focus::Viewport,
        Focus::Viewport => {
            if approval_pending {
                Focus::ApprovalCard
            } else {
                Focus::Composer
            }
        }
        Focus::ApprovalCard => Focus::Composer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn ctrl(char: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(char), KeyModifiers::CONTROL)
    }

    #[test]
    fn composer_submit_and_newline_are_distinct() {
        assert_eq!(
            map_key(Focus::Composer, key(KeyCode::Enter)),
            Some(Action::Submit)
        );
        assert_eq!(
            map_key(
                Focus::Composer,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)
            ),
            Some(Action::Newline)
        );
        assert_eq!(map_key(Focus::Composer, ctrl('j')), Some(Action::Newline));
        // Plain Enter in the viewport is unbound: no accidental submit.
        assert_eq!(map_key(Focus::Viewport, key(KeyCode::Enter)), None);
    }

    #[test]
    fn esc_parks_but_never_cancels_or_decides() {
        for focus in [Focus::Composer, Focus::Viewport, Focus::ApprovalCard] {
            let action = map_key(focus, key(KeyCode::Esc));
            assert_ne!(action, Some(Action::Cancel), "{focus:?}");
            assert_ne!(action, Some(Action::ApproveOnce), "{focus:?}");
            assert_ne!(action, Some(Action::Deny), "{focus:?}");
        }
        assert_eq!(
            map_key(Focus::ApprovalCard, key(KeyCode::Esc)),
            Some(Action::ParkFocus)
        );
    }

    #[test]
    fn approval_intents_require_the_approval_focus() {
        assert_eq!(
            map_key(Focus::ApprovalCard, key(KeyCode::Char('a'))),
            Some(Action::ApproveOnce)
        );
        assert_eq!(
            map_key(Focus::ApprovalCard, key(KeyCode::Char('d'))),
            Some(Action::Deny)
        );
        // Typing the same letters in the composer types text, never decides.
        assert_eq!(
            map_key(Focus::Composer, key(KeyCode::Char('a'))),
            Some(Action::Type('a'))
        );
        assert_eq!(map_key(Focus::Viewport, key(KeyCode::Char('a'))), None);
    }

    #[test]
    fn ctrl_c_cancels_from_any_focus_ctrl_d_quits() {
        for focus in [Focus::Composer, Focus::Viewport, Focus::ApprovalCard] {
            assert_eq!(map_key(focus, ctrl('c')), Some(Action::Cancel));
            assert_eq!(map_key(focus, ctrl('d')), Some(Action::Quit));
        }
    }

    #[test]
    fn focus_cycles_through_a_live_approval_card() {
        assert_eq!(next_focus(Focus::Composer, false), Focus::Viewport);
        assert_eq!(next_focus(Focus::Viewport, false), Focus::Composer);
        assert_eq!(next_focus(Focus::Viewport, true), Focus::ApprovalCard);
        assert_eq!(next_focus(Focus::ApprovalCard, true), Focus::Composer);
    }
}
