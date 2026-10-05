#![forbid(unsafe_code)]

//! Percent-encoding coverage through the public machine-encoding API.
//!
//! These checks pin the output contract the stdout record format depends on:
//! unreserved ASCII is an identity, every other byte becomes a complete `%XX`
//! escape with uppercase hex, no raw separator or control byte can appear
//! inside a value, encoding is reversible for any UTF-8 text, and the decoder
//! rejects malformed escapes and non-UTF-8 byte sequences. Every case is a
//! fixed table or exhaustive ASCII/basic-plane iteration, so the suite is
//! deterministic and needs no clock, filesystem, or process.

use nexus_headless::{PercentDecodeError, percent_decode, sanitize};

/// Exact RFC 3986 unreserved set: the only bytes that stay literal.
const UNRESERVED: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

/// Text samples: empty input, whitespace, multi-byte scalars, a literal `%`
/// that must not be mistaken for an escape, and record-shaped payloads.
const TEXT_SAMPLES: [&str; 12] = [
    "",
    "hello",
    "héllo 🌍 世界",
    "line\nbreak\ttab",
    "a=b c",
    "%25 already escaped",
    "%2",
    "rev=m0-test-0 type=result",
    "\u{1b}[31mred\u{1b}[0m",
    "\u{0}\u{7f}",
    "Ω≈ç√∫˜µ≤≥÷",
    "𝄞 astral \u{10ffff}",
];

/// The framing-relevant ASCII bytes with their exact escapes: controls, the
/// ESC byte, space, and every separator that could split a `key=value` field.
const ESCAPED_ASCII: [(char, &str); 30] = [
    ('\u{0}', "%00"),
    ('\u{1b}', "%1B"),
    ('\u{7f}', "%7F"),
    ('\t', "%09"),
    ('\n', "%0A"),
    ('\r', "%0D"),
    (' ', "%20"),
    ('!', "%21"),
    ('"', "%22"),
    ('#', "%23"),
    ('$', "%24"),
    ('%', "%25"),
    ('&', "%26"),
    ('+', "%2B"),
    ('/', "%2F"),
    (':', "%3A"),
    (';', "%3B"),
    ('<', "%3C"),
    ('=', "%3D"),
    ('>', "%3E"),
    ('?', "%3F"),
    ('@', "%40"),
    ('[', "%5B"),
    ('\\', "%5C"),
    (']', "%5D"),
    ('^', "%5E"),
    ('`', "%60"),
    ('{', "%7B"),
    ('|', "%7C"),
    ('}', "%7D"),
];

/// Decodes `encoded`, asserting it is encoder-shaped: ASCII only, no raw
/// separator or control byte, and every `%` a complete uppercase escape.
fn assert_encoded_token(raw: &str, encoded: &str) {
    assert!(encoded.is_ascii(), "encoded value is ASCII: {encoded}");
    assert!(
        !encoded.contains([' ', '=', '\n', '\r', '\t', '\x1b']),
        "no raw separator or control byte: {encoded}"
    );
    assert!(
        encoded.len() >= raw.len() && encoded.len() <= raw.len() * 3,
        "1..=3 characters per input byte: {raw} -> {encoded}"
    );
    let bytes = encoded.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let pair = encoded.get(index + 1..index + 3).expect("complete escape");
            assert_eq!(pair.len(), 2, "two hex digits per escape: {encoded}");
            assert_eq!(
                pair,
                pair.to_ascii_uppercase(),
                "escape uses uppercase hex: {encoded}"
            );
            assert!(
                pair.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "escape uses hex digits: {encoded}"
            );
            index += 3;
        } else {
            assert!(
                UNRESERVED.contains(char::from(bytes[index])),
                "unreserved passthrough only: {encoded}"
            );
            index += 1;
        }
    }
}

/// Marks which byte values appear as `%XX` escapes in an encoded value.
fn escaped_bytes(encoded: &str) -> [bool; 256] {
    let bytes = encoded.as_bytes();
    let mut seen = [false; 256];
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let pair = encoded.get(index + 1..index + 3).expect("complete escape");
            let byte = u8::from_str_radix(pair, 16).expect("escape holds two hex digits");
            seen[usize::from(byte)] = true;
            index += 3;
        } else {
            index += 1;
        }
    }
    seen
}

/// The whole unreserved set is an exact identity, and it is the *only*
/// passthrough set: every other ASCII byte is escaped.
#[test]
fn unreserved_ascii_is_an_exact_identity() {
    assert_eq!(sanitize(UNRESERVED), UNRESERVED);
    assert_eq!(sanitize("run-1_item.key~0"), "run-1_item.key~0");
    assert_eq!(sanitize("host_read"), "host_read");
    assert_eq!(sanitize("m0-test-0"), "m0-test-0");
    assert_eq!(sanitize("~-_."), "~-_.");

    let mut expected_escapes = 0usize;
    for byte in 0u8..=0x7F {
        let raw = char::from(byte).to_string();
        let encoded = sanitize(&raw);
        assert_eq!(
            escaped_bytes(&encoded)[usize::from(byte)],
            !UNRESERVED.contains(raw.as_str()),
            "byte {byte:#04X} is escaped exactly when it is reserved"
        );
        if encoded == raw {
            assert_eq!(encoded, raw, "identity for byte {byte:#04X}");
        } else {
            assert_eq!(encoded, format!("%{byte:02X}"), "escape for {byte:#04X}");
            expected_escapes += 1;
        }
    }
    assert_eq!(
        expected_escapes, 62,
        "66 unreserved ASCII bytes stay literal, the other 62 are escaped"
    );
}

/// Controls, separators, and `%` become complete `%XX` escapes with
/// uppercase hex digits.
#[test]
fn controls_separators_and_percent_are_escaped_with_uppercase_hex() {
    let mut concatenated = String::new();
    for (raw, escape) in ESCAPED_ASCII {
        let text = raw.to_string();
        let encoded = sanitize(&text);
        assert_eq!(encoded, escape, "escape for {raw:?}");
        assert_encoded_token(&text, &encoded);
        concatenated.push_str(escape);
    }
    let joined = ESCAPED_ASCII
        .iter()
        .map(|(raw, _)| *raw)
        .collect::<String>();
    assert_eq!(
        sanitize(&joined),
        concatenated,
        "encoding is a per-byte function and order preserving"
    );

    let dirty = "a\x1bb\nc\rd=e f\"g%h#,";
    let clean = sanitize(dirty);
    assert_eq!(clean, "a%1Bb%0Ac%0Dd%3De%20f%22g%25h%23%2C");
    assert_encoded_token(dirty, &clean);
    assert!(!clean.contains('\x1b'));
    for separator in ['\n', '\r', ' ', '=', '"', '#', ','] {
        assert!(!clean.contains(separator), "raw {separator:?} survives");
    }
    // A raw `%` only ever introduces one of the escapes just asserted.
    assert_eq!(
        clean.matches('%').count(),
        dirty
            .bytes()
            .filter(|byte| !UNRESERVED.contains(char::from(*byte)))
            .count(),
        "one escape per reserved byte, and no extra percent sign"
    );
}

/// Every byte value that can occur in UTF-8 text is escaped somewhere across
/// the generated encodings, and nothing outside the UTF-8 range can appear.
#[test]
fn every_encodable_byte_value_is_escaped_somewhere() {
    let mut seen = escaped_bytes(&sanitize(&(0u8..=0x7F).map(char::from).collect::<String>()));
    // Every valid two-byte sequence contributes its lead and continuation.
    for lead in 0xC2u8..=0xDF {
        for low in 0x80u8..=0xBF {
            let raw = [lead, low];
            let text = std::str::from_utf8(&raw).expect("valid two-byte sequence");
            let escapes = escaped_bytes(&sanitize(text));
            assert!(escapes[usize::from(lead)] && escapes[usize::from(low)]);
            for (index, escaped) in escapes.into_iter().enumerate() {
                seen[index] |= escaped;
            }
        }
    }
    // Every three- and four-byte lead, with each possible continuation.
    for lead in 0xE0u8..=0xEF {
        for low in 0x80u8..=0xBF {
            let raw = [lead, low, 0x80];
            let Ok(text) = std::str::from_utf8(&raw) else {
                continue;
            };
            for (index, escaped) in escaped_bytes(&sanitize(text)).into_iter().enumerate() {
                seen[index] |= escaped;
            }
        }
    }
    for lead in 0xF0u8..=0xF4 {
        for low in 0x80u8..=0xBF {
            let raw = [lead, low, 0x80, 0x80];
            let Ok(text) = std::str::from_utf8(&raw) else {
                continue;
            };
            for (index, escaped) in escaped_bytes(&sanitize(text)).into_iter().enumerate() {
                seen[index] |= escaped;
            }
        }
    }

    for byte in 0u8..=0x7F {
        assert_eq!(
            seen[usize::from(byte)],
            !UNRESERVED.contains(char::from(byte)),
            "byte {byte:#04X} escape coverage"
        );
    }
    for byte in 0x80u8..=0xF4 {
        if !matches!(byte, 0xC0 | 0xC1) {
            // C0/C1 are overlong leads and can never appear in valid UTF-8.
            assert!(seen[usize::from(byte)], "byte {byte:#04X} is escaped");
        }
    }
    for byte in 0xF5u8..=0xFF {
        assert!(
            !seen[usize::from(byte)],
            "byte {byte:#04X} cannot occur in UTF-8 text"
        );
    }
    assert_eq!(
        seen.iter().filter(|escaped| **escaped).count(),
        177,
        "66 unreserved ASCII bytes stay literal, every other encodable byte is escaped"
    );
}

/// Every single-byte value round-trips through the public string API: ASCII
/// bytes decode back to themselves, and anything at or above 0x80 is rejected
/// as non-UTF-8 when it stands alone.
#[test]
fn roundtrips_every_byte_value_through_the_public_api() {
    for byte in 0u8..=u8::MAX {
        let encoded = format!("%{byte:02X}");
        let decoded = percent_decode(&encoded);
        if byte.is_ascii() {
            assert_eq!(
                decoded.expect("single ASCII byte decodes"),
                char::from(byte).to_string(),
                "byte {byte:#04X}"
            );
            let raw = char::from(byte).to_string();
            let expected = if UNRESERVED.contains(raw.as_str()) {
                raw.clone()
            } else {
                format!("%{byte:02X}")
            };
            assert_eq!(
                sanitize(&raw),
                expected,
                "byte {byte:#04X} encodes as a literal or one escape"
            );
        } else {
            assert_eq!(
                decoded,
                Err(PercentDecodeError::InvalidUtf8),
                "lone byte {byte:#04X} is not UTF-8"
            );
        }
    }
    // The whole ASCII range plus multi-byte scalars in one value.
    let text = format!("{}é🌍𝄞", (0u8..=0x7F).map(char::from).collect::<String>());
    let encoded = sanitize(&text);
    assert_encoded_token(&text, &encoded);
    assert_eq!(percent_decode(&encoded).expect("roundtrip"), text);
}

/// Every valid two-byte UTF-8 sequence survives encode/decode byte-exactly.
#[test]
fn roundtrips_every_two_byte_utf8_sequence() {
    for lead in 0xC2u8..=0xDF {
        for low in 0x80u8..=0xBF {
            let bytes = [lead, low];
            let text = std::str::from_utf8(&bytes).expect("valid two-byte sequence");
            let encoded = sanitize(text);
            assert_eq!(encoded.len(), 6, "three characters per byte: {encoded}");
            assert_eq!(
                percent_decode(&encoded).expect("roundtrip"),
                text,
                "sequence {lead:#04X} {low:#04X}"
            );
        }
    }
}

/// The whole basic plane plus selected astral scalars round-trip, one scalar
/// at a time and as one concatenated value.
#[test]
fn roundtrips_every_basic_plane_scalar_and_selected_astral_scalars() {
    let mut scalars = 0usize;
    for scalar in 0u32..=0xFFFF {
        // Surrogate code points are not scalars and have no `char` form.
        let Some(raw) = char::from_u32(scalar).map(|value| value.to_string()) else {
            assert!(
                (0xD800..=0xDFFF).contains(&scalar),
                "no scalar at U+{scalar:04X}"
            );
            continue;
        };
        scalars += 1;
        let encoded = sanitize(&raw);
        assert_encoded_token(&raw, &encoded);
        assert_eq!(
            percent_decode(&encoded).expect("roundtrip"),
            raw,
            "scalar U+{scalar:04X}"
        );
    }
    assert_eq!(
        scalars, 63_488,
        "the whole basic plane minus 2048 surrogates"
    );
    for scalar in [0x1_0000, 0x1_F1E6, 0x1_F600, 0x1_FFF0, 0x10_0000, 0x10_FFFF] {
        let raw = char::from_u32(scalar).expect("astral scalar").to_string();
        let encoded = sanitize(&raw);
        assert!(
            encoded.is_ascii(),
            "astral text encodes to ASCII: {encoded}"
        );
        assert_eq!(encoded.len(), raw.len() * 3, "three characters per byte");
        assert_eq!(percent_decode(&encoded).expect("roundtrip"), raw);
    }
    let plane = (0u32..=0xFFFF)
        .filter_map(char::from_u32)
        .collect::<String>();
    assert_eq!(
        percent_decode(&sanitize(&plane)).expect("roundtrip"),
        plane,
        "a full-plane value round-trips as one token"
    );
}

/// Representative text samples keep their identity and stay single-token.
#[test]
fn roundtrips_text_samples_as_single_encoded_tokens() {
    for text in TEXT_SAMPLES {
        let encoded = sanitize(text);
        assert_encoded_token(text, &encoded);
        assert_eq!(
            percent_decode(&encoded).expect("roundtrip"),
            text,
            "sample round-trips"
        );
        assert_eq!(encoded.split(' ').count(), 1, "one token: {encoded}");
    }
    assert_eq!(sanitize(""), "");
    assert_eq!(percent_decode("").expect("empty decodes"), "");
}

/// A literal `%` is escaped, so encoded text never decodes twice and the
/// encoder is not idempotent on already-encoded input.
#[test]
fn percent_is_escaped_so_encoded_text_never_decodes_twice() {
    assert_eq!(sanitize("%"), "%25");
    assert_eq!(sanitize("%25"), "%2525");
    assert_eq!(
        percent_decode(&sanitize("%2525")).expect("roundtrip"),
        "%2525"
    );
    assert_eq!(
        percent_decode("%252F").expect("single decode"),
        "%2F",
        "an escaped separator does not collapse into a separator"
    );
    assert_eq!(sanitize("%2F"), "%252F");
    assert!(
        sanitize("%2F")
            .bytes()
            .all(|byte| UNRESERVED.contains(char::from(byte)) || byte == b'%')
    );
}

/// The decoder accepts either hex case and passes plain ASCII through.
#[test]
fn decoder_accepts_lowercase_mixed_hex_and_plain_ascii() {
    assert_eq!(percent_decode("%c3%a9").expect("lowercase"), "é");
    assert_eq!(percent_decode("%C3%A9").expect("uppercase"), "é");
    assert_eq!(percent_decode("%C3%a9").expect("mixed case"), "é");
    assert_eq!(percent_decode("%e2%82%ac").expect("euro sign"), "€");
    assert_eq!(percent_decode("%f0%9d%84%9e").expect("clef"), "𝄞");
    assert_eq!(
        percent_decode("%41%42%43").expect("adjacent escapes"),
        "ABC"
    );
    assert_eq!(
        percent_decode("plain_value~1").expect("plain"),
        "plain_value~1"
    );
    assert_eq!(percent_decode("a=b c").expect("raw separators"), "a=b c");
    assert_eq!(percent_decode("%0d%0a").expect("escaped newline"), "\r\n");
    assert_eq!(percent_decode("%00").expect("escaped NUL"), "\u{0}");
    for text in TEXT_SAMPLES {
        assert_eq!(
            percent_decode(&sanitize(text)).expect("roundtrip"),
            text,
            "encoder output is always decodable: {text:?}"
        );
    }
}

/// A `%` not followed by exactly two hexadecimal digits is rejected.
#[test]
fn decoder_rejects_malformed_escapes() {
    for encoded in [
        "%",      // no digits at all
        "%2",     // one digit only
        "%GG",    // neither digit is hex
        "%G2",    // high nibble not hex
        "%2G",    // low nibble not hex
        "%+0",    // sign is not a hex digit
        "%-1",    // neither digit is hex
        "% 0",    // space is not a hex digit
        "%0 ",    // high digit is hex, low is not
        "%41%",   // trailing lone percent
        "%41%4",  // escape truncated at end of value
        "%zz",    // lowercase non-hex
        "%4g%41", // malformed escape before a valid one
        "%%41",   // first percent has no digits
        "abc%",   // trailing percent after plain bytes
    ] {
        assert_eq!(
            percent_decode(encoded),
            Err(PercentDecodeError::MalformedEscape),
            "rejects {encoded:?}"
        );
    }
    // An escape is exactly three characters: a fourth character is a literal
    // byte and the escape always wins over plain bytes.
    assert_eq!(percent_decode("%61bc").expect("trailing byte kept"), "abc");
    assert_eq!(percent_decode("a%42c").expect("escape wins"), "aBc");
    assert_eq!(
        percent_decode("%2541").expect("escaped percent then digit"),
        "%41"
    );
}

/// A well-formed escape that decodes to non-UTF-8 bytes is rejected:
/// invalid or truncated leads, bad continuations, overlong forms, surrogates,
/// and out-of-range scalars.
#[test]
fn decoder_rejects_invalid_utf8_byte_sequences() {
    for encoded in [
        "%FF", // invalid lead byte
        "%80", // lone continuation
        "%FE",
        "%C3",             // truncated two-byte sequence
        "%C3%28",          // bad continuation
        "%C0%AF",          // overlong two-byte encoding
        "%E0%80%80",       // overlong three-byte encoding
        "%E0%9F%BF",       // overlong three-byte encoding
        "%ED%A0%80",       // UTF-16 surrogate half
        "%F0%82%82%AC",    // overlong four-byte encoding
        "%F4%90%80%80",    // above U+10FFFF
        "%F5%80%80%80",    // invalid four-byte lead
        "%F8%88%80%80%80", // five-byte sequence
        "%41%FF",          // a valid prefix does not rescue invalid bytes
        "%C3%A9%FF",
        "%E2%82",
    ] {
        assert_eq!(
            percent_decode(encoded),
            Err(PercentDecodeError::InvalidUtf8),
            "rejects {encoded:?}"
        );
    }
    for byte in 0u8..=0x7F {
        assert!(
            percent_decode(&format!("%{byte:02X}")).is_ok(),
            "ASCII byte {byte:#04X} is valid UTF-8"
        );
    }
}

/// Decode failures are stable, static diagnostics with no source chaining,
/// so callers can map them without inspecting hidden state.
#[test]
fn decode_errors_expose_stable_static_diagnostics() {
    assert_eq!(
        PercentDecodeError::MalformedEscape.to_string(),
        "malformed percent escape"
    );
    assert_eq!(
        PercentDecodeError::InvalidUtf8.to_string(),
        "decoded bytes are not valid UTF-8"
    );
    assert_eq!(
        PercentDecodeError::MalformedEscape,
        PercentDecodeError::MalformedEscape
    );
    for error in [
        PercentDecodeError::MalformedEscape,
        PercentDecodeError::InvalidUtf8,
    ] {
        let as_error: &dyn std::error::Error = &error;
        assert!(as_error.source().is_none(), "no source chain");
    }
}
