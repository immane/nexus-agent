//! Untrusted-output sanitization for the presentation boundary.
//!
//! Model and tool text is untrusted: emitting it raw would let a malicious
//! or accidental escape sequence rewrite the screen, spoof the approval
//! card, or hide the true operation under test. Every line stored in
//! presentation state passes through [`sanitize`] first.
//!
//! Sanitization removes terminal control sequences (CSI and OSC), C0/C1
//! controls, and DEL while keeping `\n` and `\t`. Bidirectional formatting
//! controls (Trojan Source overrides and isolates) are escaped into a
//! visible `\u{XXXX}` form instead of being silently dropped, so a reorder
//! or removal attempt is noticeable. Ordinary multilingual text survives
//! unchanged.
//!
//! [`sanitize_approval`] adds a stronger pass for approval summaries: after
//! the standard sanitize it also escapes invisible formatting and
//! separator characters (zero-width spaces/joiners, word joiner, BOM, line
//! and paragraph separators, soft hyphen, invisible musical formatting)
//! that could hide or split the exact operation shown for confirmation.
//! ZWJ is escaped only here; the general sanitizer keeps it so emoji and
//! shaping sequences render normally outside approvals.

/// True for Unicode bidirectional formatting controls that can reorder or
/// hide displayed text: directional marks, embeddings/overrides, isolates,
/// and the deprecated bidi formatting block.
#[must_use]
pub(crate) fn is_bidi_format(control: char) -> bool {
    matches!(
        control,
        '\u{061C}'                  // Arabic letter mark
            | '\u{200E}' | '\u{200F}' // LRM / RLM
            | '\u{202A}'..='\u{202E}' // LRE/RLE/PDF/LRO/RLO
            | '\u{2066}'..='\u{2069}' // LRI/RLI/FSI/PDI
            | '\u{206A}'..='\u{206F}' // deprecated bidi format controls
    )
}

/// True for invisible formatting or separator characters that are not
/// terminal controls but can hide or split text in an approval summary:
/// soft hyphen, zero-width spaces/joiners, line and paragraph separators,
/// the word joiner, the zero-width no-break space, and the invisible
/// musical formatting block.
#[must_use]
fn is_invisible_format(control: char) -> bool {
    matches!(
        control,
        '\u{00AD}'                  // soft hyphen
            | '\u{200B}'..='\u{200D}' // ZWSP / ZWNJ / ZWJ
            | '\u{2028}' | '\u{2029}' // line / paragraph separator
            | '\u{2060}'              // word joiner
            | '\u{FEFF}'              // zero width no-break space
            | '\u{1D173}'..='\u{1D17A}' // invisible musical formatting
    )
}

/// Replaces one stripped region: keeps newlines and tabs, drops everything
/// else that could move the cursor or be interpreted by the terminal.
fn replacement(control: char) -> Option<char> {
    match control {
        '\n' | '\t' => Some(control),
        _ => None,
    }
}

/// Strips terminal control sequences (CSI `ESC [ ...` or C1 `U+009B`,
/// OSC `ESC ] ...` or C1 `U+009D`, other `ESC` introductions plus one
/// follower so two-byte sequences such as `ESC M` or `ESC c` cannot act)
/// and C0/C1 controls plus DEL, keeping `\n` and `\t`. Bidi formatting
/// controls become a visible `\u{XXXX}` escape. The result never contains
/// `ESC` (0x1B) or C1 bytes.
#[must_use]
pub fn sanitize(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(char) = chars.next() {
        if char == '\x1b' {
            // Consume a full escape sequence, not just the introducer.
            match chars.peek() {
                Some(&'[') => {
                    chars.next();
                    consume_csi(&mut chars);
                }
                Some(&']') => {
                    chars.next();
                    consume_osc(&mut chars);
                }
                Some(&'(') | Some(&')') | Some(&'#') => {
                    chars.next();
                    chars.next();
                }
                _ => {
                    // Bare ESC plus one follower: covers two-byte
                    // sequences, which are complete operations.
                    chars.next();
                }
            }
            continue;
        }
        if char == '\u{9b}' {
            consume_csi(&mut chars);
            continue;
        }
        if char == '\u{9d}' {
            consume_osc(&mut chars);
            continue;
        }
        if char.is_control() {
            out.extend(replacement(char));
        } else if is_bidi_format(char) {
            // Visible escape, never silently dropped: an attempted reorder
            // must be noticeable in the rendered line.
            out.push_str(&format!("\\u{{{:04X}}}", char as u32));
        } else {
            out.push(char);
        }
    }
    out
}

/// Stronger sanitization for approval summaries: [`sanitize`] first, then
/// every remaining invisible formatting/separator character is escaped to a
/// visible `\u{XXXX}` form so it cannot hide or split the exact operation
/// shown for confirmation. Normal multilingual text is preserved. Unlike
/// [`sanitize`], this escapes the zero-width joiner too: approval text
/// never needs emoji shaping, while ordinary output keeps ZWJ sequences.
#[must_use]
pub fn sanitize_approval(raw: &str) -> String {
    let clean = sanitize(raw);
    let mut out = String::with_capacity(clean.len());
    for char in clean.chars() {
        if is_invisible_format(char) {
            out.push_str(&format!("\\u{{{:04X}}}", char as u32));
        } else {
            out.push(char);
        }
    }
    out
}

/// Consumes a CSI body up to and including its final byte (0x40..=0x7E).
/// Parameter bytes (0x30..=0x3F) and intermediate bytes (0x20..=0x2F) are
/// consumed inside the sequence; anything else ends it without being
/// consumed, so text after a malformed or truncated sequence survives.
fn consume_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(&next) = chars.peek() {
        if matches!(next, '\x20'..='\x3f') {
            chars.next();
            continue;
        }
        if matches!(next, '\x40'..='\x7e') {
            chars.next();
        }
        break;
    }
}

/// Consumes an OSC body up to BEL, C1 ST (`U+009C`), or `ESC \`. A control
/// that cannot be part of an OSC ends the malformed string without being
/// consumed, so newlines and following text survive an unterminated
/// sequence.
fn consume_osc(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    let mut previous_esc = false;
    while let Some(&next) = chars.peek() {
        if next == '\x07' || next == '\u{9c}' || (previous_esc && next == '\\') {
            chars.next();
            break;
        }
        if next.is_control() && next != '\x1b' {
            break;
        }
        chars.next();
        previous_esc = next == '\x1b';
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csi_osc_and_single_escapes_are_removed() {
        let dirty = "run \x1b[2J\x1b[31mtool\x1b]0;title\x07 ok\x1bM end";
        assert_eq!(sanitize(dirty), "run tool ok end");
    }

    #[test]
    fn csi_ends_at_every_final_byte_from_0x40_to_0x7e() {
        // Regression: finals below 'A' (for example '@') were treated as
        // parameters, so the sequence swallowed the following text.
        for final_byte in '\x40'..='\x7e' {
            let dirty = format!("head\x1b[1;2{final_byte}tail");
            assert_eq!(sanitize(&dirty), "headtail", "ESC [ final {final_byte:?}");
            let c1 = format!("head\u{9b}1;2{final_byte}tail");
            assert_eq!(sanitize(&c1), "headtail", "C1 CSI final {final_byte:?}");
        }
    }

    #[test]
    fn csi_parameters_intermediates_and_finals_are_consumed_together() {
        assert_eq!(sanitize("a\x1b[?25lb"), "ab");
        assert_eq!(sanitize("a\x1b[1 qb"), "ab");
        assert_eq!(sanitize("a\x1b[38;5;196;1mb"), "ab");
    }

    #[test]
    fn osc_terminates_on_bel_c1_st_and_esc_backslash() {
        assert_eq!(sanitize("a\x1b]0;title\x07b"), "ab");
        assert_eq!(sanitize("a\x1b]0;title\u{9c}b"), "ab");
        assert_eq!(sanitize("a\x1b]0;title\x1b\\b"), "ab");
        assert_eq!(sanitize("a\u{9d}0;title\u{9c}b"), "ab");
        assert_eq!(
            sanitize("a\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\b"),
            "alinkb"
        );
    }

    #[test]
    fn c0_controls_are_dropped_except_newline_and_tab() {
        // A bare ESC consumes one follower (two-byte sequences are whole
        // operations such as ESC M or a full reset ESC c).
        assert_eq!(sanitize("a\x00b\x07c\nd\te\x1bf"), "abc\nd\te");
        assert_eq!(sanitize("a\x1bXb"), "ab");
    }

    #[test]
    fn del_and_c1_controls_are_dropped() {
        // C1 CSI (U+009B) opens a sequence exactly like ESC [ does.
        assert_eq!(sanitize("a\x7fb\u{9b}31mc"), "abc");
        assert_eq!(sanitize("a\u{9d}title\x07b"), "ab");
    }

    #[test]
    fn truncated_sequences_at_the_end_are_dropped_without_panicking() {
        assert_eq!(sanitize("text\x1b"), "text");
        assert_eq!(sanitize("text\x1b["), "text");
        assert_eq!(sanitize("text\x1b[31"), "text");
        assert_eq!(sanitize("text\x1b]0;title"), "text");
        assert_eq!(sanitize("text\u{9b}31"), "text");
        assert_eq!(sanitize("text\u{9d}0;title"), "text");
    }

    #[test]
    fn suffix_text_after_malformed_sequences_is_preserved() {
        // A byte that cannot belong to the sequence ends it without being
        // consumed; newline, tab, and multilingual text survive.
        assert_eq!(sanitize("a\x1b[31\nb"), "a\nb");
        assert_eq!(sanitize("a\x1b[31\tb"), "a\tb");
        assert_eq!(sanitize("a\x1b[31éb"), "aéb");
        assert_eq!(sanitize("a\x1b]0;title\nb"), "a\nb");
        assert_eq!(sanitize("a\x1b]0;title\néb"), "a\néb");
    }

    #[test]
    fn bidi_overrides_and_isolates_escape_visibly() {
        assert_eq!(
            sanitize("safe\u{202E}gnp.exe\u{202C} tail"),
            "safe\\u{202E}gnp.exe\\u{202C} tail"
        );
        assert_eq!(sanitize("\u{2066}left\u{2069}"), "\\u{2066}left\\u{2069}");
        assert_eq!(
            sanitize("\u{200F}\u{061C}\u{200E}"),
            "\\u{200F}\\u{061C}\\u{200E}"
        );
        assert_eq!(
            sanitize("\u{202A}\u{202B}\u{202D}"),
            "\\u{202A}\\u{202B}\\u{202D}"
        );
        let clean = sanitize("safe\u{202E}gnp.exe\u{202C}");
        assert!(!clean.chars().any(is_bidi_format), "raw controls are gone");
        assert!(clean.contains("202E"), "escape is visible");
    }

    #[test]
    fn plain_and_multilingual_text_survives() {
        let text = "héllo 🌍 日本語 العربية עברית हिन्दी\nsecond\tline";
        assert_eq!(sanitize(text), text);
    }

    #[test]
    fn approval_sanitizer_escapes_invisible_formatting_and_separators() {
        let dirty = "pay\u{200B}load\u{200C}\u{200D}\u{2060}\u{FEFF}\u{2028}\u{2029}\u{00AD}\u{1D173}\u{1D17A}";
        let clean = sanitize_approval(dirty);
        assert!(
            clean.contains("pay\\u{200B}load"),
            "visible letters are preserved: {clean}"
        );
        for control in [
            '\u{200B}',
            '\u{200C}',
            '\u{200D}',
            '\u{2060}',
            '\u{FEFF}',
            '\u{2028}',
            '\u{2029}',
            '\u{00AD}',
            '\u{1D173}',
            '\u{1D17A}',
        ] {
            assert!(
                !clean.contains(control),
                "{control:?} must not survive approval sanitization"
            );
            assert!(
                clean.contains(&format!("\\u{{{:04X}}}", control as u32)),
                "{control:?} must be visibly escaped"
            );
        }
    }

    #[test]
    fn approval_sanitizer_runs_standard_sanitize_first() {
        assert_eq!(
            sanitize_approval("ok\x1b[2J\u{200B}split"),
            "ok\\u{200B}split"
        );
        let clean = sanitize_approval("x\x1b]0;t\x1b\\y\u{200D}");
        assert!(!clean.contains('\x1b'));
        assert!(clean.contains("\\u{200D}"));
    }

    #[test]
    fn approval_sanitizer_preserves_multilingual_text() {
        let text = "héllo 日本語 العربية עברית हिन्दी 🌍";
        assert_eq!(sanitize_approval(text), text);
    }

    #[test]
    fn general_sanitizer_keeps_zwj_for_emoji_and_shaping() {
        let joined = "👨\u{200D}👩\u{200D}👧";
        assert_eq!(sanitize(joined), joined);
        assert_ne!(sanitize_approval(joined), joined);
    }

    #[test]
    fn hostile_combined_payload_is_neutralized() {
        let dirty = "run \x1b[2J\x1b]0;t\x1b\\\u{202E}exe.txt\u{1b}M\x00\x07 ok";
        let clean = sanitize(dirty);
        assert_eq!(clean, "run \\u{202E}exe.txt ok");
        assert!(!clean.contains('\x1b'));
        assert!(
            !clean
                .chars()
                .any(|char| char.is_control() && char != '\n' && char != '\t')
        );
        assert!(!clean.chars().any(is_bidi_format));
    }

    #[test]
    fn output_never_contains_esc() {
        let dirty = "\u{1b}[1;33m\u{1b}]8;;http://x\u{1b}\\x\u{9b}Y\x00\x07";
        let clean = sanitize(dirty);
        assert!(!clean.contains('\x1b'));
        assert!(
            !clean
                .chars()
                .any(|char| char.is_control() && char != '\n' && char != '\t')
        );
    }
}

/// Coverage hardening for the crate-private sanitizer internals: the two
/// character classifiers, the newline/tab replacement policy, and the escape
/// sequence consumers. `mod tests` above covers behavior through the public
/// entry points and `tests/cov_sanitize.rs` re-pins the API contract from
/// outside the crate; this module isolates the pieces so a boundary change
/// in either consumer is attributed precisely.
#[cfg(test)]
mod cov_sanitize_private {
    use super::*;

    /// Every character `is_bidi_format` must accept, mirrored from the
    /// documented ranges.
    const BIDI_CONTROLS: &[char] = &[
        '\u{061C}', '\u{200E}', '\u{200F}', '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}',
        '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}', '\u{206A}', '\u{206B}',
        '\u{206C}', '\u{206D}', '\u{206E}', '\u{206F}',
    ];

    /// Characters sitting next to the bidi ranges that must NOT be treated
    /// as bidi controls: ZWJ/ZWNJ (invisible formatting, not bidi), printable
    /// neighbors, and the invisible-format set handled by the approval pass.
    const BIDI_NEIGHBOURS: &[char] = &[
        '\u{00AD}', '\u{200C}', '\u{200D}', '\u{2010}', '\u{2028}', '\u{2029}', '\u{202F}',
        '\u{2060}', '\u{2061}', '\u{2070}', '\u{FEFF}',
    ];

    /// Every character `is_invisible_format` must accept, mirrored from the
    /// documented set and ranges.
    const INVISIBLE_CONTROLS: &[char] = &[
        '\u{00AD}',
        '\u{200B}',
        '\u{200C}',
        '\u{200D}',
        '\u{2028}',
        '\u{2029}',
        '\u{2060}',
        '\u{FEFF}',
        '\u{1D173}',
        '\u{1D17A}',
    ];

    #[test]
    fn bidi_classifier_covers_every_documented_range() {
        assert_eq!(BIDI_CONTROLS.len(), 18, "documented bidi set size");
        for control in BIDI_CONTROLS {
            assert!(
                is_bidi_format(*control),
                "{control:?} must be a bidi control"
            );
        }
        // Both ends of every range, so a shrinking range cannot pass.
        assert!(is_bidi_format('\u{061C}'));
        assert!(is_bidi_format('\u{200E}'));
        assert!(is_bidi_format('\u{200F}'));
        assert!(is_bidi_format('\u{202A}'));
        assert!(is_bidi_format('\u{202E}'));
        assert!(is_bidi_format('\u{2066}'));
        assert!(is_bidi_format('\u{2069}'));
        assert!(is_bidi_format('\u{206A}'));
        assert!(is_bidi_format('\u{206F}'));
        for neighbour in BIDI_NEIGHBOURS {
            assert!(
                !is_bidi_format(*neighbour),
                "{neighbour:?} must not be a bidi control"
            );
        }
    }

    #[test]
    fn invisible_format_classifier_covers_exactly_the_documented_set() {
        for control in INVISIBLE_CONTROLS {
            assert!(
                is_invisible_format(*control),
                "{control:?} must be escaped for approval"
            );
        }
        for code in 0x200B_u32..=0x200D {
            assert!(
                is_invisible_format(char::from_u32(code).expect("scalar value")),
                "U+{code:04X} inside the zero-width range"
            );
        }
        for code in 0x1D173_u32..=0x1D17A {
            assert!(
                is_invisible_format(char::from_u32(code).expect("scalar value")),
                "U+{code:04X} inside the invisible musical range"
            );
        }
        for neighbour in [
            '\u{200A}', // hair space, just below the zero-width range
            '\u{200E}',
            '\u{200F}', // bidi marks: escaped by the other pass
            '\u{202A}',
            '\u{202F}',  // bidi LRE / narrow no-break space
            '\u{2061}',  // function application
            '\u{2065}',  // just before the isolate range
            '\u{FEFC}',  // variation selector-16
            '\u{1D172}', // just before the musical range
        ] {
            assert!(
                !is_invisible_format(neighbour),
                "{neighbour:?} must survive the approval pass"
            );
        }
    }

    #[test]
    fn the_two_classifiers_do_not_overlap() {
        // `sanitize_approval` runs after `sanitize`, so a character in both
        // sets would be escaped twice.
        for control in BIDI_CONTROLS {
            assert!(
                !is_invisible_format(*control),
                "{control:?} would be double escaped"
            );
        }
        for control in INVISIBLE_CONTROLS {
            assert!(
                !is_bidi_format(*control),
                "{control:?} would be double escaped"
            );
        }
    }

    #[test]
    fn replacement_keeps_only_newline_and_tab() {
        assert_eq!(replacement('\n'), Some('\n'));
        assert_eq!(replacement('\t'), Some('\t'));
        for code in 0x00_u8..0xa0 {
            let control = char::from(code);
            let expected = if control == '\n' || control == '\t' {
                Some(control)
            } else {
                None
            };
            assert_eq!(replacement(control), expected, "U+{code:04X}");
        }
        // Non-controls are never routed through the replacement policy; the
        // `sanitize` loop handles them as text or as bidi controls.
        assert_eq!(replacement('a'), None);
        assert_eq!(replacement('é'), None);
        assert_eq!(replacement('\u{1F30D}'), None);
        assert_eq!(replacement('\u{202E}'), None);
    }

    #[test]
    fn consume_csi_stops_at_the_final_byte_and_keeps_the_suffix() {
        let cases: &[(&str, &str)] = &[
            ("31mrest", "rest"),
            ("?25lrest", "rest"),
            ("1 qrest", "rest"),    // intermediate byte before the final byte
            ("31\nrest", "\nrest"), // a control ends the sequence unconsumed
            ("31\trest", "\trest"),
            ("é", "é"), // cannot belong to a CSI
            ("31\u{1F30D}rest", "\u{1F30D}rest"),
            ("31\u{9c}rest", "\u{9c}rest"), // C1 ST is not a final byte
            ("\x1b[31mrest", "\x1b[31mrest"), // nested introducer ends it
            ("31", ""),                     // truncated: parameters are consumed
            ("", ""),
        ];
        for (body, suffix) in cases {
            let mut chars = body.chars().peekable();
            consume_csi(&mut chars);
            assert_eq!(
                chars.collect::<String>(),
                *suffix,
                "CSI body {body:?} left the wrong suffix"
            );
        }
    }

    #[test]
    fn consume_osc_stops_at_each_terminator_and_keeps_newlines() {
        let cases: &[(&str, &str)] = &[
            ("0;t\x07rest", "rest"),         // BEL
            ("0;t\u{9c}rest", "rest"),       // C1 ST
            ("0;t\x1b\\rest", "rest"),       // ESC backslash
            ("0;t\x1b\x1b\\rest", "rest"),   // ESC ESC backslash
            ("back\\slash\x07rest", "rest"), // a lone backslash is body content
            ("0;t\nrest", "\nrest"),
            ("0;t\trest", "\trest"),
            ("é\x07rest", "rest"),
            ("0;t\x1b", ""), // unterminated, ESC is still part of the body
            ("0;t", ""),
            ("", ""),
        ];
        for (body, suffix) in cases {
            let mut chars = body.chars().peekable();
            consume_osc(&mut chars);
            assert_eq!(
                chars.collect::<String>(),
                *suffix,
                "OSC body {body:?} left the wrong suffix"
            );
        }
    }

    #[test]
    fn bare_escape_consumes_exactly_one_follower() {
        // A bare ESC swallows one follower so two-byte operations (ESC M,
        // RIS as ESC c) are complete; the next byte is ordinary text. An ESC
        // that is itself the follower therefore leaves the rest as inert text
        // rather than a live sequence introducer.
        for (dirty, expected) in [
            ("a\x1bMc", "ac"),
            ("a\x1bcb", "ab"),
            ("a\x1b", "a"),
            ("\x1b\x1b[0m", "[0m"),
            ("\x1b\x1b", ""),
        ] {
            assert_eq!(sanitize(dirty), expected, "{dirty:?}");
        }
    }

    #[test]
    fn both_passes_use_the_same_visible_escape_form() {
        // One escape syntax for every escaped control, whichever pass
        // produced it: four uppercase hex digits, zero padded.
        assert_eq!(sanitize("\u{202E}"), "\\u{202E}");
        assert_eq!(sanitize("\u{061C}"), "\\u{061C}");
        assert_eq!(sanitize_approval("\u{202E}"), sanitize("\u{202E}"));
        assert_eq!(sanitize_approval("\u{200B}"), "\\u{200B}");
        assert_eq!(sanitize_approval("\u{1D173}"), "\\u{1D173}");
    }

    #[test]
    fn sanitizers_are_deterministic_across_repeated_calls() {
        let dirty = "run \x1b[2J\x1b]0;t\x07\u{202E}exe\u{202C}\u{200B}\x00 ok";
        let general = sanitize(dirty);
        let approval = sanitize_approval(dirty);
        for _ in 0..8 {
            assert_eq!(sanitize(dirty), general);
            assert_eq!(sanitize_approval(dirty), approval);
        }
        // Re-sanitizing already clean output is a no-op.
        assert_eq!(sanitize(&general), general);
        assert_eq!(sanitize_approval(&approval), approval);
    }
}
