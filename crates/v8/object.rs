//! Objects, arrays, and the keyed collections (`v8::Object`, `v8::Array`,
//! `v8::Map`, `v8::Set`).

use std::collections::HashSet;
use std::num::NonZeroI32;

use crate::data::{Array, DataError, Date, Map, Name, Object, Private, Proxy, Set, String, Value};
use crate::handle::{Local, LocalHandle};
use crate::property::{
    GetPropertyNamesArgs, IndexFilter, KeyCollectionMode, KeyConversionMode, PropertyAttribute,
    PropertyFilter,
};
use crate::property_descriptor::PropertyDescriptor as V8PropertyDescriptor;
use crate::scope::PinScope;
use crux::handle::Handle;
use crux::object::{JsObject, ObjectKind, Property, PropertyKind};
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::value::Value as EngineValue;
use crux::value::ValueKind;
use runtime::api;
use runtime::builtins::keyed::MapIterationKind;
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

/// The two integrity levels a host can set (`v8::IntegrityLevel`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrityLevel {
    Frozen,
    Sealed,
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

    /// Whether this object's own private table carries `key`
    /// (`v8::Object::HasPrivate`).
    ///
    /// The own table, not the chain: a private name is the receiver's own, not an
    /// inherited one, and this is the table [`delete_private`](Self::delete_private)
    /// removes from. [`get_private`](Self::get_private) reads through `[[Get]]`,
    /// which is the operation it stands for, so a name set on a prototype is
    /// readable there and not present here — the one asymmetry the symbol-backed
    /// private model carries.
    pub fn has_private(&self, scope: &PinScope<'_, '_>, key: Local<'_, Private>) -> Option<bool> {
        let symbol = key.engine().value().as_symbol()?;
        let key: Local<Value> = Local::from_engine(api::Local::from(EngineValue::Symbol(symbol)));
        let realm = crate::realm_of(scope);
        match api::Object::has_own_key(&realm, self.engine(), key.engine()) {
            Ok(found) => Some(found),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// Delete this object's own private property `key` (`v8::Object::DeletePrivate`),
    /// answering whether the property is gone.
    ///
    /// The answer is `[[Delete]]`'s, as it is for [`delete_index`](Self::delete_index):
    /// `Some(true)` for a delete that succeeded — including a name the object did
    /// not carry, since `[[Delete]]` succeeds when there is nothing to fail on —
    /// `Some(false)` for a refused one, and `None` for an error (a proxy trap that
    /// threw), with it pending.
    pub fn delete_private(
        &self,
        scope: &PinScope<'_, '_>,
        key: Local<'_, Private>,
    ) -> Option<bool> {
        let symbol = key.engine().value().as_symbol()?;
        let key: Local<Value> = Local::from_engine(api::Local::from(EngineValue::Symbol(symbol)));
        let realm = crate::realm_of(scope);
        match api::Object::delete_key(&realm, self.engine(), key.engine()) {
            Ok(deleted) => Some(deleted),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
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
    ///
    /// The key is a `Name`, as it is in the crate we stand in for (and in
    /// `v8.h`): a host's `Local<Value>` key is the divergence, since only a
    /// string or a symbol can be a property key at all.
    pub fn has_own_property(&self, scope: &PinScope<'_, '_>, key: Local<Name>) -> Option<bool> {
        let realm = crate::realm_of(scope);
        match api::Object::has_own_key(&realm, self.engine(), key.engine()) {
            Ok(found) => Some(found),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// [[HasProperty]] for an element index (`v8::Object::HasIndex`).
    ///
    /// The index goes in as its decimal string, the same key
    /// [`delete_index`](Self::delete_index), [`get_index`](Self::get_index) and
    /// [`set_index`](Self::set_index) reach for, so a host's indexed question
    /// reaches the property an indexed access reaches, on an array or on a plain
    /// object.
    pub fn has_index(&self, scope: &PinScope<'_, '_>, index: u32) -> Option<bool> {
        let realm = crate::realm_of(scope);
        let key: Local<Value> = Local::from_engine(api::Local::string(index.to_string()));
        match api::Object::has_key(&realm, self.engine(), key.engine()) {
            Ok(present) => Some(present),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// Whether this object's *ordinary* own properties carry `key`, with a host
    /// object's interceptor skipped (`v8::Object::HasRealNamedProperty`).
    pub fn has_real_named_property(
        &self,
        scope: &PinScope<'_, '_>,
        key: Local<Name>,
    ) -> Option<bool> {
        match self.real_own_property(&property_key(&key)) {
            Ok(property) => Some(property.is_some()),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// The value of this object's ordinary own property `key`, with a host
    /// object's interceptor skipped (`v8::Object::GetRealNamedProperty`).
    ///
    /// An accessor's getter runs with this object as the receiver — which is what
    /// makes this need a scope, and what lets it throw — and the empty handle
    /// means the object carries no such own property. `deno/ext/node`'s `vm`
    /// reaches for this name inside its own interceptor callbacks, where the
    /// whole point is that the question must not come back to them.
    pub fn get_real_named_property<'a>(
        &self,
        scope: &PinScope<'a, '_>,
        key: Local<Name>,
    ) -> Option<Local<'a, Value>> {
        let property = match self.real_own_property(&property_key(&key)) {
            Ok(Some(property)) => property,
            Ok(None) => return None,
            Err(error) => {
                crate::throw(scope, &error);
                return None;
            }
        };
        match property.kind {
            PropertyKind::Data { value, .. } => Some(Local::from_engine(api::Local::from(value))),
            PropertyKind::Accessor {
                get: Some(getter), ..
            } => {
                let realm = crate::realm_of(scope);
                let receiver = *self.engine().value();
                match realm.with_agent(|_agent| crux::function::call(&getter, receiver, &[])) {
                    Ok(value) => Some(Local::from_engine(api::Local::from(value))),
                    Err(error) => {
                        crate::throw(scope, &error);
                        None
                    }
                }
            }
            PropertyKind::Accessor { get: None, .. } => {
                Some(Local::from_engine(api::Local::undefined()))
            }
        }
    }

    /// The attributes of this object's ordinary own property `key`, or nothing
    /// when it carries none (`v8::Object::GetRealNamedPropertyAttributes`).
    pub fn get_real_named_property_attributes(
        &self,
        scope: &PinScope<'_, '_>,
        key: Local<Name>,
    ) -> Option<PropertyAttribute> {
        match self.real_own_property(&property_key(&key)) {
            Ok(Some(property)) => Some(attributes_of(&property)),
            Ok(None) => None,
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// The attributes of a property of this object
    /// (`v8::Object::GetPropertyAttributes`), `NONE` when it carries none.
    ///
    /// The same own-only, interceptor-skipping query as
    /// [`get_real_named_property_attributes`](Self::get_real_named_property_attributes);
    /// the difference is the one the crate documents for this name — a property
    /// the object does not carry answers `NONE` rather than nothing. A key that is
    /// not a name has no attributes either, so a `Value` the crate would coerce
    /// here is `NONE` instead.
    pub fn get_property_attributes(
        &self,
        scope: &PinScope<'_, '_>,
        key: Local<Value>,
    ) -> Option<PropertyAttribute> {
        let Ok(name) = Local::<Name>::try_from(key) else {
            return Some(PropertyAttribute::NONE);
        };
        Some(
            self.get_real_named_property_attributes(scope, name)
                .unwrap_or_default(),
        )
    }

    /// Delete this object's element at `index` (`v8::Object::DeleteIndex`), with
    /// the object's interceptor asked as the ordinary `[[Delete]]` does.
    pub fn delete_index(&self, scope: &PinScope<'_, '_>, index: u32) -> Option<bool> {
        let realm = crate::realm_of(scope);
        let key: Local<Value> = Local::from_engine(api::Local::string(index.to_string()));
        match api::Object::delete_key(&realm, self.engine(), key.engine()) {
            Ok(deleted) => Some(deleted),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// The context of the realm whose global object this object is
    /// (`v8::Object::GetCreationContext`), or nothing.
    ///
    /// Narrowed, and the method says how: V8 answers the context an object was
    /// *created* in, for any object, and this engine records no creation realm. So
    /// a realm's global object answers its own context — which is the case a
    /// host-defined global's callback is handed, and what `deno/ext/node`'s `vm`
    /// asks of the holder of a property operation — and anything else answers
    /// nothing, which makes the caller take its own other branch rather than be
    /// told a realm that is not the object's.
    pub fn get_creation_context<'a>(
        &self,
        scope: &PinScope<'a, '_>,
    ) -> Option<Local<'a, crate::data::Context>> {
        let isolate = crate::realm_of(scope).isolate();
        // SAFETY: the isolate a context names is live for as long as the context,
        // and the scope that produced it is open here.
        let context = unsafe { api::Context::of_global_object(isolate, self.engine().value()) }?;
        Some(Local::from_payload(crate::handle::Payload::Context(
            context,
        )))
    }

    /// This object's ordinary own property `key`, a host object's interceptor
    /// skipped — the shared first step of the four "real" methods.
    ///
    /// `None` is "the ordinary own property table does not carry it", which is
    /// not an error even for a host object whose interceptor would have answered;
    /// a value this engine cannot read an own table from is the same `None`, as
    /// V8 refuses a non-object the same way.
    fn real_own_property(
        &self,
        key: &PropertyKey,
    ) -> Result<Option<Property>, crux::error::JsError> {
        let value = *self.engine().value();
        match value.as_object() {
            Some(object) => object.ordinary_get_own_property(key),
            None => Ok(None),
        }
    }

    /// The own-property descriptor (`v8::Object::GetOwnPropertyDescriptor`), or
    /// the empty handle when the object has no such own property.
    ///
    /// The answer is the plain descriptor object V8 builds — only the fields the
    /// property carries — made by the `Object.getOwnPropertyDescriptor` builtin's
    /// own path, so a module namespace reports the live binding rather than the
    /// placeholder value its object stores.
    pub fn get_own_property_descriptor<'a>(
        &self,
        scope: &PinScope<'a, '_>,
        key: Local<'_, Name>,
    ) -> Option<Local<'a, Value>> {
        let value = *self.engine().value();
        let key = property_key(&key);
        let realm = crate::realm_of(scope);
        match realm.with_agent(|agent| {
            runtime::builtins::object::own_property_descriptor(agent, &value, &key)
        }) {
            Ok(descriptor) if descriptor.is_undefined() => None,
            Ok(descriptor) => Some(Local::from_engine(api::Local::from(descriptor))),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// SetIntegrityLevel (`v8::Object::SetIntegrityLevel`, spec 7.3.15).
    ///
    /// `Frozen` makes every own property non-writable and non-configurable and
    /// the object non-extensible; `Sealed` stops at non-configurable. `Ok(false)`
    /// is a refused `[[PreventExtensions]]`, which is the crate we stand in
    /// for's answer too; `Err` is a thrown trap, with the exception pending.
    pub fn set_integrity_level(
        &self,
        scope: &PinScope<'_, '_>,
        level: IntegrityLevel,
    ) -> Result<bool, DataError> {
        let value = *self.engine().value();
        let freeze = matches!(level, IntegrityLevel::Frozen);
        let realm = crate::realm_of(scope);
        match realm.with_agent(|agent| {
            runtime::builtins::object::set_integrity_level(agent, &value, freeze)
        }) {
            Ok(status) => Ok(status),
            Err(error) => {
                crate::throw(scope, &error);
                Err(DataError::GenericFailure)
            }
        }
    }

    /// The object's internal entries (`v8::Object::PreviewEntries`), for a
    /// console: a flat array of what the engine holds for this object, and
    /// whether that array is key/value pairs rather than a flat list of
    /// elements.
    ///
    /// Only the four keyed collections and map and set iterators have internal
    /// entries. Everything else — an ordinary object, a generator suspended at a
    /// `yield` — answers the empty handle, which is the answer a console's
    /// ordinary preview handles; see [`preview_entries_of`] for what each kind
    /// contributes and why a plain `Set` answers its values once.
    pub fn preview_entries<'a>(
        &self,
        scope: &PinScope<'a, '_>,
    ) -> (Option<Local<'a, Array>>, bool) {
        let Some(object) = self.engine().value().as_object() else {
            return (None, false);
        };
        let id = object.id();
        let answer = crate::realm::with_agent(|agent| preview_entries_of(agent, id)).flatten();
        let Some((values, is_key_value)) = answer else {
            return (None, false);
        };
        let locals: Vec<Local<'_, Value>> = values
            .iter()
            .map(|value| Local::from_engine(api::Local::from(*value)))
            .collect();
        (Some(Array::new_with_elements(scope, &locals)), is_key_value)
    }

    /// The object's own property names, filtered and spelled as `args` asks
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

/// The crate's attribute bits for an engine property (`v8::PropertyAttribute`).
///
/// `READ_ONLY` is "cannot be written": a data property that is not writable, or
/// an accessor with no setter — the same question a store asks of the property,
/// which is what makes these the bits V8 hands an interceptor's `query` callback
/// to report back.
fn attributes_of(property: &Property) -> PropertyAttribute {
    let mut attributes = PropertyAttribute::NONE;
    match &property.kind {
        PropertyKind::Data {
            writable: false, ..
        } => attributes |= PropertyAttribute::READ_ONLY,
        PropertyKind::Accessor { set: None, .. } => attributes |= PropertyAttribute::READ_ONLY,
        _ => {}
    }
    if !property.enumerable {
        attributes |= PropertyAttribute::DONT_ENUM;
    }
    if !property.configurable {
        attributes |= PropertyAttribute::DONT_DELETE;
    }
    attributes
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

impl Set {
    /// A new empty set in the scope's realm (`v8::Set::new`).
    ///
    /// The `%Set%` intrinsic is constructed, so the set is the one a script
    /// would make: the engine has no constructor that builds a collection
    /// without going through it, and a host's set must be indistinguishable
    /// from a script's.
    pub fn new<'s>(scope: &PinScope<'s, '_, ()>) -> Local<'s, Set> {
        let realm = crate::realm_of(scope);
        let constructor = realm
            .intrinsic("%Set%")
            .unwrap_or_else(|| panic!("bridge bug: the realm has no %Set% intrinsic"));
        match realm.try_construct(&api::Local::from(constructor), &[]) {
            Ok(set) => Local::from_engine(set),
            Err(error) => {
                crate::throw(scope, &error);
                panic!("bridge: creating a set failed: {error}");
            }
        }
    }
}

impl<'s> LocalHandle<'s, Map> {
    /// The number of live entries (`v8::Map::size`).
    ///
    /// A deleted-but-not-yet-rebuilt slot is not an entry, which is the same
    /// tombstone rule [`as_array`](Self::as_array) follows; the count is the
    /// engine's own, read while the realm is entered.
    pub fn size(&self) -> usize {
        let Some(object) = self.engine().value().as_object() else {
            return 0;
        };
        let id = object.id();
        crate::realm::with_agent(|agent| {
            agent.map_data.get(&id).map_or(0, |cell| {
                cell.borrow().entries.iter().filter(|e| e.is_some()).count()
            })
        })
        .unwrap_or(0)
    }

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
    /// The number of live elements (`v8::Set::size`), by the same tombstone
    /// rule as [`as_array`](Self::as_array).
    pub fn size(&self) -> usize {
        let Some(object) = self.engine().value().as_object() else {
            return 0;
        };
        let id = object.id();
        crate::realm::with_agent(|agent| {
            agent.set_data.get(&id).map_or(0, |cell| {
                cell.borrow().entries.iter().filter(|e| e.is_some()).count()
            })
        })
        .unwrap_or(0)
    }

    /// Add an element (`v8::Set::add`), answering the set so a caller can chain.
    ///
    /// The add runs as `%Set.prototype.add%` with the set as its receiver, which
    /// is the only route that keeps the engine's own element index and its
    /// SameValueZero normalization (`-0` stored as `+0`, a duplicate adding
    /// nothing). It is the *intrinsic* rather than the prototype's property, so a
    /// script that reassigns `Set.prototype.add` does not change what this calls;
    /// `None` is a trap that threw, with the exception pending.
    pub fn add(&self, scope: &PinScope<'_, '_>, value: Local<'_, Value>) -> Option<Local<'s, Set>> {
        let realm = crate::realm_of(scope);
        let add = realm.intrinsic("%Set.prototype.add%").unwrap_or_else(|| {
            panic!("bridge bug: the realm has no %Set.prototype.add% intrinsic")
        });
        match realm.try_call(&api::Local::from(add), self.engine(), &[*value.engine()]) {
            Ok(_) => Some(self.retag()),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

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

impl<'s> LocalHandle<'s, Date> {
    /// The time value in milliseconds since the epoch (`v8::Date::ValueOf`).
    ///
    /// Read from the engine's per-Date table, the same one
    /// `Date.prototype.valueOf` reads; `NaN` for a date whose time was never set
    /// (an invalid date), which is what the engine stores for one.
    pub fn value_of(&self) -> f64 {
        let Some(object) = self.engine().value().as_object() else {
            return f64::NAN;
        };
        let id = object.id();
        crate::realm::with_agent(|agent| agent.date_data.get(&id).copied())
            .flatten()
            .unwrap_or(f64::NAN)
    }
}

/// The internal entries of the object `id` names, in the flat shape a console
/// reads: the values, and whether they are key/value pairs. `None` is an object
/// the engine holds no entries for.
///
/// The four keyed collections read their own tables, skipping tombstones exactly
/// as [`Map::as_array`] and [`Set::as_array`] do, and a map or set *iterator*
/// reads the collection it walks from its next index on — what a preview of an
/// iterator is. Two shapes are deliberately not `as_array`'s: a plain `Set`
/// answers each value once where `as_array` repeats it (the console reads a flat
/// list, the array API answers key/value pairs), and a map iterator's entries
/// follow its `[[MapIterationKind]]`, so a key-only iterator previews the keys
/// it yields and a value-only one the values, which is what the engine's own
/// `from_code` decides.
fn preview_entries_of(agent: &runtime::Agent, id: u64) -> Option<(Vec<EngineValue>, bool)> {
    if let Some(cell) = agent.map_data.get(&id) {
        let entries = cell
            .borrow()
            .entries
            .iter()
            .flatten()
            .flat_map(|(key, value)| [*key, *value])
            .collect();
        return Some((entries, true));
    }
    if let Some(cell) = agent.set_data.get(&id) {
        let values = cell.borrow().entries.iter().flatten().copied().collect();
        return Some((values, false));
    }
    if let Some(cell) = agent.weak_map_data.get(&id) {
        let entries = cell
            .borrow()
            .iter()
            .flatten()
            .flat_map(|(key, value)| [*key, *value])
            .collect();
        return Some((entries, true));
    }
    if let Some(cell) = agent.weak_set_data.get(&id) {
        let values = cell.borrow().iter().flatten().copied().collect();
        return Some((values, false));
    }
    if let Some(cell) = agent.map_iter_data.get(&id) {
        let (map, index, code) = *cell.borrow();
        let kind = MapIterationKind::from_code(code);
        // An exhausted iterator (or one whose Map was collected, which cannot
        // happen while it is held) has no entries left rather than none at all.
        let Some(map) = map.and_then(|map| map.as_object()) else {
            return Some((Vec::new(), true));
        };
        let Some(cell) = agent.map_data.get(&map.id()) else {
            return Some((Vec::new(), true));
        };
        let data = cell.borrow();
        let mut entries = Vec::new();
        for entry in data.entries[index.min(data.entries.len())..]
            .iter()
            .flatten()
        {
            match kind {
                MapIterationKind::KeyValue => entries.extend([entry.0, entry.1]),
                MapIterationKind::Key => entries.push(entry.0),
                MapIterationKind::Value => entries.push(entry.1),
            }
        }
        return Some((entries, true));
    }
    if let Some(cell) = agent.set_iter_data.get(&id) {
        let (set, index, _kind) = *cell.borrow();
        let Some(set) = set.and_then(|set| set.as_object()) else {
            return Some((Vec::new(), false));
        };
        let Some(cell) = agent.set_data.get(&set.id()) else {
            return Some((Vec::new(), false));
        };
        let data = cell.borrow();
        let values = data.entries[index.min(data.entries.len())..]
            .iter()
            .flatten()
            .copied()
            .collect();
        return Some((values, false));
    }
    None
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
    use crate::support::MapFnTo;
    use crate::test_support::{bind, eval, eval_number, in_context};
    use crate::{Intercepted, ReturnValue};

    /// The "real" questions skip the interceptor: a host object whose handler
    /// answers `echoed` is asked by `get`/`has` and *not* by
    /// `get_real_named_property`/`has_real_named_property`/
    /// `get_real_named_property_attributes`, while an ordinary own property is
    /// found by both. The last assertion is the one that tells
    /// `get_property_attributes` from its "real" sibling: the crate documents that
    /// it answers `NONE` where the other answers nothing.
    #[test]
    fn the_real_property_questions_skip_the_interceptor() {
        fn answers_echoed<'s>(
            scope: &mut PinScope<'s, '_>,
            key: Local<'s, Name>,
            _args: crate::interceptor::PropertyCallbackArguments<'s>,
            rv: ReturnValue<'s, Value>,
        ) -> Intercepted {
            let echoed = match key.engine().value().kind() {
                ValueKind::String(text) => {
                    PropertyKey::from_js_string(&text) == PropertyKey::from_utf8("echoed")
                }
                _ => false,
            };
            if !echoed {
                return Intercepted::kNo;
            }
            // A descriptor object, which is what the engine reads out of this
            // callback's answer (`crux::property::to_property_descriptor`).
            rv.set(eval(
                scope,
                "({ value: 7, writable: true, enumerable: true, configurable: true })",
            ));
            Intercepted::kYes
        }

        let isolate = &mut crate::Isolate::new(crate::CreateParams::default());
        crate::scope!(let handle_scope, isolate);
        let template = crate::ObjectTemplate::new(handle_scope);
        template.set_named_property_handler(
            crate::interceptor::NamedPropertyHandlerConfiguration::new()
                .descriptor_raw(answers_echoed.map_fn_to()),
        );
        let context = crate::Context::new(
            handle_scope,
            crate::ContextOptions {
                global_template: Some(template),
                ..Default::default()
            },
        );
        let scope = &mut crate::ContextScope::new(handle_scope, context);
        let global = context.global(scope);

        // The interceptor answers it, so the ordinary questions see it…
        eval(scope, "globalThis.plain = 1;");
        assert_eq!(eval_number(scope, "globalThis.echoed"), 7.0);
        let echoed: Local<Value> = crate::String::new(scope, "echoed").unwrap().into();
        let echoed = match Local::<Name>::try_from(echoed) {
            Ok(name) => name,
            Err(_) => panic!("a name"),
        };
        let plain = match Local::<Name>::try_from(Local::<Value>::from(
            crate::String::new(scope, "plain").unwrap(),
        )) {
            Ok(name) => name,
            Err(_) => panic!("a name"),
        };

        assert_eq!(
            global
                .get(scope, echoed.into())
                .map(|value| value.is_undefined()),
            Some(false)
        );
        assert_eq!(
            global.has_real_named_property(scope, echoed),
            Some(false),
            "the interceptor's own answer is not a real named property"
        );
        assert!(
            global.get_real_named_property(scope, echoed).is_none(),
            "and it is not a real named property's value"
        );
        assert_eq!(
            global.get_real_named_property_attributes(scope, echoed),
            None
        );
        assert_eq!(
            global.get_property_attributes(scope, echoed.into()),
            Some(crate::PropertyAttribute::NONE),
            "this one answers NONE where the other answers nothing"
        );

        // …and the ordinary own property is visible to both questions.
        assert_eq!(global.has_real_named_property(scope, plain), Some(true));
        assert_eq!(
            global
                .get_real_named_property(scope, plain)
                .and_then(|value| value.to_number(scope))
                .map(|number| number.value()),
            Some(1.0)
        );
        assert_eq!(
            global.get_real_named_property_attributes(scope, plain),
            Some(crate::PropertyAttribute::NONE),
            "plain is writable, enumerable and configurable"
        );
    }

    /// `get_creation_context` answers a realm's global object — the case a
    /// host-defined global's callback is handed — and nothing for an object this
    /// engine keeps no realm for, which is the narrowing the method documents.
    /// `delete_index` removes the element it names and reports whether it did.
    #[test]
    fn a_globals_creation_context_is_its_realm_and_delete_index_deletes() {
        in_context!(scope, {
            let context = scope.get_current_context();
            let global = context.global(scope);
            let found = global
                .get_creation_context(scope)
                .expect("the realm's context");
            assert_eq!(
                found.get_aligned_pointer_from_embedder_data(0),
                context.get_aligned_pointer_from_embedder_data(0),
                "the same context object, told apart by a slot of its own"
            );

            let object = Local::<Object>::try_from(eval(scope, "({})")).expect("an object");
            assert!(
                object.get_creation_context(scope).is_none(),
                "an object that is no realm's global has none here"
            );

            let array_value = eval(scope, "[1, 2, 3]");
            let array = Local::<Object>::try_from(array_value).expect("an array");
            bind(scope, "arr", array_value);
            assert_eq!(eval_number(scope, "1 in arr ? 1 : 0"), 1.0, "the control");
            assert_eq!(array.delete_index(scope, 1), Some(true));
            assert_eq!(
                eval_number(scope, "1 in arr ? 1 : 0"),
                0.0,
                "the element the delete named is gone"
            );
        });
    }

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

    /// The presence question for an element is the chain question an indexed read
    /// is: present where the read finds a value, absent where it finds a hole —
    /// which is what tells an element from a hole, since `get_index` answers
    /// `undefined` for both.
    #[test]
    fn has_index_answers_for_an_element() {
        in_context!(scope, {
            let array =
                Local::<Array>::try_from(eval(scope, "globalThis.a = [7, 8, 9]")).expect("array");
            assert_eq!(array.has_index(scope, 1), Some(true));
            assert_eq!(array.has_index(scope, 9), Some(false));

            // A hole is absent where a read is `undefined`.
            let holey =
                Local::<Array>::try_from(eval(scope, "globalThis.h = [, 1]")).expect("array");
            assert_eq!(holey.has_index(scope, 0), Some(false), "a hole is absent");
            assert!(holey.get_index(scope, 0).expect("value").is_undefined());

            // The chain is walked, as `[[HasProperty]]` is: an element inherited
            // from the prototype is present.
            let derived =
                Local::<Object>::try_from(eval(scope, "Object.create([9])")).expect("object");
            assert_eq!(derived.has_index(scope, 0), Some(true));
            assert_eq!(number_of(derived.get_index(scope, 0).expect("value")), 9.0);
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
            assert_eq!(object.has_own_property(scope, iterator.cast()), Some(true));
            assert_eq!(
                number_of(object.get(scope, iterator).expect("the value set")),
                7.0
            );
            assert_eq!(object.delete(scope, iterator), Some(true));
            assert_eq!(object.has_own_property(scope, iterator.cast()), Some(false));

            let three = Number::new(scope, 3.0).cast::<Value>();
            assert!(object.get(scope, three).is_none());
            assert_eq!(object.has(scope, three), None);
        });
    }

    /// A keyed collection's count is its live entries: a deleted key leaves a
    /// tombstone the count must not see, which is the same rule `as_array`
    /// follows.
    #[test]
    fn a_keyed_collection_reports_its_live_count() {
        in_context!(scope, {
            let map =
                Local::<Map>::try_from(eval(scope, "new Map([[1, 'a'], [2, 'b']])")).expect("map");
            assert_eq!(map.size(), 2);

            let with_a_delete = Local::<Map>::try_from(eval(
                scope,
                "(() => { const m = new Map([[1, 'a'], [2, 'b']]); m.delete(1); return m; })()",
            ))
            .expect("map");
            assert_eq!(with_a_delete.size(), 1, "the tombstone is not an entry");

            let set = Local::<Set>::try_from(eval(scope, "new Set([1, 2, 3])")).expect("set");
            assert_eq!(set.size(), 3);
        });
    }

    /// A set made through the bridge is a real `Set` — the same constructor and
    /// prototype a script sees — and a script's own additions show up in the
    /// host's count.
    #[test]
    fn a_new_set_is_the_one_a_script_would_make() {
        in_context!(scope, {
            let set = Set::new(scope);
            assert_eq!(set.size(), 0, "a new set is empty");
            bind(scope, "host_set", set.into());
            assert_eq!(eval_number(scope, "host_set instanceof Set ? 1 : 0"), 1.0);
            eval(scope, "host_set.add(7)");
            assert_eq!(set.size(), 1, "the host reads what the script added");
        });
    }

    /// An own-property descriptor is the plain descriptor object — the fields
    /// the property carries and no others — and an absent property is the empty
    /// handle rather than a descriptor full of `undefined`.
    #[test]
    fn an_own_property_descriptor_is_the_plain_object() {
        in_context!(scope, {
            let object = Local::<Object>::try_from(eval(scope, "({ a: 1 })")).expect("object");
            let key: Local<'_, Name> = String::new(scope, "a").expect("string").into();
            let descriptor = object
                .get_own_property_descriptor(scope, key)
                .expect("descriptor");
            bind(scope, "descriptor", descriptor);
            assert_eq!(eval_number(scope, "descriptor.value"), 1.0);
            assert_eq!(eval_number(scope, "descriptor.writable ? 1 : 0"), 1.0);
            assert_eq!(eval_number(scope, "descriptor.enumerable ? 1 : 0"), 1.0);
            assert_eq!(eval_number(scope, "descriptor.configurable ? 1 : 0"), 1.0);

            let missing: Local<'_, Name> = String::new(scope, "b").expect("string").into();
            assert!(
                object.get_own_property_descriptor(scope, missing).is_none(),
                "an absent property is the empty handle"
            );

            // An accessor reports `get` and `set` and carries no `value`.
            let accessor = Local::<Object>::try_from(eval(scope, "({ get g() { return 2; } })"))
                .expect("object");
            let key: Local<'_, Name> = String::new(scope, "g").expect("string").into();
            let descriptor = accessor
                .get_own_property_descriptor(scope, key)
                .expect("descriptor");
            bind(scope, "accessor_descriptor", descriptor);
            assert_eq!(eval_number(scope, "accessor_descriptor.get()"), 2.0);
            assert_eq!(
                eval_number(scope, "accessor_descriptor.value === undefined ? 1 : 0"),
                1.0
            );
        });
    }

    /// The two integrity levels do what their names say: sealed stops at
    /// non-configurable, frozen also stops at non-writable — and the engine's
    /// own predicates agree with both.
    #[test]
    fn set_integrity_level_seals_and_freezes() {
        in_context!(scope, {
            let sealed = Local::<Object>::try_from(eval(scope, "({ a: 1 })")).expect("object");
            assert!(
                sealed
                    .set_integrity_level(scope, IntegrityLevel::Sealed)
                    .expect("sealed")
            );
            bind(scope, "sealed", sealed.into());
            assert_eq!(eval_number(scope, "Object.isSealed(sealed) ? 1 : 0"), 1.0);
            assert_eq!(eval_number(scope, "Object.isFrozen(sealed) ? 1 : 0"), 0.0);

            let frozen = Local::<Object>::try_from(eval(scope, "({ a: 1 })")).expect("object");
            assert!(
                frozen
                    .set_integrity_level(scope, IntegrityLevel::Frozen)
                    .expect("frozen")
            );
            bind(scope, "frozen", frozen.into());
            assert_eq!(eval_number(scope, "Object.isFrozen(frozen) ? 1 : 0"), 1.0);
            assert_eq!(
                eval_number(scope, "Object.isExtensible(frozen) ? 1 : 0"),
                0.0
            );

            // A primitive is returned unchanged and is trivially frozen, which
            // is what the spec's step 1 says rather than a special case here.
            let number = Local::<Object>::try_from(eval(scope, "new Number(1)")).expect("object");
            assert!(
                number
                    .set_integrity_level(scope, IntegrityLevel::Frozen)
                    .expect("frozen")
            );
        });
    }

    /// A date's time value is the one it was made with; an invalid date is `NaN`,
    /// which is what the engine stores for one.
    #[test]
    fn a_date_reports_its_time_value() {
        in_context!(scope, {
            let date = Local::<Date>::try_from(eval(scope, "new Date(1234567890)")).expect("date");
            assert_eq!(date.value_of(), 1234567890.0);

            let invalid = Local::<Date>::try_from(eval(scope, "new Date(NaN)")).expect("date");
            assert!(invalid.value_of().is_nan());
        });
    }

    /// `Set::add` runs the engine's own add, so what it changes is a real set: a
    /// script sees every element, a duplicate adds nothing, and `-0` is stored as
    /// `+0` — the normalization the element index is built on.
    #[test]
    fn a_set_add_is_the_engines_own_add() {
        in_context!(scope, {
            let set = Set::new(scope);
            let one = Number::new(scope, 1.0).cast::<Value>();
            assert!(set.add(scope, one).is_some());
            assert_eq!(set.size(), 1);

            let duplicate = Number::new(scope, 1.0).cast::<Value>();
            assert!(set.add(scope, duplicate).is_some());
            assert_eq!(set.size(), 1, "SameValueZero dedupes");

            let negative_zero = Number::new(scope, -0.0).cast::<Value>();
            set.add(scope, negative_zero);
            let zero = Number::new(scope, 0.0).cast::<Value>();
            set.add(scope, zero);
            assert_eq!(set.size(), 2, "-0 stored as +0 is the same element");

            bind(scope, "host_set", set.into());
            assert_eq!(eval_number(scope, "host_set.size"), 2.0);
            assert_eq!(eval_number(scope, "host_set.has(1) ? 1 : 0"), 1.0);
            assert_eq!(
                eval_number(scope, "Object.is([...host_set][1], 0) ? 1 : 0"),
                1.0,
                "the element the host added as -0 reads back as +0"
            );
        });
    }

    /// `preview_entries` answers the flat internal entries a console reads:
    /// flattened pairs for the key/value kinds, single values for the sets, the
    /// *remaining* entries for an iterator, and nothing for anything else.
    #[test]
    fn preview_entries_answers_the_consoles_internal_entries() {
        in_context!(scope, {
            let map = Local::<Object>::try_from(eval(scope, "new Map([[1, 'a'], [2, 'b']])"))
                .expect("map");
            let (entries, is_key_value) = map.preview_entries(scope);
            assert!(is_key_value, "a map is a key/value collection");
            bind(scope, "preview", entries.expect("entries").into());
            assert_eq!(eval_number(scope, "preview.length"), 4.0, "flat pairs");
            assert_eq!(eval_number(scope, "preview[0]"), 1.0);
            assert_eq!(eval_number(scope, "preview[3] === 'b' ? 1 : 0"), 1.0);

            // A set answers each value once, where `as_array` repeats it.
            let set = Local::<Object>::try_from(eval(scope, "new Set([1, 2, 3])")).expect("set");
            let (entries, is_key_value) = set.preview_entries(scope);
            assert!(!is_key_value, "a set is not a key/value collection");
            bind(scope, "preview", entries.expect("entries").into());
            assert_eq!(eval_number(scope, "preview.length"), 3.0, "once each");
            assert_eq!(eval_number(scope, "preview[2]"), 3.0);

            let weak_map =
                Local::<Object>::try_from(eval(scope, "new WeakMap([[{}, 'v']])")).expect("map");
            let (entries, is_key_value) = weak_map.preview_entries(scope);
            assert!(is_key_value);
            bind(scope, "preview", entries.expect("entries").into());
            assert_eq!(eval_number(scope, "preview.length"), 2.0);
            assert_eq!(eval_number(scope, "preview[1] === 'v' ? 1 : 0"), 1.0);

            let weak_set =
                Local::<Object>::try_from(eval(scope, "new WeakSet([{}])")).expect("set");
            let (entries, is_key_value) = weak_set.preview_entries(scope);
            assert!(!is_key_value);
            bind(scope, "preview", entries.expect("entries").into());
            assert_eq!(eval_number(scope, "preview.length"), 1.0);

            // An iterator previews what it has left, not what it started with.
            let consumed = Local::<Object>::try_from(eval(
                scope,
                "(() => { const it = new Map([[1, 'a'], [2, 'b']]).entries(); it.next(); return it; })()",
            ))
            .expect("iterator");
            let (entries, is_key_value) = consumed.preview_entries(scope);
            assert!(is_key_value);
            bind(scope, "preview", entries.expect("entries").into());
            assert_eq!(
                eval_number(scope, "preview.length"),
                2.0,
                "one entry was consumed"
            );
            assert_eq!(
                eval_number(scope, "preview[0]"),
                2.0,
                "the second entry remains"
            );

            // A key-only map iterator yields keys, not pairs — while still
            // reporting key/value, as every map iterator does.
            let keys =
                Local::<Object>::try_from(eval(scope, "new Map([[1, 'a'], [2, 'b']]).keys()"))
                    .expect("iterator");
            let (entries, is_key_value) = keys.preview_entries(scope);
            assert!(is_key_value);
            bind(scope, "preview", entries.expect("entries").into());
            assert_eq!(eval_number(scope, "preview.length"), 2.0, "one key each");
            assert_eq!(eval_number(scope, "preview[1]"), 2.0);

            let set_iterator = Local::<Object>::try_from(eval(scope, "new Set([7, 8]).values()"))
                .expect("iterator");
            let (entries, is_key_value) = set_iterator.preview_entries(scope);
            assert!(!is_key_value);
            bind(scope, "preview", entries.expect("entries").into());
            assert_eq!(eval_number(scope, "preview.length"), 2.0);
            assert_eq!(eval_number(scope, "preview[1]"), 8.0);

            // No internal entries: an ordinary object, and a generator — the
            // documented gap, since V8 reads one out of the debugger's own table.
            let plain = Local::<Object>::try_from(eval(scope, "({ a: 1 })")).expect("object");
            assert!(plain.preview_entries(scope).0.is_none());
            let generator = Local::<Object>::try_from(eval(scope, "(function* () { yield 1; })()"))
                .expect("generator");
            assert!(generator.preview_entries(scope).0.is_none());
        });
    }
}
