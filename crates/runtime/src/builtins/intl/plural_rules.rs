//! `Intl.PluralRules` (ECMA-402 §17): the constructor (type, notation,
//! compactDisplay, and the shared digit options), `select`/`selectRange`
//! through ResolvePlural/ResolvePluralRange, and `resolvedOptions` with the
//! per-locale `pluralCategories` list. The plural-rule data (the CLDR
//! cardinal/ordinal rules, the compact overrides, and the category lists)
//! lives in `plural_data.rs`; the digit-option machinery and
//! FormatNumericToString are reused from `number_format.rs`. Instances store
//! their record in the agent's `intl_plural_rules_data` map.

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
    self, DISPLAY_LONG, DISPLAY_SHORT, IntlMv, NOTATION_COMPACT, NOTATION_ENGINEERING,
    NOTATION_SCIENTIFIC, NOTATION_STANDARD, NumberFormatRecord, ROUNDING_FRACTION,
    ROUNDING_MODE_CEIL, ROUNDING_MODE_EXPAND, ROUNDING_MODE_FLOOR, ROUNDING_MODE_HALF_CEIL,
    ROUNDING_MODE_HALF_EVEN, ROUNDING_MODE_HALF_FLOOR, ROUNDING_MODE_HALF_TRUNC,
    ROUNDING_MODE_TRUNC, ROUNDING_SIGNIFICANT, STYLE_DECIMAL,
};
use crate::builtins::intl::plural_data::{self, PluralCategory};
use crate::context::{as_object, get_property};
use crate::realm::Realm;

pub const PLURAL_RULES: &str = "%Intl.PluralRules%";
pub const PLURAL_RULES_PROTO: &str = "%Intl.PluralRules.prototype%";
pub const PR_SUPPORTED_LOCALES_OF: &str = "%Intl.PluralRules.supportedLocalesOf%";
pub const PR_RESOLVED_OPTIONS: &str = "%Intl.PluralRules.prototype.resolvedOptions%";
pub const PR_SELECT: &str = "%Intl.PluralRules.prototype.select%";
pub const PR_SELECT_RANGE: &str = "%Intl.PluralRules.prototype.selectRange%";

fn range_error(message: &str) -> JsError {
    JsError::new(ErrorKind::RangeError, message.into())
}

fn type_error(message: &str) -> JsError {
    JsError::new(ErrorKind::TypeError, message.into())
}

/// The [[InitializedPluralRules]] record: the plural `type` plus the shared
/// digit/notation options carried in a NumberFormat record (the formatting
/// machinery reads the same fields).
#[derive(Debug, Clone)]
pub struct PluralRulesRecord {
    /// Whether `type` is "ordinal" (else "cardinal").
    pub ordinal: bool,
    pub number_format: NumberFormatRecord,
}

impl PluralRulesRecord {
    fn type_name(&self) -> &'static str {
        if self.ordinal { "ordinal" } else { "cardinal" }
    }
}

/// ResolvePlural (ECMA-402 §17.5.2): the category and formatted string of
/// the Intl mathematical value under the record's locale/type/notation.
pub(crate) fn resolve_plural(record: &PluralRulesRecord, n: &IntlMv) -> (PluralCategory, String) {
    if matches!(n, IntlMv::Nan | IntlMv::PosInf | IntlMv::NegInf) {
        return (PluralCategory::Other, String::new());
    }
    let nf = &record.number_format;
    let (_, formatted) = number_format::format_numeric_to_string(nf, n);
    let base = nf.locale.split('-').next().unwrap_or("en");
    let category = if nf.notation == NOTATION_COMPACT {
        let data = locale_data(&nf.locale);
        let exponent = number_format::compute_exponent(nf, data, n);
        if exponent > 0 {
            match plural_data::compact_category(base, exponent) {
                Some(category) => category,
                None => {
                    plural_data::select_category(base, record.ordinal, &n.scale_pow10(-exponent))
                }
            }
        } else {
            plural_data::select_category(base, record.ordinal, n)
        }
    } else {
        plural_data::select_category(base, record.ordinal, n)
    };
    (category, formatted)
}

/// ResolvePluralRange (ECMA-402 §17.5.4): equal formatted strings return the
/// start category; otherwise the locale's range rule (the default "other").
fn resolve_plural_range(record: &PluralRulesRecord, x: &IntlMv, y: &IntlMv) -> PluralCategory {
    let (x_category, x_string) = resolve_plural(record, x);
    let (_, y_string) = resolve_plural(record, y);
    if x_string == y_string {
        return x_category;
    }
    PluralCategory::Other
}

/// Install `Intl.PluralRules` onto `%Intl%`.
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
        Some(JsString::from_utf8("PluralRules")),
        0,
        placeholder("Intl.PluralRules"),
        Some(placeholder("Intl.PluralRules")),
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
        ("resolvedOptions", PR_RESOLVED_OPTIONS, 0),
        ("select", PR_SELECT, 1),
        ("selectRange", PR_SELECT_RANGE, 2),
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
    // %Intl.PluralRules.prototype%[@@toStringTag] = "Intl.PluralRules".
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8(
                "Intl.PluralRules",
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
        function_proto
    )?;
    realm
        .intrinsics
        .define(PR_SUPPORTED_LOCALES_OF, Value::Function(supported));
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
    realm.intrinsics.define(PLURAL_RULES_PROTO, proto_value);
    realm
        .intrinsics
        .define(PLURAL_RULES, Value::Function(ctor));
    if let Some(obj) = as_object(intl_value) {
        obj.define_property(
            &JsString::from_utf8("PluralRules"),
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
fn plural_rules_record(agent: &Agent, this: &Value) -> Result<PluralRulesRecord, JsError> {
    let Some(obj) = as_object(this) else {
        return Err(type_error("Not a PluralRules instance"));
    };
    agent
        .intl_plural_rules_data
        .get(&obj.id())
        .cloned()
        .ok_or_else(|| type_error("Not a PluralRules instance"))
}

/// GetPrototypeFromConstructor: the newTarget's `prototype`, falling back to
/// %Intl.PluralRules.prototype% of the newTarget's realm (the subclassing
/// path).
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
        .get(PLURAL_RULES_PROTO)
        .and_then(|value| as_object(&value))
        .ok_or_else(|| type_error("%Intl.PluralRules.prototype% missing"))
}

/// Intl.PluralRules (ECMA-402 §17.1.1).
fn initialize(
    agent: &mut Agent,
    locales: &Value,
    options: &Value,
) -> Result<PluralRulesRecord, JsError> {
    let requested = crate::builtins::intl::canonicalize_locale_list(agent, locales)?;
    let options = number_format::coerce_options_to_object(agent, options)?;
    // ResolveOptions: PluralRules has no relevant extension keys, so only the
    // localeMatcher option is read and the u-extension is dropped.
    number_format::get_option(
        agent,
        &options,
        "localeMatcher",
        &["lookup", "best fit"],
        Some("best fit"),
    )?;
    let locale = number_format::resolve_locale_simple(&requested)?;
    let type_value = number_format::get_option(
        agent,
        &options,
        "type",
        &["cardinal", "ordinal"],
        Some("cardinal"),
    )?;
    let notation = number_format::get_option(
        agent,
        &options,
        "notation",
        &["standard", "scientific", "engineering", "compact"],
        Some("standard"),
    )?;
    let compact_display = number_format::get_option(
        agent,
        &options,
        "compactDisplay",
        &["short", "long"],
        Some("short"),
    )?;
    let mut nf = NumberFormatRecord {
        locale: locale.clone(),
        numbering_system: "latn".to_string(),
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
        notation: match notation.as_deref() {
            Some("scientific") => NOTATION_SCIENTIFIC,
            Some("engineering") => NOTATION_ENGINEERING,
            Some("compact") => NOTATION_COMPACT,
            _ => NOTATION_STANDARD,
        },
        compact_display: DISPLAY_SHORT,
        use_grouping: 0,
        sign_display: 0,
        rounding_increment: 1,
        rounding_mode: 0,
        computed_rounding_priority: "auto",
        trailing_zero_display: 0,
        bound_format: None,
    };
    if nf.notation == NOTATION_COMPACT {
        nf.compact_display = if compact_display.as_deref() == Some("long") {
            DISPLAY_LONG
        } else {
            DISPLAY_SHORT
        };
    }
    number_format::set_number_format_digit_options(
        agent,
        &mut nf,
        &options,
        0,
        3,
        notation.as_deref().unwrap_or("standard"),
    )?;
    Ok(PluralRulesRecord {
        ordinal: type_value.as_deref() == Some("ordinal"),
        number_format: nf,
    })
}

fn create_instance(
    agent: &mut Agent,
    proto: Handle<JsObject>,
    record: PluralRulesRecord,
) -> Result<Value, JsError> {
    let instance = JsObject::ordinary_object_create(Some(proto));
    agent.intl_plural_rules_data.insert(instance.id(), record);
    Ok(Value::Object(instance))
}

/// Intl.PluralRules.prototype.select (ECMA-402 §17.3.3).
fn select_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let record = plural_rules_record(agent, this)?;
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    let n = number_format::to_intl_mathematical_value(agent, &value)?;
    let (category, _) = resolve_plural(&record, &n);
    Ok(Value::String(Handle::new(JsString::from_utf8(
        category.name(),
    ))))
}

/// Intl.PluralRules.prototype.selectRange (ECMA-402 §17.3.4).
fn select_range_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let record = plural_rules_record(agent, this)?;
    let start = args.first().cloned().unwrap_or(Value::Undefined);
    let end = args.get(1).cloned().unwrap_or(Value::Undefined);
    if start.is_undefined() || end.is_undefined() {
        return Err(type_error("start or end is undefined"));
    }
    let x = number_format::to_intl_mathematical_value(agent, &start)?;
    let y = number_format::to_intl_mathematical_value(agent, &end)?;
    if matches!(x, IntlMv::Nan) || matches!(y, IntlMv::Nan) {
        return Err(range_error("start or end is NaN"));
    }
    let category = resolve_plural_range(&record, &x, &y);
    Ok(Value::String(Handle::new(JsString::from_utf8(
        category.name(),
    ))))
}

/// Intl.PluralRules.prototype.resolvedOptions (ECMA-402 §17.3.2).
fn resolved_options_method(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let record = plural_rules_record(agent, this)?;
    let nf = &record.number_format;
    let object_proto = agent
        .current_realm()?
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let options = JsObject::ordinary_object_create(object_proto);
    let define = |name: &str, value: Option<Value>| -> Result<(), JsError> {
        if let Some(value) = value {
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
        Ok(())
    };
    let str = |s: &str| Value::String(Handle::new(JsString::from_utf8(s)));
    define("locale", Some(str(&nf.locale)))?;
    define("type", Some(str(record.type_name())))?;
    define(
        "notation",
        Some(str(match nf.notation {
            NOTATION_SCIENTIFIC => "scientific",
            NOTATION_ENGINEERING => "engineering",
            NOTATION_COMPACT => "compact",
            _ => "standard",
        })),
    )?;
    if nf.notation == NOTATION_COMPACT {
        define(
            "compactDisplay",
            Some(str(if nf.compact_display == DISPLAY_LONG {
                "long"
            } else {
                "short"
            })),
        )?;
    }
    define(
        "minimumIntegerDigits",
        Some(Value::Number(nf.minimum_integer_digits as f64)),
    )?;
    if nf.rounding_type != ROUNDING_SIGNIFICANT {
        define(
            "minimumFractionDigits",
            Some(Value::Number(nf.minimum_fraction_digits as f64)),
        )?;
        define(
            "maximumFractionDigits",
            Some(Value::Number(nf.maximum_fraction_digits as f64)),
        )?;
    }
    if nf.rounding_type != ROUNDING_FRACTION {
        define(
            "minimumSignificantDigits",
            Some(Value::Number(nf.minimum_significant_digits as f64)),
        )?;
        define(
            "maximumSignificantDigits",
            Some(Value::Number(nf.maximum_significant_digits as f64)),
        )?;
    }
    let base = nf.locale.split('-').next().unwrap_or("en");
    let categories = plural_data::plural_categories(base, record.ordinal);
    let values: Vec<Value> = categories
        .iter()
        .map(|category| Value::String(Handle::new(JsString::from_utf8(category.name()))))
        .collect();
    define(
        "pluralCategories",
        Some(crate::builtins::array::array_from_values(agent, &values)?),
    )?;
    define(
        "roundingIncrement",
        Some(Value::Number(nf.rounding_increment as f64)),
    )?;
    define(
        "roundingMode",
        Some(str(match nf.rounding_mode {
            ROUNDING_MODE_CEIL => "ceil",
            ROUNDING_MODE_FLOOR => "floor",
            ROUNDING_MODE_EXPAND => "expand",
            ROUNDING_MODE_TRUNC => "trunc",
            ROUNDING_MODE_HALF_CEIL => "halfCeil",
            ROUNDING_MODE_HALF_FLOOR => "halfFloor",
            ROUNDING_MODE_HALF_TRUNC => "halfTrunc",
            ROUNDING_MODE_HALF_EVEN => "halfEven",
            _ => "halfExpand",
        })),
    )?;
    define("roundingPriority", Some(str(nf.computed_rounding_priority)))?;
    define(
        "trailingZeroDisplay",
        Some(str(if nf.trailing_zero_display == 1 {
            "stripIfInteger"
        } else {
            "auto"
        })),
    )?;
    Ok(Value::Object(options))
}

/// Intl.PluralRules.supportedLocalesOf (ECMA-402 §17.2.2).
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

/// dispatch_call: the PluralRules constructor (as a function — throws), the
/// prototype members, and supportedLocalesOf.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(PLURAL_RULES).as_ref() == Some(callee) {
        // NewTarget is undefined → the spec throws a TypeError.
        return Some(Err(type_error("Intl.PluralRules requires 'new'")));
    }
    if intrinsics.get(PR_SUPPORTED_LOCALES_OF).as_ref() == Some(callee) {
        return Some(supported_locales_of(
            agent,
            args.first().cloned().unwrap_or(Value::Undefined),
            args.get(1).cloned().unwrap_or(Value::Undefined),
        ));
    }
    if intrinsics.get(PR_RESOLVED_OPTIONS).as_ref() == Some(callee) {
        return Some(resolved_options_method(agent, this));
    }
    if intrinsics.get(PR_SELECT).as_ref() == Some(callee) {
        return Some(select_method(agent, this, args));
    }
    if intrinsics.get(PR_SELECT_RANGE).as_ref() == Some(callee) {
        return Some(select_range_method(agent, this, args));
    }
    None
}

/// dispatch_construct: `new Intl.PluralRules(...)`.
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    if realm.intrinsics.get(PLURAL_RULES).as_ref() == Some(callee) {
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
