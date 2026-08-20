//! `Intl.DurationFormat` (ECMA-402 §13): the constructor (style/
//! fractionalDigits/per-unit style+display options), `format`/`formatToParts`
//! (the partition algorithm composing `Intl.NumberFormat` unit style and
//! `Intl.ListFormat` unit type), and `resolvedOptions`. The duration record
//! comes from a Temporal.Duration object's internal slots, a duration string,
//! or a plain property bag (ECMA-402 §13.5.3 ToDurationRecord). Instances
//! store their record in the agent's `intl_duration_format_data` map.

use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use crate::agent::Agent;
use crate::builtins::intl::list_format::{self, ListFormatRecord, TYPE_UNIT};
use crate::builtins::intl::number_data;
use crate::builtins::intl::number_format::{
    IntlMv, get_number_option, get_option, is_type_identifier, parse_string_intl_mv,
};
use crate::context::{as_object, get_property, to_string};
use crate::realm::Realm;

pub const DURATION_FORMAT: &str = "%Intl.DurationFormat%";
pub const DURATION_FORMAT_PROTO: &str = "%Intl.DurationFormat.prototype%";
const DF_RESOLVED_OPTIONS: &str = "%Intl.DurationFormat.prototype.resolvedOptions%";
const DF_FORMAT: &str = "%Intl.DurationFormat.prototype.format%";
const DF_FORMAT_TO_PARTS: &str = "%Intl.DurationFormat.prototype.formatToParts%";
const DF_SUPPORTED_LOCALES_OF: &str = "%Intl.DurationFormat.supportedLocalesOf%";

/// The units in table order (Table 20/24): the plural Temporal unit names.
const UNITS: [&str; 10] = [
    "years",
    "months",
    "weeks",
    "days",
    "hours",
    "minutes",
    "seconds",
    "milliseconds",
    "microseconds",
    "nanoseconds",
];

/// The ToDurationRecord property-read order (alphabetical): (key, field
/// index into the UNITS array).
const FIELD_ORDER: [(&str, usize); 10] = [
    ("days", 3),
    ("hours", 4),
    ("microseconds", 8),
    ("milliseconds", 7),
    ("minutes", 5),
    ("months", 1),
    ("nanoseconds", 9),
    ("seconds", 6),
    ("weeks", 2),
    ("years", 0),
];

fn type_error(message: &str) -> JsError {
    JsError::new(ErrorKind::TypeError, message.into())
}

fn range_error(message: &str) -> JsError {
    JsError::new(ErrorKind::RangeError, message.into())
}

/// A Duration Unit Options Record (ECMA-402 §13.5.6.1).
#[derive(Debug, Clone, Default)]
pub struct DurationUnitOptions {
    pub style: String,
    pub display: String,
}

/// The [[InitializedDurationFormat]] record.
#[derive(Debug, Clone)]
pub struct DurationFormatRecord {
    pub locale: String,
    pub numbering_system: String,
    pub style: String,
    pub units: [DurationUnitOptions; 10],
    pub fractional_digits: Option<u32>,
}

/// Intl.DurationFormat (ECMA-402 §13.1.1): ResolveOptions (locale +
/// numberingSystem), the style option, the per-unit GetDurationUnitOptions,
/// and fractionalDigits.
fn initialize(
    agent: &mut Agent,
    locales: &Value,
    options: &Value,
) -> Result<DurationFormatRecord, JsError> {
    let requested = crate::builtins::intl::canonicalize_locale_list(agent, locales)?;
    // ResolveOptions: GetOptionsObject (null and primitives throw).
    let options = get_options_object(agent, options)?;
    get_option(
        agent,
        &options,
        "localeMatcher",
        &["lookup", "best fit"],
        Some("best fit"),
    )?;
    let numbering_system = get_option(agent, &options, "numberingSystem", &[], None)?;
    if let Some(value) = &numbering_system
        && !is_type_identifier(value)
    {
        return Err(range_error(
            "Value cannot be matched by the type Unicode locale nonterminal",
        ));
    }
    let (locale, numbering_system) =
        number_format_resolve_locale(agent, &requested, numbering_system.as_deref())?;
    let style = get_option(
        agent,
        &options,
        "style",
        &["long", "short", "narrow", "digital"],
        Some("short"),
    )?
    .unwrap_or_else(|| "short".to_string());
    let mut units = std::array::from_fn(|_| DurationUnitOptions::default());
    let mut prev_style = String::new();
    for (index, unit) in UNITS.iter().enumerate() {
        let unit_options = get_duration_unit_options(agent, &options, unit, &style, &prev_style)?;
        prev_style = unit_options.style.clone();
        units[index] = unit_options;
    }
    let fractional_digits =
        get_number_option(agent, &options, "fractionalDigits", 0.0, 9.0, f64::NAN)?;
    let fractional_digits = if fractional_digits.is_nan() {
        None
    } else {
        Some(fractional_digits as u32)
    };
    Ok(DurationFormatRecord {
        locale,
        numbering_system,
        style,
        units,
        fractional_digits,
    })
}

fn number_format_resolve_locale(
    agent: &mut Agent,
    requested: &[String],
    numbering_system: Option<&str>,
) -> Result<(String, String), JsError> {
    crate::builtins::intl::number_format::resolve_locale(agent, requested, numbering_system)
}

/// GetDurationUnitOptions (ECMA-402 §13.5.6): the per-unit style/display,
/// the digital and numeric-prevStyle defaults, and the validation.
fn get_duration_unit_options(
    agent: &mut Agent,
    options: &Value,
    unit: &str,
    base_style: &str,
    prev_style: &str,
) -> Result<DurationUnitOptions, JsError> {
    let styles: &[&str] = match unit {
        "years" | "months" | "weeks" | "days" => &["long", "short", "narrow"],
        "hours" | "minutes" | "seconds" => &["long", "short", "narrow", "numeric", "2-digit"],
        _ => &["long", "short", "narrow", "numeric"],
    };
    let digital_base = if matches!(unit, "years" | "months" | "weeks" | "days") {
        "short"
    } else {
        "numeric"
    };
    let mut style = get_option(agent, options, unit, styles, None)?;
    let mut display_default = "always";
    if style.is_none() {
        if base_style == "digital" {
            style = Some(digital_base.to_string());
            if !matches!(unit, "hours" | "minutes" | "seconds") {
                display_default = "auto";
            }
        } else if matches!(prev_style, "fractional" | "numeric" | "2-digit") {
            style = Some("numeric".to_string());
            if !matches!(unit, "minutes" | "seconds") {
                display_default = "auto";
            }
        } else {
            style = Some(base_style.to_string());
            display_default = "auto";
        }
    }
    // A "numeric" style on a sub-second unit is the "fractional" style.
    if style.as_deref() == Some("numeric") && is_fractional_second_unit(unit) {
        style = Some("fractional".to_string());
        display_default = "auto";
    }
    let display_field = format!("{unit}Display");
    let display = get_option(
        agent,
        options,
        &display_field,
        &["auto", "always"],
        Some(display_default),
    )?;
    let mut style = style.unwrap_or_else(|| "short".to_string());
    let display = display.unwrap_or_else(|| "always".to_string());
    // ValidateDurationUnitStyle (ECMA-402 §13.5.6.2).
    if display == "always" && style == "fractional" {
        return Err(range_error(
            "fractional style cannot be displayed with \"always\"",
        ));
    }
    if prev_style == "fractional" && style != "fractional" {
        return Err(range_error(
            "a fractional-styled unit must be followed by fractional units",
        ));
    }
    if matches!(prev_style, "numeric" | "2-digit")
        && !matches!(style.as_str(), "fractional" | "numeric" | "2-digit")
    {
        return Err(range_error(
            "a numeric-styled unit must be followed by numeric units",
        ));
    }
    // The corpus locales have [[TwoDigitHours]] = false (en digital hours
    // stay numeric); minutes/seconds after a numeric unit are 2-digit.
    if matches!(unit, "minutes" | "seconds") && matches!(prev_style, "numeric" | "2-digit") {
        style = "2-digit".to_string();
    }
    Ok(DurationUnitOptions { style, display })
}

fn is_fractional_second_unit(unit: &str) -> bool {
    matches!(unit, "milliseconds" | "microseconds" | "nanoseconds")
}

/// GetOptionsObject (ECMA-402 §9.2.10): undefined → a null-prototype
/// object; objects pass through; everything else throws.
fn get_options_object(_agent: &mut Agent, options: &Value) -> Result<Value, JsError> {
    if options.is_undefined() {
        Ok(Value::Object(JsObject::ordinary_object_create(None)))
    } else if as_object(options).is_some() {
        Ok(options.clone())
    } else {
        Err(type_error("Options must be an object"))
    }
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
        Some(JsString::from_utf8("DurationFormat")),
        0,
        placeholder("Intl.DurationFormat"),
        Some(placeholder("Intl.DurationFormat")),
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
    let methods: [(&str, &str, u64); 3] = [
        ("resolvedOptions", DF_RESOLVED_OPTIONS, 0),
        ("format", DF_FORMAT, 1),
        ("formatToParts", DF_FORMAT_TO_PARTS, 1),
    ];
    for (name, key, length) in methods {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
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
    // %Intl.DurationFormat.prototype%[@@toStringTag] = "Intl.DurationFormat".
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8(
                "Intl.DurationFormat",
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
        .define(DF_SUPPORTED_LOCALES_OF, Value::Function(supported.clone()));
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
    realm.intrinsics.define(DURATION_FORMAT_PROTO, proto_value);
    realm
        .intrinsics
        .define(DURATION_FORMAT, Value::Function(ctor.clone()));
    if let Some(obj) = as_object(intl_value) {
        obj.define_property(
            &JsString::from_utf8("DurationFormat"),
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
fn duration_format_record(agent: &Agent, this: &Value) -> Result<DurationFormatRecord, JsError> {
    let Some(obj) = as_object(this) else {
        return Err(type_error("Not a DurationFormat instance"));
    };
    agent
        .intl_duration_format_data
        .get(&obj.id())
        .cloned()
        .ok_or_else(|| type_error("Not a DurationFormat instance"))
}

/// GetPrototypeFromConstructor: the newTarget's `prototype`, falling back to
/// %Intl.DurationFormat.prototype% of the newTarget's realm.
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
        .get(DURATION_FORMAT_PROTO)
        .and_then(|value| as_object(&value))
        .ok_or_else(|| type_error("%Intl.DurationFormat.prototype% missing"))
}

fn create_instance(
    agent: &mut Agent,
    proto: Handle<JsObject>,
    record: DurationFormatRecord,
) -> Result<Value, JsError> {
    let instance = JsObject::ordinary_object_create(Some(proto));
    agent
        .intl_duration_format_data
        .insert(instance.id(), record);
    Ok(Value::Object(instance))
}

/// ToDurationRecord (ECMA-402 §13.5.3): a Temporal.Duration's internal
/// slots, a duration string, or a plain property bag.
fn to_duration_record(agent: &mut Agent, input: &Value) -> Result<[f64; 10], JsError> {
    // A Temporal.Duration reads its internal slots — its prototype getters
    // are never called (taint-temporal-duration-prototype.js).
    if let ValueKind::Object(obj) = input.kind()
        && let Some(crate::builtins::temporal::TemporalRecord::Duration(fields)) =
            agent.temporal_data.get(&obj.id())
    {
        return Ok(*fields);
    }
    if !matches!(input.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
        if matches!(input.kind(), ValueKind::String(_)) {
            let text = to_string(agent, input)?;
            return crate::builtins::temporal::parse_duration_text(&text);
        }
        return Err(type_error("value must be a Duration, string, or object"));
    }
    // A property bag: fields default to 0 and read in alphabetical order;
    // all-undefined throws.
    let mut fields = [0f64; 10];
    let mut any = false;
    for (key, index) in FIELD_ORDER {
        let value = get_property(agent, input, &JsString::from_utf8(key), input.clone())?;
        if !value.is_undefined() {
            any = true;
            fields[index] = to_integer_if_integral(agent, &value)?;
        }
    }
    if !any {
        return Err(type_error("at least one duration field is required"));
    }
    if !crate::builtins::temporal::is_valid_duration(&fields) {
        return Err(range_error("invalid duration"));
    }
    Ok(fields)
}

/// ToIntegerIfIntegral (ECMA-402 §13.5.3): NaN and ±0 become 0; a
/// non-integral or infinite value throws.
fn to_integer_if_integral(agent: &mut Agent, value: &Value) -> Result<f64, JsError> {
    let number = crate::context::to_number(agent, value)?;
    if number.is_nan() || number == 0.0 {
        return Ok(0.0);
    }
    if !number.is_finite() || number.fract() != 0.0 {
        return Err(range_error("value must be an integral Number"));
    }
    Ok(number)
}

/// DurationSign (ECMA-402 §13.5.4): the sign of the most significant
/// non-zero field.
fn duration_sign(fields: &[f64; 10]) -> i64 {
    crate::builtins::temporal::duration_sign(fields)
}

/// Intl.DurationFormat.prototype.format (ECMA-402 §13.3.3).
fn format_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let record = duration_format_record(agent, this)?;
    let fields = to_duration_record(agent, &args.first().cloned().unwrap_or(Value::Undefined))?;
    let parts = partition_duration_format_pattern(agent, &record, &fields)?;
    let text: String = parts.iter().map(|part| part.value.as_str()).collect();
    Ok(Value::String(Handle::new(JsString::from_utf8(&text))))
}

/// Intl.DurationFormat.prototype.formatToParts (ECMA-402 §13.3.4).
fn format_to_parts_method(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let record = duration_format_record(agent, this)?;
    let fields = to_duration_record(agent, &args.first().cloned().unwrap_or(Value::Undefined))?;
    let parts = partition_duration_format_pattern(agent, &record, &fields)?;
    let object_proto = agent
        .current_realm()?
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let mut array = Vec::new();
    for part in parts {
        let obj = JsObject::ordinary_object_create(object_proto.clone());
        let define = |name: &str, value: Value| -> Result<(), JsError> {
            obj.define_property(
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
        define(
            "type",
            Value::String(Handle::new(JsString::from_utf8(part.part_type))),
        )?;
        define(
            "value",
            Value::String(Handle::new(JsString::from_utf8(&part.value))),
        )?;
        if let Some(unit) = &part.unit {
            define(
                "unit",
                Value::String(Handle::new(JsString::from_utf8(unit))),
            )?;
        }
        array.push(Value::Object(obj));
    }
    crate::builtins::array::array_from_values(agent, &array)
}

/// One formatted part: type/value plus the unit on number parts.
#[derive(Debug, Clone)]
struct Part {
    part_type: &'static str,
    value: String,
    unit: Option<String>,
}

/// PartitionDurationFormatPattern (ECMA-402 §13.5.15).
fn partition_duration_format_pattern(
    agent: &mut Agent,
    record: &DurationFormatRecord,
    fields: &[f64; 10],
) -> Result<Vec<Part>, JsError> {
    let mut result: Vec<Vec<Part>> = Vec::new();
    let mut sign_displayed = true;
    for (index, unit) in UNITS.iter().enumerate() {
        let unit_options = &record.units[index];
        let style = unit_options.style.as_str();
        let display = unit_options.display.as_str();
        let number_format_unit = unit[..unit.len() - 1].to_string();
        if matches!(style, "numeric" | "2-digit") {
            let numeric = format_numeric_units(agent, record, fields, unit, &mut sign_displayed)?;
            if !numeric.is_empty() {
                result.push(numeric);
            }
            break;
        }
        // A non-numeric unit (long/short/narrow): the unit that precedes a
        // fractional run absorbs it as fractional digits.
        let mut value = fields[index];
        let mut exact: Option<String> = None;
        if next_unit_fractional(record, index) {
            value += compute_fractional_digits(record, fields);
            exact = Some(combined_value_string(fields, index));
        }
        let absorbs_fractional = exact.is_some();
        if display == "always" || value != 0.0 {
            let negative_zero = sign_displayed && value == 0.0 && duration_sign(fields) == -1;
            let parts = format_non_numeric_unit(
                agent,
                record,
                &number_format_unit,
                style,
                exact,
                value,
                negative_zero,
                &mut sign_displayed,
            )?;
            result.push(parts);
        }
        if absorbs_fractional {
            break;
        }
    }
    list_format_parts(record, result)
}

/// Whether the next smaller unit uses the "fractional" style.
fn next_unit_fractional(record: &DurationFormatRecord, index: usize) -> bool {
    match index {
        6 => record.units[7].style == "fractional",
        7 => record.units[8].style == "fractional",
        8 => record.units[9].style == "fractional",
        _ => false,
    }
}

/// ComputeFractionalDigits (ECMA-402 §13.5.7): the fractional units after
/// `index` as a fraction of the unit at `index` (IEEE, for the display check).
fn compute_fractional_digits(record: &DurationFormatRecord, fields: &[f64; 10]) -> f64 {
    let mut result = 0.0;
    let mut exponent = 3.0;
    for (unit, &value) in record.units[7..10].iter().zip(&fields[7..10]) {
        if unit.style == "fractional" {
            result += value / 10f64.powf(exponent);
            exponent += 3.0;
        }
    }
    result
}

/// The exact decimal string of `fields[index]` plus the fractional units
/// after it: seconds (index 6) as total-ns/1e9, milliseconds /1e6,
/// microseconds /1e3. BigInt-free i128 arithmetic (the fields are bounded by
/// IsValidDuration, so the totals fit).
fn combined_value_string(fields: &[f64; 10], index: usize) -> String {
    let (exponent, total) = match index {
        6 => (
            9i128,
            fields[6] as i128 * 1_000_000_000
                + fields[7] as i128 * 1_000_000
                + fields[8] as i128 * 1_000
                + fields[9] as i128,
        ),
        7 => (
            6i128,
            fields[7] as i128 * 1_000_000 + fields[8] as i128 * 1_000 + fields[9] as i128,
        ),
        _ => (3i128, fields[8] as i128 * 1_000 + fields[9] as i128),
    };
    let divisor = 10i128.pow(exponent as u32);
    let integer = total / divisor;
    let fraction = (total % divisor).abs();
    let mut text = fraction.to_string();
    while text.len() < exponent as usize {
        text.insert(0, '0');
    }
    format!("{integer}.{text}")
}

/// The exact seconds string for the numeric path (seconds + all fractional
/// units), or the plain integer when there are no sub-seconds.
fn numeric_seconds_string(record: &DurationFormatRecord, fields: &[f64; 10]) -> String {
    if record.units[7].style == "fractional"
        || record.units[8].style == "fractional"
        || record.units[9].style == "fractional"
    {
        combined_value_string(fields, 6)
    } else {
        (fields[6] as i128).to_string()
    }
}

/// The exact decimal string of a plain integral field.
fn integer_string(value: f64) -> String {
    (value as i128).to_string()
}

/// FormatNumericUnits (ECMA-402 §13.5.12): the hours:minutes:seconds run.
fn format_numeric_units(
    agent: &mut Agent,
    record: &DurationFormatRecord,
    fields: &[f64; 10],
    first_numeric_unit: &str,
    sign_displayed: &mut bool,
) -> Result<Vec<Part>, JsError> {
    let hours_value = fields[4];
    let hours_display = &record.units[4].display;
    let minutes_value = fields[5];
    let minutes_display = &record.units[5].display;
    let mut seconds_value = fields[6];
    if fields[7] != 0.0 || fields[8] != 0.0 || fields[9] != 0.0 {
        seconds_value += compute_fractional_digits(record, fields);
    }
    let seconds_display = &record.units[6].display;
    let seconds_formatted = seconds_value != 0.0 || seconds_display == "always";
    let hours_formatted =
        first_numeric_unit == "hours" && (hours_value != 0.0 || hours_display == "always");
    let mut minutes_formatted = false;
    if matches!(first_numeric_unit, "hours" | "minutes") {
        minutes_formatted = (hours_formatted && seconds_formatted)
            || minutes_value != 0.0
            || minutes_display == "always";
    }
    let sign = duration_sign(fields);
    let mut parts = Vec::new();
    if hours_formatted {
        let negative_zero = *sign_displayed && hours_value == 0.0 && sign == -1;
        parts.extend(format_numeric_hour(
            agent,
            record,
            hours_value,
            negative_zero,
            *sign_displayed,
        )?);
        *sign_displayed = false;
    }
    if minutes_formatted {
        let negative_zero = *sign_displayed && minutes_value == 0.0 && sign == -1;
        parts.extend(format_numeric_minute(
            agent,
            record,
            minutes_value,
            negative_zero,
            hours_formatted,
            *sign_displayed,
        )?);
        *sign_displayed = false;
    }
    if seconds_formatted {
        let seconds = numeric_seconds_string(record, fields);
        parts.extend(format_numeric_second(
            agent,
            record,
            &seconds,
            minutes_formatted,
            *sign_displayed,
        )?);
    }
    Ok(parts)
}

/// FormatNumericHours (ECMA-402 §13.5.9).
fn format_numeric_hour(
    agent: &mut Agent,
    record: &DurationFormatRecord,
    value: f64,
    negative_zero: bool,
    sign_displayed: bool,
) -> Result<Vec<Part>, JsError> {
    let mut opts: Vec<(&str, Value)> = vec![("useGrouping", Value::Boolean(false))];
    if record.units[4].style == "2-digit" {
        opts.push(("minimumIntegerDigits", Value::Number(2.0)));
    }
    let x = if negative_zero {
        IntlMv::NegZero
    } else {
        parse_string_intl_mv(&integer_string(value))
    };
    number_format_parts(agent, record, &opts, &x, "hour", sign_displayed)
}

/// FormatNumericMinutes (ECMA-402 §13.5.10).
fn format_numeric_minute(
    agent: &mut Agent,
    record: &DurationFormatRecord,
    value: f64,
    negative_zero: bool,
    hours_formatted: bool,
    sign_displayed: bool,
) -> Result<Vec<Part>, JsError> {
    let mut parts = Vec::new();
    if hours_formatted {
        parts.push(Part {
            part_type: "literal",
            value: ":".to_string(),
            unit: None,
        });
    }
    let mut opts: Vec<(&str, Value)> = vec![("useGrouping", Value::Boolean(false))];
    if record.units[5].style == "2-digit" {
        opts.push(("minimumIntegerDigits", Value::Number(2.0)));
    }
    let x = if negative_zero {
        IntlMv::NegZero
    } else {
        parse_string_intl_mv(&integer_string(value))
    };
    parts.extend(number_format_parts(
        agent,
        record,
        &opts,
        &x,
        "minute",
        sign_displayed,
    )?);
    Ok(parts)
}

/// FormatNumericSeconds (ECMA-402 §13.5.11).
fn format_numeric_second(
    agent: &mut Agent,
    record: &DurationFormatRecord,
    seconds: &str,
    minutes_formatted: bool,
    sign_displayed: bool,
) -> Result<Vec<Part>, JsError> {
    let mut parts = Vec::new();
    if minutes_formatted {
        parts.push(Part {
            part_type: "literal",
            value: ":".to_string(),
            unit: None,
        });
    }
    let mut opts: Vec<(&str, Value)> = vec![("useGrouping", Value::Boolean(false))];
    if record.units[6].style == "2-digit" {
        opts.push(("minimumIntegerDigits", Value::Number(2.0)));
    }
    let (minimum, maximum) = match record.fractional_digits {
        Some(digits) => (digits as f64, digits as f64),
        None => (0.0, 9.0),
    };
    opts.push(("minimumFractionDigits", Value::Number(minimum)));
    opts.push(("maximumFractionDigits", Value::Number(maximum)));
    opts.push(("roundingMode", str_value("trunc")));
    let x = parse_string_intl_mv(seconds);
    parts.extend(number_format_parts(
        agent,
        record,
        &opts,
        &x,
        "second",
        sign_displayed,
    )?);
    Ok(parts)
}

fn str_value(text: &str) -> Value {
    Value::String(Handle::new(JsString::from_utf8(text)))
}

/// The non-numeric (unit-style) formatting: NumberFormat with
/// style/unit/unitDisplay, plus the fractional-digit options when the unit
/// absorbs a fractional run.
#[allow(clippy::too_many_arguments)]
fn format_non_numeric_unit(
    agent: &mut Agent,
    record: &DurationFormatRecord,
    number_format_unit: &str,
    style: &str,
    exact: Option<String>,
    value: f64,
    negative_zero: bool,
    sign_displayed: &mut bool,
) -> Result<Vec<Part>, JsError> {
    let mut opts: Vec<(&str, Value)> = Vec::new();
    if exact.is_some() {
        let (minimum, maximum) = match record.fractional_digits {
            Some(digits) => (digits as f64, digits as f64),
            None => (0.0, 9.0),
        };
        opts.push(("minimumFractionDigits", Value::Number(minimum)));
        opts.push(("maximumFractionDigits", Value::Number(maximum)));
        opts.push(("roundingMode", str_value("trunc")));
    }
    opts.push(("style", str_value("unit")));
    opts.push(("unit", str_value(number_format_unit)));
    opts.push(("unitDisplay", str_value(style)));
    let x = if negative_zero {
        IntlMv::NegZero
    } else if let Some(text) = &exact {
        parse_string_intl_mv(text)
    } else {
        parse_string_intl_mv(&integer_string(value))
    };
    let parts = number_format_parts(
        agent,
        record,
        &opts,
        &x,
        number_format_unit,
        *sign_displayed,
    )?;
    if *sign_displayed {
        *sign_displayed = false;
    }
    Ok(parts)
}

/// Construct a NumberFormat for the locale with `opts` (plus the record's
/// numberingSystem and the signDisplay when the sign is already shown) and
/// partition `x`, tagging every part with `unit`.
fn number_format_parts(
    agent: &mut Agent,
    record: &DurationFormatRecord,
    opts: &[(&str, Value)],
    x: &IntlMv,
    unit: &str,
    sign_displayed: bool,
) -> Result<Vec<Part>, JsError> {
    let options = JsObject::ordinary_object_create(None);
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
    define("numberingSystem", str_value(&record.numbering_system))?;
    if !sign_displayed {
        define("signDisplay", str_value("never"))?;
    }
    for (name, value) in opts {
        define(name, value.clone())?;
    }
    let nf_record = crate::builtins::intl::number_format::initialize(
        agent,
        &Value::String(Handle::new(JsString::from_utf8(&record.locale))),
        &Value::Object(options),
    )?;
    let data = number_data::locale_data(&nf_record.locale);
    let parts = crate::builtins::intl::number_format::partition_number_pattern(&nf_record, data, x);
    Ok(parts
        .into_iter()
        .map(|part| Part {
            part_type: part.part_type,
            value: part.value,
            unit: Some(unit.to_string()),
        })
        .collect())
}

/// ListFormatParts (ECMA-402 §13.5.14): join the unit lists with
/// `ListFormat` (type unit, the record's style — digital becomes short) and
/// substitute the element parts back with their original parts.
fn list_format_parts(
    record: &DurationFormatRecord,
    result: Vec<Vec<Part>>,
) -> Result<Vec<Part>, JsError> {
    if result.is_empty() {
        return Ok(Vec::new());
    }
    let lf_record = ListFormatRecord {
        locale: record.locale.clone(),
        type_value: TYPE_UNIT,
        style: match record.style.as_str() {
            "narrow" => list_format::STYLE_NARROW,
            "long" => list_format::STYLE_LONG,
            _ => list_format::STYLE_SHORT,
        },
    };
    let strings: Vec<String> = result
        .iter()
        .map(|parts| parts.iter().map(|part| part.value.clone()).collect())
        .collect();
    let list_parts = list_format::create_parts_from_list(&lf_record, &strings);
    let mut flattened = Vec::new();
    let mut index = 0;
    for (part_type, value) in list_parts {
        if part_type == "element" {
            flattened.extend(result[index].iter().cloned());
            index += 1;
        } else {
            flattened.push(Part {
                part_type: "literal",
                value,
                unit: None,
            });
        }
    }
    Ok(flattened)
}

/// Intl.DurationFormat.prototype.resolvedOptions (ECMA-402 §13.3.2).
fn resolved_options_method(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let record = duration_format_record(agent, this)?;
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
    define("numberingSystem", str(&record.numbering_system))?;
    define("style", str(&record.style))?;
    for (index, unit) in UNITS.iter().enumerate() {
        // The "fractional" style is reported as "numeric".
        let style = if record.units[index].style == "fractional" {
            "numeric"
        } else {
            &record.units[index].style
        };
        define(unit, str(style))?;
        let display_field = format!("{unit}Display");
        define(&display_field, str(&record.units[index].display))?;
    }
    if let Some(digits) = record.fractional_digits {
        define("fractionalDigits", Value::Number(digits as f64))?;
    }
    Ok(Value::Object(options))
}

/// Intl.DurationFormat.supportedLocalesOf (ECMA-402 §13.2.2).
fn supported_locales_of(
    agent: &mut Agent,
    locales: Value,
    options: Value,
) -> Result<Value, JsError> {
    let requested = crate::builtins::intl::canonicalize_locale_list(agent, &locales)?;
    // SupportedLocales: non-undefined options are coerced with ToObject
    // (unlike the constructor's GetOptionsObject).
    let options = crate::builtins::intl::number_format::coerce_options_to_object(agent, &options)?;
    get_option(
        agent,
        &options,
        "localeMatcher",
        &["lookup", "best fit"],
        Some("best fit"),
    )?;
    let available = number_data::NUMBER_FORMAT_LOCALES;
    let mut subset = Vec::new();
    for locale in &requested {
        let base = crate::builtins::intl::number_format::strip_unicode_extension(locale);
        if crate::builtins::intl::number_format::best_fit(available, &base).is_some() {
            subset.push(Value::String(Handle::new(JsString::from_utf8(locale))));
        }
    }
    crate::builtins::array::array_from_values(agent, &subset)
}

/// Temporal.Duration.prototype.toLocaleString: format the fields with a
/// fresh Intl.DurationFormat.
pub fn format_duration_fields(
    agent: &mut Agent,
    locales: &Value,
    options: &Value,
    fields: &[f64; 10],
) -> Result<String, JsError> {
    let record = initialize(agent, locales, options)?;
    let parts = partition_duration_format_pattern(agent, &record, fields)?;
    Ok(parts.iter().map(|part| part.value.as_str()).collect())
}

/// dispatch_call: the DurationFormat constructor (as a function — throws),
/// the prototype members, and supportedLocalesOf.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(DURATION_FORMAT).as_ref() == Some(callee) {
        return Some(Err(type_error("Intl.DurationFormat requires 'new'")));
    }
    if intrinsics.get(DF_SUPPORTED_LOCALES_OF).as_ref() == Some(callee) {
        return Some(supported_locales_of(
            agent,
            args.first().cloned().unwrap_or(Value::Undefined),
            args.get(1).cloned().unwrap_or(Value::Undefined),
        ));
    }
    if intrinsics.get(DF_RESOLVED_OPTIONS).as_ref() == Some(callee) {
        return Some(resolved_options_method(agent, this));
    }
    if intrinsics.get(DF_FORMAT).as_ref() == Some(callee) {
        return Some(format_method(agent, this, args));
    }
    if intrinsics.get(DF_FORMAT_TO_PARTS).as_ref() == Some(callee) {
        return Some(format_to_parts_method(agent, this, args));
    }
    None
}

/// dispatch_construct: `new Intl.DurationFormat(...)`.
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    if realm.intrinsics.get(DURATION_FORMAT).as_ref() == Some(callee) {
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
