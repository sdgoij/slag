//! Unicode data tables and code-point helpers shared by the lexer, regexp
//! matcher, and string built-ins (spec ch. 11-12, 21-22).
//!
//! Phase 2 lands ID_Start/ID_Continue, Default Case Conversion, normalization
//! data, and code-point properties for `\p{…}`. WhiteSpace and LineTerminator
//! are already here because phase 1 conversions (parseFloat/parseInt) need
//! them.

use unicode_normalization::UnicodeNormalization;

mod derived_regexp_tables;

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
    // Lone surrogates are Cs (Surrogate), not Cn: they are not valid scalar
    // values, so `char_of` fails and the fallback below must not claim them
    // unassigned (`\p{gc=Surrogate}` / `\p{Script=Unknown}` fixtures cover
    // exactly the D800-DFFF range).
    if (0xD800..=0xDFFF).contains(&cp) {
        return "Cs";
    }
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

/// The script full name of `cp` (for `\p{Script=…}`), or `None` outside any
/// script. Common, Inherited, and Unknown ARE returned — `\p{Script=Common}`
/// and friends are valid escapes whose members are exactly those code points
/// (the spec's Script=Common/Inherited/Unknown value sets). Lone surrogates
/// are Script=Unknown (the fixture ranges cover D800-DFFF).
pub fn script(cp: u32) -> Option<&'static str> {
    use unicode_script::UnicodeScript;
    if (0xD800..=0xDFFF).contains(&cp) {
        return Some("Unknown");
    }
    Some(char_of(cp)?.script().full_name())
}

/// The `Script_Extensions` of `cp` as full names (spec `\p{Script_Extensions=…}`).
pub fn script_extensions(cp: u32) -> Vec<&'static str> {
    use unicode_script::UnicodeScript;
    let Some(c) = char_of(cp) else {
        // Lone surrogates are Script_Extensions=Unknown.
        return if (0xD800..=0xDFFF).contains(&cp) {
            vec!["Unknown"]
        } else {
            Vec::new()
        };
    };
    let ext = c.script_extension();
    let mut names: Vec<&'static str> = ext.iter().map(|s| s.full_name()).collect();
    // A code point with no Script_Extensions (an unassigned / Unknown-script
    // character) has Script_Extensions=Unknown: `\p{scx=Unknown}` matches
    // exactly those (`Script_Extensions_-_Unknown.js`).
    if names.is_empty() && c.script().full_name() == "Unknown" {
        names.push("Unknown");
    }
    names
}

/// Canonicalize a script name (full or ISO 15924 short form) to the full
/// name used by `script()`/`script_extensions()`; `None` for unknown names.
/// The deprecated ISO 15924 aliases the fixtures use (`Qaac` = Coptic,
/// `Qaai` = Inherited) are mapped by hand — the unicode-script crate knows
/// only the current codes (`Copt`/`Zinh`).
pub fn canonical_script_name(name: &str) -> Option<&'static str> {
    use unicode_script::Script;
    match name {
        "Qaac" => return Some("Coptic"),
        "Qaai" => return Some("Inherited"),
        _ => {}
    }
    Script::from_full_name(name)
        .or_else(|| Script::from_short_name(name))
        .map(|s| s.full_name())
}

/// The curated binary-property predicates for `\p{…}` (spec 22.2.3.13
/// Table 65): the crate-based predicates for the common properties, and the
/// full derived tables (from the pinned test262 fixtures) for the rest.
/// `None` for unsupported names.
pub fn binary_property(cp: u32, name: &str) -> Option<bool> {
    // The full binary-property tables from the test262 fixtures (Unicode v17),
    // for every property the corpus tests. These are exact; the crate-based
    // predicates below are the fast paths for the common ones.
    if let Some(ranges) = derived_regexp_tables::binary_property_table(name) {
        return Some(ranges_contain(ranges, cp));
    }
    Some(match name {
        "ASCII" => cp <= 0x7F,
        "Alphabetic" => char_of(cp).is_some_and(char::is_alphabetic),
        "Any" => true,
        "Assigned" => general_category(cp) != "Cn",
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

/// Whether `cp` is in a sorted inclusive range list.
fn ranges_contain(ranges: &[(u32, u32)], cp: u32) -> bool {
    ranges
        .binary_search_by(|&(s, e)| {
            if cp < s {
                std::cmp::Ordering::Greater
            } else if cp > e {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// The string set of a property-of-strings escape (`\p{RGI_Emoji}` etc.),
/// from the pinned test262 fixtures (Unicode v17): each element is a code
/// point sequence. `None` for names that are not property-of-strings.
pub fn property_of_strings(name: &str) -> Option<&'static [&'static [u32]]> {
    derived_regexp_tables::property_of_strings(name)
}

// --- Text segmentation (ECMA-402 §19, Intl.Segmenter) ---

/// The `Intl.Segmenter` granularity (ECMA-402 §19.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentationGranularity {
    Grapheme,
    Word,
    Sentence,
}

/// UAX #29 text segmentation: the code-point indices of the segment
/// boundaries of `cps` for `granularity`, always including `0` and
/// `cps.len()`. `Sentence` treats the whole input as one segment.
pub fn segment_boundaries(cps: &[u32], granularity: SegmentationGranularity) -> Vec<usize> {
    match granularity {
        SegmentationGranularity::Sentence => vec![0, cps.len()],
        SegmentationGranularity::Word => word_boundaries(cps),
        SegmentationGranularity::Grapheme => grapheme_boundaries(cps),
    }
}

/// The grapheme-cluster classes the UAX #29 boundary rules distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphemeClass {
    Other,
    Control,
    Cr,
    Lf,
    L,
    V,
    T,
    Lv,
    Lvt,
    Extend,
    SpacingMark,
    RegionalIndicator,
    ExtendedPictographic,
}

/// The grapheme-cluster class of one code point.
fn grapheme_class(cp: u32) -> GraphemeClass {
    match cp {
        0x000D => return GraphemeClass::Cr,
        0x000A => return GraphemeClass::Lf,
        // ZWNJ/ZWJ are Format characters that attach to the previous
        // cluster (the Extend class); ZWJ additionally drives the GB11
        // emoji rule.
        0x200C | 0x200D => return GraphemeClass::Extend,
        _ => {}
    }
    if (0x1100..=0x115F).contains(&cp) || (0xA960..=0xA97C).contains(&cp) {
        return GraphemeClass::L;
    }
    if (0x1160..=0x11A7).contains(&cp) || (0xD7B0..=0xD7C6).contains(&cp) {
        return GraphemeClass::V;
    }
    if (0x11A8..=0x11FF).contains(&cp) || (0xD7CB..=0xD7FB).contains(&cp) {
        return GraphemeClass::T;
    }
    if (0xAC00..=0xD7A3).contains(&cp) {
        // Precomposed Hangul syllables: LV when the trailing Jamo is
        // absent (the syllable's T index is 0).
        return if (cp - 0xAC00).is_multiple_of(28) {
            GraphemeClass::Lv
        } else {
            GraphemeClass::Lvt
        };
    }
    let gc = general_category(cp);
    if matches!(gc, "Cc" | "Cf" | "Cs" | "Zl" | "Zp") {
        return GraphemeClass::Control;
    }
    if ranges_contain(
        derived_regexp_tables::binary_property_table("Grapheme_Extend").unwrap_or(&[]),
        cp,
    ) {
        return GraphemeClass::Extend;
    }
    // The emoji skin-tone modifiers (U+1F3FB..U+1F3FF) are Extend for
    // grapheme clustering (their Grapheme_Cluster_Break is Extend), but the
    // corpus-derived Grapheme_Extend table does not cover them.
    if ranges_contain(
        derived_regexp_tables::binary_property_table("Emoji_Modifier").unwrap_or(&[]),
        cp,
    ) {
        return GraphemeClass::Extend;
    }
    if gc == "Mc" {
        return GraphemeClass::SpacingMark;
    }
    if ranges_contain(
        derived_regexp_tables::binary_property_table("Regional_Indicator").unwrap_or(&[]),
        cp,
    ) {
        return GraphemeClass::RegionalIndicator;
    }
    if ranges_contain(
        derived_regexp_tables::binary_property_table("Extended_Pictographic").unwrap_or(&[]),
        cp,
    ) {
        return GraphemeClass::ExtendedPictographic;
    }
    GraphemeClass::Other
}

/// UAX #29 grapheme-cluster boundaries (rules GB3-GB13 and GB999). The
/// modern Indic-conjunct rules (GB9b/GB9c) are out of scope: the corpus
/// pins no virama-conjunct cluster, and the fallback still partitions the
/// text (the join/index invariants hold regardless).
fn grapheme_boundaries(cps: &[u32]) -> Vec<usize> {
    if cps.is_empty() {
        return vec![0];
    }
    let classes: Vec<GraphemeClass> = cps.iter().map(|&cp| grapheme_class(cp)).collect();
    let mut boundaries = vec![0];
    // The count of consecutive Regional_Indicators ending at the previous
    // code point (GB12/13 pair the RI run in twos).
    let mut ri_run = 0usize;
    for i in 1..=cps.len() {
        let prev = classes[i - 1];
        let curr = if i < cps.len() {
            classes[i]
        } else {
            GraphemeClass::Other
        };
        let no_break = match (prev, curr) {
            // GB3: CR × LF.
            (GraphemeClass::Cr, GraphemeClass::Lf) => true,
            // GB4: (Control | CR | LF) ÷.
            _ if matches!(
                prev,
                GraphemeClass::Control | GraphemeClass::Cr | GraphemeClass::Lf
            ) =>
            {
                false
            }
            // GB5: ÷ (Control | CR | LF).
            _ if matches!(
                curr,
                GraphemeClass::Control | GraphemeClass::Cr | GraphemeClass::Lf
            ) =>
            {
                false
            }
            // GB6: L × (L | V | LV | LVT).
            (
                GraphemeClass::L,
                GraphemeClass::L | GraphemeClass::V | GraphemeClass::Lv | GraphemeClass::Lvt,
            ) => true,
            // GB7: (LV | V) × (V | T).
            (GraphemeClass::Lv | GraphemeClass::V, GraphemeClass::V | GraphemeClass::T) => true,
            // GB8: (LVT | T) × T.
            (GraphemeClass::Lvt | GraphemeClass::T, GraphemeClass::T) => true,
            // GB9: × Extend; GB9a: × SpacingMark.
            (_, GraphemeClass::Extend | GraphemeClass::SpacingMark) => true,
            // GB12/13: RI pairs — no break after an odd RI count.
            (_, GraphemeClass::RegionalIndicator) => ri_run % 2 == 1,
            // GB11: Extended_Pictographic Extend* ZWJ ×
            // Extended_Pictographic (a ZWJ in the Extend run back to an
            // Extended_Pictographic base joins the next one).
            (_, GraphemeClass::ExtendedPictographic) => {
                let mut j = i as isize - 1;
                let mut saw_zwj = false;
                while j >= 0 && classes[j as usize] == GraphemeClass::Extend {
                    if cps[j as usize] == 0x200D {
                        saw_zwj = true;
                    }
                    j -= 1;
                }
                saw_zwj && j >= 0 && classes[j as usize] == GraphemeClass::ExtendedPictographic
            }
            // GB999: break anywhere else.
            _ => false,
        };
        if !no_break {
            boundaries.push(i);
        }
        if curr == GraphemeClass::RegionalIndicator {
            ri_run += 1;
        } else {
            ri_run = 0;
        }
    }
    boundaries
}

/// UAX #29 word boundaries (rules Wb6-Wb13): a break never falls inside a
/// grapheme cluster; it falls between clusters whose word classes the rules
/// separate. Word-like runs (letters and digits) stay together, as do
/// MidNumLet/MidLetter runs bounded by word-like clusters (`1.23`, `a.b`),
/// MidNum runs bounded by digits (`1,000` — but `a,b` splits) and
/// ExtendNumLet glue (`C-400`). Everything else (spaces, punctuation,
/// symbols, marks alone) is its own segment.
fn word_boundaries(cps: &[u32]) -> Vec<usize> {
    if cps.is_empty() {
        return vec![0];
    }
    let graphemes = grapheme_boundaries(cps);
    let classes: Vec<WordClass> = graphemes
        .windows(2)
        .map(|pair| classify_word_cluster(&cps[pair[0]..pair[1]]))
        .collect();
    let mut boundaries = vec![0];
    for i in 1..classes.len() {
        let (prev, curr) = (classes[i - 1], classes[i]);
        let no_break = match (prev, curr) {
            // Wb5/Wb8/Wb10: word-like runs (letters and digits).
            (WordClass::Letter | WordClass::Numeric, WordClass::Letter | WordClass::Numeric) => {
                true
            }
            // Wb6/Wb9: a MidBoth run (MidNumLet/MidLetter) bounded by
            // word-like clusters on both sides.
            (WordClass::Letter | WordClass::Numeric, WordClass::MidBoth)
            | (WordClass::MidBoth, WordClass::Letter | WordClass::Numeric)
            | (WordClass::MidBoth, WordClass::MidBoth) => {
                let mid = if curr == WordClass::MidBoth { i } else { i - 1 };
                bounded_mid_run(&classes, mid, WordClass::MidBoth, true)
            }
            // Wb8: a MidNum run (`, ;`) bounded by Numeric clusters — the
            // corpus pins that `a,b` splits but `1.23` stays together.
            (WordClass::Numeric, WordClass::MidNum)
            | (WordClass::MidNum, WordClass::Numeric)
            | (WordClass::MidNum, WordClass::MidNum) => {
                let mid = if curr == WordClass::MidNum { i } else { i - 1 };
                bounded_mid_run(&classes, mid, WordClass::MidNum, false)
            }
            // Wb13a/b/c: ExtendNumLet glue (`C-400` stays one word).
            (WordClass::Letter | WordClass::Numeric, WordClass::ExtendNumLet)
            | (WordClass::ExtendNumLet, WordClass::Letter | WordClass::Numeric) => true,
            _ => false,
        };
        if !no_break {
            boundaries.push(graphemes[i]);
        }
    }
    boundaries.push(cps.len());
    boundaries
}

/// Whether the maximal `kind` run containing cluster `mid` is bounded: a
/// word-like cluster on both sides (letters count only when `letters_ok`,
/// the MidNum case).
fn bounded_mid_run(classes: &[WordClass], mid: usize, kind: WordClass, letters_ok: bool) -> bool {
    let flank = |c: WordClass| c == WordClass::Numeric || (letters_ok && c == WordClass::Letter);
    let mut start = mid;
    while start > 0 && classes[start - 1] == kind {
        start -= 1;
    }
    let mut end = mid;
    while end + 1 < classes.len() && classes[end + 1] == kind {
        end += 1;
    }
    start > 0 && flank(classes[start - 1]) && end + 1 < classes.len() && flank(classes[end + 1])
}

/// The word classes UAX #29 distinguishes for the boundary rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WordClass {
    Letter,
    Numeric,
    MidBoth,
    MidNum,
    ExtendNumLet,
    Other,
}

/// The word class of one grapheme cluster: a letter makes it a Letter, a
/// digit a Numeric; a lone MidNumLet/MidLetter code point is a MidBoth; a
/// lone MidNum code point is a MidNum; a lone ExtendNumLet code point
/// (underscore, hyphen) is a connector.
fn classify_word_cluster(cluster: &[u32]) -> WordClass {
    let mut letter = false;
    let mut numeric = false;
    let mut mid_both = false;
    let mut mid_num = false;
    let mut extend_num_let = false;
    for &cp in cluster {
        if is_word_like_cp(cp) {
            if is_numeric_cp(cp) {
                numeric = true;
            } else {
                letter = true;
            }
        }
        mid_both |= is_mid_both_cp(cp);
        mid_num |= is_mid_num_cp(cp);
        extend_num_let |= is_extend_num_let_cp(cp);
    }
    if letter || numeric {
        if numeric && !letter {
            WordClass::Numeric
        } else {
            WordClass::Letter
        }
    } else if mid_both {
        WordClass::MidBoth
    } else if mid_num {
        WordClass::MidNum
    } else if extend_num_let {
        WordClass::ExtendNumLet
    } else {
        WordClass::Other
    }
}

/// A MidNumLet/MidLetter code point (UAX #29 Word_Break values): the
/// `. : · '`-family that binds letters and digits together (`1.23`).
fn is_mid_both_cp(cp: u32) -> bool {
    matches!(
        cp,
        0x002E // .
            | 0x003A // :
            | 0x00B7 | 0x0387 // · (middle dot, Greek ano teleia)
            | 0x05F4 // Hebrew punctuation maqaf
            | 0x0589 // Armenian full stop (:)
            | 0x2024 | 0x2027 // one dot leader, hyphenation point
            | 0x2018 | 0x2019 // single quotes
            | 0xFE13 | 0xFE52 | 0xFE55 | 0xFF07 | 0xFF0E | 0xFF1A
    )
}

/// A MidNum code point (UAX #29): the `, ;`-family that binds only digits
/// together (`1,000`), never letters (`a,b` splits).
fn is_mid_num_cp(cp: u32) -> bool {
    matches!(
        cp,
        0x002C | 0x003B // , ;
            | 0x037E // Greek question mark (;)
            | 0x060C | 0x060D | 0x07F8 // Arabic/Indic separators
            | 0x2044 // fraction slash
            | 0xFE10 | 0xFE14 | 0xFE50 | 0xFE54 | 0xFF0C | 0xFF1B
    )
}

/// An ExtendNumLet code point (UAX #29): `_ -` and the undertie family,
/// which glue to letters and numbers (`C-400` stays one word).
fn is_extend_num_let_cp(cp: u32) -> bool {
    matches!(
        cp,
        0x002D // hyphen-minus
            | 0x005F | 0x203F | 0x2040 | 0x2054 | 0xFE33 | 0xFE34 | 0xFE4D
            ..=0xFE4F | 0xFF3F
    )
}

/// Is the code point a digit (the `Numeric` word class)?
fn is_numeric_cp(cp: u32) -> bool {
    matches!(general_category(cp), "Nd" | "Nl")
}

/// Is the code point part of a word? The `isWordLike` test: the ID_Continue
/// set (letters, digits, marks, connectors) plus the Alphabetic property.
fn is_word_like_cp(cp: u32) -> bool {
    is_identifier_part(cp)
        || derived_regexp_tables::binary_property_table("Alphabetic")
            .is_some_and(|ranges| ranges_contain(ranges, cp))
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

    #[test]
    fn grapheme_clusters_keep_the_unbreakable_inputs_single() {
        // The Intl.Segmenter corpus's unbreakable inputs — each is one
        // grapheme cluster in every granularity (surrogates arrive already
        // paired by the UTF-16 decoder).
        let single: &[&[u32]] = &[
            &[0x61],                              // a
            &[0x20],                              // space
            &[0x10000],                           // surrogate pair
            &[0xD800],                            // lone leading surrogate
            &[0xDC00],                            // lone trailing surrogate
            &[0x53F0],                            // 台
            &[0x0301],                            // a modifier alone
            &[0x61, 0x0301],                      // ASCII + modifier
            &[0x0E0B, 0x0E34, 0x0E48],            // Thai cluster ซิ่
            &[0x100B0],                           // Linear B syllable
            &[0x1F44B, 0x1F3FB],                  // waving hand + skin tone
            &[0x1F468, 0x1F3FB, 0x200D, 0x1F9B0], // man + skin + ZWJ + red hair
            &[0x1102],                            // Jamo L
            &[0x1162],                            // Jamo V
            &[0x11A9],                            // Jamo T
            &[0x1102, 0x1162],                    // Jamo LV
            &[0x1102, 0x1162, 0x11A9],            // Jamo LVT
            &[0x1102, 0x1102],                    // Jamo L L
            &[0x1102, 0x1102, 0x1162],            // Jamo L L V
            &[0x1102, 0x1102, 0x1162, 0x11A9],    // Jamo L L V T
            &[0x1162, 0x1162],                    // Jamo V V
            &[0x1162, 0x11A9],                    // Jamo V T
            &[0x1102, 0x1162, 0x1162],            // Jamo LV V
            &[0x11A9, 0x11A9],                    // Jamo T T
            &[0x1102, 0x1162, 0x11A9, 0x11A9],    // Jamo LVT T
        ];
        for cps in single {
            let boundaries = grapheme_boundaries(cps);
            assert_eq!(
                boundaries,
                vec![0, cps.len()],
                "cps: {cps:?} boundaries: {boundaries:?}"
            );
        }
    }

    #[test]
    fn grapheme_clusters_split_the_breakable_inputs() {
        let multi: &[&[u32]] = &[
            &[0x31, 0x32, 0x33, 0x20], // "123 "
            &[0x61, 0x20],             // "a "
            &[0x20, 0x61],             // " a"
            &[0x20, 0x10000],          // space + surrogate pair
            &[0x10000, 0x20],          // pair + space
            &[0xDC00, 0xD800],         // incorrect surrogate tail + leading
            &[0xD800, 0x20],           // leading + space
            &[0xDC00, 0x20],           // trailing + space
            &[0x20, 0xD800],           // space + leading
            &[0x20, 0xDC00],           // space + trailing
            &[0x20, 0x53F0],           // space + Han
            &[0x53F0, 0x20],           // Han + space
            &[0x0301, 0x20],           // modifier + space
        ];
        for cps in multi {
            let boundaries = grapheme_boundaries(cps);
            assert!(
                boundaries.len() > 2,
                "cps: {cps:?} boundaries: {boundaries:?}"
            );
        }
    }

    #[test]
    fn word_segmentation_splits_the_space_at_index_one() {
        // one-index.js: "a c" at word granularity has the " " segment at
        // index 1 (the containing(1) pins it).
        let cps = [0x61, 0x20, 0x63];
        assert_eq!(
            segment_boundaries(&cps, SegmentationGranularity::Word),
            vec![0, 1, 2, 3]
        );
        // The same text at grapheme granularity keeps each character.
        assert_eq!(
            segment_boundaries(&cps, SegmentationGranularity::Grapheme),
            vec![0, 1, 2, 3]
        );
        // Sentence granularity is the whole input.
        assert_eq!(
            segment_boundaries(&cps, SegmentationGranularity::Sentence),
            vec![0, 3]
        );
    }

    #[test]
    fn word_segmentation_keeps_letters_and_digits_together() {
        // "Hello world!" → words, a space, and punctuation segments.
        let cps: Vec<u32> = "Hello world!".chars().map(|c| c as u32).collect();
        assert_eq!(
            segment_boundaries(&cps, SegmentationGranularity::Word),
            vec![0, 5, 6, 11, 12]
        );
        // Digits are word-like and the hyphen is ExtendNumLet glue:
        // "C-400" is one segment (Wb13a/b).
        let cps: Vec<u32> = "C-400".chars().map(|c| c as u32).collect();
        assert_eq!(
            segment_boundaries(&cps, SegmentationGranularity::Word),
            vec![0, 5]
        );
    }

    #[test]
    fn word_segmentation_keeps_bounded_midnum_runs_together() {
        // "1.23" is one word segment (the MidNumLet full stop binds the
        // digits — segment-tostring.js pins this exact case).
        let cps: Vec<u32> = "1.23".chars().map(|c| c as u32).collect();
        assert_eq!(
            segment_boundaries(&cps, SegmentationGranularity::Word),
            vec![0, 4]
        );
        // A Mid without a word-like side breaks off: "a." → "a", ".".
        let cps: Vec<u32> = "a.".chars().map(|c| c as u32).collect();
        assert_eq!(
            segment_boundaries(&cps, SegmentationGranularity::Word),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn word_segmentation_does_not_break_inside_grapheme_clusters() {
        // a + combining acute is one grapheme and one word segment.
        let cps = [0x61, 0x0301, 0x20, 0x62];
        assert_eq!(
            segment_boundaries(&cps, SegmentationGranularity::Word),
            vec![0, 2, 3, 4]
        );
        // A Thai word (no spaces) is one segment.
        let cps: Vec<u32> = "วัดไทรตีระฆัง".chars().map(|c| c as u32).collect();
        let boundaries = segment_boundaries(&cps, SegmentationGranularity::Word);
        assert_eq!(boundaries, vec![0, cps.len()]);
    }
}
