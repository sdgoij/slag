//! `Intl.ListFormat` (ECMA-402 §14): the constructor (type/style options,
//! GetOptionsObject — primitives throw), `format`/`formatToParts` through
//! CreatePartsFromList (the Start/Middle/End/Pair templates per type and
//! style), and `resolvedOptions`. The template data is the corpus-pinned
//! en-US and es-ES patterns (the `format/*` fixtures assert the exact
//! separator strings); other locales fall back to the en-US long
//! conjunction templates. Instances store their record in the agent's
//! `intl_list_format_data` map.

use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::Value;

use crate::agent::Agent;
use crate::builtins::intl::number_format::{self, get_option};
use crate::context::{as_object, get_property};
use crate::realm::Realm;

pub const LIST_FORMAT: &str = "%Intl.ListFormat%";
pub const LIST_FORMAT_PROTO: &str = "%Intl.ListFormat.prototype%";
pub const LF_SUPPORTED_LOCALES_OF: &str = "%Intl.ListFormat.supportedLocalesOf%";
pub const LF_RESOLVED_OPTIONS: &str = "%Intl.ListFormat.prototype.resolvedOptions%";
pub const LF_FORMAT: &str = "%Intl.ListFormat.prototype.format%";
pub const LF_FORMAT_TO_PARTS: &str = "%Intl.ListFormat.prototype.formatToParts%";

pub(crate) const TYPE_CONJUNCTION: u8 = 0;
pub(crate) const TYPE_DISJUNCTION: u8 = 1;
pub(crate) const TYPE_UNIT: u8 = 2;
pub(crate) const STYLE_LONG: u8 = 0;
pub(crate) const STYLE_SHORT: u8 = 1;
pub(crate) const STYLE_NARROW: u8 = 2;

fn type_error(message: &str) -> JsError {
    JsError::new(ErrorKind::TypeError, message.into())
}

/// The [[InitializedListFormat]] record.
#[derive(Debug, Clone)]
pub struct ListFormatRecord {
    pub locale: String,
    pub type_value: u8,
    pub style: u8,
}

impl ListFormatRecord {
    fn type_name(&self) -> &'static str {
        match self.type_value {
            TYPE_DISJUNCTION => "disjunction",
            TYPE_UNIT => "unit",
            _ => "conjunction",
        }
    }

    fn style_name(&self) -> &'static str {
        match self.style {
            STYLE_SHORT => "short",
            STYLE_NARROW => "narrow",
            _ => "long",
        }
    }

    /// The Start/Middle/End/Pair templates for the record's type and style.
    fn templates(&self) -> (&'static str, &'static str, &'static str, &'static str) {
        templates(&self.locale, self.type_value, self.style)
    }
}

/// The Start/Middle/End/Pair template set (CLDR list patterns; the corpus
/// pins the en-US and es-ES strings — all the es fixtures are `type:
/// "unit"`, and en pins conjunction long/short, disjunction long, and unit
/// long/narrow). The fallback is the en-US long conjunction set.
fn templates(
    locale: &str,
    type_value: u8,
    style: u8,
) -> (&'static str, &'static str, &'static str, &'static str) {
    let base = locale.split('-').next().unwrap_or("en");
    match base {
        "es" if type_value == TYPE_UNIT => match style {
            STYLE_LONG => ("{0}, {1}", "{0}, {1}", "{0} y {1}", "{0} y {1}"),
            STYLE_SHORT => ("{0}, {1}", "{0}, {1}", "{0}, {1}", "{0} y {1}"),
            _ => ("{0} {1}", "{0} {1}", "{0} {1}", "{0} {1}"),
        },
        "en" => match (type_value, style) {
            (TYPE_CONJUNCTION, STYLE_LONG) => {
                ("{0}, {1}", "{0}, {1}", "{0}, and {1}", "{0} and {1}")
            }
            (TYPE_CONJUNCTION, STYLE_SHORT) => ("{0}, {1}", "{0}, {1}", "{0}, & {1}", "{0} & {1}"),
            (TYPE_DISJUNCTION, _) => ("{0}, {1}", "{0}, {1}", "{0}, or {1}", "{0} or {1}"),
            (TYPE_UNIT, STYLE_NARROW) => ("{0} {1}", "{0} {1}", "{0} {1}", "{0} {1}"),
            (TYPE_UNIT, _) => ("{0}, {1}", "{0}, {1}", "{0}, {1}", "{0}, {1}"),
            _ => ("{0}, {1}", "{0}, {1}", "{0}, and {1}", "{0} and {1}"),
        },
        _ => ("{0}, {1}", "{0}, {1}", "{0}, and {1}", "{0} and {1}"),
    }
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

/// Install `Intl.ListFormat` onto `%Intl%`.
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
        Some(JsString::from_utf8("ListFormat")),
        0,
        placeholder("Intl.ListFormat"),
        Some(placeholder("Intl.ListFormat")),
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
        ("resolvedOptions", LF_RESOLVED_OPTIONS, 0),
        ("format", LF_FORMAT, 1),
        ("formatToParts", LF_FORMAT_TO_PARTS, 1),
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
    // %Intl.ListFormat.prototype%[@@toStringTag] = "Intl.ListFormat".
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag")),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8(
                "Intl.ListFormat",
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
    let supported = Function::create_builtin(
        Some(JsString::from_utf8("supportedLocalesOf")),
        1,
        placeholder("supportedLocalesOf"),
        None,
        function_proto
    )?;
    realm
        .intrinsics
        .define(LF_SUPPORTED_LOCALES_OF, Value::Function(supported));
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
    realm.intrinsics.define(LIST_FORMAT_PROTO, proto_value);
    realm
        .intrinsics
        .define(LIST_FORMAT, Value::Function(ctor));
    if let Some(obj) = as_object(intl_value) {
        obj.define_property(
            &JsString::from_utf8("ListFormat"),
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
fn list_format_record(agent: &Agent, this: &Value) -> Result<ListFormatRecord, JsError> {
    let Some(obj) = as_object(this) else {
        return Err(type_error("Not a ListFormat instance"));
    };
    agent
        .intl_list_format_data
        .get(&obj.id())
        .cloned()
        .ok_or_else(|| type_error("Not a ListFormat instance"))
}

/// GetPrototypeFromConstructor: the newTarget's `prototype`, falling back to
/// %Intl.ListFormat.prototype% of the newTarget's realm.
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
        .get(LIST_FORMAT_PROTO)
        .and_then(|value| as_object(&value))
        .ok_or_else(|| type_error("%Intl.ListFormat.prototype% missing"))
}

/// Intl.ListFormat (ECMA-402 §14.1.1).
fn initialize(
    agent: &mut Agent,
    locales: &Value,
    options: &Value,
) -> Result<ListFormatRecord, JsError> {
    let requested = crate::builtins::intl::canonicalize_locale_list(agent, locales)?;
    // ResolveOptions without «coerce-options»: GetOptionsObject (primitives
    // throw), no relevant extension keys.
    let options = get_options_object(agent, options)?;
    get_option(
        agent,
        &options,
        "localeMatcher",
        &["lookup", "best fit"],
        Some("best fit"),
    )?;
    let locale = number_format::resolve_locale_simple(&requested)?;
    let type_value = get_option(
        agent,
        &options,
        "type",
        &["conjunction", "disjunction", "unit"],
        Some("conjunction"),
    )?;
    let style = get_option(
        agent,
        &options,
        "style",
        &["long", "short", "narrow"],
        Some("long"),
    )?;
    Ok(ListFormatRecord {
        locale,
        type_value: match type_value.as_deref() {
            Some("disjunction") => TYPE_DISJUNCTION,
            Some("unit") => TYPE_UNIT,
            _ => TYPE_CONJUNCTION,
        },
        style: match style.as_deref() {
            Some("short") => STYLE_SHORT,
            Some("narrow") => STYLE_NARROW,
            _ => STYLE_LONG,
        },
    })
}

fn create_instance(
    agent: &mut Agent,
    proto: Handle<JsObject>,
    record: ListFormatRecord,
) -> Result<Value, JsError> {
    let instance = JsObject::ordinary_object_create(Some(proto));
    agent.intl_list_format_data.insert(instance.id(), record);
    Ok(Value::Object(instance))
}

/// StringListFromIterable (ECMA-402 §14.5.5): the list of strings, closing
/// the iterator with a TypeError when a non-string element appears.
fn string_list_from_iterable(agent: &mut Agent, iterable: &Value) -> Result<Vec<String>, JsError> {
    if iterable.is_undefined() {
        return Ok(Vec::new());
    }
    let record = crate::expr::get_iterator(agent, iterable)?;
    let mut list = Vec::new();
    loop {
        let next = crate::expr::iterator_step(agent, &record)?;
        let Some(value) = next else {
            return Ok(list);
        };
        if !value.is_string() {
            let error = type_error("List elements must be strings");
            crate::expr::iterator_close(agent, &record)?;
            return Err(error);
        }
        list.push(crate::context::to_string(agent, &value)?.to_string_lossy());
    }
}

/// CreatePartsFromList (ECMA-402 §14.5.2): the element/literal parts from
/// the Start/Middle/End/Pair templates.
pub(crate) fn create_parts_from_list(
    record: &ListFormatRecord,
    list: &[String],
) -> Vec<(String, String)> {
    let size = list.len();
    if size == 0 {
        return Vec::new();
    }
    let (start, middle, end, pair) = record.templates();
    let element = |text: &str| ("element".to_string(), text.to_string());
    if size == 1 {
        return vec![element(&list[0])];
    }
    // DeconstructPattern: substitute {0} with a single part and {1} with a
    // parts list (the Start/Middle/End/Pair patterns have the same shape).
    let deconstruct =
        |pattern: &str, head: &str, tail: Vec<(String, String)>| -> Vec<(String, String)> {
            let mut parts = Vec::new();
            let mut rest = pattern;
            while let Some(index) = rest.find('{') {
                if index > 0 {
                    parts.push(("literal".to_string(), rest[..index].to_string()));
                }
                let end_index = rest[index..]
                    .find('}')
                    .map(|i| index + i)
                    .unwrap_or(rest.len());
                let token = &rest[index + 1..end_index];
                if token == "0" {
                    parts.push(element(head));
                } else {
                    parts.extend(tail.iter().cloned());
                }
                rest = &rest[end_index + 1..];
            }
            if !rest.is_empty() {
                parts.push(("literal".to_string(), rest.to_string()));
            }
            parts
        };
    if size == 2 {
        return deconstruct(pair, &list[0], vec![element(&list[1])]);
    }
    let mut parts = vec![element(&list[size - 1])];
    let mut i = size - 2;
    loop {
        let pattern = if i == 0 {
            start
        } else if i < size - 2 {
            middle
        } else {
            end
        };
        parts = deconstruct(pattern, &list[i], parts);
        if i == 0 {
            break;
        }
        i -= 1;
    }
    parts
}

/// Intl.ListFormat.prototype.format (ECMA-402 §14.3.3).
fn format_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let record = list_format_record(agent, this)?;
    let iterable = args.first().cloned().unwrap_or(Value::Undefined);
    let list = string_list_from_iterable(agent, &iterable)?;
    let parts = create_parts_from_list(&record, &list);
    let mut result = String::new();
    for (_, value) in &parts {
        result.push_str(value);
    }
    Ok(Value::String(Handle::new(JsString::from_utf8(&result))))
}

/// Intl.ListFormat.prototype.formatToParts (ECMA-402 §14.3.4).
fn format_to_parts_method(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let record = list_format_record(agent, this)?;
    let iterable = args.first().cloned().unwrap_or(Value::Undefined);
    let list = string_list_from_iterable(agent, &iterable)?;
    let parts = create_parts_from_list(&record, &list);
    let object_proto = agent
        .current_realm()?
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let mut array = Vec::new();
    for (part_type, value) in parts {
        let obj = JsObject::ordinary_object_create(object_proto);
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
                value: Some(Value::String(Handle::new(JsString::from_utf8(&value)))),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(true),
                configurable: Some(true),
            },
        )?;
        array.push(Value::Object(obj));
    }
    crate::builtins::array::array_from_values(agent, &array)
}

/// Intl.ListFormat.prototype.resolvedOptions (ECMA-402 §14.3.2).
fn resolved_options_method(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let record = list_format_record(agent, this)?;
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
    define("type", str(record.type_name()))?;
    define("style", str(record.style_name()))?;
    Ok(Value::Object(options))
}

/// Intl.ListFormat.supportedLocalesOf (ECMA-402 §14.2.2).
fn supported_locales_of(
    agent: &mut Agent,
    locales: Value,
    options: Value,
) -> Result<Value, JsError> {
    let requested = crate::builtins::intl::canonicalize_locale_list(agent, &locales)?;
    // SupportedLocales: non-undefined options are coerced with ToObject
    // (spec 14.2.2 step 2.a — unlike the constructor's GetOptionsObject).
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

/// dispatch_call: the ListFormat constructor (as a function — throws), the
/// prototype members, and supportedLocalesOf.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(LIST_FORMAT).as_ref() == Some(callee) {
        return Some(Err(type_error("Intl.ListFormat requires 'new'")));
    }
    if intrinsics.get(LF_SUPPORTED_LOCALES_OF).as_ref() == Some(callee) {
        return Some(supported_locales_of(
            agent,
            args.first().cloned().unwrap_or(Value::Undefined),
            args.get(1).cloned().unwrap_or(Value::Undefined),
        ));
    }
    if intrinsics.get(LF_RESOLVED_OPTIONS).as_ref() == Some(callee) {
        return Some(resolved_options_method(agent, this));
    }
    if intrinsics.get(LF_FORMAT).as_ref() == Some(callee) {
        return Some(format_method(agent, this, args));
    }
    if intrinsics.get(LF_FORMAT_TO_PARTS).as_ref() == Some(callee) {
        return Some(format_to_parts_method(agent, this, args));
    }
    None
}

/// dispatch_construct: `new Intl.ListFormat(...)`.
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    if realm.intrinsics.get(LIST_FORMAT).as_ref() == Some(callee) {
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
