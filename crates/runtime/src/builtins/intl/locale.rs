//! `Intl.Locale` (ECMA-402 §15): the constructor, InitializeLocale
//! (UpdateLanguageId + the Unicode extension options), the prototype
//! getters, `maximize`/`minimize`/`toString`. Instances store only their
//! canonical `[[Locale]]` string in the agent's `intl_locale_data` map;
//! every getter derives from it.

use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use crate::agent::Agent;
use crate::builtins::intl::{IntlLocaleRecord, bcp47};
use crate::context::{as_object, get_property, to_object, to_string};
use crate::realm::Realm;

pub const LOCALE: &str = "%Intl.Locale%";
pub const LOCALE_PROTO: &str = "%Intl.Locale.prototype%";
pub const LOCALE_TO_STRING: &str = "%Intl.Locale.prototype.toString%";
pub const LOCALE_MAXIMIZE: &str = "%Intl.Locale.prototype.maximize%";
pub const LOCALE_MINIMIZE: &str = "%Intl.Locale.prototype.minimize%";
const LOCALE_BASENAME: &str = "%Intl.Locale.prototype.baseName%";
const LOCALE_LANGUAGE: &str = "%Intl.Locale.prototype.language%";
const LOCALE_SCRIPT: &str = "%Intl.Locale.prototype.script%";
const LOCALE_REGION: &str = "%Intl.Locale.prototype.region%";
const LOCALE_CALENDAR: &str = "%Intl.Locale.prototype.calendar%";
const LOCALE_COLLATION: &str = "%Intl.Locale.prototype.collation%";
const LOCALE_CASEFIRST: &str = "%Intl.Locale.prototype.caseFirst%";
const LOCALE_HOURCYCLE: &str = "%Intl.Locale.prototype.hourCycle%";
const LOCALE_NUMBERINGSYSTEM: &str = "%Intl.Locale.prototype.numberingSystem%";
const LOCALE_NUMERIC: &str = "%Intl.Locale.prototype.numeric%";
const LOCALE_FIRST_DAY_OF_WEEK: &str = "%Intl.Locale.prototype.firstDayOfWeek%";
const LOCALE_VARIANTS: &str = "%Intl.Locale.prototype.variants%";

fn range_error(message: &str) -> JsError {
    JsError::new(ErrorKind::RangeError, message.into())
}

fn type_error(message: &str) -> JsError {
    JsError::new(ErrorKind::TypeError, message.into())
}

/// Install the `Intl.Locale` constructor and prototype onto `%Intl%`.
pub fn install(realm: &Handle<Realm>, intl_value: &Value) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let function_proto = realm
        .intrinsics
        .get("%Function.prototype%")
        .and_then(|value| as_object(&value));
    let locale_proto = JsObject::ordinary_object_create(object_proto);

    let locale_ctor = Function::create_builtin(
        Some(JsString::from_utf8("Locale")),
        1,
        placeholder("Intl.Locale"),
        Some(placeholder_ctor("Intl.Locale")),
        function_proto,
    )?;
    // Intl.Locale.prototype.constructor.
    locale_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(Value::Function(locale_ctor)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // Prototype methods.
    let methods: &[(&str, &str, u64)] = &[
        ("toString", LOCALE_TO_STRING, 0),
        ("maximize", LOCALE_MAXIMIZE, 0),
        ("minimize", LOCALE_MINIMIZE, 0),
    ];
    for (name, key, length) in methods {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            *length,
            placeholder(name),
            None,
            function_proto
        )?;
        realm.intrinsics.define(key, Value::Function(func));
        locale_proto.define_property(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: Some(Value::Function(func)),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
    }

    // The string getters (accessor properties, spec 15.3.2-15.3.4).
    let getters: &[(&str, &str)] = &[
        ("baseName", LOCALE_BASENAME),
        ("language", LOCALE_LANGUAGE),
        ("script", LOCALE_SCRIPT),
        ("region", LOCALE_REGION),
        ("calendar", LOCALE_CALENDAR),
        ("collation", LOCALE_COLLATION),
        ("caseFirst", LOCALE_CASEFIRST),
        ("hourCycle", LOCALE_HOURCYCLE),
        ("numberingSystem", LOCALE_NUMBERINGSYSTEM),
        ("numeric", LOCALE_NUMERIC),
        ("firstDayOfWeek", LOCALE_FIRST_DAY_OF_WEEK),
        ("variants", LOCALE_VARIANTS),
    ];
    for (name, key) in getters {
        let getter = Function::create_builtin(
            Some(JsString::from_utf8(&format!("get {name}"))),
            0,
            placeholder(name),
            None,
            function_proto
        )?;
        realm
            .intrinsics
            .define(key, Value::Function(getter));
        locale_proto.define_property(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: None,
                writable: None,
                get: Some(Value::Function(getter)),
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
    }

    // %Intl.Locale.prototype%[@@toStringTag] = "Intl.Locale".
    locale_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag")),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8(
                "Intl.Locale",
            )))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    let locale_proto_value = Value::Object(locale_proto);
    // MakeConstructor: %Intl.Locale%.prototype (the class-extends check and
    // GetPrototypeFromConstructor read it).
    locale_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(locale_proto_value),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    realm.intrinsics.define(LOCALE_PROTO, locale_proto_value);
    realm
        .intrinsics
        .define(LOCALE, Value::Function(locale_ctor));

    // `Intl.Locale` on the %Intl% object (writable, non-enumerable,
    // configurable — a normal function property).
    if let Some(obj) = as_object(intl_value) {
        obj.define_property(
            &JsString::from_utf8("Locale"),
            &PropertyDescriptor {
                value: Some(Value::Function(locale_ctor)),
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

fn placeholder_ctor(name: &str) -> NativeFn {
    let name = name.to_string();
    Box::new(move |_, _| Err(type_error(&format!("{name} must be dispatched"))))
}

/// The instance record lookup: `None` when `this` is not an initialized
/// Intl.Locale (the branding TypeError the getters/methods throw).
fn locale_record(agent: &Agent, this: &Value) -> Result<IntlLocaleRecord, JsError> {
    let Some(obj) = as_object(this) else {
        return Err(type_error("Intl.Locale getter called on a non-object"));
    };
    agent
        .intl_locale_data
        .get(&obj.id())
        .cloned()
        .ok_or_else(|| type_error("Intl.Locale getter called on an uninitialized object"))
}

/// dispatch_call: the `Intl.Locale` prototype methods and getters.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    _args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(LOCALE_TO_STRING).as_ref() == Some(callee) {
        return Some(
            locale_record(agent, this)
                .map(|record| Value::String(Handle::new(JsString::from_utf8(&record.locale)))),
        );
    }
    if intrinsics.get(LOCALE_MAXIMIZE).as_ref() == Some(callee) {
        return Some(maximize_or_minimize(agent, this, true));
    }
    if intrinsics.get(LOCALE_MINIMIZE).as_ref() == Some(callee) {
        return Some(maximize_or_minimize(agent, this, false));
    }
    if intrinsics.get(LOCALE_BASENAME).as_ref() == Some(callee) {
        return Some(getter(agent, this, bcp47::base_name));
    }
    if intrinsics.get(LOCALE_LANGUAGE).as_ref() == Some(callee) {
        return Some(getter(agent, this, bcp47::language));
    }
    if intrinsics.get(LOCALE_SCRIPT).as_ref() == Some(callee) {
        return Some(getter_opt(agent, this, bcp47::script));
    }
    if intrinsics.get(LOCALE_REGION).as_ref() == Some(callee) {
        return Some(getter_opt(agent, this, bcp47::region));
    }
    if intrinsics.get(LOCALE_CALENDAR).as_ref() == Some(callee) {
        return Some(getter_opt(agent, this, |tag| {
            bcp47::unicode_extension_value(tag, "ca")
        }));
    }
    if intrinsics.get(LOCALE_COLLATION).as_ref() == Some(callee) {
        return Some(getter_opt(agent, this, |tag| {
            bcp47::unicode_extension_value(tag, "co")
        }));
    }
    if intrinsics.get(LOCALE_CASEFIRST).as_ref() == Some(callee) {
        return Some(getter_opt(agent, this, |tag| {
            bcp47::unicode_extension_value(tag, "kf")
        }));
    }
    if intrinsics.get(LOCALE_HOURCYCLE).as_ref() == Some(callee) {
        return Some(getter_opt(agent, this, |tag| {
            bcp47::unicode_extension_value(tag, "hc")
        }));
    }
    if intrinsics.get(LOCALE_NUMBERINGSYSTEM).as_ref() == Some(callee) {
        return Some(getter_opt(agent, this, |tag| {
            bcp47::unicode_extension_value(tag, "nu")
        }));
    }
    if intrinsics.get(LOCALE_NUMERIC).as_ref() == Some(callee) {
        // [[Numeric]]: the kn value is "true" or the empty String → true;
        // anything else (including an absent keyword) → false.
        return Some(
            locale_record(agent, this)
                .map(|record| Value::Boolean(bcp47::unicode_numeric(&record.locale) == Some(true))),
        );
    }
    if intrinsics.get(LOCALE_FIRST_DAY_OF_WEEK).as_ref() == Some(callee) {
        return Some(getter_opt(agent, this, |tag| {
            bcp47::unicode_extension_value(tag, "fw")
        }));
    }
    if intrinsics.get(LOCALE_VARIANTS).as_ref() == Some(callee) {
        return Some(getter_opt(agent, this, bcp47::get_locale_variants));
    }
    None
}

fn getter(agent: &mut Agent, this: &Value, f: impl Fn(&str) -> String) -> Result<Value, JsError> {
    locale_record(agent, this)
        .map(|record| Value::String(Handle::new(JsString::from_utf8(&f(&record.locale)))))
}

fn getter_opt(
    agent: &mut Agent,
    this: &Value,
    f: impl Fn(&str) -> Option<String>,
) -> Result<Value, JsError> {
    locale_record(agent, this).map(|record| match f(&record.locale) {
        Some(value) => Value::String(Handle::new(JsString::from_utf8(&value))),
        None => Value::Undefined,
    })
}

/// `Intl.Locale.prototype.maximize`/`minimize`: a brand-new Locale with the
/// maximized/minimized tag.
fn maximize_or_minimize(agent: &mut Agent, this: &Value, maximize: bool) -> Result<Value, JsError> {
    let record = locale_record(agent, this)?;
    let result = if maximize {
        bcp47::add_likely_subtags(&record.locale)?
    } else {
        bcp47::remove_likely_subtags(&record.locale)?
    };
    // The spec's MaximizeLocale/MinimizeLocale return a new Locale via the
    // %Intl.Locale% constructor with the same realm as `this`.
    let realm = agent.current_realm()?;
    let proto = realm
        .intrinsics
        .get(LOCALE_PROTO)
        .and_then(|value| as_object(&value))
        .ok_or_else(|| type_error("%Intl.Locale.prototype% missing"))?;
    let instance = JsObject::ordinary_object_create(Some(proto));
    agent
        .intl_locale_data
        .insert(instance.id(), IntlLocaleRecord { locale: result });
    Ok(Value::Object(instance))
}

/// dispatch_construct: `new Intl.Locale(tag, options)`.
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    if realm.intrinsics.get(LOCALE).as_ref() != Some(callee) {
        return None;
    }
    Some(construct(agent, new_target, args))
}

fn construct(agent: &mut Agent, new_target: &Value, args: &[Value]) -> Result<Value, JsError> {
    // GetPrototypeFromConstructor (spec 10.1.8): the newTarget's
    // `prototype`, falling back to %Intl.Locale.prototype%.
    let proto = crate::context::get_property(
        agent,
        new_target,
        &JsString::from_utf8("prototype"),
        *new_target,
    )?;
    let proto = if let Some(obj) = as_object(&proto) {
        obj
    } else {
        crate::context::get_function_realm(agent, new_target)?
            .intrinsics
            .get(LOCALE_PROTO)
            .and_then(|value| as_object(&value))
            .ok_or_else(|| type_error("%Intl.Locale.prototype% missing"))?
    };
    let instance = JsObject::ordinary_object_create(Some(proto));
    let locale = initialize_locale(
        agent,
        args.first().cloned().unwrap_or(Value::Undefined),
        args.get(1).cloned().unwrap_or(Value::Undefined),
    )?;
    agent
        .intl_locale_data
        .insert(instance.id(), IntlLocaleRecord { locale });
    Ok(Value::Object(instance))
}

/// GetOptionsObject (ECMA-402 §2.1): undefined → a fresh empty object;
/// null → TypeError; an object → as-is; a primitive → ToObject.
fn get_options_object(agent: &mut Agent, options: &Value) -> Result<Value, JsError> {
    match options.kind() {
        ValueKind::Undefined => Ok(Value::Object(JsObject::ordinary_object_create(None))),
        ValueKind::Null => Err(type_error("Options argument cannot be null")),
        ValueKind::Object(_) | ValueKind::Function(_) => Ok(*options),
        _ => to_object(agent, options),
    }
}

/// GetOption (spec 7.3.18, simplified to "string-or-boolean"): `None` when
/// the property is absent or undefined. Reading the property runs a getter.
fn get_option(agent: &mut Agent, options: &Value, name: &str) -> Result<Option<Value>, JsError> {
    let value = get_property(agent, options, &JsString::from_utf8(name), *options)?;
    if value.is_undefined() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn string_option(
    agent: &mut Agent,
    options: &Value,
    name: &str,
) -> Result<Option<String>, JsError> {
    match get_option(agent, options, name)? {
        Some(value) => Ok(Some(to_string(agent, &value)?.to_string_lossy())),
        None => Ok(None),
    }
}

/// The full InitializeLocale pipeline (ECMA-402 §15.1.x): the tag, the
/// options, canonicalization, UpdateLanguageId, and the Unicode extension.
fn initialize_locale(
    agent: &mut Agent,
    tag_value: Value,
    options_value: Value,
) -> Result<String, JsError> {
    // The tag: a String, an initialized Intl.Locale (its [[Locale]]), an
    // Object coerced with ToString, or a TypeError (spec §15.1.1 step 7:
    // neither a String nor an Object throws).
    let tag_text: String = if let Some(s) = tag_value.as_string() {
        s.to_string_lossy()
    } else if let Some(obj) = as_object(&tag_value) {
        if let Some(record) = agent.intl_locale_data.get(&obj.id()) {
            record.locale.clone()
        } else {
            to_string(agent, &tag_value)?.to_string_lossy()
        }
    } else {
        return Err(type_error("Invalid tag value"));
    };
    let options = get_options_object(agent, &options_value)?;

    let mut tag = bcp47::canonicalize(&tag_text)?;
    tag = update_language_id(agent, &tag, &options)?;
    let tag = apply_unicode_options(agent, &tag, &options)?;
    Ok(tag)
}

/// UpdateLanguageId (ECMA-402 §15.1.2): the language/script/region/
/// variants options override the tag's language-id; the extensions and
/// private use are preserved.
fn update_language_id(agent: &mut Agent, tag: &str, options: &Value) -> Result<String, JsError> {
    let parts = bcp47::parse_locale_id(tag).ok_or_else(|| range_error("Invalid language tag"))?;
    let base = parts.base_name();

    let language = match string_option(agent, options, "language")? {
        Some(value) => value,
        None => bcp47::language(&base),
    };
    if !bcp47::is_language_subtag(&language) {
        return Err(range_error("Invalid language subtag"));
    }
    let script = match string_option(agent, options, "script")? {
        Some(value) => {
            if !is_script_option(&value) {
                return Err(range_error("Invalid script subtag"));
            }
            Some(value)
        }
        None => bcp47::script(&base),
    };
    let region = match string_option(agent, options, "region")? {
        Some(value) => {
            if !is_region_option(&value) {
                return Err(range_error("Invalid region subtag"));
            }
            Some(value)
        }
        None => bcp47::region(&base),
    };
    let variants = match string_option(agent, options, "variants")? {
        Some(value) => {
            if value.is_empty() {
                return Err(range_error("Empty variants option"));
            }
            let mut list = Vec::new();
            for variant in value.split('-') {
                if !is_variant_option(variant) {
                    return Err(range_error("Invalid variant subtag"));
                }
                // Spec: ASCII-lowercase the whole value first, so the
                // duplicate check is case-insensitive (fonipa-valencia-Fonipa
                // is rejected).
                let lower = variant.to_ascii_lowercase();
                if list.contains(&lower) {
                    return Err(range_error("Duplicate variant subtag"));
                }
                list.push(lower);
            }
            Some(list)
        }
        None => {
            if parts.variants.is_empty() {
                None
            } else {
                Some(parts.variants.clone())
            }
        }
    };

    let mut new_tag = language;
    if let Some(script) = script {
        new_tag.push('-');
        new_tag.push_str(&script);
    }
    if let Some(region) = region {
        new_tag.push('-');
        new_tag.push_str(&region);
    }
    if let Some(variants) = variants {
        for variant in variants {
            new_tag.push('-');
            new_tag.push_str(&variant);
        }
    }
    // The extensions and private use ride along unchanged.
    for extension in &parts.extensions {
        new_tag.push('-');
        new_tag.push_str(extension);
    }
    if !parts.privateuse.is_empty() {
        new_tag.push_str("-x");
        for subtag in &parts.privateuse {
            new_tag.push('-');
            new_tag.push_str(subtag);
        }
    }
    bcp47::canonicalize(&new_tag)
}

fn is_script_option(value: &str) -> bool {
    value.len() == 4 && value.bytes().all(|c| c.is_ascii_alphabetic())
}

fn is_region_option(value: &str) -> bool {
    (value.len() == 2 && value.bytes().all(|c| c.is_ascii_alphabetic()))
        || (value.len() == 3 && value.bytes().all(|c| c.is_ascii_digit()))
}

fn is_variant_option(value: &str) -> bool {
    let len = value.len();
    if len == 4 {
        return value.as_bytes()[0].is_ascii_digit()
            && value[1..].bytes().all(|c| c.is_ascii_alphanumeric());
    }
    (5..=8).contains(&len) && value.bytes().all(|c| c.is_ascii_alphanumeric())
}

/// The u-extension options: calendar, collation, firstDayOfWeek, hourCycle,
/// caseFirst, numeric, numberingSystem — read in spec order, merged into
/// the tag's existing `u` extension, re-canonicalized.
fn apply_unicode_options(agent: &mut Agent, tag: &str, options: &Value) -> Result<String, JsError> {
    // Collect the existing u-extension's attributes and keywords.
    let mut attributes: Vec<String> = Vec::new();
    let mut keywords: Vec<(String, String)> = Vec::new();
    if let Some(parts) = bcp47::parse_locale_id(tag)
        && let Some(u) = parts.extensions.iter().find(|e| e.starts_with("u-"))
    {
        let subtags: Vec<&str> = u[2..].split('-').filter(|s| !s.is_empty()).collect();
        let mut i = 0;
        let mut attributes_done = false;
        while i < subtags.len() {
            let sub = subtags[i];
            if !attributes_done && sub.len() >= 3 && sub.len() <= 8 && !bcp47::is_key(sub) {
                attributes.push(sub.to_string());
                i += 1;
            } else {
                attributes_done = true;
                if sub.len() == 2 && sub.bytes().all(|c| c.is_ascii_alphanumeric()) {
                    let key = sub.to_ascii_lowercase();
                    let mut value = String::new();
                    i += 1;
                    while i < subtags.len() {
                        let t = subtags[i];
                        if t.len() >= 3 && t.len() <= 8 {
                            if !value.is_empty() {
                                value.push('-');
                            }
                            value.push_str(&t.to_ascii_lowercase());
                            i += 1;
                        } else {
                            break;
                        }
                    }
                    keywords.push((key, value));
                } else {
                    i += 1;
                }
            }
        }
    }

    // The option overrides, in spec read order.
    let mut set_keyword = |key: &str, value: String| {
        for entry in &mut keywords {
            if entry.0 == key {
                entry.1 = value;
                return;
            }
        }
        keywords.push((key.to_string(), value));
    };

    if let Some(value) = string_option(agent, options, "calendar")? {
        if !is_type_option(&value) {
            return Err(range_error("Invalid calendar value"));
        }
        set_keyword("ca", value);
    }
    if let Some(value) = string_option(agent, options, "collation")? {
        if !is_type_option(&value) {
            return Err(range_error("Invalid collation value"));
        }
        set_keyword("co", value);
    }
    if let Some(value) = string_option(agent, options, "firstDayOfWeek")? {
        // WeekdayToUValue: the option is already the canonical 3-letter code.
        if !matches!(
            value.as_str(),
            "mon" | "tue" | "wed" | "thu" | "fri" | "sat" | "sun"
        ) {
            return Err(range_error("Invalid firstDayOfWeek value"));
        }
        set_keyword("fw", value);
    }
    if let Some(value) = string_option(agent, options, "hourCycle")? {
        if !matches!(value.as_str(), "h11" | "h12" | "h23" | "h24") {
            return Err(range_error("Invalid hourCycle value"));
        }
        set_keyword("hc", value);
    }
    if let Some(value) = string_option(agent, options, "caseFirst")? {
        if !matches!(value.as_str(), "upper" | "lower" | "false") {
            return Err(range_error("Invalid caseFirst value"));
        }
        set_keyword("kf", value);
    }
    if let Some(value) = get_option(agent, options, "numeric")? {
        // GetOption type "boolean": ToBoolean, then ToString.
        let boolean = crux::convert::to_boolean(&value);
        let text = boolean.to_string();
        set_keyword("kn", text);
    }
    if let Some(value) = string_option(agent, options, "numberingSystem")? {
        // The numberingSystem option is a `type` (alphanum{3,8} *("-"
        // alphanum{3,8})): the corpus accepts hyphenated values here, like
        // calendar.
        if !is_type_option(&value) {
            return Err(range_error("Invalid numberingSystem value"));
        }
        set_keyword("nu", value);
    }

    // Rebuild the tag with the u-extension replaced.
    let mut parts =
        bcp47::parse_locale_id(tag).ok_or_else(|| range_error("Invalid language tag"))?;
    parts.extensions.retain(|e| !e.starts_with("u-"));
    attributes.sort();
    attributes.dedup();
    keywords.sort_by(|a, b| a.0.cmp(&b.0));
    keywords.dedup_by(|a, b| a.0 == b.0);
    // kn with value "true" is written as a bare key; "false" as `kn-false`.
    let mut u = String::from("u");
    for attribute in &attributes {
        u.push('-');
        u.push_str(attribute);
    }
    for (key, value) in &keywords {
        u.push('-');
        u.push_str(key);
        if !(value.is_empty() || (key == "kn" && value == "true")) {
            u.push('-');
            u.push_str(value);
        }
    }
    if attributes.is_empty() && keywords.is_empty() {
        // No u-extension remains.
    } else {
        parts.extensions.push(u);
    }
    bcp47::canonicalize(&parts.render())
}

/// The multi-token `type` grammar (alphanum{3,8} *("-" alphanum{3,8})) for
/// the calendar/collation/firstDayOfWeek option values.
fn is_type_option(value: &str) -> bool {
    !value.is_empty()
        && value.split('-').all(|token| {
            (3..=8).contains(&token.len()) && token.bytes().all(|c| c.is_ascii_alphanumeric())
        })
}
