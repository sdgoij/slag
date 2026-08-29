//! Contexts (`JSGlobalContextRef`), evaluation, and exceptions.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

use crux::error::JsError;
use crux::string::JsString;
use crux::value::Value;
use runtime::api::{self, Isolate};

use crate::refs;
use crate::{
    JSClassRef, JSContextGroupRef, JSContextRef, JSGlobalContextRef, JSObjectRef, JSStringRef,
    JSValueRef, OpaqueJSContextGroup,
};

/// The per-context engine state: the isolate (boxed, so its address is
/// stable) and the V8-shaped realm context.
pub struct JscContext {
    pub isolate: Box<Isolate>,
    pub api: api::Context,
    /// Opaque user data passed to the context via the `data` parameter of
    /// `JSGlobalContextCreate`. Used by `JSCallbackData` callbacks.
    #[allow(dead_code)]
    pub data: *mut std::ffi::c_void,
}

impl JscContext {
    /// Create a new context and store it in the LIVE map. Returns the raw
    /// pointer (for use as JSGlobalContextRef).
    pub fn create_leaked() -> Result<*mut JscContext, JsError> {
        let mut isolate = Isolate::new();
        let api = api::Context::new(&mut isolate)?;
        let mut ctx = Box::new(JscContext {
            isolate,
            api,
            data: std::ptr::null_mut(),
        });
        let ctx_ptr = &mut *ctx as *mut JscContext;
        LIVE.with(|live| {
            live.borrow_mut().insert(ctx_ptr as usize, ctx);
        });
        Ok(ctx_ptr)
    }
}

thread_local! {
    /// Live contexts, keyed by their (stable) heap address. Freed on
    /// JSGlobalContextRelease.
    static LIVE: RefCell<HashMap<usize, Box<JscContext>>> = RefCell::new(HashMap::new());
    /// The context of the innermost JSC API call on this thread; class
    /// callbacks (HostOps) read it to reach their context.
    static CURRENT: Cell<*mut JscContext> = const { Cell::new(std::ptr::null_mut()) };
}

impl JscContext {
    /// Create a fresh context: a new isolate with a host-defined realm.
    pub fn create() -> Result<*mut JscContext, JsError> {
        let mut isolate = Isolate::new();
        let api = api::Context::new(&mut isolate)?;
        let mut ctx = Box::new(JscContext {
            isolate,
            api,
            data: std::ptr::null_mut(),
        });
        let ptr: *mut JscContext = &mut *ctx;
        LIVE.with(|live| live.borrow_mut().insert(ptr as usize, ctx));
        Ok(ptr)
    }

    /// Resolve a context pointer; `None` once released. The map entry keeps
    /// the allocation alive, so the returned reference is valid while the
    /// caller holds the context (the usual raw-pointer borrow contract).
    pub fn get(ctx: *mut JscContext) -> Option<&'static JscContext> {
        let live = LIVE.with(|live| live.borrow().contains_key(&(ctx as usize)));
        if live { Some(unsafe { &*ctx }) } else { None }
    }

    /// Drop a context, tearing down its isolate.
    pub fn release(ctx: *mut JscContext) {
        LIVE.with(|live| {
            live.borrow_mut().remove(&(ctx as usize));
        });
    }

    /// The isolate pointer (the boxed allocation, stable).
    pub fn isolate(&self) -> *mut Isolate {
        &*self.isolate as *const Isolate as *mut Isolate
    }

    /// Run `body` with this context current, so class callbacks resolve it.
    pub fn with_current<T>(&self, body: impl FnOnce() -> T) -> T {
        let ptr = self as *const JscContext as *mut JscContext;
        let previous = CURRENT.with(|slot| slot.replace(ptr));
        let result = body();
        CURRENT.with(|slot| slot.replace(previous));
        result
    }

    /// The current context on this thread, if inside a JSC API call.
    pub fn current() -> *mut JscContext {
        CURRENT.with(|slot| slot.get())
    }

    /// Evaluate a Script in the global scope.
    pub fn eval(&self, source: &str) -> Result<Value, JsError> {
        self.api.try_eval(source).map(api::Local::into_value)
    }

    /// Convert an engine error into a thrown value (a real Error object)
    /// and set it as the pending exception, filling the C out-param.
    pub fn throw(&self, error: &JsError, out: Option<&mut JSValueRef>) {
        let isolate = self.isolate();
        let agent = unsafe { &mut *(&*isolate).agent_ptr() };
        let value = runtime::builtins::error::to_throwable(agent, error).unwrap_or_else(|_| {
            Value::String(crux::handle::Handle::new(JsString::from_utf8(&format!(
                "{}: {}",
                kind_name(error.kind),
                error.message
            ))))
        });
        unsafe { &*isolate }.set_pending_exception(value);
        if let Some(slot) = out {
            *slot = refs::value_to_ref(value);
        }
    }
}

fn kind_name(kind: crux::ErrorKind) -> &'static str {
    match kind {
        crux::ErrorKind::EvalError => "EvalError",
        crux::ErrorKind::RangeError => "RangeError",
        crux::ErrorKind::ReferenceError => "ReferenceError",
        crux::ErrorKind::SyntaxError => "SyntaxError",
        crux::ErrorKind::TypeError => "TypeError",
        crux::ErrorKind::UriError => "URIError",
    }
}

/// Convert a JS error into an engine error carrying the thrown value.
pub fn error_from_exception(exception: JSValueRef) -> JsError {
    let value = refs::ref_to_value(exception).unwrap_or(Value::Undefined);
    JsError::new(
        crux::ErrorKind::TypeError,
        "exception thrown by host callback".into(),
    )
    .with_value(value)
}

/// A `JSValueRef` exception out-param as an id slot.
pub fn exception_slot(out: *mut JSValueRef) -> Option<&'static mut JSValueRef> {
    if out.is_null() {
        None
    } else {
        Some(unsafe { &mut *out })
    }
}

/// Fill an exception out-param from an engine error.
pub fn fill_exception(ctx: &JscContext, error: &JsError, out: *mut JSValueRef) {
    ctx.throw(error, exception_slot(out));
}

/// A retained `JSStringRef` from a JS string id; `None` when not a string.
pub fn string_from_ref(r: JSStringRef) -> Option<JsString> {
    if r.is_null() {
        return None;
    }
    ffi::string(r as usize as u64)
}

/// Retain a string and hand out its ref.
pub fn string_ref(string: &JsString) -> JSStringRef {
    ffi::retain_string(string.clone()) as usize as JSStringRef
}

/// Release a retained string ref.
pub fn release_string_ref(r: JSStringRef) {
    if !r.is_null() {
        ffi::release_string(r as usize as u64);
    }
}

/// Create a context group handle (v1: groups do not share a heap; the ref
/// is a token held for as long as the group is live).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSContextGroupCreate() -> JSContextGroupRef {
    crate::guard(|| {
        let group = Box::into_raw(Box::new(OpaqueJSContextGroup { _private: [0; 0] }));
        GROUPS.with(|groups| groups.borrow_mut().insert(group as usize));
        group
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSContextGroupRetain(group: JSContextGroupRef) -> JSContextGroupRef {
    crate::guard(|| {
        if GROUPS.with(|groups| groups.borrow().contains(&(group as usize))) {
            group
        } else {
            std::ptr::null_mut()
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSContextGroupRelease(group: JSContextGroupRef) {
    crate::guard(|| {
        if GROUPS.with(|groups| groups.borrow_mut().remove(&(group as usize))) {
            drop(unsafe { Box::from_raw(group) });
        }
    })
}

thread_local! {
    static GROUPS: RefCell<HashSet<usize>> = RefCell::new(HashSet::new());
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSGlobalContextCreate(
    _global_object_class: JSClassRef,
) -> JSGlobalContextRef {
    // v1: the global object class is accepted but its static members are not
    // applied to the realm's global object.
    crate::guard(|| match JscContext::create_leaked() {
        Ok(ctx) => ctx as JSGlobalContextRef,
        Err(_) => std::ptr::null_mut(),
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSGlobalContextCreateInGroup(
    group: JSContextGroupRef,
    _global_object_class: JSClassRef,
) -> JSGlobalContextRef {
    crate::guard(|| {
        // v1: groups are accepted but do not share a heap.
        if !group.is_null() && !GROUPS.with(|groups| groups.borrow().contains(&(group as usize))) {
            return std::ptr::null_mut();
        }
        match JscContext::create() {
            Ok(ctx) => ctx as JSGlobalContextRef,
            Err(_) => std::ptr::null_mut(),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSGlobalContextRetain(ctx: JSGlobalContextRef) -> JSGlobalContextRef {
    crate::guard(|| {
        if JscContext::get(ctx as *mut JscContext).is_some() {
            ctx
        } else {
            std::ptr::null_mut()
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSGlobalContextRelease(ctx: JSGlobalContextRef) {
    crate::guard(|| JscContext::release(ctx as *mut JscContext));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSContextGetGlobalObject(ctx: JSContextRef) -> JSObjectRef {
    // In real JSC, JSGlobalContextRef IS the global object.
    // Return the context pointer as the global object.
    if !ctx.is_null() && JscContext::get(ctx as *mut JscContext).is_some() {
        ctx as JSObjectRef
    } else {
        std::ptr::null_mut()
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSContextGetGroup(ctx: JSContextRef) -> JSContextGroupRef {
    crate::guard(|| {
        let _ = ctx;
        std::ptr::null_mut()
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSEvaluateScript(
    ctx: JSContextRef,
    script: JSStringRef,
    _this_object: JSObjectRef,
    _source_url: JSStringRef,
    _starting_line_number: i32,
    exception: *mut JSValueRef,
) -> JSValueRef {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return std::ptr::null_mut();
        };
        let Some(source) = string_from_ref(script) else {
            return std::ptr::null_mut();
        };
        ctx.with_current(|| match ctx.eval(&source.to_string_lossy()) {
            Ok(value) => refs::value_to_ref(value),
            Err(error) => {
                fill_exception(ctx, &error, exception);
                std::ptr::null_mut()
            }
        })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSCheckScriptSyntax(
    ctx: JSContextRef,
    script: JSStringRef,
    _source_url: JSStringRef,
    _starting_line_number: i32,
    exception: *mut JSValueRef,
) -> bool {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return false;
        };
        let Some(source) = string_from_ref(script) else {
            return false;
        };
        ctx.with_current(
            || match api::Script::compile(&ctx.api, &source.to_string_lossy()) {
                Ok(_) => true,
                Err(error) => {
                    fill_exception(ctx, &error, exception);
                    false
                }
            },
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSContextGetException(ctx: JSContextRef) -> JSValueRef {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return std::ptr::null_mut();
        };
        unsafe { &*ctx.isolate() }
            .pending_exception()
            .map(refs::value_to_ref)
            .unwrap_or(std::ptr::null_mut())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSContextSetException(ctx: JSContextRef, value: JSValueRef) {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return;
        };
        let value = refs::ref_to_value(value).unwrap_or(Value::Undefined);
        unsafe { &*ctx.isolate() }.set_pending_exception(value);
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSContextCreateBacktrace(
    ctx: JSContextRef,
    _max_stack_size: u32,
) -> JSStringRef {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return std::ptr::null_mut();
        };
        let text = unsafe { &*ctx.isolate() }
            .pending_exception()
            .map(|value| value.to_string())
            .unwrap_or_default();
        string_ref(&JsString::from_utf8(&text))
    })
}
