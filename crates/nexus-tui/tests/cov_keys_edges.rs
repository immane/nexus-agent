//! Adversarial edge coverage for the TUI keyset (`nexus_tui::keys`).
//!
//! The unit tests in `keys.rs` pin the happy paths of the test-only keyset.
//! This file pins its *negatives* — the edges where a mapping bug would turn
//! a stray keystroke into a decision or a destructive global action:
//!
//! - unbound keys return `None` and never fall back to a decision, a
//!   cancel, a quit, or a submit, for every `KeyCode` variant in every
//!   focus;
//! - modifier combinations outside the table map to `None`, including
//!   `Alt`-decorated approval letters, which must not reach `ApproveOnce`,
//!   `Deny`, or `InspectApproval`;
//! - focus cycling reaches the approval card only while a decision is
//!   actually pending, and cycles are closed;
//! - approve/deny/inspect intents require the approval card focus, and a
//!   held key (`Repeat`/`Release`) produces no second decision.
//!
//! Everything goes through the public surface (`nexus_tui::keys` plus
//! `crossterm` event types); no private module is touched and no terminal,
//! runtime, or async context is involved.
//!
//! Determinism: the keyspace is a fixed, exhaustive table, so no sleeps,
//! randomness, wall-clock reads, I/O, or network are used.
//!
//! Known limitation, deliberately NOT asserted as desirable here: only
//! `CONTROL` and `ALT` are screened by the keyset, so `SHIFT`/`SUPER`/
//! `HYPER`/`META` fall through to the bare table.
//! `only_control_and_alt_are_screened` pins that current behavior so any
//! change to it is visible.

#![forbid(unsafe_code)]

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MediaKeyCode, ModifierKeyCode,
};

use nexus_tui::keys::{Action, Focus, map_key, next_focus};

/// Every focus the keyset can be asked about.
const ALL_FOCUSES: [Focus; 3] = [Focus::Composer, Focus::Viewport, Focus::ApprovalCard];

/// Key codes with no binding in any focus, whatever the focus: navigation
/// conveniences, editing keys the keyset does not expose, function keys,
/// media and lock keys, and the explicit null code.
const UNBOUND_SPECIAL_CODES: [KeyCode; 22] = [
    KeyCode::Home,
    KeyCode::End,
    KeyCode::Delete,
    KeyCode::Insert,
    KeyCode::BackTab,
    KeyCode::Null,
    KeyCode::CapsLock,
    KeyCode::ScrollLock,
    KeyCode::NumLock,
    KeyCode::PrintScreen,
    KeyCode::Pause,
    KeyCode::Menu,
    KeyCode::KeypadBegin,
    KeyCode::F(1),
    KeyCode::F(12),
    KeyCode::F(24),
    KeyCode::Media(MediaKeyCode::Play),
    KeyCode::Media(MediaKeyCode::MuteVolume),
    KeyCode::Media(MediaKeyCode::RaiseVolume),
    KeyCode::Modifier(ModifierKeyCode::LeftShift),
    KeyCode::Modifier(ModifierKeyCode::LeftControl),
    KeyCode::Modifier(ModifierKeyCode::RightMeta),
];

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::empty())
}

fn chord(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

fn control(char: char) -> KeyEvent {
    chord(KeyCode::Char(char), KeyModifiers::CONTROL)
}

fn alternate(char: char) -> KeyEvent {
    chord(KeyCode::Char(char), KeyModifiers::ALT)
}

/// Asserts the keyset refuses the event outright instead of substituting a
/// nearby intent.
fn expect_none(focus: Focus, key: KeyEvent, why: &str) {
    assert!(
        map_key(focus, key).is_none(),
        "{focus:?} + {key:?} must stay unbound ({why})"
    );
}

/// Asserts no intent that can decide, cancel, quit, or submit is ever
/// observed for an event the table does not bind.
fn expect_no_escape_hatch(focus: Focus, key: KeyEvent, why: &str) {
    let escaped = [
        Action::ApproveOnce,
        Action::Deny,
        Action::InspectApproval,
        Action::Cancel,
        Action::Quit,
        Action::Submit,
    ];
    match map_key(focus, key) {
        None => {}
        Some(action) => assert!(
            !escaped.contains(&action),
            "{focus:?} + {key:?} produced {action:?}, an unbound key must not \
             decide, cancel, quit, or submit ({why})"
        ),
    }
}

/// Oracle for the non-character half of the published keyset table. A `None`
/// arm is the assertion under test: every code without an explicit row is
/// unbound.
fn expected_special(focus: Focus, code: KeyCode) -> Option<Action> {
    match (focus, code) {
        (Focus::Composer, KeyCode::Enter) => Some(Action::Submit),
        (Focus::Composer, KeyCode::Tab) => Some(Action::CycleMode),
        (Focus::Composer, KeyCode::Esc) => Some(Action::ParkFocus),
        (Focus::Composer, KeyCode::Backspace) => Some(Action::Backspace),
        (Focus::Composer, KeyCode::Up) => Some(Action::ScrollUp),
        (Focus::Composer, KeyCode::Down) => Some(Action::ScrollDown),
        (Focus::Composer, KeyCode::PageUp) => Some(Action::PageUp),
        (Focus::Composer, KeyCode::PageDown) => Some(Action::PageDown),
        (Focus::Composer, KeyCode::Left | KeyCode::Right) => Some(Action::FoldToggle),
        (Focus::Viewport, KeyCode::Up) => Some(Action::ScrollUp),
        (Focus::Viewport, KeyCode::Down) => Some(Action::ScrollDown),
        (Focus::Viewport, KeyCode::PageUp) => Some(Action::PageUp),
        (Focus::Viewport, KeyCode::PageDown) => Some(Action::PageDown),
        (Focus::Viewport, KeyCode::Left | KeyCode::Right) => Some(Action::FoldToggle),
        (Focus::Viewport, KeyCode::Tab) => Some(Action::FocusSwitch),
        (Focus::Viewport, KeyCode::Esc) => Some(Action::ParkFocus),
        (Focus::ApprovalCard, KeyCode::Up) => Some(Action::ScrollUp),
        (Focus::ApprovalCard, KeyCode::Down) => Some(Action::ScrollDown),
        (Focus::ApprovalCard, KeyCode::PageUp) => Some(Action::PageUp),
        (Focus::ApprovalCard, KeyCode::PageDown) => Some(Action::PageDown),
        (Focus::ApprovalCard, KeyCode::Tab) => Some(Action::FocusSwitch),
        (Focus::ApprovalCard, KeyCode::Esc) => Some(Action::ParkFocus),
        _ => None,
    }
}

/// Oracle for printable ASCII characters. Non-ASCII composability is pinned
/// separately, because the predicate that gates it is crate-private.
fn expected_ascii_char(focus: Focus, char: char) -> Option<Action> {
    match (focus, char) {
        (Focus::Composer, _) => Some(Action::Type(char)),
        (Focus::Viewport, 'k') => Some(Action::ScrollUp),
        (Focus::Viewport, 'j') => Some(Action::ScrollDown),
        (Focus::Viewport, 'h' | 'l') => Some(Action::FoldToggle),
        (Focus::Viewport, 'm') => Some(Action::CycleModel),
        (Focus::Viewport, 'q') => Some(Action::Quit),
        (Focus::ApprovalCard, 'a') => Some(Action::ApproveOnce),
        (Focus::ApprovalCard, 'd') => Some(Action::Deny),
        (Focus::ApprovalCard, 'i') => Some(Action::InspectApproval),
        (Focus::ApprovalCard, 'k') => Some(Action::ScrollUp),
        (Focus::ApprovalCard, 'j') => Some(Action::ScrollDown),
        _ => None,
    }
}

#[test]
fn map_key_is_total_over_every_unmodified_key_code() {
    for focus in ALL_FOCUSES {
        for code in UNBOUND_SPECIAL_CODES {
            assert_eq!(
                map_key(focus, key(code)),
                expected_special(focus, code),
                "{focus:?} + {code:?} (no row in the keyset table)"
            );
        }
        for char in ' '..='~' {
            assert_eq!(
                map_key(focus, key(KeyCode::Char(char))),
                expected_ascii_char(focus, char),
                "{focus:?} + {char:?} (printable ASCII)"
            );
        }
    }
}

#[test]
fn unbound_keys_return_none_in_every_focus() {
    for focus in ALL_FOCUSES {
        for code in UNBOUND_SPECIAL_CODES {
            expect_none(focus, key(code), "no keyset row for this code");
            expect_no_escape_hatch(focus, key(code), "no keyset row");
        }
    }
}

#[test]
fn control_characters_and_bidi_controls_are_never_typed() {
    // The composer is rendered without sanitization, so terminal controls
    // and text-reordering characters must not become typed input.
    for char in [
        '\t', '\n', '\r', '\x00', '\x1b', '\x07', '\x7f', '\u{061C}', '\u{200E}', '\u{200F}',
        '\u{202A}', '\u{202E}', '\u{2066}', '\u{2069}', '\u{206F}',
    ] {
        expect_none(
            Focus::Composer,
            key(KeyCode::Char(char)),
            "not composable text",
        );
        expect_no_escape_hatch(
            Focus::Composer,
            key(KeyCode::Char(char)),
            "not composable text",
        );
    }
}

#[test]
fn focus_gaps_never_borrow_another_focus_binding() {
    // `Esc` parks focus from every card: the composer parks in the
    // viewport, the viewport returns home, and the approval card steps
    // back — but none of them cancels or decides.
    assert_eq!(
        map_key(Focus::Viewport, key(KeyCode::Esc)),
        Some(Action::ParkFocus),
        "viewport Esc returns to the composer"
    );
    assert_eq!(
        map_key(Focus::Composer, key(KeyCode::Esc)),
        Some(Action::ParkFocus)
    );
    assert_eq!(
        map_key(Focus::ApprovalCard, key(KeyCode::Esc)),
        Some(Action::ParkFocus)
    );

    // Plain `Enter` submits in the composer and is unbound elsewhere, so a
    // stray Enter in the viewport cannot start a run.
    expect_none(
        Focus::Viewport,
        key(KeyCode::Enter),
        "no viewport submit row",
    );
    expect_none(Focus::ApprovalCard, key(KeyCode::Enter), "no approval row");
    // The composer recalls submitted inputs right in the draft, so recall
    // is always visible; it also pages and folds without leaving typing
    // focus, so the viewport-only shortcuts stay one `Esc` away but are
    // rarely needed.
    assert_eq!(
        map_key(Focus::Composer, key(KeyCode::Up)),
        Some(Action::ScrollUp),
        "composer Up recalls the previous submitted input"
    );
    assert_eq!(
        map_key(Focus::Composer, key(KeyCode::Down)),
        Some(Action::ScrollDown),
        "composer Down recalls the next submitted input"
    );
    assert_eq!(
        map_key(Focus::Composer, key(KeyCode::PageUp)),
        Some(Action::PageUp),
        "composer PgUp pages the viewport"
    );
    assert_eq!(
        map_key(Focus::Composer, key(KeyCode::PageDown)),
        Some(Action::PageDown),
        "composer PgDn pages the viewport"
    );
    assert_eq!(
        map_key(Focus::Composer, key(KeyCode::Left)),
        Some(Action::FoldToggle),
        "composer Left folds the selected entry"
    );
    assert_eq!(
        map_key(Focus::Composer, key(KeyCode::Right)),
        Some(Action::FoldToggle),
        "composer Right unfolds the selected entry"
    );
    expect_none(
        Focus::ApprovalCard,
        key(KeyCode::Left),
        "no approval folding",
    );
    expect_none(
        Focus::ApprovalCard,
        key(KeyCode::Right),
        "no approval folding",
    );
    expect_none(
        Focus::ApprovalCard,
        key(KeyCode::Backspace),
        "no approval editing",
    );
    expect_none(
        Focus::Viewport,
        key(KeyCode::Backspace),
        "no viewport editing",
    );
}

#[test]
fn composer_quit_shortcut_does_not_quit_from_the_composer() {
    // `q` quits the viewport but is ordinary draft text in the composer.
    assert_eq!(
        map_key(Focus::Composer, key(KeyCode::Char('q'))),
        Some(Action::Type('q'))
    );
    assert_eq!(
        map_key(Focus::Viewport, key(KeyCode::Char('q'))),
        Some(Action::Quit)
    );
    expect_none(
        Focus::ApprovalCard,
        key(KeyCode::Char('q')),
        "no approval quit row",
    );
}

#[test]
fn control_combos_outside_the_table_map_to_none() {
    // Only `Ctrl+c` (cancel), `Ctrl+d` (quit), Composer `Ctrl+j`
    // (newline), and `Ctrl+t` (variant) are bound, so every other control
    // chord must be dropped rather than aliased onto one of them.
    let stray_controls = [
        control('a'),
        control('r'),
        control('i'),
        control('q'),
        control('n'),
        control('J'),
        control('e'),
        chord(KeyCode::Enter, KeyModifiers::CONTROL),
        chord(KeyCode::Tab, KeyModifiers::CONTROL),
        chord(KeyCode::Esc, KeyModifiers::CONTROL),
        chord(KeyCode::Backspace, KeyModifiers::CONTROL),
        chord(KeyCode::Up, KeyModifiers::CONTROL),
        chord(KeyCode::PageDown, KeyModifiers::CONTROL),
    ];
    for focus in ALL_FOCUSES {
        for event in stray_controls {
            expect_none(
                focus,
                event,
                "Ctrl is only bound to c, d, Composer j, and t",
            );
            expect_no_escape_hatch(focus, event, "Ctrl is only bound to c, d, j, and t");
        }
    }
    // `Ctrl+J` (uppercase, i.e. a real held Ctrl) is not the newline binding;
    // only the lowercase `Ctrl+j` is, and only in the composer.
    assert_eq!(
        map_key(Focus::Composer, control('j')),
        Some(Action::Newline)
    );
    expect_none(Focus::Viewport, control('j'), "newline is composer-only");
    expect_none(
        Focus::ApprovalCard,
        control('j'),
        "newline is composer-only",
    );
}

#[test]
fn alt_combos_outside_the_table_map_to_none() {
    let stray_alts = [
        alternate('a'),
        alternate('d'),
        alternate('i'),
        alternate('c'),
        alternate('q'),
        alternate('j'),
        alternate('7'),
        chord(KeyCode::Tab, KeyModifiers::ALT),
        chord(KeyCode::Esc, KeyModifiers::ALT),
        chord(KeyCode::Up, KeyModifiers::ALT),
        chord(KeyCode::F(5), KeyModifiers::ALT),
    ];
    for focus in ALL_FOCUSES {
        for event in stray_alts {
            expect_none(focus, event, "Alt is only bound to Composer Enter");
            expect_no_escape_hatch(focus, event, "Alt is only bound to Composer Enter");
        }
    }
    // `Alt+Enter` inserts a newline in the composer and nowhere else.
    assert_eq!(
        map_key(Focus::Composer, chord(KeyCode::Enter, KeyModifiers::ALT)),
        Some(Action::Newline)
    );
    expect_none(
        Focus::Viewport,
        chord(KeyCode::Enter, KeyModifiers::ALT),
        "newline is composer-only",
    );
    expect_none(
        Focus::ApprovalCard,
        chord(KeyCode::Enter, KeyModifiers::ALT),
        "newline is composer-only",
    );
}

#[test]
fn alt_decorative_approval_letters_never_decide() {
    // Alt+a / Alt+d / Alt+i are the highest-value negatives in the file: a
    // menu/terminal quirk that decorates the approval keys must not produce
    // a decision.
    for (event, forbidden) in [
        (alternate('a'), Action::ApproveOnce),
        (alternate('d'), Action::Deny),
        (alternate('i'), Action::InspectApproval),
    ] {
        for focus in ALL_FOCUSES {
            assert_eq!(
                map_key(focus, event),
                None,
                "{focus:?} + {event:?} must not decide"
            );
            assert_ne!(
                map_key(focus, event),
                Some(forbidden.clone()),
                "{focus:?} + {event:?} must not decide"
            );
        }
    }
}

#[test]
fn control_is_screened_before_alt() {
    // A CONTROL-bearing event never reaches the ALT rows: `Ctrl+Alt+j` is
    // still the composer newline and `Ctrl+Alt+d` is still the quit chord.
    let control_alt = KeyModifiers::CONTROL | KeyModifiers::ALT;
    assert_eq!(
        map_key(Focus::Composer, chord(KeyCode::Char('j'), control_alt)),
        Some(Action::Newline)
    );
    assert_eq!(
        map_key(Focus::Composer, chord(KeyCode::Char('c'), control_alt)),
        Some(Action::Cancel)
    );
    assert_eq!(
        map_key(Focus::Viewport, chord(KeyCode::Char('d'), control_alt)),
        Some(Action::Quit)
    );
    // ...and it never reaches the bare table, so it cannot approve.
    assert_eq!(
        map_key(Focus::ApprovalCard, chord(KeyCode::Char('a'), control_alt)),
        None
    );
    assert_eq!(
        map_key(Focus::Composer, chord(KeyCode::Enter, control_alt)),
        None
    );
}

#[test]
fn only_control_and_alt_are_screened() {
    // Documented limitation: SHIFT/SUPER/HYPER/META are not screened, so
    // they fall through to the bare table.
    assert_eq!(
        map_key(Focus::Composer, chord(KeyCode::Enter, KeyModifiers::SHIFT)),
        Some(Action::Submit),
        "Shift+Enter is bare Enter: the table ignores SHIFT"
    );
    assert_eq!(
        map_key(
            Focus::ApprovalCard,
            chord(KeyCode::Char('a'), KeyModifiers::SUPER)
        ),
        Some(Action::ApproveOnce),
        "SUPER is unscreened, so it reaches the bare approval row"
    );
    assert_eq!(
        map_key(
            Focus::ApprovalCard,
            chord(KeyCode::Char('d'), KeyModifiers::META)
        ),
        Some(Action::Deny)
    );
    assert_eq!(
        map_key(
            Focus::Composer,
            chord(KeyCode::Char('J'), KeyModifiers::SHIFT)
        ),
        Some(Action::Type('J'))
    );

    // Shifted letters arrive as uppercase characters, and no uppercase
    // character is bound: `Shift+a` cannot approve.
    for (char, modifiers) in [
        ('A', KeyModifiers::SHIFT),
        ('D', KeyModifiers::SHIFT),
        ('I', KeyModifiers::SHIFT),
    ] {
        expect_none(
            Focus::ApprovalCard,
            chord(KeyCode::Char(char), modifiers),
            "uppercase letters are unbound",
        );
        expect_no_escape_hatch(
            Focus::ApprovalCard,
            chord(KeyCode::Char(char), modifiers),
            "uppercase letters are unbound",
        );
    }
    // `Shift+Tab` arrives as `BackTab`, which is unbound: it must not
    // switch focus in either direction.
    for focus in ALL_FOCUSES {
        expect_none(
            focus,
            chord(KeyCode::BackTab, KeyModifiers::SHIFT),
            "Shift+Tab is BackTab and unbound",
        );
    }
}

#[test]
fn approval_decisions_require_the_approval_focus() {
    for (char, expected) in [
        ('a', Action::ApproveOnce),
        ('d', Action::Deny),
        ('i', Action::InspectApproval),
    ] {
        assert_eq!(
            map_key(Focus::ApprovalCard, key(KeyCode::Char(char))),
            Some(expected),
            "{char:?} under the approval card"
        );
        // The same letter elsewhere can only type text or do nothing.
        assert_eq!(
            map_key(Focus::Composer, key(KeyCode::Char(char))),
            Some(Action::Type(char)),
            "{char:?} in the composer is draft text"
        );
        assert_eq!(
            map_key(Focus::Viewport, key(KeyCode::Char(char))),
            None,
            "{char:?} in the viewport is unbound"
        );
    }
}

#[test]
fn no_character_other_than_a_d_i_decides_under_the_approval_card() {
    for char in ' '..='~' {
        let action = map_key(Focus::ApprovalCard, key(KeyCode::Char(char)));
        let decides = matches!(
            action,
            Some(Action::ApproveOnce) | Some(Action::Deny) | Some(Action::InspectApproval)
        );
        assert_eq!(
            decides,
            matches!(char, 'a' | 'd' | 'i'),
            "{char:?} under the approval card produced {action:?}"
        );
    }
}

#[test]
fn approval_intents_never_alias_each_other() {
    for (char, expected) in [
        ('a', Action::ApproveOnce),
        ('d', Action::Deny),
        ('i', Action::InspectApproval),
    ] {
        let action = map_key(Focus::ApprovalCard, key(KeyCode::Char(char)));
        assert_eq!(
            action,
            Some(expected.clone()),
            "{char:?} is one intent only"
        );
        for alias in [Action::ApproveOnce, Action::Deny, Action::InspectApproval] {
            if alias == expected {
                continue;
            }
            assert_ne!(
                action,
                Some(alias.clone()),
                "{char:?} must not also mean {alias:?}"
            );
        }
    }
}

#[test]
fn held_approval_keys_produce_no_repeat_decision() {
    // A held decision key must not stream approvals: only the initial press
    // carries an intent, so auto-repeat and release are dropped outright.
    for (char, expected) in [
        ('a', Action::ApproveOnce),
        ('d', Action::Deny),
        ('i', Action::InspectApproval),
    ] {
        assert_eq!(
            map_key(Focus::ApprovalCard, key(KeyCode::Char(char))),
            Some(expected),
            "{char:?} decides on the initial press"
        );
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            expect_none(
                Focus::ApprovalCard,
                KeyEvent::new_with_kind(KeyCode::Char(char), KeyModifiers::empty(), kind),
                "only Press carries a decision",
            );
        }
    }
    // Repeat and release are dropped for every focus and every bound code,
    // not just the approval keys.
    for focus in ALL_FOCUSES {
        for code in [
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Tab,
            KeyCode::Char('q'),
        ] {
            for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
                expect_none(
                    focus,
                    KeyEvent::new_with_kind(code, KeyModifiers::empty(), kind),
                    "only Press carries an intent",
                );
            }
        }
    }
}

#[test]
fn focus_cycle_reaches_the_approval_card_only_while_a_decision_is_pending() {
    // The full transition table, one row per (focus, pending) pair.
    for (focus, pending, expected) in [
        (Focus::Composer, false, Focus::Viewport),
        (Focus::Composer, true, Focus::Viewport),
        (Focus::Viewport, false, Focus::Composer),
        (Focus::Viewport, true, Focus::ApprovalCard),
        (Focus::ApprovalCard, false, Focus::Composer),
        (Focus::ApprovalCard, true, Focus::Composer),
    ] {
        assert_eq!(
            next_focus(focus, pending),
            expected,
            "next_focus({focus:?}, {pending})"
        );
    }
}

#[test]
fn approval_card_is_unreachable_without_a_pending_decision() {
    for focus in ALL_FOCUSES {
        assert_ne!(
            next_focus(focus, false),
            Focus::ApprovalCard,
            "no pending decision must mean no approval card, from {focus:?}"
        );
    }
    // A parked approval card (the decision already answered) also steps to
    // the composer instead of trapping focus on a stale card.
    assert_eq!(next_focus(Focus::ApprovalCard, false), Focus::Composer);
}

#[test]
fn focus_cycles_are_closed_with_and_without_a_pending_approval() {
    // With a pending decision the three cards form one closed cycle of
    // period three, from every starting point.
    for start in ALL_FOCUSES {
        let mut focus = start;
        for _ in 0..3 {
            focus = next_focus(focus, true);
        }
        assert_eq!(
            focus, start,
            "the pending-approval cycle is closed from {start:?}"
        );
    }
    // Without one, the approval card is transient rather than cyclic: a
    // parked card steps out to the composer and can never be returned to,
    // so focus settles onto the composer/viewport cycle of period two.
    assert_eq!(next_focus(Focus::ApprovalCard, false), Focus::Composer);
    for start in [Focus::Composer, Focus::Viewport] {
        let mut focus = start;
        for _ in 0..2 {
            focus = next_focus(focus, false);
            assert_ne!(
                focus,
                Focus::ApprovalCard,
                "step from {start:?} entered the approval card with no pending \
                 decision"
            );
        }
        assert_eq!(
            focus, start,
            "the idle focus cycle is closed from {start:?}"
        );
    }
}

#[test]
fn tab_cycling_respects_the_pending_gate_before_any_decision_key() {
    // In the composer Tab cycles the agent mode and keeps focus, so `a`
    // can only ever be draft text there.
    assert_eq!(
        map_key(Focus::Composer, key(KeyCode::Tab)),
        Some(Action::CycleMode)
    );
    assert_eq!(
        map_key(Focus::Composer, key(KeyCode::Char('a'))),
        Some(Action::Type('a'))
    );

    // Outside the composer Tab still hands the keyboard over.
    for focus in [Focus::Viewport, Focus::ApprovalCard] {
        assert_eq!(map_key(focus, key(KeyCode::Tab)), Some(Action::FocusSwitch));
        assert_ne!(
            next_focus(focus, false),
            focus,
            "a focus switch from {focus:?} must change cards"
        );
    }

    // With a pending decision, the second Tab parks on the approval card,
    // and only there do the decision keys mean anything.
    let mut focus = Focus::Composer;
    for _ in 0..2 {
        focus = next_focus(focus, true);
    }
    assert_eq!(focus, Focus::ApprovalCard);
    for (char, expected) in [
        ('a', Action::ApproveOnce),
        ('d', Action::Deny),
        ('i', Action::InspectApproval),
    ] {
        assert_eq!(
            map_key(focus, key(KeyCode::Char(char))),
            Some(expected),
            "{char:?} once the approval card holds focus"
        );
    }
    // Esc steps back out of the approval card without deciding.
    assert_eq!(map_key(focus, key(KeyCode::Esc)), Some(Action::ParkFocus));
    assert_eq!(next_focus(focus, true), Focus::Composer);
    assert_eq!(
        map_key(Focus::Composer, key(KeyCode::Char('d'))),
        Some(Action::Type('d')),
        "after parking, d is text again, never a deny"
    );
}

#[test]
fn non_ascii_text_never_decides() {
    // Ordinary multilingual characters and emoji type; nothing outside
    // ASCII can reach a decision key in any focus.
    for char in ['é', '日', '🌍', 'ß', 'Ω'] {
        assert_eq!(
            map_key(Focus::Composer, key(KeyCode::Char(char))),
            Some(Action::Type(char))
        );
        assert_eq!(map_key(Focus::Viewport, key(KeyCode::Char(char))), None);
        assert_eq!(map_key(Focus::ApprovalCard, key(KeyCode::Char(char))), None);
    }
}
