#![forbid(unsafe_code)]

//! Edge-case hardening for the presentation-boundary sanitizer, exercised
//! through the published `nexus_tui` surface only: [`sanitize`],
//! [`sanitize_approval`], and the [`MAX_APPROVAL_FIELD_BYTES`] bound those
//! escapes are sized against. The crate-internal predicates are re-declared
//! here as literal tables, so the suite never reaches past the public API.
//!
//! The unit tests inside `sanitize.rs` cover the ordinary paths. These cases
//! pin the edges where a hostile or merely malformed byte stream could behave
//! differently:
//!
//! - **truncation at the end of the input** — every introducer form (`ESC`,
//!   `ESC [`, `ESC ]`, `ESC (`, `ESC )`, `ESC #`, C1 `U+009B`, C1 `U+009D`)
//!   is walked to the end of an unterminated body. Nothing may panic and
//!   nothing from the sequence may reach the rendered line;
//! - **suffix preservation** — a character that cannot belong to a sequence
//!   ends it *without being consumed*, so following text, newlines, tabs, and
//!   multilingual characters survive, and a malformed sequence cannot swallow
//!   the next well-formed one;
//! - **multilingual text** — Latin accents, Greek, Cyrillic, CJK, Hangul,
//!   Devanagari, Arabic and Hebrew, astral-plane emoji, variation selectors,
//!   combining marks, and regional-indicator pairs pass through both
//!   sanitizers byte-for-byte;
//! - **approval escaping** — every invisible formatting/separator character in
//!   the strong pass is visibly escaped, and only there (the general pass keeps
//!   ZWJ so emoji shaping still renders), with the characters beside each
//!   range left literal;
//! - **worst-case expansion** — a notice built entirely from 2-byte format
//!   characters expands exactly 4x and still fits
//!   [`MAX_APPROVAL_FIELD_BYTES`] uncut, and no escapable character expands
//!   by more than that factor.
//!
//! Every assertion is against a literal expected string or an invariant over
//! a literal table. There are no clocks, threads, files, terminals, or random
//! inputs, so a passing run is fully deterministic.

use nexus_tui::sanitize::{sanitize, sanitize_approval};
use nexus_tui::state::MAX_APPROVAL_FIELD_BYTES;

/// Worst-case sanitizer expansion factor in bytes: a 2-byte format character
/// (U+061C, U+00AD) becomes the 8-byte visible `\u{XXXX}` escape, and no
/// escapable character expands further than that.
const WORST_CASE_EXPANSION: usize = 4;
/// Byte width of the visible escape emitted for a 2-byte control.
const ESCAPE_BYTES: usize = 8;
/// The core notice bound the approval field is sized against
/// (`nexus_core::commands::MAX_SUMMARY_BYTES`), derived from the published
/// field bound so the two cannot drift apart unnoticed.
const CORE_NOTICE_BYTES: usize = MAX_APPROVAL_FIELD_BYTES / WORST_CASE_EXPANSION;

/// Every character the general pass must turn into a visible `\u{XXXX}` form:
/// the complete bidirectional-format set — directional marks, embeddings,
/// overrides, isolates, and the deprecated bidi block.
const BIDI_CONTROLS: [char; 18] = [
    '\u{061C}', // Arabic letter mark
    '\u{200E}', '\u{200F}', // LRM / RLM
    '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', // LRE / RLE / PDF / LRO / RLO
    '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}', // LRI / RLI / FSI / PDI
    '\u{206A}', '\u{206B}', '\u{206C}', '\u{206D}', '\u{206E}',
    '\u{206F}', // deprecated bidi format controls
];

/// Every character only the approval pass escapes: invisible formatting and
/// separators that could otherwise hide or split the operation shown for
/// confirmation. Boundaries of both the two-byte and the three-byte ranges are
/// included, plus the extremes of the invisible musical-formatting block.
const INVISIBLE_FORMAT: [char; 11] = [
    '\u{00AD}', // soft hyphen
    '\u{200B}',
    '\u{200C}',
    '\u{200D}', // ZWSP / ZWNJ / ZWJ
    '\u{2028}',
    '\u{2029}', // line / paragraph separator
    '\u{2060}', // word joiner
    '\u{FEFF}', // zero width no-break space
    '\u{1D173}',
    '\u{1D176}',
    '\u{1D17A}', // invisible musical formatting
];

/// Characters immediately beside the invisible ranges. None is an invisible
/// formatting character, so both passes must leave them literal; escaping one
/// would mean an off-by-one range.
const INVISIBLE_NEIGHBOURS: [char; 8] = [
    '\u{00AC}',  // just below the soft hyphen
    '\u{00AE}',  // just above it
    '\u{200A}',  // hair space, just below the zero-width block
    '\u{2027}',  // figure space, just below the separator pair
    '\u{205F}',  // medium mathematical space, just below the word joiner
    '\u{2061}',  // function application, just above it
    '\u{1D172}', // visible musical symbol, just below the block
    '\u{1D17B}', // visible musical symbol, just above it
];

/// Characters beside the bidirectional ranges. The character directly below
/// the embedding range (`U+2029`) is deliberately absent: it belongs to the
/// invisible-format set, so the approval pass escapes it for the other reason.
const BIDI_NEIGHBOURS: [char; 4] = [
    '\u{061B}', // Arabic semicolon, just below the letter mark
    '\u{061D}', // Arabic end of ayah, just above it
    '\u{2030}', // per mille sign, just above the embedding range
    '\u{2064}', // invisible plus, just below the isolate range
];

/// (label, raw input, expected output) for every way an escape sequence can be
/// cut short at the end of the input: the introducer arrives, the body never
/// finishes, and the sanitizer must reach the end of the string without
/// panicking or emitting any part of the sequence.
const TRUNCATIONS: &[(&str, &str, &str)] = &[
    ("empty", "", ""),
    ("bare-esc", "keep\x1b", "keep"),
    ("bare-esc-one-param", "keep\x1b1", "keep"),
    ("bare-esc-two-params", "keep\x1b31", "keep1"),
    ("bare-esc-accents", "keep\x1bé", "keep"),
    ("bare-esc-emoji", "keep\x1b🌍", "keep"),
    ("bare-esc-newline", "keep\x1b\n", "keep"),
    ("esc-then-esc", "keep\x1b\x1b", "keep"),
    ("esc-then-esc-csi", "keep\x1b\x1b[0m", "keep[0m"),
    ("esc-then-esc-osc", "keep\x1b\x1b]0;t\x07", "keep]0;t"),
    ("esc-bracket", "keep\x1b[", "keep"),
    ("esc-bracket-params", "keep\x1b[31", "keep"),
    ("esc-bracket-params-semicolon", "keep\x1b[1;2", "keep"),
    ("esc-bracket-private-param", "keep\x1b[?", "keep"),
    ("esc-bracket-intermediate", "keep\x1b[1 ", "keep"),
    ("esc-bracket-hash-param", "keep\x1b[0#", "keep"),
    ("esc-bracket-colon-sgr", "keep\x1b[38:2::10:20:30", "keep"),
    ("esc-bracket-then-del", "keep\x1b[31\x7f", "keep"),
    ("c1-csi", "keep\u{9b}", "keep"),
    ("c1-csi-params", "keep\u{9b}1;2", "keep"),
    ("esc-bracket-right", "keep\x1b]", "keep"),
    ("esc-bracket-right-body", "keep\x1b]0;title", "keep"),
    (
        "esc-bracket-right-partial-url",
        "keep\x1b]8;;http://x",
        "keep",
    ),
    ("esc-bracket-right-dangling-esc", "keep\x1b]0;t\x1b", "keep"),
    ("esc-bracket-right-then-nul", "keep\x1b]0;t\x00", "keep"),
    ("c1-osc", "keep\u{9d}", "keep"),
    ("c1-osc-body", "keep\u{9d}0;title", "keep"),
    ("esc-charset-left", "keep\x1b(", "keep"),
    ("esc-charset-left-byte", "keep\x1b(B", "keep"),
    ("esc-charset-left-truncated-byte", "keep\x1b(0", "keep"),
    ("esc-charset-right", "keep\x1b)", "keep"),
    ("esc-charset-hash", "keep\x1b#", "keep"),
    ("esc-charset-hash-byte", "keep\x1b#8", "keep"),
];

/// (label, raw input, expected output) for suffixes that must survive a
/// malformed sequence. The offending character ends the sequence without being
/// consumed, so the text after it is still rendered.
const SUFFIXES: &[(&str, &str, &str)] = &[
    // CSI: newline, tab, DEL, NUL, and BEL cannot be part of the body.
    ("csi-newline", "keep\x1b[31\ntail", "keep\ntail"),
    ("csi-tab", "keep\x1b[31\ttail", "keep\ttail"),
    ("csi-nul", "keep\x1b[31\x00tail", "keeptail"),
    ("csi-bel", "keep\x1b[31\x07tail", "keeptail"),
    ("csi-del", "keep\x1b[31\x7ftail", "keeptail"),
    ("csi-accented", "keep\x1b[31étail", "keepétail"),
    ("csi-cjk", "keep\x1b[31日本tail", "keep日本tail"),
    ("csi-emoji", "keep\x1b[31🌍tail", "keep🌍tail"),
    ("c1-csi-newline", "keep\u{9b}31\ntail", "keep\ntail"),
    ("c1-csi-emoji", "keep\u{9b}31🌍tail", "keep🌍tail"),
    // OSC: any control other than ESC ends the string unconsumed.
    ("osc-newline", "keep\x1b]0;t\ntail", "keep\ntail"),
    ("osc-tab", "keep\x1b]0;t\ttail", "keep\ttail"),
    ("osc-del", "keep\x1b]0;t\x7ftail", "keeptail"),
    ("osc-bel", "keep\x1b]0;t\x07tail", "keeptail"),
    ("osc-c1-st", "keep\x1b]0;t\u{9c}tail", "keeptail"),
    (
        "osc-esc-backslash-st",
        "keep\x1b]8;;http://x\x1b\\tail",
        "keeptail",
    ),
    ("c1-osc-newline", "keep\u{9d}0;t\ntail", "keep\ntail"),
    ("c1-osc-bel", "keep\u{9d}0;t\x07tail", "keeptail"),
    // A nested introducer ends the outer body and opens a sequence of its own.
    ("csi-then-c1-csi", "keep\x1b[1\u{9b}2mtail", "keeptail"),
    ("csi-then-esc-csi", "keep\x1b[1\x1b[2mtail", "keeptail"),
    ("csi-then-esc-osc", "keep\x1b[1\x1b]0;t\x07tail", "keeptail"),
    // Two malformed sequences in a row keep both surviving separators.
    ("csi-osc-pair", "keep\x1b[1\nx\x1b]0;t\ty", "keep\nx\ty"),
];

/// (label, raw input, expected output) for the bare-ESC rule: it takes exactly
/// one follower, which is the whole operation for a two-byte sequence such as
/// `ESC M` or `ESC c`, and only its first byte for anything longer.
const BARE_ESC: &[(&str, &str, &str)] = &[
    ("esc-m-reverse-index", "\x1bM", ""),
    ("esc-c-full-reset", "\x1bc", ""),
    ("esc-m-then-text", "\x1bMab", "ab"),
    ("esc-two-digits", "\x1b31", "1"),
    ("esc-accents", "\x1bé", ""),
    ("esc-emoji", "\x1b🌍", ""),
    ("esc-newline", "\x1b\n", ""),
    ("esc-esc-bracket", "\x1b\x1b[0m", "[0m"),
    ("esc-esc-bracket-right", "\x1b\x1b]0;t\x07", "]0;t"),
];

/// Multilingual and typographic text with no control characters, which both
/// sanitizers must return byte-for-byte.
const MULTILINGUAL: &[&str] = &[
    "plain ascii text",
    "héllo wörld",
    "Grüße aus München",
    "Ελληνικά",
    "Привет, мир",
    "日本語のテキスト",
    "안녕하세요",
    "हिन्दी",
    "العربية",
    "עברית",
    "🌍🌎🌏",
    "🌍️",
    "e\u{0301}\u{0327}",
    "café — “quoted” ‘text’ … ½ ± ²",
    "🇯🇵 flag",
    "first line\nsecond line\tcolumn",
];

/// Asserts the invariants every sanitized line must satisfy: no `ESC`
/// introducer, and no control character at all apart from the newline and tab
/// the presenter lays out with. `char::is_control` covers C0, DEL, and the C1
/// block, so a raw C1 character fails here too.
fn assert_safe_line(clean: &str, context: &str) {
    assert!(
        !clean.as_bytes().contains(&0x1B),
        "{context}: ESC leaked into {clean:?}"
    );
    for char in clean.chars() {
        assert!(
            !char.is_control() || char == '\n' || char == '\t',
            "{context}: control {char:?} leaked into {clean:?}"
        );
    }
}

/// Asserts that no bidirectional formatting control survives, which would let
/// displayed text be reordered or hidden.
fn assert_no_bidi(clean: &str, context: &str) {
    for char in clean.chars() {
        assert!(
            !BIDI_CONTROLS.contains(&char),
            "{context}: bidi control {char:?} survived in {clean:?}"
        );
    }
}

/// Asserts that no invisible formatting or separator character survives, which
/// would let the exact operation shown for confirmation be hidden or split.
fn assert_no_invisible(clean: &str, context: &str) {
    for char in clean.chars() {
        assert!(
            !INVISIBLE_FORMAT.contains(&char),
            "{context}: invisible formatting {char:?} survived in {clean:?}"
        );
    }
}

/// The visible escape form both sanitizers emit for `control`.
fn escape_of(control: char) -> String {
    format!("\\u{{{:04X}}}", control as u32)
}

/// Every raw payload the shared invariant tests walk.
fn payload_corpus() -> Vec<&'static str> {
    let mut payloads: Vec<&'static str> = MULTILINGUAL.to_vec();
    payloads.extend(TRUNCATIONS.iter().map(|(_, raw, _)| *raw));
    payloads.extend(SUFFIXES.iter().map(|(_, raw, _)| *raw));
    payloads.extend(BARE_ESC.iter().map(|(_, raw, _)| *raw));
    payloads.extend([
        "\x1b",
        "\u{9b}",
        "\u{9d}",
        "\x7f",
        " ",
        "\n",
        "\t",
        "run \x1b[2J\x1b]0;title\x07 \u{202E}gnp.exe \x1bM\x00 ok",
        "\u{202E}ls -la\u{202C}",
        "\u{061C}\u{200E}\u{200F}\u{2066}\u{2069}",
        "\u{1D173}\u{1D17A}\u{2028}\u{2029}",
    ]);
    payloads
}

#[test]
fn truncated_sequences_at_the_end_are_dropped_without_panicking() {
    for (label, raw, expected) in TRUNCATIONS {
        assert_eq!(sanitize(raw), *expected, "{label}: general pass");
        assert_eq!(sanitize_approval(raw), *expected, "{label}: approval pass");
    }
}

#[test]
fn truncated_and_malformed_payloads_never_leak_controls_into_the_output() {
    for raw in payload_corpus() {
        let clean = sanitize(raw);
        assert_safe_line(&clean, "sanitize");
        assert_no_bidi(&clean, "sanitize");
        let approved = sanitize_approval(raw);
        assert_safe_line(&approved, "sanitize_approval");
        assert_no_bidi(&approved, "sanitize_approval");
        // The strong pass is the one that must leave no invisible formatting
        // behind; the general pass deliberately keeps ZWJ for emoji shaping.
        assert_no_invisible(&approved, "sanitize_approval");
    }
}

#[test]
fn sanitizing_an_already_clean_line_changes_nothing() {
    for raw in payload_corpus() {
        let once = sanitize(raw);
        assert_eq!(sanitize(&once), once, "the general pass is idempotent");
        let approved = sanitize_approval(raw);
        assert_eq!(
            sanitize_approval(&approved),
            approved,
            "the approval pass is idempotent"
        );
    }
}

#[test]
fn the_csi_final_byte_range_ends_at_exactly_its_boundaries() {
    // The final-byte range runs from '@' (0x40) to '~' (0x7E). Reading either
    // edge wrong is what makes a sequence swallow the text after it: a body
    // character below the range is a parameter, and DEL sits one byte above it
    // and can belong to nothing. Every final byte is walked in both the ESC [
    // and the C1 introducer form. The suffix starts outside the range so it can
    // only survive if the final byte really ended the sequence.
    for final_byte in '\x40'..='\x7e' {
        let esc = format!("keep\x1b[1;2{final_byte}élan");
        assert_eq!(sanitize(&esc), "keepélan", "ESC [ final {final_byte:?}");
        let c1 = format!("keep\u{9b}1;2{final_byte}élan");
        assert_eq!(sanitize(&c1), "keepélan", "C1 CSI final {final_byte:?}");
    }
    // One byte below the range: still a parameter, so the sequence stays open
    // and the accented character is what ends it.
    assert_eq!(sanitize("keep\x1b[1;2?élan"), "keepélan");
    // One byte above the range: DEL ends nothing and is dropped.
    assert_eq!(sanitize("keep\x1b[1;2\x7félan"), "keepélan");
    assert_eq!(sanitize("keep\u{9b}1;2\x7félan"), "keepélan");
}

#[test]
fn suffix_after_a_malformed_sequence_is_preserved() {
    for (label, raw, expected) in SUFFIXES {
        assert_eq!(sanitize(raw), *expected, "{label}");
        assert_eq!(sanitize_approval(raw), *expected, "{label}");
    }
}

#[test]
fn a_bare_esc_consumes_exactly_one_follower() {
    for (label, raw, expected) in BARE_ESC {
        assert_eq!(sanitize(raw), *expected, "{label}");
        assert_eq!(sanitize_approval(raw), *expected, "{label}");
    }
}

#[test]
fn a_format_control_absorbed_by_a_bare_esc_never_reaches_the_output() {
    // ESC takes exactly one follower, so a bidi control in that slot is
    // dropped rather than escaped. It cannot reorder anything because it never
    // reaches the output, though the attempt is not visible as an escape.
    let clean = sanitize("keep\x1b\u{202E}tail");
    assert_eq!(clean, "keeptail");
    assert!(!clean.contains('\u{202E}'));
    let approval = sanitize_approval("keep\x1b\u{00AD}tail");
    assert_eq!(approval, "keeptail");
}

#[test]
fn format_characters_inside_a_stripped_sequence_never_reach_the_output() {
    // A control hidden in a stripped OSC body disappears with the body: it can
    // neither reorder nor hide text in the rendered line.
    let raw = "keep\x1b]0;\u{202E}\u{200B}\x07tail";
    assert_eq!(sanitize(raw), "keeptail");
    assert_eq!(sanitize_approval(raw), "keeptail");
    // Inside a CSI body the same control cannot belong to the sequence, so it
    // ends the sequence without being consumed and is escaped anyway: the
    // attempt stays visible instead of hiding.
    let csi = "keep\x1b[1\u{202E}mtail";
    assert_eq!(sanitize(csi), "keep\\u{202E}mtail");
    assert_no_bidi(&sanitize(csi), "csi body");
    assert_no_bidi(&sanitize_approval(csi), "csi body");
}

#[test]
fn multilingual_text_survives_both_sanitizers() {
    for text in MULTILINGUAL {
        assert_eq!(sanitize(text), *text, "general pass changed {text:?}");
        assert_eq!(
            sanitize_approval(text),
            *text,
            "approval pass changed {text:?}"
        );
    }
}

#[test]
fn multilingual_neighbours_of_escaped_controls_keep_their_own_characters() {
    let bidi = "日本\u{202E}العربية\u{202C}🌍";
    assert_eq!(sanitize(bidi), "日本\\u{202E}العربية\\u{202C}🌍");
    // Zero-width characters are the general pass's business to keep, so the
    // same payload is untouched there and escaped for an approval.
    let invisible = "日本\u{200B}العربية\u{200D}🌍";
    assert_eq!(sanitize(invisible), invisible);
    assert_eq!(
        sanitize_approval(invisible),
        "日本\\u{200B}العربية\\u{200D}🌍"
    );
}

#[test]
fn the_general_pass_keeps_emoji_shaping_that_approval_escapes() {
    // Outside an approval nothing needs to defeat shaping, so a ZWJ sequence
    // renders normally. The strong pass escapes ZWJ too, because an approval
    // never needs shaping and a hidden joiner could split the shown operation.
    let joined = "👨\u{200D}👩\u{200D}👧";
    assert_eq!(sanitize(joined), joined);
    assert_eq!(sanitize_approval(joined), "👨\\u{200D}👩\\u{200D}👧");
    // A variation selector is not invisible formatting, so it stays literal
    // and emoji presentation is not degraded.
    let selected = "🌍\u{FE0F}";
    assert_eq!(sanitize(selected), selected);
    assert_eq!(sanitize_approval(selected), selected);
}

#[test]
fn every_bidi_control_is_escaped_by_both_sanitizers() {
    for control in BIDI_CONTROLS {
        let raw = control.to_string();
        let escape = escape_of(control);
        assert_eq!(
            sanitize(&raw),
            escape,
            "{control:?} must be visibly escaped"
        );
        assert_eq!(
            sanitize_approval(&raw),
            escape,
            "{control:?} must be visibly escaped"
        );
    }
    // A run of controls is escaped one by one, in order, with no collapsing.
    let run: String = BIDI_CONTROLS.iter().collect();
    let escaped: String = BIDI_CONTROLS.iter().map(|c| escape_of(*c)).collect();
    assert_eq!(sanitize(&run), escaped);
    assert_eq!(sanitize_approval(&run), escaped);
}

#[test]
fn every_invisible_format_character_is_escaped_only_by_the_approval_sanitizer() {
    for control in INVISIBLE_FORMAT {
        let raw = control.to_string();
        let escape = escape_of(control);
        assert_eq!(
            sanitize(&raw),
            raw,
            "the general pass keeps {control:?} so shaping still renders"
        );
        assert_eq!(
            sanitize_approval(&raw),
            escape,
            "{control:?} must be visibly escaped for an approval"
        );
        assert!(
            !sanitize_approval(&raw).contains(control),
            "{control:?} must not survive as a raw character"
        );
    }
}

#[test]
fn characters_beside_the_escaped_ranges_are_left_literal() {
    for neighbour in INVISIBLE_NEIGHBOURS {
        let raw = neighbour.to_string();
        assert_eq!(
            sanitize_approval(&raw),
            raw,
            "{neighbour:?} must not be escaped"
        );
        assert_eq!(sanitize(&raw), raw, "{neighbour:?} must not be escaped");
    }
    for neighbour in BIDI_NEIGHBOURS {
        let raw = neighbour.to_string();
        assert_eq!(sanitize(&raw), raw, "{neighbour:?} must not be escaped");
        assert_eq!(
            sanitize_approval(&raw),
            raw,
            "{neighbour:?} must not be escaped"
        );
    }
}

#[test]
fn escapes_are_not_escaped_a_second_time() {
    // The visible form is plain ASCII, so the approval pass must leave the
    // general pass's escapes alone instead of doubling their backslashes.
    let clean = sanitize_approval("a\u{202E}b\u{200B}c");
    assert_eq!(clean, "a\\u{202E}b\\u{200B}c");
    assert_eq!(
        clean.matches("\\u{").count(),
        2,
        "each control escapes once"
    );
    assert_eq!(sanitize_approval(&clean), clean);
}

#[test]
fn approval_keeps_newline_and_tab_but_escapes_unicode_line_separators() {
    assert_eq!(sanitize_approval("a\nb\tc"), "a\nb\tc");
    assert_eq!(
        sanitize_approval("a\u{2028}b\u{2029}c"),
        "a\\u{2028}b\\u{2029}c"
    );
    assert_eq!(sanitize_approval("\u{2028}"), "\\u{2028}");
    assert_eq!(sanitize_approval("\n\u{2028}\n"), "\n\\u{2028}\n");
}

#[test]
fn escape_notation_in_the_input_is_preserved_verbatim() {
    // The escape is a display convention, not a parse boundary: literal
    // `\u{202E}` text in a payload stays literal, so it is indistinguishable on
    // screen from a genuinely escaped control. Only real characters are
    // escaped; the notation itself is ordinary text.
    let literal = r"run \u{202E}gnp.exe";
    assert_eq!(sanitize(literal), literal);
    assert_eq!(sanitize_approval(literal), literal);
    let mixed = r"a\u{200B}b";
    assert_eq!(sanitize_approval(mixed), mixed);
}

#[test]
fn worst_case_four_x_expansion_fits_the_approval_field_bound() {
    // The field bound is 4x the core notice bound precisely because the
    // sanitizer expands one input byte at most 4x. U+061C (bidi, escaped by
    // the general pass) and U+00AD (soft hyphen, escaped by the approval pass)
    // are the worst case: two input bytes, eight escape bytes each.
    assert_eq!(
        MAX_APPROVAL_FIELD_BYTES,
        WORST_CASE_EXPANSION * CORE_NOTICE_BYTES,
        "the approval field bound is sized at the 4x expansion"
    );
    let controls = CORE_NOTICE_BYTES / 2;
    assert!(
        controls > 0,
        "the field bound admits a whole notice of controls"
    );
    let bidi = "\u{061C}".repeat(controls);
    let soft_hyphen = "\u{00AD}".repeat(controls);
    for (label, raw, escape, clean) in [
        ("arabic-letter-mark", &bidi, "\\u{061C}", sanitize(&bidi)),
        (
            "soft-hyphen",
            &soft_hyphen,
            "\\u{00AD}",
            sanitize_approval(&soft_hyphen),
        ),
    ] {
        assert_eq!(
            raw.len(),
            controls * 2,
            "{label}: a full notice of 2-byte controls"
        );
        assert_eq!(
            clean.len(),
            controls * ESCAPE_BYTES,
            "{label}: every control escapes to {ESCAPE_BYTES} bytes"
        );
        assert_eq!(
            clean.len(),
            raw.len() * WORST_CASE_EXPANSION,
            "{label}: the expansion is exactly the 4x worst case"
        );
        assert!(
            clean.len() <= MAX_APPROVAL_FIELD_BYTES,
            "{label}: the 4x expansion fits the field bound uncut"
        );
        assert_eq!(
            clean,
            escape.repeat(controls),
            "{label}: exact escaped form"
        );
        assert_safe_line(&clean, label);
    }
}

#[test]
fn no_format_character_expands_beyond_four_x() {
    for control in BIDI_CONTROLS.iter().chain(INVISIBLE_FORMAT.iter()) {
        let raw = control.to_string();
        for (pass, clean) in [
            ("sanitize", sanitize(&raw)),
            ("sanitize_approval", sanitize_approval(&raw)),
        ] {
            assert!(
                clean.len() <= WORST_CASE_EXPANSION * raw.len(),
                "{pass}: {control:?} expands {} bytes from {}, more than 4x",
                clean.len(),
                raw.len()
            );
            if clean != raw {
                assert!(
                    clean.is_ascii(),
                    "{pass}: an escaped {control:?} must become ASCII text"
                );
            }
        }
    }
    // The factor is tight, not slack: 2-byte controls reach it exactly, and a
    // 4-byte musical-format control still has room to spare.
    assert_eq!(
        sanitize("\u{061C}").len(),
        WORST_CASE_EXPANSION * "\u{061C}".len()
    );
    assert_eq!(
        sanitize_approval("\u{00AD}").len(),
        WORST_CASE_EXPANSION * "\u{00AD}".len()
    );
    assert_eq!(sanitize_approval("\u{1D173}").len(), 9);
}
