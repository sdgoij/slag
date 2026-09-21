//! Objects, arrays, and the keyed collections (`v8::Object`, `v8::Array`,
//! `v8::Map`, `v8::Set`).

use crux::handle::Handle;
use crux::object::{JsObject, PropertyKind};
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::value::Value as EngineValue;
use crux::value::ValueKind;
use runtime::api;
use slag::objects::{ensure_deferred_namespace_evaluation, materialize_pending_prototype_value};

use crate::data::{Array, Map, Name, Object, Set, Value};
use crate::handle::Local;
use crate::property::{GetPropertyNamesArgs, KeyConversionMode, PropertyFilter};
use crate::property_descriptor::PropertyDescriptor as V8PropertyDescriptor;
use crate::scope::PinScope;

impl Object {
    /// A new ordinary object in the scope's realm (`v8::Object::New`).
    pub fn new<'s>(scope: &PinScope<'s, '_, ()>) -> Local<'s, Object> {
        let realm = crate::realm_of(scope);
        match api::Object::new(&realm) {
            Ok(object) => Local::from_engine(object),
            Err(error) => {
                crate::throw(scope, &error);
                // The caller has nowhere to put an error: `v8::Object::New`
                // has no failure channel either, so a realm that cannot make an
                // ordinary object is a bridge bug.
                panic!("bridge: creating an object failed: {error}");
            }
        }
    }

    /// A new ordinary object with `prototype_or_null` as its prototype, and
    /// `names` defined on it as data properties holding `values`
    /// (`v8::Object::New` with a prototype and properties).
    ///
    /// The properties are *defined* rather than assigned, so each is created
    /// with `writable`, `enumerable` and `configurable` all true no matter what
    /// the prototype holds — a prototype with a setter of the same name cannot
    /// intercept one, which is what the crate we stand in for does here.
    pub fn with_prototype_and_properties<'s>(
        scope: &PinScope<'s, '_>,
        prototype_or_null: Local<'s, Value>,
        names: &[Local<'s, Name>],
        values: &[Local<'s, Value>],
    ) -> Local<'s, Object> {
        assert_eq!(names.len(), values.len());
        let prototype = prototype_or_null.engine().value().as_object();
        let object = JsObject::ordinary_object_create(prototype);
        for (name, value) in names.iter().zip(values) {
            let key = property_key(name);
            match object
                .define_property_key(&key, &PropertyDescriptor::data(*value.engine().value()))
            {
                Ok(_) => {}
                Err(error) => {
                    crate::throw(scope, &error);
                    panic!("bridge: defining a property on a fresh object failed: {error}");
                }
            }
        }
        Local::from_engine(api::Local::from(EngineValue::Object(object)))
    }
}

impl<'s> Local<'s, Object> {
    /// [[Get]] a property (`v8::Object::Get`).
    ///
    /// Only string keys are supported: Slag's public API resolves properties by
    /// name, and a key of any other type is reported the way a failed
    /// conversion is — as a pending exception.
    pub fn get<'a>(&self, scope: &PinScope<'a, '_>, key: Local<Value>) -> Option<Local<'a, Value>> {
        let name = key.engine().as_string()?;
        let realm = crate::realm_of(scope);
        match api::Object::get(&realm, self.engine(), &name) {
            Ok(value) => Some(Local::from_engine(value)),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// [[Set]] a property (`v8::Object::Set`).
    pub fn set(
        &self,
        scope: &PinScope<'_, '_>,
        key: Local<Value>,
        value: Local<Value>,
    ) -> Option<bool> {
        let name = key.engine().as_string()?;
        let realm = crate::realm_of(scope);
        match api::Object::set(&realm, self.engine(), &name, value.engine(), true) {
            Ok(ok) => Some(ok),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// [[HasProperty]] (`v8::Object::Has`).
    pub fn has(&self, scope: &PinScope<'_, '_>, key: Local<Value>) -> Option<bool> {
        let name = key.engine().as_string()?;
        let realm = crate::realm_of(scope);
        match api::Object::has(&realm, self.engine(), &name) {
            Ok(ok) => Some(ok),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// Define a property from a descriptor (`v8::Object::DefineProperty`).
    ///
    /// An ordinary `DefineOwnProperty` that does not throw, as there: the
    /// fields the descriptor mentions are the ones defined, a rejection answers
    /// `Some(false)`, and `None` is a real error — a proxy trap that threw —
    /// with it pending.
    pub fn define_property(
        &self,
        scope: &PinScope<'_, '_>,
        key: Local<Name>,
        descriptor: &V8PropertyDescriptor,
    ) -> Option<bool> {
        let object = self.engine().value().as_object()?;
        let key = property_key(&key);
        match object.define_property_key(&key, descriptor.engine()) {
            Ok(defined) => Some(defined),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// [[Get]] with an integer index (`v8::Object::GetIndex`).
    ///
    /// The engine resolves properties by name, so the index goes in as one: an
    /// integer-like name reaches the property an indexed lookup reaches, on an
    /// array or on a plain object, which is the case this is reached for.
    pub fn get_index<'a>(&self, scope: &PinScope<'a, '_>, index: u32) -> Option<Local<'a, Value>> {
        self.get(
            scope,
            Local::from_engine(api::Local::string(index.to_string())),
        )
    }

    /// The object's own property names, filtered and spelled as `args` asks
    /// (`v8::Object::GetOwnPropertyNames`).
    ///
    /// The order is the engine's own key order — integer-like keys first in
    /// ascending order, then the rest in insertion order, then the symbols —
    /// and two barriers are crossed first, because an enumeration observes keys
    /// a single-key lookup does not: a function's pending `prototype`, and a
    /// deferred module namespace, whose reading triggers its module.
    pub fn get_own_property_names<'a>(
        &self,
        scope: &PinScope<'a, '_>,
        args: GetPropertyNamesArgs,
    ) -> Option<Local<'a, Array>> {
        let value = *self.engine().value();
        let object = object_of(&value)?;
        let realm = crate::realm_of(scope);
        let keys = match realm.with_agent(|agent| {
            materialize_pending_prototype_value(agent, &value)?;
            ensure_deferred_namespace_evaluation(agent, &object)?;
            object.own_property_keys()
        }) {
            Ok(keys) => keys,
            Err(error) => {
                crate::throw(scope, &error);
                return None;
            }
        };

        let filter = args.property_filter;
        let mut names: Vec<Local<'_, Value>> = Vec::new();
        for key in keys {
            let skipped = match &key {
                PropertyKey::Symbol(_) => filter.is_skip_symbols(),
                PropertyKey::String(_) => filter.is_skip_strings(),
            };
            if skipped {
                continue;
            }
            match key_passes_filter(&filter, &object, &key) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    crate::throw(scope, &error);
                    return None;
                }
            }
            if let Some(name) = key_name(&key, args.key_conversion) {
                names.push(Local::from_engine(api::Local::from(name)));
            }
        }
        Some(Array::new_with_elements(scope, &names))
    }
}

/// The object part of a value: an ordinary object, or a function, which is an
/// object for every property purpose.
fn object_of(value: &EngineValue) -> Option<Handle<JsObject>> {
    match value.kind() {
        ValueKind::Object(object) => Some(object),
        ValueKind::Function(function) => Some(function.object),
        _ => None,
    }
}

/// The property key a `v8::Name` names.
fn property_key(name: &Local<'_, Name>) -> PropertyKey {
    match name.engine().value().kind() {
        ValueKind::String(text) => PropertyKey::from_js_string(&text),
        ValueKind::Symbol(symbol) => PropertyKey::Symbol(symbol),
        _ => panic!("bridge bug: a Name handle that is neither string nor symbol"),
    }
}

/// Whether the key's own property survives the attribute half of the filter.
/// The kind half is the caller's, which knows a string from a symbol.
fn key_passes_filter(
    filter: &PropertyFilter,
    object: &Handle<JsObject>,
    key: &PropertyKey,
) -> Result<bool, crux::error::JsError> {
    let wants_attributes =
        filter.is_only_enumerable() || filter.is_only_configurable() || filter.is_only_writable();
    if !wants_attributes {
        return Ok(true);
    }
    let Some(property) = object.get_own_property_key(key)? else {
        return Ok(false);
    };
    Ok((!filter.is_only_enumerable() || property.enumerable)
        && (!filter.is_only_configurable() || property.configurable)
        && (!filter.is_only_writable()
            || matches!(&property.kind, PropertyKind::Data { writable: true, .. })))
}

/// The name a key contributes to the enumeration, or `None` when the spelling
/// rule leaves it out.
fn key_name(key: &PropertyKey, conversion: KeyConversionMode) -> Option<EngineValue> {
    match key {
        PropertyKey::Symbol(symbol) => Some(EngineValue::Symbol(*symbol)),
        PropertyKey::String(_) => {
            let text = key.display_string();
            match (conversion, array_index_of(&text)) {
                (KeyConversionMode::NoNumbers, Some(_)) => None,
                (KeyConversionMode::KeepNumbers, Some(index)) => {
                    Some(EngineValue::Number(f64::from(index)))
                }
                _ => Some(EngineValue::String(crux::handle::Handle::new(
                    crux::string::JsString::from_utf8(&text),
                ))),
            }
        }
    }
}

/// The array index a key's text stands for (spec 7.1.21): a canonical decimal
/// in `u32` range. That is what the crate we stand in for reports as a number
/// under `kKeepNumbers`, and leaves out under `kNoNumbers`.
fn array_index_of(text: &str) -> Option<u32> {
    if text.is_empty() || (text.len() > 1 && text.starts_with('0')) {
        return None;
    }
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

impl Array {
    /// A new array of `length` holes (`v8::Array::New`).
    pub fn new<'s>(scope: &PinScope<'s, '_, ()>, _length: i32) -> Local<'s, Array> {
        let realm = crate::realm_of(scope);
        match api::Array::new(&realm, &[]) {
            Ok(array) => Local::from_engine(array),
            Err(error) => {
                crate::throw(scope, &error);
                panic!("bridge: creating an array failed: {error}");
            }
        }
    }

    /// A new array from `elements` (`v8::Array::New` with elements).
    pub fn new_with_elements<'s>(
        scope: &PinScope<'s, '_, ()>,
        elements: &[Local<'_, Value>],
    ) -> Local<'s, Array> {
        let realm = crate::realm_of(scope);
        let values: Vec<api::Local> = elements.iter().map(|e| *e.engine()).collect();
        match api::Array::new(&realm, &values) {
            Ok(array) => Local::from_engine(array),
            Err(error) => {
                crate::throw(scope, &error);
                panic!("bridge: creating an array failed: {error}");
            }
        }
    }
}

impl<'s> Local<'s, Array> {
    /// The array's `length` (`v8::Array::Length`).
    pub fn length(&self) -> u32 {
        let realm = crate::realm_current();
        api::Array::length(&realm, self.engine()).map_or(0, |len| len as u32)
    }
}

impl<'s> Local<'s, Map> {
    /// The map's entries as one flat array — key, value, key, value
    /// (`v8::Map::as_array`), in insertion order, with deleted keys left out.
    pub fn as_array<'a>(&self, scope: &PinScope<'a, '_>) -> Local<'a, Array> {
        let pairs = entries(self.engine(), |agent, id| {
            let cell = agent.map_data.get(&id)?;
            let entries: Vec<EngineValue> = cell
                .borrow()
                .entries
                .iter()
                .flatten()
                .flat_map(|(key, value)| [*key, *value])
                .collect();
            Some(entries)
        });
        entries_array(scope, pairs)
    }
}

impl<'s> Local<'s, Set> {
    /// The set's elements as one flat array — each element as both its key and
    /// its value (`v8::Set::as_array`), in insertion order, with deleted
    /// elements left out.
    pub fn as_array<'a>(&self, scope: &PinScope<'a, '_>) -> Local<'a, Array> {
        let doubled = entries(self.engine(), |agent, id| {
            let cell = agent.set_data.get(&id)?;
            let entries: Vec<EngineValue> = cell
                .borrow()
                .entries
                .iter()
                .flatten()
                .flat_map(|element| [*element, *element])
                .collect();
            Some(entries)
        });
        entries_array(scope, doubled)
    }
}

/// The elements a keyed collection's table holds, read through `read`.
fn entries(
    value: &api::Local,
    read: impl FnOnce(&runtime::Agent, u64) -> Option<Vec<EngineValue>>,
) -> Vec<EngineValue> {
    let Some(object) = value.value().as_object() else {
        return Vec::new();
    };
    let id = object.id();
    crate::realm::with_agent(|agent| read(agent, id))
        .flatten()
        .expect("bridge bug: a Map or Set handle needs the realm it came from")
}

/// An array of engine values (`v8::Map::as_array`'s result).
fn entries_array<'a>(scope: &PinScope<'a, '_>, values: Vec<EngineValue>) -> Local<'a, Array> {
    let locals: Vec<Local<'_, Value>> = values
        .into_iter()
        .map(|value| Local::from_engine(api::Local::from(value)))
        .collect();
    Array::new_with_elements(scope, &locals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Number, Set};
    use crate::property::{GetPropertyNamesArgsBuilder, IndexFilter, KeyCollectionMode};
    use crate::test_support::{bind, eval, eval_number, in_context};

    /// The number a handle holds.
    fn number_of(value: Local<'_, Value>) -> f64 {
        Local::<Number>::try_from(value).expect("number").value()
    }

    /// The text a handle holds.
    fn text_of(scope: &PinScope<'_, '_>, value: Local<'_, Value>) -> std::string::String {
        value.to_rust_string_lossy(scope)
    }

    #[test]
    fn get_index_reads_a_property_by_position() {
        in_context!(scope, {
            let array =
                Local::<Array>::try_from(eval(scope, "globalThis.a = [7, 8, 9]")).expect("array");
            assert_eq!(number_of(array.get_index(scope, 1).expect("element")), 8.0);
            assert!(array.get_index(scope, 9).expect("value").is_undefined());

            // Integer-like names are properties on a plain object too.
            let object = Local::<Object>::try_from(eval(scope, "({ 0: 'zero' })")).expect("object");
            let first = object.get_index(scope, 0).expect("property");
            assert_eq!(text_of(scope, first), "zero");
        });
    }

    /// The names an enumeration yields, as texts (a number key renders as its
    /// digits, which is enough to see the order).
    fn names_of(
        scope: &PinScope<'_, '_>,
        object: &Local<'_, Object>,
        args: GetPropertyNamesArgs,
    ) -> Vec<std::string::String> {
        let names = object.get_own_property_names(scope, args).expect("names");
        (0..names.length())
            .map(|index| text_of(scope, names.get_index(scope, index).expect("name")))
            .collect()
    }

    /// An object's names come out in the engine's key order, spelled the way
    /// the arguments ask: integer-like keys first, as numbers by default.
    #[test]
    fn own_property_names_are_ordered_and_spelled_as_asked() {
        in_context!(scope, {
            let object = Local::<Object>::try_from(eval(
                scope,
                "globalThis.o = { b: 1, 2: 1, a: 1, 1: 1, [Symbol('s')]: 1 }",
            ))
            .expect("object");

            // The default filter: enumerable, symbols skipped, numbers kept.
            let names = object
                .get_own_property_names(scope, GetPropertyNamesArgs::default())
                .expect("names");
            assert_eq!(
                names_of(scope, &object, GetPropertyNamesArgs::default()),
                ["1", "2", "b", "a"]
            );
            assert!(names.get_index(scope, 0).expect("name").is_number());

            // ConvertToString keeps that order and spells every key as text.
            let args = GetPropertyNamesArgsBuilder::new()
                .key_conversion(KeyConversionMode::ConvertToString)
                .build();
            let names = object.get_own_property_names(scope, args).expect("names");
            assert!(names.get_index(scope, 0).expect("name").is_string());

            // NoNumbers leaves integer-like keys out entirely.
            let args = GetPropertyNamesArgsBuilder::new()
                .key_conversion(KeyConversionMode::NoNumbers)
                .build();
            assert_eq!(names_of(scope, &object, args), ["b", "a"]);

            // No filter flags at all: the symbol comes too, as a symbol.
            let args = GetPropertyNamesArgsBuilder::new()
                .mode(KeyCollectionMode::IncludePrototypes)
                .index_filter(IndexFilter::IncludeIndices)
                .property_filter(PropertyFilter::ALL_PROPERTIES)
                .key_conversion(KeyConversionMode::ConvertToString)
                .build();
            let names = object.get_own_property_names(scope, args).expect("names");
            assert_eq!(names.length(), 5);
            assert!(names.get_index(scope, 4).expect("symbol").is_symbol());

            // A non-enumerable own property is left out by enumerability, and
            // kept when the filter asks about a different attribute.
            eval(
                scope,
                "Object.defineProperty(o, 'hidden', { value: 1, enumerable: false, configurable: true })",
            );
            assert_eq!(
                names_of(scope, &object, GetPropertyNamesArgs::default()),
                ["1", "2", "b", "a"]
            );
            let args = GetPropertyNamesArgsBuilder::new()
                .property_filter(PropertyFilter::ONLY_CONFIGURABLE | PropertyFilter::SKIP_SYMBOLS)
                .key_conversion(KeyConversionMode::ConvertToString)
                .build();
            assert_eq!(
                names_of(scope, &object, args),
                ["1", "2", "b", "a", "hidden"],
                "kept: only configurability was asked about, and it has it"
            );
        });
    }

    /// An enumeration sees the keys a single-key lookup does not: an ordinary
    /// function's `prototype` is pending until something materializes it.
    #[test]
    fn own_property_names_see_a_functions_pending_prototype() {
        in_context!(scope, {
            let function =
                Local::<Object>::try_from(eval(scope, "(function () {})")).expect("object");
            let args = GetPropertyNamesArgsBuilder::new()
                .property_filter(PropertyFilter::ALL_PROPERTIES)
                .key_conversion(KeyConversionMode::ConvertToString)
                .build();
            // The order is the engine's own creation order; the point here is
            // that `prototype` is in the list at all, which takes crossing the
            // pending-prototype barrier.
            assert_eq!(
                names_of(scope, &function, args),
                ["length", "name", "caller", "arguments", "prototype"]
            );
        });
    }

    /// The properties are defined, not assigned: a setter on the prototype
    /// cannot intercept one, and every attribute comes out true.
    #[test]
    fn with_prototype_and_properties_defines_data_properties() {
        in_context!(scope, {
            let proto = eval(scope, "globalThis.hit = 0; ({ set k(v) { hit = v } })");
            let name = Local::<Name>::try_from(eval(scope, "'k'")).expect("name");
            let value = eval(scope, "42");

            let object = Object::with_prototype_and_properties(scope, proto, &[name], &[value]);
            bind(scope, "o", object.into());

            assert_eq!(
                eval_number(scope, "Object.getOwnPropertyDescriptor(o, 'k').value"),
                42.0,
                "the own data property holds the value"
            );
            assert_eq!(eval_number(scope, "hit"), 0.0, "the setter never ran");
            assert_eq!(
                eval_number(
                    scope,
                    "['writable', 'enumerable', 'configurable']\
                     .every((attribute) => Object.getOwnPropertyDescriptor(o, 'k')[attribute]) ? 1 : 0"
                ),
                1.0
            );
        });
    }

    /// A null prototype is the null prototype, not `%Object.prototype%`.
    #[test]
    fn with_prototype_and_properties_takes_a_null_prototype() {
        in_context!(scope, {
            let name = Local::<Name>::try_from(eval(scope, "'k'")).expect("name");
            let value = eval(scope, "1");
            let object = Object::with_prototype_and_properties(
                scope,
                eval(scope, "null"),
                &[name],
                &[value],
            );
            bind(scope, "o", object.into());
            assert_eq!(
                eval_number(scope, "Object.getPrototypeOf(o) === null ? 1 : 0"),
                1.0
            );
            assert_eq!(eval_number(scope, "o.k"), 1.0);
        });
    }

    /// The entries of a map come out flat, in insertion order, with a deleted
    /// key left out and an updated one holding its place — which is the order a
    /// script iterating the map sees.
    #[test]
    fn a_map_is_its_entries_flat() {
        in_context!(scope, {
            let map =
                Local::<Map>::try_from(eval(scope, "globalThis.m = new Map([['a', 1], ['b', 2]])"))
                    .expect("map");

            let entries = map.as_array(scope);
            assert_eq!(entries.length(), 4);
            assert_eq!(
                text_of(scope, entries.get_index(scope, 0).expect("key")),
                "a"
            );
            assert_eq!(number_of(entries.get_index(scope, 1).expect("value")), 1.0);
            assert_eq!(number_of(entries.get_index(scope, 3).expect("value")), 2.0);

            eval(scope, "m.delete('a'); m.set('b', 5); m.set('c', 3)");
            let entries = map.as_array(scope);
            assert_eq!(entries.length(), 4);
            assert_eq!(
                text_of(scope, entries.get_index(scope, 0).expect("key")),
                "b"
            );
            assert_eq!(number_of(entries.get_index(scope, 1).expect("value")), 5.0);
            assert_eq!(
                text_of(scope, entries.get_index(scope, 2).expect("key")),
                "c"
            );
        });
    }

    #[test]
    fn a_set_is_its_elements_each_way() {
        in_context!(scope, {
            let set = Local::<Set>::try_from(eval(scope, "new Set([1, 2])")).expect("set");
            let entries = set.as_array(scope);
            assert_eq!(entries.length(), 4);
            assert_eq!(
                number_of(entries.get_index(scope, 0).expect("element")),
                1.0
            );
            assert_eq!(
                number_of(entries.get_index(scope, 1).expect("element")),
                1.0
            );
            assert_eq!(
                number_of(entries.get_index(scope, 3).expect("element")),
                2.0
            );
        });
    }
}
