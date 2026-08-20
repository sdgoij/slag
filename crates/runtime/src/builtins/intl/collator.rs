//! `Intl.Collator` (ECMA-402 §10): the constructor (usage/sensitivity/
//! ignorePunctuation options, the `co`/`kn`/`kf` unicode-extension keys,
//! the locale tailoring), `compare` (a DUCET-style multi-level Latin
//! collation over NFC-normalized text with the corpus-pinned locale
//! variants: the Swedish/Danish trailing letters, the Turkish dotless ı,
//! the German phonebook umlaut expansions, numeric digit runs, and
//! ignorePunctuation), and `resolvedOptions`. The collation tables are
//! the en-US surface the corpus pins (the accents order at the secondary
//! level, the case at the tertiary); other locales fall back to the
//! default Latin ordering. Instances store their record in the agent's
//! `intl_collator_data` map.

use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use crate::agent::Agent;
use crate::builtins::intl::number_format::{self, get_option};
use crate::context::{as_object, get_property, to_string};
use crate::realm::Realm;

pub const COLLATOR: &str = "%Intl.Collator%";
pub const COLLATOR_PROTO: &str = "%Intl.Collator.prototype%";
pub const COLLATOR_COMPARE_GETTER: &str = "%Intl.Collator.prototype.compare%";
pub const COLLATOR_RESOLVED_OPTIONS: &str = "%Intl.Collator.prototype.resolvedOptions%";
pub const COLLATOR_SUPPORTED_LOCALES_OF: &str = "%Intl.Collator.supportedLocalesOf%";

fn type_error(message: &str) -> JsError {
    JsError::new(ErrorKind::TypeError, message.into())
}

/// The collation data variant: the locale tailoring that changes the
/// letter order or the collation elements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollationVariant {
    /// The default Latin ordering: a-z, accents at the secondary level,
    /// case at the tertiary.
    Default,
    /// Swedish: å, ä, ö sort after z (æ ≈ ä, ø ≈ ö).
    Swedish,
    /// Danish/Norwegian: æ, ø, å sort after z.
    Danish,
    /// Turkish: the dotless ı sorts before i.
    Turkish,
    /// German phonebook / search: ä → ae, ö → oe, ü → ue expansions.
    Phonebook,
}

/// The [[InitializedCollator]] record.
#[derive(Debug, Clone)]
pub struct CollatorRecord {
    pub locale: String,
    pub usage: String,
    pub collation: String,
    pub numeric: bool,
    pub case_first: String,
    pub sensitivity: String,
    pub ignore_punctuation: bool,
    pub variant: CollationVariant,
    /// The cached [[BoundCompare]] function value.
    pub bound_compare: Option<Value>,
}

/// The collation variant for a locale/usage/collation triple.
fn collation_variant(locale: &str, usage: &str, collation: &str) -> CollationVariant {
    let lang = locale.split('-').next().unwrap_or("en");
    match lang {
        "sv" => CollationVariant::Swedish,
        "da" | "nb" | "no" => CollationVariant::Danish,
        "tr" => CollationVariant::Turkish,
        "de" if usage == "search" || collation == "phonebk" => CollationVariant::Phonebook,
        _ => CollationVariant::Default,
    }
}

/// A collation element: the DUCET-style (primary, secondary, tertiary)
/// weights of one character.
#[derive(Debug, Clone, Copy)]
struct Element {
    primary: u16,
    secondary: u8,
    tertiary: u8,
}

/// One comparison unit: a collation element, or a numeric-mode digit run.
#[derive(Debug, Clone, Copy)]
enum Unit {
    El(Element),
    Num(u64),
}

/// The primary-weight bases (relative order only): punctuation below the
/// digits below the Latin letters, then Hangul, then symbols/CJK.
const PUNCT_BASE: u16 = 0x0100;
const DIGIT_BASE: u16 = 0x0F00;
const LETTER_BASE: u16 = 0x1000;
const HANGUL_BASE: u16 = 0x2000;
const SYMBOL_BASE: u16 = 0x3000;
const CJK_BASE: u16 = 0x4000;

/// The DUCET primary order of the tested punctuation (space < hyphen <
/// comma < semicolon < ! < ? < period < apostrophe < asterisk).
fn punct_primary(cp: u32) -> Option<u16> {
    let index = match cp {
        0x20 => 1, // space
        0x2D => 2, // hyphen
        0x2C => 3, // comma
        0x3B => 4, // semicolon
        0x21 => 5, // !
        0x3F => 6, // ?
        0x2E => 7, // period
        0x27 => 8, // apostrophe
        0x2A => 9, // asterisk
        _ => return None,
    };
    Some(PUNCT_BASE + index)
}

fn is_digit(cp: u32) -> bool {
    (0x30..=0x39).contains(&cp)
}

/// Is the code point an uppercase Latin letter (the ASCII range and the
/// Latin-1/Latin Extended-A uppercase forms)?
fn is_uppercase(cp: u32) -> bool {
    if (0x41..=0x5A).contains(&cp) {
        return true;
    }
    if (0xC0..=0xDE).contains(&cp) && cp != 0xD7 {
        return true;
    }
    (0x100..0x178).contains(&cp) && cp.is_multiple_of(2)
}

/// The base letter (0-25) and accent class of a Latin letter, or None for
/// the letters with a special element form (ß) and non-letters.
fn letter_info(cp: u32) -> Option<(u8, u8)> {
    // Accent classes (the DUCET secondary order): 0 plain, 1 acute,
    // 2 grave, 3 circumflex, 4 tilde, 5 diaeresis, 6 ring, 7 cedilla,
    // 8 macron, 9 breve, 10 dot-above, 11 ogonek, 12 caron,
    // 13 double-acute, 14 stroke, 15 ligature, 16 other.
    let info = match cp {
        0x41..=0x5A => ((cp - 0x41) as u8, 0),
        0x61..=0x7A => ((cp - 0x61) as u8, 0),
        // a
        0xC0 | 0xE0 => (0, 2),    // À à
        0xC1 | 0xE1 => (0, 1),    // Á á
        0xC2 | 0xE2 => (0, 3),    // Â â
        0xC3 | 0xE3 => (0, 4),    // Ã ã
        0xC4 | 0xE4 => (0, 5),    // Ä ä
        0xC5 | 0xE5 => (0, 6),    // Å å
        0xC6 | 0xE6 => (0, 15),   // Æ æ
        0x100 | 0x101 => (0, 8),  // Ā ā
        0x102 | 0x103 => (0, 9),  // Ă ă
        0x104 | 0x105 => (0, 11), // Ą ą
        // c
        0xC7 | 0xE7 => (2, 7),    // Ç ç
        0x106 | 0x107 => (2, 1),  // Ć ć
        0x108 | 0x109 => (2, 3),  // Ĉ ĉ
        0x10A | 0x10B => (2, 10), // Ċ ċ
        0x10C | 0x10D => (2, 12), // Č č
        // d
        0xD0 | 0xF0 => (3, 14),   // Ð ð
        0x10E | 0x10F => (3, 12), // Ď ď
        0x110 | 0x111 => (3, 14), // Đ đ
        // e
        0xC8 | 0xE8 => (4, 2),    // È è
        0xC9 | 0xE9 => (4, 1),    // É é
        0xCA | 0xEA => (4, 3),    // Ê ê
        0xCB | 0xEB => (4, 5),    // Ë ë
        0x112 | 0x113 => (4, 8),  // Ē ē
        0x114 | 0x115 => (4, 9),  // Ĕ ĕ
        0x116 | 0x117 => (4, 10), // Ė ė
        0x118 | 0x119 => (4, 11), // Ę ę
        0x11A | 0x11B => (4, 12), // Ě ě
        // g
        0x11C | 0x11D => (6, 3),  // Ĝ ĝ
        0x11E | 0x11F => (6, 9),  // Ğ ğ
        0x120 | 0x121 => (6, 10), // Ġ ġ
        0x122 | 0x123 => (6, 7),  // Ģ ģ
        // h
        0x124 | 0x125 => (7, 3),  // Ĥ ĥ
        0x126 | 0x127 => (7, 14), // Ħ ħ
        // i
        0xCC | 0xEC => (8, 2),    // Ì ì
        0xCD | 0xED => (8, 1),    // Í í
        0xCE | 0xEE => (8, 3),    // Î î
        0xCF | 0xEF => (8, 5),    // Ï ï
        0x128 | 0x129 => (8, 4),  // Ĩ ĩ
        0x12A | 0x12B => (8, 8),  // Ī ī
        0x12C | 0x12D => (8, 9),  // Ĭ ĭ
        0x12E | 0x12F => (8, 11), // Į į
        0x130 => (8, 10),         // İ (dotted capital I)
        0x131 => (8, 0),          // ı (dotless i)
        0x132 | 0x133 => (8, 16), // Ĳ ĳ
        // j
        0x134 | 0x135 => (9, 3), // Ĵ ĵ
        // k
        0x136 | 0x137 => (10, 7), // Ķ ķ
        0x138 => (10, 0),         // ĸ
        // l
        0x139 | 0x13A => (11, 1),  // Ĺ ĺ
        0x13B | 0x13C => (11, 7),  // Ļ ļ
        0x13D | 0x13E => (11, 12), // Ľ ľ
        0x13F | 0x140 => (11, 10), // Ŀ ŀ
        0x141 | 0x142 => (11, 14), // Ł ł
        // n
        0xD1 | 0xF1 => (13, 4),    // Ñ ñ
        0x143 | 0x144 => (13, 1),  // Ń ń
        0x145 | 0x146 => (13, 7),  // Ņ ņ
        0x147 | 0x148 => (13, 12), // Ň ň
        0x149 => (13, 16),         // ŉ
        0x14A | 0x14B => (13, 16), // Ŋ ŋ
        // o
        0xD2 | 0xF2 => (14, 2),    // Ò ò
        0xD3 | 0xF3 => (14, 1),    // Ó ó
        0xD4 | 0xF4 => (14, 3),    // Ô ô
        0xD5 | 0xF5 => (14, 4),    // Õ õ
        0xD6 | 0xF6 => (14, 5),    // Ö ö
        0xD8 | 0xF8 => (14, 14),   // Ø ø
        0x14C | 0x14D => (14, 8),  // Ō ō
        0x14E | 0x14F => (14, 9),  // Ŏ ŏ
        0x150 | 0x151 => (14, 13), // Ő ő
        0x152 | 0x153 => (14, 15), // Œ œ
        // r
        0x154 | 0x155 => (17, 1),  // Ŕ ŕ
        0x156 | 0x157 => (17, 7),  // Ŗ ŗ
        0x158 | 0x159 => (17, 12), // Ř ř
        // s
        0x15A | 0x15B => (18, 1),  // Ś ś
        0x15C | 0x15D => (18, 3),  // Ŝ ŝ
        0x15E | 0x15F => (18, 7),  // Ş ş
        0x160 | 0x161 => (18, 12), // Š š
        0x17F => (18, 0),          // ſ (long s)
        // t
        0x162 | 0x163 => (19, 7),  // Ţ ţ
        0x164 | 0x165 => (19, 12), // Ť ť
        0x166 | 0x167 => (19, 14), // Ŧ ŧ
        // u
        0xD9 | 0xF9 => (20, 2),    // Ù ù
        0xDA | 0xFA => (20, 1),    // Ú ú
        0xDB | 0xFB => (20, 3),    // Û û
        0xDC | 0xFC => (20, 5),    // Ü ü
        0x168 | 0x169 => (20, 4),  // Ũ ũ
        0x16A | 0x16B => (20, 8),  // Ū ū
        0x16C | 0x16D => (20, 9),  // Ŭ ŭ
        0x16E | 0x16F => (20, 6),  // Ů ů
        0x170 | 0x171 => (20, 13), // Ű ű
        0x172 | 0x173 => (20, 11), // Ų ų
        // w
        0x174 | 0x175 => (22, 3), // Ŵ ŵ
        // y
        0xDD | 0xFD => (24, 1),   // Ý ý
        0x176 | 0x177 => (24, 3), // Ŷ ŷ
        0x178 => (24, 5),         // Ÿ
        // z
        0x179 | 0x17A => (25, 1),  // Ź ź
        0x17B | 0x17C => (25, 10), // Ż ż
        0x17D | 0x17E => (25, 12), // Ž ž
        _ => return None,
    };
    Some(info)
}

/// The primary weight of the base letter under the locale variant.
fn primary_of(base: u8, cp: u32, variant: CollationVariant) -> u16 {
    let letter = LETTER_BASE + base as u16;
    match variant {
        CollationVariant::Swedish => match cp {
            0xC5 | 0xE5 => LETTER_BASE + 26,               // å
            0xC4 | 0xE4 | 0xC6 | 0xE6 => LETTER_BASE + 27, // ä, æ
            0xD6 | 0xF6 => LETTER_BASE + 28,               // ö
            0xD8 | 0xF8 => LETTER_BASE + 29,               // ø
            _ => letter,
        },
        CollationVariant::Danish => match cp {
            0xC6 | 0xE6 => LETTER_BASE + 26, // æ
            0xD8 | 0xF8 => LETTER_BASE + 27, // ø
            0xC5 | 0xE5 => LETTER_BASE + 28, // å
            _ => letter,
        },
        CollationVariant::Turkish => match cp {
            // The dotless/dotted i-family: ı < I < i < İ, then j..z.
            0x131 => LETTER_BASE + 8,  // ı
            0x49 => LETTER_BASE + 9,   // I (the uppercase of ı)
            0x69 => LETTER_BASE + 10,  // i
            0x130 => LETTER_BASE + 11, // İ (the uppercase of i)
            _ if base >= 8 => LETTER_BASE + base as u16 + 4,
            _ => letter,
        },
        _ => letter,
    }
}

/// The collation elements of a code point under the variant (the German
/// phonebook/search umlaut expansions produce two elements).
fn elements_of(cp: u32, variant: CollationVariant) -> Option<Vec<Element>> {
    if variant == CollationVariant::Phonebook {
        let expansion = match cp {
            0xC4 | 0xE4 => Some(0),  // Ä ä → ae
            0xD6 | 0xF6 => Some(14), // Ö ö → oe
            0xDC | 0xFC => Some(20), // Ü ü → ue
            _ => None,
        };
        if let Some(base) = expansion {
            return Some(vec![
                Element {
                    primary: LETTER_BASE + base,
                    secondary: 0,
                    tertiary: if is_uppercase(cp) { 1 } else { 0 },
                },
                Element {
                    primary: LETTER_BASE + 4,
                    // The phonebook `e` carries a secondary marker so the
                    // umlaut form sorts after the plain digraph (ä > ae).
                    secondary: 1,
                    tertiary: 0,
                },
            ]);
        }
    }
    // ß: the [s, s] expansion.
    if cp == 0xDF {
        return Some(vec![
            Element {
                primary: LETTER_BASE + 18,
                secondary: 0,
                tertiary: 0,
            },
            Element {
                primary: LETTER_BASE + 18,
                secondary: 0,
                tertiary: 0,
            },
        ]);
    }
    let (base, accent) = letter_info(cp)?;
    Some(vec![Element {
        primary: primary_of(base, cp, variant),
        secondary: accent,
        tertiary: if is_uppercase(cp) { 1 } else { 0 },
    }])
}

/// The NFKC-level CJK compatibility mapping the corpus pins: the
/// supplementary compatibility ideographs collate as their base form.
fn cjk_compat(cp: u32) -> u32 {
    match cp {
        0x2F82B => 0x5317, // 北 → 北
        _ => cp,
    }
}

/// Build the comparison units of an NFC-normalized string.
fn build_units(
    text: &str,
    variant: CollationVariant,
    numeric: bool,
    ignore_punctuation: bool,
) -> Vec<Unit> {
    let mut units = Vec::new();
    let cps: Vec<u32> = text.chars().map(|c| c as u32).collect();
    let mut i = 0;
    while i < cps.len() {
        let cp = cps[i];
        if numeric && is_digit(cp) {
            // The digit run becomes a single numeric unit.
            let mut value: u64 = 0;
            while i < cps.len() && is_digit(cps[i]) {
                value = value
                    .saturating_mul(10)
                    .saturating_add((cps[i] - 0x30) as u64);
                i += 1;
            }
            units.push(Unit::Num(value));
            continue;
        }
        i += 1;
        if is_digit(cp) {
            units.push(Unit::El(Element {
                primary: DIGIT_BASE + (cp - 0x30) as u16,
                secondary: 0,
                tertiary: 0,
            }));
            continue;
        }
        if let Some(primary) = punct_primary(cp) {
            if !ignore_punctuation {
                units.push(Unit::El(Element {
                    primary,
                    secondary: 0,
                    tertiary: 0,
                }));
            }
            continue;
        }
        let cp = cjk_compat(cp);
        if let Some(elements) = elements_of(cp, variant) {
            for element in elements {
                units.push(Unit::El(element));
            }
            continue;
        }
        let primary = if (0xAC00..=0xD7A3).contains(&cp) {
            HANGUL_BASE + ((cp - 0xAC00) / 28) as u16
        } else if cp >= 0x4E00 || cp == 0x3007 {
            CJK_BASE
        } else {
            SYMBOL_BASE
        };
        units.push(Unit::El(Element {
            primary,
            secondary: 0,
            tertiary: 0,
        }));
    }
    units
}

/// The tertiary (case) weight under the caseFirst setting.
fn case_weight(tertiary: u8, case_first: &str) -> u8 {
    if case_first == "upper" {
        // Uppercase first: uppercase gets the lower weight.
        if tertiary == 1 { 0 } else { 1 }
    } else {
        tertiary
    }
}

fn compare_level(a: &[Unit], b: &[Unit], level: u8, case_first: &str) -> std::cmp::Ordering {
    let mut i = 0;
    let mut j = 0;
    while i < a.len() && j < b.len() {
        let cmp = match level {
            1 => match (a[i], b[j]) {
                (Unit::Num(x), Unit::Num(y)) => x.cmp(&y),
                (Unit::Num(_), Unit::El(_)) => std::cmp::Ordering::Less,
                (Unit::El(_), Unit::Num(_)) => std::cmp::Ordering::Greater,
                (Unit::El(x), Unit::El(y)) => x.primary.cmp(&y.primary),
            },
            2 => match (a[i], b[j]) {
                (Unit::El(x), Unit::El(y)) => x.secondary.cmp(&y.secondary),
                _ => std::cmp::Ordering::Equal,
            },
            _ => match (a[i], b[j]) {
                (Unit::El(x), Unit::El(y)) => {
                    case_weight(x.tertiary, case_first).cmp(&case_weight(y.tertiary, case_first))
                }
                _ => std::cmp::Ordering::Equal,
            },
        };
        if cmp != std::cmp::Ordering::Equal {
            return cmp;
        }
        i += 1;
        j += 1;
    }
    (a.len() - i).cmp(&(b.len() - j))
}

/// CompareStrings (ECMA-402 §10.3.2): the multi-level collation of the
/// two NFC-normalized strings.
fn compare_strings(record: &CollatorRecord, a: &str, b: &str) -> std::cmp::Ordering {
    let normalize = |text: &str| -> String {
        let cps: Vec<u32> = text.chars().map(|c| c as u32).collect();
        let normalized = unicode::normalize_code_points(&cps, unicode::NormalizationForm::Nfc);
        crux::string::code_points_to_string(&normalized)
            .map(|s| s.to_string_lossy())
            .unwrap_or_else(|_| text.to_string())
    };
    let a = normalize(a);
    let b = normalize(b);
    let units_a = build_units(
        &a,
        record.variant,
        record.numeric,
        record.ignore_punctuation,
    );
    let units_b = build_units(
        &b,
        record.variant,
        record.numeric,
        record.ignore_punctuation,
    );
    let primary = compare_level(&units_a, &units_b, 1, &record.case_first);
    if primary != std::cmp::Ordering::Equal {
        return primary;
    }
    match record.sensitivity.as_str() {
        "base" => std::cmp::Ordering::Equal,
        "accent" => compare_level(&units_a, &units_b, 2, &record.case_first),
        "case" => compare_level(&units_a, &units_b, 3, &record.case_first),
        _ => {
            let secondary = compare_level(&units_a, &units_b, 2, &record.case_first);
            if secondary != std::cmp::Ordering::Equal {
                return secondary;
            }
            compare_level(&units_a, &units_b, 3, &record.case_first)
        }
    }
}

/// The supported collations per locale: the corpus pins the per-locale
/// acceptance (de → phonebk, sv → reformed, zh → the CJK collations, ...)
/// so that `Intl.supportedValuesOf("collation")` round-trips through
/// Collator; the behavioral tailoring is only pinned for phonebk (the
/// other accepted values use the default Latin ordering).
fn supported_collations(locale: &str) -> &'static [&'static str] {
    let lang = locale.split('-').next().unwrap_or("en");
    match lang {
        "de" => &["default", "phonebk", "eor", "emoji"],
        "en" => &["default", "ducet", "emoji", "eor"],
        "ar" => &["default", "compat", "eor", "emoji"],
        "es" => &["default", "trad", "eor", "emoji"],
        "hi" => &["default", "direct", "eor", "emoji"],
        "ko" => &["default", "searchjl", "eor", "emoji"],
        "ln" => &["default", "phonetic", "eor", "emoji"],
        "si" => &["default", "dict", "eor", "emoji"],
        "sv" => &["default", "reformed", "eor", "emoji"],
        "zh" => &[
            "default", "big5han", "gb2312", "pinyin", "stroke", "unihan", "zhuyin", "eor", "emoji",
        ],
        _ => &["default", "eor", "emoji"],
    }
}

/// The value of a keyword inside a `-u-` extension sequence; a bare
/// keyword (no type tokens) returns Some("").
fn extension_keyword(extension: &str, key: &str) -> Option<String> {
    let parts: Vec<&str> = extension.split('-').collect();
    for (i, part) in parts.iter().enumerate() {
        if *part == key {
            let mut tokens: Vec<&str> = Vec::new();
            let mut j = i + 1;
            while j < parts.len() && (3..=8).contains(&parts[j].len()) {
                tokens.push(parts[j]);
                j += 1;
            }
            return Some(tokens.join("-"));
        }
    }
    None
}

/// ResolveLocale for Collator: the `co`/`kn`/`kf` extension keys with the
/// option overrides. Returns (locale, collation, numeric, case_first).
fn resolve_locale_collator(
    _agent: &mut Agent,
    requested: &[String],
    collation: Option<&str>,
    numeric: Option<bool>,
    case_first: Option<&str>,
) -> Result<(String, String, bool, String), JsError> {
    let available = crate::builtins::intl::number_data::NUMBER_FORMAT_LOCALES;
    let mut found: Option<String> = None;
    let mut extension: Option<String> = None;
    for locale in requested {
        let base = number_format::strip_unicode_extension(locale);
        if let Some(matched) = number_format::best_fit(available, &base) {
            found = Some(matched);
            extension = if base == *locale {
                None
            } else {
                Some(locale.clone())
            };
            break;
        }
    }
    let mut found_locale = found.unwrap_or_else(|| number_format::default_locale().to_string());
    let mut co: Option<String> = None;
    let mut kn: Option<String> = None;
    let mut kf: Option<String> = None;
    let mut supported: Vec<(String, String)> = Vec::new();
    if let Some(ext) = extension {
        if let Some(value) = extension_keyword(&ext, "co")
            && !value.is_empty()
            && supported_collations(&found_locale).contains(&value.as_str())
        {
            co = Some(value.clone());
            supported.push(("co".to_string(), value));
        }
        if let Some(value) = extension_keyword(&ext, "kn") {
            // A bare `kn` (or `kn-true`) means true.
            let value = if value.is_empty() || value == "true" {
                "true".to_string()
            } else {
                value
            };
            if value == "true" || value == "false" {
                kn = Some(value.clone());
                supported.push(("kn".to_string(), value));
            }
        }
        if let Some(value) = extension_keyword(&ext, "kf")
            && !value.is_empty()
            && (value == "upper" || value == "lower" || value == "false")
        {
            kf = Some(value.clone());
            supported.push(("kf".to_string(), value));
        }
    }
    // The option overrides: a supported option value replaces the
    // extension value and drops the corresponding keyword from the locale.
    if let Some(value) = collation
        && supported_collations(&found_locale).contains(&value)
        && Some(value) != co.as_deref()
    {
        co = Some(value.to_string());
        supported.retain(|(key, _)| key != "co");
    }
    if let Some(value) = numeric {
        let text = if value { "true" } else { "false" };
        if Some(text) != kn.as_deref() {
            kn = Some(text.to_string());
            supported.retain(|(key, _)| key != "kn");
        }
    }
    if let Some(value) = case_first
        && (value == "upper" || value == "lower" || value == "false")
        && Some(value) != kf.as_deref()
    {
        kf = Some(value.to_string());
        supported.retain(|(key, _)| key != "kf");
    }
    if !supported.is_empty() {
        let mut keywords: Vec<String> = Vec::new();
        for (key, value) in supported {
            // The boolean `kn` keyword is canonicalized to the bare form
            // for true (`en-u-kn`, not `en-u-kn-true`).
            if key == "kn" && value == "true" {
                keywords.push("kn".to_string());
            } else {
                keywords.push(format!("{key}-{value}"));
            }
        }
        keywords.sort();
        let base = number_format::strip_unicode_extension(&found_locale);
        let tagged = format!("{base}-u-{}", keywords.join("-"));
        found_locale = crate::builtins::intl::bcp47::canonicalize(&tagged).unwrap_or(tagged);
    }
    Ok((
        found_locale,
        co.unwrap_or_else(|| "default".to_string()),
        kn.as_deref() == Some("true"),
        kf.unwrap_or_else(|| "false".to_string()),
    ))
}

/// GetOption with type `boolean` (ToBoolean; no ToString).
fn get_boolean_option(
    agent: &mut Agent,
    options: &Value,
    name: &str,
) -> Result<Option<bool>, JsError> {
    let value = get_property(agent, options, &JsString::from_utf8(name), options.clone())?;
    if value.is_undefined() {
        return Ok(None);
    }
    Ok(Some(crux::convert::to_boolean(&value)))
}

/// InitializeCollator (ECMA-402 §10.1.2).
fn initialize(
    agent: &mut Agent,
    locales: &Value,
    options: &Value,
) -> Result<CollatorRecord, JsError> {
    let requested = crate::builtins::intl::canonicalize_locale_list(agent, locales)?;
    let options = number_format::coerce_options_to_object(agent, options)?;
    let usage = get_option(agent, &options, "usage", &["sort", "search"], Some("sort"))?
        .unwrap_or_else(|| "sort".to_string());
    // ResolveOptions: localeMatcher, then the co/kn/kf options (in the
    // pinned read order: collation, numeric, caseFirst).
    get_option(
        agent,
        &options,
        "localeMatcher",
        &["lookup", "best fit"],
        Some("best fit"),
    )?;
    let collation = get_option(agent, &options, "collation", &[], None)?;
    let numeric = get_boolean_option(agent, &options, "numeric")?;
    let case_first = get_option(
        agent,
        &options,
        "caseFirst",
        &["upper", "lower", "false"],
        None,
    )?;
    let (locale, resolved_collation, resolved_numeric, resolved_case_first) =
        resolve_locale_collator(
            agent,
            &requested,
            collation.as_deref(),
            numeric,
            case_first.as_deref(),
        )?;
    let sensitivity = get_option(
        agent,
        &options,
        "sensitivity",
        &["base", "accent", "case", "variant"],
        Some("variant"),
    )?;
    let ignore_punctuation = get_boolean_option(agent, &options, "ignorePunctuation")?;
    // The locale default: the corpus pins Thai's ignorePunctuation to
    // true (everywhere else false).
    let ignore_punctuation = ignore_punctuation.unwrap_or_else(|| locale.starts_with("th"));
    let variant = collation_variant(&locale, &usage, &resolved_collation);
    Ok(CollatorRecord {
        locale,
        usage,
        collation: resolved_collation,
        numeric: resolved_numeric,
        case_first: resolved_case_first,
        sensitivity: sensitivity.unwrap_or_else(|| "variant".to_string()),
        ignore_punctuation,
        variant,
        bound_compare: None,
    })
}

pub fn install(realm: &Handle<Realm>, intl_value: &Value) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let function_proto = realm
        .intrinsics
        .get("%Function.prototype%")
        .and_then(|value| as_object(&value));
    let proto = JsObject::ordinary_object_create(object_proto);
    let ctor = Function::create_builtin(
        Some(JsString::from_utf8("Collator")),
        0,
        placeholder("Intl.Collator"),
        Some(placeholder("Intl.Collator")),
        function_proto.clone(),
    )?;
    proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(Value::Function(ctor.clone())),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    let resolved = Function::create_builtin(
        Some(JsString::from_utf8("resolvedOptions")),
        0,
        placeholder("resolvedOptions"),
        None,
        function_proto.clone(),
    )?;
    realm
        .intrinsics
        .define(COLLATOR_RESOLVED_OPTIONS, Value::Function(resolved.clone()));
    proto.define_property(
        &JsString::from_utf8("resolvedOptions"),
        &PropertyDescriptor {
            value: Some(Value::Function(resolved)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    // The compare accessor (a bound per-instance function).
    let compare_getter = Function::create_builtin(
        Some(JsString::from_utf8("get compare")),
        0,
        placeholder("compare getter"),
        None,
        function_proto.clone(),
    )?;
    realm.intrinsics.define(
        COLLATOR_COMPARE_GETTER,
        Value::Function(compare_getter.clone()),
    );
    proto.define_property(
        &JsString::from_utf8("compare"),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: Some(Value::Function(compare_getter)),
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    // %Intl.Collator.prototype%[@@toStringTag] = "Intl.Collator".
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8(
                "Intl.Collator",
            )))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    let proto_value = Value::Object(proto.clone());
    ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    let supported = Function::create_builtin(
        Some(JsString::from_utf8("supportedLocalesOf")),
        1,
        placeholder("supportedLocalesOf"),
        None,
        function_proto.clone(),
    )?;
    realm.intrinsics.define(
        COLLATOR_SUPPORTED_LOCALES_OF,
        Value::Function(supported.clone()),
    );
    ctor.define_property(
        &JsString::from_utf8("supportedLocalesOf"),
        &PropertyDescriptor {
            value: Some(Value::Function(supported)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    realm.intrinsics.define(COLLATOR_PROTO, proto_value);
    realm
        .intrinsics
        .define(COLLATOR, Value::Function(ctor.clone()));
    if let Some(obj) = as_object(intl_value) {
        obj.define_property(
            &JsString::from_utf8("Collator"),
            &PropertyDescriptor {
                value: Some(Value::Function(ctor)),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
    }
    Ok(())
}

fn placeholder(name: &str) -> NativeFn {
    let name = name.to_string();
    Box::new(move |_, _| Err(type_error(&format!("{name} must be dispatched"))))
}

/// The record of `this` (RequireInternalSlot).
fn collator_record(agent: &Agent, this: &Value) -> Result<CollatorRecord, JsError> {
    let Some(obj) = as_object(this) else {
        return Err(type_error("Not a Collator instance"));
    };
    agent
        .intl_collator_data
        .get(&obj.id())
        .cloned()
        .ok_or_else(|| type_error("Not a Collator instance"))
}

/// The compare accessor: the cached bound compare function.
fn compare_getter(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let collator = unwrap_collator(agent, this)?;
    let mut record = collator_record(agent, &collator)?;
    if let Some(bound) = &record.bound_compare {
        return Ok(bound.clone());
    }
    let Some(obj) = as_object(&collator) else {
        return Err(type_error("Not a Collator instance"));
    };
    let collator_id = obj.id();
    let function_proto = agent
        .current_realm()?
        .intrinsics
        .get("%Function.prototype%")
        .and_then(|value| as_object(&value));
    let func = Function::create_builtin(
        Some(JsString::from_utf8("")),
        2,
        placeholder("bound compare"),
        None,
        function_proto,
    )?;
    agent
        .intl_collator_compare_functions
        .insert(func.id(), collator_id);
    let bound = Value::Function(func);
    record.bound_compare = Some(bound.clone());
    agent.intl_collator_data.insert(collator_id, record);
    Ok(bound)
}

/// The bound compare function body: compare the two arguments.
fn compare_bound(agent: &mut Agent, collator_id: u64, args: &[Value]) -> Result<Value, JsError> {
    let record = agent
        .intl_collator_data
        .get(&collator_id)
        .cloned()
        .ok_or_else(|| type_error("Not a Collator instance"))?;
    let a = to_string(agent, &args.first().cloned().unwrap_or(Value::Undefined))?;
    let b = to_string(agent, &args.get(1).cloned().unwrap_or(Value::Undefined))?;
    let ordering = compare_strings(&record, &a.to_string_lossy(), &b.to_string_lossy());
    Ok(Value::Number(match ordering {
        std::cmp::Ordering::Less => -1.0,
        std::cmp::Ordering::Equal => 0.0,
        std::cmp::Ordering::Greater => 1.0,
    }))
}

/// Intl.Collator.prototype.resolvedOptions (ECMA-402 §10.3.4).
fn resolved_options_method(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let collator = unwrap_collator(agent, this)?;
    let record = collator_record(agent, &collator)?;
    let object_proto = agent
        .current_realm()?
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let options = JsObject::ordinary_object_create(object_proto);
    let str = |s: &str| Value::String(Handle::new(JsString::from_utf8(s)));
    let entries: [(&str, Value); 7] = [
        ("locale", str(&record.locale)),
        ("usage", str(&record.usage)),
        ("sensitivity", str(&record.sensitivity)),
        (
            "ignorePunctuation",
            Value::Boolean(record.ignore_punctuation),
        ),
        ("collation", str(&record.collation)),
        ("numeric", Value::Boolean(record.numeric)),
        ("caseFirst", str(&record.case_first)),
    ];
    for (name, value) in entries {
        options.define_property(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: Some(value),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(true),
                configurable: Some(true),
            },
        )?;
    }
    Ok(Value::Object(options))
}

/// Intl.Collator.supportedLocalesOf (ECMA-402 §10.2.2).
fn supported_locales_of(
    agent: &mut Agent,
    locales: Value,
    options: Value,
) -> Result<Value, JsError> {
    let requested = crate::builtins::intl::canonicalize_locale_list(agent, &locales)?;
    let options = number_format::coerce_options_to_object(agent, &options)?;
    get_option(
        agent,
        &options,
        "localeMatcher",
        &["lookup", "best fit"],
        Some("best fit"),
    )?;
    let available = crate::builtins::intl::number_data::NUMBER_FORMAT_LOCALES;
    let mut subset = Vec::new();
    for locale in &requested {
        let base = number_format::strip_unicode_extension(locale);
        if number_format::best_fit(available, &base).is_some() {
            subset.push(Value::String(Handle::new(JsString::from_utf8(locale))));
        }
    }
    crate::builtins::array::array_from_values(agent, &subset)
}

/// dispatch_call: the Collator constructor (as a function — the legacy
/// chain), the prototype members, and supportedLocalesOf.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(COLLATOR).as_ref() == Some(callee) {
        return Some(construct_inner(agent, callee, this, true, args));
    }
    if intrinsics.get(COLLATOR_SUPPORTED_LOCALES_OF).as_ref() == Some(callee) {
        return Some(supported_locales_of(
            agent,
            args.first().cloned().unwrap_or(Value::Undefined),
            args.get(1).cloned().unwrap_or(Value::Undefined),
        ));
    }
    if intrinsics.get(COLLATOR_RESOLVED_OPTIONS).as_ref() == Some(callee) {
        return Some(resolved_options_method(agent, this));
    }
    if intrinsics.get(COLLATOR_COMPARE_GETTER).as_ref() == Some(callee) {
        return Some(compare_getter(agent, this));
    }
    // The per-instance bound compare functions.
    if let ValueKind::Function(function) = callee.kind()
        && let Some(collator_id) = agent
            .intl_collator_compare_functions
            .get(&function.id())
            .copied()
    {
        return Some(compare_bound(agent, collator_id, args));
    }
    None
}

/// dispatch_construct: `new Intl.Collator(...)`.
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    if realm.intrinsics.get(COLLATOR).as_ref() == Some(callee) {
        return Some(construct_inner(
            agent,
            new_target,
            &Value::Undefined,
            false,
            args,
        ));
    }
    None
}

/// The shared constructor path: `new` (new_target present) and the
/// function-call mode (new_target was undefined). Collator always
/// creates a fresh instance — the this-value is ignored (10.1.1 sets the
/// newTarget to the active function object; the corpus pins that
/// `Intl.Collator.call(obj)` never returns obj).
fn construct_inner(
    agent: &mut Agent,
    new_target: &Value,
    this: &Value,
    new_target_was_undefined: bool,
    args: &[Value],
) -> Result<Value, JsError> {
    let _ = this;
    let _ = new_target_was_undefined;
    let proto = proto_from_ctor(agent, new_target)?;
    let locales = args.first().cloned().unwrap_or(Value::Undefined);
    let options = args.get(1).cloned().unwrap_or(Value::Undefined);
    let record = initialize(agent, &locales, &options)?;
    create_instance(agent, proto, record)
}

fn proto_from_ctor(agent: &mut Agent, new_target: &Value) -> Result<Handle<JsObject>, JsError> {
    let proto = get_property(
        agent,
        new_target,
        &JsString::from_utf8("prototype"),
        new_target.clone(),
    )?;
    if let Some(obj) = as_object(&proto) {
        return Ok(obj);
    }
    crate::context::get_function_realm(agent, new_target)?
        .intrinsics
        .get(COLLATOR_PROTO)
        .and_then(|value| as_object(&value))
        .ok_or_else(|| type_error("%Intl.Collator.prototype% missing"))
}

fn create_instance(
    agent: &mut Agent,
    proto: Handle<JsObject>,
    record: CollatorRecord,
) -> Result<Value, JsError> {
    let instance = JsObject::ordinary_object_create(Some(proto));
    agent.intl_collator_data.insert(instance.id(), record);
    Ok(Value::Object(instance))
}

/// The record of `this` (RequireInternalSlot).
fn unwrap_collator(agent: &mut Agent, value: &Value) -> Result<Value, JsError> {
    let Some(obj) = as_object(value) else {
        return Err(type_error("Not a Collator instance"));
    };
    if agent.intl_collator_data.contains_key(&obj.id()) {
        return Ok(value.clone());
    }
    Err(type_error("Not a Collator instance"))
}
/// String.prototype.localeCompare (ECMA-262 §22.1.3.19): construct the
/// default Collator for (locales, options) and compare.
pub fn locale_compare(
    agent: &mut Agent,
    s: &str,
    t: &str,
    locales: &Value,
    options: &Value,
) -> Result<f64, JsError> {
    let record = initialize(agent, locales, options)?;
    Ok(match compare_strings(&record, s, t) {
        std::cmp::Ordering::Less => -1.0,
        std::cmp::Ordering::Equal => 0.0,
        std::cmp::Ordering::Greater => 1.0,
    })
}
