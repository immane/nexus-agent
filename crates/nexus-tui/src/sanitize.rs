//! Untrusted-output sanitization for the presentation boundary.
//!
//! Model and tool text is untrusted: emitting it raw would let a malicious
//! or accidental escape sequence rewrite the screen, spoof the approval
//! card, or hide the true operation under test. Every line stored in
//! presentation state passes through [`sanitize`] first.

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
/// and C0/C1 controls plus DEL, keeping `\n` and `\t`. The result never
/// contains `ESC` (0x1B) or C1 bytes.
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
        if char.is_control() || char == '\x7f' {
            out.extend(replacement(char));
        } else {
            out.push(char);
        }
    }
    out
}

/// Consumes a CSI body up to and including its final byte.
fn consume_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for next in chars.by_ref() {
        if next.is_ascii_alphabetic() || next == '~' {
            break;
        }
    }
}

/// Consumes an OSC body up to BEL or `ESC \`.
fn consume_osc(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    let mut previous_esc = false;
    for next in chars.by_ref() {
        if next == '\x07' {
            break;
        }
        if previous_esc && next == '\\' {
            break;
        }
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
    fn plain_and_multibyte_text_survives() {
        assert_eq!(sanitize("héllo 🌍\nsecond\tline"), "héllo 🌍\nsecond\tline");
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
