//! BCP 47 / UTS #35 Unicode locale identifier machinery (ECMA-402 §6.2,
//! §15): the grammar, canonicalization (with the corpus-pinned CLDR alias
//! tables), and the likely-subtags algorithms behind
//! `Intl.Locale.maximize`/`minimize`. All functions are pure string
//! transforms; the JsError results only carry RangeError for the invalid
//! tags the corpus throws on.

use crux::error::{ErrorKind, JsError};

use crate::builtins::intl::data;

/// One parsed Unicode locale identifier (UTS #35 §3.2 `unicode_locale_id`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocaleParts {
    pub language: String,
    pub script: Option<String>,
    pub region: Option<String>,
    /// Variant subtags, in source order (canonicalized later).
    pub variants: Vec<String>,
    /// Extension sequences (`u-...`, `t-...`, other singletons), each with
    /// its singleton first, in source order (sorted later).
    pub extensions: Vec<String>,
    /// Private-use subtags (after the `x`), in source order.
    pub privateuse: Vec<String>,
}

impl LocaleParts {
    /// The language-id portion: language[-script[-region[-variants]]].
    pub fn base_name(&self) -> String {
        let mut out = self.language.clone();
        if let Some(script) = &self.script {
            out.push('-');
            out.push_str(script);
        }
        if let Some(region) = &self.region {
            out.push('-');
            out.push_str(region);
        }
        for variant in &self.variants {
            out.push('-');
            out.push_str(variant);
        }
        out
    }

    /// The full canonical-form rendering: base name, sorted extensions, and
    /// the private use.
    pub fn render(&self) -> String {
        let mut out = self.base_name();
        for extension in &self.extensions {
            out.push('-');
            out.push_str(extension);
        }
        if !self.privateuse.is_empty() {
            out.push_str("-x");
            for subtag in &self.privateuse {
                out.push('-');
                out.push_str(subtag);
            }
        }
        out
    }
}

// ---- the grammar (UTS #35 §3.2) ----

fn is_alpha(c: u8) -> bool {
    c.is_ascii_alphabetic()
}

fn is_digit(c: u8) -> bool {
    c.is_ascii_digit()
}

fn is_alnum(c: u8) -> bool {
    c.is_ascii_alphanumeric()
}

fn all(s: &str, pred: impl Fn(u8) -> bool) -> bool {
    !s.is_empty() && s.bytes().all(pred)
}

/// `unicode_language_subtag`: alpha{2,3} | alpha{5,8}.
pub fn is_language_subtag(s: &str) -> bool {
    let len = s.len();
    (len == 2 || len == 3 || len == 5 || len == 6 || len == 7 || len == 8) && all(s, is_alpha)
}

/// `unicode_script_subtag`: alpha{4}.
fn is_script_subtag(s: &str) -> bool {
    s.len() == 4 && all(s, is_alpha)
}

/// `unicode_region_subtag`: alpha{2} | digit{3}.
fn is_region_subtag(s: &str) -> bool {
    (s.len() == 2 && all(s, is_alpha)) || (s.len() == 3 && all(s, is_digit))
}

/// `unicode_variant_subtag`: alphanum{5,8} | digit alphanum{3}.
fn is_variant_subtag(s: &str) -> bool {
    let bytes = s.as_bytes();
    if s.len() == 4 {
        return is_digit(bytes[0]) && all(&s[1..], is_alnum);
    }
    (5..=8).contains(&s.len()) && all(s, is_alnum)
}

/// `unicode_locale_extension_attribute`: alphanum{3,8}.
fn is_attribute(s: &str) -> bool {
    (3..=8).contains(&s.len()) && all(s, is_alnum)
}

/// `unicode_locale_extension_key`: alphanum{2} with no digit as the second
/// character (the corpus rejects `en-u-c0`/`en-u-00` but accepts `en-u-0c`).
pub fn is_key(s: &str) -> bool {
    let bytes = s.as_bytes();
    s.len() == 2 && is_alnum(bytes[0]) && is_alpha(bytes[1])
}

/// `unicode_locale_extension_type` token: alphanum{3,8}.
pub fn is_type_token(s: &str) -> bool {
    (3..=8).contains(&s.len()) && all(s, is_alnum)
}

/// `singleton`: alphanum — but `x` starts private use, handled separately.
fn is_singleton(s: &str) -> bool {
    s.len() == 1 && is_alnum(s.as_bytes()[0]) && !s.eq_ignore_ascii_case("x")
}

/// `transformed_extension_key`: alphanum{2} (no digit restriction — the
/// `m0`/`d0` tkeys are valid).
fn is_tkey(s: &str) -> bool {
    s.len() == 2 && all(s, is_alnum)
}

/// `transformed_extension_value` token: alphanum{3,8}.
fn is_tvalue(s: &str) -> bool {
    (3..=8).contains(&s.len()) && all(s, is_alnum)
}

fn is_privateuse_subtag(s: &str) -> bool {
    (1..=8).contains(&s.len()) && all(s, is_alnum)
}

/// Parse a Unicode locale identifier (UTS #35 §3.2). `None` when the tag
/// does not match the grammar.
pub fn parse_locale_id(tag: &str) -> Option<LocaleParts> {
    // Separators: the tag must not start or end with "-" or contain "--".
    if tag.starts_with('-') || tag.ends_with('-') || tag.contains("--") {
        return None;
    }
    let subtags: Vec<&str> = tag.split('-').filter(|s| !s.is_empty()).collect();
    if subtags.len() < 2 && tag.is_empty() {
        return None;
    }
    let mut index = 0;
    let language = *subtags.first()?;
    if !is_language_subtag(language) {
        return None;
    }
    index += 1;

    let mut script = None;
    let mut region = None;
    let mut variants: Vec<String> = Vec::new();

    if let Some(&next) = subtags.get(index)
        && is_script_subtag(next)
    {
        script = Some(next.to_string());
        index += 1;
    }
    if let Some(&next) = subtags.get(index)
        && is_region_subtag(next)
    {
        region = Some(next.to_string());
        index += 1;
    }
    while let Some(&next) = subtags.get(index) {
        if is_variant_subtag(next) {
            // BCP 47: a language id must not contain duplicate variants
            // (zh-hakka-hakka is rejected during validation).
            if variants.iter().any(|v| v.eq_ignore_ascii_case(next)) {
                return None;
            }
            variants.push(next.to_string());
            index += 1;
        } else {
            break;
        }
    }

    // Extensions and private use. An extension is a singleton followed by
    // its subtags; the `u` extension additionally parses attributes then
    // keywords so a 1-char subtag (a new singleton) ends it. Private use
    // (`x`) swallows every remaining subtag greedily.
    let mut extensions: Vec<String> = Vec::new();
    let mut privateuse: Vec<String> = Vec::new();
    let mut seen_singletons: Vec<&str> = Vec::new();
    while let Some(&singleton) = subtags.get(index) {
        if singleton.eq_ignore_ascii_case("x") {
            for &sub in &subtags[index + 1..] {
                if !is_privateuse_subtag(sub) {
                    return None;
                }
                privateuse.push(sub.to_string());
            }
            if privateuse.is_empty() {
                // Private use requires at least one subtag (`si-x` is not
                // well-formed).
                return None;
            }
            break;
        }
        if !is_singleton(singleton) {
            return None;
        }
        // UTS #35: each extension singleton appears at most once
        // (`da-u-ca-gregory-u-ca-buddhist` is not well-formed).
        if seen_singletons
            .iter()
            .any(|s| s.eq_ignore_ascii_case(singleton))
        {
            return None;
        }
        seen_singletons.push(singleton);
        index += 1;
        if singleton.eq_ignore_ascii_case("u") {
            let mut attributes = Vec::new();
            let mut keywords: Vec<(String, Option<String>)> = Vec::new();
            // Attributes first (alphanum{3,8}).
            while let Some(&sub) = subtags.get(index) {
                if !is_attribute(sub) {
                    break;
                }
                attributes.push(sub.to_string());
                index += 1;
            }
            // Then keywords: key (alphanum{2}) with an optional type of one
            // or more alphanum{3,8} tokens.
            while let Some(&sub) = subtags.get(index) {
                if !is_key(sub) {
                    break;
                }
                index += 1;
                let mut type_tokens: Vec<String> = Vec::new();
                while let Some(&token) = subtags.get(index) {
                    if !is_type_token(token) {
                        break;
                    }
                    type_tokens.push(token.to_string());
                    index += 1;
                }
                let value = if type_tokens.is_empty() {
                    None
                } else {
                    Some(type_tokens.join("-"))
                };
                keywords.push((sub.to_string(), value));
            }
            if attributes.is_empty() && keywords.is_empty() {
                // `u` with no attributes or keywords is not a well-formed
                // extension (`da-u` is invalid).
                return None;
            }
            let mut ext = String::from("u");
            for attribute in &attributes {
                ext.push('-');
                ext.push_str(attribute);
            }
            for (key, value) in &keywords {
                ext.push('-');
                ext.push_str(key);
                if let Some(value) = value {
                    ext.push('-');
                    ext.push_str(value);
                }
            }
            extensions.push(ext);
        } else if singleton.eq_ignore_ascii_case("t") {
            // transformed_extension: an optional tlang followed by tfields.
            // The tlang is parsed greedily (language[-script[-region
            // [-variants]]]) so a 2-char alpha subtag after the language is
            // its region (`en-t-en-ca`), while a 2-char alnum tkey that is
            // not a region (`d0`, `m0`) ends it.
            let mut tlang = None;
            if let Some(&next) = subtags.get(index)
                && is_language_subtag(next)
            {
                let mut t = LocaleParts {
                    language: next.to_string(),
                    script: None,
                    region: None,
                    variants: Vec::new(),
                    extensions: Vec::new(),
                    privateuse: Vec::new(),
                };
                let mut i = index + 1;
                if let Some(&sub) = subtags.get(i)
                    && is_script_subtag(sub)
                {
                    t.script = Some(sub.to_string());
                    i += 1;
                }
                if let Some(&sub) = subtags.get(i)
                    && is_region_subtag(sub)
                {
                    t.region = Some(sub.to_string());
                    i += 1;
                }
                while let Some(&sub) = subtags.get(i) {
                    if !is_variant_subtag(sub) {
                        break;
                    }
                    if t.variants.iter().any(|v| v.eq_ignore_ascii_case(sub)) {
                        return None;
                    }
                    t.variants.push(sub.to_string());
                    i += 1;
                }
                index = i;
                tlang = Some(t);
            }
            // tfields: tkey (alphanum{2}) with one or more tvalue
            // (alphanum{3,8}) tokens.
            let mut fields: Vec<(String, Vec<String>)> = Vec::new();
            while let Some(&sub) = subtags.get(index) {
                if !is_tkey(sub) {
                    break;
                }
                index += 1;
                let mut values = Vec::new();
                while let Some(&token) = subtags.get(index) {
                    if !is_tvalue(token) {
                        break;
                    }
                    values.push(token.to_string());
                    index += 1;
                }
                if values.is_empty() {
                    // A tkey must be followed by a tvalue.
                    return None;
                }
                fields.push((sub.to_string(), values));
            }
            if tlang.is_none() && fields.is_empty() {
                // `t` with neither a tlang nor a tfield is not well-formed.
                return None;
            }
            let mut ext = String::from("t");
            if let Some(t) = tlang {
                ext.push('-');
                ext.push_str(&t.base_name());
            }
            for (key, values) in &fields {
                ext.push('-');
                ext.push_str(key);
                for value in values {
                    ext.push('-');
                    ext.push_str(value);
                }
            }
            extensions.push(ext);
        } else {
            let mut ext = String::from(singleton);
            let mut count = 0;
            while let Some(&sub) = subtags.get(index) {
                // A 1-char alnum subtag is the next extension's singleton, and
                // `x` starts private use — either ends this extension.
                if sub == "x" || is_singleton(sub) {
                    break;
                }
                let len = sub.len();
                if !(2..=8).contains(&len) || !all(sub, is_alnum) {
                    return None;
                }
                ext.push('-');
                ext.push_str(sub);
                index += 1;
                count += 1;
            }
            if count == 0 {
                return None;
            }
            extensions.push(ext);
        }
    }

    Some(LocaleParts {
        language: language.to_string(),
        script,
        region,
        variants,
        extensions,
        privateuse,
    })
}

/// IsStructurallyValidLanguageTag (ECMA-402 §6.2.1): the tag matches the
/// `unicode_locale_id` grammar.
pub fn is_well_formed(tag: &str) -> bool {
    parse_locale_id(tag).is_some()
}

fn range_error(message: &str) -> JsError {
    JsError::new(ErrorKind::RangeError, message.into())
}

/// Apply the corpus-pinned CLDR alias tables to the language id (TR35
/// Annex C step 3): languageAlias (including the multi-subtag types),
/// variantAlias (both replacement kinds), and territoryAlias (with the
/// likely-subtags selection for multi-region replacements).
fn apply_aliases(parts: &mut LocaleParts) {
    // languageAlias: the type is matched against the language (longer types
    // — language+variant, language+region — first, so the grandfathered
    // forms win over plain language matches).
    for &(from, to) in data::LANGUAGE_ALIASES {
        let from_parts: Vec<&str> = from.split('-').collect();
        if !from_parts[0].eq_ignore_ascii_case(&parts.language) {
            continue;
        }
        let matched = match from_parts.as_slice() {
            [lang] => *lang,
            [lang, second] if is_region_subtag(second) => {
                if parts
                    .region
                    .as_deref()
                    .is_some_and(|r| r.eq_ignore_ascii_case(second))
                {
                    parts.region = None;
                    *lang
                } else {
                    continue;
                }
            }
            [lang, variant] => {
                if parts
                    .variants
                    .last()
                    .is_some_and(|v| v.eq_ignore_ascii_case(variant))
                {
                    parts.variants.pop();
                    *lang
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        let mut to_parts = to.split('-');
        parts.language = to_parts.next().unwrap_or("und").to_string();
        // Additional replacement subtags are added only when the tag lacks
        // the corresponding subtag (sh → sr-Latn keeps an existing script).
        for sub in to_parts {
            if is_script_subtag(sub) && parts.script.is_none() {
                parts.script = Some(sub.to_string());
            } else if is_region_subtag(sub) && parts.region.is_none() {
                parts.region = Some(sub.to_string());
            }
        }
        debug_assert!(!matched.is_empty());
        return;
    }
    // variantAlias with a language replacement (hy-arevela → hy).
    if parts.variants.len() == 1 {
        let variant = parts.variants[0].clone();
        for &(from_lang, from_variant, to_lang) in data::VARIANT_ALIASES {
            if parts.language.eq_ignore_ascii_case(from_lang)
                && variant.eq_ignore_ascii_case(from_variant)
            {
                parts.language = to_lang.to_string();
                parts.variants.clear();
                break;
            }
        }
    }
    // variantAlias with a variant replacement (heploc → alalc97): the
    // replacement replaces the whole variant sequence
    // (ja-Latn-hepburn-heploc → ja-Latn-alalc97).
    if let Some(last) = parts.variants.last() {
        for &(from_variant, to_variant) in data::VARIANT_SUBTAG_ALIASES {
            if last.eq_ignore_ascii_case(from_variant) {
                parts.variants.clear();
                parts.variants.push(to_variant.to_string());
                break;
            }
        }
    }
    // territoryAlias (SU → RU/AM/..., CS → RS/ME, ...).
    if let Some(region) = &parts.region {
        for &(from, replacements) in data::TERRITORY_ALIASES {
            if region.eq_ignore_ascii_case(from) {
                let chosen = select_region_replacement(parts, replacements);
                parts.region = Some(chosen.to_string());
                break;
            }
        }
    }
}

/// The replacement region for a territory alias with several choices: the
/// likely subtags of the language id (without the region) decide when their
/// region is among the choices; otherwise the default (first) is used — the
/// `az-NT` → `az-SA` case.
fn select_region_replacement(parts: &LocaleParts, replacements: &[&str]) -> String {
    let mut trial = parts.clone();
    trial.region = None;
    if let Ok(maxed) = add_likely_subtags(&trial.render())
        && let Some(region) = region(&maxed)
        && let Some(chosen) = replacements.iter().find(|r| **r == region)
    {
        return chosen.to_string();
    }
    replacements[0].to_string()
}

/// CanonicalizeUnicodeLocaleId (ECMA-402 §6.2.2): parse, apply the aliases,
/// case-fold, dedupe + sort variants, canonicalize the extensions (sorted
/// by singleton), lowercase private use.
pub fn canonicalize(tag: &str) -> Result<String, JsError> {
    let mut parts = parse_locale_id(tag).ok_or_else(|| range_error("Invalid language tag"))?;
    apply_aliases(&mut parts);

    parts.language = parts.language.to_ascii_lowercase();
    if let Some(script) = &mut parts.script {
        let lower = script.to_ascii_lowercase();
        *script = format!("{}{}", lower[..1].to_ascii_uppercase(), &lower[1..]);
    }
    if let Some(region) = &mut parts.region {
        *region = region.to_ascii_uppercase();
    }
    for variant in &mut parts.variants {
        *variant = variant.to_ascii_lowercase();
    }
    parts.variants.sort();
    parts.variants.dedup();

    for extension in &mut parts.extensions {
        *extension = canonicalize_extension(extension);
    }
    parts
        .extensions
        .sort_by(|a, b| a.as_bytes()[0].cmp(&b.as_bytes()[0]));

    for subtag in &mut parts.privateuse {
        *subtag = subtag.to_ascii_lowercase();
    }

    Ok(parts.render())
}

/// Canonicalize a single extension sequence. The `u` extension sorts and
/// dedupes its attributes, canonicalizes keyword values (CanonicalizeUValue),
/// drops keyword values of "true", and keeps only the first keyword per key.
/// The `t` extension canonicalizes its tlang and sorts its tfields. Other
/// extensions are lowercased.
fn canonicalize_extension(extension: &str) -> String {
    let singleton = &extension[..1];
    if singleton.eq_ignore_ascii_case("t") {
        return canonicalize_t_extension(extension);
    }
    if !singleton.eq_ignore_ascii_case("u") {
        return extension.to_ascii_lowercase();
    }
    let mut attributes = Vec::new();
    let mut keywords: Vec<(String, String)> = Vec::new();
    let mut seen_keys: Vec<String> = Vec::new();
    let subtags: Vec<&str> = extension[1..]
        .split('-')
        .filter(|s| !s.is_empty())
        .collect();
    for sub in &subtags {
        if is_attribute(sub) && seen_keys.is_empty() {
            attributes.push(sub.to_ascii_lowercase());
        } else if is_key(sub) {
            seen_keys.push(sub.to_ascii_lowercase());
            keywords.push((sub.to_ascii_lowercase(), String::new()));
        } else if !seen_keys.is_empty() && is_type_token(sub) {
            // A type token continues the previous keyword's value (types
            // are alphanum{3,8} *("-" alphanum{3,8})).
            let last = keywords.last_mut().expect("keyword present");
            if last.1.is_empty() {
                last.1 = sub.to_ascii_lowercase();
            } else {
                last.1.push('-');
                last.1.push_str(&sub.to_ascii_lowercase());
            }
        }
    }
    attributes.sort();
    attributes.dedup();
    let mut seen = Vec::new();
    let mut out = String::from("u");
    for attribute in &attributes {
        out.push('-');
        out.push_str(attribute);
    }
    keywords.sort_by(|a, b| a.0.cmp(&b.0));
    for (key, value) in keywords {
        if seen.contains(&key) {
            continue;
        }
        seen.push(key.clone());
        // CanonicalizeUValue, then drop the "true" types (und-u-kb-yes →
        // und-u-kb; kf-true → kf).
        let value = canonicalize_uvalue(&key, &value);
        out.push('-');
        out.push_str(&key);
        if !value.is_empty() && value != "true" {
            out.push('-');
            out.push_str(&value);
        }
    }
    out
}

/// CanonicalizeUValue (ECMA-402 §9.2.2): the canonical form of a u-extension
/// keyword value per the key's alias data; unchanged when not aliased.
pub fn canonicalize_uvalue(key: &str, value: &str) -> String {
    // CanonicalizeUValue (§9.2.2): the value is ASCII-lowercased first
    // (a `type`-nonterminal check already ran on the option), then the
    // alias table maps the deprecated spellings.
    let lower = value.to_ascii_lowercase();
    for &(k, from, to) in data::UVALUE_ALIASES {
        if k == key && lower == from {
            return to.to_string();
        }
    }
    lower
}

/// tfield value aliasing for the `t` extension (m0-names → m0-prprname).
fn canonicalize_tvalue(key: &str, value: &str) -> String {
    for &(k, from, to) in data::TFIELD_ALIASES {
        if k == key && value == from {
            return to.to_string();
        }
    }
    value.to_string()
}

/// Canonicalize a `t` extension: the tlang's language id (aliases, case,
/// variant sort) and the tfields sorted by key with tvalue aliases applied.
/// A tvalue of "true" is kept (the corpus pins the UTS 35 spec bug).
fn canonicalize_t_extension(extension: &str) -> String {
    let subtags: Vec<&str> = extension[1..]
        .split('-')
        .filter(|s| !s.is_empty())
        .collect();
    let mut index = 0;
    let mut out = String::from("t");
    // The tlang, when present: language[-script[-region[-variants]]], ending
    // at the first 2-char alnum tkey that is not a region.
    if subtags.first().is_some_and(|s| is_language_subtag(s)) {
        let mut end = subtags.len();
        for (i, &sub) in subtags.iter().enumerate().skip(1) {
            if is_tkey(sub) && !is_region_subtag(sub) {
                end = i;
                break;
            }
        }
        out.push('-');
        out.push_str(&canonicalize_tlang(&subtags[..end].join("-")));
        index = end;
    }
    // tfields sorted by key.
    let mut fields: Vec<(String, String)> = Vec::new();
    while index < subtags.len() {
        let key = subtags[index].to_ascii_lowercase();
        index += 1;
        let mut value = String::new();
        while index < subtags.len() {
            let token = subtags[index];
            if !is_tvalue(token) {
                break;
            }
            if !value.is_empty() {
                value.push('-');
            }
            value.push_str(&token.to_ascii_lowercase());
            index += 1;
        }
        value = canonicalize_tvalue(&key, &value);
        fields.push((key, value));
    }
    fields.sort_by(|a, b| a.0.cmp(&b.0));
    for (key, value) in fields {
        out.push('-');
        out.push_str(&key);
        out.push('-');
        out.push_str(&value);
    }
    out
}

/// Canonicalize the tlang portion of a `t` extension: the language-id
/// aliases, then ASCII-lowercase every subtag (the corpus and ICU keep the
/// tlang entirely lowercase), and sort the variants.
fn canonicalize_tlang(tlang: &str) -> String {
    let mut parts = parse_locale_id(tlang).unwrap_or_else(|| LocaleParts {
        language: tlang.to_string(),
        script: None,
        region: None,
        variants: Vec::new(),
        extensions: Vec::new(),
        privateuse: Vec::new(),
    });
    apply_aliases(&mut parts);
    parts.language = parts.language.to_ascii_lowercase();
    if let Some(script) = &mut parts.script {
        *script = script.to_ascii_lowercase();
    }
    if let Some(region) = &mut parts.region {
        *region = region.to_ascii_lowercase();
    }
    for variant in &mut parts.variants {
        *variant = variant.to_ascii_lowercase();
    }
    parts.variants.sort();
    parts.variants.dedup();
    parts.base_name()
}

/// Look up the likely-subtags table: `(from_language, from_script,
/// from_region)` with "Zzzz"/"ZZ" as the absent script/region wildcards.
/// Returns the maximal `(language, script, region)` triple.
fn likely_lookup(
    language: &str,
    script: Option<&str>,
    region: Option<&str>,
) -> Option<(&'static str, &'static str, &'static str)> {
    let script = script.unwrap_or("Zzzz");
    let region = region.unwrap_or("ZZ");
    for &(from, to) in data::LIKELY_SUBTAGS {
        let key: Vec<&str> = from.split('-').collect();
        let (from_lang, from_script, from_region) = match key.as_slice() {
            [lang] => (*lang, "Zzzz", "ZZ"),
            [lang, second] => {
                if is_script_subtag(second) {
                    (*lang, *second, "ZZ")
                } else {
                    (*lang, "Zzzz", *second)
                }
            }
            [lang, scr, reg] => (*lang, *scr, *reg),
            _ => continue,
        };
        if from_lang == language && from_script == script && from_region == region {
            let mut to_parts = to.split('-');
            let to_lang = to_parts.next().unwrap_or("und");
            let to_script = to_parts.next().unwrap_or("Latn");
            let to_region = to_parts.next().unwrap_or("US");
            return Some((to_lang, to_script, to_region));
        }
    }
    None
}

/// AddLikelySubtags (ECMA-402 §15.4.2 / UTS #35): return the tag with the
/// maximal (language, script, region) per the corpus-pinned table. The
/// variants, extensions, and private use are preserved.
pub fn add_likely_subtags(tag: &str) -> Result<String, JsError> {
    let mut parts = parse_locale_id(tag).ok_or_else(|| range_error("Invalid language tag"))?;
    apply_aliases(&mut parts);
    let (language, script, region) = (
        parts.language.as_str(),
        parts.script.as_deref(),
        parts.region.as_deref(),
    );

    // The und-based lookups apply only when the language is "und"; a tag
    // with no likely-subtags match at all returns unchanged (TR35: the
    // maximal form of an unmatched tag is the tag itself).
    let maximal = if language == "und" {
        likely_lookup("und", script, region)
            .or_else(|| {
                if script.is_some() {
                    likely_lookup("und", script, None)
                } else {
                    None
                }
            })
            .or_else(|| {
                if region.is_some() {
                    likely_lookup("und", None, region)
                } else {
                    None
                }
            })
            .or_else(|| likely_lookup("und", None, None))
    } else {
        likely_lookup(language, script, region)
            .or_else(|| {
                if script.is_some() {
                    likely_lookup(language, script, None)
                } else {
                    None
                }
            })
            .or_else(|| {
                if region.is_some() {
                    likely_lookup(language, None, region)
                } else {
                    None
                }
            })
            .or_else(|| likely_lookup(language, None, None))
    };
    let Some(maximal) = maximal else {
        return Ok(parts.render());
    };

    parts.language = maximal.0.to_string();
    parts.script = Some(maximal.1.to_string());
    parts.region = Some(maximal.2.to_string());
    Ok(parts.render())
}

/// RemoveLikelySubtags (ECMA-402 §15.4.3 / UTS #35): the minimal
/// (language, script, region) that still maximizes back to the tag's
/// maximal form. The maximal form's script is dropped when the maximal form
/// without it still maximizes back; then the region is dropped the same way
/// on the current form. The result keeps the maximal form's language and the
/// tag's variants/extensions/private use.
pub fn remove_likely_subtags(tag: &str) -> Result<String, JsError> {
    let parts = parse_locale_id(tag).ok_or_else(|| range_error("Invalid language tag"))?;
    let mut aliased = parts.clone();
    apply_aliases(&mut aliased);
    let max_tag = add_likely_subtags(tag)?;
    let max_parts = parse_locale_id(&max_tag).ok_or_else(|| range_error("Invalid language tag"))?;

    let mut script = max_parts.script.clone();
    let mut region = max_parts.region.clone();
    let mut current = max_parts.clone();

    // Try removing the script: the maximal form without it re-maximizes to
    // the same maximal form.
    if script.is_some() {
        let mut trial = current.clone();
        trial.script = None;
        if let Ok(trial_max) = add_likely_subtags(&trial.render())
            && trial_max == max_tag
        {
            script = None;
            current = trial;
        }
    }
    // Try removing the region on the current (possibly script-cleared) form.
    if region.is_some() {
        let mut trial = current.clone();
        trial.region = None;
        if let Ok(trial_max) = add_likely_subtags(&trial.render())
            && trial_max == max_tag
        {
            region = None;
        }
    }

    Ok(LocaleParts {
        language: max_parts.language,
        script,
        region,
        variants: aliased.variants,
        extensions: aliased.extensions,
        privateuse: aliased.privateuse,
    }
    .render())
}

/// GetLocaleBaseName (ECMA-402 §15.3.2): the longest prefix matching the
/// `unicode_language_id` — the language-id without any extension.
pub fn base_name(tag: &str) -> String {
    parse_locale_id(tag)
        .map(|parts| parts.base_name())
        .unwrap_or_default()
}

/// The first subtag of the base name (the language).
pub fn language(tag: &str) -> String {
    base_name(tag)
        .split('-')
        .next()
        .unwrap_or_default()
        .to_string()
}

/// The script subtag of the base name, if present.
pub fn script(tag: &str) -> Option<String> {
    let base = base_name(tag);
    let mut it = base.split('-');
    it.next()?;
    let script = it.next()?;
    if is_script_subtag(script) {
        Some(script.to_string())
    } else {
        None
    }
}

/// The region subtag of the base name, if present (only valid immediately
/// after the language, optionally preceded by a script).
pub fn region(tag: &str) -> Option<String> {
    let base = base_name(tag);
    let mut it = base.split('-');
    it.next()?;
    let second = it.next()?;
    if is_region_subtag(second) {
        return Some(second.to_string());
    }
    if is_script_subtag(second) {
        let third = it.next()?;
        if is_region_subtag(third) {
            return Some(third.to_string());
        }
    }
    None
}

/// GetLocaleVariants (ECMA-402 §15.5.5): the variant subtags of the base
/// name as a "-"-joined String, or `None` when the base name has none.
pub fn get_locale_variants(tag: &str) -> Option<String> {
    let parts = parse_locale_id(tag)?;
    if parts.variants.is_empty() {
        None
    } else {
        Some(parts.variants.join("-"))
    }
}

/// The value of a `u` extension keyword, or `None`. Keywords are `ca`, `co`,
/// `fw`, `hc`, `kf`, `kn`, `nu`; `numeric` (kn) returns "true"/"false".
pub fn unicode_extension_value(tag: &str, key: &str) -> Option<String> {
    let parts = parse_locale_id(tag)?;
    for extension in &parts.extensions {
        if !extension.starts_with("u-") {
            continue;
        }
        let subtags: Vec<&str> = extension[2..]
            .split('-')
            .filter(|s| !s.is_empty())
            .collect();
        let mut i = 0;
        while i < subtags.len() {
            if subtags[i].len() == 2 {
                if subtags[i] == key {
                    // The value is the following type token(s); a 2-char
                    // subtag is the next key, so this key is bare.
                    let mut value = String::new();
                    while i + 1 < subtags.len() && is_type_token(subtags[i + 1]) {
                        i += 1;
                        if !value.is_empty() {
                            value.push('-');
                        }
                        value.push_str(subtags[i]);
                    }
                    return Some(value);
                }
                // Skip the value tokens of a non-matching key.
                i += 1;
                while i < subtags.len() && is_type_token(subtags[i]) {
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
    }
    None
}

/// The `kn` keyword as a tri-state: `Some(true)`/`Some(false)` when the
/// keyword is present (an empty value is `true`), `None` when absent.
pub fn unicode_numeric(tag: &str) -> Option<bool> {
    match unicode_extension_value(tag, "kn").as_deref() {
        Some("") | Some("true") => Some(true),
        Some("false") => Some(false),
        Some(_) => None,
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(tag: &str) -> String {
        canonicalize(tag).unwrap()
    }

    #[test]
    fn canonicalizes_case_and_order() {
        assert_eq!(canon("eN"), "en");
        assert_eq!(canon("en-gb"), "en-GB");
        assert_eq!(canon("IT-LATN-iT"), "it-Latn-IT");
        assert_eq!(canon("th-th-u-nu-thai"), "th-TH-u-nu-thai");
        assert_eq!(canon("sl-ROZAJ-BISKE-1994"), "sl-1994-biske-rozaj");
        assert_eq!(canon("zh-latn-pinyin-pinyin2"), "zh-Latn-pinyin-pinyin2");
    }

    #[test]
    fn canonicalizes_extensions() {
        assert_eq!(canon("da-u-ca-gregory-ca-buddhist"), "da-u-ca-gregory");
        assert_eq!(canon("zh-u-nu-hans-ca-chinese"), "zh-u-ca-chinese-nu-hans");
        assert_eq!(canon("de-u-nu-latn-cu-eur"), "de-u-cu-eur-nu-latn");
        assert_eq!(
            canon("pt-u-attr2-attr1-ca-gregory"),
            "pt-u-attr1-attr2-ca-gregory"
        );
        assert_eq!(canon("en-u-baz-a-bar-x-u-foo"), "en-a-bar-u-baz-x-u-foo");
        assert_eq!(canon("en-a-bar-x-u-foo"), "en-a-bar-x-u-foo");
        assert_eq!(canon("en-x-u-foo-a-bar"), "en-x-u-foo-a-bar");
        assert_eq!(canon("en-x-u-foo"), "en-x-u-foo");
        // CanonicalizeUValue + "true"-type removal + transformed extensions.
        assert_eq!(canon("und-u-ca-ethiopic-amete-alem"), "und-u-ca-ethioaa");
        assert_eq!(canon("und-u-ks-primary"), "und-u-ks-level1");
        assert_eq!(canon("und-u-kb-yes"), "und-u-kb");
        assert_eq!(canon("de-u-kf-true"), "de-u-kf");
        assert_eq!(canon("und-u-tz-uct"), "und-u-tz-utc");
        assert_eq!(
            canon("sl-t-sl-rozaj-biske-1994"),
            "sl-t-sl-1994-biske-rozaj"
        );
        assert_eq!(canon("DE-T-M0-DIN-K0-QWERTZ"), "de-t-k0-qwertz-m0-din");
        assert_eq!(canon("en-t-iw"), "en-t-he");
        assert_eq!(
            canon("und-Latn-t-und-hani-m0-names"),
            "und-Latn-t-und-hani-m0-prprname"
        );
    }

    #[test]
    fn rejects_invalid_tags() {
        for tag in [
            "X-u-foo",
            "Flob",
            "ZORK",
            "Blah-latn",
            "QuuX-latn-us",
            "SPAM-gb-x-Sausages-BACON-eggs",
            "da-u",
            "da-u-",
            "da-u--",
            "da-u-t-latn",
            "da-u-x-priv",
            "da-u-ca-gregory-u-ca-buddhist",
            // Duplicate variants are rejected during validation.
            "zh-hakka-hakka",
            "en-t-en-latn-latn",
            // u-keys must not have a digit as their second character.
            "en-u-c0",
            "en-u-00",
            // transformed extension grammar.
            "en-t",
            "en-t-a",
            "en-t-x",
            "en-t-0",
            "en-t-",
            "en-t-root",
            "en-t-abcdefghi",
            "en-t-ar-aao",
            "en-t-en-0",
            "en-t-en-00",
            "en-t-en-latn-xyz",
            "en-t-en-latn-gb-ab",
            "en-t-d0",
            "en-t-d0-m0",
        ] {
            assert!(canonicalize(tag).is_err(), "{tag} should be invalid");
        }
        // The first u-key character may be a digit.
        assert_eq!(canon("en-u-0c"), "en-u-0c");
        assert_eq!(canon("en-t-en-ca"), "en-t-en-ca");
        assert_eq!(canon("en-t-en-latn-ca-emodeng"), "en-t-en-latn-ca-emodeng");
        assert_eq!(canon("en-t-d0-ascii"), "en-t-d0-ascii");
        // The tlang is entirely lowercased, like ICU.
        assert_eq!(canon("en-t-EN-Latn-GB"), "en-t-en-latn-gb");
    }

    #[test]
    fn applies_aliases() {
        assert_eq!(canon("mo"), "ro");
        assert_eq!(canon("aar-x-private"), "aa-x-private");
        assert_eq!(canon("heb-x-private"), "he-x-private");
        assert_eq!(canon("ces"), "cs");
        assert_eq!(canon("hy-arevela"), "hy");
        assert_eq!(canon("hy-arevmda"), "hyw");
        // Multi-subtag language aliases and territory aliases.
        assert_eq!(canon("cmn-hans-cn"), "zh-Hans-CN");
        assert_eq!(canon("cmn"), "zh");
        assert_eq!(canon("ji"), "yi");
        assert_eq!(canon("in"), "id");
        assert_eq!(canon("sh"), "sr-Latn");
        assert_eq!(canon("sh-Cyrl"), "sr-Cyrl");
        assert_eq!(canon("cnr"), "sr-ME");
        assert_eq!(canon("cnr-BA"), "sr-BA");
        assert_eq!(canon("sgn-GR"), "gss");
        assert_eq!(canon("art-lojban"), "jbo");
        assert_eq!(canon("cel-gaulish"), "xtg");
        assert_eq!(canon("zh-guoyu"), "zh");
        assert_eq!(canon("zh-hakka"), "hak");
        assert_eq!(canon("zh-xiang"), "hsn");
        assert_eq!(canon("ru-SU"), "ru-RU");
        assert_eq!(canon("hy-SU"), "hy-AM");
        assert_eq!(canon("und-Armn-SU"), "und-Armn-AM");
        assert_eq!(canon("en-SU"), "en-RU");
        assert_eq!(canon("und-Latn-SU"), "und-Latn-RU");
        assert_eq!(canon("sr-CS"), "sr-RS");
        assert_eq!(canon("az-NT"), "az-SA");
        assert_eq!(canon("de-DD"), "de-DE");
        assert_eq!(canon("ja-Latn-hepburn-heploc"), "ja-Latn-alalc97");
    }

    #[test]
    fn maximizes() {
        let max = |tag: &str| add_likely_subtags(tag).unwrap();
        assert_eq!(max("en"), "en-Latn-US");
        assert_eq!(max("en-Shaw"), "en-Shaw-GB");
        assert_eq!(max("en-Arab"), "en-Arab-US");
        assert_eq!(max("en-US"), "en-Latn-US");
        assert_eq!(max("en-GB"), "en-Latn-GB");
        assert_eq!(max("und"), "en-Latn-US");
        assert_eq!(max("und-Thai"), "th-Thai-TH");
        assert_eq!(max("und-419"), "es-Latn-419");
        assert_eq!(max("und-AT"), "de-Latn-AT");
        assert_eq!(max("und-Cyrl-RO"), "bg-Cyrl-RO");
        assert_eq!(max("und-AQ"), "en-Latn-AQ");
        assert_eq!(max("it-Kana-CA"), "it-Kana-CA");
        assert_eq!(max("de-u-kf"), "de-Latn-DE-u-kf");
        assert_eq!(max("aar-x-private"), "aa-Latn-ET-x-private");
        assert_eq!(max("zh-pinyin"), "zh-Hans-CN-pinyin");
        assert_eq!(max("hi-direct"), "hi-Deva-IN-direct");
        assert_eq!(max("es-ES-preeuro"), "es-Latn-ES-preeuro");
        // The grandfathered maximals.
        assert_eq!(max("jbo"), "jbo-Latn-001");
        assert_eq!(max("hak"), "hak-Hans-CN");
        assert_eq!(max("hsn"), "hsn-Hans-CN");
    }

    #[test]
    fn minimizes() {
        let min = |tag: &str| remove_likely_subtags(tag).unwrap();
        assert_eq!(min("en"), "en");
        assert_eq!(min("en-Latn-US"), "en");
        assert_eq!(min("en-GB"), "en-GB");
        assert_eq!(min("en-Shaw-GB"), "en-Shaw");
        assert_eq!(min("en-Arab-US"), "en-Arab");
        assert_eq!(min("en-Latn-GB"), "en-GB");
        assert_eq!(min("it-Kana-CA"), "it-Kana-CA");
        assert_eq!(min("th-Thai-TH"), "th");
        assert_eq!(min("es-Latn-419"), "es-419");
        assert_eq!(min("de-Latn-AT"), "de-AT");
        assert_eq!(min("bg-Cyrl-RO"), "bg-RO");
        assert_eq!(min("und-Latn-AQ"), "en-AQ");
        assert_eq!(min("ru-Cyrl-RU"), "ru");
    }

    #[test]
    fn base_name_and_getters() {
        assert_eq!(base_name("th-TH-u-nu-thai"), "th-TH");
        assert_eq!(base_name("en-a-bar-u-baz-x-u-foo"), "en");
        assert_eq!(language("th-TH-u-nu-thai"), "th");
        assert_eq!(script("en-Latn-US"), Some("Latn".to_string()));
        assert_eq!(script("en-US"), None);
        assert_eq!(region("en-Latn-US"), Some("US".to_string()));
        assert_eq!(region("en-US"), Some("US".to_string()));
        assert_eq!(
            unicode_extension_value("th-TH-u-nu-thai", "nu"),
            Some("thai".to_string())
        );
        assert_eq!(
            unicode_extension_value("de-u-kf", "kf"),
            Some(String::new())
        );
        assert_eq!(unicode_numeric("en-u-kn"), Some(true));
        assert_eq!(unicode_numeric("en-u-kn-false"), Some(false));
        assert_eq!(unicode_numeric("en"), None);
        assert_eq!(
            get_locale_variants("de-Latn-DE-1996-fonipa"),
            Some("1996-fonipa".to_string())
        );
        assert_eq!(get_locale_variants("en"), None);
        assert_eq!(canonicalize_uvalue("ca", "islamic"), "islamic");
        assert_eq!(canonicalize_uvalue("nu", "latn"), "latn");
    }
}
