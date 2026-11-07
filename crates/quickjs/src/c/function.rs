//! C functions and function lists (`JS_NewCFunction2`, `JS_SetPropertyFunctionList`).
//!
//! A C function is a host function the engine calls through a `FunctionTemplate`;
//! the callback forwards to the host's `JSCFunction` with the same
//! `(ctx, this_val, argc, argv)` shape QuickJS uses. A function list is a
//! `JSCFunctionListEntry` table the facade walks, turning `JS_DEF_CFUNC` rows
//! into functions, the `JS_DEF_PROP_*` rows into constants, the
//! `JS_DEF_CGETSET` rows into accessor properties, the `JS_DEF_OBJECT` rows
//! into nested objects, and the `JS_DEF_ALIAS` rows into property aliases.

use std::ffi::{CStr, c_char};

use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::property::PropertyDescriptor;
use crux::string::JsString;
use crux::value::Value as CruxValue;
use runtime::api::{self, Local};

use crate::c::abi;
use crate::c::runtime::{JSContext, context_api};

/// The C `JSCFunction` (quickjs.h): `(ctx, this_val, argc, argv) -> JSValue`.
pub type JSCFunction =
    unsafe extern "C" fn(*mut JSContext, abi::JSValue, i32, *const abi::JSValue) -> abi::JSValue;

/// The C accessor functions (quickjs.h): a getter takes `this`, a setter takes
/// `this` and the assigned value.
pub type JSCGetter = unsafe extern "C" fn(*mut JSContext, abi::JSValue) -> abi::JSValue;
pub type JSCSetter =
    unsafe extern "C" fn(*mut JSContext, abi::JSValue, abi::JSValue) -> abi::JSValue;
pub type JSCGetterMagic = unsafe extern "C" fn(*mut JSContext, abi::JSValue, i32) -> abi::JSValue;
pub type JSCSetterMagic =
    unsafe extern "C" fn(*mut JSContext, abi::JSValue, abi::JSValue, i32) -> abi::JSValue;

// `JSCFunctionEnum` values (quickjs.h); only `generic` is dispatched today.
const JS_CFUNC_GENERIC: u8 = 0;

// `JS_DEF_*` row kinds (quickjs.h).
const JS_DEF_CFUNC: u8 = 0;
const JS_DEF_CGETSET: u8 = 1;
const JS_DEF_CGETSET_MAGIC: u8 = 2;
const JS_DEF_PROP_STRING: u8 = 3;
const JS_DEF_PROP_INT32: u8 = 4;
const JS_DEF_PROP_INT64: u8 = 5;
const JS_DEF_PROP_DOUBLE: u8 = 6;
const JS_DEF_PROP_UNDEFINED: u8 = 7;
const JS_DEF_OBJECT: u8 = 8;
const JS_DEF_ALIAS: u8 = 9;
const JS_DEF_PROP_BOOL: u8 = 11;

// Property attribute bits (quickjs.h).
const JS_PROP_CONFIGURABLE: u8 = 1;
const JS_PROP_ENUMERABLE: u8 = 4;

/// The C `JSCFunctionType` union (quickjs.h). One pointer wide; the accessor
/// members overlap `generic` in the same storage.
#[repr(C)]
#[derive(Clone, Copy)]
pub union JSCFunctionType {
    generic: Option<JSCFunction>,
    getter: Option<JSCGetter>,
    setter: Option<JSCSetter>,
    getter_magic: Option<JSCGetterMagic>,
    setter_magic: Option<JSCSetterMagic>,
}

/// The `func` arm of the `JSCFunctionListEntry` union (quickjs.h).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct JSCFunctionListEntryFunc {
    length: u8,
    cproto: u8,
    cfunc: JSCFunctionType,
}

/// The `getset` arm (quickjs.h): an accessor pair.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct JSCFunctionListEntryGetSet {
    get: JSCFunctionType,
    set: JSCFunctionType,
}

/// The `alias` arm (quickjs.h): `base < 0` names a property on the same object;
/// `base >= 0` names the entry at that index in the table.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct JSCFunctionListEntryAlias {
    name: *const c_char,
    base: i32,
}

/// The `prop_list` arm (quickjs.h): a nested function list.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct JSCFunctionListEntryPropList {
    tab: *const JSCFunctionListEntry,
    len: i32,
}

/// The `JSCFunctionListEntry` union (quickjs.h). The `func` arm is 16 bytes,
/// which is the union's width; the other arms overlap the same storage.
#[repr(C)]
#[derive(Clone, Copy)]
pub union JSCFunctionListEntryU {
    func: JSCFunctionListEntryFunc,
    getset: JSCFunctionListEntryGetSet,
    alias: JSCFunctionListEntryAlias,
    prop_list: JSCFunctionListEntryPropList,
    str: *const c_char,
    i32_: i32,
    i64_: i64,
    u64_: u64,
    f64_: f64,
}

/// The C `JSCFunctionListEntry` (quickjs.h).
#[repr(C)]
pub struct JSCFunctionListEntry {
    name: *const c_char,
    prop_flags: u8,
    def_type: u8,
    magic: i16,
    u: JSCFunctionListEntryU,
}

fn no_context() -> JsError {
    JsError::new(ErrorKind::Error, "no context".into())
}

/// Build a host function over a callback with the engine's default length and
/// name.
fn make_host_function(
    ctx: *mut JSContext,
    callback: api::FunctionCallback,
) -> Result<Local, JsError> {
    let Some(api_ctx) = context_api(ctx) else {
        return Err(no_context());
    };
    let isolate = unsafe { &mut *api_ctx.isolate() };
    let template = api::FunctionTemplate::new(isolate, callback);
    template.get_function(&api_ctx)
}

/// Build a host function over `func` and set its `length` (and `name`, when
/// given).
fn make_cfunction(
    ctx: *mut JSContext,
    func: JSCFunction,
    name: Option<&str>,
    length: i32,
) -> Result<Local, JsError> {
    let Some(api_ctx) = context_api(ctx) else {
        return Err(no_context());
    };
    let isolate = unsafe { &mut *api_ctx.isolate() };
    let template = api::FunctionTemplate::new(
        isolate,
        Box::new(move |info: &api::FunctionCallbackInfo| {
            dispatch_cfunc(ctx, func, info);
        }),
    );
    template.set_length(length);
    let function = template.get_function(&api_ctx)?;
    if let Some(name) = name {
        let name_local =
            Local::from_value(CruxValue::String(Handle::new(JsString::from_utf8(name))));
        api::Object::set(&api_ctx, &function, "name", &name_local, false)?;
    }
    Ok(function)
}

fn make_getter(ctx: *mut JSContext, getter: JSCGetter) -> Result<Local, JsError> {
    make_host_function(
        ctx,
        Box::new(move |info| {
            let this_val = abi::value_to_jsvalue(info.this().into_value());
            let result = unsafe { getter(ctx, this_val) };
            abi::free_jsvalue(this_val);
            if let Some(value) = abi::jsvalue_to_value(result) {
                info.get_return_value().set(Local::from_value(value));
            }
            abi::free_jsvalue(result);
        }),
    )
}

fn make_setter(ctx: *mut JSContext, setter: JSCSetter) -> Result<Local, JsError> {
    make_host_function(
        ctx,
        Box::new(move |info| {
            let this_val = abi::value_to_jsvalue(info.this().into_value());
            let arg = info
                .args()
                .next()
                .map(|arg| abi::value_to_jsvalue(arg.into_value()))
                .unwrap_or_else(abi::undefined);
            let result = unsafe { setter(ctx, this_val, arg) };
            abi::free_jsvalue(this_val);
            abi::free_jsvalue(arg);
            if let Some(value) = abi::jsvalue_to_value(result) {
                info.get_return_value().set(Local::from_value(value));
            }
            abi::free_jsvalue(result);
        }),
    )
}

fn make_getter_magic(
    ctx: *mut JSContext,
    getter: JSCGetterMagic,
    magic: i32,
) -> Result<Local, JsError> {
    make_host_function(
        ctx,
        Box::new(move |info| {
            let this_val = abi::value_to_jsvalue(info.this().into_value());
            let result = unsafe { getter(ctx, this_val, magic) };
            abi::free_jsvalue(this_val);
            if let Some(value) = abi::jsvalue_to_value(result) {
                info.get_return_value().set(Local::from_value(value));
            }
            abi::free_jsvalue(result);
        }),
    )
}

fn make_setter_magic(
    ctx: *mut JSContext,
    setter: JSCSetterMagic,
    magic: i32,
) -> Result<Local, JsError> {
    make_host_function(
        ctx,
        Box::new(move |info| {
            let this_val = abi::value_to_jsvalue(info.this().into_value());
            let arg = info
                .args()
                .next()
                .map(|arg| abi::value_to_jsvalue(arg.into_value()))
                .unwrap_or_else(abi::undefined);
            let result = unsafe { setter(ctx, this_val, arg, magic) };
            abi::free_jsvalue(this_val);
            abi::free_jsvalue(arg);
            if let Some(value) = abi::jsvalue_to_value(result) {
                info.get_return_value().set(Local::from_value(value));
            }
            abi::free_jsvalue(result);
        }),
    )
}

/// Forward one engine call to the host's `JSCFunction`.
fn dispatch_cfunc(ctx: *mut JSContext, func: JSCFunction, info: &api::FunctionCallbackInfo<'_>) {
    let this_val = abi::value_to_jsvalue(info.this().into_value());
    let args: Vec<abi::JSValue> = info
        .args()
        .map(|arg| abi::value_to_jsvalue(arg.into_value()))
        .collect();
    // SAFETY: `ctx` names a live context; `this_val` and `args` are valid for
    // the callback and freed after it returns.
    let result = unsafe { func(ctx, this_val, args.len() as i32, args.as_ptr()) };
    abi::free_jsvalue(this_val);
    for arg in args {
        abi::free_jsvalue(arg);
    }
    let converted = abi::jsvalue_to_value(result);
    abi::free_jsvalue(result);
    if let Some(value) = converted {
        info.get_return_value().set(Local::from_value(value));
    }
    // A pending exception (the callback returned the exception sentinel) is
    // left in the context and propagates out of the call.
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewCFunction2(
    ctx: *mut JSContext,
    func: Option<JSCFunction>,
    name: *const c_char,
    length: i32,
    cproto: i32,
    _magic: i32,
) -> abi::JSValue {
    if cproto != 0 {
        // Only the generic prototype is dispatched; constructor/magic/etc.
        // have a different `func` signature this facade does not call.
        return abi::exception();
    }
    let Some(func) = func else {
        return abi::exception();
    };
    let name = if name.is_null() {
        None
    } else {
        // SAFETY: the header requires a NUL-terminated name.
        match unsafe { CStr::from_ptr(name) }.to_str() {
            Ok(name) => Some(name),
            Err(_) => return abi::exception(),
        }
    };
    match make_cfunction(ctx, func, name, length) {
        Ok(function) => abi::value_to_jsvalue(function.into_value()),
        Err(_) => abi::exception(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_SetPropertyFunctionList(
    ctx: *mut JSContext,
    obj: abi::JSValue,
    tab: *const JSCFunctionListEntry,
    len: i32,
) -> i32 {
    let Some(api_ctx) = context_api(ctx) else {
        return -1;
    };
    let Some(object) = abi::jsvalue_to_value(obj) else {
        return -1;
    };
    let object = Local::from_value(object);
    if tab.is_null() || len <= 0 {
        return 0;
    }
    // SAFETY: the header requires `len` readable entries from `tab`.
    let entries = unsafe { std::slice::from_raw_parts(tab, len as usize) };
    for entry in entries {
        if set_entry(&api_ctx, ctx, &object, entries, entry).is_err() {
            return -1;
        }
    }
    0
}

fn set_entry(
    api_ctx: &api::Context,
    ctx: *mut JSContext,
    object: &Local,
    tab: &[JSCFunctionListEntry],
    entry: &JSCFunctionListEntry,
) -> Result<(), JsError> {
    if entry.name.is_null() {
        return Err(JsError::new(
            ErrorKind::Error,
            "function-list entry has no name".into(),
        ));
    }
    // SAFETY: the header requires a NUL-terminated name.
    let name = unsafe { CStr::from_ptr(entry.name) }
        .to_str()
        .map_err(|_| {
            JsError::new(
                ErrorKind::Error,
                "function-list entry name is not UTF-8".into(),
            )
        })?;

    match entry.def_type {
        JS_DEF_CFUNC => {
            if unsafe { entry.u.func.cproto } != JS_CFUNC_GENERIC {
                return Err(JsError::new(
                    ErrorKind::Error,
                    "unsupported C function proto in function list".into(),
                ));
            }
            // SAFETY: `def_type == JS_DEF_CFUNC` means the union holds `func`.
            let func = unsafe { entry.u.func.cfunc.generic };
            let Some(func) = func else {
                return Err(JsError::new(
                    ErrorKind::Error,
                    "function-list entry has a null function".into(),
                ));
            };
            let function =
                make_cfunction(ctx, func, Some(name), unsafe { entry.u.func.length } as i32)?;
            store_value(api_ctx, object, name, function.into_value())
        }
        JS_DEF_CGETSET => {
            // SAFETY: `def_type == JS_DEF_CGETSET` means the union holds `getset`.
            let get = unsafe { entry.u.getset.get.getter };
            let set = unsafe { entry.u.getset.set.setter };
            set_accessor(ctx, object, name, get, set, entry.prop_flags)
        }
        JS_DEF_CGETSET_MAGIC => {
            let magic = i32::from(entry.magic);
            // SAFETY: the union holds `getset` for this row kind.
            let get = unsafe { entry.u.getset.get.getter_magic };
            let set = unsafe { entry.u.getset.set.setter_magic };
            set_accessor_magic(ctx, object, name, get, set, magic, entry.prop_flags)
        }
        JS_DEF_PROP_STRING => {
            // SAFETY: `def_type == JS_DEF_PROP_STRING` means the union holds `str`.
            let text = unsafe { CStr::from_ptr(entry.u.str) }
                .to_str()
                .map_err(|_| {
                    JsError::new(ErrorKind::Error, "function-list string is not UTF-8".into())
                })?;
            store_value(
                api_ctx,
                object,
                name,
                CruxValue::String(Handle::new(JsString::from_utf8(text))),
            )
        }
        JS_DEF_PROP_INT32 => {
            // SAFETY: the union holds `i32` for this row kind.
            store_value(
                api_ctx,
                object,
                name,
                CruxValue::Number(unsafe { entry.u.i32_ } as f64),
            )
        }
        JS_DEF_PROP_INT64 => store_value(
            api_ctx,
            object,
            name,
            CruxValue::Number(unsafe { entry.u.i64_ } as f64),
        ),
        JS_DEF_PROP_DOUBLE => store_value(
            api_ctx,
            object,
            name,
            CruxValue::Number(unsafe { entry.u.f64_ }),
        ),
        JS_DEF_PROP_UNDEFINED => store_value(api_ctx, object, name, CruxValue::Undefined),
        JS_DEF_PROP_BOOL => {
            // SAFETY: `def_type == JS_DEF_PROP_BOOL` means the union holds `i32`.
            let value = unsafe { entry.u.i32_ } != 0;
            store_value(api_ctx, object, name, CruxValue::Boolean(value))
        }
        JS_DEF_OBJECT => {
            // SAFETY: `def_type == JS_DEF_OBJECT` means the union holds `prop_list`.
            let nested_tab = unsafe { entry.u.prop_list.tab };
            let nested_len = unsafe { entry.u.prop_list.len };
            let nested = api::Object::new(api_ctx)?;
            if !nested_tab.is_null() && nested_len > 0 {
                // SAFETY: the header requires `nested_len` readable entries.
                let nested_entries =
                    unsafe { std::slice::from_raw_parts(nested_tab, nested_len as usize) };
                for nested_entry in nested_entries {
                    set_entry(api_ctx, ctx, &nested, nested_entries, nested_entry)?;
                }
            }
            store_value(api_ctx, object, name, nested.into_value())
        }
        JS_DEF_ALIAS => {
            // SAFETY: `def_type == JS_DEF_ALIAS` means the union holds `alias`.
            let alias_name = unsafe { entry.u.alias.name };
            let base = unsafe { entry.u.alias.base };
            let target = if base < 0 {
                alias_name
            } else {
                let Some(target) = tab.get(base as usize) else {
                    return Err(JsError::new(
                        ErrorKind::Error,
                        "function-list alias base is out of range".into(),
                    ));
                };
                target.name
            };
            if target.is_null() {
                return Err(JsError::new(
                    ErrorKind::Error,
                    "function-list alias target has no name".into(),
                ));
            }
            // SAFETY: the alias names a NUL-terminated string.
            let target_name = unsafe { CStr::from_ptr(target) }.to_str().map_err(|_| {
                JsError::new(
                    ErrorKind::Error,
                    "function-list alias name is not UTF-8".into(),
                )
            })?;
            let value = api::Object::get(api_ctx, object, target_name)?;
            store_value(api_ctx, object, name, value.into_value())
        }
        other => Err(JsError::new(
            ErrorKind::Error,
            format!("unsupported function-list def_type {other}"),
        )),
    }
}

/// Define an accessor property from a plain getter/setter pair.
fn set_accessor(
    ctx: *mut JSContext,
    object: &Local,
    name: &str,
    get: Option<JSCGetter>,
    set: Option<JSCSetter>,
    prop_flags: u8,
) -> Result<(), JsError> {
    let get = get.map(|g| make_getter(ctx, g)).transpose()?;
    let set = set.map(|s| make_setter(ctx, s)).transpose()?;
    define_accessor(object, name, get, set, prop_flags)
}

fn set_accessor_magic(
    ctx: *mut JSContext,
    object: &Local,
    name: &str,
    get: Option<JSCGetterMagic>,
    set: Option<JSCSetterMagic>,
    magic: i32,
    prop_flags: u8,
) -> Result<(), JsError> {
    let get = get.map(|g| make_getter_magic(ctx, g, magic)).transpose()?;
    let set = set.map(|s| make_setter_magic(ctx, s, magic)).transpose()?;
    define_accessor(object, name, get, set, prop_flags)
}

fn define_accessor(
    object: &Local,
    name: &str,
    get: Option<Local>,
    set: Option<Local>,
    prop_flags: u8,
) -> Result<(), JsError> {
    let object_handle = object.as_object().ok_or_else(|| {
        JsError::new(
            ErrorKind::Error,
            "function-list target is not an object".into(),
        )
    })?;
    object_handle.define_property_or_throw(
        &JsString::from_utf8(name),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: get.map(|g| g.into_value()),
            set: set.map(|s| s.into_value()),
            enumerable: Some(prop_flags & JS_PROP_ENUMERABLE != 0),
            configurable: Some(prop_flags & JS_PROP_CONFIGURABLE != 0),
        },
    )
}

/// Store `value` as an own property of `object`. The value is boxed first so
/// the box registry roots it while the store runs: a store may allocate (and
/// collect), and a bare engine value on the Rust stack is not a collector root.
fn store_value(
    api_ctx: &api::Context,
    object: &Local,
    name: &str,
    value: CruxValue,
) -> Result<(), JsError> {
    let boxed = abi::value_to_jsvalue(value);
    let decoded = abi::jsvalue_to_value(boxed).unwrap_or(CruxValue::Undefined);
    api::Object::set(api_ctx, object, name, &Local::from_value(decoded), false)?;
    abi::free_jsvalue(boxed);
    Ok(())
}
