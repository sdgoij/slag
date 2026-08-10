//! Unicode data tables and code-point helpers shared by the lexer, regexp
//! matcher, and string built-ins (spec ch. 11-12, 21-22).
//!
//! Phase 2 lands ID_Start/ID_Continue, Default Case Conversion, normalization
//! data, and code-point properties for `\p{…}`. WhiteSpace and LineTerminator
//! are already here because phase 1 conversions (parseFloat/parseInt) need
//! them.

/// `WhiteSpace` (spec 11.2): TAB, VT, FF, SP, NBSP, ZWNBSP, and the
/// Space_Separator (Zs) category.
pub fn is_white_space(cp: u32) -> bool {
    matches!(
        cp,
        0x0009 | 0x000B | 0x000C | 0x0020 | 0x00A0 | 0x1680 | 0x2000
            ..=0x200A | 0x202F | 0x205F | 0x3000 | 0xFEFF
    )
}

/// `LineTerminator` (spec 11.3): LF, CR, LS, PS.
pub fn is_line_terminator(cp: u32) -> bool {
    matches!(cp, 0x000A | 0x000D | 0x2028 | 0x2029)
}

/// `IdentifierStartChar` (spec 12.6.1): Unicode ID_Start, plus `$` and `_`.
pub fn is_identifier_start(cp: u32) -> bool {
    cp == 0x24 || cp == 0x5F || is_unicode_id_start(cp)
}

/// `IdentifierPartChar` (spec 12.6.1): Unicode ID_Continue, plus `$`, `_`,
/// ZWNJ, and ZWJ.
pub fn is_identifier_part(cp: u32) -> bool {
    cp == 0x24 || cp == 0x5F || cp == 0x200C || cp == 0x200D || is_unicode_id_continue(cp)
}

/// The Unicode `ID_Start` property, used for escaped identifiers
/// (spec 12.6.1: an escaped code point must be UnicodeIDStart).
pub fn is_unicode_id_start(cp: u32) -> bool {
    use unicode_id::UnicodeID;
    char_of(cp).is_some_and(char::is_id_start)
}

/// The Unicode `ID_Continue` property, used for escaped identifiers.
pub fn is_unicode_id_continue(cp: u32) -> bool {
    use unicode_id::UnicodeID;
    char_of(cp).is_some_and(char::is_id_continue)
}

fn char_of(cp: u32) -> Option<char> {
    char::from_u32(cp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn white_space_accepts_spec_set() {
        for cp in [
            0x0009, 0x000B, 0x000C, 0x0020, 0x00A0, 0x1680, 0x2000, 0x200A, 0x202F, 0x205F, 0x3000,
            0xFEFF,
        ] {
            assert!(is_white_space(cp), "U+{cp:04X}");
        }
    }

    #[test]
    fn white_space_rejects_non_members() {
        assert!(!is_white_space(0x0041)); // 'A'
        assert!(!is_white_space(0x000A)); // LF is a line terminator, not white space
        assert!(!is_white_space(0x2028));
        assert!(!is_white_space(0x0085)); // NEL is neither
    }

    #[test]
    fn identifier_start_accepts_dollar_underscore_and_unicode() {
        assert!(is_identifier_start(0x24)); // $
        assert!(is_identifier_start(0x5F)); // _
        assert!(is_identifier_start(0x61)); // a
        assert!(is_identifier_start(0x41)); // A
        assert!(is_identifier_start(0x4E00)); // CJK ideograph
        assert!(is_identifier_start(0x0370)); // Greek capital letter
        assert!(!is_identifier_start(0x30)); // digit
        assert!(!is_identifier_start(0x1F600)); // emoji
        assert!(!is_identifier_start(0xD800)); // lone surrogate
    }

    #[test]
    fn identifier_part_accepts_digits_and_joiners() {
        assert!(is_identifier_part(0x30)); // digit
        assert!(is_identifier_part(0x200C)); // ZWNJ
        assert!(is_identifier_part(0x200D)); // ZWJ
        assert!(is_identifier_part(0x24)); // $
        assert!(is_identifier_part(0x4E00));
        assert!(!is_identifier_part(0x20)); // space
        assert!(!is_identifier_part(0x2E)); // .
    }

    #[test]
    fn unicode_id_properties_reject_extras() {
        assert!(!is_unicode_id_start(0x24)); // $ is not in the Unicode property
        assert!(!is_unicode_id_start(0x5F)); // _ is not in the Unicode property
        assert!(is_unicode_id_continue(0x30));
        // ZWNJ/ZWJ are already ID_Continue; the spec lists them explicitly
        // in IdentifierPartChar as a belt-and-suspenders measure.
        assert!(is_unicode_id_continue(0x200C));
        assert!(is_unicode_id_continue(0x200D));
        assert!(!is_unicode_id_continue(0x24)); // $ is not ID_Continue either
    }

    #[test]
    fn line_terminators_are_exactly_four() {
        for cp in [0x000A, 0x000D, 0x2028, 0x2029] {
            assert!(is_line_terminator(cp));
        }
        assert!(!is_line_terminator(0x000B));
        assert!(!is_line_terminator(0x0020));
    }
}
