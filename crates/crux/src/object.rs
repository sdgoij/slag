//! Ordinary and exotic objects (spec ch. 10).
//!
//! Phase 5 completes the object model: properties are data or accessor
//! descriptors (spec 6.2.5) with full `ValidateAndApplyPropertyDescriptor`
//! semantics (10.1.6.4), and every essential internal method dispatches on
//! the object's `ObjectKind` with ordinary fallthrough. Exotic objects:
//! Array (10.4.2, length/index synchronization), String (10.4.3, virtual
//! code-unit properties), and Arguments (10.4.4, mapped parameters).
//! Integer-Indexed, Proxy, and Module namespace exotics join with their
//! owning phases (12, 16, 7).

use std::cell::{Cell, RefCell};
use std::fmt;
use std::rc::Weak;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{ErrorKind, JsError};
use crate::function::call;
use crate::handle::Handle;
use crate::ops::{same_value, same_value_zero};
use crate::property::{PropertyDescriptor, PropertyKey};
use crate::string::{JsString, lookup};
use crate::symbol::well_known;
use crate::value::{Value, is_callable};

static NEXT_OBJECT_ID: AtomicU64 = AtomicU64::new(1);

/// The data/accessor union of a stored property (spec 6.2.5 table).
#[derive(Debug, Clone, PartialEq)]
pub enum PropertyKind {
    Data {
        value: Value,
        writable: bool,
    },
    Accessor {
        get: Option<Value>,
        set: Option<Value>,
    },
}

/// A fully populated own property: attributes plus a data or accessor value.
#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    pub kind: PropertyKind,
    pub enumerable: bool,
    pub configurable: bool,
}

impl Property {
    pub fn data(value: Value, writable: bool, enumerable: bool, configurable: bool) -> Self {
        Self {
            kind: PropertyKind::Data { value, writable },
            enumerable,
            configurable,
        }
    }

    pub fn accessor(
        get: Option<Value>,
        set: Option<Value>,
        enumerable: bool,
        configurable: bool,
    ) -> Self {
        // A completed accessor descriptor fills missing getter/setter fields
        // with *undefined*; storing those as absent keeps the stored form
        // canonical.
        let get = get.filter(|v| !matches!(v, Value::Undefined));
        let set = set.filter(|v| !matches!(v, Value::Undefined));
        Self {
            kind: PropertyKind::Accessor { get, set },
            enumerable,
            configurable,
        }
    }

    /// spec 6.2.5.7: present if [[Value]] or [[Writable]] is present.
    pub fn is_data(&self) -> bool {
        matches!(self.kind, PropertyKind::Data { .. })
    }

    /// spec 6.2.5.6: present if [[Get]] or [[Set]] is present.
    pub fn is_accessor(&self) -> bool {
        matches!(self.kind, PropertyKind::Accessor { .. })
    }

    /// The data property's [[Value]], or `None` for accessors.
    pub fn value(&self) -> Option<Value> {
        match &self.kind {
            PropertyKind::Data { value, .. } => Some(value.clone()),
            PropertyKind::Accessor { .. } => None,
        }
    }

    /// The data property's [[Writable]], or `None` for accessors.
    pub fn writable(&self) -> Option<bool> {
        match &self.kind {
            PropertyKind::Data { writable, .. } => Some(*writable),
            PropertyKind::Accessor { .. } => None,
        }
    }

    pub fn getter(&self) -> Option<Value> {
        match &self.kind {
            PropertyKind::Accessor { get, .. } => get.clone(),
            PropertyKind::Data { .. } => None,
        }
    }

    pub fn setter(&self) -> Option<Value> {
        match &self.kind {
            PropertyKind::Accessor { set, .. } => set.clone(),
            PropertyKind::Data { .. } => None,
        }
    }

    /// The stored property as a fully populated Property Descriptor.
    pub fn to_descriptor(&self) -> PropertyDescriptor {
        match &self.kind {
            PropertyKind::Data { value, writable } => PropertyDescriptor {
                value: Some(value.clone()),
                writable: Some(*writable),
                get: None,
                set: None,
                enumerable: Some(self.enumerable),
                configurable: Some(self.configurable),
            },
            PropertyKind::Accessor { get, set } => PropertyDescriptor {
                value: None,
                writable: None,
                get: Some(get.clone().unwrap_or(Value::Undefined)),
                set: Some(set.clone().unwrap_or(Value::Undefined)),
                enumerable: Some(self.enumerable),
                configurable: Some(self.configurable),
            },
        }
    }

    /// Build the stored form of a complete descriptor (`desc.complete()`
    /// must have been called first).
    pub fn from_descriptor(desc: &PropertyDescriptor) -> Option<Property> {
        let enumerable = desc.enumerable?;
        let configurable = desc.configurable?;
        if desc.is_data_descriptor() {
            Some(Property {
                kind: PropertyKind::Data {
                    value: desc.value.clone()?,
                    writable: desc.writable?,
                },
                enumerable,
                configurable,
            })
        } else if desc.is_accessor_descriptor() {
            let get = desc.get.clone().filter(|v| !matches!(v, Value::Undefined));
            let set = desc.set.clone().filter(|v| !matches!(v, Value::Undefined));
            Some(Property {
                kind: PropertyKind::Accessor { get, set },
                enumerable,
                configurable,
            })
        } else {
            None
        }
    }
}

/// The exotic-object kind, selecting internal-method behaviour (spec ch. 10).
#[derive(Debug, Clone)]
pub enum ObjectKind {
    Ordinary,
    /// Array exotic (spec 10.4.2); `length` lives in `properties`.
    Array,
    /// String exotic (spec 10.4.3): virtual code-unit index properties.
    String(Handle<JsString>),
    /// Arguments exotic (spec 10.4.4): mapped parameter bindings.
    Arguments(Handle<ArgumentsSlots>),
    /// Proxy exotic (spec 10.5): every internal method is a handler trap.
    Proxy(Handle<crate::proxy::ProxySlots>),
    /// TypedArray (Integer-Indexed) exotic shell (spec 10.4.5).
    IntegerIndexed(Handle<TypedArraySlots>),
    /// Module namespace exotic (spec 10.4.6).
    ModuleNamespace(Handle<ModuleNamespaceSlots>),
}

/// The [[ParameterMap]] of an arguments exotic object (spec 10.4.4): an
/// ordinary object holding accessor properties that read/write the mapped
/// formal parameter bindings.
#[derive(Debug, Clone)]
pub struct ArgumentsSlots {
    pub parameter_map: Option<Handle<JsObject>>,
}

/// The TypedArray (Integer-Indexed) exotic slots (spec 10.4.5.1): the viewed
/// buffer, the element type, and the byte geometry. The buffer is shared
/// with the [[ArrayBufferData]] storage.
#[derive(Debug, Clone)]
pub struct TypedArraySlots {
    /// [[ViewedArrayBuffer]] as a language value (the `buffer` accessor).
    pub buffer_object: Value,
    /// The shared byte storage ([[ArrayBufferData]] of the viewed buffer).
    pub buffer: crate::typed_array::SharedBuffer,
    /// The element type (spec 25.2.1: [[TypedArrayName]] table).
    pub element_type: crate::typed_array::ElementType,
    /// [[ByteLength]]: the number of bytes this view covers.
    pub byte_length: usize,
    /// [[ByteOffset]]: the offset of the first element in the buffer.
    pub byte_offset: usize,
    /// [[ArrayLength]]: the number of elements. For a view over a resizable
    /// buffer created without an explicit length this is "auto": the value
    /// tracks the buffer via [`typed_array_effective_length`].
    pub array_length: usize,
    /// Whether [[ArrayLength]] is auto (spec 25.2.2.1 step 12: a view over a
    /// resizable buffer without an explicit length).
    pub auto_length: bool,
}

/// The [[Exports]] of a module namespace exotic object (spec 10.4.6). Phase
/// 7 populates it from the module's export list; until then a namespace
/// exposes no properties and rejects every mutation.
#[derive(Debug, Clone, Default)]
pub struct ModuleNamespaceSlots {
    pub exports: Vec<PropertyKey>,
}

impl ObjectKind {
    pub fn name(&self) -> &'static str {
        match self {
            ObjectKind::Ordinary => "Object",
            ObjectKind::Array => "Array",
            ObjectKind::String(_) => "String",
            ObjectKind::Arguments(_) => "Arguments",
            ObjectKind::Proxy(_) => "Proxy",
            ObjectKind::IntegerIndexed(_) => "TypedArray",
            ObjectKind::ModuleNamespace(_) => "Module",
        }
    }
}

/// A PrivateElement Record (spec 10.1.4): a private field or method added
/// to an object's [[PrivateElements]] when its class constructor runs.
#[derive(Debug, Clone)]
pub struct PrivateElement {
    /// The Private Name's unique id.
    pub name_id: u64,
    pub kind: PrivateElementKind,
}

#[derive(Debug, Clone)]
pub enum PrivateElementKind {
    Field(Value),
    Method(Value),
    Accessor {
        get: Option<Value>,
        set: Option<Value>,
    },
}

/// An ECMAScript object. Equality is identity: each object carries a unique
/// `id` (mirroring `Symbol`), so `Handle<JsObject>` equality and the derived
/// `PartialEq` on `Value` are identity tests.
#[derive(Clone)]
pub struct JsObject {
    id: u64,
    pub kind: ObjectKind,
    /// [[Prototype]]; `None` when the prototype is *null*.
    pub prototype: RefCell<Option<Handle<JsObject>>>,
    /// [[Extensible]].
    pub extensible: Cell<bool>,
    /// Whether this object is an immutable prototype exotic object (spec
    /// 9.4.7): `[[SetPrototypeOf]]` accepts only a SameValue prototype.
    immutable_prototype: Cell<bool>,
    /// Own properties in insertion order (the [[OwnPropertyKeys]] string
    /// order for ordinary objects).
    pub properties: RefCell<Vec<(PropertyKey, Property)>>,
    /// [[PrivateElements]] (spec 10.1.4): private fields and methods added
    /// by InitializeInstanceElements.
    pub private_elements: RefCell<Vec<PrivateElement>>,
    /// A weak back-reference to the owning handle, so internal methods that
    /// need `this` as a language value (accessor invocation, the arguments
    /// mapping) can recover the real handle instead of a copy.
    self_handle: RefCell<Option<Weak<JsObject>>>,
    /// When this object is a function's object part, a weak back-reference to
    /// that function so a prototype link recovers the function value (e.g.
    /// `Object.getPrototypeOf(Int8Array)` is %TypedArray% the function, not
    /// its object part).
    pub function_self: RefCell<Option<Weak<crate::function::Function>>>,
    /// The wrapped value of a primitive wrapper object (spec 10.4.2
    /// [[NumberData]]/[[BooleanData]]/[[BigIntData]]), mirrored on the object
    /// so crux's ToPrimitive/ToNumber coerce a boxed primitive without
    /// invoking the agent-dispatched `valueOf`.
    pub boxed: RefCell<Option<BoxedPrimitive>>,
}

/// The wrapped value of a primitive wrapper object (spec 10.4.2).
#[derive(Debug, Clone)]
pub enum BoxedPrimitive {
    Number(f64),
    BigInt(crate::BigInt),
    Boolean(bool),
}

impl PartialEq for JsObject {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for JsObject {}

impl fmt::Debug for JsObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.kind.name(), self.id)
    }
}

impl JsObject {
    /// When this object is a function's object part, the function as a
    /// language value (so a prototype link like `Object.getPrototypeOf(f)`
    /// keeps `typeof`/`is_constructor` working).
    pub fn function_value(&self) -> Option<Value> {
        self.function_self
            .borrow()
            .as_ref()
            .and_then(|weak| weak.upgrade())
            .map(Value::Function)
    }

    /// The raw object with ordinary behaviour (no handle back-reference).
    /// Used to embed an object inside `Function`; constructors that hand out
    /// a `Handle` call `link_self_handle` on the way out.
    pub fn basic_object_create(prototype: Option<Handle<JsObject>>) -> Self {
        Self {
            id: NEXT_OBJECT_ID.fetch_add(1, Ordering::Relaxed),
            kind: ObjectKind::Ordinary,
            prototype: RefCell::new(prototype),
            extensible: Cell::new(true),
            immutable_prototype: Cell::new(false),
            properties: RefCell::new(Vec::new()),
            private_elements: RefCell::new(Vec::new()),
            self_handle: RefCell::new(None),
            function_self: RefCell::new(None),
            boxed: RefCell::new(None),
        }
    }

    fn link_self_handle(object: &Handle<JsObject>) {
        *object.self_handle.borrow_mut() = Some(Handle::downgrade(object));
    }

    /// The object as a language value, recovering the original handle.
    pub fn self_value(&self) -> Value {
        self.self_handle
            .borrow()
            .as_ref()
            .and_then(|weak| weak.upgrade())
            .map(Value::Object)
            .unwrap_or(Value::Undefined)
    }

    /// Recover the owning handle of an embedded object (a `Function`'s object
    /// part); `None` for raw copies without a back-reference.
    pub fn handle(&self) -> Option<Handle<JsObject>> {
        self.self_handle
            .borrow()
            .as_ref()
            .and_then(|weak| weak.upgrade())
    }

    /// The object's unique identity ([[ObjectId]]), used by the runtime to
    /// key per-object state (promises, generators) in agent-side tables.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// OrdinaryObjectCreate (spec 10.1.13).
    pub fn ordinary_object_create(prototype: Option<Handle<JsObject>>) -> Handle<JsObject> {
        let object = Handle::new(Self::basic_object_create(prototype));
        Self::link_self_handle(&object);
        object
    }

    /// ArrayCreate (spec 10.4.2.2): a fresh Array with `length` set via
    /// OrdinaryDefineOwnProperty (writable, non-enumerable, non-configurable).
    pub fn array_create(
        prototype: Option<Handle<JsObject>>,
        length: f64,
    ) -> Result<Handle<JsObject>, JsError> {
        if length > 4294967295.0 || length.is_nan() || length < 0.0 || length.trunc() != length {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "Invalid array length".into(),
            ));
        }
        let array = Handle::new(Self {
            id: NEXT_OBJECT_ID.fetch_add(1, Ordering::Relaxed),
            kind: ObjectKind::Array,
            prototype: RefCell::new(prototype),
            extensible: Cell::new(true),
            immutable_prototype: Cell::new(false),
            properties: RefCell::new(Vec::new()),
            private_elements: RefCell::new(Vec::new()),
            self_handle: RefCell::new(None),
            function_self: RefCell::new(None),
            boxed: RefCell::new(None),
        });
        Self::link_self_handle(&array);
        let length_desc = PropertyDescriptor {
            value: Some(Value::Number(length)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        };
        array.ordinary_define_own_property(&PropertyKey::from_utf8("length"), &length_desc)?;
        Ok(array)
    }

    /// StringCreate (spec 10.4.3.2): a String exotic with the virtual
    /// code-unit index properties and the `length` data property.
    pub fn string_create(
        value: JsString,
        prototype: Option<Handle<JsObject>>,
    ) -> Result<Handle<JsObject>, JsError> {
        let length = value.len() as f64;
        let string = Handle::new(Self {
            id: NEXT_OBJECT_ID.fetch_add(1, Ordering::Relaxed),
            kind: ObjectKind::String(Handle::new(value)),
            prototype: RefCell::new(prototype),
            extensible: Cell::new(true),
            immutable_prototype: Cell::new(false),
            properties: RefCell::new(Vec::new()),
            private_elements: RefCell::new(Vec::new()),
            self_handle: RefCell::new(None),
            function_self: RefCell::new(None),
            boxed: RefCell::new(None),
        });
        Self::link_self_handle(&string);
        let length_desc = PropertyDescriptor {
            value: Some(Value::Number(length)),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        };
        string.ordinary_define_own_property(&PropertyKey::from_utf8("length"), &length_desc)?;
        Ok(string)
    }

    /// The object half of ProxyCreate (spec 10.5.14): the proxy's own slots
    /// are set up by `crate::proxy::proxy_create`.
    pub fn proxy_object_create(slots: crate::proxy::ProxySlots) -> Handle<JsObject> {
        let proxy = Handle::new(Self {
            id: NEXT_OBJECT_ID.fetch_add(1, Ordering::Relaxed),
            kind: ObjectKind::Proxy(Handle::new(slots)),
            prototype: RefCell::new(None),
            extensible: Cell::new(true),
            immutable_prototype: Cell::new(false),
            properties: RefCell::new(Vec::new()),
            private_elements: RefCell::new(Vec::new()),
            self_handle: RefCell::new(None),
            function_self: RefCell::new(None),
            boxed: RefCell::new(None),
        });
        Self::link_self_handle(&proxy);
        proxy
    }

    /// TypedArrayCreate (spec 25.2.4.1): an Integer-Indexed exotic with the
    /// full slot set (buffer, element type, byte geometry).
    pub fn integer_indexed_object_create(
        slots: TypedArraySlots,
        prototype: Option<Handle<JsObject>>,
    ) -> Result<Handle<JsObject>, JsError> {
        let object = Handle::new(Self {
            id: NEXT_OBJECT_ID.fetch_add(1, Ordering::Relaxed),
            kind: ObjectKind::IntegerIndexed(Handle::new(slots)),
            prototype: RefCell::new(prototype),
            extensible: Cell::new(true),
            immutable_prototype: Cell::new(false),
            properties: RefCell::new(Vec::new()),
            private_elements: RefCell::new(Vec::new()),
            self_handle: RefCell::new(None),
            function_self: RefCell::new(None),
            boxed: RefCell::new(None),
        });
        Self::link_self_handle(&object);
        Ok(object)
    }

    /// ModuleNamespaceObjectCreate (spec 10.4.6.2): a non-extensible object
    /// with *null* prototype exposing only the (Phase 7) exports.
    pub fn module_namespace_object_create(
        exports: Vec<PropertyKey>,
    ) -> Result<Handle<JsObject>, JsError> {
        let object = Handle::new(Self {
            id: NEXT_OBJECT_ID.fetch_add(1, Ordering::Relaxed),
            kind: ObjectKind::ModuleNamespace(Handle::new(ModuleNamespaceSlots { exports })),
            prototype: RefCell::new(None),
            extensible: Cell::new(false),
            immutable_prototype: Cell::new(false),
            properties: RefCell::new(Vec::new()),
            private_elements: RefCell::new(Vec::new()),
            self_handle: RefCell::new(None),
            function_self: RefCell::new(None),
            boxed: RefCell::new(None),
        });
        Self::link_self_handle(&object);
        Ok(object)
    }

    /// CreateMappedArgumentsObject (spec 10.4.4.2). `make_getter`/`make_setter`
    /// build the native functions that read/write a formal parameter binding
    /// (MakeArgGetter/MakeArgSetter); the runtime supplies closures over the
    /// parameter environment at call time (Phase 7).
    pub fn mapped_arguments_object_create(
        prototype: Option<Handle<JsObject>>,
        func: Value,
        formals: &[JsString],
        args: &[Value],
        make_getter: impl Fn(&JsString) -> Value,
        make_setter: impl Fn(&JsString) -> Value,
    ) -> Result<Handle<JsObject>, JsError> {
        let map = JsObject::ordinary_object_create(None);
        let obj = Handle::new(Self {
            id: NEXT_OBJECT_ID.fetch_add(1, Ordering::Relaxed),
            kind: ObjectKind::Arguments(Handle::new(ArgumentsSlots {
                parameter_map: Some(map.clone()),
            })),
            prototype: RefCell::new(prototype),
            extensible: Cell::new(true),
            immutable_prototype: Cell::new(false),
            properties: RefCell::new(Vec::new()),
            private_elements: RefCell::new(Vec::new()),
            self_handle: RefCell::new(None),
            function_self: RefCell::new(None),
            boxed: RefCell::new(None),
        });
        Self::link_self_handle(&obj);
        // spec steps 15-16: index properties first, then length. The map is
        // still empty, so these defines do not touch the mappings.
        for (index, value) in args.iter().enumerate() {
            obj.create_data_property(&JsString::from_utf8(&index.to_string()), value.clone())?;
        }
        obj.define_property(
            &JsString::from_utf8("length"),
            &PropertyDescriptor {
                value: Some(Value::Number(args.len() as f64)),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
        // spec steps 18-24: the parameter map; the last parameter of a
        // duplicated name wins the mapping, and only passed arguments map.
        let mut mapped_names: Vec<JsString> = Vec::new();
        let mut index = formals.len() as isize - 1;
        while index >= 0 {
            let name = &formals[index as usize];
            if !mapped_names.contains(name) {
                mapped_names.push(name.clone());
                if (index as usize) < args.len() {
                    let getter = make_getter(name);
                    let setter = make_setter(name);
                    map.define_property(
                        &JsString::from_utf8(&index.to_string()),
                        &PropertyDescriptor {
                            value: None,
                            writable: None,
                            get: Some(getter),
                            set: Some(setter),
                            enumerable: Some(false),
                            configurable: Some(true),
                        },
                    )?;
                }
            }
            index -= 1;
        }
        // spec steps 25-26: @@iterator and callee. %Array.prototype.values%
        // joins with Phase 8.
        obj.define_property_key(
            &PropertyKey::Symbol(well_known("iterator").as_ref().clone()),
            &PropertyDescriptor {
                value: Some(Value::Undefined),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
        obj.define_property(
            &JsString::from_utf8("callee"),
            &PropertyDescriptor {
                value: Some(func),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
        Ok(obj)
    }

    /// CreateUnmappedArgumentsObject (spec 10.4.4.9): an ordinary object
    /// with index properties, `length`, @@iterator, and a throwing `callee`
    /// accessor.
    pub fn unmapped_arguments_object_create(
        prototype: Option<Handle<JsObject>>,
        args: &[Value],
        thrower: Value,
    ) -> Result<Handle<JsObject>, JsError> {
        let obj = Handle::new(Self {
            id: NEXT_OBJECT_ID.fetch_add(1, Ordering::Relaxed),
            kind: ObjectKind::Arguments(Handle::new(ArgumentsSlots {
                parameter_map: None,
            })),
            prototype: RefCell::new(prototype),
            extensible: Cell::new(true),
            immutable_prototype: Cell::new(false),
            properties: RefCell::new(Vec::new()),
            private_elements: RefCell::new(Vec::new()),
            self_handle: RefCell::new(None),
            function_self: RefCell::new(None),
            boxed: RefCell::new(None),
        });
        Self::link_self_handle(&obj);
        obj.define_property(
            &JsString::from_utf8("length"),
            &PropertyDescriptor {
                value: Some(Value::Number(args.len() as f64)),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
        for (index, value) in args.iter().enumerate() {
            obj.create_data_property(&JsString::from_utf8(&index.to_string()), value.clone())?;
        }
        obj.define_property_key(
            &PropertyKey::Symbol(well_known("iterator").as_ref().clone()),
            &PropertyDescriptor {
                value: Some(Value::Undefined),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
        obj.define_property(
            &JsString::from_utf8("callee"),
            &PropertyDescriptor {
                value: None,
                writable: None,
                get: Some(thrower.clone()),
                set: Some(thrower),
                enumerable: Some(false),
                configurable: Some(false),
            },
        )?;
        Ok(obj)
    }

    /// spec 7.3.10 IsExtensible.
    pub fn is_extensible(&self) -> Result<bool, JsError> {
        match &self.kind {
            ObjectKind::Proxy(slots) => crate::proxy::is_extensible(slots),
            ObjectKind::ModuleNamespace(_) => Ok(false),
            _ => Ok(self.extensible.get()),
        }
    }

    /// OrdinaryPreventExtensions (spec 10.1.5.2) with the proxy trap.
    pub fn prevent_extensions(&self) -> Result<bool, JsError> {
        match &self.kind {
            ObjectKind::Proxy(slots) => return crate::proxy::prevent_extensions(slots),
            // TypedArrays are fixed-length here (the resizable-buffer check
            // joins with Phase 12): OrdinaryPreventExtensions applies.
            ObjectKind::ModuleNamespace(_) => return Ok(true),
            _ => {}
        }
        self.extensible.set(false);
        Ok(true)
    }

    /// OrdinaryGetPrototypeOf (spec 10.1.1.1) with the exotic overrides.
    pub fn get_prototype_of(&self) -> Result<Option<Handle<JsObject>>, JsError> {
        match &self.kind {
            ObjectKind::Proxy(slots) => crate::proxy::get_prototype_of(slots),
            ObjectKind::ModuleNamespace(_) => Ok(None),
            _ => Ok(self.prototype.borrow().clone()),
        }
    }

    /// OrdinarySetPrototypeOf (spec 10.1.2.2): no cycles, and no prototype
    /// change once non-extensible.
    pub fn set_prototype_of(&self, proto: Option<Handle<JsObject>>) -> Result<bool, JsError> {
        match &self.kind {
            ObjectKind::Proxy(slots) => return crate::proxy::set_prototype_of(slots, proto),
            ObjectKind::ModuleNamespace(_) => return Ok(false),
            _ => {}
        }
        let current = self.get_prototype_of()?;
        let same = match (&current, &proto) {
            (Some(a), Some(b)) => Handle::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        // spec 9.4.7: an immutable prototype exotic object (e.g.
        // %Object.prototype%) never changes its prototype; only a SameValue
        // assignment succeeds (SetImmutablePrototype, spec 9.4.7.1).
        if self.immutable_prototype.get() {
            return Ok(same);
        }
        if same {
            return Ok(true);
        }
        if !self.extensible.get() {
            return Ok(false);
        }
        if let Some(proto) = &proto {
            let mut ancestor = Some(proto.clone());
            while let Some(obj) = ancestor {
                if obj.id == self.id {
                    return Ok(false);
                }
                // spec 9.1.2.2 step 8c: the cycle scan stops at any object
                // whose [[GetPrototypeOf]] is not the ordinary internal
                // method (proxies, module namespace objects), since a proxy's
                // prototype can change at any time.
                match &obj.kind {
                    ObjectKind::Proxy(_) | ObjectKind::ModuleNamespace(_) => break,
                    _ => ancestor = obj.get_prototype_of()?,
                }
            }
        }
        *self.prototype.borrow_mut() = proto;
        Ok(true)
    }

    /// Mark this object as an immutable prototype exotic object (spec
    /// 9.4.7): its `[[SetPrototypeOf]]` accepts only a SameValue prototype.
    pub fn mark_immutable_prototype(&self) {
        self.immutable_prototype.set(true);
    }

    /// OrdinaryGetOwnProperty (spec 10.1.7.1) with exotic fallback: `None`
    /// when the object does not have an own property `key`.
    pub fn get_own_property(&self, key: &JsString) -> Result<Option<Property>, JsError> {
        self.get_own_property_key(&PropertyKey::from_js_string(key))
    }

    pub fn get_own_property_key(&self, key: &PropertyKey) -> Result<Option<Property>, JsError> {
        match &self.kind {
            ObjectKind::Proxy(slots) => crate::proxy::get_own_property(slots, key),
            ObjectKind::IntegerIndexed(slots) => typed_array_get_own_property(self, slots, key),
            ObjectKind::ModuleNamespace(slots) => {
                if slots.exports.contains(key) {
                    // spec 10.4.6.8: exported bindings appear as own data
                    // properties (writable, enumerable, non-configurable);
                    // the runtime reads the live binding value.
                    return Ok(Some(Property::data(Value::Undefined, true, true, false)));
                }
                self.ordinary_get_own_property(key)
            }
            ObjectKind::String(string) => match self.ordinary_get_own_property(key)? {
                Some(property) => Ok(Some(property)),
                None => Ok(string_get_own_property(string, key)),
            },
            ObjectKind::Arguments(slots) => {
                let Some(property) = self.ordinary_get_own_property(key)? else {
                    return Ok(None);
                };
                // spec 10.4.4.1 steps 3-6: mapped index properties reflect the
                // current parameter binding through the map's getter.
                if let Some(map) = slots.parameter_map.as_ref()
                    && map.has_own_property_key(key)?
                {
                    let value = map.get_key(key)?;
                    return match property.kind {
                        PropertyKind::Data { writable, .. } => Ok(Some(Property::data(
                            value,
                            writable,
                            property.enumerable,
                            property.configurable,
                        ))),
                        PropertyKind::Accessor { .. } => Ok(Some(property)),
                    };
                }
                Ok(Some(property))
            }
            _ => self.ordinary_get_own_property(key),
        }
    }

    fn ordinary_get_own_property(&self, key: &PropertyKey) -> Result<Option<Property>, JsError> {
        Ok(self
            .properties
            .borrow()
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, p)| p.clone()))
    }

    /// spec 7.3.12 HasOwnProperty.
    pub fn has_own_property(&self, key: &JsString) -> Result<bool, JsError> {
        self.has_own_property_key(&PropertyKey::from_js_string(key))
    }

    pub fn has_own_property_key(&self, key: &PropertyKey) -> Result<bool, JsError> {
        // TypedArray and module namespace elements are virtual: HasOwnProperty
        // checks the index/export directly instead of going through
        // [[GetOwnProperty]] (whose element access is a Phase 12 concern).
        match &self.kind {
            ObjectKind::IntegerIndexed(slots) => {
                if let Some(index) = canonical_index(key) {
                    return Ok(typed_array_valid_index(slots, index));
                }
            }
            ObjectKind::ModuleNamespace(slots) => return Ok(slots.exports.contains(key)),
            _ => {}
        }
        Ok(self.get_own_property_key(key)?.is_some())
    }

    /// spec 7.3.13 HasProperty: walks the prototype chain.
    pub fn has_property(&self, key: &JsString) -> Result<bool, JsError> {
        self.has_property_key(&PropertyKey::from_js_string(key))
    }

    pub fn has_property_key(&self, key: &PropertyKey) -> Result<bool, JsError> {
        // A proxy's [[HasProperty]] is the `has` trap, not an own-property
        // walk.
        if let ObjectKind::Proxy(slots) = &self.kind {
            return crate::proxy::has_property(slots, key);
        }
        // The Integer-Indexed [[HasProperty]] intercepts canonical numeric
        // index strings: an invalid index reads *undefined*, so the property
        // is absent without consulting the prototype chain (spec 10.4.7.4).
        if let ObjectKind::IntegerIndexed(slots) = &self.kind
            && let Some(index) = canonical_index(key)
        {
            return Ok(typed_array_valid_index(slots, index));
        }
        if self.has_own_property_key(key)? {
            return Ok(true);
        }
        match self.get_prototype_of()? {
            Some(proto) => proto.has_property_key(key),
            None => Ok(false),
        }
    }

    /// OrdinaryGet (spec 10.1.8.3) with the object itself as receiver:
    /// data properties return their value; accessor properties invoke the
    /// getter with the real object handle.
    pub fn get(&self, key: &JsString) -> Result<Value, JsError> {
        self.get_key(&PropertyKey::from_js_string(key))
    }

    pub fn get_key(&self, key: &PropertyKey) -> Result<Value, JsError> {
        self.get_with_receiver_key(key, self.self_value())
    }

    /// [[Get]] (P, Receiver) with the arguments-exotic mapping (spec 10.4.4.4).
    pub fn get_with_receiver_key(
        &self,
        key: &PropertyKey,
        receiver: Value,
    ) -> Result<Value, JsError> {
        match &self.kind {
            ObjectKind::Proxy(slots) => return crate::proxy::get(slots, key, receiver),
            ObjectKind::IntegerIndexed(slots) => {
                return typed_array_get(self, slots, key, receiver);
            }
            ObjectKind::ModuleNamespace(slots) => {
                if slots.exports.contains(key) {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Module namespace exports are not implemented until Phase 7".into(),
                    ));
                }
                return Ok(Value::Undefined);
            }
            ObjectKind::Arguments(slots) => {
                if let Some(map) = slots.parameter_map.as_ref()
                    && map.has_own_property_key(key)?
                {
                    return map.get_key(key);
                }
            }
            _ => {}
        }
        ordinary_get(self, key, receiver)
    }

    /// OrdinarySet (spec 10.1.9.3) with the object itself as receiver.
    /// Non-writable properties and absent setters fail: silently when `throw`
    /// is false, with a TypeError when it is true.
    pub fn set(&self, key: &JsString, value: Value, throw: bool) -> Result<bool, JsError> {
        self.set_key(&PropertyKey::from_js_string(key), value, throw)
    }

    pub fn set_key(&self, key: &PropertyKey, value: Value, throw: bool) -> Result<bool, JsError> {
        self.set_with_receiver_key(key, value, self.self_value(), throw)
    }

    /// [[Set]] (P, V, Receiver): the arguments-exotic mapping (spec 10.4.4.6)
    /// followed by OrdinarySet.
    pub fn set_with_receiver_key(
        &self,
        key: &PropertyKey,
        value: Value,
        receiver: Value,
        throw: bool,
    ) -> Result<bool, JsError> {
        match &self.kind {
            ObjectKind::Proxy(slots) => {
                // spec 7.3.5 Set step 4: a false [[Set]] result with Throw
                // true becomes a TypeError. The proxy internal method itself
                // only reports success.
                let success = crate::proxy::set(slots, key, value, receiver)?;
                return if success {
                    Ok(true)
                } else if throw {
                    Err(JsError::new(
                        ErrorKind::TypeError,
                        format!("Cannot set property {:?}", key.display_string()),
                    ))
                } else {
                    Ok(false)
                };
            }
            ObjectKind::IntegerIndexed(slots) => {
                return typed_array_set(self, slots, key, value, receiver);
            }
            ObjectKind::ModuleNamespace(_) => return Ok(false),
            ObjectKind::Arguments(slots) => {
                if same_value(&receiver, &self.self_value())
                    && let Some(map) = slots.parameter_map.as_ref()
                    && map.has_own_property_key(key)?
                {
                    map.set_key(key, value.clone(), false)?;
                }
            }
            _ => {}
        }
        ordinary_set(self, key, value, receiver, throw)
    }

    /// OrdinaryDefineOwnProperty (spec 10.1.6.3) with exotic overrides:
    /// ArraySetLength/array-index sync for Arrays, the compatibility check
    /// for String index properties, and the mapped-arguments bookkeeping.
    pub fn define_property(
        &self,
        key: &JsString,
        desc: &PropertyDescriptor,
    ) -> Result<bool, JsError> {
        self.define_property_key(&PropertyKey::from_js_string(key), desc)
    }

    pub fn define_property_key(
        &self,
        key: &PropertyKey,
        desc: &PropertyDescriptor,
    ) -> Result<bool, JsError> {
        match &self.kind {
            ObjectKind::Proxy(slots) => return crate::proxy::define_own_property(slots, key, desc),
            ObjectKind::IntegerIndexed(slots) => {
                return typed_array_define_own_property(self, slots, key, desc);
            }
            // A module namespace rejects every define (its properties are
            // non-configurable and it is not extensible).
            ObjectKind::ModuleNamespace(_) => return Ok(false),
            ObjectKind::Array => return array_define_own_property(self, key, desc),
            ObjectKind::String(string) => {
                if let Some(current) = string_get_own_property(string, key) {
                    return is_compatible_property_descriptor(
                        self.extensible.get(),
                        desc,
                        Some(&current),
                    );
                }
                return self.ordinary_define_own_property(key, desc);
            }
            ObjectKind::Arguments(slots) => {
                return arguments_define_own_property(self, slots, key, desc);
            }
            _ => {}
        }
        self.ordinary_define_own_property(key, desc)
    }

    /// ValidateAndApplyPropertyDescriptor with the object's [[Extensible]]
    /// (spec 10.1.6.4): the full decision table including data↔accessor
    /// conversions.
    fn ordinary_define_own_property(
        &self,
        key: &PropertyKey,
        desc: &PropertyDescriptor,
    ) -> Result<bool, JsError> {
        let current = self.ordinary_get_own_property(key)?;
        validate_and_apply(Some(self), key, self.extensible.get(), desc, current)
    }

    /// spec 7.3.4 CreateDataProperty.
    pub fn create_data_property(&self, key: &JsString, value: Value) -> Result<bool, JsError> {
        self.create_data_property_key(&PropertyKey::from_js_string(key), value)
    }

    pub fn create_data_property_key(
        &self,
        key: &PropertyKey,
        value: Value,
    ) -> Result<bool, JsError> {
        self.define_property_key(key, &PropertyDescriptor::data(value))
    }

    /// spec 7.3.5 CreateDataPropertyOrThrow.
    pub fn create_data_property_or_throw(
        &self,
        key: &JsString,
        value: Value,
    ) -> Result<(), JsError> {
        if !self.create_data_property(key, value)? {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Cannot define property".into(),
            ));
        }
        Ok(())
    }

    /// spec 7.3.5 CreateDataPropertyOrThrow for any property key.
    pub fn create_data_property_or_throw_key(
        &self,
        key: &PropertyKey,
        value: Value,
    ) -> Result<(), JsError> {
        if !self.create_data_property_key(key, value)? {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Cannot define property".into(),
            ));
        }
        Ok(())
    }

    /// PrivateFieldAdd/PrivateMethodOrAccessorAdd storage (spec 10.2.10,
    /// 10.2.13): append a private element, rejecting a duplicate name.
    pub fn private_element_add(&self, element: PrivateElement) -> Result<(), JsError> {
        let mut elements = self.private_elements.borrow_mut();
        if elements
            .iter()
            .any(|existing| existing.name_id == element.name_id)
        {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Cannot add private member to an object that already has it".into(),
            ));
        }
        elements.push(element);
        Ok(())
    }

    /// The private element for `name_id`, if the object has been initialized
    /// by the declaring class.
    pub fn private_element(&self, name_id: u64) -> Option<PrivateElement> {
        self.private_elements
            .borrow()
            .iter()
            .find(|element| element.name_id == name_id)
            .cloned()
    }

    /// Whether `name_id` is in the object's [[PrivateElements]] — the `#x in
    /// obj` brand check (spec 13.11.1).
    pub fn has_private_element(&self, name_id: u64) -> bool {
        self.private_elements
            .borrow()
            .iter()
            .any(|element| element.name_id == name_id)
    }

    /// spec 7.3.6 DefinePropertyOrThrow.
    pub fn define_property_or_throw(
        &self,
        key: &JsString,
        desc: &PropertyDescriptor,
    ) -> Result<(), JsError> {
        if !self.define_property(key, desc)? {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Cannot redefine property".into(),
            ));
        }
        Ok(())
    }

    /// OrdinaryDelete (spec 10.1.10.2) with the arguments-exotic mapping
    /// cleanup (spec 10.4.4.7).
    pub fn delete(&self, key: &JsString) -> Result<bool, JsError> {
        self.delete_key(&PropertyKey::from_js_string(key))
    }

    pub fn delete_key(&self, key: &PropertyKey) -> Result<bool, JsError> {
        match &self.kind {
            ObjectKind::Proxy(slots) => return crate::proxy::delete(slots, key),
            ObjectKind::IntegerIndexed(slots) => return typed_array_delete(self, slots, key),
            // A module namespace never deletes (its properties are
            // non-configurable).
            ObjectKind::ModuleNamespace(_) => return Ok(false),
            ObjectKind::String(string) => {
                // spec 10.4.3.7: the virtual code-unit index properties are
                // non-configurable; deleting an in-range index fails, out of
                // range falls through to the ordinary delete.
                if let PropertyKey::String(id) = key {
                    let text = lookup(*id);
                    if let Some(index) = canonical_numeric_index_string(text.as_slice())
                        && index >= 0.0
                        && index.trunc() == index
                        && (index as usize) < string.len()
                    {
                        return Ok(false);
                    }
                }
            }
            ObjectKind::Arguments(slots) => {
                let mapped = slots
                    .parameter_map
                    .as_ref()
                    .map(|map| map.has_own_property_key(key))
                    .transpose()?
                    .unwrap_or(false);
                let result = {
                    let mut props = self.properties.borrow_mut();
                    if let Some(index) = props.iter().position(|(name, _)| name == key) {
                        if props[index].1.configurable {
                            props.remove(index);
                            true
                        } else {
                            false
                        }
                    } else {
                        true
                    }
                };
                if result
                    && mapped
                    && let Some(map) = slots.parameter_map.as_ref()
                {
                    map.delete_key(key)?;
                }
                return Ok(result);
            }
            _ => {}
        }
        let result = {
            let mut props = self.properties.borrow_mut();
            if let Some(index) = props.iter().position(|(name, _)| name == key) {
                if props[index].1.configurable {
                    props.remove(index);
                    true
                } else {
                    false
                }
            } else {
                true
            }
        };
        Ok(result)
    }

    /// [[OwnPropertyKeys]] (spec 10.1.12.1): array indices ascending, then
    /// strings in insertion order, then symbols.
    pub fn own_property_keys(&self) -> Result<Vec<PropertyKey>, JsError> {
        match &self.kind {
            ObjectKind::Proxy(slots) => crate::proxy::own_property_keys(slots),
            ObjectKind::IntegerIndexed(slots) => Ok(typed_array_own_property_keys(slots, self)),
            ObjectKind::ModuleNamespace(slots) => Ok(slots.exports.clone()),
            ObjectKind::Array => Ok(array_own_property_keys(self)),
            ObjectKind::String(string) => Ok(string_own_property_keys(string, self)),
            _ => Ok(ordinary_own_property_keys(self)),
        }
    }

    /// spec 7.3.11 GetMethod: `None` when the property is *undefined* or
    /// absent, a TypeError when it is present but not callable.
    pub fn get_method(&self, key: &JsString) -> Result<Option<Value>, JsError> {
        let value = self.get(key)?;
        match value {
            Value::Undefined | Value::Null => Ok(None),
            v if is_callable(&v) => Ok(Some(v)),
            _ => Err(JsError::new(
                ErrorKind::TypeError,
                format!("{:?} is not a function", key.to_string_lossy()),
            )),
        }
    }
}

/// OrdinaryOwnPropertyKeys (spec 10.1.12.1) over the stored properties.
fn ordinary_own_property_keys(obj: &JsObject) -> Vec<PropertyKey> {
    let props = obj.properties.borrow();
    let mut indices: Vec<(u64, PropertyKey)> = Vec::new();
    let mut strings = Vec::new();
    let mut symbols = Vec::new();
    for (key, _) in props.iter() {
        if let Some(index) = array_index_of(key) {
            indices.push((index, key.clone()));
        } else {
            match key {
                PropertyKey::String(_) => strings.push(key.clone()),
                PropertyKey::Symbol(_) => symbols.push(key.clone()),
            }
        }
    }
    indices.sort_by_key(|(index, _)| *index);
    let mut keys = Vec::with_capacity(props.len());
    keys.extend(indices.into_iter().map(|(_, key)| key));
    keys.extend(strings);
    keys.extend(symbols);
    keys
}

/// ArrayOwnPropertyKeys (spec 10.4.2.5): ordinary keys plus the missing
/// index names from `length - 1` down to 0, appended at the end.
/// ArrayExoticObject [[OwnPropertyKeys]] (spec 10.4.2.6): the ordinary keys
/// plus the array-index keys — only those actually present as own properties;
/// holes are not own keys.
fn array_own_property_keys(array: &JsObject) -> Vec<PropertyKey> {
    ordinary_own_property_keys(array)
}

/// StringOwnPropertyKeys (spec 10.4.3.4): code-unit indices first, then own
/// array-index keys at or beyond the length, then strings, then symbols.
fn string_own_property_keys(string: &JsString, obj: &JsObject) -> Vec<PropertyKey> {
    let mut keys = Vec::new();
    for i in 0..string.len() {
        keys.push(PropertyKey::from_utf8(&i.to_string()));
    }
    let props = obj.properties.borrow();
    let mut late_indices: Vec<(u64, PropertyKey)> = Vec::new();
    let mut strings = Vec::new();
    let mut symbols = Vec::new();
    for (key, _) in props.iter() {
        if let Some(index) = array_index_of(key) {
            if index >= string.len() as u64 {
                late_indices.push((index, key.clone()));
            }
        } else {
            match key {
                PropertyKey::String(_) => strings.push(key.clone()),
                PropertyKey::Symbol(_) => symbols.push(key.clone()),
            }
        }
    }
    late_indices.sort_by_key(|(index, _)| *index);
    keys.extend(late_indices.into_iter().map(|(_, key)| key));
    keys.extend(strings);
    keys.extend(symbols);
    keys
}

/// StringGetOwnProperty (spec 10.4.3.1): the virtual non-writable,
/// enumerable, non-configurable single-code-unit data property.
fn string_get_own_property(string: &JsString, key: &PropertyKey) -> Option<Property> {
    let PropertyKey::String(id) = key else {
        return None;
    };
    let text = lookup(*id);
    let index = canonical_numeric_index_string(text.as_slice())?;
    if index < 0.0 || index.trunc() != index {
        return None;
    }
    let length = string.len() as f64;
    if index >= length {
        return None;
    }
    let unit = string.as_slice().get(index as usize)?;
    Some(Property::data(
        Value::String(Handle::new(JsString::from_utf16(&[*unit]))),
        false,
        true,
        false,
    ))
}

/// ValidateAndApplyPropertyDescriptor with no object to apply to
/// (spec 10.1.6.2 IsCompatiblePropertyDescriptor).
pub(crate) fn is_compatible_property_descriptor(
    extensible: bool,
    desc: &PropertyDescriptor,
    current: Option<&Property>,
) -> Result<bool, JsError> {
    validate_and_apply(
        None,
        &PropertyKey::from_utf8(""),
        extensible,
        desc,
        current.cloned(),
    )
}

/// ValidateAndApplyPropertyDescriptor (spec 10.1.6.4). When `obj` is
/// `Some`, the validated descriptor is applied to the stored property.
fn validate_and_apply(
    obj: Option<&JsObject>,
    key: &PropertyKey,
    extensible: bool,
    desc: &PropertyDescriptor,
    current: Option<Property>,
) -> Result<bool, JsError> {
    let Some(current) = current else {
        // spec steps 2a-2e: a new property requires an extensible object.
        if !extensible {
            return Ok(false);
        }
        let Some(obj) = obj else {
            return Ok(true);
        };
        let mut complete = desc.clone();
        complete.complete();
        let Some(property) = Property::from_descriptor(&complete) else {
            return Ok(false);
        };
        obj.properties.borrow_mut().push((key.clone(), property));
        return Ok(true);
    };
    // spec step 3: an empty descriptor leaves the property untouched.
    if desc.is_empty() {
        return Ok(true);
    }
    // spec step 4: invariant checks against a non-configurable current.
    if !current.configurable {
        if desc.configurable == Some(true) {
            return Ok(false);
        }
        if let Some(enumerable) = desc.enumerable
            && enumerable != current.enumerable
        {
            return Ok(false);
        }
        if !desc.is_generic_descriptor() && desc.is_accessor_descriptor() != current.is_accessor() {
            return Ok(false);
        }
        if current.is_accessor() {
            if let Some(get) = &desc.get
                && !same_value(get, &current.getter().unwrap_or(Value::Undefined))
            {
                return Ok(false);
            }
            if let Some(set) = &desc.set
                && !same_value(set, &current.setter().unwrap_or(Value::Undefined))
            {
                return Ok(false);
            }
        } else if current.writable() == Some(false) {
            if desc.writable == Some(true) {
                return Ok(false);
            }
            if desc.writable.is_none()
                && let Some(value) = &desc.value
                && !same_value(value, &current.value().unwrap_or(Value::Undefined))
            {
                return Ok(false);
            }
        }
    }
    let Some(obj) = obj else {
        return Ok(true);
    };
    // spec step 5: apply — converting between data and accessor when the
    // descriptor changes kind.
    let mut next = current;
    if next.is_data() && desc.is_accessor_descriptor() {
        next = Property::accessor(
            desc.get.clone(),
            desc.set.clone(),
            next.enumerable,
            next.configurable,
        );
    } else if next.is_accessor() && desc.is_data_descriptor() {
        next = Property::data(
            desc.value.clone().unwrap_or(Value::Undefined),
            desc.writable.unwrap_or(false),
            next.enumerable,
            next.configurable,
        );
    }
    if let (Some(value), PropertyKind::Data { value: slot, .. }) = (&desc.value, &mut next.kind) {
        *slot = value.clone();
    }
    if let (Some(writable), PropertyKind::Data { writable: slot, .. }) =
        (desc.writable, &mut next.kind)
    {
        *slot = writable;
    }
    if let Some(enumerable) = desc.enumerable {
        next.enumerable = enumerable;
    }
    if let Some(configurable) = desc.configurable {
        next.configurable = configurable;
    }
    if let (Some(get), PropertyKind::Accessor { get: slot, .. }) = (&desc.get, &mut next.kind) {
        *slot = Some(get.clone()).filter(|v| !matches!(v, Value::Undefined));
    }
    if let (Some(set), PropertyKind::Accessor { set: slot, .. }) = (&desc.set, &mut next.kind) {
        *slot = Some(set.clone()).filter(|v| !matches!(v, Value::Undefined));
    }
    let mut props = obj.properties.borrow_mut();
    if let Some((_, slot)) = props.iter_mut().find(|(name, _)| name == key) {
        *slot = next;
    }
    Ok(true)
}

/// OrdinaryGet (spec 10.1.8.3): walk the prototype chain, invoking accessor
/// getters with the receiver.
fn ordinary_get(obj: &JsObject, key: &PropertyKey, receiver: Value) -> Result<Value, JsError> {
    if let Some(desc) = obj.get_own_property_key(key)? {
        return match desc.kind {
            PropertyKind::Data { value, .. } => Ok(value),
            PropertyKind::Accessor { get, .. } => match get {
                Some(getter) if is_callable(&getter) => call(&getter, receiver, &[]),
                _ => Ok(Value::Undefined),
            },
        };
    }
    match obj.get_prototype_of()? {
        // spec 10.1.9.1 step 5: recurse through the parent's own [[Get]]
        // (a proxy parent runs its get trap, an Integer-Indexed exotic its
        // indexed read), not the ordinary walk.
        Some(parent) => parent.get_with_receiver_key(key, receiver),
        None => Ok(Value::Undefined),
    }
}

/// OrdinarySet (spec 10.1.9.3).
fn ordinary_set(
    obj: &JsObject,
    key: &PropertyKey,
    value: Value,
    receiver: Value,
    throw: bool,
) -> Result<bool, JsError> {
    let own_desc = obj.get_own_property_key(key)?;
    ordinary_set_with_own_descriptor(obj, key, value, receiver, own_desc, throw)
}

/// OrdinarySetWithOwnDescriptor (spec 10.1.9.4).
fn ordinary_set_with_own_descriptor(
    obj: &JsObject,
    key: &PropertyKey,
    value: Value,
    receiver: Value,
    own_desc: Option<Property>,
    throw: bool,
) -> Result<bool, JsError> {
    let Some(own_desc) = own_desc else {
        // spec steps 1a-1c: not an own property — recurse into the parent's
        // [[Set]], or a synthesized writable data descriptor at the end of
        // the chain whose write lands on the receiver.
        if let Some(parent) = obj.get_prototype_of()? {
            return parent.set_with_receiver_key(key, value, receiver, throw);
        }
        return receiver_data_write(&receiver, key, value, throw);
    };
    match own_desc.kind {
        // spec steps 2a-2e: a writable data property on the receiver.
        PropertyKind::Data { writable, .. } => {
            if !writable {
                return set_failure(throw, key);
            }
            receiver_data_write(&receiver, key, value, throw)
        }
        // spec steps 3a-3e: an accessor — invoke the setter.
        PropertyKind::Accessor { set, .. } => match set {
            Some(setter) if is_callable(&setter) => {
                call(&setter, receiver, &[value])?;
                Ok(true)
            }
            _ => {
                if throw {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        format!(
                            "Cannot set property {:?} which has only a getter",
                            key.display_string()
                        ),
                    ));
                }
                Ok(false)
            }
        },
    }
}

/// spec 10.1.3.3 steps 2b-2e: write a data value to the receiver, checking
/// the receiver's own descriptor first (a proxy receiver runs its
/// getOwnPropertyDescriptor and defineProperty traps).
fn receiver_data_write(
    receiver: &Value,
    key: &PropertyKey,
    value: Value,
    throw: bool,
) -> Result<bool, JsError> {
    if !matches!(receiver, Value::Object(_) | Value::Function(_)) {
        return Ok(false);
    }
    if let Some(existing) = receiver_get_own_property(receiver, key)? {
        if !existing.is_data() || existing.writable() != Some(true) {
            return Ok(false);
        }
        let value_desc = PropertyDescriptor {
            value: Some(value),
            writable: None,
            get: None,
            set: None,
            enumerable: None,
            configurable: None,
        };
        return receiver_define_property(receiver, key, &value_desc);
    }
    receiver_create_data_property(receiver, key, value, throw)
}

fn set_failure(throw: bool, key: &PropertyKey) -> Result<bool, JsError> {
    if throw {
        return Err(JsError::new(
            ErrorKind::TypeError,
            format!(
                "Cannot assign to read only property {:?}",
                key.display_string()
            ),
        ));
    }
    Ok(false)
}

/// spec 10.1.9.4 step 2e: CreateDataProperty on the receiver. The receiver
/// must be an object; primitives fail the [[Set]].
fn receiver_create_data_property(
    receiver: &Value,
    key: &PropertyKey,
    value: Value,
    throw: bool,
) -> Result<bool, JsError> {
    let created = match receiver {
        Value::Object(obj) => obj.create_data_property_key(key, value)?,
        Value::Function(f) => f.object.create_data_property_key(key, value)?,
        _ => false,
    };
    if created {
        Ok(true)
    } else if throw {
        // spec 10.1.9.3 step 3.e.ii: CreateDataProperty failing on a
        // non-extensible receiver with Throw true is a TypeError.
        Err(JsError::new(
            ErrorKind::TypeError,
            format!(
                "Cannot create property {:?} on a non-extensible object",
                key.display_string()
            ),
        ))
    } else {
        Ok(false)
    }
}

/// `Receiver.[[GetOwnProperty]]` through a language value.
fn receiver_get_own_property(
    receiver: &Value,
    key: &PropertyKey,
) -> Result<Option<Property>, JsError> {
    match receiver {
        Value::Object(obj) => obj.get_own_property_key(key),
        Value::Function(f) => f.object.get_own_property_key(key),
        _ => Ok(None),
    }
}

/// `Receiver.[[DefineOwnProperty]]` through a language value.
fn receiver_define_property(
    receiver: &Value,
    key: &PropertyKey,
    desc: &PropertyDescriptor,
) -> Result<bool, JsError> {
    match receiver {
        Value::Object(obj) => obj.define_property_key(key, desc),
        Value::Function(f) => f.object.define_property_key(key, desc),
        _ => Ok(false),
    }
}

/// ArrayDefineOwnProperty (spec 10.4.2.1): `length` goes through
/// ArraySetLength; array-index definitions keep `length` one past the index.
fn array_define_own_property(
    array: &JsObject,
    key: &PropertyKey,
    desc: &PropertyDescriptor,
) -> Result<bool, JsError> {
    if *key == PropertyKey::from_utf8("length") {
        return array_set_length(array, desc);
    }
    if let Some(index) = array_index_of(key) {
        let length_desc = array
            .ordinary_get_own_property(&PropertyKey::from_utf8("length"))?
            .expect("arrays always have a length property");
        let PropertyKind::Data {
            value: length_value,
            writable,
        } = &length_desc.kind
        else {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "array length is not a data property".into(),
            ));
        };
        let length = match length_value {
            Value::Number(n) => *n,
            _ => {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "array length is not a Number".into(),
                ));
            }
        };
        if (index as f64) >= length && !writable {
            return Ok(false);
        }
        if !array.ordinary_define_own_property(key, desc)? {
            return Ok(false);
        }
        if (index as f64) >= length {
            let mut new_length = length_desc.clone();
            if let PropertyKind::Data { value, .. } = &mut new_length.kind {
                *value = Value::Number(index as f64 + 1.0);
            }
            array.ordinary_define_own_property(
                &PropertyKey::from_utf8("length"),
                &new_length.to_descriptor(),
            )?;
        }
        return Ok(true);
    }
    array.ordinary_define_own_property(key, desc)
}

/// ArraySetLength (spec 10.4.2.4).
fn array_set_length(array: &JsObject, desc: &PropertyDescriptor) -> Result<bool, JsError> {
    let Some(value) = &desc.value else {
        return array.ordinary_define_own_property(&PropertyKey::from_utf8("length"), desc);
    };
    let new_length = crate::convert::to_uint32(crate::convert::to_number(value)?);
    let number_length = crate::convert::to_number(value)?;
    if !same_value_zero(
        &Value::Number(new_length as f64),
        &Value::Number(number_length),
    ) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "Invalid array length".into(),
        ));
    }
    let mut new_length_desc = desc.clone();
    new_length_desc.value = Some(Value::Number(new_length as f64));
    let old_length_desc = array
        .ordinary_get_own_property(&PropertyKey::from_utf8("length"))?
        .expect("arrays always have a length property");
    let old_length = old_length_desc
        .value()
        .and_then(|v| match v {
            Value::Number(n) => Some(n),
            _ => None,
        })
        .unwrap_or(f64::NAN);
    if (new_length as f64) >= old_length {
        return array
            .ordinary_define_own_property(&PropertyKey::from_utf8("length"), &new_length_desc);
    }
    if old_length_desc.writable() != Some(true) {
        return Ok(false);
    }
    let new_writable =
        if new_length_desc.writable.is_none() || new_length_desc.writable == Some(true) {
            true
        } else {
            new_length_desc.writable = Some(true);
            false
        };
    if !array.ordinary_define_own_property(&PropertyKey::from_utf8("length"), &new_length_desc)? {
        return Ok(false);
    }
    // spec step 13: delete elements at or beyond the new length, descending;
    // on failure pin the length to the first undeletable index.
    let keys = array.own_property_keys()?;
    let mut to_delete: Vec<u64> = keys
        .iter()
        .filter_map(array_index_of)
        .filter(|index| *index >= new_length as u64)
        .collect();
    to_delete.sort_unstable_by(|a, b| b.cmp(a));
    for index in to_delete {
        let key = PropertyKey::from_utf8(&index.to_string());
        if !array.delete_key(&key)? {
            new_length_desc.value = Some(Value::Number(index as f64 + 1.0));
            if !new_writable {
                new_length_desc.writable = Some(false);
            }
            array.ordinary_define_own_property(
                &PropertyKey::from_utf8("length"),
                &new_length_desc,
            )?;
            return Ok(false);
        }
    }
    if !new_writable {
        let writable_false = PropertyDescriptor {
            value: None,
            writable: Some(false),
            get: None,
            set: None,
            enumerable: None,
            configurable: None,
        };
        array.ordinary_define_own_property(&PropertyKey::from_utf8("length"), &writable_false)?;
    }
    Ok(true)
}

/// Arguments [[DefineOwnProperty]] (spec 10.4.4.5): keep the parameter map
/// in sync — detach the mapping when a mapped property becomes
/// non-writable, an accessor, or gets a new value.
fn arguments_define_own_property(
    args: &JsObject,
    slots: &Handle<ArgumentsSlots>,
    key: &PropertyKey,
    desc: &PropertyDescriptor,
) -> Result<bool, JsError> {
    let Some(map) = slots.parameter_map.as_ref() else {
        return args.ordinary_define_own_property(key, desc);
    };
    let is_mapped = map.has_own_property_key(key)?;
    let mut new_arg_desc = desc.clone();
    if is_mapped
        && desc.is_data_descriptor()
        && desc.value.is_none()
        && desc.writable == Some(false)
    {
        new_arg_desc.value = Some(map.get_key(key)?);
    }
    if !args.ordinary_define_own_property(key, &new_arg_desc)? {
        return Ok(false);
    }
    if is_mapped {
        if desc.is_accessor_descriptor() {
            map.delete_key(key)?;
        } else {
            if let Some(value) = &desc.value {
                map.set_key(key, value.clone(), false)?;
            }
            if desc.writable == Some(false) {
                map.delete_key(key)?;
            }
        }
    }
    Ok(true)
}

/// CanonicalNumericIndexString (spec 7.1.21) restricted to the canonical
/// decimal integers the engine needs: *"-0"* maps to -0, other strings must
/// round-trip ToNumber/ToString. Non-integers and huge magnitudes return
/// `None` (they are never valid string indices).
fn canonical_numeric_index_string(text: &[u16]) -> Option<f64> {
    // spec 7.1.21 CanonicalNumericIndexString: "-0" is its own case; any
    // other string must round-trip through ToNumber → ToString (so "-1" and
    // "1.1" are canonical while "01", "1e2", and "0x10" are not).
    if text == [b'-' as u16, b'0' as u16] {
        return Some(-0.0);
    }
    let value = crate::convert::string_numeric_literal(text);
    let roundtrip = crate::number::to_string(value);
    (roundtrip.as_slice() == text).then_some(value)
}

/// CanonicalNumericIndexString (spec 7.1.21) for a property key: `None` for
/// symbol keys.
fn canonical_index(key: &PropertyKey) -> Option<f64> {
    let PropertyKey::String(id) = key else {
        return None;
    };
    canonical_numeric_index_string(lookup(*id).as_slice())
}

/// Whether `key` is a canonical numeric index string (spec 7.1.21); the
/// Integer-Indexed exotic [[Get]]/[[Set]]/[[HasProperty]]/… intercept these,
/// so property reads on a TypedArray must not consult the prototype chain.
pub fn is_canonical_index_key(key: &PropertyKey) -> bool {
    canonical_index(key).is_some()
}

/// Parse a canonical non-negative decimal integer (no leading zeros).
fn parse_canonical_u64(text: &[u16]) -> Option<u64> {
    if text.is_empty() {
        return None;
    }
    if text[0] == b'0' as u16 && text.len() > 1 {
        return None;
    }
    let mut value = 0u64;
    for &unit in text {
        if !matches!(unit, 0x30..=0x39) {
            return None;
        }
        value = value
            .checked_mul(10)?
            .checked_add((unit - b'0' as u16) as u64)?;
    }
    Some(value)
}

/// spec 6.1.7.1: an array index is a canonical integer string with value in
/// [0, 2^32 - 2]. Returns the numeric value, or `None`.
pub fn array_index_of(key: &PropertyKey) -> Option<u64> {
    let PropertyKey::String(id) = key else {
        return None;
    };
    let value = parse_canonical_u64(lookup(*id).as_slice())?;
    if value <= 0xFFFF_FFFE {
        Some(value)
    } else {
        None
    }
}

/// The current number of elements a TypedArray view covers: for an auto-length
/// view (resizable buffer without an explicit length) this tracks the buffer's
/// current byte length; for a fixed-length view the elements are zero when the
/// buffer has shrunk below the view's byte range and restored when it grows
/// back (spec 25.2.2.1 steps 12-14 with resizable buffers).
pub fn typed_array_effective_length(slots: &TypedArraySlots) -> usize {
    let element_size = slots.element_type.size();
    if slots.auto_length {
        slots.buffer.byte_length().saturating_sub(slots.byte_offset) / element_size
    } else if slots.byte_offset + slots.byte_length <= slots.buffer.byte_length() {
        slots.array_length
    } else {
        0
    }
}

/// Whether a canonical numeric index is an in-bounds TypedArray element
/// (spec 10.4.7.4 IsValidIntegerIndex): a detached buffer, a non-integer, and
/// -0 are never valid.
fn typed_array_valid_index(slots: &TypedArraySlots, index: f64) -> bool {
    !slots.buffer.is_detached()
        && index >= 0.0
        && !index.is_sign_negative()
        && index.trunc() == index
        && (index as usize) < typed_array_effective_length(slots)
}

/// The element bytes of the TypedArray at a valid canonical index.
fn element_bytes(slots: &TypedArraySlots, index: f64) -> Result<Vec<u8>, JsError> {
    let index = index as usize;
    let offset = slots.byte_offset + index * slots.element_type.size();
    slots
        .buffer
        .read(offset, slots.element_type.size())
        .map_err(|_| {
            JsError::new(
                ErrorKind::TypeError,
                "TypedArray element access out of bounds".into(),
            )
        })
}

/// TypedArray [[GetOwnProperty]] (spec 10.4.7.2): an in-bounds canonical
/// index produces a writable/enumerable/configurable data property backed
/// by the buffer; other canonical numeric strings (incl. a detached buffer,
/// which reads *undefined*) produce no property.
fn typed_array_get_own_property(
    obj: &JsObject,
    slots: &TypedArraySlots,
    key: &PropertyKey,
) -> Result<Option<Property>, JsError> {
    if let Some(index) = canonical_index(key) {
        if typed_array_valid_index(slots, index) {
            let bytes = element_bytes(slots, index)?;
            let value = crate::typed_array::decode_element(slots.element_type, &bytes, 0)?;
            return Ok(Some(Property::data(value, true, true, true)));
        }
        return Ok(None);
    }
    obj.ordinary_get_own_property(key)
}

/// TypedArray [[DefineOwnProperty]] (spec 10.4.5.5): element descriptors must
/// be writable/enumerable/configurable data properties; the write goes to
/// the buffer (SetValueInBuffer).
fn typed_array_define_own_property(
    obj: &JsObject,
    slots: &TypedArraySlots,
    key: &PropertyKey,
    desc: &PropertyDescriptor,
) -> Result<bool, JsError> {
    if let Some(index) = canonical_index(key) {
        // IsValidIntegerIndex is false (a detached buffer, -0, or an
        // out-of-bounds index): the define fails, and DefinePropertyOrThrow
        // turns that into the TypeError the fixtures expect.
        if !typed_array_valid_index(slots, index) {
            return Ok(false);
        }
        if slots.buffer.is_immutable() {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "TypedArray buffer is immutable".into(),
            ));
        }
        if desc.configurable == Some(false) {
            return Ok(false);
        }
        if desc.enumerable == Some(false) {
            return Ok(false);
        }
        if desc.is_accessor_descriptor() {
            return Ok(false);
        }
        if desc.writable == Some(false) {
            return Ok(false);
        }
        if let Some(value) = &desc.value {
            let bytes = crate::typed_array::encode_element(slots.element_type, value)?;
            write_element_bytes(slots, index, &bytes)?;
        }
        return Ok(true);
    }
    obj.ordinary_define_own_property(key, desc)
}

/// Write `bytes` to the buffer at element `index`.
fn write_element_bytes(slots: &TypedArraySlots, index: f64, bytes: &[u8]) -> Result<(), JsError> {
    let offset = slots.byte_offset + index as usize * slots.element_type.size();
    slots.buffer.write(offset, bytes).map_err(|_| {
        JsError::new(
            ErrorKind::TypeError,
            "TypedArray element write out of bounds".into(),
        )
    })
}

/// TypedArray [[Get]] (spec 10.4.5.7): an in-bounds element read from the
/// buffer; out-of-bounds canonical indices read *undefined*.
fn typed_array_get(
    obj: &JsObject,
    slots: &TypedArraySlots,
    key: &PropertyKey,
    receiver: Value,
) -> Result<Value, JsError> {
    if let Some(index) = canonical_index(key) {
        if typed_array_valid_index(slots, index) {
            let bytes = element_bytes(slots, index)?;
            return crate::typed_array::decode_element(slots.element_type, &bytes, 0);
        }
        // An invalid index (incl. a detached buffer) reads *undefined*
        // without consulting the prototype chain (spec 10.4.7.5).
        return Ok(Value::Undefined);
    }
    ordinary_get(obj, key, receiver)
}

/// TypedArray [[Set]] (spec 10.4.5.8): writing an element with the TypedArray
/// as receiver encodes into the buffer; a different receiver skips
/// out-of-bounds indices.
fn typed_array_set(
    obj: &JsObject,
    slots: &TypedArraySlots,
    key: &PropertyKey,
    value: Value,
    receiver: Value,
) -> Result<bool, JsError> {
    if let Some(index) = canonical_index(key) {
        // IntegerIndexedElementSet coerces the value before the index check:
        // a throwing ToNumber/ToBigInt propagates, and a write invalidated
        // by the coercion (detach or resize) is a no-op that reports
        // success; an invalid index on the live buffer also reports success
        // (spec 10.4.7.6 with web-reality semantics).
        if same_value(&receiver, &obj.self_value()) {
            if slots.buffer.is_immutable() {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "TypedArray buffer is immutable".into(),
                ));
            }
            let bytes = crate::typed_array::encode_element(slots.element_type, &value)?;
            if typed_array_valid_index(slots, index) {
                write_element_bytes(slots, index, &bytes)?;
            }
            return Ok(true);
        }
        if !typed_array_valid_index(slots, index) {
            return Ok(true);
        }
    }
    ordinary_set(obj, key, value, receiver, false)
}

/// TypedArray [[Delete]] (spec 10.4.5.9): elements cannot be deleted;
/// out-of-bounds canonical indices report success.
fn typed_array_delete(
    obj: &JsObject,
    slots: &TypedArraySlots,
    key: &PropertyKey,
) -> Result<bool, JsError> {
    if let Some(index) = canonical_index(key) {
        return Ok(!typed_array_valid_index(slots, index));
    }
    // OrdinaryDelete over the stored properties (not the dispatching
    // [[Delete]], which would re-enter the TypedArray branch).
    let mut props = obj.properties.borrow_mut();
    if let Some(position) = props.iter().position(|(name, _)| name == key) {
        if props[position].1.configurable {
            props.remove(position);
            return Ok(true);
        }
        return Ok(false);
    }
    Ok(true)
}

/// TypedArray [[OwnPropertyKeys]] (spec 10.4.5.11): element indices first,
/// then own non-index strings, then symbols (each in insertion order).
fn typed_array_own_property_keys(slots: &TypedArraySlots, obj: &JsObject) -> Vec<PropertyKey> {
    let mut keys = Vec::new();
    for index in 0..typed_array_effective_length(slots) {
        keys.push(PropertyKey::from_utf8(&index.to_string()));
    }
    let mut string_keys = Vec::new();
    let mut symbol_keys = Vec::new();
    for (stored_key, _) in obj.properties.borrow().iter() {
        match stored_key {
            PropertyKey::Symbol(_) => symbol_keys.push(stored_key.clone()),
            PropertyKey::String(_) if array_index_of(stored_key).is_none() => {
                string_keys.push(stored_key.clone())
            }
            _ => {}
        }
    }
    keys.extend(string_keys);
    keys.extend(symbol_keys);
    keys
}

/// The internal methods of a language value (an Object or Function), used by
/// the proxy traps to forward to the target and by the receiver operations of
/// [[Set]].
pub(crate) fn value_get_prototype_of(value: &Value) -> Result<Option<Handle<JsObject>>, JsError> {
    match value {
        Value::Object(obj) => obj.get_prototype_of(),
        Value::Function(f) => f.object.get_prototype_of(),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "value is not an object".into(),
        )),
    }
}

pub(crate) fn value_set_prototype_of(
    value: &Value,
    proto: Option<Handle<JsObject>>,
) -> Result<bool, JsError> {
    match value {
        Value::Object(obj) => obj.set_prototype_of(proto),
        Value::Function(f) => f.object.set_prototype_of(proto),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "value is not an object".into(),
        )),
    }
}

pub(crate) fn value_is_extensible(value: &Value) -> Result<bool, JsError> {
    match value {
        Value::Object(obj) => obj.is_extensible(),
        Value::Function(f) => f.object.is_extensible(),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "value is not an object".into(),
        )),
    }
}

pub(crate) fn value_prevent_extensions(value: &Value) -> Result<bool, JsError> {
    match value {
        Value::Object(obj) => obj.prevent_extensions(),
        Value::Function(f) => f.object.prevent_extensions(),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "value is not an object".into(),
        )),
    }
}

pub(crate) fn value_get_own_property(
    value: &Value,
    key: &PropertyKey,
) -> Result<Option<Property>, JsError> {
    match value {
        Value::Object(obj) => obj.get_own_property_key(key),
        Value::Function(f) => f.object.get_own_property_key(key),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "value is not an object".into(),
        )),
    }
}

pub(crate) fn value_define_property(
    value: &Value,
    key: &PropertyKey,
    desc: &PropertyDescriptor,
) -> Result<bool, JsError> {
    match value {
        Value::Object(obj) => obj.define_property_key(key, desc),
        Value::Function(f) => f.object.define_property_key(key, desc),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "value is not an object".into(),
        )),
    }
}

pub(crate) fn value_has_property(value: &Value, key: &PropertyKey) -> Result<bool, JsError> {
    match value {
        Value::Object(obj) => obj.has_property_key(key),
        Value::Function(f) => f.object.has_property_key(key),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "value is not an object".into(),
        )),
    }
}

pub(crate) fn value_get(
    value: &Value,
    key: &PropertyKey,
    receiver: Value,
) -> Result<Value, JsError> {
    match value {
        Value::Object(obj) => obj.get_with_receiver_key(key, receiver),
        Value::Function(f) => f.object.get_with_receiver_key(key, receiver),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "value is not an object".into(),
        )),
    }
}

pub(crate) fn value_set(
    value: &Value,
    key: &PropertyKey,
    v: Value,
    receiver: Value,
    throw: bool,
) -> Result<bool, JsError> {
    match value {
        Value::Object(obj) => obj.set_with_receiver_key(key, v, receiver, throw),
        Value::Function(f) => f.object.set_with_receiver_key(key, v, receiver, throw),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "value is not an object".into(),
        )),
    }
}

pub(crate) fn value_delete(value: &Value, key: &PropertyKey) -> Result<bool, JsError> {
    match value {
        Value::Object(obj) => obj.delete_key(key),
        Value::Function(f) => f.object.delete_key(key),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "value is not an object".into(),
        )),
    }
}

pub(crate) fn value_own_property_keys(value: &Value) -> Result<Vec<PropertyKey>, JsError> {
    match value {
        Value::Object(obj) => obj.own_property_keys(),
        Value::Function(f) => f.object.own_property_keys(),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "value is not an object".into(),
        )),
    }
}

pub(crate) fn value_get_method(value: &Value, key: &JsString) -> Result<Option<Value>, JsError> {
    match value {
        Value::Object(obj) => obj.get_method(key),
        Value::Function(f) => f.object.get_method(key),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "value is not an object".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbol::Symbol;

    fn key(text: &str) -> JsString {
        JsString::from_utf8(text)
    }

    fn descriptor(
        value: Option<Value>,
        writable: Option<bool>,
        enumerable: Option<bool>,
        configurable: Option<bool>,
    ) -> PropertyDescriptor {
        PropertyDescriptor {
            value,
            writable,
            get: None,
            set: None,
            enumerable,
            configurable,
        }
    }

    fn builtin(
        name: &str,
        f: impl Fn(&Value, &[Value]) -> Result<Value, JsError> + 'static,
    ) -> Value {
        Value::Function(
            crate::function::Function::create_builtin(Some(key(name)), 0, Box::new(f), None, None)
                .unwrap(),
        )
    }

    #[test]
    fn objects_are_identity_equal() {
        let a = JsObject::ordinary_object_create(None);
        let b = JsObject::ordinary_object_create(None);
        assert_eq!(a, a);
        assert_ne!(a, b);
    }

    #[test]
    fn create_data_property_then_get() {
        let obj = JsObject::ordinary_object_create(None);
        obj.create_data_property(&key("x"), Value::Number(1.0))
            .unwrap();
        assert!(obj.has_own_property(&key("x")).unwrap());
        assert_eq!(obj.get(&key("x")).unwrap(), Value::Number(1.0));
        assert_eq!(obj.get(&key("missing")).unwrap(), Value::Undefined);
    }

    #[test]
    fn set_updates_value_keeping_attributes() {
        let obj = JsObject::ordinary_object_create(None);
        obj.define_property(
            &key("x"),
            &descriptor(
                Some(Value::Number(1.0)),
                Some(true),
                Some(false),
                Some(false),
            ),
        )
        .unwrap();
        assert!(obj.set(&key("x"), Value::Number(2.0), false).unwrap());
        let prop = obj.get_own_property(&key("x")).unwrap().unwrap();
        assert_eq!(prop.value(), Some(Value::Number(2.0)));
        assert!(!prop.enumerable);
    }

    #[test]
    fn non_writable_property_rejects_set() {
        let obj = JsObject::ordinary_object_create(None);
        obj.define_property(
            &key("x"),
            &descriptor(
                Some(Value::Number(1.0)),
                Some(false),
                Some(true),
                Some(true),
            ),
        )
        .unwrap();
        assert!(!obj.set(&key("x"), Value::Number(2.0), false).unwrap());
        assert!(obj.set(&key("x"), Value::Number(2.0), true).is_err());
        assert_eq!(obj.get(&key("x")).unwrap(), Value::Number(1.0));
    }

    #[test]
    fn prototype_walk_finds_inherited_properties() {
        let proto = JsObject::ordinary_object_create(None);
        proto
            .create_data_property(&key("p"), Value::Number(7.0))
            .unwrap();
        let obj = JsObject::ordinary_object_create(Some(proto));
        assert!(obj.has_property(&key("p")).unwrap());
        assert!(!obj.has_own_property(&key("p")).unwrap());
        assert_eq!(obj.get(&key("p")).unwrap(), Value::Number(7.0));
    }

    #[test]
    fn delete_requires_configurable() {
        let obj = JsObject::ordinary_object_create(None);
        obj.define_property(&key("fixed"), &PropertyDescriptor::none(Value::Number(1.0)))
            .unwrap();
        obj.create_data_property(&key("free"), Value::Number(2.0))
            .unwrap();
        assert!(!obj.delete(&key("fixed")).unwrap());
        assert!(obj.delete(&key("free")).unwrap());
        assert!(!obj.has_property(&key("free")).unwrap());
        assert!(obj.delete(&key("absent")).unwrap());
    }

    #[test]
    fn non_extensible_rejects_new_properties() {
        let obj = JsObject::ordinary_object_create(None);
        assert!(obj.prevent_extensions().unwrap());
        assert!(
            !obj.create_data_property(&key("x"), Value::Undefined)
                .unwrap()
        );
        assert!(
            obj.create_data_property_or_throw(&key("x"), Value::Undefined)
                .is_err()
        );
    }

    #[test]
    fn set_prototype_of_blocks_cycles_and_non_extensible_change() {
        let a = JsObject::ordinary_object_create(None);
        let b = JsObject::ordinary_object_create(Some(a.clone()));
        // a -> b -> a is a cycle: rejected.
        assert!(!a.set_prototype_of(Some(b.clone())).unwrap());
        assert!(a.set_prototype_of(None).unwrap());
        // Once non-extensible the prototype is fixed.
        assert!(b.set_prototype_of(None).unwrap());
        assert!(b.prevent_extensions().unwrap());
        assert!(!b.set_prototype_of(Some(a.clone())).unwrap());
        // Setting the same prototype is a no-op success.
        assert!(b.set_prototype_of(None).unwrap());
    }

    #[test]
    fn symbol_keyed_properties_are_supported() {
        let obj = JsObject::ordinary_object_create(None);
        let sym = crate::symbol::unscopables();
        let key = PropertyKey::Symbol(sym.as_ref().clone());
        obj.create_data_property_key(&key, Value::Number(9.0))
            .unwrap();
        assert!(obj.has_own_property_key(&key).unwrap());
        assert_eq!(obj.get_key(&key).unwrap(), Value::Number(9.0));
        assert_eq!(
            obj.get(&JsString::from_utf8("x")).unwrap(),
            Value::Undefined
        );
        let other = Symbol::new(Some(JsString::from_utf8("Symbol.unscopables")));
        assert!(
            !obj.has_own_property_key(&PropertyKey::Symbol(other))
                .unwrap()
        );
    }

    #[test]
    fn accessor_properties_invoke_getters_and_setters() {
        let storage = std::rc::Rc::new(std::cell::RefCell::new(Value::Number(1.0)));
        let obj = JsObject::ordinary_object_create(None);
        let getter_storage = storage.clone();
        let getter = crate::function::Function::create_builtin(
            Some(key("get x")),
            0,
            Box::new(move |_, _| Ok(getter_storage.borrow().clone())),
            None,
            None,
        )
        .unwrap();
        let setter_storage = storage.clone();
        let setter = crate::function::Function::create_builtin(
            Some(key("set x")),
            1,
            Box::new(move |_, args| {
                let value = args.first().cloned().unwrap_or(Value::Undefined);
                *setter_storage.borrow_mut() = value;
                Ok(Value::Undefined)
            }),
            None,
            None,
        )
        .unwrap();
        obj.define_property(
            &key("x"),
            &PropertyDescriptor {
                value: None,
                writable: None,
                get: Some(Value::Function(getter)),
                set: Some(Value::Function(setter)),
                enumerable: Some(true),
                configurable: Some(true),
            },
        )
        .unwrap();
        assert_eq!(obj.get(&key("x")).unwrap(), Value::Number(1.0));
        assert!(obj.set(&key("x"), Value::Number(5.0), false).unwrap());
        assert_eq!(obj.get(&key("x")).unwrap(), Value::Number(5.0));
        assert_eq!(storage.borrow().clone(), Value::Number(5.0));
    }

    #[test]
    fn getter_without_setter_rejects_set() {
        let obj = JsObject::ordinary_object_create(None);
        obj.define_property(
            &key("x"),
            &PropertyDescriptor::accessor(
                Some(builtin("get x", |_, _| Ok(Value::Number(3.0)))),
                None,
            ),
        )
        .unwrap();
        assert!(!obj.set(&key("x"), Value::Number(9.0), false).unwrap());
        assert!(obj.set(&key("x"), Value::Number(9.0), true).is_err());
    }

    #[test]
    fn inherited_accessor_uses_receiver() {
        let getter = crate::function::Function::create_builtin(
            Some(key("get r")),
            0,
            Box::new(|this, _| {
                // `this` is the real receiver object, not a copy: the getter
                // must see the receiver's own "_data".
                match this {
                    Value::Object(obj) => obj.get(&key("_data")),
                    _ => Ok(Value::Undefined),
                }
            }),
            None,
            None,
        )
        .unwrap();
        let proto = JsObject::ordinary_object_create(None);
        proto
            .define_property(
                &key("r"),
                &PropertyDescriptor::accessor(Some(Value::Function(getter)), None),
            )
            .unwrap();
        let obj = JsObject::ordinary_object_create(Some(proto));
        obj.create_data_property(&key("_data"), Value::Number(42.0))
            .unwrap();
        // obj.r — the inherited getter sees the receiver's own "_data".
        assert_eq!(obj.get(&key("r")).unwrap(), Value::Number(42.0));
    }

    #[test]
    fn inherited_setter_writes_through_to_receiver() {
        let proto = JsObject::ordinary_object_create(None);
        let setter = crate::function::Function::create_builtin(
            Some(key("set r")),
            1,
            Box::new(|this, args| {
                let value = args.first().cloned().unwrap_or(Value::Undefined);
                match this {
                    Value::Object(obj) => {
                        // Writing a different property proves `this` is the
                        // receiver: the write lands on the receiver.
                        obj.set_key(&PropertyKey::from_utf8("_store"), value, true)?;
                        Ok(Value::Undefined)
                    }
                    _ => Ok(Value::Undefined),
                }
            }),
            None,
            None,
        )
        .unwrap();
        proto
            .define_property(
                &key("r"),
                &PropertyDescriptor::accessor(None, Some(Value::Function(setter))),
            )
            .unwrap();
        let obj = JsObject::ordinary_object_create(Some(proto));
        // Setting through the prototype invokes the setter with the receiver.
        assert!(obj.set(&key("r"), Value::Number(7.0), false).unwrap());
        assert_eq!(obj.get(&key("_store")).unwrap(), Value::Number(7.0));
    }

    #[test]
    fn define_rejects_incompatible_descriptors_on_non_configurable() {
        let obj = JsObject::ordinary_object_create(None);
        obj.define_property(&key("x"), &PropertyDescriptor::none(Value::Number(1.0)))
            .unwrap();
        assert!(
            !obj.define_property(&key("x"), &PropertyDescriptor::data(Value::Number(2.0)))
                .unwrap()
        );
        assert_eq!(obj.get(&key("x")).unwrap(), Value::Number(1.0));
        // Same value with a generic descriptor is allowed.
        assert!(
            obj.define_property(&key("x"), &descriptor(None, None, None, Some(false)))
                .unwrap()
        );
        // Non-configurable data -> accessor conversion fails.
        let accessor = PropertyDescriptor::accessor(Some(Value::Undefined), Some(Value::Undefined));
        assert!(!obj.define_property(&key("x"), &accessor).unwrap());
    }

    #[test]
    fn data_to_accessor_conversion_preserves_attributes() {
        let obj = JsObject::ordinary_object_create(None);
        obj.define_property(
            &key("x"),
            &descriptor(Some(Value::Number(1.0)), Some(true), Some(true), Some(true)),
        )
        .unwrap();
        obj.define_property(
            &key("x"),
            &PropertyDescriptor::accessor(Some(builtin("g", |_, _| Ok(Value::Number(2.0)))), None),
        )
        .unwrap();
        let prop = obj.get_own_property(&key("x")).unwrap().unwrap();
        assert!(prop.is_accessor());
        assert!(prop.enumerable);
        assert!(prop.configurable);
        assert_eq!(obj.get(&key("x")).unwrap(), Value::Number(2.0));
    }

    #[test]
    fn accessor_to_data_conversion_preserves_attributes() {
        let obj = JsObject::ordinary_object_create(None);
        obj.define_property(
            &key("x"),
            &PropertyDescriptor::accessor(Some(builtin("g", |_, _| Ok(Value::Undefined))), None),
        )
        .unwrap();
        assert!(
            obj.define_property(
                &key("x"),
                &descriptor(Some(Value::Number(9.0)), Some(true), None, None)
            )
            .unwrap()
        );
        let prop = obj.get_own_property(&key("x")).unwrap().unwrap();
        assert!(prop.is_data());
        assert_eq!(prop.value(), Some(Value::Number(9.0)));
        assert_eq!(prop.writable(), Some(true));
    }

    #[test]
    fn own_property_keys_order_integer_indices_first() {
        let obj = JsObject::ordinary_object_create(None);
        obj.create_data_property(&key("b"), Value::Undefined)
            .unwrap();
        obj.create_data_property(&key("10"), Value::Undefined)
            .unwrap();
        obj.create_data_property(&key("a"), Value::Undefined)
            .unwrap();
        obj.create_data_property(&key("2"), Value::Undefined)
            .unwrap();
        let names: Vec<String> = obj
            .own_property_keys()
            .unwrap()
            .iter()
            .map(|k| k.display_string())
            .collect();
        assert_eq!(names, ["2", "10", "b", "a"]);
    }

    #[test]
    fn array_create_sets_length_and_grows_on_index_define() {
        let array = JsObject::array_create(None, 0.0).unwrap();
        assert_eq!(array.get(&key("length")).unwrap(), Value::Number(0.0));
        array
            .create_data_property(&key("3"), Value::Number(1.0))
            .unwrap();
        assert_eq!(array.get(&key("length")).unwrap(), Value::Number(4.0));
        let length_desc = array.get_own_property(&key("length")).unwrap().unwrap();
        assert!(!length_desc.configurable);
        assert!(!length_desc.enumerable);
        assert_eq!(length_desc.writable(), Some(true));
    }

    #[test]
    fn array_length_truncation_deletes_elements() {
        let array = JsObject::array_create(None, 0.0).unwrap();
        for i in 0..5 {
            array
                .create_data_property(&key(&i.to_string()), Value::Number(i as f64))
                .unwrap();
        }
        assert!(
            array
                .set(&key("length"), Value::Number(3.0), false)
                .unwrap()
        );
        assert_eq!(array.get(&key("length")).unwrap(), Value::Number(3.0));
        assert!(!array.has_own_property(&key("3")).unwrap());
        assert!(!array.has_own_property(&key("4")).unwrap());
        assert_eq!(array.get(&key("2")).unwrap(), Value::Number(2.0));
    }

    #[test]
    fn array_non_writable_length_blocks_index_growth() {
        let array = JsObject::array_create(None, 0.0).unwrap();
        array
            .define_property(
                &key("length"),
                &descriptor(Some(Value::Number(2.0)), Some(false), None, None),
            )
            .unwrap();
        assert!(
            !array
                .create_data_property(&key("2"), Value::Number(1.0))
                .unwrap()
        );
        assert!(
            array
                .create_data_property(&key("1"), Value::Number(5.0))
                .unwrap()
        );
        assert_eq!(array.get(&key("1")).unwrap(), Value::Number(5.0));
    }

    #[test]
    fn array_length_range_error_on_non_uint32() {
        let array = JsObject::array_create(None, 0.0).unwrap();
        assert!(
            array
                .set(&key("length"), Value::Number(4294967296.0), false)
                .is_err()
        );
        assert!(
            array
                .set(&key("length"), Value::Number(-1.0), false)
                .is_err()
        );
        // -0 is an acceptable length (SameValueZero with +0).
        assert!(
            array
                .set(&key("length"), Value::Number(-0.0), false)
                .unwrap()
        );
    }

    #[test]
    fn array_own_property_keys_omits_holes() {
        let array = JsObject::array_create(None, 2.0).unwrap();
        array
            .create_data_property(&key("1"), Value::Number(1.0))
            .unwrap();
        let names: Vec<String> = array
            .own_property_keys()
            .unwrap()
            .iter()
            .map(|k| k.display_string())
            .collect();
        // Spec 10.4.2.6: only the stored array-index keys appear; the hole
        // at "0" is not an own property key (ES5-era behavior appended it).
        assert_eq!(names, ["1", "length"]);
    }

    #[test]
    fn string_exotic_exposes_virtual_code_units() {
        let string = JsObject::string_create(JsString::from_utf8("abc"), None).unwrap();
        assert_eq!(
            string.get(&key("0")).unwrap(),
            Value::String(Handle::new(JsString::from_utf8("a")))
        );
        assert_eq!(
            string.get(&key("2")).unwrap(),
            Value::String(Handle::new(JsString::from_utf8("c")))
        );
        assert_eq!(string.get(&key("3")).unwrap(), Value::Undefined);
        assert_eq!(string.get(&key("length")).unwrap(), Value::Number(3.0));
        let prop = string.get_own_property(&key("1")).unwrap().unwrap();
        assert_eq!(prop.writable(), Some(false));
        assert!(prop.enumerable);
        assert!(!prop.configurable);
        assert_eq!(string.get(&key("01")).unwrap(), Value::Undefined);
        let sym = PropertyKey::Symbol(Symbol::new(None));
        assert_eq!(string.get_key(&sym).unwrap(), Value::Undefined);
    }

    #[test]
    fn string_exotic_define_on_virtual_index_checks_compatibility() {
        let string = JsObject::string_create(JsString::from_utf8("abc"), None).unwrap();
        // IsCompatiblePropertyDescriptor runs the full validate-and-apply
        // decision table: the same value passes, a different one fails.
        assert!(
            string
                .define_property(
                    &key("0"),
                    &descriptor(
                        Some(Value::String(Handle::new(JsString::from_utf8("a")))),
                        None,
                        None,
                        None
                    ),
                )
                .unwrap()
        );
        assert!(
            !string
                .define_property(
                    &key("0"),
                    &descriptor(
                        Some(Value::String(Handle::new(JsString::from_utf8("z")))),
                        None,
                        None,
                        None
                    ),
                )
                .unwrap()
        );
        assert_eq!(
            string.get(&key("0")).unwrap(),
            Value::String(Handle::new(JsString::from_utf8("a")))
        );
        // An accessor descriptor is incompatible with the data property.
        let accessor = PropertyDescriptor::accessor(Some(Value::Undefined), Some(Value::Undefined));
        assert!(!string.define_property(&key("1"), &accessor).unwrap());
        // Non-index keys define normally.
        assert!(
            string
                .define_property(
                    &key("x"),
                    &descriptor(Some(Value::Number(1.0)), Some(true), Some(true), Some(true)),
                )
                .unwrap()
        );
    }

    #[test]
    fn string_exotic_own_property_keys_emits_indices_first() {
        let string = JsObject::string_create(JsString::from_utf8("ab"), None).unwrap();
        string
            .define_property(
                &key("x"),
                &descriptor(Some(Value::Number(1.0)), Some(true), Some(true), Some(true)),
            )
            .unwrap();
        let names: Vec<String> = string
            .own_property_keys()
            .unwrap()
            .iter()
            .map(|k| k.display_string())
            .collect();
        assert_eq!(names, ["0", "1", "length", "x"]);
    }

    #[test]
    fn string_exotic_set_on_virtual_index_is_rejected() {
        let string = JsObject::string_create(JsString::from_utf8("abc"), None).unwrap();
        assert!(
            !string
                .set(
                    &key("0"),
                    Value::String(Handle::new(JsString::from_utf8("z"))),
                    false
                )
                .unwrap()
        );
        assert!(
            string
                .set(
                    &key("0"),
                    Value::String(Handle::new(JsString::from_utf8("z"))),
                    true
                )
                .is_err()
        );
    }

    #[test]
    fn mapped_arguments_reflect_parameter_bindings() {
        let binding = std::rc::Rc::new(std::cell::RefCell::new(Value::Number(1.0)));
        let binding_for_getter = binding.clone();
        let binding_for_setter = binding.clone();
        let args = JsObject::mapped_arguments_object_create(
            None,
            Value::Undefined,
            &[JsString::from_utf8("a"), JsString::from_utf8("b")],
            &[Value::Number(1.0), Value::Number(2.0)],
            move |_name| {
                let binding = binding_for_getter.clone();
                builtin("get a", move |_, _| Ok(binding.borrow().clone()))
            },
            move |_name| {
                let binding = binding_for_setter.clone();
                builtin("set a", move |_, args| {
                    let value = args.first().cloned().unwrap_or(Value::Undefined);
                    *binding.borrow_mut() = value;
                    Ok(Value::Undefined)
                })
            },
        )
        .unwrap();
        assert_eq!(args.get(&key("0")).unwrap(), Value::Number(1.0));
        assert_eq!(args.get(&key("length")).unwrap(), Value::Number(2.0));
        // Writing the mapped property updates the binding.
        assert!(args.set(&key("0"), Value::Number(10.0), false).unwrap());
        assert_eq!(binding.borrow().clone(), Value::Number(10.0));
        // Reading reflects the binding.
        *binding.borrow_mut() = Value::Number(20.0);
        assert_eq!(args.get(&key("0")).unwrap(), Value::Number(20.0));
        // Deleting the mapped property detaches the mapping.
        assert!(args.delete(&key("0")).unwrap());
        *binding.borrow_mut() = Value::Number(30.0);
        assert_eq!(args.get(&key("0")).unwrap(), Value::Undefined);
    }

    #[test]
    fn mapped_arguments_callee_and_duplicate_names() {
        let args = JsObject::mapped_arguments_object_create(
            None,
            Value::Number(7.0),
            &[JsString::from_utf8("a"), JsString::from_utf8("a")],
            &[Value::Number(1.0), Value::Number(2.0)],
            |_name| builtin("get a", |_, _| Ok(Value::Undefined)),
            |_name| builtin("set a", |_, _| Ok(Value::Undefined)),
        )
        .unwrap();
        assert_eq!(args.get(&key("callee")).unwrap(), Value::Number(7.0));
        assert!(args.has_own_property(&key("0")).unwrap());
        assert!(args.has_own_property(&key("1")).unwrap());
    }

    #[test]
    fn unmapped_arguments_have_throwing_callee() {
        let thrower = Value::Function(crate::function::throw_type_error(None).unwrap());
        let args = JsObject::unmapped_arguments_object_create(None, &[Value::Number(1.0)], thrower)
            .unwrap();
        assert_eq!(args.get(&key("0")).unwrap(), Value::Number(1.0));
        assert!(args.get(&key("callee")).is_err());
        assert!(args.set(&key("callee"), Value::Undefined, false).is_err());
    }

    #[test]
    fn integer_indexed_shell_routes_index_keys() {
        let buffer = crate::typed_array::SharedBuffer::new(4);
        let typed = JsObject::integer_indexed_object_create(
            TypedArraySlots {
                buffer_object: Value::Undefined,
                buffer: buffer.clone(),
                element_type: crate::typed_array::ElementType::Uint8,
                byte_length: 4,
                byte_offset: 0,
                auto_length: false,
                array_length: 3,
            },
            None,
        )
        .unwrap();
        // Element indices are virtual: own keys lists them ascending, and
        // ordinary keys still work.
        typed
            .create_data_property(&key("name"), Value::String(Handle::new(key("u8"))))
            .unwrap();
        let names: Vec<String> = typed
            .own_property_keys()
            .unwrap()
            .iter()
            .map(|k| k.display_string())
            .collect();
        assert_eq!(names, ["0", "1", "2", "name"]);
        // HasProperty / Delete behave correctly without a buffer.
        assert!(typed.has_own_property(&key("0")).unwrap());
        assert!(!typed.has_own_property(&key("3")).unwrap());
        assert!(!typed.delete(&key("0")).unwrap());
        assert!(typed.delete(&key("3")).unwrap());
        assert!(typed.delete(&key("name")).unwrap());
        // Out-of-bounds canonical index reads are *undefined*.
        assert_eq!(typed.get(&key("3")).unwrap(), Value::Undefined);
        // Descriptor checks reject incompatible defines.
        assert!(
            !typed
                .define_property(
                    &key("0"),
                    &PropertyDescriptor::accessor(Some(Value::Undefined), None),
                )
                .unwrap()
        );
        assert!(
            !typed
                .define_property(
                    &key("0"),
                    &descriptor(Some(Value::Undefined), Some(false), None, None),
                )
                .unwrap()
        );
        // In-bounds element access reads and writes the shared buffer.
        typed.set(&key("0"), Value::Number(200.0), false).unwrap();
        assert_eq!(typed.get(&key("0")).unwrap(), Value::Number(200.0));
        assert_eq!(typed.get(&key("1")).unwrap(), Value::Number(0.0));
        typed
            .define_property(
                &key("1"),
                &descriptor(
                    Some(Value::Number(255.0)),
                    Some(true),
                    Some(true),
                    Some(true),
                ),
            )
            .unwrap();
        assert_eq!(typed.get(&key("1")).unwrap(), Value::Number(255.0));
        // Element writes overflow into the byte type.
        typed.set(&key("2"), Value::Number(300.0), false).unwrap();
        assert_eq!(typed.get(&key("2")).unwrap(), Value::Number(44.0));
        // Non-canonical keys are ordinary.
        typed
            .create_data_property(&key("01"), Value::Number(9.0))
            .unwrap();
        assert_eq!(typed.get(&key("01")).unwrap(), Value::Number(9.0));
    }

    #[test]
    fn module_namespace_shell_is_frozen_with_null_prototype() {
        let ns = JsObject::module_namespace_object_create(Vec::new()).unwrap();
        assert!(ns.get_prototype_of().unwrap().is_none());
        assert!(!ns.is_extensible().unwrap());
        assert!(ns.prevent_extensions().unwrap());
        assert!(!ns.set_prototype_of(None).unwrap());
        assert!(ns.own_property_keys().unwrap().is_empty());
        assert_eq!(ns.get(&key("x")).unwrap(), Value::Undefined);
        assert!(!ns.has_property(&key("x")).unwrap());
        assert!(
            !ns.define_property(&key("x"), &PropertyDescriptor::data(Value::Undefined))
                .unwrap()
        );
        assert!(!ns.set(&key("x"), Value::Undefined, false).unwrap());
        assert!(!ns.delete(&key("x")).unwrap());
    }

    #[test]
    fn array_index_of_recognizes_canonical_strings() {
        let cases = [
            ("0", Some(0)),
            ("5", Some(5)),
            ("4294967294", Some(0xFFFF_FFFE)),
            ("4294967295", None),
            ("01", None),
            ("-1", None),
            ("1.5", None),
            ("", None),
            ("abc", None),
        ];
        for (text, expected) in cases {
            assert_eq!(
                array_index_of(&PropertyKey::from_utf8(text)),
                expected,
                "array_index_of({text:?})"
            );
        }
        assert_eq!(
            array_index_of(&PropertyKey::Symbol(Symbol::new(None))),
            None
        );
    }
}
