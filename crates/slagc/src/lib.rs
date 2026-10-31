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
    let pending = Rc::clone(&context.pending_error);
    let host: HostFn = Box::new(move |call: &FunctionCall<'_>| {
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
    });
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
}
