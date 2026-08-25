//! `Intl.supportedValuesOf` (ECMA-402 §8.3.2): the data-driven lists —
//! calendars, collations, currencies, numbering systems, time zones, and
//! units. The lists ship in `number_data.rs` (the corpus pins their
//! membership: gregory for calendars, the Table 30 rows for numbering
//! systems, the sanctioned simple units, and the round-trip through
//! `Intl.Locale`).

use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::property::PropertyDescriptor;
use crux::string::JsString;
use crux::value::Value;

use crate::agent::Agent;
use crate::builtins::intl::number_data::{
    ISO_4217_CURRENCIES, SANCTIONED_UNITS, SUPPORTED_CALENDARS, SUPPORTED_COLLATIONS,
    SUPPORTED_NUMBERING_SYSTEMS, SUPPORTED_TIME_ZONES,
};
use crate::context::as_object;
use crate::realm::Realm;

pub const SUPPORTED_VALUES_OF: &str = "%Intl.supportedValuesOf%";

/// Install `Intl.supportedValuesOf` onto `%Intl%`.
pub fn install(realm: &Handle<Realm>, intl_value: &Value) -> Result<(), JsError> {
    let function_proto = realm
        .intrinsics
        .get("%Function.prototype%")
        .and_then(|value| as_object(&value));
    let func = Function::create_builtin(
        Some(JsString::from_utf8("supportedValuesOf")),
        1,
        placeholder(),
        None,
        function_proto,
    )?;
    realm
        .intrinsics
        .define(SUPPORTED_VALUES_OF, Value::Function(func));
    if let Some(obj) = as_object(intl_value) {
        obj.define_property(
            &JsString::from_utf8("supportedValuesOf"),
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
    Ok(())
}

fn placeholder() -> NativeFn {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            "Intl.supportedValuesOf must be dispatched".into(),
        ))
    })
}

/// dispatch_call: `Intl.supportedValuesOf`.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    _this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    if realm.intrinsics.get(SUPPORTED_VALUES_OF).as_ref() != Some(callee) {
        return None;
    }
    Some(supported_values_of(
        agent,
        args.first().cloned().unwrap_or(Value::Undefined),
    ))
}

fn supported_values_of(agent: &mut Agent, key: Value) -> Result<Value, JsError> {
    let key = crate::context::to_string(agent, &key)?.to_string_lossy();
    let list: &[&str] = match key.as_str() {
        "calendar" => SUPPORTED_CALENDARS,
        "collation" => SUPPORTED_COLLATIONS,
        "currency" => ISO_4217_CURRENCIES,
        "numberingSystem" => SUPPORTED_NUMBERING_SYSTEMS,
        "timeZone" => SUPPORTED_TIME_ZONES,
        "unit" => SANCTIONED_UNITS,
        _ => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                format!("Invalid key {key}"),
            ));
        }
    };
    let values: Vec<Value> = list
        .iter()
        .map(|s| Value::String(Handle::new(JsString::from_utf8(s))))
        .collect();
    crate::builtins::array::array_from_values(agent, &values)
}
