//! The `%Intl%` intrinsic (ECMA-402 §2): the namespace object, its
//! `getCanonicalLocales`, and the `Intl.Locale` constructor (plan Cut 1).
//! The remaining ECMA-402 components (NumberFormat, DateTimeFormat, ...)
//! land in later cuts; their feature-gated fixtures skip in the harness
//! until then.

pub mod bcp47;
pub mod collator;
pub mod data;
pub mod date_time_format;
pub mod display_names;
pub mod duration_format;
pub mod list_format;
pub mod locale;
pub mod number_data;
pub mod number_format;
pub mod plural_data;
pub mod plural_rules;
pub mod relative_time_format;
pub mod segmenter;
pub mod supported_values;

use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::Value;

use crate::agent::Agent;
use crate::context::{as_object, get_property, to_object, to_string};
use crate::realm::{Realm, ResolvedNames};

const INTL: &str = "%Intl%";
const GET_CANONICAL_LOCALES: &str = "%Intl.getCanonicalLocales%";

/// The `[[InitializedLocale]]` internal slot: the canonical locale string.
#[derive(Debug, Clone)]
pub struct IntlLocaleRecord {
    pub locale: String,
}

fn type_error(message: &str) -> JsError {
    JsError::new(ErrorKind::TypeError, message.into())
}

/// SetDefaultGlobalBindings install for ECMA-402.
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let function_proto = realm
        .intrinsics
        .get("%Function.prototype%")
        .and_then(|value| as_object(&value));
    let intl = JsObject::ordinary_object_create(object_proto);

    // Intl.getCanonicalLocales (spec 9.2.1).
    let get_canonical = Function::create_builtin(
        Some(JsString::from_utf8("getCanonicalLocales")),
        1,
        placeholder("Intl.getCanonicalLocales"),
        None,
        function_proto,
    )?;
    realm
        .intrinsics
        .define(GET_CANONICAL_LOCALES, Value::Function(get_canonical));
    intl.define_property(
        &JsString::from_utf8("getCanonicalLocales"),
        &PropertyDescriptor {
            value: Some(Value::Function(get_canonical)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // Intl[@@toStringTag] = "Intl".
    intl.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag")),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8("Intl")))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    let intl_value = Value::Object(intl);
    realm.intrinsics.define(INTL, intl_value);

    // Intl.Locale (the constructor + prototype), Intl.NumberFormat (plan
    // Cut 2), and Intl.PluralRules/Intl.RelativeTimeFormat (plan Cut 3).
    locale::install(realm, &intl_value)?;
    number_format::install(realm, &intl_value)?;
    supported_values::install(realm, &intl_value)?;
    plural_rules::install(realm, &intl_value)?;
    relative_time_format::install(realm, &intl_value)?;
    list_format::install(realm, &intl_value)?;
    display_names::install(realm, &intl_value)?;
    date_time_format::install(realm, &intl_value)?;
    collator::install(realm, &intl_value)?;
    segmenter::install(realm, &intl_value)?;
    duration_format::install(realm, &intl_value)?;

    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("Intl"),
        &PropertyDescriptor {
            value: Some(intl_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

fn placeholder(name: &str) -> NativeFn {
    let name = name.to_string();
    Box::new(move |_, _| Err(type_error(&format!("{name} must be dispatched"))))
}

type CallDispatch = fn(&mut Agent, &Value, &Value, &[Value]) -> Option<Result<Value, JsError>>;
type ConstructDispatch = fn(&mut Agent, &Value, &[Value], &Value) -> Option<Result<Value, JsError>>;

/// The ECMA-402 component a `%Intl.<Name>…%` intrinsic belongs to: the text
/// between `%Intl.` and the next `.` or `%`. `None` when the callee is not an
/// `%Intl.` intrinsic, or when its names disagree about the component — which
/// routes the call to the chain below rather than to a guess.
fn component_of(resolved: &ResolvedNames) -> Option<&str> {
    let mut owner = None;
    for name in resolved.names() {
        let Some(rest) = name.strip_prefix("%Intl.") else {
            continue;
        };
        let head = rest.split('.').next().unwrap_or(rest);
        let head = head.strip_suffix('%').unwrap_or(head);
        match owner {
            Some(existing) if existing != head => return None,
            _ => owner = Some(head),
        }
    }
    owner
}

/// The sub-dispatcher of the component that owns an `%Intl.<Name>%` prefix.
/// Each component owns exactly one namespace, so this picks it in one string
/// compare where the chain below resolves the callee's names once per
/// sub-dispatcher it visits.
fn routed_call(owner: &str) -> Option<CallDispatch> {
    Some(match owner {
        "Collator" => collator::dispatch_call,
        "DateTimeFormat" => date_time_format::dispatch_call,
        "DisplayNames" => display_names::dispatch_call,
        "DurationFormat" => duration_format::dispatch_call,
        "ListFormat" => list_format::dispatch_call,
        "Locale" => locale::dispatch_call,
        "NumberFormat" => number_format::dispatch_call,
        "PluralRules" => plural_rules::dispatch_call,
        "RelativeTimeFormat" => relative_time_format::dispatch_call,
        "Segmenter" => segmenter::dispatch_call,
        "supportedValuesOf" => supported_values::dispatch_call,
        _ => return None,
    })
}

/// The construct side of [`routed_call`]: the components whose constructors
/// are reachable through `new` (`Intl.supportedValuesOf` is not one).
fn routed_construct(owner: &str) -> Option<ConstructDispatch> {
    Some(match owner {
        "Collator" => collator::dispatch_construct,
        "DateTimeFormat" => date_time_format::dispatch_construct,
        "DisplayNames" => display_names::dispatch_construct,
        "DurationFormat" => duration_format::dispatch_construct,
        "ListFormat" => list_format::dispatch_construct,
        "Locale" => locale::dispatch_construct,
        "NumberFormat" => number_format::dispatch_construct,
        "PluralRules" => plural_rules::dispatch_construct,
        "RelativeTimeFormat" => relative_time_format::dispatch_construct,
        "Segmenter" => segmenter::dispatch_construct,
        _ => return None,
    })
}

/// dispatch_call: `Intl.getCanonicalLocales`, `Intl.supportedValuesOf` and
/// the `Intl.Locale`/`Intl.NumberFormat` prototype members.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    let resolved = intrinsics.name_of(callee);
    if resolved.is(GET_CANONICAL_LOCALES) {
        return Some(get_canonical_locales(
            agent,
            args.first().cloned().unwrap_or(Value::Undefined),
        ));
    }
    // Routing is a pure shortcut: a component that does not claim the callee
    // falls through to the walk, which stays the thing that decides.
    if let Some(dispatch) = component_of(&resolved).and_then(routed_call)
        && let Some(result) = dispatch(agent, callee, this, args)
    {
        return Some(result);
    }
    if let Some(result) = supported_values::dispatch_call(agent, callee, this, args) {
        return Some(result);
    }
    if let Some(result) = locale::dispatch_call(agent, callee, this, args) {
        return Some(result);
    }
    if let Some(result) = number_format::dispatch_call(agent, callee, this, args) {
        return Some(result);
    }
    if let Some(result) = plural_rules::dispatch_call(agent, callee, this, args) {
        return Some(result);
    }
    if let Some(result) = relative_time_format::dispatch_call(agent, callee, this, args) {
        return Some(result);
    }
    if let Some(result) = list_format::dispatch_call(agent, callee, this, args) {
        return Some(result);
    }
    if let Some(result) = display_names::dispatch_call(agent, callee, this, args) {
        return Some(result);
    }
    if let Some(result) = collator::dispatch_call(agent, callee, this, args) {
        return Some(result);
    }
    if let Some(result) = segmenter::dispatch_call(agent, callee, this, args) {
        return Some(result);
    }
    if let Some(result) = duration_format::dispatch_call(agent, callee, this, args) {
        return Some(result);
    }
    date_time_format::dispatch_call(agent, callee, this, args)
}

/// dispatch_construct: `new Intl.Locale(...)` and `new Intl.NumberFormat(...)`.
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let resolved = realm.intrinsics.name_of(callee);
    if let Some(dispatch) = component_of(&resolved).and_then(routed_construct)
        && let Some(result) = dispatch(agent, callee, args, new_target)
    {
        return Some(result);
    }
    if let Some(result) = locale::dispatch_construct(agent, callee, args, new_target) {
        return Some(result);
    }
    if let Some(result) = number_format::dispatch_construct(agent, callee, args, new_target) {
        return Some(result);
    }
    if let Some(result) = plural_rules::dispatch_construct(agent, callee, args, new_target) {
        return Some(result);
    }
    if let Some(result) = relative_time_format::dispatch_construct(agent, callee, args, new_target)
    {
        return Some(result);
    }
    if let Some(result) = list_format::dispatch_construct(agent, callee, args, new_target) {
        return Some(result);
    }
    if let Some(result) = display_names::dispatch_construct(agent, callee, args, new_target) {
        return Some(result);
    }
    if let Some(result) = collator::dispatch_construct(agent, callee, args, new_target) {
        return Some(result);
    }
    if let Some(result) = segmenter::dispatch_construct(agent, callee, args, new_target) {
        return Some(result);
    }
    if let Some(result) = duration_format::dispatch_construct(agent, callee, args, new_target) {
        return Some(result);
    }
    date_time_format::dispatch_construct(agent, callee, args, new_target)
}

/// CanonicalizeLocaleList (ECMA-402 §9.2.1): the `locales` argument — an
/// undefined → empty list; a String or an initialized Intl.Locale → a
/// one-element list; otherwise the array-like's elements, each an
/// initialized Intl.Locale's [[Locale]] or a String/Object coerced with
/// ToString, validated and canonicalized, deduplicated. Each element is
/// processed in its own loop iteration so a `toString` on an earlier element
/// is observable by later `HasProperty`/`Get` (spec steps 7.b-7.f).
pub fn canonicalize_locale_list(
    agent: &mut Agent,
    locales: &Value,
) -> Result<Vec<String>, JsError> {
    let mut result = Vec::new();
    if locales.is_undefined() {
        return Ok(result);
    }
    // Spec §9.2.1 step 3: a String (or an initialized Intl.Locale) is a
    // one-element list, not an array-like to iterate.
    let single = locales.is_string()
        || as_object(locales).is_some_and(|obj| agent.intl_locale_data.contains_key(&obj.id()));
    let mut seen: Vec<String> = Vec::new();
    let mut process = |agent: &mut Agent, element: Value| -> Result<(), JsError> {
        // Spec §9.2.1 step 7.c.ii: neither a String nor an Object throws.
        if !element.is_string() && as_object(&element).is_none() {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Invalid locale value".into(),
            ));
        }
        let tag_text: String = if let Some(obj) = as_object(&element)
            && let Some(record) = agent.intl_locale_data.get(&obj.id())
        {
            record.locale.clone()
        } else {
            to_string(agent, &element)?.to_string_lossy()
        };
        if !bcp47::is_well_formed(&tag_text) {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "Invalid language tag".into(),
            ));
        }
        let canonical = bcp47::canonicalize(&tag_text)?;
        if !seen.contains(&canonical) {
            seen.push(canonical.clone());
            result.push(canonical);
        }
        Ok(())
    };
    if single {
        process(agent, *locales)?;
    } else {
        let object = to_object(agent, locales)?;
        let length_value = get_property(agent, &object, &JsString::from_utf8("length"), object)?;
        let length = crux::convert::to_length(crux::convert::to_number(&length_value)?) as usize;
        let object_ref = as_object(&object)
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "Invalid locales value".into()))?;
        for index in 0..length {
            let key = JsString::from_utf8(&index.to_string());
            // Spec §9.2.1 step 7.b: HasProperty before Get — a proxy's `has`
            // trap (and its errors) must be honored.
            let property_key = crux::property::PropertyKey::from_js_string(&key);
            if !crate::module::has_property_with_deferred_trigger(
                agent,
                &object_ref,
                &property_key,
            )? {
                continue;
            }
            let element = get_property(agent, &object, &key, object)?;
            process(agent, element)?;
        }
    }
    Ok(result)
}

/// Intl.getCanonicalLocales (ECMA-402 §8.3.1): an array of the canonical
/// locale strings.
fn get_canonical_locales(agent: &mut Agent, locales: Value) -> Result<Value, JsError> {
    let list = canonicalize_locale_list(agent, &locales)?;
    let values: Vec<Value> = list
        .into_iter()
        .map(|tag| Value::String(Handle::new(JsString::from_utf8(&tag))))
        .collect();
    crate::builtins::array::array_from_values(agent, &values)
}
