//! `RegExp.escape` (spec 22.2.4.3 + EncodeForRegExpEscape): escape a string
//! for safe inclusion in a pattern.

/// The SyntaxCharacter set (spec 22.2.1).
fn is_syntax_character(cp: u32) -> bool {
    matches!(
        cp,
        0x5E | 0x24
            | 0x5C
            | 0x2E
            | 0x2A
            | 0x2B
            | 0x3F
            | 0x28
            | 0x29
            | 0x5B
            | 0x5D
            | 0x7B
            | 0x7D
            | 0x7C
    )
}

/// The Code Point → ControlEscape table (spec Table 67).
fn control_escape(cp: u32) -> Option<&'static str> {
    Some(match cp {
        0x09 => "t",
        0x0A => "n",
        0x0B => "v",
        0x0C => "f",
        0x0D => "r",
        _ => return None,
    })
}

/// The "other punctuators" escaped as `\xNN`/`\uNNNN` (spec 22.2.4.3.1).
fn is_other_punctuator(cp: u32) -> bool {
    matches!(
        cp,
        0x2C | 0x2D
            | 0x3D
            | 0x3C
            | 0x3E
            | 0x23
            | 0x26
            | 0x21
            | 0x25
            | 0x3A
            | 0x3B
            | 0x40
            | 0x7E
            | 0x27
            | 0x60
            | 0x22
    )
}

/// UnicodeEscape (spec 22.2.4.3.1 step 6): `\xHH` for ≤ 0xFF, else `\uHHHH`.
fn unicode_escape(unit: u16) -> String {
    if unit <= 0xFF {
        format!("\\x{:02x}", unit)
    } else {
        format!("\\u{:04x}", unit)
    }
}

/// spec 22.2.4.3 RegExp.escape over code points (UTF-16 input).
pub fn escape(input: &[u16]) -> String {
    let mut out = String::new();
    let mut first = true;
    let mut i = 0;
    while i < input.len() {
        let (cp, _, count) = crate::crux_code_point_at(input, i);
        i += count;
        if first && matches!(cp, 0x30..=0x39 | 0x41..=0x5A | 0x61..=0x7A) {
            out.push_str(&format!("\\x{:02x}", cp));
        } else {
            out.push_str(&encode_for_regexp_escape(cp));
        }
        first = false;
    }
    out
}

/// EncodeForRegExpEscape (spec 22.2.4.3.1).
fn encode_for_regexp_escape(cp: u32) -> String {
    if is_syntax_character(cp) || cp == b'/' as u32 {
        let mut s = String::from("\\");
        s.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
        return s;
    }
    if let Some(escape) = control_escape(cp) {
        return format!("\\{escape}");
    }
    let needs_hex = is_other_punctuator(cp)
        || unicode::is_white_space(cp)
        || unicode::is_line_terminator(cp)
        || (0xD800..=0xDFFF).contains(&cp);
    if needs_hex {
        if cp <= 0xFF {
            return format!("\\x{:02x}", cp);
        }
        // Surrogate pair or lone surrogate: escape each code unit.
        let units: Vec<u16> = if cp <= 0xFFFF {
            vec![cp as u16]
        } else {
            let x = cp - 0x10000;
            vec![0xD800 + (x >> 10) as u16, 0xDC00 + (x & 0x3FF) as u16]
        };
        return units.iter().map(|&u| unicode_escape(u)).collect();
    }
    char::from_u32(cp)
        .map(|c| c.to_string())
        .unwrap_or_else(|| "\u{FFFD}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_basics() {
        assert_eq!(
            escape("a".encode_utf16().collect::<Vec<u16>>().as_slice()),
            "\\x61"
        );
        assert_eq!(
            escape("1".encode_utf16().collect::<Vec<u16>>().as_slice()),
            "\\x31"
        );
        assert_eq!(
            escape(".".encode_utf16().collect::<Vec<u16>>().as_slice()),
            "\\."
        );
        assert_eq!(
            escape("/".encode_utf16().collect::<Vec<u16>>().as_slice()),
            "\\/"
        );
        assert_eq!(
            escape(" ".encode_utf16().collect::<Vec<u16>>().as_slice()),
            "\\x20"
        );
        assert_eq!(
            escape("\n".encode_utf16().collect::<Vec<u16>>().as_slice()),
            "\\n"
        );
        assert_eq!(
            escape("a".encode_utf16().collect::<Vec<u16>>().as_slice()).len(),
            4
        );
    }

    #[test]
    fn escape_surrogates() {
        // A valid astral code point is not a surrogate: passed through.
        let out = escape(&[0xD83D, 0xDE00]);
        assert_eq!(out, "\u{1F600}");
        let lone = escape(&[0xD800]);
        assert_eq!(lone, "\\ud800");
    }
}
