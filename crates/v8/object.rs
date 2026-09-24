//! Objects, arrays, and the keyed collections (`v8::Object`, `v8::Array`,
//! `v8::Map`, `v8::Set`).

use std::collections::HashSet;
use std::num::NonZeroI32;

use crate::data::{Array, Map, Name, Object, Private, Proxy, Set, String, Value};
use crate::handle::{Local, LocalHandle};
use crate::property::{
    GetPropertyNamesArgs, IndexFilter, KeyCollectionMode, KeyConversionMode, PropertyAttribute,
    PropertyFilter,
};
use crate::property_descriptor::PropertyDescriptor as V8PropertyDescriptor;
use crate::scope::PinScope;
use crux::handle::Handle;
use crux::object::{JsObject, ObjectKind, PropertyKind};
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::value::Value as EngineValue;
use crux::value::ValueKind;
use runtime::api;
use slag::objects::{ensure_deferred_namespace_evaluation, materialize_pending_prototype_value};

/// A descriptor that defines a new own data property with every attribute set,
/// which is `CreateDataProperty` (spec 7.3.5) — every attribute, so nothing a
/// prototype's setter or a non-configurable inherited property says can change
/// what the define does.
pub(crate) fn data_property(value: Local<'_, Value>) -> V8PropertyDescriptor {
    let mut descriptor = V8PropertyDescriptor::new_from_value_writable(value, true);
    descriptor.set_enumerable(true);
    descriptor.set_configurable(true);
    descriptor
}

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

impl<'s> LocalHandle<'s, Object> {
    /// [[Get]] a property by a private name (`v8::Object::GetPrivate`).
    ///
    /// A private name is a symbol here — see [`private`](crate::private) — so
    /// this is the symbol-keyed read, and `None` is the empty handle the crate
    /// answers with for a name the object does not have.
    pub fn get_private<'a>(
        &self,
        scope: &PinScope<'a, '_>,
        key: Local<'_, Private>,
    ) -> Option<Local<'a, Value>> {
        let symbol = key.engine().value().as_symbol()?;
        let object = self.engine().value().as_object()?;
        let key = PropertyKey::Symbol(symbol);
        let realm = crate::realm_of(scope);
        let value = realm.with_agent(|_| object.get_key(&key).ok())?;
        if value.is_undefined() {
            // A read answers `undefined` both for a name that was never set and
            // for one that was set to `undefined`, and the crate's empty handle
            // is only the first. The own-property question is what tells them
            // apart, and it is the same extra lookup the crate's own
            // implementation makes to detect absence.
            let own = realm
                .with_agent(|_| object.has_own_property_key(&key))
                .unwrap_or(false);
            if !own {
                return None;
            }
        }
        Some(Local::from_engine(api::Local::from(value)))
    }

    /// [[Set]] a property by a private name (`v8::Object::SetPrivate`).
    ///
    /// Throw semantics, as there: a rejection (`Some(false)`) is a property that
    /// could not be written, and `None` is a real error — a proxy trap that
    /// threw — with it pending.
    pub fn set_private(
        &self,
        scope: &PinScope<'_, '_>,
        key: Local<'_, Private>,
        value: Local<'_, Value>,
    ) -> Option<bool> {
        let symbol = key.engine().value().as_symbol()?;
        let object = self.engine().value().as_object()?;
        let value = *value.engine().value();
        crate::realm_of(scope).with_agent(|_| {
            object
                .set_key(&PropertyKey::Symbol(symbol), value, true)
                .ok()
        })
    }

    /// The object's identity hash (v8::Object::GetIdentityHash), the number a
    /// host keys a table by.
    ///
    /// The engine gives every object an id of its own for as long as it lives,
    /// which is the identity this bridge's handle equality already compares, so
    /// a table keyed by this agrees with `==`.
    pub fn get_identity_hash(&self) -> NonZeroI32 {
        self.identity_hash()
    }

    /// The object's constructor name (`v8::Object::GetConstructorName`).
    ///
    /// The crate answers from the object's map — the constructor it was
    /// instantiated with — reads `Symbol.toStringTag` from the prototype chain as
    /// its other source, and falls back to the string "Object". This walks the
    /// chain for the same answers: the nearest own `constructor` whose `name` is
    /// neither empty nor "Object", then "Object" itself.
    ///
    /// Two divergences from the crate, both recorded in `.notes/embedding.md`
    /// §9. A map remembers the constructor an object was *made* with, while this
    /// reads the property as it is now, so a reassigned
    /// `prototype.constructor` shows up here and not there. And
    /// `Symbol.toStringTag` is not consulted: this walk reads the constructor's
    /// `name` where V8's helper reads the tag as another source — an iterator
    /// answers "Iterator" here — the name its prototype chain carries — where
    /// V8's tag makes it "Map Iterator" or "Array Iterator" by kind. The tag is
    /// readable now; not reading it is the decision, not a limit.
    pub fn get_constructor_name(&self) -> Local<'_, String> {
        let realm = crate::realm_current();
        let mut current: Local<'_, Object> = self.retag();
        loop {
            let constructor = api::Object::get(&realm, current.engine(), "constructor").ok();
            if let Some(name) = constructor
                .as_ref()
                .and_then(|c| constructor_name(&realm, c))
            {
                return name;
            }
            match api::Object::get_prototype(&realm, current.engine()) {
                Ok(prototype) if prototype.value().is_object() => {
                    current = Local::<Object>::from_engine(prototype).retag();
                }
                _ => break,
            }
        }
        // The crate's own fallback, for an object whose chain names no
        // constructor: the string "Object".
        Local::from_engine(api::Local::from(EngineValue::String(Handle::new(
            crux::string::JsString::from_utf8("Object"),
        ))))
    }

    /// [[Get]] a property (`v8::Object::Get`).
    ///
    /// The key is a `Name` — a String or a Symbol — and a key of any other kind
    /// is reported the way a failed conversion is: as a pending exception.
    pub fn get<'a>(&self, scope: &PinScope<'a, '_>, key: Local<Value>) -> Option<Local<'a, Value>> {
        let realm = crate::realm_of(scope);
        match api::Object::get_key(&realm, self.engine(), key.engine()) {
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
        let realm = crate::realm_of(scope);
        match api::Object::set_key(&realm, self.engine(), key.engine(), value.engine(), true) {
            Ok(ok) => Some(ok),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// [[HasProperty]] (`v8::Object::Has`).
    pub fn has(&self, scope: &PinScope<'_, '_>, key: Local<Value>) -> Option<bool> {
        let realm = crate::realm_of(scope);
        match api::Object::has_key(&realm, self.engine(), key.engine()) {
            Ok(ok) => Some(ok),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// [[HasOwnProperty]] (`v8::Object::HasOwnProperty`).
    ///
    /// The chain is not consulted, so an inherited property answers `false` —
    /// which is the question that tells an array element from a hole, since a
    /// `[[Get]]` of either answers `undefined`.
    pub fn has_own_property(&self, scope: &PinScope<'_, '_>, key: Local<Value>) -> Option<bool> {
        let realm = crate::realm_of(scope);
        match api::Object::has_own_key(&realm, self.engine(), key.engine()) {
            Ok(found) => Some(found),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// [[Set]] with an integer index (`v8::Object::SetIndex`).
    ///
    /// As [`get_index`](Self::get_index): the engine resolves properties by
    /// name, so the index goes in as one.
    pub fn set_index(
        &self,
        scope: &PinScope<'_, '_>,
        index: u32,
        value: Local<Value>,
    ) -> Option<bool> {
        self.set(
            scope,
            Local::from_engine(api::Local::string(index.to_string())),
            value,
        )
    }

    /// [[Delete]] (`v8::Object::Delete`).
    pub fn delete(&self, scope: &PinScope<'_, '_>, key: Local<Value>) -> Option<bool> {
        let realm = crate::realm_of(scope);
        match api::Object::delete_key(&realm, self.engine(), key.engine()) {
            Ok(deleted) => Some(deleted),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// The object's prototype (`v8::Object::GetPrototype`), or *null* for an
    /// object with none.
    pub fn get_prototype<'a>(&self, scope: &PinScope<'a, '_>) -> Option<Local<'a, Value>> {
        let realm = crate::realm_of(scope);
        match api::Object::get_prototype(&realm, self.engine()) {
            Ok(prototype) => Some(Local::from_engine(prototype)),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// Set the object's prototype (`v8::Object::SetPrototype`); `prototype` must
    /// be an object or *null*.
    pub fn set_prototype(&self, scope: &PinScope<'_, '_>, prototype: Local<Value>) -> Option<bool> {
        let realm = crate::realm_of(scope);
        match api::Object::set_prototype(&realm, self.engine(), prototype.engine()) {
            Ok(set) => Some(set),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// CreateDataProperty (spec 7.3.4, `v8::Object::CreateDataProperty`): an own
    /// data property, configurable, writable and enumerable.
    pub fn create_data_property(
        &self,
        scope: &PinScope<'_, '_>,
        key: Local<Name>,
        value: Local<Value>,
    ) -> Option<bool> {
        self.define_property(scope, key, &data_property(value))
    }

    /// DefineOwnProperty with the attributes `attr` names
    /// (`v8::Object::DefineOwnProperty`).
    ///
    /// The attributes are the crate we stand in for's mask, and the ones it does
    /// not name are set: a property defined with `READ_ONLY` is not writable, and
    /// one with `DONT_ENUM` is not enumerable.
    pub fn define_own_property(
        &self,
        scope: &PinScope<'_, '_>,
        key: Local<Name>,
        value: Local<Value>,
        attr: PropertyAttribute,
    ) -> Option<bool> {
        let mut descriptor = V8PropertyDescriptor::new_from_value_writable(
            value,
            !attr.has(PropertyAttribute::READ_ONLY),
        );
        descriptor.set_enumerable(!attr.has(PropertyAttribute::DONT_ENUM));
        descriptor.set_configurable(!attr.has(PropertyAttribute::DONT_DELETE));
        self.define_property(scope, key, &descriptor)
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
    ///
    /// Own keys only, whatever `args`' mode says: that is what this call asks
    /// for. [`get_property_names`](Self::get_property_names) is the one that
    /// walks the chain.
    pub fn get_own_property_names<'a>(
        &self,
        scope: &PinScope<'a, '_>,
        args: GetPropertyNamesArgs,
    ) -> Option<Local<'a, Array>> {
        let value = *self.engine().value();
        let object = object_of(&value)?;
        names_of_objects(scope, &value, &[object], args)
    }

    /// The object's property names, including the prototype chain's when `args`
    /// asks for them (`v8::Object::GetPropertyNames`).
    pub fn get_property_names<'a>(
        &self,
        scope: &PinScope<'a, '_>,
        args: GetPropertyNamesArgs,
    ) -> Option<Local<'a, Array>> {
        let value = *self.engine().value();
        let object = object_of(&value)?;
        let mut objects = vec![object];
        if args.mode == KeyCollectionMode::IncludePrototypes {
            let realm = crate::realm_of(scope);
            let mut current: Local<'_, Object> = self.retag();
            loop {
                let prototype = match api::Object::get_prototype(&realm, current.engine()) {
                    Ok(prototype) => prototype,
                    Err(error) => {
                        crate::throw(scope, &error);
                        return None;
                    }
                };
                // The chain ends where the crate's does: at the first value that
                // is not an object, which is `null` for every ordinary chain.
                let Some(next) = object_of(prototype.value()) else {
                    break;
                };
                objects.push(next);
                current = Local::from_engine(prototype);
            }
        }
        names_of_objects(scope, &value, &objects, args)
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

/// The names `args` selects out of the own keys of `objects`, visited in order.
///
/// One object is the own-property call; a chain of them is what
/// `GetPropertyNames` asks for. Which object answers for a key is the whole of
/// the subtlety, and both rules are the crate we stand in for's:
///
/// - A key is reported where it is *first* seen, so an occurrence further along
///   the chain contributes nothing.
/// - A key the *attribute* filter rejects still counts as seen, which is what
///   makes a non-enumerable own property hide an enumerable one further up. The
///   kind filter (strings against symbols) and the index filter do not count,
///   because the crate drops those before its shadowing bookkeeping — and with
///   no key remembered there is nothing for the chain to shadow.
fn names_of_objects<'a>(
    scope: &PinScope<'a, '_>,
    value: &EngineValue,
    objects: &[Handle<JsObject>],
    args: GetPropertyNamesArgs,
) -> Option<Local<'a, Array>> {
    let realm = crate::realm_of(scope);
    let filter = args.property_filter;
    let mut seen: HashSet<PropertyKey> = HashSet::new();
    let mut names: Vec<Local<'a, Value>> = Vec::new();
    for (visited, object) in objects.iter().enumerate() {
        let keys = match realm.with_agent(|agent| {
            if visited == 0 {
                materialize_pending_prototype_value(agent, value)?;
            }
            ensure_deferred_namespace_evaluation(agent, object)?;
            object.own_property_keys()
        }) {
            Ok(keys) => keys,
            Err(error) => {
                crate::throw(scope, &error);
                return None;
            }
        };
        for key in keys {
            let kind_wanted = match &key {
                PropertyKey::Symbol(_) => !filter.is_skip_symbols(),
                PropertyKey::String(_) => !filter.is_skip_strings(),
            };
            if !kind_wanted {
                continue;
            }
            if args.index_filter == IndexFilter::SkipIndices
                && crux::object::array_index_of(&key).is_some()
            {
                continue;
            }
            // The key answers for itself from here on, whether or not the
            // attribute filter keeps it.
            if !seen.insert(key.clone()) {
                continue;
            }
            match key_passes_filter(&filter, object, &key) {
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
    }
    Some(Array::new_with_elements(scope, &names))
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

impl<'s> LocalHandle<'s, Array> {
    /// The array's `length` (`v8::Array::Length`).
    pub fn length(&self) -> u32 {
        let realm = crate::realm_current();
        api::Array::length(&realm, self.engine()).map_or(0, |len| len as u32)
    }
}

impl<'s> LocalHandle<'s, Proxy> {
    /// The proxy's target (`v8::Proxy::GetTarget`).
    ///
    /// A revoked proxy has none. The crate we stand in for's wrapper aborts on
    /// the empty handle its own API answers there, so this aborts too, with the
    /// reason rather than a null dereference.
    pub fn get_target(&self, _scope: &PinScope<'s, '_>) -> Local<'s, Value> {
        self.slot(|slots| *slots.target.borrow())
            .unwrap_or_else(|| panic_proxy_slot("get_target"))
    }

    /// The proxy's handler (`v8::Proxy::GetHandler`), with the same answer for a
    /// revoked proxy as [`get_target`](Self::get_target).
    pub fn get_handler(&self, _scope: &PinScope<'s, '_>) -> Local<'s, Value> {
        self.slot(|slots| *slots.handler.borrow())
            .unwrap_or_else(|| panic_proxy_slot("get_handler"))
    }

    /// The engine value one of the proxy's cells holds, if the proxy is live.
    fn slot(
        &self,
        read: impl FnOnce(&crux::proxy::ProxySlots) -> Option<crux::value::Value>,
    ) -> Option<Local<'s, Value>> {
        let object = self.engine().value().as_object()?;
        let ObjectKind::Proxy(slots) = &object.kind else {
            return None;
        };
        Some(Local::from_engine(api::Local::from(read(slots)?)))
    }
}

/// The abort a revoked proxy's cell asks for.
fn panic_proxy_slot(what: &str) -> Local<'static, Value> {
    panic!("bridge: Proxy::{what} on a proxy with no target or handler")
}

impl<'s> LocalHandle<'s, Map> {
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

impl<'s> LocalHandle<'s, Set> {
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

/// The name of a constructor the prototype chain named, when it is one the crate
/// we stand in for would report: a function whose `name` is neither empty nor
/// "Object" — V8's own helper skips both and keeps walking.
fn constructor_name<'s>(
    realm: &api::Context,
    constructor: &api::Local,
) -> Option<Local<'s, String>> {
    if !constructor.value().is_function() {
        return None;
    }
    let name = api::Object::get(realm, constructor, "name").ok()?;
    let text = name.as_string()?;
    if text.is_empty() || text == "Object" {
        return None;
    }
    Some(Local::from_engine(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Number, Set};
    use crate::handle::Global;
    use crate::property::{GetPropertyNamesArgsBuilder, IndexFilter, KeyCollectionMode};
    use crate::scope::GetIsolate;
    use crate::test_support::{bind, eval, eval_number, in_context};

    /// The number a handle holds.
    fn number_of(value: Local<'_, Value>) -> f64 {
        Local::<Number>::try_from(value).expect("number").value()
    }

    /// The name a constructor gives an object, read the way the crate reads it:
    /// the nearest `constructor` on the prototype chain, with the string
    /// "Object" as the fallback. The last case is the documented divergence — V8
    /// names an iterator by its `Symbol.toStringTag` ("Map Iterator"), while
    /// this bridge reads the name the
    /// engine's iterator prototypes carry ("Iterator").
    #[test]
    fn the_constructor_name_comes_from_the_prototype_chain() {
        in_context!(scope, {
            for (source, expected) in [
                ("({})", "Object"),
                ("[]", "Array"),
                ("new Map()", "Map"),
                ("/x/", "RegExp"),
                ("new Date()", "Date"),
                ("(function () {})", "Function"),
                ("Object.create(null)", "Object"),
                ("class Foo {}; new Foo()", "Foo"),
                ("new Map().entries()", "Iterator"),
            ] {
                let value = eval(scope, source);
                let object = Local::<Object>::try_from(value)
                    .unwrap_or_else(|_| panic!("{source} is not an object"));
                assert_eq!(
                    object.get_constructor_name().to_rust_string_lossy(scope),
                    expected,
                    "{source}"
                );
            }
        });
    }

    /// The text a handle holds.
    fn text_of(scope: &PinScope<'_, '_>, value: Local<'_, Value>) -> std::string::String {
        value.to_rust_string_lossy(scope)
    }

    /// A handle can key a host's table, which needs `Hash` to agree with `==`:
    /// one object under two handles is one key, a different object is another,
    /// and the same holds for a persistent handle. The identity hash a host can
    /// read is the same number the table keys on.
    #[test]
    fn handle_identity_is_what_a_host_table_keys_on() {
        in_context!(scope, {
            let global = Local::<Object>::try_from(eval(scope, "globalThis")).expect("object");
            let same_global = Local::<Object>::try_from(eval(scope, "globalThis")).expect("object");
            let other = Local::<Object>::try_from(eval(scope, "({})")).expect("object");

            assert_eq!(global, same_global);
            assert_eq!(global.get_identity_hash(), same_global.get_identity_hash());
            assert_ne!(global.get_identity_hash(), other.get_identity_hash());

            let mut keys = std::collections::HashSet::new();
            keys.insert(global);
            keys.insert(same_global);
            assert_eq!(
                keys.len(),
                1,
                "one object is one key, however many handles name it"
            );
            keys.insert(other);
            assert_eq!(keys.len(), 2, "and a different object is a different key");
            assert!(keys.contains(&global));

            let isolate = scope.get_isolate_ptr();
            let mut persistent = std::collections::HashSet::new();
            persistent.insert(Global::new(&isolate, global));
            persistent.insert(Global::new(&isolate, same_global));
            assert_eq!(
                persistent.len(),
                1,
                "a `Global` keys the same way a `Local` does"
            );
            assert!(!persistent.contains(&Global::new(&isolate, other)));
        });
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

    /// As [`names_of`], through `GetPropertyNames`, which is the call that may
    /// walk the prototype chain.
    fn chained_names(
        scope: &PinScope<'_, '_>,
        object: &Local<'_, Object>,
        args: GetPropertyNamesArgs,
    ) -> Vec<std::string::String> {
        let names = object.get_property_names(scope, args).expect("names");
        (0..names.length())
            .map(|index| text_of(scope, names.get_index(scope, index).expect("name")))
            .collect()
    }

    /// The chain walk answers a key where it is first seen, and a key the filter
    /// rejects on the nearer object hides the same key further up — which is the
    /// difference between the two modes, and the difference between this and
    /// `get_own_property_names`.
    #[test]
    fn property_names_walk_the_chain_only_when_asked() {
        in_context!(scope, {
            let object = Local::<Object>::try_from(eval(
                scope,
                "globalThis.proto = { inherited: 1, shadowed: 2 };\n\
                 globalThis.child = Object.create(globalThis.proto);\n\
                 Object.defineProperty(globalThis.child, 'shadowed', { value: 3, enumerable: false });\n\
                 globalThis.child.own = 4; globalThis.child",
            ))
            .expect("object");

            let own = GetPropertyNamesArgs {
                mode: KeyCollectionMode::OwnOnly,
                property_filter: PropertyFilter::ONLY_ENUMERABLE,
                index_filter: IndexFilter::IncludeIndices,
                key_conversion: KeyConversionMode::KeepNumbers,
            };
            assert_eq!(names_of(scope, &object, own), vec!["own".to_string()]);

            let chain = GetPropertyNamesArgs {
                mode: KeyCollectionMode::IncludePrototypes,
                property_filter: PropertyFilter::ONLY_ENUMERABLE,
                index_filter: IndexFilter::IncludeIndices,
                key_conversion: KeyConversionMode::KeepNumbers,
            };
            assert_eq!(
                chained_names(scope, &object, chain),
                vec!["own".to_string(), "inherited".to_string()],
                "the chain's keys follow the object's, and the non-enumerable own \
                 `shadowed` keeps the inherited one out"
            );

            // The own-only call is the own-only call, whatever the mode says.
            let chain_again = GetPropertyNamesArgs {
                mode: KeyCollectionMode::IncludePrototypes,
                property_filter: PropertyFilter::ONLY_ENUMERABLE,
                index_filter: IndexFilter::IncludeIndices,
                key_conversion: KeyConversionMode::KeepNumbers,
            };
            assert_eq!(
                names_of(scope, &object, chain_again),
                vec!["own".to_string()],
                "`get_own_property_names` does not walk the chain"
            );
        });
    }

    /// The index filter drops integer-like keys and keeps everything else, which
    /// is the spelling the host this stands in for asks for when it names an
    /// object's properties for a serializer.
    #[test]
    fn the_index_filter_drops_indices_only() {
        in_context!(scope, {
            let object = Local::<Object>::try_from(eval(
                scope,
                "globalThis.o = { 0: 'a', 2: 'b', name: 'c' }; globalThis.o",
            ))
            .expect("object");

            let with = GetPropertyNamesArgs {
                mode: KeyCollectionMode::OwnOnly,
                property_filter: PropertyFilter::ONLY_ENUMERABLE,
                index_filter: IndexFilter::IncludeIndices,
                key_conversion: KeyConversionMode::KeepNumbers,
            };
            assert_eq!(
                names_of(scope, &object, with),
                vec!["0".to_string(), "2".to_string(), "name".to_string()]
            );

            let without = GetPropertyNamesArgs {
                mode: KeyCollectionMode::OwnOnly,
                property_filter: PropertyFilter::ONLY_ENUMERABLE,
                index_filter: IndexFilter::SkipIndices,
                key_conversion: KeyConversionMode::KeepNumbers,
            };
            assert_eq!(names_of(scope, &object, without), vec!["name".to_string()]);
        });
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

    /// A key is a `Name`, so a Symbol resolves as one: found, set, asked about
    /// and deleted. This is the read deno's sequence converter makes first
    /// (`Symbol.iterator` on the value it converts), so a bridge that answered
    /// only string keys made every sequence conversion fail. A key that is not a
    /// `Name` is refused as a pending exception — the check goes last, because
    /// the refusal is what the host sees next.
    #[test]
    fn a_symbol_key_resolves_like_a_name() {
        in_context!(scope, {
            let iterator = crate::data::Symbol::get_iterator(scope).cast::<Value>();
            let array = Local::<Object>::try_from(eval(scope, "[1, 2]")).expect("array");
            let found = array.get(scope, iterator).expect("the symbol key resolves");
            assert!(found.is_function(), "Symbol.iterator is a function");
            assert_eq!(array.has(scope, iterator), Some(true));

            let object = Local::<Object>::try_from(eval(scope, "({})")).expect("object");
            let seven = Number::new(scope, 7.0).cast::<Value>();
            assert_eq!(object.set(scope, iterator, seven), Some(true));
            assert_eq!(object.has_own_property(scope, iterator), Some(true));
            assert_eq!(
                number_of(object.get(scope, iterator).expect("the value set")),
                7.0
            );
            assert_eq!(object.delete(scope, iterator), Some(true));
            assert_eq!(object.has_own_property(scope, iterator), Some(false));

            let three = Number::new(scope, 3.0).cast::<Value>();
            assert!(object.get(scope, three).is_none());
            assert_eq!(object.has(scope, three), None);
        });
    }
}
