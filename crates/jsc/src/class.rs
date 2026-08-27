//! `JSClassRef`: host classes with callbacks (JSClassRef.h). Each class
//! dispatches the object internal methods its definition declares (absent
//! callbacks behave ordinarily) and shares one ops handle across instances,
//! so `JSValueIsObjectOfClass` is an `Rc::ptr_eq` on that handle.
//!
//! Classes are intentionally process-lifetime: `JSClassRelease` drops the
//! registry entry but the allocation is leaked, so instances (and the
//! callbacks they dispatch) stay valid for the life of the process — real
//! JSC classes are effectively permanent too.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::CStr;
use std::rc::Rc;

use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::host::HostOps;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::Value;

use crate::context::{JscContext, error_from_exception, release_string_ref, string_ref};
use crate::refs;
use crate::{
    JSClassDefinition, JSClassRef, JSContextRef, JSObjectCallAsFunctionCallback,
    JSObjectGetPropertyCallback, JSObjectSetPropertyCallback, JSPropertyNameAccumulatorRef,
    JSStaticFunction, JSStaticValue, JSValueRef,
};

thread_local! {
    /// Live classes keyed by their (stable, leaked) heap address.
    static CLASSES: RefCell<HashMap<usize, *mut ClassEntry>> = RefCell::new(HashMap::new());
    /// Object id -> host private data (JSObjectSetPrivate/GetPrivate). The
    /// API has no context parameter, so the data is thread-global; entries
    /// live for the thread (they hold host pointers, not engine values).
    static PRIVATE: RefCell<HashMap<u64, usize>> = RefCell::new(HashMap::new());
}

/// Set an object's host private data.
pub(crate) fn set_private(object_id: u64, data: usize) {
    PRIVATE.with(|private| private.borrow_mut().insert(object_id, data));
}

/// Read an object's host private data.
pub(crate) fn get_private(object_id: u64) -> Option<usize> {
    PRIVATE.with(|private| private.borrow().get(&object_id).copied())
}

/// The per-class state: the definition (pointer copies of the host's C
/// struct — the host keeps the strings/arrays alive) and the shared ops
/// handle.
struct ClassEntry {
    definition: Box<JSClassDefinition>,
    ops: Rc<ClassOps>,
}

/// The `HostOps` implementation dispatching to a class's callbacks. Holds a
/// pointer into its `ClassEntry` (valid for the process lifetime).
struct ClassOps {
    entry: Cell<*const ClassEntry>,
}

impl ClassOps {
    fn entry(&self) -> &'static ClassEntry {
        unsafe { &*self.entry.get() }
    }

    /// The shared ops handle, for handing to new instances.
    fn ops_rc(&self) -> Rc<dyn HostOps> {
        self.entry().ops.clone()
    }

    fn definition(&self) -> &'static JSClassDefinition {
        &self.entry().definition
    }

    fn callbacks_ctx(&self) -> Option<&'static JscContext> {
        let ctx = JscContext::current();
        if ctx.is_null() {
            None
        } else {
            JscContext::get(ctx)
        }
    }
}

impl std::fmt::Debug for ClassOps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ClassOps")
    }
}

/// The property-name accumulator a `getPropertyNames` callback writes to.
pub(crate) fn property_names_of(
    accumulator: JSPropertyNameAccumulatorRef,
) -> &'static mut Vec<JsString> {
    unsafe { &mut *(accumulator as *mut Vec<JsString>) }
}

fn jsstring_of_key(key: &PropertyKey) -> JsString {
    JsString::from_utf8(&key.display_string())
}

impl HostOps for ClassOps {
    fn has_property(&self, object: &JsObject, key: &PropertyKey) -> Option<Result<bool, JsError>> {
        let callback = self.definition().has_property?;
        let ctx = self.callbacks_ctx()?;
        let object_ref = refs::object_id_ref(object.id());
        let name_ref = string_ref(&jsstring_of_key(key));
        let result = unsafe { callback(ctx as *const _ as JSContextRef, object_ref, name_ref) };
        release_string_ref(name_ref);
        Some(Ok(result))
    }

    fn get(
        &self,
        object: &JsObject,
        key: &PropertyKey,
        _receiver: &Value,
    ) -> Option<Result<Value, JsError>> {
        let callback = self.definition().get_property?;
        let ctx = self.callbacks_ctx()?;
        let object_ref = refs::object_id_ref(object.id());
        let name_ref = string_ref(&jsstring_of_key(key));
        let mut exception: JSValueRef = std::ptr::null_mut();
        let result = unsafe {
            callback(
                ctx as *const _ as JSContextRef,
                object_ref,
                name_ref,
                &mut exception,
            )
        };
        release_string_ref(name_ref);
        if !exception.is_null() {
            return Some(Err(error_from_exception(exception)));
        }
        Some(Ok(refs::ref_to_value(result).unwrap_or(Value::Undefined)))
    }

    fn set(
        &self,
        object: &JsObject,
        key: &PropertyKey,
        value: &Value,
        _receiver: &Value,
    ) -> Option<Result<bool, JsError>> {
        let callback = self.definition().set_property?;
        let ctx = self.callbacks_ctx()?;
        let object_ref = refs::object_id_ref(object.id());
        let name_ref = string_ref(&jsstring_of_key(key));
        let value_ref = refs::value_to_ref(*value);
        let mut exception: JSValueRef = std::ptr::null_mut();
        let result = unsafe {
            callback(
                ctx as *const _ as JSContextRef,
                object_ref,
                name_ref,
                value_ref,
                &mut exception,
            )
        };
        release_string_ref(name_ref);
        refs::release_value_ref(value_ref);
        if !exception.is_null() {
            return Some(Err(error_from_exception(exception)));
        }
        Some(Ok(result))
    }

    fn delete(&self, object: &JsObject, key: &PropertyKey) -> Option<Result<bool, JsError>> {
        let callback = self.definition().delete_property?;
        let ctx = self.callbacks_ctx()?;
        let object_ref = refs::object_id_ref(object.id());
        let name_ref = string_ref(&jsstring_of_key(key));
        let mut exception: JSValueRef = std::ptr::null_mut();
        let result = unsafe {
            callback(
                ctx as *const _ as JSContextRef,
                object_ref,
                name_ref,
                &mut exception,
            )
        };
        release_string_ref(name_ref);
        if !exception.is_null() {
            return Some(Err(error_from_exception(exception)));
        }
        Some(Ok(result))
    }

    fn own_property_keys(&self, object: &JsObject) -> Option<Result<Vec<PropertyKey>, JsError>> {
        let callback = self.definition().get_property_names?;
        let ctx = self.callbacks_ctx()?;
        let mut names: Vec<JsString> = Vec::new();
        let accumulator = &mut names as *mut Vec<JsString> as JSPropertyNameAccumulatorRef;
        let object_ref = refs::object_id_ref(object.id());
        unsafe { callback(ctx as *const _ as JSContextRef, object_ref, accumulator) };
        Some(Ok(names
            .into_iter()
            .map(|name| PropertyKey::from_js_string(&name))
            .collect()))
    }

    fn call(
        &self,
        object: &JsObject,
        this: &Value,
        args: &[Value],
    ) -> Option<Result<Value, JsError>> {
        let callback = self.definition().call_as_function?;
        let ctx = self.callbacks_ctx()?;
        let arg_refs: Vec<JSValueRef> = args
            .iter()
            .map(|arg| refs::value_to_ref(*arg))
            .collect();
        let this_ref = refs::value_object_ref(this);
        let object_ref = refs::object_id_ref(object.id());
        let mut exception: JSValueRef = std::ptr::null_mut();
        let result = unsafe {
            callback(
                ctx as *const _ as JSContextRef,
                object_ref,
                this_ref,
                arg_refs.len(),
                arg_refs.as_ptr(),
                &mut exception,
            )
        };
        for arg_ref in &arg_refs {
            refs::release_value_ref(*arg_ref);
        }
        if !exception.is_null() {
            return Some(Err(error_from_exception(exception)));
        }
        Some(Ok(refs::ref_to_value(result).unwrap_or(Value::Undefined)))
    }

    fn construct(
        &self,
        object: &JsObject,
        args: &[Value],
        new_target: &Value,
    ) -> Option<Result<Value, JsError>> {
        let callback = self.definition().call_as_constructor?;
        let ctx = self.callbacks_ctx()?;
        // The instance `new` builds: a host object of this class whose
        // prototype is the constructor's `.prototype`.
        let prototype = prototype_of(new_target);
        let instance = JsObject::host_object_create(self.ops_rc(), prototype);
        set_private(instance.id(), 0);
        let arg_refs: Vec<JSValueRef> = args
            .iter()
            .map(|arg| refs::value_to_ref(*arg))
            .collect();
        let mut exception: JSValueRef = std::ptr::null_mut();
        let result = unsafe {
            callback(
                ctx as *const _ as JSContextRef,
                refs::object_id_ref(object.id()),
                arg_refs.len(),
                arg_refs.as_ptr(),
                &mut exception,
            )
        };
        for arg_ref in &arg_refs {
            refs::release_value_ref(*arg_ref);
        }
        if !exception.is_null() {
            return Some(Err(error_from_exception(exception)));
        }
        let value = if result.is_null() {
            Value::Object(instance)
        } else {
            refs::ref_to_value(result).unwrap_or(Value::Object(instance))
        };
        Some(Ok(value))
    }

    fn is_callable(&self) -> bool {
        self.definition().call_as_function.is_some()
    }

    fn is_constructible(&self) -> bool {
        self.definition().call_as_constructor.is_some()
    }
}

/// The `.prototype` property of the `newTarget` — the instance's prototype.
fn prototype_of(new_target: &Value) -> Option<Handle<JsObject>> {
    let function = new_target.as_function()?;
    let value = function.get(&JsString::from_utf8("prototype")).ok()?;
    value.as_object()
}

/// Create a class from a C `JSClassDefinition` (copied by value; the host
/// keeps the pointed-to strings/arrays alive).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSClassCreate(definition: *const JSClassDefinition) -> JSClassRef {
    crate::guard(|| {
        if definition.is_null() {
            return std::ptr::null_mut();
        }
        let definition = unsafe { (*definition).clone() };
        let ops = Rc::new(ClassOps {
            entry: Cell::new(std::ptr::null()),
        });
        let mut entry = Box::new(ClassEntry {
            definition: Box::new(definition),
            ops: ops.clone(),
        });
        let entry_ptr = &mut *entry as *mut ClassEntry;
        ops.entry.set(entry_ptr);
        let raw = Box::into_raw(entry);
        CLASSES.with(|classes| {
            classes.borrow_mut().insert(raw as usize, raw);
        });
        raw as JSClassRef
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSClassRetain(js_class: JSClassRef) -> JSClassRef {
    crate::guard(|| {
        if class_entry(js_class).is_some() {
            js_class
        } else {
            std::ptr::null_mut()
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSClassRelease(js_class: JSClassRef) {
    crate::guard(|| {
        // Process-lifetime: deregister but deliberately leak, so live
        // instances keep dispatching their callbacks.
        CLASSES.with(|classes| {
            classes.borrow_mut().remove(&(js_class as usize));
        });
    })
}

/// The entry behind a class ref, if live.
fn class_entry(js_class: JSClassRef) -> Option<&'static ClassEntry> {
    if js_class.is_null() {
        return None;
    }
    CLASSES.with(|classes| {
        classes
            .borrow()
            .get(&(js_class as usize))
            .map(|entry| unsafe { &**entry })
    })
}

/// The shared ops handle of a class (for `JSValueIsObjectOfClass`).
pub(crate) fn class_ops(js_class: JSClassRef) -> Rc<dyn HostOps> {
    match class_entry(js_class) {
        Some(entry) => entry.ops.clone(),
        None => Rc::new(EmptyOps),
    }
}

#[derive(Debug)]
struct EmptyOps;
impl HostOps for EmptyOps {}

/// Create an instance of a class: a host object with the class's callbacks,
/// its static values/functions applied, and a private-data slot.
pub(crate) fn make_object(
    ctx: &JscContext,
    js_class: JSClassRef,
    data: *mut std::ffi::c_void,
) -> Handle<JsObject> {
    let object = match class_entry(js_class) {
        Some(entry) => {
            let ops: Rc<dyn HostOps> = entry.ops.clone();
            let object = JsObject::host_object_create(ops, object_prototype(ctx));
            apply_static_members(ctx, &object, &entry.definition);
            object
        }
        None => JsObject::ordinary_object_create(object_prototype(ctx)),
    };
    if !js_class.is_null() {
        set_private(object.id(), data as usize);
        if let Some(initialize) =
            class_entry(js_class).and_then(|entry| entry.definition.initialize)
        {
            let ctx_ptr = ctx as *const JscContext as *mut JscContext;
            let object_ref = refs::object_ref(&object);
            unsafe { initialize(ctx_ptr as JSContextRef, object_ref) };
        }
    }
    object
}

fn object_prototype(ctx: &JscContext) -> Option<Handle<JsObject>> {
    ctx.api
        .intrinsic("%Object.prototype%")
        .and_then(|value| runtime::context::as_object(&value))
}

/// Apply a class's static values/functions to a fresh instance (v1:
/// attributes other than DontEnum/ReadOnly are ignored).
fn apply_static_members(
    ctx: &JscContext,
    object: &Handle<JsObject>,
    definition: &JSClassDefinition,
) {
    if !definition.static_values.is_null() {
        let mut index = 0;
        while let Some(entry) = static_value_at(definition.static_values, index) {
            apply_static_value(ctx, object, entry);
            index += 1;
        }
    }
    if !definition.static_functions.is_null() {
        let mut index = 0;
        while let Some(entry) = static_function_at(definition.static_functions, index) {
            apply_static_function(ctx, object, entry);
            index += 1;
        }
    }
}

fn static_value_at(values: *const JSStaticValue, index: usize) -> Option<&'static JSStaticValue> {
    let entry = unsafe { &*values.add(index) };
    if entry.name.is_null() {
        None
    } else {
        Some(entry)
    }
}

fn static_function_at(
    functions: *const JSStaticFunction,
    index: usize,
) -> Option<&'static JSStaticFunction> {
    let entry = unsafe { &*functions.add(index) };
    if entry.name.is_null() {
        None
    } else {
        Some(entry)
    }
}

fn static_name(entry: &JSStaticValue) -> String {
    unsafe { CStr::from_ptr(entry.name) }
        .to_string_lossy()
        .into_owned()
}

fn static_function_name(entry: &JSStaticFunction) -> String {
    unsafe { CStr::from_ptr(entry.name) }
        .to_string_lossy()
        .into_owned()
}

/// A static value becomes an accessor property whose getter/setter dispatch
/// to the class's static callbacks.
fn apply_static_value(ctx: &JscContext, object: &Handle<JsObject>, entry: &JSStaticValue) {
    let name = static_name(entry);
    let ctx_ptr = ctx as *const JscContext as *mut JscContext;
    let get = entry.get_property.map(|callback| {
        let name = name.clone();
        make_static_value_function(ctx, format!("get {name}"), move |this, _args| {
            let ctx = current_context(ctx_ptr)?;
            invoke_get_callback(ctx, callback, this, &name)
        })
    });
    let set = entry.set_property.map(|callback| {
        let name = name.clone();
        make_static_value_function(ctx, format!("set {name}"), move |this, args| {
            let ctx = current_context(ctx_ptr)?;
            invoke_set_callback(ctx, callback, this, &name, args)
        })
    });
    let _ = object.define_property_or_throw(
        &JsString::from_utf8(&name),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get,
            set,
            enumerable: Some(true),
            configurable: Some(true),
        },
    );
}

/// Resolve the context a static-member closure was created in.
fn current_context(ctx_ptr: *mut JscContext) -> Result<&'static JscContext, JsError> {
    JscContext::get(ctx_ptr).ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "context released during callback".into(),
        )
    })
}

/// A static function becomes a data property whose value is a function
/// dispatching to the callback.
fn apply_static_function(ctx: &JscContext, object: &Handle<JsObject>, entry: &JSStaticFunction) {
    let name = static_function_name(entry);
    let Some(callback) = entry.call_as_function else {
        return;
    };
    let function = make_function_with_callback(ctx, Some(name.clone()), callback);
    let _ = object.define_property_or_throw(
        &JsString::from_utf8(&name),
        &PropertyDescriptor::data(Value::Function(function)),
    );
}

fn make_static_value_function(
    ctx: &JscContext,
    name: String,
    body: impl Fn(&Value, &[Value]) -> Result<Value, JsError> + 'static,
) -> Value {
    let function = Function::create_builtin(
        Some(JsString::from_utf8(&name)),
        0,
        Box::new(body),
        None,
        function_prototype(ctx),
    );
    match function {
        Ok(function) => Value::Function(function),
        Err(_) => Value::Undefined,
    }
}

fn invoke_get_callback(
    ctx: &JscContext,
    callback: JSObjectGetPropertyCallback,
    this: &Value,
    name: &str,
) -> Result<Value, JsError> {
    let this_ref = refs::value_object_ref(this);
    let name_ref = string_ref(&JsString::from_utf8(name));
    let mut exception: JSValueRef = std::ptr::null_mut();
    let result = unsafe {
        callback(
            ctx as *const JscContext as *const _ as JSContextRef,
            this_ref,
            name_ref,
            &mut exception,
        )
    };
    release_string_ref(name_ref);
    if !exception.is_null() {
        return Err(error_from_exception(exception));
    }
    Ok(refs::ref_to_value(result).unwrap_or(Value::Undefined))
}

fn invoke_set_callback(
    ctx: &JscContext,
    callback: JSObjectSetPropertyCallback,
    this: &Value,
    name: &str,
    args: &[Value],
) -> Result<Value, JsError> {
    let this_ref = refs::value_object_ref(this);
    let name_ref = string_ref(&JsString::from_utf8(name));
    let value_ref = refs::value_to_ref(args.first().cloned().unwrap_or(Value::Undefined));
    let mut exception: JSValueRef = std::ptr::null_mut();
    let result = unsafe {
        callback(
            ctx as *const JscContext as *const _ as JSContextRef,
            this_ref,
            name_ref,
            value_ref,
            &mut exception,
        )
    };
    release_string_ref(name_ref);
    refs::release_value_ref(value_ref);
    if !exception.is_null() {
        return Err(error_from_exception(exception));
    }
    let _ = result;
    Ok(Value::Undefined)
}

/// A plain host function dispatching to a `callAsFunction`-style callback
/// (JSObjectMakeFunctionWithCallback and static functions).
pub(crate) fn make_function_with_callback(
    ctx: &JscContext,
    name: Option<String>,
    callback: JSObjectCallAsFunctionCallback,
) -> Handle<Function> {
    let ctx_ptr = ctx as *const JscContext as *mut JscContext;
    let call: crux::function::NativeFn = Box::new(move |this, args| {
        let arg_refs: Vec<JSValueRef> = args
            .iter()
            .map(|arg| refs::value_to_ref(*arg))
            .collect();
        let this_ref = refs::value_object_ref(this);
        let mut exception: JSValueRef = std::ptr::null_mut();
        let result = unsafe {
            callback(
                ctx_ptr as JSContextRef,
                std::ptr::null_mut(),
                this_ref,
                arg_refs.len(),
                arg_refs.as_ptr(),
                &mut exception,
            )
        };
        for arg_ref in &arg_refs {
            refs::release_value_ref(*arg_ref);
        }
        if !exception.is_null() {
            return Err(error_from_exception(exception));
        }
        Ok(refs::ref_to_value(result).unwrap_or(Value::Undefined))
    });
    Function::create_builtin(
        name.map(|name| JsString::from_utf8(&name)),
        0,
        call,
        None,
        function_prototype(ctx),
    )
    .unwrap_or_else(|_| Function::new(None))
}

fn function_prototype(ctx: &JscContext) -> Option<Handle<JsObject>> {
    ctx.api
        .intrinsic("%Function.prototype%")
        .and_then(|value| runtime::context::as_object(&value))
}
