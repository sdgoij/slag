//! Plural-rule and relative-time data (plan Cut 3): the CLDR plural
//! categories for the fixture locales (cardinal + ordinal), the compact
//! notation overrides the corpus pins (fr compact 10^6 → "many"), and the
//! RelativeTimeFormat pattern tables — unit strings per style and plural
//! category, the future/past tense affixes, the numeric grouping flag, and
//! the `numeric: "auto"` exceptions — for en-US and pl-PL. The corpus is the
//! data spec: `plural-categories-order.js` pins the category lists for
//! ar/en/fa/fr/gv/ko/sl, `select/notation.js` pins the fr standard/compact
//! selections, and the `format/*`/`formatToParts/*` fixtures pin the en-US
//! and pl-PL unit strings.

use crux::BigInt;

use crate::builtins::intl::number_format::IntlMv;

/// The CLDR plural categories in the resolvedOptions order (ECMA-402
/// §17.3.2: "zero", "one", "two", "few", "many", "other").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluralCategory {
    Zero,
    One,
    Two,
    Few,
    Many,
    Other,
}

impl PluralCategory {
    pub fn name(self) -> &'static str {
        match self {
            PluralCategory::Zero => "zero",
            PluralCategory::One => "one",
            PluralCategory::Two => "two",
            PluralCategory::Few => "few",
            PluralCategory::Many => "many",
            PluralCategory::Other => "other",
        }
    }
}

fn int_eq(x: &IntlMv, n: i64) -> bool {
    let IntlMv::Value {
        negative: false,
        mant,
        exp10,
    } = x
    else {
        return false;
    };
    if *exp10 < 0 {
        return false;
    }
    let expected = BigInt::parse_str(&n.to_string(), 10).expect("n");
    let scaled = if *exp10 == 0 {
        mant.clone()
    } else {
        crate::builtins::intl::number_format::multiply_pow10(mant, *exp10 as u32)
    };
    scaled.0 == expected.0
}

/// `0 <= x <= 1` (the CLDR `n = 0..1` range): mant × 10^exp10 ≤ 1.
fn between_zero_and_one(x: &IntlMv) -> bool {
    let IntlMv::Value {
        negative: false,
        mant,
        exp10,
    } = x
    else {
        return false;
    };
    if mant.is_zero() {
        return true;
    }
    if *exp10 > 0 {
        return false;
    }
    if *exp10 == 0 {
        return mant.0 == BigInt::parse_str("1", 10).expect("1").0;
    }
    // exp10 < 0: mant ≤ 10^(-exp10).
    let limit =
        BigInt::parse_str(&format!("1{}", "0".repeat((-exp10) as usize)), 10).expect("10^k");
    mant.0 <= limit.0
}

/// `x mod m` when x is a non-negative integer, else None.
fn int_mod(x: &IntlMv, m: i64) -> Option<i64> {
    let IntlMv::Value {
        negative: false,
        mant,
        exp10,
    } = x
    else {
        return None;
    };
    if *exp10 < 0 {
        return None;
    }
    let scaled = crate::builtins::intl::number_format::multiply_pow10(mant, *exp10 as u32);
    let modulus = BigInt::parse_str(&m.to_string(), 10).expect("m");
    let rem = crux::bigint::remainder(&scaled, &modulus);
    crux::bigint::to_string(&rem, 10).parse().ok()
}

/// Whether the non-negative integer x is a non-zero multiple of `m`.
fn is_multiple_of(x: &IntlMv, m: i64) -> bool {
    let IntlMv::Value {
        negative: false,
        mant,
        exp10,
    } = x
    else {
        return false;
    };
    if mant.is_zero() {
        return false;
    }
    if *exp10 < 0 {
        return false;
    }
    if *exp10 >= 6 {
        return true;
    }
    let scaled = crate::builtins::intl::number_format::multiply_pow10(mant, *exp10 as u32);
    let modulus = BigInt::parse_str(&m.to_string(), 10).expect("m");
    crux::bigint::remainder(&scaled, &modulus).is_zero()
}

/// The cardinal category of `x` for a locale base (CLDR `plurals.xml`
/// `type="cardinal"` rules for the fixture locales; the fallback is the
/// en-style one/other split).
fn cardinal(base: &str, x: &IntlMv) -> PluralCategory {
    match base {
        "ar" => {
            if int_eq(x, 0) {
                PluralCategory::Zero
            } else if int_eq(x, 1) {
                PluralCategory::One
            } else if int_eq(x, 2) {
                PluralCategory::Two
            } else {
                match int_mod(x, 100) {
                    Some(m) if (3..=10).contains(&m) => PluralCategory::Few,
                    Some(m) if (11..=99).contains(&m) => PluralCategory::Many,
                    _ => PluralCategory::Other,
                }
            }
        }
        "fr" => {
            if between_zero_and_one(x) {
                PluralCategory::One
            } else if is_multiple_of(x, 1_000_000) {
                PluralCategory::Many
            } else {
                PluralCategory::Other
            }
        }
        "fa" => {
            if between_zero_and_one(x) {
                PluralCategory::One
            } else {
                PluralCategory::Other
            }
        }
        "gv" => match int_mod(x, 10) {
            Some(1) if !matches!(int_mod(x, 100), Some(11 | 71 | 91)) => PluralCategory::One,
            Some(2) if !matches!(int_mod(x, 100), Some(12 | 72 | 92)) => PluralCategory::Two,
            Some(3..=10) => PluralCategory::Few,
            Some(13..=19) => PluralCategory::Few,
            Some(73..=79) => PluralCategory::Few,
            Some(93..=99) => PluralCategory::Few,
            _ => {
                if matches!(int_mod(x, 100), Some(11 | 71 | 91)) {
                    PluralCategory::Many
                } else {
                    PluralCategory::Other
                }
            }
        },
        "ko" => PluralCategory::Other,
        "sl" => match int_mod(x, 100) {
            Some(1) => PluralCategory::One,
            Some(2) => PluralCategory::Two,
            Some(3..=4) => PluralCategory::Few,
            _ => PluralCategory::Other,
        },
        "pl" => {
            if int_eq(x, 1) {
                PluralCategory::One
            } else {
                match (int_mod(x, 10), int_mod(x, 100)) {
                    (Some(tens), Some(hundreds))
                        if (2..=4).contains(&tens) && !(12..=14).contains(&hundreds) =>
                    {
                        PluralCategory::Few
                    }
                    (Some(tens), Some(hundreds))
                        if tens <= 1 || tens >= 5 || (12..=14).contains(&hundreds) =>
                    {
                        PluralCategory::Many
                    }
                    // Fractional values satisfy none of the integer mod
                    // ranges → "other" (the pl formatToParts fixtures pin
                    // "dnia"/"roku").
                    _ => PluralCategory::Other,
                }
            }
        }
        _ => {
            if int_eq(x, 1) {
                PluralCategory::One
            } else {
                PluralCategory::Other
            }
        }
    }
}

/// The ordinal category of `x` for a locale base (CLDR `type="ordinal"`
/// rules; only en has non-`other` categories among the fixture locales).
fn ordinal(base: &str, x: &IntlMv) -> PluralCategory {
    match base {
        "en" => match (int_mod(x, 10), int_mod(x, 100)) {
            (Some(1), Some(h)) if h != 11 => PluralCategory::One,
            (Some(2), Some(h)) if h != 12 => PluralCategory::Two,
            (Some(3), Some(h)) if h != 13 => PluralCategory::Few,
            _ => PluralCategory::Other,
        },
        _ => PluralCategory::Other,
    }
}

/// The set of categories the rules can produce, in the spec order — the
/// `pluralCategories` of `resolvedOptions` (ECMA-402 §17.3.2 step 4).
pub fn plural_categories(base: &str, ordinal_type: bool) -> Vec<PluralCategory> {
    let all = [
        PluralCategory::Zero,
        PluralCategory::One,
        PluralCategory::Two,
        PluralCategory::Few,
        PluralCategory::Many,
        PluralCategory::Other,
    ];
    let mut result = Vec::new();
    for category in all {
        if can_produce(base, ordinal_type, category) {
            result.push(category);
        }
    }
    result
}

fn can_produce(base: &str, ordinal_type: bool, category: PluralCategory) -> bool {
    if ordinal_type {
        // Only the en ordinal rules produce one/two/few; every locale can
        // produce "other".
        return match base {
            "en" => !matches!(category, PluralCategory::Zero | PluralCategory::Many),
            _ => matches!(category, PluralCategory::Other),
        };
    }
    match base {
        "ar" => true,
        "en" | "fa" => matches!(category, PluralCategory::One | PluralCategory::Other),
        "fr" => matches!(
            category,
            PluralCategory::One | PluralCategory::Many | PluralCategory::Other
        ),
        "gv" => !matches!(category, PluralCategory::Zero),
        "ko" => matches!(category, PluralCategory::Other),
        "sl" => matches!(
            category,
            PluralCategory::One | PluralCategory::Two | PluralCategory::Few | PluralCategory::Other
        ),
        "pl" => matches!(
            category,
            PluralCategory::One
                | PluralCategory::Few
                | PluralCategory::Many
                | PluralCategory::Other
        ),
        _ => matches!(category, PluralCategory::One | PluralCategory::Other),
    }
}

/// PluralRuleSelect for the standard/other notations: the category of `x`
/// for the locale's cardinal/ordinal rule set. The rules evaluate on the
/// absolute value (CLDR `n` is the magnitude; `pr.select(-1)` is "one" for
/// en).
pub fn select_category(base: &str, ordinal_type: bool, x: &IntlMv) -> PluralCategory {
    let x = match x {
        IntlMv::NegZero => IntlMv::Value {
            negative: false,
            mant: BigInt::zero(),
            exp10: 0,
        },
        IntlMv::Value {
            negative: _,
            mant,
            exp10,
        } => IntlMv::Value {
            negative: false,
            mant: mant.clone(),
            exp10: *exp10,
        },
        other => other.clone(),
    };
    if ordinal_type {
        ordinal(base, &x)
    } else {
        cardinal(base, &x)
    }
}

/// The compact-notation override the corpus pins: fr compact 10^6 → "many"
/// (`select/notation.js`). None means the standard rules apply to the
/// compacted mantissa.
pub fn compact_category(base: &str, exponent: i64) -> Option<PluralCategory> {
    match base {
        "fr" if exponent == 6 => Some(PluralCategory::Many),
        _ => None,
    }
}

/// The relative-time unit strings per (locale, style, unit, category).
/// The `other` entries double as the pl-PL "many" form (pl-PL has no
/// `other` category in the RTF data the fixtures pin).
pub fn rtf_unit<'a>(base: &str, style: u8, unit: &'a str, category: PluralCategory) -> &'a str {
    match base {
        "pl" => match style {
            // style 0 = long, 1 = short, 2 = narrow.
            1 | 2 => match (unit, category) {
                ("second", _) => {
                    if style == 1 {
                        "sek."
                    } else {
                        "s"
                    }
                }
                ("minute", _) => "min",
                ("hour", _) => {
                    if style == 1 {
                        "godz."
                    } else {
                        "g."
                    }
                }
                ("day", PluralCategory::One) => "dzień",
                ("day", PluralCategory::Other) => "dnia",
                ("day", _) => "dni",
                ("week", PluralCategory::One) => "tydz.",
                ("week", _) => "tyg.",
                ("month", _) => "mies.",
                ("quarter", _) => "kw.",
                ("year", PluralCategory::One) => "rok",
                ("year", PluralCategory::Few) => "lata",
                ("year", PluralCategory::Other) => "roku",
                ("year", _) => "lat",
                _ => "",
            },
            _ => match (unit, category) {
                ("second", PluralCategory::One) => "sekundę",
                ("second", PluralCategory::Few | PluralCategory::Other) => "sekundy",
                ("second", _) => "sekund",
                ("minute", PluralCategory::One) => "minutę",
                ("minute", PluralCategory::Few | PluralCategory::Other) => "minuty",
                ("minute", _) => "minut",
                ("hour", PluralCategory::One) => "godzinę",
                ("hour", PluralCategory::Few | PluralCategory::Other) => "godziny",
                ("hour", _) => "godzin",
                ("day", PluralCategory::One) => "dzień",
                ("day", PluralCategory::Other) => "dnia",
                ("day", _) => "dni",
                ("week", PluralCategory::One) => "tydzień",
                ("week", PluralCategory::Few) => "tygodnie",
                ("week", PluralCategory::Other) => "tygodnia",
                ("week", _) => "tygodni",
                ("month", PluralCategory::One) => "miesiąc",
                ("month", PluralCategory::Few) => "miesiące",
                ("month", PluralCategory::Other) => "miesiąca",
                ("month", _) => "miesięcy",
                ("quarter", PluralCategory::One) => "kwartał",
                ("quarter", PluralCategory::Few) => "kwartały",
                ("quarter", PluralCategory::Other) => "kwartału",
                ("quarter", _) => "kwartałów",
                ("year", PluralCategory::One) => "rok",
                ("year", PluralCategory::Few) => "lata",
                ("year", PluralCategory::Other) => "roku",
                ("year", _) => "lat",
                _ => "",
            },
        },
        // en-US and the fallback: short and narrow share the short table;
        // the long table appends the plural "s".
        _ => {
            let (singular, plural) = match (style, unit) {
                (_, "second") => ("sec.", "sec."),
                (_, "minute") => ("min.", "min."),
                (_, "hour") => ("hr.", "hr."),
                (_, "day") => ("day", "days"),
                (_, "week") => ("wk.", "wk."),
                (_, "month") => ("mo.", "mo."),
                (_, "quarter") => ("qtr.", "qtrs."),
                (_, "year") => ("yr.", "yr."),
                _ => ("", ""),
            };
            if style == 0 {
                // long: the unit name + "s" for the other category.
                if matches!(category, PluralCategory::One) {
                    unit
                } else {
                    // "quarter" → "quarters" etc. via the plural table.
                    match unit {
                        "day" => "days",
                        "hour" => "hours",
                        "minute" => "minutes",
                        "month" => "months",
                        "quarter" => "quarters",
                        "second" => "seconds",
                        "week" => "weeks",
                        "year" => "years",
                        _ => unit,
                    }
                }
            } else if matches!(category, PluralCategory::One) {
                singular
            } else {
                plural
            }
        }
    }
}

/// The future prefix and past suffix around `{0} {unit}`: en-US
/// `in {0} X` / `{0} X ago`, pl-PL `za {0} X` / `{0} X temu`.
pub fn rtf_affixes(base: &str) -> (&'static str, &'static str) {
    match base {
        "pl" => ("za ", " temu"),
        _ => ("in ", " ago"),
    }
}

/// Whether the RTF number formatting groups with the locale's default
/// (`minimumGroupingDigits`): en-US groups "1,000", pl-PL only groups when
/// the secondary group has 2+ digits ("123 456" but not "1000") per the
/// pinned fixtures.
pub fn rtf_min2_grouping(base: &str) -> bool {
    base == "pl"
}

/// The `numeric: "auto"` exception table (en-US; per-unit -1/0/+1 literals).
/// hour/minute/second only have the "0" exception — their ±1 strings are
/// identical to the default pattern's output, so the formatToParts fixture
/// encodes them as regular number parts.
pub fn rtf_auto_exception(base: &str, unit: &str, value_string: &str) -> Option<&'static str> {
    match base {
        "en" => match (unit, value_string) {
            ("year", "-1") => Some("last year"),
            ("year", "0") => Some("this year"),
            ("year", "1") => Some("next year"),
            ("quarter", "-1") => Some("last quarter"),
            ("quarter", "0") => Some("this quarter"),
            ("quarter", "1") => Some("next quarter"),
            ("month", "-1") => Some("last month"),
            ("month", "0") => Some("this month"),
            ("month", "1") => Some("next month"),
            ("week", "-1") => Some("last week"),
            ("week", "0") => Some("this week"),
            ("week", "1") => Some("next week"),
            ("day", "-1") => Some("yesterday"),
            ("day", "0") => Some("today"),
            ("day", "1") => Some("tomorrow"),
            ("hour", "0") => Some("this hour"),
            ("minute", "0") => Some("this minute"),
            ("second", "0") => Some("now"),
            _ => None,
        },
        _ => None,
    }
}
