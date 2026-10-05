//! CLDR-derived data for the `Intl.Locale` information getters (ECMA-402
//! `Intl.Locale-info`): `getCalendars`, `getCollations`, `getHourCycles`,
//! `getNumberingSystems`, `getTimeZones`, `getTextInfo` and `getWeekInfo`.
//!
//! The tables are the CLDR 48 supplemental data the fixtures exercise
//! (`weekData`, `calendarPreferenceData`, `timeData`), restricted to the
//! calendars and numbering systems the engine supports, plus the IANA
//! `zone.tab` region mapping for the common regions. Values follow the spec
//! priority order via [`region_preference`].

use crate::builtins::intl::bcp47;
use crate::builtins::intl::number_data::NUMBER_FORMAT_LOCALES;
use crate::builtins::intl::number_format;

/// RegionPreference (ECMA-402): the region a data lookup uses and the `rg`
/// region override. The region falls back from the region subtag to the `sd`
/// subdivision, to the Add-Likely-Subtags region, to "001".
pub fn region_preference(locale: &str) -> (String, Option<String>) {
    let region = match bcp47::region(locale) {
        Some(region) => region,
        None => match canonical_unicode_subdivision(locale, "sd") {
            Some(region) => region,
            None => {
                let maximal =
                    bcp47::add_likely_subtags(locale).unwrap_or_else(|_| locale.to_string());
                let maximal = bcp47::canonicalize(&maximal).unwrap_or(maximal);
                bcp47::region(&maximal).unwrap_or_else(|| "001".to_string())
            }
        },
    };
    let override_region = canonical_unicode_subdivision(locale, "rg");
    (region, override_region)
}

/// CanonicalUnicodeSubdivision (ECMA-402): the region named by an `sd`/`rg`
/// subdivision keyword value (region prefix), or `None`.
fn canonical_unicode_subdivision(locale: &str, key: &str) -> Option<String> {
    let value = bcp47::unicode_extension_value(locale, key)?;
    let bytes = value.as_bytes();
    let region_len = if bytes.len() >= 3 && bytes[..3].iter().all(u8::is_ascii_digit) {
        3
    } else if bytes.len() >= 2 && bytes[..2].iter().all(u8::is_ascii_alphabetic) {
        2
    } else {
        return None;
    };
    let suffix = &value[region_len..];
    if suffix.len() > 4 || !suffix.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    let region_locale = bcp47::canonicalize(&format!("und-{}", &value[..region_len])).ok()?;
    bcp47::region(&region_locale)
}

/// The data regions a lookup iterates: the `rg` override first (when present),
/// then the resolved region.
fn lookup_regions(locale: &str) -> Vec<String> {
    let (region, override_region) = region_preference(locale);
    match override_region {
        Some(override_region) => vec![override_region, region],
        None => vec![region],
    }
}

// Calendar preference (CLDR `calendarPreferenceData`), filtered to the
// canonical calendars the engine supports and ordered by descending
// preference.
fn calendars_for_key(key: &str) -> Option<&'static [&'static str]> {
    Some(match key {
        "AF" | "IR" => &["persian", "gregory"],
        "TH" => &["buddhist", "gregory"],
        "CN" | "CX" | "HK" | "MO" | "SG" => &["gregory", "chinese"],
        "ET" => &["gregory", "ethiopic"],
        "IL" => &["gregory", "hebrew"],
        "IN" => &["gregory", "indian"],
        "JP" => &["gregory", "japanese"],
        "KR" => &["gregory", "dangi"],
        "TW" => &["gregory", "roc", "chinese"],
        "EG" => &["gregory", "coptic"],
        "AE" | "BH" | "KW" | "QA" | "SA" => &["gregory", "islamic-umalqura"],
        "001" => &["gregory"],
        _ => return None,
    })
}

/// CalendarsOfLocale (ECMA-402): the calendar preference list for a locale.
pub fn calendars(locale: &str) -> Vec<&'static str> {
    let language = bcp47::language(locale);
    for region in lookup_regions(locale) {
        let language_region = format!("{language}-{region}");
        if let Some(list) = calendars_for_key(&language_region) {
            return list.to_vec();
        }
        if let Some(list) = calendars_for_key(&region) {
            return list.to_vec();
        }
    }
    vec!["gregory"]
}

/// CollationsOfLocale (ECMA-402): the available collations for a locale, or
/// the root `« emoji, eor »` when the locale matches no collator data.
pub fn collations(locale: &str) -> Vec<&'static str> {
    let base = bcp47::base_name(locale);
    match number_format::best_fit(NUMBER_FORMAT_LOCALES, &base) {
        Some(found) => collations_for_language(&bcp47::language(&found)).to_vec(),
        None => vec!["emoji", "eor"],
    }
}

/// The per-language collation identifiers (minus Standard/Search), sorted in
/// lexicographic code unit order.
fn collations_for_language(language: &str) -> &'static [&'static str] {
    match language {
        "ar" => &["compat", "emoji", "eor"],
        "de" => &["emoji", "eor", "phonebk"],
        "ja" => &["emoji", "eor", "unihan"],
        "sv" => &["emoji", "eor", "reformed"],
        "zh" => &[
            "big5han", "emoji", "eor", "gb2312", "pinyin", "stroke", "unihan", "zhuyin",
        ],
        _ => &["emoji", "eor"],
    }
}

/// HourCyclesOfLocale (ECMA-402): the hour cycle preference list for a locale.
pub fn hour_cycles(locale: &str) -> Vec<&'static str> {
    let language = bcp47::language(locale);
    for region in lookup_regions(locale) {
        let language_region = format!("{language}-{region}");
        if let Some(list) = hour_cycles_for_key(&language_region) {
            return list.to_vec();
        }
        if let Some(list) = hour_cycles_for_key(&region) {
            return list.to_vec();
        }
    }
    vec!["h23"]
}

/// The CLDR `timeData` `_allowed` set mapped to hour cycle identifiers, in
/// descending preference order (duplicates collapsed).
fn hour_cycles_for_key(key: &str) -> Option<&'static [&'static str]> {
    Some(match key {
        "JP" => &["h23", "h11", "h12"],
        "AX" | "BQ" | "CP" | "CZ" | "DK" | "FI" | "ID" | "IS" | "ML" | "NE" | "RU" | "SE"
        | "SJ" | "SK" => &["h23"],
        "001" | "BI" | "BY" | "FO" | "GL" | "HU" | "MG" | "MT" | "MU" | "MV" | "NO" | "PL"
        | "RW" | "TH" | "TJ" | "TM" | "VN" | "ZW" | "ca-ES" | "CF" | "CM" | "fr-CA" | "gl-ES"
        | "it-CH" | "it-IT" | "LU" | "NP" | "PF" | "SC" | "SM" | "SN" | "TF" | "VA" | "AC"
        | "AI" | "BW" | "BZ" | "CC" | "CK" | "CX" | "DG" | "en-IL" | "FK" | "GB" | "GG" | "GI"
        | "GS" | "IE" | "IM" | "IO" | "JE" | "LT" | "MK" | "MN" | "MS" | "NF" | "NG" | "NR"
        | "NU" | "PN" | "SH" | "SX" | "TA" | "ZA" | "af-ZA" | "EA" | "es-BR" | "es-ES"
        | "es-GQ" | "IC" | "KG" | "KM" | "LK" | "MA" | "AD" | "AM" | "AO" | "AT" | "AW" | "BE"
        | "BF" | "BJ" | "BL" | "BR" | "CG" | "CI" | "CV" | "CW" | "DE" | "EE" | "FR" | "GA"
        | "GF" | "GN" | "GP" | "GW" | "HR" | "IL" | "IT" | "ku-SY" | "KZ" | "MC" | "MD" | "MF"
        | "MQ" | "MZ" | "NC" | "NL" | "PM" | "PT" | "RE" | "RO" | "SI" | "SR" | "ST" | "TG"
        | "TR" | "WF" | "YT" | "AZ" | "BA" | "BG" | "CH" | "GE" | "LI" | "ME" | "RS" | "UA"
        | "UZ" | "XK" | "ES" | "GQ" | "AF" | "LA" | "CN" | "LV" | "TL" | "zu-ZA" => &["h23", "h12"],
        "AS" | "BT" | "DJ" | "ER" | "GH" | "IN" | "LS" | "PG" | "PW" | "SO" | "TO" | "VU"
        | "WS" | "BD" | "PK" | "AG" | "AU" | "BB" | "BM" | "BS" | "CA" | "DM" | "en-001"
        | "en-HK" | "en-MY" | "FJ" | "FM" | "GD" | "GM" | "GU" | "GY" | "JM" | "KI" | "KN"
        | "KY" | "LC" | "LR" | "MH" | "MP" | "MW" | "NZ" | "SB" | "SG" | "SL" | "SS" | "SZ"
        | "TC" | "TT" | "UM" | "US" | "VC" | "VG" | "VI" | "ZM" | "AE" | "ar-001" | "BH" | "DZ"
        | "EG" | "EH" | "HK" | "IQ" | "JO" | "KW" | "LB" | "LY" | "MO" | "MR" | "OM" | "PH"
        | "PS" | "QA" | "SA" | "SD" | "SY" | "TN" | "YE" | "CD" | "IR" | "hi-IN" | "kn-IN"
        | "ml-IN" | "te-IN" | "KH" | "ta-IN" | "BN" | "MY" | "ET" | "gu-IN" | "mr-IN" | "pa-IN"
        | "TW" | "KE" | "MM" | "TZ" | "UG" | "AL" | "TD" | "CY" | "GR" | "419" | "AR" | "BO"
        | "CL" | "CO" | "CR" | "CU" | "DO" | "EC" | "GT" | "HN" | "KP" | "KR" | "MX" | "NA"
        | "NI" | "PA" | "PE" | "PR" | "PY" | "SV" | "UY" | "VE" => &["h12", "h23"],
        _ => return None,
    })
}

/// NumberingSystemsOfLocale (ECMA-402): the locale's default numbering system,
/// or `latn` when the locale matches no number-format data.
pub fn numbering_systems(locale: &str) -> Vec<String> {
    let base = bcp47::base_name(locale);
    match number_format::best_fit(NUMBER_FORMAT_LOCALES, &base) {
        Some(found) => vec![number_format::default_numbering_system(&found).to_string()],
        None => vec!["latn".to_string()],
    }
}

/// TimeZonesOfLocale (ECMA-402): the canonical zones in common use in the
/// locale's region, sorted, or `None` when the locale has no region subtag.
pub fn time_zones(locale: &str) -> Option<Vec<&'static str>> {
    let region = bcp47::region(locale)?;
    let mut zones = zones_for_region(&region).to_vec();
    zones.sort_unstable();
    Some(zones)
}

/// The IANA zones whose principal location is in the region (a curated
/// `zone.tab` subset covering the common regions).
fn zones_for_region(region: &str) -> &'static [&'static str] {
    match region {
        "US" => &[
            "America/Adak",
            "America/Anchorage",
            "America/Boise",
            "America/Chicago",
            "America/Denver",
            "America/Detroit",
            "America/Indiana/Indianapolis",
            "America/Indiana/Knox",
            "America/Indiana/Marengo",
            "America/Indiana/Petersburg",
            "America/Indiana/Tell_City",
            "America/Indiana/Vevay",
            "America/Indiana/Vincennes",
            "America/Indiana/Winamac",
            "America/Juneau",
            "America/Kentucky/Louisville",
            "America/Kentucky/Monticello",
            "America/Los_Angeles",
            "America/Menominee",
            "America/Metlakatla",
            "America/New_York",
            "America/Nome",
            "America/North_Dakota/Beulah",
            "America/North_Dakota/Center",
            "America/North_Dakota/New_Salem",
            "America/Phoenix",
            "America/Sitka",
            "America/Yakutat",
            "Pacific/Honolulu",
        ],
        "CA" => &[
            "America/Dawson",
            "America/Dawson_Creek",
            "America/Edmonton",
            "America/Fort_Nelson",
            "America/Glace_Bay",
            "America/Goose_Bay",
            "America/Halifax",
            "America/Inuvik",
            "America/Iqaluit",
            "America/Moncton",
            "America/Rankin_Inlet",
            "America/Regina",
            "America/Resolute",
            "America/St_Johns",
            "America/Swift_Current",
            "America/Toronto",
            "America/Vancouver",
            "America/Whitehorse",
            "America/Winnipeg",
        ],
        "BR" => &[
            "America/Araguaina",
            "America/Bahia",
            "America/Belem",
            "America/Boa_Vista",
            "America/Campo_Grande",
            "America/Cuiaba",
            "America/Eirunepe",
            "America/Fortaleza",
            "America/Maceio",
            "America/Manaus",
            "America/Noronha",
            "America/Porto_Velho",
            "America/Recife",
            "America/Rio_Branco",
            "America/Santarem",
            "America/Sao_Paulo",
        ],
        "AU" => &[
            "Antarctica/Macquarie",
            "Australia/Adelaide",
            "Australia/Brisbane",
            "Australia/Broken_Hill",
            "Australia/Darwin",
            "Australia/Eucla",
            "Australia/Hobart",
            "Australia/Lindeman",
            "Australia/Lord_Howe",
            "Australia/Melbourne",
            "Australia/Perth",
            "Australia/Sydney",
        ],
        "DE" => &["Europe/Berlin", "Europe/Zurich"],
        "CN" => &["Asia/Shanghai", "Asia/Urumqi"],
        "NZ" => &["Pacific/Auckland", "Pacific/Chatham"],
        "GB" => &["Europe/London"],
        "FR" => &["Europe/Paris"],
        "JP" => &["Asia/Tokyo"],
        "IN" => &["Asia/Kolkata"],
        "IR" => &["Asia/Tehran"],
        _ => &[],
    }
}

/// TextDirectionOfLocale (ECMA-402): "rtl"/"ltr" for the locale's script
/// (resolved through Add-Likely-Subtags), or `None` when it cannot be
/// determined.
pub fn text_direction(locale: &str) -> Option<&'static str> {
    let script = match bcp47::script(locale) {
        Some(script) => script,
        None => {
            let maximal = bcp47::add_likely_subtags(locale).ok()?;
            bcp47::script(&maximal)?
        }
    };
    Some(if is_rtl_script(&script) { "rtl" } else { "ltr" })
}

/// The scripts whose default inline progression is right-to-left (CLDR script
/// metadata).
fn is_rtl_script(script: &str) -> bool {
    matches!(
        script,
        "Adlm"
            | "Arab"
            | "Armi"
            | "Avst"
            | "Chrs"
            | "Cprt"
            | "Elym"
            | "Hatr"
            | "Hebr"
            | "Khar"
            | "Lydi"
            | "Mand"
            | "Mani"
            | "Mend"
            | "Merc"
            | "Mero"
            | "Narb"
            | "Nbat"
            | "Nkoo"
            | "Ougr"
            | "Palm"
            | "Phli"
            | "Phlp"
            | "Phnx"
            | "Prti"
            | "Rohg"
            | "Samr"
            | "Sarb"
            | "Sogd"
            | "Sogo"
            | "Syrc"
            | "Thaa"
            | "Yezi"
    )
}

/// WeekInfoOfLocale (ECMA-402): `(firstDay, weekend)` as ISO day numbers
/// (Monday = 1 … Sunday = 7), with the `fw` override applied.
pub fn week_info(locale: &str, first_day_of_week: Option<&str>) -> (u8, Vec<u8>) {
    let (region, override_region) = region_preference(locale);
    let lookup_region = match override_region {
        Some(override_region) if week_data(&override_region).is_some() => override_region,
        _ if week_data(&region).is_some() => region,
        _ => "001".to_string(),
    };
    let (mut first_day, weekend) = week_data(&lookup_region).unwrap_or((1, &[6, 7]));
    if let Some(day) = first_day_of_week.and_then(weekday_uvalue_to_number) {
        first_day = day;
    }
    (first_day, weekend.to_vec())
}

/// FirstDayOfWeekToNumber: the `fw` keyword value as an ISO day number.
fn weekday_uvalue_to_number(value: &str) -> Option<u8> {
    Some(match value {
        "mon" => 1,
        "tue" => 2,
        "wed" => 3,
        "thu" => 4,
        "fri" => 5,
        "sat" => 6,
        "sun" => 7,
        _ => return None,
    })
}

/// The CLDR `weekData` (first day + weekend) for a region, or `None` when the
/// region has no explicit entry.
fn week_data(region: &str) -> Option<(u8, &'static [u8])> {
    first_day(region).map(|first_day| (first_day, weekend(region)))
}

fn first_day(region: &str) -> Option<u8> {
    Some(match region {
        "AF" | "BH" | "DJ" | "DZ" | "EG" | "IQ" | "IR" | "JO" | "KW" | "LY" | "OM" | "QA"
        | "SD" | "SY" => 6,
        "MV" => 5,
        "AG" | "AS" | "BD" | "BR" | "BS" | "BT" | "BW" | "BZ" | "CA" | "CO" | "DM" | "DO"
        | "ET" | "GT" | "GU" | "HK" | "HN" | "ID" | "IL" | "IN" | "JM" | "JP" | "KE" | "KH"
        | "KR" | "LA" | "MH" | "MM" | "MO" | "MT" | "MX" | "MZ" | "NI" | "NP" | "PA" | "PE"
        | "PH" | "PK" | "PR" | "PT" | "PY" | "SA" | "SG" | "SV" | "TH" | "TT" | "TW" | "UM"
        | "US" | "VE" | "VI" | "WS" | "YE" | "ZA" | "ZW" => 7,
        "001" | "AD" | "AE" | "AI" | "AL" | "AM" | "AN" | "AR" | "AT" | "AU" | "AX" | "AZ"
        | "BA" | "BE" | "BG" | "BM" | "BN" | "BY" | "CH" | "CL" | "CM" | "CN" | "CR" | "CY"
        | "CZ" | "DE" | "DK" | "EC" | "EE" | "ES" | "FI" | "FJ" | "FO" | "FR" | "GB" | "GE"
        | "GF" | "GP" | "GR" | "HR" | "HU" | "IE" | "IS" | "IT" | "KG" | "KZ" | "LB" | "LI"
        | "LK" | "LT" | "LU" | "LV" | "MC" | "MD" | "ME" | "MK" | "MN" | "MQ" | "MY" | "NL"
        | "NO" | "NZ" | "PL" | "RE" | "RO" | "RS" | "RU" | "SE" | "SI" | "SK" | "SM" | "TJ"
        | "TM" | "TR" | "UA" | "UY" | "UZ" | "VA" | "VN" | "XK" => 1,
        _ => return None,
    })
}

fn weekend(region: &str) -> &'static [u8] {
    match region {
        "AF" => &[4, 5],
        "BH" | "DZ" | "EG" | "IL" | "IQ" | "JO" | "KW" | "LY" | "OM" | "QA" | "SA" | "SD"
        | "SY" | "YE" => &[5, 6],
        "IR" => &[5],
        "IN" | "UG" => &[7],
        _ => &[6, 7],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn week_data_regions_differ_for_priority_levels() {
        assert_ne!(week_info("fa-AF", None), week_info("fa-JP", None));
        assert_ne!(week_info("fa-JP", None), week_info("fa-IN", None));
        assert_ne!(week_info("fa-IN", None), week_info("fa-IR", None));
        assert_ne!(week_info("fa-IR", None), week_info("eo-001", None));
    }

    #[test]
    fn first_day_override_wins() {
        assert_eq!(week_info("en", Some("fri")).0, 5);
        assert_eq!(week_info("en-u-fw-sun", None).0, 7);
    }

    #[test]
    fn region_preference_follows_the_signal_order() {
        assert_eq!(region_preference("fa-JP-u-sd-inka-rg-thzzzz").0, "JP");
        assert_eq!(
            region_preference("fa-JP-u-sd-inka-rg-thzzzz").1.as_deref(),
            Some("TH")
        );
        assert_eq!(region_preference("fa-u-sd-inka").0, "IN");
        assert_eq!(region_preference("fa").0, "IR");
        assert_eq!(region_preference("eo").0, "001");
    }

    #[test]
    fn collations_fall_back_to_root_for_unmatched() {
        assert_eq!(collations("und"), vec!["emoji", "eor"]);
        assert_eq!(collations("qfz"), vec!["emoji", "eor"]);
        assert!(!collations("de").is_empty());
    }

    #[test]
    fn time_zones_require_a_region() {
        assert_eq!(time_zones("en"), None);
        let us = time_zones("en-US").expect("US zones");
        assert!(!us.is_empty());
        let mut sorted = us.clone();
        sorted.sort_unstable();
        assert_eq!(us, sorted);
    }

    #[test]
    fn text_direction_from_script() {
        assert_eq!(text_direction("en"), Some("ltr"));
        assert_eq!(text_direction("ar"), Some("rtl"));
    }
}
