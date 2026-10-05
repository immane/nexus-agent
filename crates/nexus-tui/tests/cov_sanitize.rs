//! Coverage hardening for the presentation-boundary sanitizers in
//! [`nexus_tui::sanitize`].
//!
//! Everything here goes through the public API only (`sanitize` and
//! `sanitize_approval`), so the suite pins the contract the TUI actually
//! relies on when it renders untrusted model/tool text:
//!
//! - CSI, OSC, and single/two-byte escape sequences are removed completely;
//! - C0 controls, C1 controls, and DEL are dropped, while `\n` and `\t`
//!   survive;
//! - bidi overrides/isolates are escaped into a *visible* form instead of
//!   being silently dropped;
//! - ZWJ (and the rest of ordinary emoji shaping) survives the general pass
//!   but is escaped by the approval pass;
//! - no output ever contains ESC (0x1B) or a C1 byte;
//! - combined hostile payloads are neutralized and every payload is
//!   idempotent under re-sanitization.
//!
//! All payloads are inline literals, so the suite is fully deterministic:
//! no clock, no filesystem, no terminal, no network.

#![forbid(unsafe_code)]

use nexus_tui::sanitize::{sanitize, sanitize_approval};

/// Hostile inputs reused by the invariant, idempotence, and relation tests.
///
/// The cases cover every removal path plus the near-miss shapes that a real
/// payload generator emits: alternate charset selection, DEC special
/// graphics, a DCS body, an unterminated OSC, a C1-introduced sequence, and
/// the invisible-formatting block.
const HOSTILE_CORPUS: &[&str] = &[
    "\x1b[2J\x1b[H\x1b[?25l",
    "\x1b[1;2H\x1b[2K\x1b[1G\x1b[38;5;196;1m",
    "\x1b]0;spoofed window title\x07after",
    "\x1b]8;;http://evil.example\x1b\\click\x1b]8;;\x1b\\",
    "\x1b]52;c;cHduCg==\x07",
    "\x1b]0;unterminated",
    "\x1b_Gf=100,a=T;AAAA\x1b\\",
    "\x1bPq#0;2;0;0;0\x1b\\data",
    "\x1b(B\x1b#8\x1bM\x1bc",
    "\x1b[\x1b[\x1b[m",
    "\x1b[31",
    "\x1b\x1b\x1b[0m",
    "\u{9b}31m\u{9d}0;title\u{9c}",
    "\u{202E}gnp.exe\u{202C}",
    "\u{2066}\u{2067}\u{2069}\u{206F}",
    "\u{200B}\u{200C}\u{200D}\u{2060}\u{FEFF}\u{00AD}",
    "\u{1D173}\u{1D17A}\u{2028}\u{2029}",
    "\u{1F469}\u{200D}\u{1F4BB}",
    "\x07\x00\x7f\x1b\u{9c}\u{80}\u{9f}",
    "\r\rcarriage\r\n",
    "\x1b[2Jrm -rf /\x07 approved",
];

/// Asserts the invariant that makes the sanitizers a security boundary: the
/// output carries no ESC byte, no C1 byte, and no control character other
/// than newline and tab.
fn assert_no_terminal_control(clean: &str, context: &str) {
    assert!(
        !clean.contains('\x1b'),
        "{context}: ESC survived in {clean:?}"
    );
    for ch in clean.chars() {
        assert!(
            !('\u{80}'..='\u{9f}').contains(&ch),
            "{context}: C1 {ch:?} survived in {clean:?}"
        );
        assert!(
            !(ch.is_control() && ch != '\n' && ch != '\t'),
            "{context}: control {ch:?} survived in {clean:?}"
        );
    }
}

#[test]
fn csi_sequences_are_removed_completely() {
    let cases: &[(&str, &str)] = &[
        ("\x1b[2J\x1b[H", ""),
        ("\x1b[31mred\x1b[0m", "red"),
        ("\x1b[1;2H", ""),
        ("\x1b[?25l", ""),
        ("\x1b[38;5;196;1m", ""),
        ("\x1b[1 q", ""),
        ("\x1b[3@", ""),
        ("\x1b[6n", ""),
        ("a\x1b[31mb", "ab"),
        ("a\x1b[31;1m b", "a b"),
        ("\u{9b}31mred", "red"),
        ("\u{9b}2J", ""),
    ];
    for (dirty, expected) in cases {
        assert_eq!(sanitize(dirty), *expected, "dirty={dirty:?}");
        assert_no_terminal_control(&sanitize(dirty), "csi");
    }
}

#[test]
fn csi_is_consumed_for_every_final_byte() {
    // Regression net for finals below 'A' (0x40 upwards): each must end the
    // sequence instead of being mistaken for a parameter.
    for final_byte in '\x40'..='\x7e' {
        let esc_form = format!("head\x1b[1;2{final_byte}tail");
        assert_eq!(
            sanitize(&esc_form),
            "headtail",
            "ESC [ final {final_byte:?}"
        );
        let c1_form = format!("head\u{9b}1;2{final_byte}tail");
        assert_eq!(
            sanitize(&c1_form),
            "headtail",
            "C1 CSI final {final_byte:?}"
        );
    }
}

#[test]
fn truncated_and_malformed_csi_keeps_following_text() {
    // Truncated at end of input: the remainder is dropped, nothing panics.
    assert_eq!(sanitize("text\x1b"), "text");
    assert_eq!(sanitize("text\x1b["), "text");
    assert_eq!(sanitize("text\x1b[31"), "text");
    assert_eq!(sanitize("text\x1b[3;"), "text");
    assert_eq!(sanitize("text\u{9b}31"), "text");
    // A byte that cannot belong to the sequence ends it unconsumed.
    assert_eq!(sanitize("a\x1b[31\nb"), "a\nb");
    assert_eq!(sanitize("a\x1b[31\tb"), "a\tb");
    assert_eq!(sanitize("a\x1b[31éb"), "aéb");
    assert_eq!(sanitize("a\x1b[31\u{1F600}b"), "a\u{1F600}b");
    // A nested introducer is re-entered as its own sequence.
    assert_eq!(sanitize("a\x1b[\x1b[31mb"), "ab");
    assert_eq!(sanitize("a\x1b[1\x1b[0mb"), "ab");
}

#[test]
fn osc_sequences_are_removed_and_text_survives() {
    assert_eq!(sanitize("\x1b]0;spoofed title\x07real"), "real");
    assert_eq!(sanitize("\x1b]2;window\x1b\\real"), "real");
    assert_eq!(
        sanitize("\x1b]8;;http://evil.example\x1b\\click\x1b]8;;\x1b\\ok"),
        "clickok"
    );
    assert_eq!(sanitize("\x1b]52;c;cHduCg==\x07ok"), "ok");
    assert_eq!(sanitize("\u{9d}0;title\u{9c}ok"), "ok");
    assert_eq!(sanitize("a\x1b]0;title\nb"), "a\nb");
    assert_eq!(sanitize("a\x1b]0;title\tb"), "a\tb");
    assert_eq!(sanitize("text\x1b]0;title"), "text");
    assert_eq!(sanitize("text\x1b]"), "text");
    assert_eq!(sanitize("text\u{9d}0;title"), "text");
}

#[test]
fn osc_body_terminators_are_exact() {
    // Only ESC \ terminates, so a lone backslash stays ordinary body content.
    assert_eq!(sanitize("\x1b]0;back\\slash\x07ok"), "ok");
    // ESC ESC \ still terminates the string.
    assert_eq!(sanitize("\x1b]0;t\x1b\x1b\\ok"), "ok");
    assert_eq!(sanitize("\x1b]0;t\x1b"), "");
    for dirty in [
        "\x1b]0;back\\slash\x07ok",
        "\x1b]0;t\x1b\x1b\\ok",
        "\x1b]0;t\x1b",
        "\u{9d}0;t\u{9c}ok",
    ] {
        assert_no_terminal_control(&sanitize(dirty), "osc terminators");
    }
}

#[test]
fn two_byte_and_charset_escapes_are_removed() {
    // Bare ESC plus exactly one follower covers complete two-byte operations
    // such as ESC M (reverse index) or RIS (ESC c).
    for introducer in ['M', 'D', 'E', 'H', 'c', '7', '8', '=', '>'] {
        let dirty = format!("a\x1b{introducer}b");
        assert_eq!(sanitize(&dirty), "ab", "ESC {introducer:?}");
    }
    // Charset and DEC special introducers take one extra follower.
    for introducer in ['(', ')', '#'] {
        let dirty = format!("a\x1b{introducer}Bb");
        assert_eq!(sanitize(&dirty), "ab", "ESC {introducer:?} B");
    }
    assert_eq!(sanitize("a\x1b(Bb"), "ab");
    assert_eq!(sanitize("a\x1b)0b"), "ab");
    assert_eq!(sanitize("a\x1b#8b"), "ab");
    assert_eq!(sanitize("a\x1bcb"), "ab");
    assert_eq!(sanitize("a\x1bMb"), "ab");
}

#[test]
fn c0_controls_are_dropped_except_newline_and_tab() {
    for code in 0x00_u8..0x20 {
        let control = char::from(code);
        if control == '\n' || control == '\t' {
            assert_eq!(sanitize(&format!("a{control}b")), format!("a{control}b"));
            continue;
        }
        if control == '\x1b' {
            // ESC opens a sequence, so its one follower goes with it.
            assert_eq!(sanitize("a\x1bb"), "a");
            continue;
        }
        assert_eq!(sanitize(&format!("a{control}b")), "ab", "C0 {code:#04x}");
    }
    assert_eq!(
        sanitize("line one\nline two\tcol"),
        "line one\nline two\tcol"
    );
    // CR is dropped, so it cannot overwrite an already printed line.
    assert_eq!(sanitize("cr\r\nlf"), "cr\nlf");
    assert_eq!(sanitize("bel\x07"), "bel");
}

#[test]
fn del_and_c1_controls_are_dropped() {
    assert_eq!(sanitize("a\x7fb"), "ab");
    assert_eq!(sanitize("a\x7f\x7fb"), "ab");
    assert_eq!(sanitize("\u{9c}"), "");
    assert_eq!(sanitize("a\u{80}b"), "ab");
    for code in 0x80_u8..0xa0 {
        let control = char::from(code);
        let clean = sanitize(&format!("a{control}b\x1b[0mc"));
        assert_no_terminal_control(&clean, &format!("C1 {code:#04x}"));
        if code == 0x9b {
            // CSI (U+009B) opens a sequence and takes the next ASCII byte as
            // its final byte, exactly like ESC [ does.
            assert_eq!(clean, "ac", "C1 {code:#04x} opens a CSI");
        } else if code == 0x9d {
            // OSC (U+009D) opens a string whose body also admits an ESC, so
            // the trailing CSI is swallowed as inert text.
            assert_eq!(clean, "a", "C1 {code:#04x} opens an OSC");
        } else {
            assert_eq!(clean, "abc", "C1 {code:#04x}");
        }
    }
}

#[test]
fn a_c1_sequence_swallows_only_its_own_bytes() {
    // Same shape as ESC [ : the next ASCII letter is the CSI final byte.
    assert_eq!(sanitize("a\u{9b}Jb"), "ab");
    assert_eq!(sanitize("a\u{9b}"), "a");
    // An unterminated OSC runs to the end of the input rather than emitting
    // anything control-like.
    assert_eq!(sanitize("a\u{9d}titleb"), "a");
}

#[test]
fn multibyte_characters_never_split_at_sequence_boundaries() {
    assert_eq!(sanitize("x\x1b[é"), "xé");
    assert_eq!(sanitize("x\x1b[\u{1F30D}y"), "x\u{1F30D}y");
    assert_eq!(sanitize("x\u{9b}\u{1F30D}y"), "x\u{1F30D}y");
    assert_eq!(sanitize("\x1b]0;t\u{1F30D}\x07y"), "y");
    assert_eq!(sanitize("\x1b]0;t\u{1F30D}y"), "");
}

#[test]
fn repeated_and_adjacent_escape_sequences_are_all_removed() {
    assert_eq!(sanitize("\x1b[0m\x1b[0m\x1b[0m"), "");
    assert_eq!(sanitize("\x1b\x1b\x1b[0m"), "");
    assert_eq!(sanitize("\x1b[\x1b[\x1b[m"), "");
    assert_eq!(sanitize("\x1b]\x1b]0;t\x07\x1b[0m"), "");
}

#[test]
fn bidi_controls_are_escaped_visibly() {
    let bidi: Vec<char> = ['\u{061C}', '\u{200E}', '\u{200F}']
        .into_iter()
        .chain('\u{202A}'..='\u{202E}')
        .chain('\u{2066}'..='\u{2069}')
        .chain('\u{206A}'..='\u{206F}')
        .collect();
    assert_eq!(bidi.len(), 18, "every documented bidi control");
    for control in bidi {
        let clean = sanitize(&control.to_string());
        assert_eq!(
            clean,
            format!("\\u{{{:04X}}}", control as u32),
            "{control:?} must be visibly escaped"
        );
        assert!(!clean.contains(control), "{control:?} survives raw");
    }
}

#[test]
fn bidi_overrides_and_isolates_cannot_reorder_a_command() {
    let clean = sanitize("run \u{2066}rm -rf\u{2069} \u{202E}exe.sh\u{202C}");
    assert_eq!(
        clean,
        "run \\u{2066}rm -rf\\u{2069} \\u{202E}exe.sh\\u{202C}"
    );
    for control in ['\u{202E}', '\u{202C}', '\u{2066}', '\u{2069}'] {
        assert!(!clean.contains(control), "{control:?} survives raw");
    }
    // The visible escape form is stable under a second pass.
    assert_eq!(sanitize(&clean), clean);
    assert_no_terminal_control(&clean, "bidi escapes");
}

#[test]
fn near_miss_invisible_characters_survive_the_general_pass() {
    // Zero-width and separator characters are not terminal controls, so the
    // general sanitizer must leave them alone; only the approval pass
    // escapes them.
    for kept in [
        '\u{00AD}',
        '\u{200B}',
        '\u{200C}',
        '\u{200D}',
        '\u{2060}',
        '\u{FEFF}',
        '\u{2028}',
        '\u{2029}',
        '\u{1D173}',
    ] {
        let raw = format!("a{kept}b");
        assert_eq!(sanitize(&raw), raw, "{kept:?} must survive sanitize");
        assert!(
            sanitize_approval(&raw).contains(&format!("\\u{{{:04X}}}", kept as u32)),
            "{kept:?} must be escaped for approval"
        );
    }
}

#[test]
fn literal_escape_lookalikes_are_not_rewritten() {
    // Text that already looks like an escape stays as typed: an attacker
    // cannot spoof a sanitized escape by typing it literally.
    assert_eq!(sanitize("\\u{202E}"), "\\u{202E}");
    assert_eq!(sanitize_approval("\\u{200B}"), "\\u{200B}");
    assert_eq!(sanitize_approval("a\\u{0000}b"), "a\\u{0000}b");
    assert_eq!(sanitize("ESC[2J"), "ESC[2J");
}

#[test]
fn zwj_and_emoji_sequences_are_preserved_by_the_general_pass() {
    let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
    assert_eq!(sanitize(family), family);
    assert_eq!(sanitize("\u{1F1EF}\u{1F1F5}"), "\u{1F1EF}\u{1F1F5}");
    assert_eq!(sanitize("\u{1F468}\u{1F3FD}"), "\u{1F468}\u{1F3FD}");
    assert_eq!(sanitize("\u{1F44D}\u{FE0F}"), "\u{1F44D}\u{FE0F}");
    assert_eq!(sanitize("e\u{0301}"), "e\u{0301}"); // combining acute
    // ZWNJ shaping stays too.
    assert_eq!(sanitize("پ\u{200C}ا"), "پ\u{200C}ا");
    // The approval pass escapes ZWJ because an approval needs no shaping.
    let approved = sanitize_approval(family);
    assert_eq!(approved, "\u{1F468}\\u{200D}\u{1F469}\\u{200D}\u{1F467}");
    assert_eq!(sanitize_approval(&approved), approved);
}

#[test]
fn approval_pass_escapes_every_documented_invisible_character() {
    let codes = [0x00AD_u32, 0x2028, 0x2029, 0x2060, 0xFEFF]
        .into_iter()
        .chain(0x200B..=0x200D)
        .chain(0x1D173..=0x1D17A);
    for code in codes {
        let control = char::from_u32(code).expect("scalar value");
        let raw = format!("a{control}b");
        let approval = sanitize_approval(&raw);
        assert_eq!(
            approval,
            format!("a\\u{{{:04X}}}b", code),
            "{control:?} must be visibly escaped"
        );
        assert_eq!(sanitize(&raw), raw, "{control:?} must survive sanitize");
        assert_no_terminal_control(&approval, "approval escapes");
    }
}

#[test]
fn output_never_contains_esc_across_the_hostile_corpus() {
    for &dirty in HOSTILE_CORPUS {
        for clean in [sanitize(dirty), sanitize_approval(dirty)] {
            assert!(!clean.contains('\x1b'), "{dirty:?} leaked ESC as {clean:?}");
            assert_no_terminal_control(&clean, dirty);
        }
    }
}

#[test]
fn sanitizers_are_idempotent_on_their_own_output() {
    for &dirty in HOSTILE_CORPUS {
        let general = sanitize(dirty);
        assert_eq!(sanitize(&general), general, "sanitize twice: {dirty:?}");
        let approval = sanitize_approval(dirty);
        assert_eq!(
            sanitize_approval(&approval),
            approval,
            "sanitize_approval twice: {dirty:?}"
        );
    }
}

#[test]
fn approval_pass_never_shrinks_the_general_result() {
    for &dirty in HOSTILE_CORPUS {
        let general = sanitize(dirty);
        let approval = sanitize_approval(dirty);
        assert!(
            approval.chars().count() >= general.chars().count(),
            "{dirty:?}: approval {approval:?} is shorter than general {general:?}"
        );
    }
}

#[test]
fn approval_pass_equals_general_pass_without_invisible_formatting() {
    // With no zero-width/separator characters to escape, the stronger pass is
    // exactly the standard one.
    for dirty in [
        "\x1b[2J\x1b[31mok\x1b[0m",
        "\x1b]0;t\x07ok",
        "héllo 🌍 日本語\n\tsecond",
        "\x07\x00\x7f\u{9c}\u{202E}x\u{202C}",
        "",
    ] {
        assert_eq!(sanitize_approval(dirty), sanitize(dirty), "{dirty:?}");
    }
}

#[test]
fn plain_and_multilingual_text_is_unchanged() {
    for text in [
        "",
        "ok",
        "cargo test",
        "a b\tc\nd",
        "path/to/file.rs:12:5",
        "--flag=value",
        "héllo 🌍 日本語 العربية עברית हिन्दी Ελληνικά",
    ] {
        assert_eq!(sanitize(text), text, "{text:?}");
        assert_eq!(sanitize_approval(text), text, "{text:?}");
    }
    assert_eq!(sanitize("العربية\u{200F}"), "العربية\\u{200F}");
}

#[test]
fn hostile_combined_payload_is_neutralized() {
    let payload = concat!(
        "\x1b[2J\x1b[H",
        "\x1b]0;ALLOWED\x07",
        "\x1b]52;c;aGVsbG8=\x07",
        "\x1b[?1049h",
        "\u{202E}sh.exe\u{202C}",
        "\x1b(B\x1b#8",
        "\x07\x00\x7f\u{9c}",
        "APPROVED\n",
    );
    let clean = sanitize(payload);
    assert_eq!(clean, "\\u{202E}sh.exe\\u{202C}APPROVED\n");
    assert_no_terminal_control(&clean, "combined payload");
}

#[test]
fn carriage_return_and_bidi_spoofs_are_neutralized() {
    // CR overwrite of a printed approval line.
    assert_eq!(
        sanitize("approved\rshred -rf /\nFAILED"),
        "approvedshred -rf /\nFAILED"
    );
    // Trojan Source: the command name is reversed by an escaped override.
    assert_eq!(
        sanitize("\u{202E}sh.exe\u{202C} --allow"),
        "\\u{202E}sh.exe\\u{202C} --allow"
    );
    // Hyperlink smuggling into an approval summary.
    assert_eq!(
        sanitize_approval("see \x1b]8;;http://evil.example\x1b\\here\x1b]8;;\x1b\\ ok"),
        "see here ok"
    );
    // Approval pass: bidi override and zero-width space both escaped.
    assert_eq!(
        sanitize_approval("\u{202E}ls\u{200B} -la\u{FEFF}"),
        "\\u{202E}ls\\u{200B} -la\\u{FEFF}"
    );
}

#[test]
fn truncated_payloads_do_not_erase_the_following_line() {
    assert_eq!(sanitize("run \x1b]0;title\ntool ok"), "run \ntool ok");
    assert_eq!(sanitize("run \x1b[2\ntool ok"), "run \ntool ok");
    assert_eq!(sanitize("run \x1b]8;;\ntool ok"), "run \ntool ok");
}

#[test]
fn large_hostile_input_is_handled_deterministically() {
    let dirty = "\x1b[2J\u{202E}x\u{202C}\x07\n".repeat(4096);
    let clean = sanitize(&dirty);
    assert_eq!(clean.matches('\n').count(), 4096);
    assert!(clean.contains('x'));
    assert_no_terminal_control(&clean, "large input");
    assert_eq!(sanitize(&clean), clean);
    let approval = sanitize_approval(&dirty);
    assert_no_terminal_control(&approval, "large input");
    assert_eq!(sanitize_approval(&approval), approval);
}
