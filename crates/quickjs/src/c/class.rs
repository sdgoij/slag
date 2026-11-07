//! Classes: `JSClassID` allocation, registration, per-instance opaque data,
//! class prototypes, and the `call`/`finalize` callbacks. Instances are host
//! objects (`crux::host::HostOps`) whose class id and opaque pointer live in a
//! side table keyed by object id.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};
use std::rc::Rc;

use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::host::HostOps;
use crux::object::{JsObject, Property, PropertyKind};
use crux::property::{PropertyDescriptor, PropertyKey};
#[cfg(feature = "ng")]
use crux::string::JsString;
use crux::string::lookup;
use crux::value::Value as CruxValue;

use crate::c::abi;
use crate::c::atom;
use crate::c::runtime::{JSContext, JSRuntime, context_api, current};

pub type JSClassFinalizer = unsafe extern "C" fn(*mut JSRuntime, abi::JSValue);
pub type JSClassCall = unsafe extern "C" fn(
    *mut JSContext,
    abi::JSValue,
    abi::JSValue,
    i32,
    *const abi::JSValue,
    i32,
) -> abi::JSValue;

const JS_CALL_FLAG_CONSTRUCTOR: i32 = 1;

// The `JS_PROP_*` flags the exotic callbacks read and write (quickjs.h); only
// the subset this facade decodes from a filled descriptor is listed.
const JS_PROP_CONFIGURABLE: i32 = 1 << 0;
const JS_PROP_WRITABLE: i32 = 1 << 1;
const JS_PROP_ENUMERABLE: i32 = 1 << 2;
const JS_PROP_HAS_GET: i32 = 1 << 11;
const JS_PROP_HAS_SET: i32 = 1 << 12;
const JS_PROP_HAS_CONFIGURABLE: i32 = 1 << 8;
const JS_PROP_HAS_WRITABLE: i32 = 1 << 9;
const JS_PROP_HAS_ENUMERABLE: i32 = 1 << 10;
const JS_PROP_HAS_VALUE: i32 = 1 << 13;
const JS_PROP_THROW: i32 = 1 << 14;

/// The C `JSPropertyDescriptor` (quickjs.h): flags plus the three values.
#[repr(C)]
pub struct JSPropertyDescriptor {
    flags: i32,
    value: abi::JSValue,
    getter: abi::JSValue,
    setter: abi::JSValue,
}

/// The C `JSPropertyEnum` (quickjs.h). `is_enumerable` is a C `bool` (1 byte)
/// in QuickJS-NG and a `JS_BOOL` (`int`, 4 bytes) in Bellard; `abi::JS_BOOL`
/// names the right type per target. Both put `atom` at offset 4 (the byte-pad
/// of `_Bool` and the 4-byte `int` align it identically).
#[repr(C)]
pub struct JSPropertyEnum {
    is_enumerable: abi::JS_BOOL,
    atom: atom::JSAtom,
}

/// The C `JSClassExoticMethods` (quickjs.h): seven exotic callbacks in
/// QuickJS-NG, plus four more (`get_prototype`, `set_prototype`, `is_extensible`,
/// `prevent_extensions`) in Bellard. The facade only reads the first seven; the
/// trailing fields exist so the struct is layout-identical to each header.
#[repr(C)]
pub struct JSClassExoticMethods {
    get_own_property: Option<
        unsafe extern "C" fn(
            *mut JSContext,
            *mut JSPropertyDescriptor,
            abi::JSValue,
            atom::JSAtom,
        ) -> i32,
    >,
    get_own_property_names: Option<
        unsafe extern "C" fn(
            *mut JSContext,
            *mut *mut JSPropertyEnum,
            *mut u32,
            abi::JSValue,
        ) -> i32,
    >,
    delete_property:
        Option<unsafe extern "C" fn(*mut JSContext, abi::JSValue, atom::JSAtom) -> i32>,
    define_own_property: Option<
        unsafe extern "C" fn(
            *mut JSContext,
            abi::JSValue,
            atom::JSAtom,
            abi::JSValue,
            abi::JSValue,
            abi::JSValue,
            i32,
        ) -> i32,
    >,
    has_property: Option<unsafe extern "C" fn(*mut JSContext, abi::JSValue, atom::JSAtom) -> i32>,
    get_property: Option<
        unsafe extern "C" fn(
            *mut JSContext,
            abi::JSValue,
            atom::JSAtom,
            abi::JSValue,
        ) -> abi::JSValue,
    >,
    set_property: Option<
        unsafe extern "C" fn(
            *mut JSContext,
            abi::JSValue,
            atom::JSAtom,
            abi::JSValue,
            abi::JSValue,
            i32,
        ) -> i32,
    >,
    #[cfg(feature = "bellard")]
    get_prototype: Option<unsafe extern "C" fn(*mut JSContext, abi::JSValue) -> abi::JSValue>,
    #[cfg(feature = "bellard")]
    set_prototype: Option<unsafe extern "C" fn(*mut JSContext, abi::JSValue, abi::JSValue) -> i32>,
    #[cfg(feature = "bellard")]
    is_extensible: Option<unsafe extern "C" fn(*mut JSContext, abi::JSValue) -> i32>,
    #[cfg(feature = "bellard")]
    prevent_extensions: Option<unsafe extern "C" fn(*mut JSContext, abi::JSValue) -> i32>,
}

/// The C `JSClassDef` (five pointer fields). `gc_mark` is accepted but never
/// called (host refs are roots); `exotic` dispatches to the exotic methods.
#[repr(C)]
pub struct JSClassDef {
    class_name: *const c_char,
    finalizer: Option<JSClassFinalizer>,
    gc_mark: *const c_void,
    call: Option<JSClassCall>,
    exotic: *const JSClassExoticMethods,
}

struct ClassEntry {
    name: String,
    def: *const JSClassDef,
    ops: Rc<ClassOps>,
}

#[derive(Clone, Copy)]
struct OpaqueEntry {
    class_id: u32,
    opaque: *mut c_void,
}

struct ClassOps {
    class_id: u32,
    rt: *mut JSRuntime,
    def: *const JSClassDef,
}

impl std::fmt::Debug for ClassOps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ClassOps({})", self.class_id)
    }
}

impl ClassOps {
    fn def_ref(&self) -> &JSClassDef {
        // SAFETY: the host keeps the `JSClassDef` alive while the class is
        // registered (QuickJS's contract).
        unsafe { &*self.def }
    }

    fn call_callback(&self) -> Option<JSClassCall> {
        self.def_ref().call
    }

    fn finalizer_callback(&self) -> Option<JSClassFinalizer> {
        self.def_ref().finalizer
    }

    fn exotic_ref(&self) -> Option<&JSClassExoticMethods> {
        // SAFETY: the host keeps the exotic table alive for the class's
        // lifetime, like `JSClassDef` itself.
        let exotic = self.def_ref().exotic;
        if exotic.is_null() {
            None
        } else {
            Some(unsafe { &*exotic })
        }
    }

    /// Dispatch a `call` or `construct` to the class's C `call` callback.
    fn dispatch(
        &self,
        object: &JsObject,
        this: &CruxValue,
        args: &[CruxValue],
        flags: i32,
    ) -> Option<Result<CruxValue, JsError>> {
        let callback = self.call_callback()?;
        let ctx = current();
        if ctx.is_null() {
            return Some(Err(JsError::new(
                ErrorKind::TypeError,
                "no current context for a class call".into(),
            )));
        }
        let func_obj = object
            .handle()
            .map(CruxValue::Object)
            .map(abi::value_to_jsvalue)?;
        let this_val = abi::value_to_jsvalue(*this);
        let argv: Vec<abi::JSValue> = args.iter().map(|arg| abi::value_to_jsvalue(*arg)).collect();

        // SAFETY: `ctx` names a live context; `func_obj`, `this_val` and `argv`
        // are valid for the duration of the callback (the boxes are freed below,
        // after it returns).
        let result = unsafe {
            callback(
                ctx,
                func_obj,
                this_val,
                argv.len() as i32,
                argv.as_ptr(),
                flags,
            )
        };

        abi::free_jsvalue(func_obj);
        abi::free_jsvalue(this_val);
        for arg in argv {
            abi::free_jsvalue(arg);
        }

        let converted = abi::jsvalue_to_value(result);
        abi::free_jsvalue(result);
        match converted {
            Some(value) => Some(Ok(value)),
            // The callback set a pending exception; the caller surfaces it.
            None => Some(Err(JsError::new(
                ErrorKind::Error,
                "host class call threw".into(),
            ))),
        }
    }
}

impl HostOps for ClassOps {
    fn is_callable(&self) -> bool {
        self.call_callback().is_some()
    }

    fn is_constructible(&self) -> bool {
        self.call_callback().is_some()
    }

    fn call(
        &self,
        object: &JsObject,
        this: &CruxValue,
        args: &[CruxValue],
    ) -> Option<Result<CruxValue, JsError>> {
        self.dispatch(object, this, args, 0)
    }

    fn construct(
        &self,
        object: &JsObject,
        args: &[CruxValue],
        new_target: &CruxValue,
    ) -> Option<Result<CruxValue, JsError>> {
        // QuickJS passes `new.target` as `this_val` for a constructor call.
        self.dispatch(object, new_target, args, JS_CALL_FLAG_CONSTRUCTOR)
    }

    fn finalize(&self, object_id: u64) {
        if let Some(callback) = self.finalizer_callback() {
            // A dead-object ref: the object id rides in `u`, and `JS_GetOpaque`
            // decodes it from the side table (which is cleared below).
            let value = abi::JSValue {
                u: object_id,
                tag: abi::JS_TAG_OBJECT,
            };
            // SAFETY: `rt` is live while the class is registered.
            unsafe { callback(self.rt, value) };
        }
        OPAQUE.with(|opaque| opaque.borrow_mut().remove(&object_id));
    }

    fn get_own_property(
        &self,
        object: &JsObject,
        key: &PropertyKey,
    ) -> Option<Result<Property, JsError>> {
        let callback = self.exotic_ref()?.get_own_property?;
        let Some(ctx) = current_ctx() else {
            return Some(Err(no_context_error()));
        };
        let obj = match object_jsvalue(object) {
            Ok(obj) => obj,
            Err(error) => return Some(Err(error)),
        };
        let prop = property_key_atom(key);
        let mut desc = JSPropertyDescriptor {
            flags: 0,
            value: abi::undefined(),
            getter: abi::undefined(),
            setter: abi::undefined(),
        };
        // SAFETY: `ctx` names a live context; `obj` and `prop` are valid for
        // the callback; `desc` is a writable out slot.
        let result = unsafe { callback(ctx, &mut desc, obj, prop) };
        atom::free_atom(prop);
        abi::free_jsvalue(obj);
        if result < 0 {
            return Some(Err(JsError::new(
                ErrorKind::Error,
                "host exotic get_own_property threw".into(),
            )));
        }
        if result == 0 {
            return None;
        }
        let property = descriptor_to_property(&desc);
        abi::free_jsvalue(desc.value);
        abi::free_jsvalue(desc.getter);
        abi::free_jsvalue(desc.setter);
        Some(Ok(property))
    }

    fn get(
        &self,
        object: &JsObject,
        key: &PropertyKey,
        receiver: &CruxValue,
    ) -> Option<Result<CruxValue, JsError>> {
        let callback = self.exotic_ref()?.get_property?;
        let Some(ctx) = current_ctx() else {
            return Some(Err(no_context_error()));
        };
        let obj = match object_jsvalue(object) {
            Ok(obj) => obj,
            Err(error) => return Some(Err(error)),
        };
        let prop = property_key_atom(key);
        let receiver_val = abi::value_to_jsvalue(*receiver);
        // SAFETY: `ctx` names a live context; `obj`, `prop` and `receiver_val`
        // are valid for the callback.
        let result = unsafe { callback(ctx, obj, prop, receiver_val) };
        atom::free_atom(prop);
        abi::free_jsvalue(obj);
        abi::free_jsvalue(receiver_val);
        let converted = abi::jsvalue_to_value(result);
        abi::free_jsvalue(result);
        match converted {
            Some(value) => Some(Ok(value)),
            None => Some(Err(JsError::new(
                ErrorKind::Error,
                "host exotic get_property threw".into(),
            ))),
        }
    }

    fn set(
        &self,
        object: &JsObject,
        key: &PropertyKey,
        value: &CruxValue,
        receiver: &CruxValue,
        throw: bool,
    ) -> Option<Result<bool, JsError>> {
        let callback = self.exotic_ref()?.set_property?;
        let Some(ctx) = current_ctx() else {
            return Some(Err(no_context_error()));
        };
        let obj = match object_jsvalue(object) {
            Ok(obj) => obj,
            Err(error) => return Some(Err(error)),
        };
        let prop = property_key_atom(key);
        let value_val = abi::value_to_jsvalue(*value);
        let receiver_val = abi::value_to_jsvalue(*receiver);
        let flags = if throw { JS_PROP_THROW } else { 0 };
        // SAFETY: `ctx` names a live context; the boxed values are valid for
        // the callback and freed after it returns.
        let result = unsafe { callback(ctx, obj, prop, value_val, receiver_val, flags) };
        atom::free_atom(prop);
        abi::free_jsvalue(obj);
        abi::free_jsvalue(value_val);
        abi::free_jsvalue(receiver_val);
        if result < 0 {
            Some(Err(JsError::new(
                ErrorKind::Error,
                "host exotic set_property threw".into(),
            )))
        } else {
            Some(Ok(result != 0))
        }
    }

    fn has_property(&self, object: &JsObject, key: &PropertyKey) -> Option<Result<bool, JsError>> {
        let callback = self.exotic_ref()?.has_property?;
        let Some(ctx) = current_ctx() else {
            return Some(Err(no_context_error()));
        };
        let obj = match object_jsvalue(object) {
            Ok(obj) => obj,
            Err(error) => return Some(Err(error)),
        };
        let prop = property_key_atom(key);
        // SAFETY: `ctx` names a live context; `obj` and `prop` are valid for
        // the callback.
        let result = unsafe { callback(ctx, obj, prop) };
        atom::free_atom(prop);
        abi::free_jsvalue(obj);
        if result < 0 {
            Some(Err(JsError::new(
                ErrorKind::Error,
                "host exotic has_property threw".into(),
            )))
        } else {
            Some(Ok(result != 0))
        }
    }

    fn delete(&self, object: &JsObject, key: &PropertyKey) -> Option<Result<bool, JsError>> {
        let callback = self.exotic_ref()?.delete_property?;
        let Some(ctx) = current_ctx() else {
            return Some(Err(no_context_error()));
        };
        let obj = match object_jsvalue(object) {
            Ok(obj) => obj,
            Err(error) => return Some(Err(error)),
        };
        let prop = property_key_atom(key);
        // SAFETY: `ctx` names a live context; `obj` and `prop` are valid for
        // the callback.
        let result = unsafe { callback(ctx, obj, prop) };
        atom::free_atom(prop);
        abi::free_jsvalue(obj);
        if result < 0 {
            Some(Err(JsError::new(
                ErrorKind::Error,
                "host exotic delete_property threw".into(),
            )))
        } else {
            Some(Ok(result != 0))
        }
    }

    fn define_property(
        &self,
        object: &JsObject,
        key: &PropertyKey,
        desc: &PropertyDescriptor,
    ) -> Option<Result<bool, JsError>> {
        let callback = self.exotic_ref()?.define_own_property?;
        let Some(ctx) = current_ctx() else {
            return Some(Err(no_context_error()));
        };
        let obj = match object_jsvalue(object) {
            Ok(obj) => obj,
            Err(error) => return Some(Err(error)),
        };
        let prop = property_key_atom(key);
        let value = desc
            .value
            .map(abi::value_to_jsvalue)
            .unwrap_or_else(abi::undefined);
        let getter = desc
            .get
            .map(abi::value_to_jsvalue)
            .unwrap_or_else(abi::undefined);
        let setter = desc
            .set
            .map(abi::value_to_jsvalue)
            .unwrap_or_else(abi::undefined);
        let flags = descriptor_flags(desc);
        // SAFETY: `ctx` names a live context; the boxed values are valid for
        // the callback and freed after it returns.
        let result = unsafe { callback(ctx, obj, prop, value, getter, setter, flags) };
        atom::free_atom(prop);
        abi::free_jsvalue(obj);
        abi::free_jsvalue(value);
        abi::free_jsvalue(getter);
        abi::free_jsvalue(setter);
        if result < 0 {
            Some(Err(JsError::new(
                ErrorKind::Error,
                "host exotic define_own_property threw".into(),
            )))
        } else if result == 0 {
            // FALSE: the host did not handle it; fall back to the default.
            None
        } else {
            Some(Ok(true))
        }
    }

    fn own_property_keys(&self, object: &JsObject) -> Option<Result<Vec<PropertyKey>, JsError>> {
        let callback = self.exotic_ref()?.get_own_property_names?;
        let Some(ctx) = current_ctx() else {
            return Some(Err(no_context_error()));
        };
        let obj = match object_jsvalue(object) {
            Ok(obj) => obj,
            Err(error) => return Some(Err(error)),
        };
        let mut tab: *mut JSPropertyEnum = std::ptr::null_mut();
        let mut len: u32 = 0;
        // SAFETY: `ctx` names a live context; `tab`/`len` are writable out slots.
        let result = unsafe { callback(ctx, &mut tab, &mut len, obj) };
        abi::free_jsvalue(obj);
        if result < 0 {
            return Some(Err(JsError::new(
                ErrorKind::Error,
                "host exotic get_own_property_names threw".into(),
            )));
        }
        let mut keys = Vec::with_capacity(len as usize);
        if !tab.is_null() && len > 0 {
            // SAFETY: the callback filled `len` readable entries.
            let entries = unsafe { std::slice::from_raw_parts(tab, len as usize) };
            for entry in entries {
                if let Some(text) = atom::atom_text(entry.atom) {
                    keys.push(PropertyKey::from_utf8(&text.to_string_lossy()));
                }
                atom::free_atom(entry.atom);
            }
            // The host allocated the table with `js_malloc`; the caller frees it.
            // SAFETY: the callback returned a `js_malloc` allocation.
            unsafe { crate::c::alloc::js_free(ctx, tab as *mut c_void) };
        }
        Some(Ok(keys))
    }
}

fn current_ctx() -> Option<*mut JSContext> {
    let ctx = current();
    (!ctx.is_null()).then_some(ctx)
}

fn no_context_error() -> JsError {
    JsError::new(
        ErrorKind::TypeError,
        "no current context for an exotic method".into(),
    )
}

fn object_jsvalue(object: &JsObject) -> Result<abi::JSValue, JsError> {
    object
        .handle()
        .map(CruxValue::Object)
        .map(abi::value_to_jsvalue)
        .ok_or_else(|| JsError::new(ErrorKind::Error, "host object has no handle".into()))
}

fn property_key_atom(key: &PropertyKey) -> atom::JSAtom {
    match key {
        PropertyKey::String(id) => atom::intern(lookup(*id)),
        // Symbol keys are not interned in the facade's atom table yet.
        PropertyKey::Symbol(_) => 0,
    }
}

fn descriptor_to_property(desc: &JSPropertyDescriptor) -> Property {
    let enumerable = desc.flags & JS_PROP_ENUMERABLE != 0;
    let configurable = desc.flags & JS_PROP_CONFIGURABLE != 0;
    if desc.flags & (JS_PROP_HAS_GET | JS_PROP_HAS_SET) != 0 {
        let get = if desc.flags & JS_PROP_HAS_GET != 0 {
            abi::jsvalue_to_value(desc.getter)
        } else {
            None
        };
        let set = if desc.flags & JS_PROP_HAS_SET != 0 {
            abi::jsvalue_to_value(desc.setter)
        } else {
            None
        };
        Property {
            kind: PropertyKind::Accessor { get, set },
            enumerable,
            configurable,
        }
    } else {
        let value = abi::jsvalue_to_value(desc.value).unwrap_or(CruxValue::Undefined);
        let writable = desc.flags & JS_PROP_WRITABLE != 0;
        Property {
            kind: PropertyKind::Data { value, writable },
            enumerable,
            configurable,
        }
    }
}

/// The `JS_DefineProperty` flags for a descriptor: which fields are present
/// plus the attribute values.
fn descriptor_flags(desc: &PropertyDescriptor) -> i32 {
    let mut flags = 0;
    if let Some(configurable) = desc.configurable {
        flags |= JS_PROP_HAS_CONFIGURABLE;
        if configurable {
            flags |= JS_PROP_CONFIGURABLE;
        }
    }
    if let Some(writable) = desc.writable {
        flags |= JS_PROP_HAS_WRITABLE;
        if writable {
            flags |= JS_PROP_WRITABLE;
        }
    }
    if let Some(enumerable) = desc.enumerable {
        flags |= JS_PROP_HAS_ENUMERABLE;
        if enumerable {
            flags |= JS_PROP_ENUMERABLE;
        }
    }
    if desc.get.is_some() {
        flags |= JS_PROP_HAS_GET;
    }
    if desc.set.is_some() {
        flags |= JS_PROP_HAS_SET;
    }
    if desc.value.is_some() {
        flags |= JS_PROP_HAS_VALUE;
    }
    flags
}

thread_local! {
    static NEXT_CLASS_ID: Cell<u32> = const { Cell::new(1) };
    static CLASSES: RefCell<HashMap<u32, ClassEntry>> = RefCell::new(HashMap::new());
    static CLASS_PROTO: RefCell<HashMap<u32, CruxValue>> = RefCell::new(HashMap::new());
    static OPAQUE: RefCell<HashMap<u64, OpaqueEntry>> = RefCell::new(HashMap::new());
}

/// The object id a `JSValue` names, whether it is live (decoded through its
/// box) or a dead-object ref handed to a finalizer (the id rides in `u`).
fn object_id_of(value: abi::JSValue) -> Option<u64> {
    if let Some(value) = abi::jsvalue_to_value(value)
        && let Some(object) = value.as_object()
    {
        return Some(object.id());
    }
    if value.tag == abi::JS_TAG_OBJECT {
        return Some(value.u);
    }
    None
}

fn allocate_class_id(pclass_id: *mut u32) -> u32 {
    if pclass_id.is_null() {
        return 0;
    }
    let existing = unsafe { *pclass_id };
    if existing != 0 {
        return existing;
    }
    let id = NEXT_CLASS_ID.with(|next| {
        let id = next.get();
        next.set(id + 1);
        id
    });
    unsafe { *pclass_id = id };
    id
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewClassID(_rt: *mut JSRuntime, pclass_id: *mut u32) -> u32 {
    allocate_class_id(pclass_id)
}

#[cfg(feature = "bellard")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewClassID(pclass_id: *mut u32) -> u32 {
    allocate_class_id(pclass_id)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewClass(
    rt: *mut JSRuntime,
    class_id: u32,
    class_def: *const JSClassDef,
) -> i32 {
    let name = if class_def.is_null() {
        String::new()
    } else {
        // SAFETY: the host keeps `class_def` alive for the class's lifetime.
        unsafe { CStr::from_ptr((*class_def).class_name) }
            .to_string_lossy()
            .into_owned()
    };
    let ops = Rc::new(ClassOps {
        class_id,
        rt,
        def: class_def,
    });
    CLASSES.with(|classes| {
        classes.borrow_mut().insert(
            class_id,
            ClassEntry {
                name,
                def: class_def,
                ops,
            },
        );
    });
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_IsRegisteredClass(_rt: *mut JSRuntime, class_id: u32) -> abi::JS_BOOL {
    abi::js_bool(CLASSES.with(|classes| classes.borrow().contains_key(&class_id)))
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_GetClassName(_rt: *mut JSRuntime, class_id: u32) -> u32 {
    let name = CLASSES.with(|classes| {
        classes
            .borrow()
            .get(&class_id)
            .map(|entry| entry.name.clone())
    });
    match name {
        Some(name) => atom::intern(JsString::from_utf8(&name)),
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_SetClassProto(_ctx: *mut JSContext, class_id: u32, obj: abi::JSValue) {
    let Some(value) = abi::jsvalue_to_value(obj) else {
        return;
    };
    CLASS_PROTO.with(|protos| protos.borrow_mut().insert(class_id, value));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_GetClassProto(_ctx: *mut JSContext, class_id: u32) -> abi::JSValue {
    match CLASS_PROTO.with(|protos| protos.borrow().get(&class_id).copied()) {
        Some(value) => abi::value_to_jsvalue(value),
        None => abi::null(),
    }
}

fn class_ops(class_id: u32) -> Option<Rc<ClassOps>> {
    CLASSES.with(|classes| {
        classes
            .borrow()
            .get(&class_id)
            .map(|entry| entry.ops.clone())
    })
}

fn make_instance(class_id: u32, prototype: Option<Handle<JsObject>>) -> abi::JSValue {
    let Some(ops) = class_ops(class_id) else {
        return abi::exception();
    };
    let object = JsObject::host_object_create(ops, prototype);
    OPAQUE.with(|opaque| {
        opaque.borrow_mut().insert(
            object.id(),
            OpaqueEntry {
                class_id,
                opaque: std::ptr::null_mut(),
            },
        );
    });
    abi::value_to_jsvalue(CruxValue::Object(object))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewObjectClass(ctx: *mut JSContext, class_id: u32) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    let prototype = CLASS_PROTO
        .with(|protos| protos.borrow().get(&class_id).copied())
        .and_then(|value| value.as_object())
        .or_else(|| {
            api_ctx
                .intrinsic("%Object.prototype%")
                .and_then(|value| value.as_object())
        });
    make_instance(class_id, prototype)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewObjectProtoClass(
    ctx: *mut JSContext,
    proto: abi::JSValue,
    class_id: u32,
) -> abi::JSValue {
    let Some(_api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    let prototype = abi::jsvalue_to_value(proto).and_then(|value| value.as_object());
    make_instance(class_id, prototype)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_SetOpaque(obj: abi::JSValue, opaque: *mut c_void) -> i32 {
    let Some(object_id) = object_id_of(obj) else {
        return -1;
    };
    OPAQUE.with(|table| {
        let mut table = table.borrow_mut();
        let Some(entry) = table.get_mut(&object_id) else {
            return -1;
        };
        entry.opaque = opaque;
        0
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_GetOpaque(obj: abi::JSValue, class_id: u32) -> *mut c_void {
    let Some(object_id) = object_id_of(obj) else {
        return std::ptr::null_mut();
    };
    OPAQUE.with(|table| {
        table
            .borrow()
            .get(&object_id)
            .filter(|entry| entry.class_id == class_id)
            .map(|entry| entry.opaque)
            .unwrap_or(std::ptr::null_mut())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_GetOpaque2(
    _ctx: *mut JSContext,
    obj: abi::JSValue,
    class_id: u32,
) -> *mut c_void {
    // SAFETY: forwarding the C contract to `JS_GetOpaque`.
    unsafe { JS_GetOpaque(obj, class_id) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_GetAnyOpaque(obj: abi::JSValue, pclass_id: *mut u32) -> *mut c_void {
    let entry = object_id_of(obj)
        .and_then(|object_id| OPAQUE.with(|table| table.borrow().get(&object_id).copied()));
    match entry {
        Some(entry) => {
            if !pclass_id.is_null() {
                // SAFETY: the caller passes a writable out slot or null.
                unsafe { *pclass_id = entry.class_id };
            }
            entry.opaque
        }
        None => {
            if !pclass_id.is_null() {
                // SAFETY: the caller passes a writable out slot or null.
                unsafe { *pclass_id = 0 };
            }
            std::ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_GetClassID(value: abi::JSValue) -> u32 {
    let Some(object_id) = object_id_of(value) else {
        return 0;
    };
    OPAQUE.with(|table| {
        table
            .borrow()
            .get(&object_id)
            .map(|entry| entry.class_id)
            .unwrap_or(0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsproperty_descriptor_has_the_c_layout() {
        assert_eq!(std::mem::size_of::<JSPropertyDescriptor>(), 56);
        assert_eq!(std::mem::align_of::<JSPropertyDescriptor>(), 8);
        assert_eq!(std::mem::offset_of!(JSPropertyDescriptor, value), 8);
        assert_eq!(std::mem::offset_of!(JSPropertyDescriptor, getter), 24);
        assert_eq!(std::mem::offset_of!(JSPropertyDescriptor, setter), 40);
    }

    #[test]
    fn jsproperty_enum_has_the_c_layout() {
        assert_eq!(std::mem::size_of::<JSPropertyEnum>(), 8);
        assert_eq!(std::mem::align_of::<JSPropertyEnum>(), 4);
        assert_eq!(std::mem::offset_of!(JSPropertyEnum, atom), 4);
    }

    #[test]
    fn jsclass_def_has_the_c_layout() {
        assert_eq!(std::mem::size_of::<JSClassDef>(), 40);
        assert_eq!(std::mem::align_of::<JSClassDef>(), 8);
    }

    #[test]
    fn jsclass_exotic_methods_has_the_c_layout() {
        #[cfg(feature = "ng")]
        let callbacks = 7usize;
        #[cfg(feature = "bellard")]
        let callbacks = 11usize;
        assert_eq!(
            std::mem::size_of::<JSClassExoticMethods>(),
            callbacks * std::mem::size_of::<usize>()
        );
        assert_eq!(
            std::mem::align_of::<JSClassExoticMethods>(),
            std::mem::align_of::<usize>()
        );
    }
}
