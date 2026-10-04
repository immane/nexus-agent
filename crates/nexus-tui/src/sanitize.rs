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
