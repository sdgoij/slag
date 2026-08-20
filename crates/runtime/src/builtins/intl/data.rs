//! Corpus-derived CLDR data for the Intl.Locale machinery (plan Cut 1).
//!
//! The pinned test262 fixtures assert exact canonicalization and
//! maximize/minimize outputs (e.g. `constructor-non-iana-canon.js` pins the
//! language aliases, `likely-subtags.js` pins the CLDR 44 likely-subtags
//! behavior). These tables are the exact entries the corpus exercises —
//! the fixtures ARE the data spec (the same pattern as
//! `crates/unicode/src/derived_regexp_tables.rs`).

/// CLDR languageAlias entries the corpus pins (supplementalMetadata.xml):
/// `(from_type, to_replacement)`. The `from` type uses the underscore-joined
/// form (`language[_variant]` or `language_region`); the `to` replacement
/// may carry a script/region that is added only when the tag lacks the
/// corresponding subtag (TR35 Annex C step 3a).
pub const LANGUAGE_ALIASES: &[(&str, &str)] = &[
    // language-only replacements
    ("mo", "ro"),
    ("aar", "aa"),
    ("heb", "he"),
    ("ces", "cs"),
    ("cmn", "zh"),
    ("ji", "yi"),
    ("in", "id"),
    ("iw", "he"),
    // legacy replacements that add a script/region unless already present
    ("sh", "sr-Latn"),
    ("cnr", "sr-ME"),
    // language + region types
    ("sgn-GR", "gss"),
    // regular grandfathered tags (the variant subtag is consumed)
    ("art-lojban", "jbo"),
    ("cel-gaulish", "xtg"),
    ("zh-guoyu", "zh"),
    ("zh-hakka", "hak"),
    ("zh-xiang", "hsn"),
];

/// CLDR variantAlias entries with a language replacement: `(from_language,
/// from_variant, to_language)`. The variant is dropped and the language
/// replaced (hy-arevela → hy, hy-arevmda → hyw).
pub const VARIANT_ALIASES: &[(&str, &str, &str)] =
    &[("hy", "arevela", "hy"), ("hy", "arevmda", "hyw")];

/// CLDR variantAlias entries with a variant replacement: `(from_variant,
/// to_variant)` (ja-Latn-hepburn-heploc → ja-Latn-alalc97).
pub const VARIANT_SUBTAG_ALIASES: &[(&str, &str)] = &[("heploc", "alalc97")];

/// CLDR territoryAlias entries the corpus pins: `(from_region,
/// replacement_list)`. For a multi-region replacement the choice depends on
/// the likely subtags of the language id (the "az-NT" → "az-SA" case); the
/// first entry is the default.
pub const TERRITORY_ALIASES: &[(&str, &[&str])] = &[
    (
        "SU",
        &[
            "RU", "AM", "AZ", "BY", "EE", "GE", "KZ", "KG", "LV", "LT", "MD", "TJ", "TM", "UA",
            "UZ",
        ],
    ),
    (
        "810",
        &[
            "RU", "AM", "AZ", "BY", "EE", "GE", "KZ", "KG", "LV", "LT", "MD", "TJ", "TM", "UA",
            "UZ",
        ],
    ),
    ("CS", &["RS", "ME"]),
    ("NT", &["SA", "IQ"]),
    ("DD", &["DE"]),
    ("554", &["NZ"]),
];

/// CanonicalizeUValue data (ECMA-402 §9.2.2): `(key, alias, canonical)`.
/// Applied to u-extension keyword values and the Locale constructor's option
/// overrides.
pub const UVALUE_ALIASES: &[(&str, &str, &str)] = &[
    ("ca", "ethiopic-amete-alem", "ethioaa"),
    ("ca", "islamicc", "islamic-civil"),
    ("ca", "islamic", "islamic-civil"),
    ("ks", "primary", "level1"),
    ("ks", "tertiary", "level3"),
    ("ms", "imperial", "uksystem"),
    ("rg", "no23", "no50"),
    ("rg", "cn11", "cnbj"),
    ("rg", "cz10a", "cz110"),
    ("rg", "fra", "frges"),
    ("rg", "frg", "frges"),
    ("rg", "lud", "lucl"),
    ("sd", "no23", "no50"),
    ("sd", "cn11", "cnbj"),
    ("sd", "cz10a", "cz110"),
    ("sd", "fra", "frges"),
    ("sd", "frg", "frges"),
    ("sd", "lud", "lucl"),
    ("tz", "cnckg", "cnsha"),
    ("tz", "eire", "iedub"),
    ("tz", "est", "papty"),
    ("tz", "gmt0", "gmt"),
    ("tz", "uct", "utc"),
    ("tz", "zulu", "utc"),
    // "yes" is an alias of "true" for these keys; "true" types are then
    // removed (und-u-kb-yes → und-u-kb).
    ("kb", "yes", "true"),
    ("kc", "yes", "true"),
    ("kh", "yes", "true"),
    ("kk", "yes", "true"),
    ("kn", "yes", "true"),
];

/// tfield value aliases for the `t` extension (key `m0`: transform names):
/// `(tkey, alias, canonical)`. "true" tvalues are NOT removed (a UTS 35
/// spec bug the corpus pins in `transformed-ext-canonical.js`).
pub const TFIELD_ALIASES: &[(&str, &str, &str)] = &[("m0", "names", "prprname")];

/// The CLDR likelySubtags entries the corpus exercises, in the CLDR `from`
/// key format (`language`, `language_script`, `language_region`,
/// `und_script`, `und_region`, `und_script_region`), mapping to the
/// maximal `language_script_region` form.
pub const LIKELY_SUBTAGS: &[(&str, &str)] = &[
    // language alone
    ("en", "en-Latn-US"),
    ("de", "de-Latn-DE"),
    ("th", "th-Thai-TH"),
    ("es", "es-Latn-ES"),
    ("es-419", "es-Latn-419"),
    ("ru", "ru-Cyrl-RU"),
    ("hi", "hi-Deva-IN"),
    ("uz", "uz-Latn-UZ"),
    ("ro", "ro-Latn-RO"),
    ("aa", "aa-Latn-ET"),
    ("he", "he-Hebr-IL"),
    ("cs", "cs-Latn-CZ"),
    ("hy", "hy-Armn-AM"),
    ("hyw", "hyw-Armn-AM"),
    ("aae", "aae-Latn-IT"),
    ("pap", "pap-Latn-CW"),
    ("ar", "ar-Arab-EG"),
    ("zh", "zh-Hans-CN"),
    ("bg", "bg-Cyrl-BG"),
    ("it", "it-Latn-IT"),
    // the grandfathered maximals (likely-subtags-grandfathered.js)
    ("jbo", "jbo-Latn-001"),
    ("hak", "hak-Hans-CN"),
    ("hsn", "hsn-Hans-CN"),
    // the ICU-13786 minimal forms (removing-likely-subtags-*.js)
    ("aae-Thai-CO", "aae-Thai-CO"),
    ("aae-Thai", "aae-Thai-IT"),
    ("aae-CO", "aae-Latn-CO"),
    ("aae-Thai-IT", "aae-Thai-IT"),
    ("aae-Latn-CO", "aae-Latn-CO"),
    // language + script
    ("en-Shaw", "en-Shaw-GB"),
    ("en-Arab", "en-Arab-US"),
    ("en-Latn", "en-Latn-US"),
    ("th-Thai", "th-Thai-TH"),
    ("zh-Hant", "zh-Hant-TW"),
    ("zh-Hani", "zh-Hani-CN"),
    ("zh-Hans", "zh-Hans-CN"),
    ("ru-Cyrl", "ru-Cyrl-RU"),
    ("bg-Cyrl", "bg-Cyrl-BG"),
    ("it-Kana-CA", "it-Kana-CA"),
    ("hy-Armn", "hy-Armn-AM"),
    ("hyw-Armn", "hyw-Armn-AM"),
    ("he-Hebr", "he-Hebr-IL"),
    ("aa-Latn", "aa-Latn-ET"),
    ("cs-Latn", "cs-Latn-CZ"),
    ("hi-Deva", "hi-Deva-IN"),
    ("uz-Latn", "uz-Latn-UZ"),
    ("ro-Latn", "ro-Latn-RO"),
    ("de-Latn", "de-Latn-DE"),
    ("es-Latn", "es-Latn-ES"),
    ("aae-Latn", "aae-Latn-IT"),
    ("pap-Latn", "pap-Latn-CW"),
    ("ar-Arab", "ar-Arab-EG"),
    // language + region
    ("en-US", "en-Latn-US"),
    ("en-GB", "en-Latn-GB"),
    ("en-FR", "en-Latn-FR"),
    ("de-AT", "de-Latn-AT"),
    ("th-TH", "th-Thai-TH"),
    ("es-ES", "es-Latn-ES"),
    ("ru-RU", "ru-Cyrl-RU"),
    ("bg-RO", "bg-Cyrl-RO"),
    ("bg-Cyrl-RO", "bg-Cyrl-RO"),
    ("bg-BG", "bg-Cyrl-BG"),
    ("zh-TW", "zh-Hant-TW"),
    ("zh-CN", "zh-Hans-CN"),
    ("it-IT", "it-Latn-IT"),
    ("hy-AM", "hy-Armn-AM"),
    ("hyw-AM", "hyw-Armn-AM"),
    ("he-IL", "he-Hebr-IL"),
    ("aa-ET", "aa-Latn-ET"),
    ("cs-CZ", "cs-Latn-CZ"),
    ("hi-IN", "hi-Deva-IN"),
    ("uz-UZ", "uz-Latn-UZ"),
    ("ro-RO", "ro-Latn-RO"),
    ("de-DE", "de-Latn-DE"),
    ("ar-EG", "ar-Arab-EG"),
    ("aae-IT", "aae-Latn-IT"),
    ("pap-CW", "pap-Latn-CW"),
    // The exact maximal triples the minimize fixtures pin (the lookup for
    // a tag that is already maximal must return it unchanged).
    ("en-Latn-US", "en-Latn-US"),
    ("en-Latn-GB", "en-Latn-GB"),
    ("en-Latn-FR", "en-Latn-FR"),
    ("en-Shaw-GB", "en-Shaw-GB"),
    ("en-Arab-US", "en-Arab-US"),
    ("th-Thai-TH", "th-Thai-TH"),
    ("es-Latn-419", "es-Latn-419"),
    ("de-Latn-AT", "de-Latn-AT"),
    ("ru-Cyrl-RU", "ru-Cyrl-RU"),
    ("hy-Armn-AM", "hy-Armn-AM"),
    ("hyw-Armn-AM", "hyw-Armn-AM"),
    ("aa-Latn-ET", "aa-Latn-ET"),
    ("he-Hebr-IL", "he-Hebr-IL"),
    ("cs-Latn-CZ", "cs-Latn-CZ"),
    ("hi-Deva-IN", "hi-Deva-IN"),
    ("uz-Latn-UZ", "uz-Latn-UZ"),
    ("ro-Latn-RO", "ro-Latn-RO"),
    ("de-Latn-DE", "de-Latn-DE"),
    ("es-Latn-ES", "es-Latn-ES"),
    ("ar-Arab-EG", "ar-Arab-EG"),
    ("aae-Latn-IT", "aae-Latn-IT"),
    ("pap-Latn-CW", "pap-Latn-CW"),
    // und + script / region (the multi-language defaults)
    ("und", "en-Latn-US"),
    ("und-Thai", "th-Thai-TH"),
    ("und-Cyrl", "bg-Cyrl-BG"),
    ("und-Cyrl-RO", "bg-Cyrl-RO"),
    ("und-Arab", "ar-Arab-EG"),
    ("und-Armn", "hy-Armn-AM"),
    ("und-419", "es-Latn-419"),
    ("und-150", "en-Latn-150"),
    ("und-AT", "de-Latn-AT"),
    ("und-AQ", "en-Latn-AQ"),
    ("en-AQ", "en-Latn-AQ"),
    ("en-Latn-AQ", "en-Latn-AQ"),
    ("en-150", "en-Latn-150"),
    ("en-Latn-150", "en-Latn-150"),
    ("und-CW", "pap-Latn-CW"),
    ("und-US", "en-Latn-US"),
    ("und-TH", "th-Thai-TH"),
    ("und-DE", "de-Latn-DE"),
    ("und-ES", "es-Latn-ES"),
    ("und-RU", "ru-Cyrl-RU"),
    ("und-GB", "en-Latn-GB"),
    ("und-FR", "en-Latn-FR"),
    ("und-IT", "it-Latn-IT"),
    ("und-CN", "zh-Hans-CN"),
    ("und-TW", "zh-Hant-TW"),
    ("und-RO", "ro-Latn-RO"),
    ("und-IN", "hi-Deva-IN"),
    ("und-IL", "he-Hebr-IL"),
    ("und-AM", "hy-Armn-AM"),
    // territory-alias selection (complex-region-subtag-replacement.js)
    ("sr", "sr-Cyrl-RS"),
    ("sr-Latn", "sr-Latn-RS"),
    ("sr-Cyrl", "sr-Cyrl-RS"),
    ("sr-ME", "sr-Latn-ME"),
    ("und-RS", "sr-Cyrl-RS"),
    ("und-ME", "sr-Latn-ME"),
    ("az", "az-Latn-AZ"),
];
