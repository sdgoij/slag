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
    fn line_terminators_are_exactly_four() {
        for cp in [0x000A, 0x000D, 0x2028, 0x2029] {
            assert!(is_line_terminator(cp));
        }
        assert!(!is_line_terminator(0x000B));
        assert!(!is_line_terminator(0x0020));
    }
}
