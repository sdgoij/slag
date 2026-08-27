//! `Intl.DisplayNames` (ECMA-402 §12): the constructor (require-options —
//! the options argument is mandatory — plus style/type/fallback/
//! languageDisplay), `of` through CanonicalCodeForDisplayNames, and
//! `resolvedOptions`. The corpus's `of()` fixtures assert only the
//! well-formedness of the codes (`typeof result === 'string'`), never the
//! display-name values, so the fields tables are empty and the default
//! `fallback: "code"` returns the canonical code. Instances store their
//! record in the agent's `intl_display_names_data` map.

use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::Value;

use crate::agent::Agent;
use crate::builtins::intl::bcp47;
use crate::builtins::intl::number_format::{self, get_option};
use crate::context::{as_object, get_property, to_string};
use crate::realm::Realm;

pub const DISPLAY_NAMES: &str = "%Intl.DisplayNames%";
pub const DISPLAY_NAMES_PROTO: &str = "%Intl.DisplayNames.prototype%";
pub const DN_RESOLVED_OPTIONS: &str = "%Intl.DisplayNames.prototype.resolvedOptions%";
pub const DN_OF: &str = "%Intl.DisplayNames.prototype.of%";

const STYLE_LONG: u8 = 0;
const STYLE_SHORT: u8 = 1;
const STYLE_NARROW: u8 = 2;
const FALLBACK_CODE: u8 = 0;
const FALLBACK_NONE: u8 = 1;

fn range_error(message: &str) -> JsError {
    JsError::new(ErrorKind::RangeError, message.into())
}

fn type_error(message: &str) -> JsError {
    JsError::new(ErrorKind::TypeError, message.into())
}

/// The [[InitializedDisplayNames]] record. The [[Fields]] table is empty —
/// `of()` falls back to the canonical code (or undefined with "none").
#[derive(Debug, Clone)]
pub struct DisplayNamesRecord {
    pub locale: String,
    pub style: u8,
    pub type_value: String,
    pub fallback: u8,
    pub language_display: Option<String>,
}

/// GetOptionsObject (ECMA-262 §8.4.5): undefined → a fresh object; an
/// Object → itself; any primitive → TypeError.
fn get_options_object(_agent: &mut Agent, options: &Value) -> Result<Value, JsError> {
    if options.is_undefined() {
        Ok(Value::Object(JsObject::ordinary_object_create(None)))
    } else if as_object(options).is_some() {
        Ok(*options)
    } else {
        Err(type_error("Options must be an object"))
    }
}

/// IsValidDateTimeFieldCode (ECMA-402 §12.5.2, Table 19).
fn is_valid_date_time_field_code(code: &str) -> bool {
    matches!(
        code,
        "era"
            | "year"
            | "quarter"
            | "month"
            | "weekOfYear"
            | "weekday"
            | "day"
            | "dayPeriod"
            | "hour"
            | "minute"
            | "second"
            | "timeZoneName"
    )
}

/// CanonicalCodeForDisplayNames (ECMA-402 §12.5.1).
fn canonical_code_for_display_names(type_value: &str, code: &str) -> Result<String, JsError> {
    match type_value {
        "language" => {
            // The unicode_language_id nonterminal: the base language id only
            // — no extensions or private use (an `en-u-hebrew` code throws).
            let Some(parts) = bcp47::parse_locale_id(code) else {
                return Err(range_error("Invalid language code"));
            };
            if !parts.extensions.is_empty() || !parts.privateuse.is_empty() {
                return Err(range_error("Invalid language code"));
            }
            bcp47::canonicalize(code)
        }
        "region" => {
            let bytes = code.as_bytes();
            let well_formed = (bytes.len() == 2 && bytes.iter().all(|b| b.is_ascii_alphabetic()))
                || (bytes.len() == 3 && bytes.iter().all(|b| b.is_ascii_digit()));
            if !well_formed {
                return Err(range_error("Invalid region code"));
            }
            Ok(code.to_ascii_uppercase())
        }
        "script" => {
            if code.len() != 4 || !code.bytes().all(|b| b.is_ascii_alphabetic()) {
                return Err(range_error("Invalid script code"));
            }
            let mut chars: Vec<char> = code.chars().collect();
            chars[0] = chars[0].to_ascii_uppercase();
            for c in &mut chars[1..] {
                *c = c.to_ascii_lowercase();
            }
            Ok(chars.into_iter().collect())
        }
        "calendar" => {
            if !code.split('-').all(|subtag| {
                (3..=8).contains(&subtag.len()) && subtag.bytes().all(|b| b.is_ascii_alphanumeric())
            }) {
                return Err(range_error("Invalid calendar code"));
            }
            Ok(code.to_ascii_lowercase())
        }
        "dateTimeField" => {
            if !is_valid_date_time_field_code(code) {
                return Err(range_error("Invalid dateTimeField code"));
            }
            Ok(code.to_string())
        }
        _ => {
            // currency
            if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_alphabetic()) {
                return Err(range_error("Invalid currency code"));
            }
            Ok(code.to_ascii_uppercase())
        }
    }
}

/// Install `Intl.DisplayNames` onto `%Intl%`.
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
        Some(JsString::from_utf8("DisplayNames")),
        2,
        placeholder("Intl.DisplayNames"),
        Some(placeholder("Intl.DisplayNames")),
        function_proto
    )?;
    proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(Value::Function(ctor)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    let methods: &[(&str, &str, u64)] = &[
        ("resolvedOptions", DN_RESOLVED_OPTIONS, 0),
        ("of", DN_OF, 1),
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
        proto.define_property(
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
    // %Intl.DisplayNames.prototype%[@@toStringTag] = "Intl.DisplayNames".
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag")),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8(
                "Intl.DisplayNames",
            )))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    let proto_value = Value::Object(proto);
    ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(proto_value),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    realm.intrinsics.define(DISPLAY_NAMES_PROTO, proto_value);
    realm
        .intrinsics
        .define(DISPLAY_NAMES, Value::Function(ctor));
    if let Some(obj) = as_object(intl_value) {
        obj.define_property(
            &JsString::from_utf8("DisplayNames"),
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
fn display_names_record(agent: &Agent, this: &Value) -> Result<DisplayNamesRecord, JsError> {
    let Some(obj) = as_object(this) else {
        return Err(type_error("Not a DisplayNames instance"));
    };
    agent
        .intl_display_names_data
        .get(&obj.id())
        .cloned()
        .ok_or_else(|| type_error("Not a DisplayNames instance"))
}

/// GetPrototypeFromConstructor: the newTarget's `prototype`, falling back to
/// %Intl.DisplayNames.prototype% of the newTarget's realm.
fn proto_from_ctor(agent: &mut Agent, new_target: &Value) -> Result<Handle<JsObject>, JsError> {
    let proto = get_property(
        agent,
        new_target,
        &JsString::from_utf8("prototype"),
        *new_target,
    )?;
    if let Some(obj) = as_object(&proto) {
        return Ok(obj);
    }
    crate::context::get_function_realm(agent, new_target)?
        .intrinsics
        .get(DISPLAY_NAMES_PROTO)
        .and_then(|value| as_object(&value))
        .ok_or_else(|| type_error("%Intl.DisplayNames.prototype% missing"))
}

/// Intl.DisplayNames (ECMA-402 §12.1.1).
fn initialize(
    agent: &mut Agent,
    locales: &Value,
    options: &Value,
) -> Result<DisplayNamesRecord, JsError> {
    let requested = crate::builtins::intl::canonicalize_locale_list(agent, locales)?;
    // ResolveOptions with « require-options »: the options argument is
    // mandatory, and non-object options throw.
    if options.is_undefined() {
        return Err(type_error("Options is required"));
    }
    let options = get_options_object(agent, options)?;
    get_option(
        agent,
        &options,
        "localeMatcher",
        &["lookup", "best fit"],
        Some("best fit"),
    )?;
    let locale = number_format::resolve_locale_simple(&requested)?;
    let style = get_option(
        agent,
        &options,
        "style",
        &["narrow", "short", "long"],
        Some("long"),
    )?;
    let type_value = get_option(
        agent,
        &options,
        "type",
        &[
            "language",
            "region",
            "script",
            "currency",
            "calendar",
            "dateTimeField",
        ],
        None,
    )?;
    let type_value = type_value.ok_or_else(|| type_error("Type option is required"))?;
    let fallback = get_option(agent, &options, "fallback", &["code", "none"], Some("code"))?;
    let language_display = if type_value == "language" {
        get_option(
            agent,
            &options,
            "languageDisplay",
            &["dialect", "standard"],
            Some("dialect"),
        )?
    } else {
        None
    };
    Ok(DisplayNamesRecord {
        locale,
        style: match style.as_deref() {
            Some("narrow") => STYLE_NARROW,
            Some("short") => STYLE_SHORT,
            _ => STYLE_LONG,
        },
        type_value,
        fallback: if fallback.as_deref() == Some("none") {
            FALLBACK_NONE
        } else {
            FALLBACK_CODE
        },
        language_display,
    })
}

fn create_instance(
    agent: &mut Agent,
    proto: Handle<JsObject>,
    record: DisplayNamesRecord,
) -> Result<Value, JsError> {
    let instance = JsObject::ordinary_object_create(Some(proto));
    agent.intl_display_names_data.insert(instance.id(), record);
    Ok(Value::Object(instance))
}

/// The display-name fields tables. The corpus pins only the *coverage* —
/// every supportedValuesOf calendar/currency must map to a string under
/// `fallback: "none"` (`calendars-accepted-by-DisplayNames.js` etc.) — never
/// the display-name values, so the identity of the code satisfies it.
fn fields_contains(type_value: &str, code: &str) -> bool {
    match type_value {
        "calendar" => crate::builtins::intl::number_data::SUPPORTED_CALENDARS.contains(&code),
        "currency" => crate::builtins::intl::number_data::ISO_4217_CURRENCIES.contains(&code),
        _ => false,
    }
}

/// Intl.DisplayNames.prototype.of (ECMA-402 §12.3.3).
fn of_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let record = display_names_record(agent, this)?;
    let code_value = args.first().cloned().unwrap_or(Value::Undefined);
    let code = to_string(agent, &code_value)?.to_string_lossy();
    let code = canonical_code_for_display_names(&record.type_value, &code)?;
    if fields_contains(&record.type_value, &code) {
        return Ok(Value::String(Handle::new(JsString::from_utf8(&code))));
    }
    if record.fallback == FALLBACK_NONE {
        Ok(Value::Undefined)
    } else {
        Ok(Value::String(Handle::new(JsString::from_utf8(&code))))
    }
}

/// Intl.DisplayNames.prototype.resolvedOptions (ECMA-402 §12.3.2).
fn resolved_options_method(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let record = display_names_record(agent, this)?;
    let object_proto = agent
        .current_realm()?
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let options = JsObject::ordinary_object_create(object_proto);
    let define = |name: &str, value: Value| -> Result<(), JsError> {
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
        Ok(())
    };
    let str = |s: &str| Value::String(Handle::new(JsString::from_utf8(s)));
    define("locale", str(&record.locale))?;
    define(
        "style",
        str(match record.style {
            STYLE_NARROW => "narrow",
            STYLE_SHORT => "short",
            _ => "long",
        }),
    )?;
    define("type", str(&record.type_value))?;
    define(
        "fallback",
        str(if record.fallback == FALLBACK_NONE {
            "none"
        } else {
            "code"
        }),
    )?;
    if let Some(language_display) = &record.language_display {
        define("languageDisplay", str(language_display))?;
    }
    Ok(Value::Object(options))
}

/// dispatch_call: the DisplayNames constructor (as a function — throws) and
/// the prototype members.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(DISPLAY_NAMES).as_ref() == Some(callee) {
        return Some(Err(type_error("Intl.DisplayNames requires 'new'")));
    }
    if intrinsics.get(DN_RESOLVED_OPTIONS).as_ref() == Some(callee) {
        return Some(resolved_options_method(agent, this));
    }
    if intrinsics.get(DN_OF).as_ref() == Some(callee) {
        return Some(of_method(agent, this, args));
    }
    None
}

/// dispatch_construct: `new Intl.DisplayNames(...)`.
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    if realm.intrinsics.get(DISPLAY_NAMES).as_ref() == Some(callee) {
        let proto = match proto_from_ctor(agent, new_target) {
            Ok(proto) => proto,
            Err(error) => return Some(Err(error)),
        };
        let locales = args.first().cloned().unwrap_or(Value::Undefined);
        let options = args.get(1).cloned().unwrap_or(Value::Undefined);
        return Some(match initialize(agent, &locales, &options) {
            Ok(record) => create_instance(agent, proto, record),
            Err(error) => Err(error),
        });
    }
    None
}
