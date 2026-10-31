//! A0's flat C ABI: the surface the C++ facade in `crates/v8/cpp/` calls.
//!
//! Values cross as the engine's tagged `u64` (`JsValue::to_bits`), strings as
//! UTF-8 byte spans, and errors through a per-context pending slot. The
//! functions are single-threaded, like the isolate they belong to.
//!
//! This is a scaffold for Route A's first milestone (`.notes/chromium-facade.md`
//! §11): enough to run V8's `hello-world.cc` and the sample `shell.cc`. It is
//! not the stable ABI the note describes; that one grows from here.

#![allow(non_camel_case_types)]
#![allow(clippy::missing_safety_doc)]
// Every entry point is an `unsafe extern "C"` function whose contract is the
// C caller's, so the 2024 lint's per-operation blocks add ceremony without a
// safety boundary. The documented contract is on each function.
#![allow(unsafe_op_in_unsafe_fn)]

use std::cell::RefCell;
use std::ffi::{c_char, c_void};
use std::rc::Rc;

use crux::heap::{GcAny, RootSource, Trace, register_root_source};
use crux::value::Value;
use slag::api::{Local as EngineLocal, Traced};
use slag::{CompiledScript, Context, ErrorKind, FunctionCall, HostFn, JsError, JsValue};

/// The isolate a host enters. The engine's contexts are the state; this token
/// exists so the facade's `Isolate` can own and free them in order.
pub struct SlagIsolate {
    _private: (),
}

/// One realm and its pending-exception slot (`v8::Context`).
///
/// The engine's `Context` is behind a `RefCell` so a re-entrant call from a host
/// callback fails with a pending error instead of aliasing it. The safe
/// re-entry is `FunctionCall::eval` ([`slag_call_eval`]); `load()` in the
/// sample shell does not use it, so it is refused rather than unsound.
pub struct SlagContext {
    context: RefCell<Context>,
    pending_error: Rc<RefCell<Option<String>>>,
}

/// A parsed, not-yet-run script (`v8::Script`).
pub struct SlagScript {
    script: CompiledScript,
}

/// A host-traced root (`v8::TracedReference<T>`): the engine keeps the value
/// alive until the host frees this box, which is what the C++ handle's `Reset`
/// and destructor do.
pub struct SlagTraced {
    traced: Traced,
}

/// The C++ function a registered host global calls. `call` is the
/// `FunctionCall*` for this invocation, usable with [`slag_call_eval`] when the
/// host callback re-enters the engine.
pub type SlagHostFn =
    Option<unsafe extern "C" fn(*mut c_void, *const c_void, u64, *const u64, usize) -> u64>;

unsafe fn bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() {
        return &[];
    }
    std::slice::from_raw_parts(ptr, len)
}

unsafe fn text<'a>(ptr: *const u8, len: usize) -> Result<&'a str, ()> {
    std::str::from_utf8(bytes(ptr, len)).map_err(|_| ())
}

fn record(context: &SlagContext, error: JsError) {
    *context.pending_error.borrow_mut() = Some(error.to_string());
}

fn reentrant(context: &SlagContext) {
    record(
        context,
        JsError::new(
            ErrorKind::Error,
            "re-entrant evaluation is not supported; use FunctionCall::eval".to_string(),
        ),
    );
}

/// Wrap a C callback in the engine's `HostFn`, with this context's pending-error
/// slot: a callback that set an error fails the call rather than returning.
fn host_fn(context: &SlagContext, callback: SlagHostFn, data: *mut c_void) -> HostFn {
    let pending = Rc::clone(&context.pending_error);
    Box::new(move |call: &FunctionCall<'_>| {
        let args: Vec<u64> = call.args().iter().map(JsValue::to_bits).collect();
        let this = call.this().to_bits();
        let produced = match callback {
            Some(callback) => unsafe {
                callback(
                    data,
                    call as *const FunctionCall<'_> as *const c_void,
                    this,
                    args.as_ptr(),
                    args.len(),
                )
            },
            None => JsValue::undefined().to_bits(),
        };
        if let Some(message) = pending.borrow_mut().take() {
            return Err(JsError::new(ErrorKind::Error, message));
        }
        Ok(JsValue::from_bits(produced))
    })
}

thread_local! {
    /// The values the facade's handle slots still hold. The engine consults
    /// this as a root source, so a `Local<T>` is a precise root rather than a
    /// word the conservative stack scan has to guess about.
    static ROOTS: RefCell<Vec<Value>> = const { RefCell::new(Vec::new()) };
}

struct HandleRoots;

impl RootSource for HandleRoots {
    fn roots(&self, visit: &mut dyn FnMut(GcAny)) {
        ROOTS.with(|roots| {
            for value in roots.borrow().iter() {
                value.trace(visit);
            }
        });
    }
}

static HANDLE_ROOTS: HandleRoots = HandleRoots;

fn register_roots() {
    register_root_source(&HANDLE_ROOTS);
}

/// Retain one engine value for a facade handle slot. The facade calls this only
/// for `Local<T>` where `T` is an engine value; a `Context*`, `Script*`, or
/// template pointer must never enter the registry.
#[unsafe(no_mangle)]
pub extern "C" fn slag_root_retain(value: u64) {
    register_roots();
    ROOTS.with(|roots| roots.borrow_mut().push(Value::from_bits(value)));
}

/// Release one retention, matching [`slag_root_retain`]. A duplicate value in
/// two slots has two entries, so this removes one.
#[unsafe(no_mangle)]
pub extern "C" fn slag_root_release(value: u64) {
    ROOTS.with(|roots| {
        let mut roots = roots.borrow_mut();
        if let Some(index) = roots.iter().rposition(|held| held.bits() == value) {
            roots.remove(index);
        }
    });
}

/// The number of retentions the facade's registry holds. A diagnostic for the
/// C++ harness: a collection cannot show that a `Global<T>` retained its value,
/// because a stale word in a dead C++ frame keeps the value alive by itself, so
/// the harness asserts this count moves by exactly one instead.
#[unsafe(no_mangle)]
pub extern "C" fn slag_root_count() -> usize {
    ROOTS.with(|roots| roots.borrow().len())
}

/// Run a full collection now, with the facade's roots in place. The C++ harness
/// uses this to prove a value held only by a handle slot survives.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_collect(context: *mut SlagContext) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let Ok(mut engine) = context.context.try_borrow_mut() else {
        reentrant(context);
        return 1;
    };
    engine.agent_mut().collect_garbage();
    0
}

/// Make `value` a root the host owns (`v8::TracedReference<T>`), returned as a
/// box the host frees with [`slag_traced_free`]. A clone is a *second*
/// reference, so releasing one does not release the other.
#[unsafe(no_mangle)]
pub extern "C" fn slag_traced_new(value: u64) -> *mut SlagTraced {
    Box::into_raw(Box::new(SlagTraced {
        traced: Traced::new(EngineLocal::from_value(
            JsValue::from_bits(value).into_value(),
        )),
    }))
}

/// The value a traced reference names, written to `out`. 0 on success, 1 once
/// the reference has been released, 2 for a bad argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_traced_get(traced: *const SlagTraced, out: *mut u64) -> i32 {
    let Some(traced) = traced.as_ref() else {
        return 2;
    };
    match traced.traced.to_local() {
        Some(value) => {
            if !out.is_null() {
                *out = value.value().bits();
            }
            0
        }
        None => 1,
    }
}

/// Whether the host has released `traced` (`v8::TracedReference<T>::IsEmpty`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_traced_is_empty(traced: *const SlagTraced) -> i32 {
    traced
        .as_ref()
        .map_or(1, |traced| i32::from(traced.traced.is_empty()))
}

/// A second reference to the same value, as its own box.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_traced_clone(traced: *const SlagTraced) -> *mut SlagTraced {
    let Some(traced) = traced.as_ref() else {
        return std::ptr::null_mut();
    };
    Box::into_raw(Box::new(SlagTraced {
        traced: traced.traced.clone(),
    }))
}

/// Release the reference (`v8::TracedReference<T>::Reset`): the value stops
/// being a root at the next collection.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_traced_reset(traced: *mut SlagTraced) {
    if let Some(traced) = traced.as_mut() {
        traced.traced.reset();
    }
}

/// Free the box, which is the host releasing its reference.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_traced_free(traced: *mut SlagTraced) {
    if !traced.is_null() {
        drop(Box::from_raw(traced));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn slag_isolate_new() -> *mut SlagIsolate {
    Box::into_raw(Box::new(SlagIsolate { _private: () }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_isolate_free(isolate: *mut SlagIsolate) {
    if !isolate.is_null() {
        drop(Box::from_raw(isolate));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn slag_context_new(_isolate: *mut SlagIsolate) -> *mut SlagContext {
    match Context::new() {
        Ok(context) => Box::into_raw(Box::new(SlagContext {
            context: RefCell::new(context),
            pending_error: Rc::new(RefCell::new(None)),
        })),
        Err(_) => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_context_free(context: *mut SlagContext) {
    if !context.is_null() {
        drop(Box::from_raw(context));
    }
}

/// Evaluate `source` and drain the job queues, writing the completion value to
/// `out`. Returns 0 on success, 1 with a pending error, 2 for a bad argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_eval(
    context: *mut SlagContext,
    source: *const u8,
    len: usize,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let Ok(source) = text(source, len) else {
        return 2;
    };
    let Ok(mut engine) = context.context.try_borrow_mut() else {
        reentrant(context);
        return 1;
    };
    match engine.eval(source) {
        Ok(value) => {
            if !out.is_null() {
                *out = value.to_bits();
            }
            0
        }
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// Parse `source` without running it (`v8::Script::Compile`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_compile(
    context: *mut SlagContext,
    source: *const u8,
    len: usize,
    out: *mut *mut SlagScript,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let Ok(source) = text(source, len) else {
        return 2;
    };
    let Ok(engine) = context.context.try_borrow() else {
        reentrant(context);
        return 1;
    };
    match engine.compile_script(source) {
        Ok(script) => {
            if !out.is_null() {
                *out = Box::into_raw(Box::new(SlagScript { script }));
            }
            0
        }
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// Run a script `slag_compile` produced (`v8::Script::Run`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_script_run(
    context: *mut SlagContext,
    script: *mut SlagScript,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let Some(script) = script.as_ref() else {
        return 2;
    };
    let Ok(mut engine) = context.context.try_borrow_mut() else {
        reentrant(context);
        return 1;
    };
    match engine.run_compiled_script(&script.script) {
        Ok(value) => {
            if !out.is_null() {
                *out = value.to_bits();
            }
            0
        }
        Err(error) => {
            record(context, error);
            1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_script_free(script: *mut SlagScript) {
    if !script.is_null() {
        drop(Box::from_raw(script));
    }
}

/// Register `callback` as global `name` (`Context::New` over a global
/// template). `data` is the facade's `FunctionTemplate*`; it is passed back to
/// the callback unchanged.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_register_fn(
    context: *mut SlagContext,
    name: *const u8,
    name_len: usize,
    callback: SlagHostFn,
    data: *mut c_void,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let Ok(name) = text(name, name_len) else {
        return 2;
    };
    let host = host_fn(context, callback, data);
    let Ok(mut engine) = context.context.try_borrow_mut() else {
        reentrant(context);
        return 1;
    };
    match engine.register_fn(name, 0, host) {
        Ok(_) => 0,
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// Evaluate inside a host callback (`FunctionCall::eval`), which is the safe
/// re-entry path: the outer evaluation is already running.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_call_eval(
    call: *const c_void,
    source: *const u8,
    len: usize,
    out: *mut u64,
) -> i32 {
    let Some(call) = (call as *const FunctionCall<'_>).as_ref() else {
        return 2;
    };
    let Ok(source) = text(source, len) else {
        return 2;
    };
    match call.eval(source) {
        Ok(value) => {
            if !out.is_null() {
                *out = value.to_bits();
            }
            0
        }
        Err(_) => 1,
    }
}

/// The host callback's `Isolate::ThrowError`: set the pending exception the
/// outer call will surface.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_throw_error(
    context: *mut SlagContext,
    message: *const u8,
    len: usize,
) {
    let Some(context) = context.as_mut() else {
        return;
    };
    let message = text(message, len).unwrap_or("error");
    *context.pending_error.borrow_mut() = Some(message.to_string());
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_has_error(context: *mut SlagContext) -> i32 {
    match context.as_ref() {
        Some(context) if context.pending_error.borrow().is_some() => 1,
        _ => 0,
    }
}

/// Copy the pending error into `buffer` (NUL terminated) and clear it. Returns
/// the message's byte length, or -1 when there was none.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_take_error(
    context: *mut SlagContext,
    buffer: *mut c_char,
    len: usize,
) -> i64 {
    let Some(context) = context.as_mut() else {
        return -1;
    };
    let Some(message) = context.pending_error.borrow_mut().take() else {
        return -1;
    };
    let bytes = message.as_bytes();
    if buffer.is_null() || len == 0 {
        return 0;
    }
    let copy = bytes.len().min(len - 1);
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.cast::<u8>(), copy);
    *buffer.add(copy) = 0;
    copy as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn slag_undefined() -> u64 {
    JsValue::undefined().to_bits()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_string_new(text: *const u8, len: usize) -> u64 {
    JsValue::string(str_of_unchecked(text, len)).to_bits()
}

#[unsafe(no_mangle)]
pub extern "C" fn slag_number_new(value: f64) -> u64 {
    JsValue::number(value).to_bits()
}

unsafe fn str_of_unchecked<'a>(ptr: *const u8, len: usize) -> &'a str {
    text(ptr, len).unwrap_or("")
}

/// Render a value the way the A0 samples ask for it. Strings, numbers,
/// booleans, `undefined`, `null`, and a generic object rendering; this is not
/// the engine's full `ToString`.
fn render(value: &JsValue) -> String {
    if let Some(text) = value.as_string() {
        return text;
    }
    if let Some(boolean) = value.as_boolean() {
        return boolean.to_string();
    }
    if let Some(number) = value.as_number() {
        if number.is_nan() {
            return "NaN".to_string();
        }
        if number == f64::INFINITY {
            return "Infinity".to_string();
        }
        if number == f64::NEG_INFINITY {
            return "-Infinity".to_string();
        }
        if number == 0.0 && number.is_sign_negative() {
            return "0".to_string();
        }
        return format!("{number}");
    }
    if value.is_undefined() {
        return "undefined".to_string();
    }
    if value.is_null() {
        return "null".to_string();
    }
    if value.is_object() {
        return "[object Object]".to_string();
    }
    String::new()
}

/// Write a value's UTF-8 rendering. Returns the required byte length (excluding
/// the NUL); writes at most `len - 1` bytes plus a NUL when `buffer` is usable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_value_to_utf8(value: u64, buffer: *mut c_char, len: usize) -> i64 {
    let text = render(&JsValue::from_bits(value));
    let bytes = text.as_bytes();
    if !buffer.is_null() && len > 0 {
        let copy = bytes.len().min(len - 1);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.cast::<u8>(), copy);
        *buffer.add(copy) = 0;
    }
    bytes.len() as i64
}

/// A number for `Int32Value`/`Uint32Value`. Undefined and null convert to 0,
/// which is the `ToInt32` result the sample shell asks for.
#[unsafe(no_mangle)]
pub extern "C" fn slag_value_to_number(value: u64) -> f64 {
    let value = JsValue::from_bits(value);
    if let Some(number) = value.as_number() {
        return number;
    }
    if let Some(boolean) = value.as_boolean() {
        return if boolean { 1.0 } else { 0.0 };
    }
    if value.is_undefined() || value.is_null() {
        return 0.0;
    }
    if let Some(text) = value.as_string() {
        return text.trim().parse().unwrap_or(f64::NAN);
    }
    f64::NAN
}

#[unsafe(no_mangle)]
pub extern "C" fn slag_value_is_string(value: u64) -> i32 {
    i32::from(JsValue::from_bits(value).is_string())
}

#[unsafe(no_mangle)]
pub extern "C" fn slag_value_is_undefined(value: u64) -> i32 {
    i32::from(JsValue::from_bits(value).is_undefined())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_global(context: *mut SlagContext) -> u64 {
    match context.as_ref().and_then(|context| {
        context
            .context
            .try_borrow()
            .ok()
            .and_then(|engine| engine.global().ok())
    }) {
        Some(global) => global.as_value().to_bits(),
        None => JsValue::undefined().to_bits(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_object_set(
    object: u64,
    key: *const u8,
    key_len: usize,
    value: u64,
) -> i32 {
    let Ok(key) = text(key, key_len) else {
        return 2;
    };
    let Some(object) = JsValue::from_bits(object).as_object() else {
        return 2;
    };
    match object.set(key, JsValue::from_bits(value)) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// Create an ordinary object in `context`'s realm with `count` internal fields,
/// writing its tagged value to `out`. `api_wrapper` sets
/// `v8::Object::IsApiWrapper`: this is the engine half of
/// `v8::ObjectTemplate::NewInstance`. An internal field is the host's untraced
/// pointer, not something the language can see.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_object_new(
    context: *mut SlagContext,
    count: usize,
    api_wrapper: i32,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let created = {
        let engine = match context.context.try_borrow() {
            Ok(engine) => engine,
            Err(_) => {
                reentrant(context);
                return 1;
            }
        };
        engine.create_object()
    };
    let object = match created {
        Ok(object) => object,
        Err(error) => {
            record(context, error);
            return 1;
        }
    };
    object.set_internal_fields(count, api_wrapper != 0);
    if !out.is_null() {
        *out = object.as_value().to_bits();
    }
    0
}

/// How many internal fields `object` carries
/// (`v8::Object::InternalFieldCount`); 0 for a non-object and for an object no
/// template made.
#[unsafe(no_mangle)]
pub extern "C" fn slag_object_internal_field_count(object: u64) -> usize {
    JsValue::from_bits(object)
        .as_object()
        .map_or(0, |object| object.internal_field_count())
}

/// Whether an embedder template made `object`
/// (`v8::Object::IsApiWrapper`).
#[unsafe(no_mangle)]
pub extern "C" fn slag_object_is_api_wrapper(object: u64) -> i32 {
    i32::from(
        JsValue::from_bits(object)
            .as_object()
            .is_some_and(|object| object.is_api_wrapper()),
    )
}

/// Write one internal field (`v8::Object::SetAlignedPointerInInternalField`).
/// Returns 2 when the object has no such field, where V8 aborts.
#[unsafe(no_mangle)]
pub extern "C" fn slag_object_set_internal_field(
    object: u64,
    index: usize,
    pointer: *mut c_void,
) -> i32 {
    match JsValue::from_bits(object).as_object() {
        Some(object) if object.set_internal_field(index, pointer as usize) => 0,
        _ => 2,
    }
}

/// The pointer in one internal field
/// (`v8::Object::GetAlignedPointerFromInternalField`), null when there is none.
#[unsafe(no_mangle)]
pub extern "C" fn slag_object_internal_field(object: u64, index: usize) -> *mut c_void {
    JsValue::from_bits(object)
        .as_object()
        .map_or(std::ptr::null_mut(), |object| {
            object.internal_field(index) as *mut c_void
        })
}

/// Define a host function on `object`: the per-instance methods a template's
/// `NewInstance` carries. The property roots the function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_object_register_fn(
    context: *mut SlagContext,
    object: u64,
    name: *const u8,
    name_len: usize,
    callback: SlagHostFn,
    data: *mut c_void,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Ok(name) = text(name, name_len) else {
        return 2;
    };
    let Some(object) = JsValue::from_bits(object).as_object() else {
        return 2;
    };
    let host = host_fn(context, callback, data);
    let created = {
        let engine = match context.context.try_borrow() {
            Ok(engine) => engine,
            Err(_) => {
                reentrant(context);
                return 1;
            }
        };
        engine.create_function(name, 0, host)
    };
    let function = match created {
        Ok(function) => function,
        Err(error) => {
            record(context, error);
            return 1;
        }
    };
    match object.set(name, function) {
        Ok(()) => 0,
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// [[Get]] a property by a key value (`v8::Object::Get`), writing the result to
/// `out`. ToPropertyKey runs in the engine, so a numeric or object key reads the
/// property the language would.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_object_get_key(
    context: *mut SlagContext,
    object: u64,
    key: u64,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let result = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.get_property(JsValue::from_bits(object), JsValue::from_bits(key))
    };
    match result {
        Ok(value) => {
            if !out.is_null() {
                *out = value.to_bits();
            }
            0
        }
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// [[Set]] a property by a key value (`v8::Object::Set`), throwing on failure as
/// V8's throwing mode does.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_object_set_key(
    context: *mut SlagContext,
    object: u64,
    key: u64,
    value: u64,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let result = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.set_property(
            JsValue::from_bits(object),
            JsValue::from_bits(key),
            JsValue::from_bits(value),
        )
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            record(context, error);
            1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn slag_version() -> *const c_char {
    c"slag-a0".as_ptr()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_round_trips_a_string() {
        unsafe {
            let isolate = slag_isolate_new();
            let context = slag_context_new(isolate);
            assert!(!context.is_null());

            let source = b"'Hello' + ', World!'";
            let mut value = 0;
            assert_eq!(
                slag_eval(context, source.as_ptr(), source.len(), &mut value),
                0
            );

            let len = slag_value_to_utf8(value, std::ptr::null_mut(), 0);
            let mut buffer = vec![0u8; len as usize + 1];
            slag_value_to_utf8(value, buffer.as_mut_ptr().cast(), buffer.len());
            assert_eq!(
                std::ffi::CStr::from_ptr(buffer.as_ptr().cast())
                    .to_str()
                    .expect("utf8"),
                "Hello, World!"
            );

            slag_context_free(context);
            slag_isolate_free(isolate);
        }
    }

    #[test]
    fn numbers_round_trip_through_the_abi() {
        let value = slag_number_new(2.5);
        assert_eq!(slag_value_to_number(value), 2.5);
    }

    #[test]
    fn the_root_registry_keeps_a_value_the_heap_would_otherwise_sweep() {
        use crux::handle::Handle;
        use crux::string::JsString;

        let value = Value::String(Handle::new(JsString::from_utf8("rooted")));
        let peer = Value::String(Handle::new(JsString::from_utf8("peer")));
        let rooted = value.as_string().expect("string").as_any().addr();
        let peer_box = peer.as_string().expect("string").as_any().addr();

        slag_root_retain(value.bits());
        let swept = crux::heap::with_heap_mut(|heap| heap.collect(&[]));
        assert!(
            !swept.contains(&rooted),
            "a retained value was swept: {swept:?}"
        );
        assert!(
            swept.contains(&peer_box),
            "an unretained peer survived: {swept:?}"
        );

        slag_root_release(value.bits());
        let swept = crux::heap::with_heap_mut(|heap| heap.collect(&[]));
        assert!(
            swept.contains(&rooted),
            "the released value was still rooted: {swept:?}"
        );
    }

    #[test]
    fn the_root_registry_counts_each_retention_separately() {
        use crux::handle::Handle;
        use crux::string::JsString;

        // A `Global<T>` retains its value once, and a `HandleScope` slot for the
        // same value retains it again, so the registry has to keep one entry per
        // retention rather than remember that a value is rooted.
        let value = Value::String(Handle::new(JsString::from_utf8("held twice")));
        let rooted = value.as_string().expect("string").as_any().addr();

        slag_root_retain(value.bits());
        slag_root_retain(value.bits());

        slag_root_release(value.bits());
        let swept = crux::heap::with_heap_mut(|heap| heap.collect(&[]));
        assert!(
            !swept.contains(&rooted),
            "one release dropped both retentions: {swept:?}"
        );

        slag_root_release(value.bits());
        let swept = crux::heap::with_heap_mut(|heap| heap.collect(&[]));
        assert!(
            swept.contains(&rooted),
            "the second release left the value rooted: {swept:?}"
        );
    }

    #[test]
    fn a_traced_reference_roots_until_the_host_releases_it() {
        use crux::handle::Handle;
        use crux::string::JsString;

        // `Heap::collect` is the deterministic entry point: it does not scan the
        // stack, so only a registered root keeps the box.
        let value = Value::String(Handle::new(JsString::from_utf8("traced")));
        let box_addr = value.as_string().expect("string").as_any().addr();

        let traced = slag_traced_new(value.bits());
        assert!(!traced.is_null());
        unsafe {
            assert_eq!(slag_traced_is_empty(traced), 0);
        }

        // A clone is a second reference: releasing the first leaves the value
        // rooted by the copy.
        let copy = unsafe { slag_traced_clone(traced) };
        assert!(!copy.is_null());
        unsafe { slag_traced_free(traced) };
        let swept = crux::heap::with_heap_mut(|heap| heap.collect(&[]));
        assert!(
            !swept.contains(&box_addr),
            "the copy stopped rooting the value: {swept:?}"
        );

        unsafe {
            let mut out = 0;
            assert_eq!(slag_traced_get(copy, &mut out), 0);
            assert_eq!(out, value.bits());

            slag_traced_reset(copy);
            assert_eq!(slag_traced_is_empty(copy), 1);
            assert_eq!(slag_traced_get(copy, &mut out), 1, "a released reference");
            slag_traced_free(copy);
        }

        let swept = crux::heap::with_heap_mut(|heap| heap.collect(&[]));
        assert!(
            swept.contains(&box_addr),
            "the released value was still rooted: {swept:?}"
        );
    }

    #[test]
    fn compile_reports_syntax_before_run() {
        unsafe {
            let isolate = slag_isolate_new();
            let context = slag_context_new(isolate);

            let bad = b"function (";
            let mut script = std::ptr::null_mut();
            assert_eq!(
                slag_compile(context, bad.as_ptr(), bad.len(), &mut script),
                1
            );
            assert!(script.is_null());
            assert_eq!(slag_has_error(context), 1);

            let mut message = [0i8; 256];
            let len = slag_take_error(context, message.as_mut_ptr(), message.len());
            assert!(len > 0);
            assert_eq!(slag_has_error(context), 0);

            let good = b"1 + 2";
            assert_eq!(
                slag_compile(context, good.as_ptr(), good.len(), &mut script),
                0
            );
            let mut value = 0;
            assert_eq!(slag_script_run(context, script, &mut value), 0);
            assert_eq!(slag_value_to_number(value), 3.0);
            slag_script_free(script);

            slag_context_free(context);
            slag_isolate_free(isolate);
        }
    }

    #[test]
    fn a_registered_function_runs_and_returns_a_value() {
        unsafe extern "C" fn double(
            _data: *mut c_void,
            _call: *const c_void,
            _this: u64,
            args: *const u64,
            len: usize,
        ) -> u64 {
            let arg = if len == 0 {
                0.0
            } else {
                slag_value_to_number(*args)
            };
            JsValue::number(arg * 2.0).to_bits()
        }

        unsafe {
            let isolate = slag_isolate_new();
            let context = slag_context_new(isolate);
            let name = b"twice";
            assert_eq!(
                slag_register_fn(
                    context,
                    name.as_ptr(),
                    name.len(),
                    Some(double),
                    std::ptr::null_mut()
                ),
                0
            );

            let source = b"twice(21)";
            let mut value = 0;
            assert_eq!(
                slag_eval(context, source.as_ptr(), source.len(), &mut value),
                0
            );
            assert_eq!(slag_value_to_number(value), 42.0);

            slag_context_free(context);
            slag_isolate_free(isolate);
        }
    }

    #[test]
    fn an_api_object_carries_host_pointers_and_instance_methods() {
        unsafe extern "C" fn double(
            _data: *mut c_void,
            _call: *const c_void,
            _this: u64,
            args: *const u64,
            len: usize,
        ) -> u64 {
            let arg = if len == 0 {
                0.0
            } else {
                slag_value_to_number(*args)
            };
            JsValue::number(arg * 2.0).to_bits()
        }

        unsafe {
            let isolate = slag_isolate_new();
            let context = slag_context_new(isolate);

            let mut object = 0;
            assert_eq!(slag_object_new(context, 2, 1, &mut object), 0);
            assert_eq!(slag_object_internal_field_count(object), 2);
            assert_eq!(slag_object_is_api_wrapper(object), 1);
            let mut marker = Box::new(7u8);
            let pointer: *mut c_void = (&mut *marker as *mut u8).cast();
            assert_eq!(
                slag_object_internal_field(object, 0),
                std::ptr::null_mut(),
                "a fresh slot"
            );
            assert_eq!(slag_object_set_internal_field(object, 0, pointer), 0);
            assert_eq!(slag_object_internal_field(object, 0), pointer);
            assert_eq!(
                slag_object_set_internal_field(object, 2, pointer),
                2,
                "past the end"
            );

            // An instance method, as a template's NewInstance would define.
            let name = b"twice";
            assert_eq!(
                slag_object_register_fn(
                    context,
                    object,
                    name.as_ptr(),
                    name.len(),
                    Some(double),
                    std::ptr::null_mut()
                ),
                0
            );

            let key = b"thing";
            assert_eq!(
                slag_object_set(slag_global(context), key.as_ptr(), key.len(), object),
                0
            );
            let source = b"thing.twice(21)";
            let mut value = 0;
            assert_eq!(
                slag_eval(context, source.as_ptr(), source.len(), &mut value),
                0
            );
            assert_eq!(slag_value_to_number(value), 42.0);

            // An ordinary object answers the same questions with zeroes.
            let plain = slag_global(context);
            assert_eq!(slag_object_internal_field_count(plain), 0);
            assert_eq!(slag_object_is_api_wrapper(plain), 0);
            assert_eq!(slag_object_internal_field(plain, 0), std::ptr::null_mut());

            slag_context_free(context);
            slag_isolate_free(isolate);
        }
    }
}
