//! Unicode data tables and code-point helpers shared by the lexer, regexp
//! matcher, and string built-ins (spec ch. 11-12, 21-22).
//!
//! Phase 2 lands ID_Start/ID_Continue, Default Case Conversion, normalization
//! data, and code-point properties for `\p{…}`. WhiteSpace and LineTerminator
//! are already here because phase 1 conversions (parseFloat/parseInt) need
//! them.

use unicode_normalization::UnicodeNormalization;

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

/// The four Unicode normalization forms (spec 22.1.3.17).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::upper_case_acronyms)]
pub enum NormalizationForm {
    Nfc,
    Nfd,
    Nfkc,
    Nfkd,
}

/// Normalize a code-point sequence into `form` (Unicode Normalization
/// Forms); lone surrogates pass through unchanged, since they are not valid
/// Unicode scalar values and never participate in normalization.
///
/// The Unicode data comes from `unicode-normalization` (Unicode 16.0); the
/// pinned spec version drift is documented at the crate root.
pub fn normalize_code_points(cps: &[u32], form: NormalizationForm) -> Vec<u32> {
    let mut out = Vec::with_capacity(cps.len());
    let mut run: Vec<char> = Vec::new();
    for &cp in cps {
        match char_of(cp) {
            Some(c) => run.push(c),
            None => {
                flush_run(&mut run, &mut out, form);
                out.push(cp);
            }
        }
    }
    flush_run(&mut run, &mut out, form);
    out
}

fn flush_run(run: &mut Vec<char>, out: &mut Vec<u32>, form: NormalizationForm) {
    if run.is_empty() {
        return;
    }
    let text: String = run.drain(..).collect();
    let normalized: String = match form {
        NormalizationForm::Nfc => text.nfc().collect(),
        NormalizationForm::Nfd => text.nfd().collect(),
        NormalizationForm::Nfkc => text.nfkc().collect(),
        NormalizationForm::Nfkd => text.nfkd().collect(),
    };
    out.extend(normalized.chars().map(|c| c as u32));
}

/// Default Case Conversion (spec 22.1.3.30): the locale-insensitive
/// lowercase mapping of one code point, which may expand to several.
pub fn to_lowercase(cp: u32) -> Vec<u32> {
    match char_of(cp) {
        Some(c) => c.to_lowercase().map(|c| c as u32).collect(),
        None => vec![cp],
    }
}

/// Default Case Conversion (spec 22.1.3.31): the uppercase mapping of one
/// code point, which may expand to several (`ß` → `"SS"`).
pub fn to_uppercase(cp: u32) -> Vec<u32> {
    match char_of(cp) {
        Some(c) => c.to_uppercase().map(|c| c as u32).collect(),
        None => vec![cp],
    }
}

// --- RegExp support (spec 22.2.3) ---

/// Simple/common case folding, the `Canonicalize` mapping under `u`/`v` +
/// `i` (spec 22.2.3.2.2 step 1: the CaseFolding.txt simple mapping).
/// Approximated by `to_lowercase` when it yields a single code point, with
/// the known multi-char-lowercase-but-single-fold divergence fixed.
pub fn simple_case_fold(cp: u32) -> u32 {
    // CaseFolding.txt simple (S) / common (C) mappings that to_lowercase
    // does not produce (multi-char or otherwise divergent forms).
    match cp {
        0x0130 => return 0x0069, // İ folds to i (lowercase is i + dot above)
        0x017F => return 0x0073, // long s (C mapping) → s
        0x0345 => return 0x03B9, // combining ypogegrammeni (C mapping) → iota
        0x03C2 => return 0x03C3, // final sigma (S mapping) → sigma
        0x00B5 => return 0x03BC, // micro sign (C mapping) → mu
        0x1FD3 => return 0x0390, // iota with dialytika and oxia → tonos
        0x1FE3 => return 0x03B0, // upsilon with dialytika and oxia → tonos
        0xFB05 => return 0xFB06, // long s t ligature → st ligature
        _ => {}
    }
    match char_of(cp) {
        Some(c) => {
            let mut it = c.to_lowercase();
            match (it.next(), it.next()) {
                (Some(lower), None) => lower as u32,
                _ => cp,
            }
        }
        None => cp,
    }
}

/// `Canonicalize` for non-unicode `i` mode (spec 22.2.3.2.2 steps 3-9):
/// toUppercase via Default Case Conversion, never mapping a non-ASCII code
/// point into ASCII, and never expanding.
pub fn non_unicode_canonicalize(cp: u32) -> u32 {
    match char_of(cp) {
        Some(c) => {
            let mut it = c.to_uppercase();
            match (it.next(), it.next()) {
                (Some(upper), None) => {
                    let upper = upper as u32;
                    if cp >= 0x80 && upper < 0x80 {
                        cp
                    } else {
                        upper
                    }
                }
                _ => cp,
            }
        }
        None => cp,
    }
}

/// The basic `WordCharacters` set (spec 22.2.3.3): ASCII `[A-Za-z0-9_]`.
/// The `u`+`i` extras (chars whose canonical form lands in this set) are
/// folded in by the caller via `simple_case_fold`.
pub fn is_ascii_word_char(cp: u32) -> bool {
    matches!(cp, 0x30..=0x39 | 0x41..=0x5A | 0x61..=0x7A | 0x5F)
}

/// The two-letter `General_Category` abbreviation of `cp` (for `\p{…}`).
pub fn general_category(cp: u32) -> &'static str {
    use unicode_properties::UnicodeGeneralCategory;
    match char_of(cp).map(|c| c.general_category()) {
        Some(unicode_properties::GeneralCategory::UppercaseLetter) => "Lu",
        Some(unicode_properties::GeneralCategory::LowercaseLetter) => "Ll",
        Some(unicode_properties::GeneralCategory::TitlecaseLetter) => "Lt",
        Some(unicode_properties::GeneralCategory::ModifierLetter) => "Lm",
        Some(unicode_properties::GeneralCategory::OtherLetter) => "Lo",
        Some(unicode_properties::GeneralCategory::NonspacingMark) => "Mn",
        Some(unicode_properties::GeneralCategory::SpacingMark) => "Mc",
        Some(unicode_properties::GeneralCategory::EnclosingMark) => "Me",
        Some(unicode_properties::GeneralCategory::DecimalNumber) => "Nd",
        Some(unicode_properties::GeneralCategory::LetterNumber) => "Nl",
        Some(unicode_properties::GeneralCategory::OtherNumber) => "No",
        Some(unicode_properties::GeneralCategory::ConnectorPunctuation) => "Pc",
        Some(unicode_properties::GeneralCategory::DashPunctuation) => "Pd",
        Some(unicode_properties::GeneralCategory::OpenPunctuation) => "Ps",
        Some(unicode_properties::GeneralCategory::ClosePunctuation) => "Pe",
        Some(unicode_properties::GeneralCategory::InitialPunctuation) => "Pi",
        Some(unicode_properties::GeneralCategory::FinalPunctuation) => "Pf",
        Some(unicode_properties::GeneralCategory::OtherPunctuation) => "Po",
        Some(unicode_properties::GeneralCategory::MathSymbol) => "Sm",
        Some(unicode_properties::GeneralCategory::CurrencySymbol) => "Sc",
        Some(unicode_properties::GeneralCategory::ModifierSymbol) => "Sk",
        Some(unicode_properties::GeneralCategory::OtherSymbol) => "So",
        Some(unicode_properties::GeneralCategory::SpaceSeparator) => "Zs",
        Some(unicode_properties::GeneralCategory::LineSeparator) => "Zl",
        Some(unicode_properties::GeneralCategory::ParagraphSeparator) => "Zp",
        Some(unicode_properties::GeneralCategory::Control) => "Cc",
        Some(unicode_properties::GeneralCategory::Format) => "Cf",
        Some(unicode_properties::GeneralCategory::Surrogate) => "Cs",
        Some(unicode_properties::GeneralCategory::PrivateUse) => "Co",
        _ => "Cn",
    }
}

/// The script full name of `cp`, or `None` outside any script (for
/// `\p{Script=…}`).
pub fn script(cp: u32) -> Option<&'static str> {
    use unicode_script::UnicodeScript;
    let script = char_of(cp)?.script();
    let name = script.full_name();
    if name == "Unknown" || name == "Common" || name == "Inherited" {
        return None;
    }
    Some(name)
}

/// The `Script_Extensions` of `cp` as full names (spec `\p{Script_Extensions=…}`).
pub fn script_extensions(cp: u32) -> Vec<&'static str> {
    use unicode_script::UnicodeScript;
    let Some(c) = char_of(cp) else {
        return Vec::new();
    };
    let ext = c.script_extension();
    ext.iter()
        .map(|s| s.full_name())
        .filter(|n| *n != "Common" && *n != "Inherited" && *n != "Unknown")
        .collect()
}

/// Canonicalize a script name (full or ISO 15924 short form) to the full
/// name used by `script()`/`script_extensions()`; `None` for unknown names.
pub fn canonical_script_name(name: &str) -> Option<&'static str> {
    use unicode_script::Script;
    Script::from_full_name(name)
        .or_else(|| Script::from_short_name(name))
        .map(|s| s.full_name())
}

/// The curated binary-property predicates for `\p{…}` (spec 22.2.3.13
/// Table 65 subset): `None` for unsupported names.
pub fn binary_property(cp: u32, name: &str) -> Option<bool> {
    Some(match name {
        "ASCII" => cp <= 0x7F,
        "ASCII_Hex_Digit" => matches!(cp, 0x30..=0x39 | 0x41..=0x46 | 0x61..=0x66),
        "Alphabetic" => char_of(cp).is_some_and(char::is_alphabetic),
        "Any" => true,
        "Assigned" => general_category(cp) != "Cn",
        "Hex_Digit" => matches!(cp, 0x30..=0x39 | 0x41..=0x46 | 0x61..=0x66),
        "ID_Continue" => char_of(cp).is_some_and(|c| {
            use unicode_id::UnicodeID;
            c.is_id_continue()
        }),
        "ID_Start" => char_of(cp).is_some_and(|c| {
            use unicode_id::UnicodeID;
            c.is_id_start()
        }),
        "Lowercase" => char_of(cp).is_some_and(char::is_lowercase),
        "Uppercase" => char_of(cp).is_some_and(char::is_uppercase),
        "White_Space" => is_white_space(cp) || is_line_terminator(cp),
        // XID_Start/XID_Continue are ID_Start/ID_Continue minus a handful of
        // code points; the ID tables are the practical approximation.
        "XID_Continue" => char_of(cp).is_some_and(|c| {
            use unicode_id::UnicodeID;
            c.is_id_continue()
        }),
        "XID_Start" => char_of(cp).is_some_and(|c| {
            use unicode_id::UnicodeID;
            c.is_id_start()
        }),
        _ => return None,
    })
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

    #[test]
    fn nfd_decomposes_and_nfc_recomposes() {
        let e_accent = vec![0x00E9]; // é
        let nfd = normalize_code_points(&e_accent, NormalizationForm::Nfd);
        assert_eq!(nfd, vec![0x65, 0x0301]); // e + combining acute
        assert_eq!(
            normalize_code_points(&nfd, NormalizationForm::Nfc),
            e_accent
        );
    }

    #[test]
    fn nfkc_compatibility_decomposes() {
        // U+FF21 FULLWIDTH LATIN CAPITAL LETTER A → "A".
        assert_eq!(
            normalize_code_points(&[0xFF21], NormalizationForm::Nfkc),
            vec![0x41]
        );
        // NFKC keeps the ligature intact.
        assert_eq!(
            normalize_code_points(&[0xFB01], NormalizationForm::Nfc),
            vec![0xFB01]
        );
        assert_eq!(
            normalize_code_points(&[0xFB01], NormalizationForm::Nfkc),
            vec![0x66, 0x69]
        );
    }

    #[test]
    fn normalization_passes_lone_surrogates_through() {
        let cps = vec![0x61, 0xD800, 0x62, 0x0301];
        let nfd = normalize_code_points(&cps, NormalizationForm::Nfd);
        assert_eq!(nfd, vec![0x61, 0xD800, 0x62, 0x0301]);
    }

    #[test]
    fn case_conversion_expands_code_points() {
        assert_eq!(to_lowercase(0x41), vec![0x61]);
        assert_eq!(to_uppercase(0x61), vec![0x41]);
        assert_eq!(to_uppercase(0x00DF), vec![0x53, 0x53]); // ß → "SS"
        assert_eq!(to_lowercase(0x0130), vec![0x69, 0x0307]); // İ → "i̇"
        assert_eq!(to_lowercase(0xD800), vec![0xD800]); // lone surrogate
    }
}
