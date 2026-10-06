//! Contract coverage for the TUI keyset ([`nexus_tui::keys`]).
//!
//! `src/keys.rs` publishes a test-only focus/key table in its module docs and
//! pins its happy paths in private unit tests. This file hardens the *main*
//! mapping contract from the outside, through the public API only:
//!
//! - [`PUBLISHED_ROWS`] transcribes the published table row for row, and
//!   every row maps to exactly its documented intent;
//! - every `Action` variant is both documented *and* reachable, and each
//!   intent is produced by exactly its documented keys and by no others (a
//!   producer census over the whole ASCII keyspace), so a row can never be
//!   silently duplicated, aliased, or dropped;
//! - `Esc` parks focus and is the only key that can: it never cancels,
//!   approves, denies, inspects, submits, or quits, under any modifier;
//! - the composer accepts every ordinary character as text, including the
//!   letters that decide, navigate, fold, or quit in another focus;
//! - intents are partitioned by focus, and each row sits under the focus that
//!   owns its intent;
//! - `Enter`, `Alt+Enter`, and `Ctrl+J` stay three distinct composer events,
//!   and the named composer keys never type text;
//! - `map_key` depends only on `(focus, event)` and carries no state between
//!   keystrokes.
//!
//! Negatives — unbound codes, stray modifier chords, held (repeat/release)
//! events, and focus-cycle closure — are the sibling `cov_keys_edges.rs`’s
//! subject; this file stays on the positive mapping contract.
//!
//! Determinism: every input comes from a literal table. No clock, filesystem,
//! terminal, environment, randomness, or network is involved, and no test
//! shares mutable state.

#![forbid(unsafe_code)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use nexus_tui::keys::{Action, Focus, map_key, next_focus};

/// Every focus the keyset can be asked about.
const ALL_FOCUSES: [Focus; 3] = [Focus::Composer, Focus::Viewport, Focus::ApprovalCard];

/// An unmodified key press, as the published table writes its rows.
const NONE: KeyModifiers = KeyModifiers::NONE;

/// One row of the focus/key table published in the `keys.rs` module docs.
///
/// The `Any`-scoped rows of that table are written out once per focus here so
/// the oracle stays a plain `(focus, key) -> intent` function with no wildcard
/// to reason about. The table's single parameterized row (the composer accepts
/// every printable character) is checked exhaustively over the printable range
/// instead of being enumerated 95 times; see [`PARAMETERIZED_FAMILIES`].
#[derive(Debug, Clone)]
struct Row {
    /// The focus whose table the row belongs to.
    focus: Focus,
    /// The key the row binds.
    code: KeyCode,
    /// The modifiers the row is documented with.
    modifiers: KeyModifiers,
    /// The intent the row must produce, and only that intent.
    intent: Action,
    /// The published row this was transcribed from, quoted in failures.
    doc: &'static str,
}

impl Row {
    /// The key event the row stands for.
    fn event(&self) -> KeyEvent {
        KeyEvent::new(self.code, self.modifiers)
    }

    /// The same label the producer census uses, so oracle rows and observed
    /// producers are compared as strings.
    fn label(&self) -> String {
        row_label(self.focus, self.code, self.modifiers)
    }
}

/// The published table, row for row.
///
/// Ordered as the module docs are: composer rows, viewport rows, approval rows,
/// then the `Any` rows. The count is pinned by
/// `published_table_has_no_duplicate_or_conflicting_rows` so a row dropped from
/// this transcription fails loudly instead of quietly weakening the oracle.
const PUBLISHED_ROWS: &[Row] = &[
    // Composer | Enter | Submit the draft
    Row {
        focus: Focus::Composer,
        code: KeyCode::Enter,
        modifiers: NONE,
        intent: Action::Submit,
        doc: "Composer | Enter | Submit the draft",
    },
    // Composer | Alt+Enter | Multiline newline
    Row {
        focus: Focus::Composer,
        code: KeyCode::Enter,
        modifiers: KeyModifiers::ALT,
        intent: Action::Newline,
        doc: "Composer | Alt+Enter | Multiline newline",
    },
    // Composer | Ctrl+J | Multiline newline
    Row {
        focus: Focus::Composer,
        code: KeyCode::Char('j'),
        modifiers: KeyModifiers::CONTROL,
        intent: Action::Newline,
        doc: "Composer | Ctrl+J | Multiline newline",
    },
    // Composer | Tab | Switch focus
    Row {
        focus: Focus::Composer,
        code: KeyCode::Tab,
        modifiers: NONE,
        intent: Action::FocusSwitch,
        doc: "Composer | Tab | Switch focus",
    },
    // Composer | Esc | Park focus in the viewport (never cancel)
    Row {
        focus: Focus::Composer,
        code: KeyCode::Esc,
        modifiers: NONE,
        intent: Action::ParkFocus,
        doc: "Composer | Esc | Park focus in the viewport (never cancel)",
    },
    // Composer | Backspace | Delete last draft char
    Row {
        focus: Focus::Composer,
        code: KeyCode::Backspace,
        modifiers: NONE,
        intent: Action::Backspace,
        doc: "Composer | Backspace | Delete last draft char",
    },
    // Composer | Up/Down | Previous/next submitted input in the composer
    Row {
        focus: Focus::Composer,
        code: KeyCode::Up,
        modifiers: NONE,
        intent: Action::ScrollUp,
        doc: "Composer | Up | Previous submitted input",
    },
    Row {
        focus: Focus::Composer,
        code: KeyCode::Down,
        modifiers: NONE,
        intent: Action::ScrollDown,
        doc: "Composer | Down | Next submitted input",
    },
    // Viewport | Up/k, Down/j | Move selection (scrolls)
    Row {
        focus: Focus::Viewport,
        code: KeyCode::Up,
        modifiers: NONE,
        intent: Action::ScrollUp,
        doc: "Viewport | Up | Move selection (scrolls)",
    },
    Row {
        focus: Focus::Viewport,
        code: KeyCode::Char('k'),
        modifiers: NONE,
        intent: Action::ScrollUp,
        doc: "Viewport | k | Move selection (scrolls)",
    },
    Row {
        focus: Focus::Viewport,
        code: KeyCode::Down,
        modifiers: NONE,
        intent: Action::ScrollDown,
        doc: "Viewport | Down | Move selection (scrolls)",
    },
    Row {
        focus: Focus::Viewport,
        code: KeyCode::Char('j'),
        modifiers: NONE,
        intent: Action::ScrollDown,
        doc: "Viewport | j | Move selection (scrolls)",
    },
    // Viewport | PageUp/PageDown | Scroll one page
    Row {
        focus: Focus::Viewport,
        code: KeyCode::PageUp,
        modifiers: NONE,
        intent: Action::PageUp,
        doc: "Viewport | PageUp | Scroll one page",
    },
    Row {
        focus: Focus::Viewport,
        code: KeyCode::PageDown,
        modifiers: NONE,
        intent: Action::PageDown,
        doc: "Viewport | PageDown | Scroll one page",
    },
    // Viewport | Left/h, Right/l | Fold/unfold the selected entry
    Row {
        focus: Focus::Viewport,
        code: KeyCode::Left,
        modifiers: NONE,
        intent: Action::FoldToggle,
        doc: "Viewport | Left | Fold/unfold the selected entry",
    },
    Row {
        focus: Focus::Viewport,
        code: KeyCode::Char('h'),
        modifiers: NONE,
        intent: Action::FoldToggle,
        doc: "Viewport | h | Fold/unfold the selected entry",
    },
    Row {
        focus: Focus::Viewport,
        code: KeyCode::Right,
        modifiers: NONE,
        intent: Action::FoldToggle,
        doc: "Viewport | Right | Fold/unfold the selected entry",
    },
    Row {
        focus: Focus::Viewport,
        code: KeyCode::Char('l'),
        modifiers: NONE,
        intent: Action::FoldToggle,
        doc: "Viewport | l | Fold/unfold the selected entry",
    },
    // Viewport | m | Cycle the active model (admission order)
    Row {
        focus: Focus::Viewport,
        code: KeyCode::Char('m'),
        modifiers: NONE,
        intent: Action::CycleModel,
        doc: "Viewport | m | Cycle the active model (admission order)",
    },
    // Viewport | Tab | Switch focus
    Row {
        focus: Focus::Viewport,
        code: KeyCode::Tab,
        modifiers: NONE,
        intent: Action::FocusSwitch,
        doc: "Viewport | Tab | Switch focus",
    },
    // Viewport | q | Quit (test-only convenience)
    Row {
        focus: Focus::Viewport,
        code: KeyCode::Char('q'),
        modifiers: NONE,
        intent: Action::Quit,
        doc: "Viewport | q | Quit (test-only convenience)",
    },
    // Approval | a | Allow once (exact live call only)
    Row {
        focus: Focus::ApprovalCard,
        code: KeyCode::Char('a'),
        modifiers: NONE,
        intent: Action::ApproveOnce,
        doc: "Approval | a | Allow once (exact live call only)",
    },
    // Approval | d | Deny without executing
    Row {
        focus: Focus::ApprovalCard,
        code: KeyCode::Char('d'),
        modifiers: NONE,
        intent: Action::Deny,
        doc: "Approval | d | Deny without executing",
    },
    // Approval | i | Open/close the expanded detail (inspection only)
    Row {
        focus: Focus::ApprovalCard,
        code: KeyCode::Char('i'),
        modifiers: NONE,
        intent: Action::InspectApproval,
        doc: "Approval | i | Open/close the expanded detail (inspection only)",
    },
    // Approval | Up/k, Down/j | Scroll the expanded detail
    Row {
        focus: Focus::ApprovalCard,
        code: KeyCode::Up,
        modifiers: NONE,
        intent: Action::ScrollUp,
        doc: "Approval | Up | Scroll the expanded detail",
    },
    Row {
        focus: Focus::ApprovalCard,
        code: KeyCode::Char('k'),
        modifiers: NONE,
        intent: Action::ScrollUp,
        doc: "Approval | k | Scroll the expanded detail",
    },
    Row {
        focus: Focus::ApprovalCard,
        code: KeyCode::Down,
        modifiers: NONE,
        intent: Action::ScrollDown,
        doc: "Approval | Down | Scroll the expanded detail",
    },
    Row {
        focus: Focus::ApprovalCard,
        code: KeyCode::Char('j'),
        modifiers: NONE,
        intent: Action::ScrollDown,
        doc: "Approval | j | Scroll the expanded detail",
    },
    // Approval | PageUp/PageDown | Page the expanded detail
    Row {
        focus: Focus::ApprovalCard,
        code: KeyCode::PageUp,
        modifiers: NONE,
        intent: Action::PageUp,
        doc: "Approval | PageUp | Page the expanded detail",
    },
    Row {
        focus: Focus::ApprovalCard,
        code: KeyCode::PageDown,
        modifiers: NONE,
        intent: Action::PageDown,
        doc: "Approval | PageDown | Page the expanded detail",
    },
    // Approval | Tab | Switch focus
    Row {
        focus: Focus::ApprovalCard,
        code: KeyCode::Tab,
        modifiers: NONE,
        intent: Action::FocusSwitch,
        doc: "Approval | Tab | Switch focus",
    },
    // Approval | Esc | Close the expanded detail, else park focus
    Row {
        focus: Focus::ApprovalCard,
        code: KeyCode::Esc,
        modifiers: NONE,
        intent: Action::ParkFocus,
        doc: "Approval | Esc | Close the expanded detail, else park focus",
    },
    // Any | Ctrl+C | Cancel while cancellable (the caller checks can_cancel)
    Row {
        focus: Focus::Composer,
        code: KeyCode::Char('c'),
        modifiers: KeyModifiers::CONTROL,
        intent: Action::Cancel,
        doc: "Any | Ctrl+C | Cancel while cancellable, else quit",
    },
    Row {
        focus: Focus::Viewport,
        code: KeyCode::Char('c'),
        modifiers: KeyModifiers::CONTROL,
        intent: Action::Cancel,
        doc: "Any | Ctrl+C | Cancel while cancellable, else quit",
    },
    Row {
        focus: Focus::ApprovalCard,
        code: KeyCode::Char('c'),
        modifiers: KeyModifiers::CONTROL,
        intent: Action::Cancel,
        doc: "Any | Ctrl+C | Cancel while cancellable, else quit",
    },
    // Any | Ctrl+D | Quit
    Row {
        focus: Focus::Composer,
        code: KeyCode::Char('d'),
        modifiers: KeyModifiers::CONTROL,
        intent: Action::Quit,
        doc: "Any | Ctrl+D | Quit",
    },
    Row {
        focus: Focus::Viewport,
        code: KeyCode::Char('d'),
        modifiers: KeyModifiers::CONTROL,
        intent: Action::Quit,
        doc: "Any | Ctrl+D | Quit",
    },
    Row {
        focus: Focus::ApprovalCard,
        code: KeyCode::Char('d'),
        modifiers: KeyModifiers::CONTROL,
        intent: Action::Quit,
        doc: "Any | Ctrl+D | Quit",
    },
];

/// The intents the published table gives to exactly one focus. Each row of
/// [`PUBLISHED_ROWS`] must sit under the focus named here, which cross-checks
/// the row transcription against the focus partition independently.
///
/// The remaining families are shared (`FocusSwitch`, `Cancel`, `Quit`,
/// `ParkFocus`, and the four scrolling/paging intents) and are pinned by
/// [`SHARED_FAMILIES`].
const EXCLUSIVE_FAMILIES: &[(&str, Focus)] = &[
    ("Submit", Focus::Composer),
    ("Newline", Focus::Composer),
    ("Type", Focus::Composer),
    ("Backspace", Focus::Composer),
    ("FoldToggle", Focus::Viewport),
    ("CycleModel", Focus::Viewport),
    ("ApproveOnce", Focus::ApprovalCard),
    ("Deny", Focus::ApprovalCard),
    ("InspectApproval", Focus::ApprovalCard),
];

/// The intents the published table shares between focuses, with the complete
/// list of focuses that may reach them.
const SHARED_FAMILIES: &[(&str, &[Focus])] = &[
    ("FocusSwitch", &ALL_FOCUSES),
    ("Cancel", &ALL_FOCUSES),
    ("Quit", &ALL_FOCUSES),
    ("ParkFocus", &[Focus::Composer, Focus::ApprovalCard]),
    (
        "ScrollUp",
        &[Focus::Composer, Focus::Viewport, Focus::ApprovalCard],
    ),
    (
        "ScrollDown",
        &[Focus::Composer, Focus::Viewport, Focus::ApprovalCard],
    ),
    ("PageUp", &[Focus::Viewport, Focus::ApprovalCard]),
    ("PageDown", &[Focus::Viewport, Focus::ApprovalCard]),
];

/// Intents documented by a parameterized row rather than by a literal one.
/// The only such row is `Composer | printable char | Type into the draft`,
/// whose exhaustive half is asserted over the whole printable range by
/// `typed_text_is_produced_only_by_the_composer_and_preserves_the_character`.
const PARAMETERIZED_FAMILIES: &[&str] = &["Type"];

/// Characters that stand in for the parameterized composer row: every one of
/// them means something *elsewhere* in the keyset (the decision keys, the
/// navigation and folding keys, the viewport quit key, and a shifted letter),
/// so none of them may be special-cased as text.
const COMPOSER_TYPE_SAMPLE: &[char] = &['a', 'd', 'i', 'k', 'j', 'h', 'l', 'q', 'A', ' '];

/// Named (non-character) key codes fed to the keyset by the producer census,
/// in a fixed order: exactly the named keys the published table binds.
const NAMED_CODES: &[KeyCode] = &[
    KeyCode::Enter,
    KeyCode::Tab,
    KeyCode::Esc,
    KeyCode::Backspace,
    KeyCode::Up,
    KeyCode::Down,
    KeyCode::Left,
    KeyCode::Right,
    KeyCode::PageUp,
    KeyCode::PageDown,
];

/// The modifier chords the published table documents: `Ctrl+C`, `Ctrl+D`,
/// `Ctrl+J`, and `Alt+Enter`. Only these are swept, so the census compares
/// documented chords against documented chords; chord *negatives* are
/// `cov_keys_edges.rs`’s subject.
const DOCUMENTED_CHORDS: &[(KeyCode, KeyModifiers)] = &[
    (KeyCode::Char('c'), KeyModifiers::CONTROL),
    (KeyCode::Char('d'), KeyModifiers::CONTROL),
    (KeyCode::Char('j'), KeyModifiers::CONTROL),
    (KeyCode::Enter, KeyModifiers::ALT),
];

/// A short scripted session, in the order a user types it: draft text, a
/// submitted run, a newline, viewport navigation including the `Esc` the
/// viewport does not bind, then an approval inspected, allowed, and parked,
/// and finally the two chords that end the session.
const SESSION: &[(Focus, KeyCode, KeyModifiers)] = &[
    (Focus::Composer, KeyCode::Char('h'), NONE),
    (Focus::Composer, KeyCode::Char('i'), NONE),
    (Focus::Composer, KeyCode::Enter, NONE),
    (Focus::Composer, KeyCode::Char('j'), KeyModifiers::CONTROL),
    (Focus::Composer, KeyCode::Tab, NONE),
    (Focus::Viewport, KeyCode::Down, NONE),
    (Focus::Viewport, KeyCode::PageDown, NONE),
    (Focus::Viewport, KeyCode::Left, NONE),
    (Focus::Viewport, KeyCode::Esc, NONE),
    (Focus::Viewport, KeyCode::Tab, NONE),
    (Focus::ApprovalCard, KeyCode::Char('i'), NONE),
    (Focus::ApprovalCard, KeyCode::PageDown, NONE),
    (Focus::ApprovalCard, KeyCode::Char('a'), NONE),
    (Focus::ApprovalCard, KeyCode::Esc, NONE),
    (Focus::Composer, KeyCode::Char('a'), NONE),
    (Focus::Composer, KeyCode::Backspace, NONE),
    (Focus::Composer, KeyCode::Char('c'), KeyModifiers::CONTROL),
    (Focus::Composer, KeyCode::Char('d'), KeyModifiers::CONTROL),
];

/// Variant name of an intent, dropping the typed character so sets of intents
/// can be compared directly.
///
/// The match is exhaustive on purpose: adding an `Action` variant breaks the
/// build here, which forces [`all_intents`] and [`PUBLISHED_ROWS`] to grow with
/// it instead of leaving the table quietly incomplete.
fn intent_family(intent: &Action) -> &'static str {
    match intent {
        Action::Submit => "Submit",
        Action::Newline => "Newline",
        Action::FocusSwitch => "FocusSwitch",
        Action::ParkFocus => "ParkFocus",
        Action::ScrollUp => "ScrollUp",
        Action::ScrollDown => "ScrollDown",
        Action::PageUp => "PageUp",
        Action::PageDown => "PageDown",
        Action::FoldToggle => "FoldToggle",
        Action::CycleModel => "CycleModel",
        Action::ApproveOnce => "ApproveOnce",
        Action::Deny => "Deny",
        Action::InspectApproval => "InspectApproval",
        Action::Cancel => "Cancel",
        Action::Quit => "Quit",
        Action::Type(_) => "Type",
        Action::Backspace => "Backspace",
    }
}

/// One instance of every intent the keyset can produce, in declaration order.
fn all_intents() -> Vec<Action> {
    vec![
        Action::Submit,
        Action::Newline,
        Action::FocusSwitch,
        Action::ParkFocus,
        Action::ScrollUp,
        Action::ScrollDown,
        Action::PageUp,
        Action::PageDown,
        Action::FoldToggle,
        Action::CycleModel,
        Action::ApproveOnce,
        Action::Deny,
        Action::InspectApproval,
        Action::Cancel,
        Action::Quit,
        Action::Type('x'),
        Action::Backspace,
    ]
}

/// Compact, stable label for one `(focus, key, modifiers)` triple. The oracle
/// rows and the census observations use the same label, so a mismatch reads as
/// a missing or extra key rather than as noise.
fn row_label(focus: Focus, code: KeyCode, modifiers: KeyModifiers) -> String {
    let chord = if modifiers.is_empty() {
        String::new()
    } else {
        format!("{modifiers:?}+")
    };
    format!("{focus:?} {chord}{code:?}")
}

/// The intent the keyset produces for one triple, reduced to its family name,
/// or `None` when the event is unbound.
fn observed(focus: Focus, code: KeyCode, modifiers: KeyModifiers) -> Option<String> {
    map_key(focus, KeyEvent::new(code, modifiers))
        .as_ref()
        .map(intent_family)
        .map(String::from)
}

/// The fixed keyspace the producer census sweeps: every ASCII character and
/// every named key the table binds, unmodified, plus the four documented
/// chords, in every focus.
fn census_space() -> Vec<(Focus, KeyEvent)> {
    let mut space = Vec::new();
    for focus in ALL_FOCUSES {
        for char in '\x00'..='\x7f' {
            space.push((focus, KeyEvent::new(KeyCode::Char(char), NONE)));
        }
        for code in NAMED_CODES {
            space.push((focus, KeyEvent::new(*code, NONE)));
        }
        for (code, modifiers) in DOCUMENTED_CHORDS {
            space.push((focus, KeyEvent::new(*code, *modifiers)));
        }
    }
    space
}

/// Sorted, duplicate-free copy of a set of labels.
fn sorted(mut labels: Vec<String>) -> Vec<String> {
    labels.sort();
    labels.dedup();
    labels
}

/// Every `(focus, key)` the swept keyspace reaches `family` from.
fn producers_of(family: &str, space: &[(Focus, KeyEvent)]) -> Vec<String> {
    sorted(
        space
            .iter()
            .filter(|(focus, event)| {
                map_key(*focus, *event)
                    .as_ref()
                    .is_some_and(|action| intent_family(action) == family)
            })
            .map(|(focus, event)| row_label(*focus, event.code, event.modifiers))
            .collect(),
    )
}

/// Every `(focus, key)` in [`PUBLISHED_ROWS`] that documents `family`.
fn documented_producers(family: &str) -> Vec<String> {
    sorted(
        PUBLISHED_ROWS
            .iter()
            .filter(|row| intent_family(&row.intent) == family)
            .map(Row::label)
            .collect(),
    )
}

/// The focuses the swept keyspace can reach `family` from.
fn focuses_producing(family: &str, space: &[(Focus, KeyEvent)]) -> Vec<String> {
    sorted(
        space
            .iter()
            .filter(|(focus, event)| {
                map_key(*focus, *event)
                    .as_ref()
                    .is_some_and(|action| intent_family(action) == family)
            })
            .map(|(focus, _)| format!("{focus:?}"))
            .collect(),
    )
}

/// Every modifier the `Esc` sweep pairs with the key: the unscreened ones and
/// the two the keyset screens before the bare table.
const ESC_MODIFIERS: &[KeyModifiers] = &[
    NONE,
    KeyModifiers::SHIFT,
    KeyModifiers::SUPER,
    KeyModifiers::META,
    KeyModifiers::HYPER,
    KeyModifiers::CONTROL,
    KeyModifiers::ALT,
];

#[test]
fn every_published_row_maps_to_its_documented_intent() {
    for row in PUBLISHED_ROWS {
        assert_eq!(
            map_key(row.focus, row.event()),
            Some(row.intent.clone()),
            "{}: {:?} + {:?} must mean exactly {:?}",
            row.doc,
            row.focus,
            row.event(),
            row.intent
        );
    }
}

#[test]
fn published_table_has_no_duplicate_or_conflicting_rows() {
    // 38 literal rows: 8 composer, 13 viewport, 11 approval, and the 6
    // focus-expanded `Any` rows (`Ctrl+C` and `Ctrl+D` in each focus). The
    // 39th published row is parameterized over printable characters.
    assert_eq!(
        PUBLISHED_ROWS.len(),
        38,
        "the transcription lost or invented a published row"
    );
    // The oracle is only meaningful if it is a function: no `(focus, key,
    // modifiers)` triple may claim two different intents.
    let mut keys: Vec<String> = PUBLISHED_ROWS.iter().map(|row| row.label()).collect();
    keys.sort();
    for pair in keys.windows(2) {
        assert_ne!(pair[0], pair[1], "duplicate row in the published table");
    }
}

#[test]
fn every_intent_variant_is_documented_and_reachable() {
    let space = census_space();
    let expected: Vec<String> = sorted(
        all_intents()
            .iter()
            .map(|intent| String::from(intent_family(intent)))
            .collect(),
    );
    // The keyset reaches every intent the table documents, and nothing else:
    // no dead intent, and no intent invented outside `Action`.
    let reachable: Vec<String> = sorted(
        space
            .iter()
            .filter_map(|(focus, event)| {
                map_key(*focus, *event)
                    .as_ref()
                    .map(intent_family)
                    .map(String::from)
            })
            .collect(),
    );
    assert_eq!(
        reachable, expected,
        "the swept keyspace must reach exactly the published intents"
    );
    // And every intent is claimed by at least one published row, literal or
    // parameterized, so the table above has no silent gap.
    let mut documented: Vec<String> = PUBLISHED_ROWS
        .iter()
        .map(|row| String::from(intent_family(&row.intent)))
        .collect();
    documented.extend(
        PARAMETERIZED_FAMILIES
            .iter()
            .map(|family| String::from(*family)),
    );
    assert_eq!(
        sorted(documented),
        expected,
        "every intent needs a published row or a parameterized one"
    );
}

#[test]
fn each_intent_is_produced_by_exactly_its_documented_keys() {
    let space = census_space();
    let mut families: Vec<&str> = PUBLISHED_ROWS
        .iter()
        .map(|row| intent_family(&row.intent))
        .collect();
    families.sort_unstable();
    families.dedup();
    for family in families {
        assert_eq!(
            producers_of(family, &space),
            documented_producers(family),
            "{family} must be produced by exactly the keys the published table \
             binds it to"
        );
    }
}

#[test]
fn each_row_sits_under_the_focus_that_owns_its_intent() {
    // Cross-checks the row transcription against the focus partition: an
    // accidental `Viewport` row for `Deny`, or an `ApprovalCard` row for
    // `Submit`, cannot slip through both views at once.
    for row in PUBLISHED_ROWS {
        let family = intent_family(&row.intent);
        if let Some((_, owner)) = EXCLUSIVE_FAMILIES.iter().find(|(name, _)| *name == family) {
            assert_eq!(
                row.focus, *owner,
                "{}: {family} is owned by {owner:?}, not {:?}",
                row.doc, row.focus
            );
        }
    }
}

#[test]
fn intents_are_partitioned_by_focus() {
    let space = census_space();
    // The complete intent set each focus can reach.
    for (focus, expected) in [
        (
            Focus::Composer,
            vec![
                "Backspace",
                "Cancel",
                "FocusSwitch",
                "Newline",
                "ParkFocus",
                "Quit",
                "ScrollDown",
                "ScrollUp",
                "Submit",
                "Type",
            ],
        ),
        (
            Focus::Viewport,
            vec![
                "Cancel",
                "CycleModel",
                "FocusSwitch",
                "FoldToggle",
                "PageDown",
                "PageUp",
                "Quit",
                "ScrollDown",
                "ScrollUp",
            ],
        ),
        (
            Focus::ApprovalCard,
            vec![
                "ApproveOnce",
                "Cancel",
                "Deny",
                "FocusSwitch",
                "InspectApproval",
                "PageDown",
                "PageUp",
                "ParkFocus",
                "Quit",
                "ScrollDown",
                "ScrollUp",
            ],
        ),
    ] {
        let reachable = sorted(
            space
                .iter()
                .filter(|(candidate, _)| *candidate == focus)
                .filter_map(|(_, event)| {
                    map_key(focus, *event)
                        .as_ref()
                        .map(intent_family)
                        .map(String::from)
                })
                .collect(),
        );
        assert_eq!(
            reachable,
            sorted(
                expected
                    .iter()
                    .map(|family| String::from(*family))
                    .collect()
            ),
            "{focus:?} may reach exactly the intents its own rows document"
        );
    }
    // The same partition seen from the intents: each one is reachable from
    // exactly the focuses the table gives it to.
    for (family, owner) in EXCLUSIVE_FAMILIES {
        let expected = vec![format!("{owner:?}")];
        assert_eq!(
            focuses_producing(family, &space),
            expected,
            "{family} must be reachable from {owner:?} alone"
        );
    }
    for (family, owners) in SHARED_FAMILIES {
        let expected: Vec<String> = owners.iter().map(|focus| format!("{focus:?}")).collect();
        assert_eq!(
            focuses_producing(family, &space),
            sorted(expected),
            "{family} must be reachable from exactly the focuses that document it"
        );
    }
}

#[test]
fn esc_parks_and_never_answers_or_decides() {
    // The two published `Esc` rows park focus; the viewport documents no `Esc`
    // row, so it is unbound there.
    assert_eq!(
        map_key(Focus::Composer, KeyEvent::new(KeyCode::Esc, NONE)),
        Some(Action::ParkFocus),
        "the composer parks focus in the viewport"
    );
    assert_eq!(
        map_key(Focus::ApprovalCard, KeyEvent::new(KeyCode::Esc, NONE)),
        Some(Action::ParkFocus),
        "the approval card closes its detail or parks; it never decides"
    );
    assert_eq!(
        map_key(Focus::Viewport, KeyEvent::new(KeyCode::Esc, NONE)),
        None,
        "the viewport documents no Esc row"
    );
    // The loud form of the contract: no `Esc` event, in any focus and under
    // any modifier, answers a pending approval, cancels, submits, or ends the
    // session.
    for focus in ALL_FOCUSES {
        for modifiers in ESC_MODIFIERS {
            let action = map_key(focus, KeyEvent::new(KeyCode::Esc, *modifiers));
            assert!(
                !matches!(
                    action,
                    Some(Action::Cancel)
                        | Some(Action::ApproveOnce)
                        | Some(Action::Deny)
                        | Some(Action::InspectApproval)
                        | Some(Action::Submit)
                        | Some(Action::Quit)
                ),
                "Esc under {focus:?} with {modifiers:?} produced {action:?}"
            );
        }
    }
    // `Esc` is also the only key that can park: `ParkFocus` has exactly the two
    // published producers, so parking can never be reached by accident.
    assert_eq!(
        producers_of("ParkFocus", &census_space()),
        vec![
            row_label(Focus::ApprovalCard, KeyCode::Esc, NONE),
            row_label(Focus::Composer, KeyCode::Esc, NONE),
        ],
        "ParkFocus is Esc's alone"
    );
}

#[test]
fn typed_text_is_produced_only_by_the_composer_and_preserves_the_character() {
    let space = census_space();
    // Within the swept keyspace, the composer types every printable ASCII
    // character and nothing else does: the `Type` producers are exactly the
    // 95 printable characters under the composer.
    let expected: Vec<String> = (' '..='~')
        .map(|char| row_label(Focus::Composer, KeyCode::Char(char), NONE))
        .collect();
    assert_eq!(
        producers_of("Type", &space),
        sorted(expected),
        "only printable characters under the composer may be typed"
    );
    // The intent carries the character that was pressed, unchanged.
    for char in ' '..='~' {
        assert_eq!(
            map_key(Focus::Composer, KeyEvent::new(KeyCode::Char(char), NONE)),
            Some(Action::Type(char)),
            "{char:?} must enter the draft as itself"
        );
    }
    // Multilingual text, punctuation, and symbols outside ASCII are ordinary
    // text as well.
    for char in ['é', 'ß', 'Ω', '日', '本', '語', '🌍', '✓', '—', '«', '¡'] {
        assert_eq!(
            map_key(Focus::Composer, KeyEvent::new(KeyCode::Char(char), NONE)),
            Some(Action::Type(char)),
            "{char:?} is ordinary text"
        );
    }
    // The letters that decide, navigate, fold, or quit in another focus are
    // plain draft text here: the composer row has no exceptions.
    for char in COMPOSER_TYPE_SAMPLE {
        assert_eq!(
            map_key(Focus::Composer, KeyEvent::new(KeyCode::Char(*char), NONE)),
            Some(Action::Type(*char)),
            "{char:?} must type, never decide, navigate, fold, or quit"
        );
    }
    // Only the composer types: no other focus reaches the intent at all.
    assert_eq!(
        focuses_producing("Type", &space),
        vec![String::from("Composer")],
        "text entry belongs to the composer alone"
    );
}

#[test]
fn the_composer_line_keys_are_three_distinct_events() {
    let enter = KeyEvent::new(KeyCode::Enter, NONE);
    let alt_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
    let ctrl_j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL);
    assert_eq!(map_key(Focus::Composer, enter), Some(Action::Submit));
    assert_eq!(map_key(Focus::Composer, alt_enter), Some(Action::Newline));
    assert_eq!(map_key(Focus::Composer, ctrl_j), Some(Action::Newline));
    // Submit and newline are never interchangeable: plain `Enter` is the only
    // submit, and the two chords are the only newlines.
    assert_ne!(
        map_key(Focus::Composer, enter),
        Some(Action::Newline),
        "plain Enter submits; it never inserts a newline"
    );
    let space = census_space();
    assert_eq!(
        producers_of("Submit", &space),
        documented_producers("Submit"),
        "Submit stays composer-only plain Enter"
    );
    assert_eq!(
        producers_of("Newline", &space),
        documented_producers("Newline"),
        "Newline stays composer-only Alt+Enter and Ctrl+J"
    );
    // The named composer keys are commands, never text: the `Type` row is
    // parameterized over characters only.
    for (code, expected) in [
        (KeyCode::Enter, Action::Submit),
        (KeyCode::Tab, Action::FocusSwitch),
        (KeyCode::Esc, Action::ParkFocus),
        (KeyCode::Backspace, Action::Backspace),
    ] {
        assert_eq!(
            map_key(Focus::Composer, KeyEvent::new(code, NONE)),
            Some(expected),
            "{code:?} is a composer command, not text"
        );
    }
}

#[test]
fn approval_rows_answer_only_under_the_approval_card_and_never_alias() {
    let space = census_space();
    // `a`, `d`, and `i` are three distinct answers, each from its own key.
    for (code, expected) in [
        (KeyCode::Char('a'), Action::ApproveOnce),
        (KeyCode::Char('d'), Action::Deny),
        (KeyCode::Char('i'), Action::InspectApproval),
    ] {
        let event = KeyEvent::new(code, NONE);
        let action = map_key(Focus::ApprovalCard, event);
        assert_eq!(
            action,
            Some(expected.clone()),
            "{code:?} answers with {expected:?}"
        );
        for other in [Action::ApproveOnce, Action::Deny, Action::InspectApproval] {
            if other != expected {
                assert_ne!(
                    action,
                    Some(other.clone()),
                    "{code:?} must not also mean {other:?}"
                );
            }
        }
    }
    // Inspection is inspection: it is not reachable from the other two keys,
    // and neither of them is reachable from `i`.
    assert_eq!(
        producers_of("InspectApproval", &space),
        vec![row_label(Focus::ApprovalCard, KeyCode::Char('i'), NONE)],
        "only `i` inspects"
    );
    assert_eq!(
        producers_of("ApproveOnce", &space),
        vec![row_label(Focus::ApprovalCard, KeyCode::Char('a'), NONE)],
        "only `a` allows"
    );
    assert_eq!(
        producers_of("Deny", &space),
        vec![row_label(Focus::ApprovalCard, KeyCode::Char('d'), NONE)],
        "only `d` denies"
    );
    // The arrow rows are shared across composer, viewport, and the approval
    // card, and decide nothing; paging rows belong to the viewport and the
    // approval card only (the composer pages nothing; the wheel covers it).
    for family in ["ScrollUp", "ScrollDown"] {
        assert_eq!(
            focuses_producing(family, &space),
            vec![
                String::from("ApprovalCard"),
                String::from("Composer"),
                String::from("Viewport")
            ],
            "{family} navigates or scrolls detail and decides nothing"
        );
    }
    for family in ["PageUp", "PageDown"] {
        assert_eq!(
            focuses_producing(family, &space),
            vec![String::from("ApprovalCard"), String::from("Viewport")],
            "{family} pages the viewport or detail and decides nothing"
        );
    }
    assert_eq!(
        map_key(Focus::ApprovalCard, KeyEvent::new(KeyCode::Esc, NONE)),
        Some(Action::ParkFocus),
        "Esc closes the detail or parks; it never decides"
    );
}

#[test]
fn the_tab_row_hands_the_keyboard_on_and_parking_never_re_arms_the_card() {
    // Every focus's `Tab` row is `FocusSwitch`, and following it always lands
    // the keyboard on a different card.
    for focus in ALL_FOCUSES {
        assert_eq!(
            map_key(focus, KeyEvent::new(KeyCode::Tab, NONE)),
            Some(Action::FocusSwitch),
            "Tab is the focus row in {focus:?}"
        );
        assert_ne!(
            next_focus(focus, true),
            focus,
            "a focus switch from {focus:?} must change cards"
        );
    }
    // `ParkFocus` steps back one rung: the composer parks into the viewport and
    // the approval card parks back into the composer, so parking can never
    // return the keyboard to a card it just left.
    assert_eq!(
        map_key(Focus::Composer, KeyEvent::new(KeyCode::Esc, NONE)),
        Some(Action::ParkFocus)
    );
    assert_eq!(next_focus(Focus::Composer, true), Focus::Viewport);
    assert_eq!(
        map_key(Focus::ApprovalCard, KeyEvent::new(KeyCode::Esc, NONE)),
        Some(Action::ParkFocus)
    );
    assert_eq!(next_focus(Focus::ApprovalCard, true), Focus::Composer);
    assert_ne!(
        next_focus(Focus::ApprovalCard, false),
        Focus::ApprovalCard,
        "a parked approval card is never re-entered by parking"
    );
}

#[test]
fn a_scripted_session_maps_row_for_row() {
    // The session in order: draft text, submit, newline, hand over to the
    // viewport, navigate and fold, an `Esc` the viewport does not bind, hand
    // over to the approval card, inspect, page, allow, park, and end.
    let observed_session: Vec<Option<String>> = SESSION
        .iter()
        .map(|(focus, code, modifiers)| observed(*focus, *code, *modifiers))
        .collect();
    assert_eq!(
        observed_session,
        vec![
            Some(String::from("Type")),
            Some(String::from("Type")),
            Some(String::from("Submit")),
            Some(String::from("Newline")),
            Some(String::from("FocusSwitch")),
            Some(String::from("ScrollDown")),
            Some(String::from("PageDown")),
            Some(String::from("FoldToggle")),
            None,
            Some(String::from("FocusSwitch")),
            Some(String::from("InspectApproval")),
            Some(String::from("PageDown")),
            Some(String::from("ApproveOnce")),
            Some(String::from("ParkFocus")),
            Some(String::from("Type")),
            Some(String::from("Backspace")),
            Some(String::from("Cancel")),
            Some(String::from("Quit")),
        ],
        "the scripted session must produce the documented intent sequence"
    );
}

#[test]
fn mapping_depends_only_on_the_focus_and_the_event() {
    // A noisy prefix drawn from other focuses, other keys, and unbound codes
    // must not change how the next event maps: the keyset carries no state
    // between keystrokes, so a key means the same thing wherever it appears in
    // a session.
    let space = census_space();
    let noise: Vec<(Focus, KeyEvent)> = space.iter().step_by(37).copied().collect();
    assert!(!noise.is_empty(), "the noise prefix must not be empty");
    for (focus, event) in &space {
        let alone = map_key(*focus, *event);
        for (other_focus, other_event) in &noise {
            let _ = map_key(*other_focus, *other_event);
        }
        assert_eq!(
            map_key(*focus, *event),
            alone,
            "{focus:?} + {event:?} changed after unrelated keys"
        );
    }
}
