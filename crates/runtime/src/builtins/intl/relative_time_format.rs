//! `Intl.RelativeTimeFormat` (ECMA-402 §18): the constructor (style, numeric,
//! and the `nu` locale resolution), `format`/`formatToParts` through
//! PartitionRelativeTimePattern, and `resolvedOptions`. The number part is
//! formatted with an internal NumberFormat record (grouping per-locale) and
//! the plural form comes from an internal PluralRules record; the unit
//! strings and tense affixes live in `plural_data.rs`. Instances store their
//! record in the agent's `intl_rtf_data` map.

use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::Value;

use crate::agent::Agent;
use crate::builtins::intl::number_data::locale_data;
use crate::builtins::intl::number_format::{
    self, DISPLAY_SHORT, GROUPING_AUTO, GROUPING_MIN2, IntlMv, NOTATION_STANDARD,
    NumberFormatRecord, ROUNDING_FRACTION, STYLE_DECIMAL,
};
use crate::builtins::intl::plural_data;
use crate::context::{as_object, get_property, to_number};
use crate::realm::Realm;

pub const RELATIVE_TIME_FORMAT: &str = "%Intl.RelativeTimeFormat%";
pub const RTF_PROTO: &str = "%Intl.RelativeTimeFormat.prototype%";
pub const RTF_SUPPORTED_LOCALES_OF: &str = "%Intl.RelativeTimeFormat.supportedLocalesOf%";
pub const RTF_RESOLVED_OPTIONS: &str = "%Intl.RelativeTimeFormat.prototype.resolvedOptions%";
pub const RTF_FORMAT: &str = "%Intl.RelativeTimeFormat.prototype.format%";
pub const RTF_FORMAT_TO_PARTS: &str = "%Intl.RelativeTimeFormat.prototype.formatToParts%";

const STYLE_LONG: u8 = 0;
const STYLE_SHORT: u8 = 1;
const STYLE_NARROW: u8 = 2;
const NUMERIC_ALWAYS: u8 = 0;
const NUMERIC_AUTO: u8 = 1;

fn range_error(message: &str) -> JsError {
    JsError::new(ErrorKind::RangeError, message.into())
}

fn type_error(message: &str) -> JsError {
    JsError::new(ErrorKind::TypeError, message.into())
}

/// The [[InitializedRelativeTimeFormat]] record. The internal NumberFormat
/// and PluralRules records are derived from the resolved locale + numbering
/// system on demand.
#[derive(Debug, Clone)]
pub struct RelativeTimeFormatRecord {
    pub locale: String,
    pub numbering_system: String,
    pub style: u8,
    pub numeric: u8,
}

/// The singular unit forms (SingularRelativeTimeUnit, ECMA-402 §18.5.1).
fn singular_relative_time_unit(unit: &str) -> Option<&'static str> {
    Some(match unit {
        "second" | "seconds" => "second",
        "minute" | "minutes" => "minute",
        "hour" | "hours" => "hour",
        "day" | "days" => "day",
        "week" | "weeks" => "week",
        "month" | "months" => "month",
        "quarter" | "quarters" => "quarter",
        "year" | "years" => "year",
        _ => return None,
    })
}

/// The internal NumberFormat record: the `numberingSystem` option only
/// (spec 18.1.1 steps 19-20), with the locale's grouping flag.
fn internal_number_format(locale: &str, numbering_system: &str) -> NumberFormatRecord {
    let base = locale.split('-').next().unwrap_or("en");
    NumberFormatRecord {
        locale: locale.to_string(),
        numbering_system: numbering_system.to_string(),
        style: STYLE_DECIMAL,
        currency: None,
        currency_display: 0,
        currency_sign: 0,
        unit: None,
        unit_display: 0,
        minimum_integer_digits: 1,
        minimum_fraction_digits: 0,
        maximum_fraction_digits: 3,
        minimum_significant_digits: 1,
        maximum_significant_digits: 21,
        rounding_type: ROUNDING_FRACTION,
        notation: NOTATION_STANDARD,
        compact_display: DISPLAY_SHORT,
        use_grouping: if plural_data::rtf_min2_grouping(base) {
            GROUPING_MIN2
        } else {
            GROUPING_AUTO
        },
        sign_display: 0,
        rounding_increment: 1,
        rounding_mode: 0,
        computed_rounding_priority: "auto",
        trailing_zero_display: 0,
        bound_format: None,
    }
}

/// The internal PluralRules record (cardinal, standard notation).
fn internal_plural_rules(locale: &str) -> crate::builtins::intl::plural_rules::PluralRulesRecord {
    crate::builtins::intl::plural_rules::PluralRulesRecord {
        ordinal: false,
        number_format: internal_number_format(locale, "latn"),
    }
}

/// PartitionRelativeTimePattern (ECMA-402 §18.5.2): the parts list.
fn partition_relative_time_pattern(
    record: &RelativeTimeFormatRecord,
    value: f64,
    unit: &str,
) -> Result<Vec<(String, String, Option<String>)>, JsError> {
    if !value.is_finite() {
        return Err(range_error("value must be finite"));
    }
    let unit = singular_relative_time_unit(unit)
        .ok_or_else(|| range_error("Invalid relative time unit"))?;
    let base = record.locale.split('-').next().unwrap_or("en");
    // `numeric: "auto"`: the -1/0/1 exception literals, keyed by ToString.
    if record.numeric == NUMERIC_AUTO {
        let value_string = number_to_string(value);
        if let Some(literal) = plural_data::rtf_auto_exception(base, unit, &value_string) {
            return Ok(vec![("literal".to_string(), literal.to_string(), None)]);
        }
    }
    let tense_past = value.is_sign_negative();
    let (future_prefix, past_suffix) = plural_data::rtf_affixes(base);
    let nf = internal_number_format(&record.locale, &record.numbering_system);
    let data = locale_data(&record.locale);
    let x = to_intl_mv(value);
    let (category, _) = crate::builtins::intl::plural_rules::resolve_plural(
        &internal_plural_rules(&record.locale),
        &x,
    );
    let unit_string = plural_data::rtf_unit(base, record.style, unit, category);
    // The number part carries the magnitude only: the tense affix conveys
    // the sign (spec 18.5.2 step 12 formats ℝ(value) into the pattern).
    let number_parts = number_format::partition_number_pattern(&nf, data, &to_intl_mv(value.abs()));
    let mut parts = Vec::new();
    if tense_past {
        // Past: `{0} {unit} ago` / `{0} {unit} temu` — the number comes
        // first, then the unit literal and the suffix.
        for part in &number_parts {
            parts.push((
                part.part_type.to_string(),
                part.value.clone(),
                Some(unit.to_string()),
            ));
        }
        parts.push((
            "literal".to_string(),
            format!(" {unit_string}{past_suffix}"),
            None,
        ));
    } else {
        parts.push(("literal".to_string(), future_prefix.to_string(), None));
        for part in &number_parts {
            parts.push((
                part.part_type.to_string(),
                part.value.clone(),
                Some(unit.to_string()),
            ));
        }
        parts.push(("literal".to_string(), format!(" {unit_string}"), None));
    }
    Ok(parts)
}

/// The IntlMV of a finite double, for the plural selection.
fn to_intl_mv(value: f64) -> IntlMv {
    number_format::parse_string_intl_mv(&value.to_string())
}

/// The spec's `! ToString(value)` for the -0/0/±1 exception keys.
fn number_to_string(value: f64) -> String {
    if value == 0.0 {
        "0".to_string()
    } else if value == value.trunc() {
        format!("{}", value as i64)
    } else {
        value.to_string()
    }
}

/// Install `Intl.RelativeTimeFormat` onto `%Intl%`.
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
        Some(JsString::from_utf8("RelativeTimeFormat")),
        0,
        placeholder("Intl.RelativeTimeFormat"),
        Some(placeholder("Intl.RelativeTimeFormat")),
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
    let methods: &[(&str, &str, u64)] = &[
        ("resolvedOptions", RTF_RESOLVED_OPTIONS, 0),
        ("format", RTF_FORMAT, 2),
        ("formatToParts", RTF_FORMAT_TO_PARTS, 2),
    ];
    for (name, key, length) in methods {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            *length,
            placeholder(name),
            None,
            function_proto.clone(),
        )?;
        realm.intrinsics.define(key, Value::Function(func.clone()));
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
    // %Intl.RelativeTimeFormat.prototype%[@@toStringTag] = "Intl.RelativeTimeFormat".
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8(
                "Intl.RelativeTimeFormat",
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
    realm
        .intrinsics
        .define(RTF_SUPPORTED_LOCALES_OF, Value::Function(supported.clone()));
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
    realm.intrinsics.define(RTF_PROTO, proto_value);
    realm
        .intrinsics
        .define(RELATIVE_TIME_FORMAT, Value::Function(ctor.clone()));
    if let Some(obj) = as_object(intl_value) {
        obj.define_property(
            &JsString::from_utf8("RelativeTimeFormat"),
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
fn rtf_record(agent: &Agent, this: &Value) -> Result<RelativeTimeFormatRecord, JsError> {
    let Some(obj) = as_object(this) else {
        return Err(type_error("Not a RelativeTimeFormat instance"));
    };
    agent
        .intl_rtf_data
        .get(&obj.id())
        .cloned()
        .ok_or_else(|| type_error("Not a RelativeTimeFormat instance"))
}

/// GetPrototypeFromConstructor: the newTarget's `prototype`, falling back to
/// %Intl.RelativeTimeFormat.prototype% of the newTarget's realm.
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
        .get(RTF_PROTO)
        .and_then(|value| as_object(&value))
        .ok_or_else(|| type_error("%Intl.RelativeTimeFormat.prototype% missing"))
}

/// Intl.RelativeTimeFormat (ECMA-402 §18.1.1).
fn initialize(
    agent: &mut Agent,
    locales: &Value,
    options: &Value,
) -> Result<RelativeTimeFormatRecord, JsError> {
    let (locale, numbering_system, options) =
        number_format::resolve_options(agent, locales, options)?;
    let style = number_format::get_option(
        agent,
        &options,
        "style",
        &["long", "short", "narrow"],
        Some("long"),
    )?;
    let numeric = number_format::get_option(
        agent,
        &options,
        "numeric",
        &["always", "auto"],
        Some("always"),
    )?;
    Ok(RelativeTimeFormatRecord {
        locale,
        numbering_system,
        style: match style.as_deref() {
            Some("short") => STYLE_SHORT,
            Some("narrow") => STYLE_NARROW,
            _ => STYLE_LONG,
        },
        numeric: if numeric.as_deref() == Some("auto") {
            NUMERIC_AUTO
        } else {
            NUMERIC_ALWAYS
        },
    })
}

fn create_instance(
    agent: &mut Agent,
    proto: Handle<JsObject>,
    record: RelativeTimeFormatRecord,
) -> Result<Value, JsError> {
    let instance = JsObject::ordinary_object_create(Some(proto));
    agent.intl_rtf_data.insert(instance.id(), record);
    Ok(Value::Object(instance))
}

/// Intl.RelativeTimeFormat.prototype.format (ECMA-402 §18.3.3).
fn format_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let record = rtf_record(agent, this)?;
    let value = to_number(agent, &args.first().cloned().unwrap_or(Value::Undefined))?;
    let unit = crate::context::to_string(agent, &args.get(1).cloned().unwrap_or(Value::Undefined))?
        .to_string_lossy();
    let parts = partition_relative_time_pattern(&record, value, &unit)?;
    let mut result = String::new();
    for (_, value, _) in &parts {
        result.push_str(value);
    }
    Ok(Value::String(Handle::new(JsString::from_utf8(&result))))
}

/// Intl.RelativeTimeFormat.prototype.formatToParts (ECMA-402 §18.3.4).
fn format_to_parts_method(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let record = rtf_record(agent, this)?;
    let value = to_number(agent, &args.first().cloned().unwrap_or(Value::Undefined))?;
    let unit = crate::context::to_string(agent, &args.get(1).cloned().unwrap_or(Value::Undefined))?
        .to_string_lossy();
    let parts = partition_relative_time_pattern(&record, value, &unit)?;
    let object_proto = agent
        .current_realm()?
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let mut array = Vec::new();
    for (part_type, part_value, part_unit) in parts {
        let obj = JsObject::ordinary_object_create(object_proto.clone());
        obj.define_property(
            &JsString::from_utf8("type"),
            &PropertyDescriptor {
                value: Some(Value::String(Handle::new(JsString::from_utf8(&part_type)))),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(true),
                configurable: Some(true),
            },
        )?;
        obj.define_property(
            &JsString::from_utf8("value"),
            &PropertyDescriptor {
                value: Some(Value::String(Handle::new(JsString::from_utf8(&part_value)))),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(true),
                configurable: Some(true),
            },
        )?;
        if let Some(unit) = part_unit {
            obj.define_property(
                &JsString::from_utf8("unit"),
                &PropertyDescriptor {
                    value: Some(Value::String(Handle::new(JsString::from_utf8(&unit)))),
                    writable: Some(true),
                    get: None,
                    set: None,
                    enumerable: Some(true),
                    configurable: Some(true),
                },
            )?;
        }
        array.push(Value::Object(obj));
    }
    crate::builtins::array::array_from_values(agent, &array)
}

/// Intl.RelativeTimeFormat.prototype.resolvedOptions (ECMA-402 §18.3.2).
fn resolved_options_method(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let record = rtf_record(agent, this)?;
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
            STYLE_SHORT => "short",
            STYLE_NARROW => "narrow",
            _ => "long",
        }),
    )?;
    define(
        "numeric",
        str(if record.numeric == NUMERIC_AUTO {
            "auto"
        } else {
            "always"
        }),
    )?;
    define("numberingSystem", str(&record.numbering_system))?;
    Ok(Value::Object(options))
}

/// Intl.RelativeTimeFormat.supportedLocalesOf (ECMA-402 §18.2.2).
fn supported_locales_of(
    agent: &mut Agent,
    locales: Value,
    options: Value,
) -> Result<Value, JsError> {
    let requested = crate::builtins::intl::canonicalize_locale_list(agent, &locales)?;
    let options = number_format::coerce_options_to_object(agent, &options)?;
    number_format::get_option(
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

/// dispatch_call: the RelativeTimeFormat constructor (as a function —
/// throws), the prototype members, and supportedLocalesOf.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(RELATIVE_TIME_FORMAT).as_ref() == Some(callee) {
        // NewTarget is undefined → the spec throws a TypeError.
        return Some(Err(type_error("Intl.RelativeTimeFormat requires 'new'")));
    }
    if intrinsics.get(RTF_SUPPORTED_LOCALES_OF).as_ref() == Some(callee) {
        return Some(supported_locales_of(
            agent,
            args.first().cloned().unwrap_or(Value::Undefined),
            args.get(1).cloned().unwrap_or(Value::Undefined),
        ));
    }
    if intrinsics.get(RTF_RESOLVED_OPTIONS).as_ref() == Some(callee) {
        return Some(resolved_options_method(agent, this));
    }
    if intrinsics.get(RTF_FORMAT).as_ref() == Some(callee) {
        return Some(format_method(agent, this, args));
    }
    if intrinsics.get(RTF_FORMAT_TO_PARTS).as_ref() == Some(callee) {
        return Some(format_to_parts_method(agent, this, args));
    }
    None
}

/// dispatch_construct: `new Intl.RelativeTimeFormat(...)`.
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    if realm.intrinsics.get(RELATIVE_TIME_FORMAT).as_ref() == Some(callee) {
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
