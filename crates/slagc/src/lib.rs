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

mod handler;

use handler::CxxHandler;
pub use handler::{SlagPropertyDescriptor, SlagPropertyHandler};

use crux::handle::Handle;
use crux::heap::{GcAny, RootSource, Trace, register_root_source};
use crux::property::PropertyKey;
use crux::string::JsString;
use crux::value::{Value, ValueKind};
use slag::api::{Local as EngineLocal, Traced};
use slag::{
    CallbackWindow, CompiledScript, Context, ErrorKind, FunctionCall, HostFn, JsError, JsModule,
    JsValue, OwnProperty, PromiseHandles, PromiseState,
};

/// The isolate a host enters. The engine's contexts are the state; this token
/// exists so the facade's `Isolate` can own and free them in order, and so the
/// host's external-memory account (`AdjustAmountOfExternalAllocatedMemory`)
/// has somewhere to live.
pub struct SlagIsolate {
    /// The bytes the host says it holds outside the engine's heap. The engine's
    /// own buffer walk is added when the total is asked for.
    external_memory: std::cell::Cell<i64>,
}

/// One realm and its pending-exception slot (`v8::Context`).
///
/// The engine's `Context` is behind a `RefCell` so a re-entrant call from a host
/// callback fails with a pending error instead of aliasing it. The safe
/// re-entry is `FunctionCall::eval` ([`slag_call_eval`]); `load()` in the
/// sample shell does not use it, so it is refused rather than unsound.
pub struct SlagContext {
    context: RefCell<Context>,
    /// The realm this context's objects are created in, read once at creation
    /// so `slag_context_realm` and the scope enter/leave pair never borrow the
    /// engine. It is also the realm `Context::Scope` makes current, which is
    /// what an access check compares a value's realm against.
    realm: u32,
    /// The error the last failed operation left: its message is what
    /// `slag_take_error` reports, the thrown value is what `TryCatch::Exception`
    /// returns, and a compile's position is what `TryCatch::Message` answers.
    pending_error: Rc<RefCell<Option<PendingError>>>,
    /// The context's agent address, which is stable because the context boxes it.
    /// A host callback publishes it as its token so it can allocate through the
    /// operation that is running (see [`slag_callback_token`]).
    agent: crux::function::AgentPtr,
    /// The capabilities a host holds a resolving function for
    /// (`v8::Promise::Resolver`), keyed by that function's identity. The engine's
    /// own record names the pair by the same identity but keeps no value for the
    /// rejecting function, so this is where a handle names it back. The promise
    /// and the resolving function are roots for as long as an entry exists, which
    /// is what keeps the key from ever naming another function.
    resolvers: RefCell<std::collections::HashMap<u64, Resolver>>,
}

/// One capability a `v8::Promise::Resolver` handle names: the promise it settles
/// and the pair's rejecting function (`0` once the pair has fired, which is when
/// neither function can run again).
struct Resolver {
    promise: u64,
    reject: u64,
}

impl Drop for SlagContext {
    fn drop(&mut self) {
        for (_, resolver) in self.resolvers.get_mut().drain() {
            slag_root_release(resolver.promise);
            if resolver.reject != 0 {
                slag_root_release(resolver.reject);
            }
        }
    }
}

/// Where a pending error came from, when a compile could name it: the host's
/// script name, the position, and the line's text.
///
/// Only a failed compile records one. A `JsError::span` indexes whichever source
/// the failing code was parsed from and does not say which one that was, so a
/// runtime error's span could name another script; a compile's necessarily came
/// from the text in hand. A runtime error therefore answers "no position".
struct ErrorPosition {
    name: Option<String>,
    /// 1-based.
    line: u32,
    /// 0-based, as V8's `Message::GetStartColumn` counts.
    column: u32,
    line_text: String,
}

/// The pending error and the position a compile recorded for it.
pub(crate) struct PendingError {
    pub(crate) error: JsError,
    position: Option<ErrorPosition>,
}

/// A parsed, not-yet-run script (`v8::Script`).
pub struct SlagScript {
    script: CompiledScript,
}

/// A module record (`v8::Module`). The Rust box pins the record; a module is
/// never freed, like a script or a template in this facade.
pub struct SlagModule {
    module: JsModule,
}

/// A decoded WebAssembly module (`v8::CompiledWasmModule`): plain data the host
/// can carry between module objects.
pub struct SlagWasmModule {
    compiled: slag::api::CompiledWasmModule,
}

/// A host-traced root (`v8::TracedReference<T>`): the engine keeps the value
/// alive until the host frees this box, which is what the C++ handle's `Reset`
/// and destructor do.
pub struct SlagTraced {
    traced: Traced,
}

/// A block of buffer bytes the host holds independently of a JavaScript buffer
/// (`v8::BackingStore`). The block is a refcounted handle to the same storage
/// the buffer names, so this box can outlive the object that came from and the
/// bytes stay alive while it exists.
pub struct SlagBuffer {
    block: crux::typed_array::SharedBuffer,
    byte_length: usize,
    is_shared: bool,
    resizable: bool,
}

/// A weak persistent handle (`v8::Global<T>::SetWeak`): it does not keep its
/// value alive, and the collector empties it on the sweep that takes the value.
pub struct SlagWeak {
    handle: crux::heap::WeakHandle,
}

/// The heap numbers `v8::Isolate::GetHeapStatistics` fills, mirrored from
/// `runtime::api::HeapStatistics`. Every field is a `usize` except the flags;
/// the facade copies them into V8's struct.
#[repr(C)]
pub struct SlagHeapStatistics {
    pub total_heap_size: usize,
    pub total_heap_size_executable: usize,
    pub total_physical_size: usize,
    pub total_available_size: usize,
    pub used_heap_size: usize,
    pub heap_size_limit: usize,
    pub malloced_memory: usize,
    pub external_memory: usize,
    pub peak_malloced_memory: usize,
    pub does_zap_garbage: i32,
    pub number_of_native_contexts: usize,
    pub number_of_detached_contexts: usize,
    pub total_global_handles_size: usize,
    pub used_global_handles_size: usize,
    pub embedded_blob_size: usize,
}

/// The C++ function a registered host global calls. `call` is the
/// `FunctionCall*` for this invocation, usable with [`slag_call_eval`] when the
/// host callback re-enters the engine.
pub type SlagHostFn =
    Option<unsafe extern "C" fn(*mut c_void, *const c_void, u64, *const u64, usize) -> u64>;

/// The C++ side's accessor callbacks (`v8::ObjectTemplate::SetAccessor`):
/// `(data, holder, key, out)` writes the value a read answers, and
/// `(data, holder, key, value)` takes a write. Both report 1 with an error in the
/// context's pending slot, and 2 for a bad argument.
pub type SlagAccessorGetter = Option<unsafe extern "C" fn(*mut c_void, u64, u64, *mut u64) -> i32>;
pub type SlagAccessorSetter = Option<unsafe extern "C" fn(*mut c_void, u64, u64, u64) -> i32>;

unsafe fn bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() {
        return &[];
    }
    std::slice::from_raw_parts(ptr, len)
}

unsafe fn text<'a>(ptr: *const u8, len: usize) -> Result<&'a str, ()> {
    std::str::from_utf8(bytes(ptr, len)).map_err(|_| ())
}

unsafe fn slice_of<'a, T>(ptr: *const T, len: usize) -> &'a [T] {
    if ptr.is_null() || len == 0 {
        return &[];
    }
    std::slice::from_raw_parts(ptr, len)
}

fn record(context: &SlagContext, error: JsError) {
    record_at(context, error, None);
}

fn record_at(context: &SlagContext, error: JsError, position: Option<ErrorPosition>) {
    *context.pending_error.borrow_mut() = Some(PendingError { error, position });
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
    let agent = context.agent;
    Box::new(move |call: &FunctionCall<'_>| {
        // Publish the running window, so a callback that allocates or writes
        // through the flat ABI's token route (`slag_callback_token`) is admitted
        // rather than refused as a re-entry.
        let _window = CallbackWindow::enter(agent);
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
        if let Some(pending) = pending.borrow_mut().take() {
            return Err(pending.error);
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
    engine.collect_garbage();
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

/// A new zeroed block of `byte_length` bytes
/// (`v8::ArrayBuffer::NewBackingStore`).
#[unsafe(no_mangle)]
pub extern "C" fn slag_buffer_new(byte_length: usize) -> *mut SlagBuffer {
    Box::into_raw(Box::new(SlagBuffer {
        block: crux::typed_array::SharedBuffer::new(byte_length),
        byte_length,
        is_shared: false,
        resizable: false,
    }))
}

/// How a host releases memory it lent to a backing store
/// (`v8::BackingStoreDeleterCallback`). The engine calls it once, with the
/// bytes, their length, and the host's own pointer.
pub type SlagBufferDeleter = extern "C" fn(*mut u8, usize, *mut c_void);

/// A block over memory the host owns
/// (`ArrayBuffer::NewBackingStore(data, length, deleter, deleter_data)`).
///
/// `counted` says whether the bytes belong in the isolate's external-memory
/// account: the address range a host lends through a deleter is the host's own
/// (0), while one its array-buffer allocator handed out is the isolate's (1).
///
/// # Safety
///
/// `data` must point at `byte_length` readable and writable bytes that stay
/// valid until `deleter` runs, and `deleter` must be the only thing that
/// releases them.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_buffer_borrowed(
    data: *mut u8,
    byte_length: usize,
    deleter: Option<SlagBufferDeleter>,
    deleter_data: *mut c_void,
    counted: i32,
) -> *mut SlagBuffer {
    let release = deleter.map(|deleter| {
        Box::new(move || deleter(data, byte_length, deleter_data)) as Box<dyn FnOnce()>
    });
    // SAFETY: the caller's contract; `release` is the one call that frees the
    // bytes.
    let block = unsafe {
        if counted != 0 {
            crux::typed_array::SharedBuffer::borrowed_counted(data, byte_length, release)
        } else {
            crux::typed_array::SharedBuffer::borrowed(data, byte_length, release)
        }
    };
    Box::into_raw(Box::new(SlagBuffer {
        block,
        byte_length,
        is_shared: false,
        resizable: false,
    }))
}

/// The first byte of the block, or null for an empty one
/// (`v8::BackingStore::Data`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_buffer_data(buffer: *const SlagBuffer) -> *mut u8 {
    match buffer.as_ref() {
        Some(buffer) => buffer.block.data_ptr(),
        None => std::ptr::null_mut(),
    }
}

/// The byte offset of a view (`v8::ArrayBufferView::ByteOffset`): a typed array's
/// own slots answer without the engine, and a `DataView`'s record needs it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_array_view_byte_offset(
    context: *mut SlagContext,
    value: u64,
) -> usize {
    array_view_geometry(context, value).map_or(0, |geometry| geometry.0)
}

/// The byte length of a view (`v8::ArrayBufferView::ByteLength`), read the same
/// way as [`slag_array_view_byte_offset`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_array_view_byte_length(
    context: *mut SlagContext,
    value: u64,
) -> usize {
    array_view_geometry(context, value).map_or(0, |geometry| geometry.1)
}

unsafe fn array_view_geometry(context: *mut SlagContext, value: u64) -> Option<(usize, usize)> {
    let value = JsValue::from_bits(value);
    if let Some(info) = slag::typed_array_info(&value) {
        return Some((info.byte_offset, info.byte_length));
    }
    let context = context.as_ref()?;
    let engine = context.context.try_borrow().ok()?;
    engine.data_view_info(value)
}

/// The block's length in bytes (`v8::BackingStore::ByteLength`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_buffer_byte_length(buffer: *const SlagBuffer) -> usize {
    buffer.as_ref().map_or(0, |buffer| buffer.byte_length)
}

/// Whether the block backs a SharedArrayBuffer (`v8::BackingStore::IsShared`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_buffer_is_shared(buffer: *const SlagBuffer) -> i32 {
    buffer
        .as_ref()
        .map_or(0, |buffer| i32::from(buffer.is_shared))
}

/// Whether the buffer the block came from may be resized by JavaScript.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_buffer_is_resizable(buffer: *const SlagBuffer) -> i32 {
    buffer
        .as_ref()
        .map_or(0, |buffer| i32::from(buffer.resizable))
}

/// Whether the bytes are the host's rather than the engine's
/// (`SharedBuffer::is_borrowed`): an external-memory walk skips those.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_buffer_is_borrowed(buffer: *const SlagBuffer) -> i32 {
    buffer
        .as_ref()
        .map_or(0, |buffer| i32::from(buffer.block.is_borrowed()))
}

/// Release one host handle to the block. The storage stays alive while a
/// buffer or another handle still names it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_buffer_free(buffer: *mut SlagBuffer) {
    if !buffer.is_null() {
        drop(Box::from_raw(buffer));
    }
}

/// Create an ArrayBuffer (or a SharedArrayBuffer, for a shared block) over
/// `buffer` (`v8::ArrayBuffer::New` with a `BackingStore`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_buffer_to_array_buffer(
    context: *mut SlagContext,
    buffer: *const SlagBuffer,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let Some(buffer) = buffer.as_ref() else {
        return 2;
    };
    let created = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        let block = buffer.block.clone();
        let length = buffer.byte_length;
        if buffer.is_shared {
            engine.shared_array_buffer_from_shared(block, length)
        } else {
            engine.array_buffer_from_shared(block, length)
        }
    };
    match created {
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

/// Clone the block behind an ArrayBuffer/SharedArrayBuffer into a host handle
/// (`v8::ArrayBuffer::GetBackingStore`). A null pointer answers a value that is
/// not a buffer; a detached buffer answers its block with a zero length, which
/// is how V8 reports it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_buffer_of(context: *mut SlagContext, value: u64) -> *mut SlagBuffer {
    let Some(info) = buffer_info_of(context, value) else {
        return std::ptr::null_mut();
    };
    let byte_length = if info.detached { 0 } else { info.byte_length };
    Box::into_raw(Box::new(SlagBuffer {
        block: info.block,
        byte_length,
        is_shared: info.is_shared,
        resizable: info.resizable,
    }))
}

unsafe fn buffer_info_of(context: *mut SlagContext, value: u64) -> Option<slag::BufferInfo> {
    let value = JsValue::from_bits(value);
    match context
        .as_ref()
        .and_then(|context| context.context.try_borrow().ok())
    {
        Some(engine) => engine.buffer_info(&value),
        // Inside a callback the run holds the context's cell; the live window
        // has the agent the buffer table lives on.
        None => slag::buffer_info(&value),
    }
}

/// A buffer's byte length, zero once detached (`v8::ArrayBuffer::ByteLength`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_array_buffer_byte_length(
    context: *mut SlagContext,
    value: u64,
) -> usize {
    match buffer_info_of(context, value) {
        Some(info) if !info.detached => info.byte_length,
        _ => 0,
    }
}

/// A buffer's first byte, or a null pointer when it is detached or not a buffer
/// (`v8::ArrayBuffer::Data`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_array_buffer_data(context: *mut SlagContext, value: u64) -> *mut u8 {
    match buffer_info_of(context, value) {
        Some(info) if !info.detached => info.block.data_ptr(),
        _ => std::ptr::null_mut(),
    }
}

/// Whether the buffer was detached (`v8::ArrayBuffer::WasDetached`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_array_buffer_is_detached(
    context: *mut SlagContext,
    value: u64,
) -> i32 {
    i32::from(buffer_info_of(context, value).is_some_and(|info| info.detached))
}

/// Whether the buffer may be detached (`v8::ArrayBuffer::IsDetachable`): not
/// shared and not already detached.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_array_buffer_is_detachable(
    context: *mut SlagContext,
    value: u64,
) -> i32 {
    i32::from(buffer_info_of(context, value).is_some_and(|info| !info.is_shared && !info.detached))
}

/// Create a SharedArrayBuffer over `buffer`, whatever the block's own flags say
/// (`v8::SharedArrayBuffer::New` with a `BackingStore`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_shared_array_buffer_new(
    context: *mut SlagContext,
    buffer: *const SlagBuffer,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let Some(buffer) = buffer.as_ref() else {
        return 2;
    };
    let created = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.shared_array_buffer_from_shared(buffer.block.clone(), buffer.byte_length)
    };
    match created {
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

/// Detach a buffer (`v8::ArrayBuffer::Detach`). `key` is absent when the host
/// passed none; answers 1 when the buffer was detached, 0 when it refused (a
/// key mismatch or an already detached buffer).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_array_buffer_detach(
    context: *mut SlagContext,
    value: u64,
    key: u64,
    has_key: i32,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 0;
    };
    let Ok(mut engine) = context.context.try_borrow_mut() else {
        reentrant(context);
        return 0;
    };
    let value = JsValue::from_bits(value);
    let key = if has_key != 0 {
        Some(JsValue::from_bits(key))
    } else {
        None
    };
    i32::from(engine.detach_buffer(&value, key.as_ref()))
}

/// Record a buffer's [[ArrayBufferDetachKey]]
/// (`v8::ArrayBuffer::SetDetachKey`); `undefined` clears it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_array_buffer_set_detach_key(
    context: *mut SlagContext,
    value: u64,
    key: u64,
) {
    let Some(context) = context.as_ref() else {
        return;
    };
    let Ok(mut engine) = context.context.try_borrow_mut() else {
        reentrant(context);
        return;
    };
    engine.set_detach_key(&JsValue::from_bits(value), &JsValue::from_bits(key));
}

/// A typed array over `buffer` (`v8::Uint8Array::New` and its siblings).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_typed_array_new(
    context: *mut SlagContext,
    kind: i32,
    buffer: u64,
    byte_offset: usize,
    length: usize,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let Some(kind) = typed_array_kind(kind) else {
        return 2;
    };
    let created = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.create_typed_array(kind, &JsValue::from_bits(buffer), byte_offset, length)
    };
    match created {
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

/// The buffer a typed array views (`v8::ArrayBufferView::Buffer`), written to
/// `out`. Answers 0 for a view and 2 for a value that is not one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_typed_array_buffer(value: u64, out: *mut u64) -> i32 {
    match slag::typed_array_info(&JsValue::from_bits(value)) {
        Some(info) => {
            if !out.is_null() {
                *out = info.buffer.bits();
            }
            0
        }
        None => 2,
    }
}

/// A typed array's byte offset, byte length, and element count
/// (`v8::ArrayBufferView::ByteOffset`/`ByteLength` and `TypedArray::Length`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_typed_array_byte_offset(value: u64) -> usize {
    slag::typed_array_info(&JsValue::from_bits(value)).map_or(0, |info| info.byte_offset)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_typed_array_byte_length(value: u64) -> usize {
    slag::typed_array_info(&JsValue::from_bits(value)).map_or(0, |info| info.byte_length)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_typed_array_length(value: u64) -> usize {
    slag::typed_array_info(&JsValue::from_bits(value)).map_or(0, |info| info.length)
}

/// Add `change` bytes to the host's external-memory account and answer the new
/// account (`v8::Isolate::AdjustAmountOfExternalAllocatedMemory`). The engine's
/// own buffer walk is added by the caller
/// ([`slag_context_external_memory`]).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_isolate_adjust_external_memory(
    isolate: *mut SlagIsolate,
    change: i64,
) -> i64 {
    let Some(isolate) = isolate.as_ref() else {
        return 0;
    };
    let total = isolate.external_memory.get().saturating_add(change).max(0);
    isolate.external_memory.set(total);
    total
}

/// The bytes this realm's live buffers hold, without the host's account
/// (`GetHeapStatistics`'s `external_memory` for one realm).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_context_external_memory(context: *mut SlagContext) -> i64 {
    match context
        .as_ref()
        .and_then(|context| context.context.try_borrow().ok())
    {
        Some(engine) => engine.external_memory(0),
        None => 0,
    }
}

/// Write this realm's snapshot into `buffer` (`SnapshotCreator::CreateBlob`).
/// A null buffer, or one too small, answers the required byte length without
/// writing; -1 is a failed write, with the error recorded.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_context_write_snapshot(
    context: *mut SlagContext,
    buffer: *mut u8,
    len: usize,
) -> i64 {
    let Some(context) = context.as_mut() else {
        return -1;
    };
    let Ok(mut engine) = context.context.try_borrow_mut() else {
        reentrant(context);
        return -1;
    };
    match engine.write_snapshot() {
        Ok(bytes) => {
            if buffer.is_null() || len < bytes.len() {
                return bytes.len() as i64;
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len());
            bytes.len() as i64
        }
        Err(text) => {
            record(context, JsError::new(ErrorKind::Error, text));
            -1
        }
    }
}

/// Restore a blob onto this realm's global (`Isolate::CreateParams`'s
/// `snapshot_blob`). Answers 0 on success, 1 with the error recorded, 2 for a
/// bad argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_context_read_snapshot(
    context: *mut SlagContext,
    data: *const u8,
    len: usize,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let Ok(mut engine) = context.context.try_borrow_mut() else {
        reentrant(context);
        return 1;
    };
    match engine.read_snapshot(bytes(data, len)) {
        Ok(()) => 0,
        Err(text) => {
            record(context, JsError::new(ErrorKind::Error, text));
            1
        }
    }
}

/// Fill `out` with the heap's numbers (`v8::Isolate::GetHeapStatistics`).
/// `host_account` is the isolate's external-memory account, which is added to
/// the engine's own buffer walk.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_heap_statistics(
    context: *mut SlagContext,
    host_account: i64,
    out: *mut SlagHeapStatistics,
) -> i32 {
    let Some(out) = out.as_mut() else {
        return 2;
    };
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Ok(engine) = context.context.try_borrow() else {
        return 1;
    };
    let stats = engine.heap_statistics(host_account);
    *out = SlagHeapStatistics {
        total_heap_size: stats.total_heap_size,
        total_heap_size_executable: stats.total_heap_size_executable,
        total_physical_size: stats.total_physical_size,
        total_available_size: stats.total_available_size,
        used_heap_size: stats.used_heap_size,
        heap_size_limit: stats.heap_size_limit,
        malloced_memory: stats.malloced_memory,
        external_memory: stats.external_memory,
        peak_malloced_memory: stats.peak_malloced_memory,
        does_zap_garbage: i32::from(stats.does_zap_garbage),
        number_of_native_contexts: stats.number_of_native_contexts,
        number_of_detached_contexts: stats.number_of_detached_contexts,
        total_global_handles_size: stats.total_global_handles_size,
        used_global_handles_size: stats.used_global_handles_size,
        embedded_blob_size: 0,
    };
    0
}

/// A weak persistent handle over `value` (`v8::Global<T>::SetWeak`).
#[unsafe(no_mangle)]
pub extern "C" fn slag_weak_new(value: u64) -> *mut SlagWeak {
    Box::into_raw(Box::new(SlagWeak {
        handle: crux::heap::weak_new(Value::from_bits(value)),
    }))
}

/// The value the handle still names, written to `out`. 0 while it is alive, 1
/// once a sweep has taken it, 2 for a bad argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_weak_get(weak: *const SlagWeak, out: *mut u64) -> i32 {
    let Some(weak) = weak.as_ref() else {
        return 2;
    };
    match crux::heap::weak_get(weak.handle) {
        Some(value) => {
            if !out.is_null() {
                *out = value.bits();
            }
            0
        }
        None => 1,
    }
}

/// Release the handle without freeing the box
/// (`Global<T>::ClearWeak` keeps the object).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_weak_clear(weak: *const SlagWeak) {
    if let Some(weak) = weak.as_ref() {
        crux::heap::weak_clear(weak.handle);
    }
}

/// Release the handle and free the box.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_weak_free(weak: *mut SlagWeak) {
    if !weak.is_null() {
        let boxed = Box::from_raw(weak);
        crux::heap::weak_clear(boxed.handle);
    }
}

/// The C callback a weak handle's death runs. The second pass is a separate
/// queue, so it needs no handle: it runs after every first pass of the same
/// drain even if the first pass released the handle.
pub type SlagWeakCallback = extern "C" fn(*mut c_void);

/// Register the callback the engine runs after the collection that took the
/// value. `data` is the host's pointer, passed back unchanged.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_weak_set_callback(
    weak: *const SlagWeak,
    data: *mut c_void,
    callback: Option<SlagWeakCallback>,
) -> i32 {
    let Some(weak) = weak.as_ref() else {
        return 2;
    };
    let Some(callback) = callback else {
        return 2;
    };
    crux::heap::weak_set_callback(weak.handle, Box::new(move || callback(data)));
    0
}

/// Queue a guaranteed finalizer for the current drain
/// (`v8::WeakCallbackInfo::SetSecondPassCallback`): it runs after every first
/// pass, even if the first pass released the handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_weak_queue_second_pass(
    data: *mut c_void,
    callback: Option<SlagWeakCallback>,
) -> i32 {
    let Some(callback) = callback else {
        return 2;
    };
    crux::heap::queue_late_second_pass(Box::new(move || callback(data)));
    0
}

/// The collection phases a host observer receives, mirroring `SlagGcType`.
const SLAG_GC_PROLOGUE: i32 = 0;
const SLAG_GC_SCAVENGE: i32 = 1;
const SLAG_GC_MARK_SWEEP: i32 = 2;

/// A host observer of this agent's collections (`v8::Isolate::AddGC*Callback`).
pub type SlagCollectionObserver = extern "C" fn(*mut c_void, i32, i32);

struct HostCollectionObserver {
    data: *mut c_void,
    observer: SlagCollectionObserver,
}

impl crux::heap::GcObserver for HostCollectionObserver {
    fn prologue(&self, generation: crux::heap::GcGeneration) {
        (self.observer)(self.data, SLAG_GC_PROLOGUE, gc_type(generation));
    }

    fn epilogue(&self, generation: crux::heap::GcGeneration) {
        (self.observer)(self.data, 1, gc_type(generation));
    }
}

fn gc_type(generation: crux::heap::GcGeneration) -> i32 {
    match generation {
        crux::heap::GcGeneration::Minor => SLAG_GC_SCAVENGE,
        crux::heap::GcGeneration::Major => SLAG_GC_MARK_SWEEP,
    }
}

/// Watch every collection this context's agent drives, including the ones its
/// growth policy starts. The observer runs around the mark and sweep, so a host
/// timer or a diagnostics hook sees both ends.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_watch_collections(
    context: *mut SlagContext,
    data: *mut c_void,
    observer: Option<SlagCollectionObserver>,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Some(observer) = observer else {
        return 2;
    };
    let Ok(engine) = context.context.try_borrow() else {
        return 1;
    };
    engine.add_collection_observer(Box::new(HostCollectionObserver { data, observer }));
    0
}

fn typed_array_kind(kind: i32) -> Option<slag::TypedArrayKind> {
    Some(match kind {
        0 => slag::TypedArrayKind::Int8,
        1 => slag::TypedArrayKind::Uint8,
        2 => slag::TypedArrayKind::Uint8Clamped,
        3 => slag::TypedArrayKind::Int16,
        4 => slag::TypedArrayKind::Uint16,
        5 => slag::TypedArrayKind::Int32,
        6 => slag::TypedArrayKind::Uint32,
        7 => slag::TypedArrayKind::Float16,
        8 => slag::TypedArrayKind::Float32,
        9 => slag::TypedArrayKind::Float64,
        10 => slag::TypedArrayKind::BigInt64,
        11 => slag::TypedArrayKind::BigUint64,
        _ => return None,
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn slag_isolate_new() -> *mut SlagIsolate {
    Box::into_raw(Box::new(SlagIsolate {
        external_memory: std::cell::Cell::new(0),
    }))
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
        Ok(mut context) => {
            let agent = context.agent_mut() as *mut _ as crux::function::AgentPtr;
            let realm = context.realm_signifier();
            Box::into_raw(Box::new(SlagContext {
                context: RefCell::new(context),
                realm,
                pending_error: Rc::new(RefCell::new(None)),
                agent,
                resolvers: RefCell::new(std::collections::HashMap::new()),
            }))
        }
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
    name: *const u8,
    name_len: usize,
    out: *mut *mut SlagScript,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let Ok(source) = text(source, len) else {
        return 2;
    };
    let Ok(name) = text(name, name_len) else {
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
            // A compile error's span indexes the text in hand, so this is the one
            // place a position is honest (see `ErrorPosition`).
            let position = error.span.map(|span| {
                let text = syntax::SourceText::from_utf8(source);
                let location = text.line_column(span.start);
                ErrorPosition {
                    name: (!name.is_empty()).then(|| name.to_string()),
                    line: location.line,
                    // The parser counts columns from one, V8's message from zero.
                    column: location.column.saturating_sub(1),
                    line_text: text.line_text(span.start).to_string_lossy(),
                }
            });
            record_at(context, error, position);
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

/// Parse `source` as a module (`v8::ScriptCompiler::CompileModule`). `name` is
/// the frame name and may be absent; `specifier` is the name importers resolve
/// the module by.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_module_compile(
    context: *mut SlagContext,
    specifier: *const u8,
    specifier_len: usize,
    name: *const u8,
    name_len: usize,
    source: *const u8,
    source_len: usize,
    out: *mut *mut SlagModule,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let Ok(specifier) = text(specifier, specifier_len) else {
        return 2;
    };
    let name = if name.is_null() || name_len == 0 {
        None
    } else {
        match text(name, name_len) {
            Ok(name) => Some(name),
            Err(()) => return 2,
        }
    };
    let Ok(source) = text(source, source_len) else {
        return 2;
    };
    let compiled = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.compile_module(specifier, name, source)
    };
    match compiled {
        Ok(module) => {
            if !out.is_null() {
                *out = Box::into_raw(Box::new(SlagModule { module }));
            }
            0
        }
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// The module's state as a `SlagModuleStatus` number; -1 for a bad argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_module_status(module: *const SlagModule) -> i32 {
    let Some(module) = module.as_ref() else {
        return -1;
    };
    match module.module.status() {
        slag::api::ModuleStatus::Uninstantiated => 0,
        slag::api::ModuleStatus::Instantiating => 1,
        slag::api::ModuleStatus::Instantiated => 2,
        slag::api::ModuleStatus::Evaluating => 3,
        slag::api::ModuleStatus::Evaluated => 4,
        slag::api::ModuleStatus::Errored => 5,
    }
}

/// The module's evaluation error, written to `out`. 0 with a value, 1 when it
/// has none, 2 for a bad argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_module_exception(module: *const SlagModule, out: *mut u64) -> i32 {
    let Some(module) = module.as_ref() else {
        return 2;
    };
    match module.module.exception() {
        Some(value) => {
            if !out.is_null() {
                *out = value.bits();
            }
            0
        }
        None => 1,
    }
}

/// The module's identity hash (`v8::Module::GetIdentityHash`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_module_identity_hash(module: *const SlagModule) -> i32 {
    module
        .as_ref()
        .map_or(0, |module| module.module.identity_hash())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_module_is_source_text(module: *const SlagModule) -> i32 {
    module
        .as_ref()
        .map_or(0, |module| i32::from(module.module.is_source_text()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_module_is_synthetic(module: *const SlagModule) -> i32 {
    module
        .as_ref()
        .map_or(0, |module| i32::from(module.module.is_synthetic()))
}

/// How many imports the module declares (`Module::GetModuleRequestsLength`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_module_request_count(module: *const SlagModule) -> usize {
    module
        .as_ref()
        .map_or(0, |module| module.module.request_count())
}

/// The `index`-th request's specifier as a string value, written to `out`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_module_request(
    module: *const SlagModule,
    index: usize,
    out: *mut u64,
) -> i32 {
    let Some(module) = module.as_ref() else {
        return 2;
    };
    match module.module.request_specifier(index) {
        Some(specifier) => {
            if !out.is_null() {
                *out = Value::String(Handle::new(specifier)).bits();
            }
            0
        }
        None => 1,
    }
}

/// Where `offset` into the module's source is, as a 0-based line and column.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_module_source_offset_to_location(
    module: *const SlagModule,
    offset: i32,
    line: *mut i32,
    column: *mut i32,
) -> i32 {
    let Some(module) = module.as_ref() else {
        return 2;
    };
    let location = module
        .module
        .source_offset_to_location(offset.max(0) as u32);
    if !line.is_null() {
        *line = location.line as i32;
    }
    if !column.is_null() {
        *column = location.column as i32;
    }
    0
}

/// Name `module` for later resolutions of `specifier` (a string value), which
/// is what `Module::InstantiateModule`'s host callback does with a resolved
/// dependency.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_module_register(
    context: *mut SlagContext,
    module: *const SlagModule,
    specifier: u64,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Some(module) = module.as_ref() else {
        return 2;
    };
    let Some(text) = Value::from_bits(specifier).as_string() else {
        return 2;
    };
    let specifier = JsString::owned_of(&text);
    let Ok(mut engine) = context.context.try_borrow_mut() else {
        reentrant(context);
        return 1;
    };
    match engine.register_module(&specifier.to_string_lossy(), &module.module) {
        Ok(()) => 0,
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// Link the module and everything the registry resolves for it
/// (`Module::InstantiateModule`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_module_instantiate(
    context: *mut SlagContext,
    module: *const SlagModule,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let Some(module) = module.as_ref() else {
        return 2;
    };
    let linked = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.instantiate_module(&module.module)
    };
    match linked {
        Ok(()) => 0,
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// Evaluate the module (`Module::Evaluate`); the value is a promise.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_module_evaluate(
    context: *mut SlagContext,
    module: *const SlagModule,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let Some(module) = module.as_ref() else {
        return 2;
    };
    let evaluated = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.evaluate_module(&module.module)
    };
    match evaluated {
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

/// Compile `wire_bytes` into a `WebAssembly.Module` object
/// (`v8::WasmModuleObject::Compile`). A decode or validation failure records
/// the `CompileError` the JS-API would report and answers 1.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_wasm_compile(
    context: *mut SlagContext,
    wire_bytes: *const u8,
    wire_len: usize,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let compiled = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.compile_wasm_module(bytes(wire_bytes, wire_len))
    };
    match compiled {
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

/// The decoded module behind a module object
/// (`WasmModuleObject::GetCompiledModule`), boxed for the host to hold.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_wasm_get_compiled(
    context: *mut SlagContext,
    object: u64,
    out: *mut *mut SlagWasmModule,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let compiled = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.wasm_compiled_module(&JsValue::from_bits(object))
    };
    match compiled {
        Ok(compiled) => {
            if !out.is_null() {
                *out = Box::into_raw(Box::new(SlagWasmModule { compiled }));
            }
            0
        }
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// A module object carrying `compiled`
/// (`WasmModuleObject::FromCompiledModule`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_wasm_from_compiled(
    context: *mut SlagContext,
    compiled: *const SlagWasmModule,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let Some(compiled) = compiled.as_ref() else {
        return 2;
    };
    let created = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.wasm_module_object(&compiled.compiled)
    };
    match created {
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

/// Release a compiled-module box (`v8::CompiledWasmModule`'s destructor).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_wasm_module_free(compiled: *mut SlagWasmModule) {
    if !compiled.is_null() {
        drop(Box::from_raw(compiled));
    }
}

/// The module's namespace object (`Module::GetModuleNamespace`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_module_namespace(
    context: *mut SlagContext,
    module: *const SlagModule,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let Some(module) = module.as_ref() else {
        return 2;
    };
    let namespace = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.module_namespace(&module.module)
    };
    match namespace {
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

/// Call `function` with `this` and arguments (`Function::Call`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_call(
    context: *mut SlagContext,
    function: u64,
    this: u64,
    args: *const u64,
    len: usize,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let values: Vec<JsValue> = slice_of::<u64>(args, len)
        .iter()
        .map(|bits| JsValue::from_bits(*bits))
        .collect();
    let called = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.call(
            &JsValue::from_bits(function),
            &JsValue::from_bits(this),
            &values,
        )
    };
    match called {
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

/// Construct `constructor` with arguments (`Function::NewInstance`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_construct(
    context: *mut SlagContext,
    constructor: u64,
    args: *const u64,
    len: usize,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let values: Vec<JsValue> = slice_of::<u64>(args, len)
        .iter()
        .map(|bits| JsValue::from_bits(*bits))
        .collect();
    let constructed = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.construct(&JsValue::from_bits(constructor), &values)
    };
    match constructed {
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

/// A constructible host function over a fixed prototype and instance shape
/// (`v8::FunctionTemplate::GetFunction`): `proto` is the object `new` instances
/// inherit from, and `internal_fields`/`api_wrapper` are the template's shape.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_template_constructor(
    context: *mut SlagContext,
    name: *const u8,
    name_len: usize,
    length: u32,
    proto: u64,
    internal_fields: usize,
    api_wrapper: i32,
    allow_construction: i32,
    callback: SlagHostFn,
    data: *mut c_void,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let Ok(name) = text(name, name_len) else {
        return 2;
    };
    let Some(prototype) = JsValue::from_bits(proto).as_object() else {
        return 2;
    };
    let host = host_fn(context, callback, data);
    let created = {
        let Ok(engine) = context.context.try_borrow() else {
            reentrant(context);
            return 1;
        };
        engine.create_template_constructor(
            name,
            length,
            slag::TemplateInstance {
                prototype,
                internal_fields,
                api_wrapper: api_wrapper != 0,
                allow_construction: allow_construction != 0,
            },
            host,
        )
    };
    match created {
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

/// The object's prototype (`Object::GetPrototype`), or `slag_null()` when it has
/// none. Answers 2 for a value with no object half.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_object_prototype(
    context: *mut SlagContext,
    object: u64,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let prototype = {
        let Ok(engine) = context.context.try_borrow() else {
            reentrant(context);
            return 1;
        };
        engine.object_prototype(&JsValue::from_bits(object))
    };
    match prototype {
        Ok(Some(proto)) => {
            if !out.is_null() {
                *out = proto.to_bits();
            }
            0
        }
        Ok(None) => {
            if !out.is_null() {
                *out = Value::Null.bits();
            }
            0
        }
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// Set the object's prototype (`Object::SetPrototype`), answering 1 when the
/// change took and 0 when the engine refused it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_object_set_prototype(
    context: *mut SlagContext,
    object: u64,
    prototype: u64,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 0;
    };
    let changed = {
        let Ok(engine) = context.context.try_borrow() else {
            reentrant(context);
            return 0;
        };
        engine.set_object_prototype(&JsValue::from_bits(object), &JsValue::from_bits(prototype))
    };
    match changed {
        Ok(changed) => i32::from(changed),
        Err(error) => {
            record(context, error);
            0
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
    record(context, JsError::new(ErrorKind::Error, message.to_string()));
}

/// Record a pending error of `kind` without building the language object, which
/// is what a trampoline can do while it holds the engine (`v8::Signature`'s
/// refusal is a TypeError). The call that ran the trampoline turns it into the
/// thrown value, as [`slag_throw_error`] does.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_throw_error_kind(
    context: *mut SlagContext,
    kind: i32,
    message: *const u8,
    len: usize,
) {
    let Some(context) = context.as_mut() else {
        return;
    };
    let kind = error_kind(kind).unwrap_or(ErrorKind::Error);
    let message = text(message, len).unwrap_or("error");
    record(context, JsError::new(kind, message.to_string()));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_has_error(context: *mut SlagContext) -> i32 {
    match context.as_ref() {
        Some(context) if context.pending_error.borrow().is_some() => 1,
        _ => 0,
    }
}

/// Copy the pending error's message into `buffer` (NUL terminated). Returns the
/// message's byte length, or -1 when there was none. The pending error is left
/// in place: `TryCatch` reads it more than once and its destructor clears it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_pending_error(
    context: *mut SlagContext,
    buffer: *mut c_char,
    len: usize,
) -> i64 {
    let Some(context) = context.as_ref() else {
        return -1;
    };
    let message = context.pending_error.borrow();
    let Some(message) = message.as_ref() else {
        return -1;
    };
    let bytes = message.error.message.as_bytes();
    if buffer.is_null() || len == 0 {
        return 0;
    }
    let copy = bytes.len().min(len - 1);
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.cast::<u8>(), copy);
    *buffer.add(copy) = 0;
    copy as i64
}

/// The thrown value of the pending exception, when it has one
/// (`v8::TryCatch::Exception`), written to `out`. Unlike [`slag_take_error`] this
/// does not clear the slot: a host asks for the exception, the message, and the
/// stack of the same error, and the `TryCatch` destructor is what clears it.
/// Returns 0 when nothing is pending or the error carries no language value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_pending_exception(context: *mut SlagContext, out: *mut u64) -> i32 {
    let Some(context) = context.as_ref() else {
        return 0;
    };
    let pending = context.pending_error.borrow();
    match pending.as_ref().and_then(|pending| pending.error.value) {
        Some(value) => {
            if !out.is_null() {
                *out = value.bits();
            }
            1
        }
        None => 0,
    }
}

/// The V8-style stack the engine captured for the pending exception's object,
/// when it has one (`v8::TryCatch::StackTrace`), copied into `buffer` and
/// NUL-terminated. Returns the byte length, or -1 when the pending error has no
/// value or that value has no recorded stack.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_pending_stack(
    context: *mut SlagContext,
    buffer: *mut c_char,
    len: usize,
) -> i64 {
    let Some(context) = context.as_ref() else {
        return -1;
    };
    let stack = {
        let pending = context.pending_error.borrow();
        let Some(value) = pending.as_ref().and_then(|pending| pending.error.value) else {
            return -1;
        };
        let engine = match context.context.try_borrow() {
            Ok(engine) => engine,
            Err(_) => return -1,
        };
        engine.stack_trace_of(&JsValue::from_bits(value.bits()))
    };
    let Some(stack) = stack else {
        return -1;
    };
    let bytes = stack.as_bytes();
    if buffer.is_null() || len == 0 {
        return 0;
    }
    let copy = bytes.len().min(len - 1);
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.cast::<u8>(), copy);
    *buffer.add(copy) = 0;
    copy as i64
}

/// The position a compile recorded for the pending error (`v8::Message`):
/// `out` receives the line and column, and the buffers receive the script's name
/// and the line's text (each NUL-terminated and truncated to its length).
/// Returns 1 when there is a position, 0 when the error has none (a runtime
/// error), and 2 for a bad argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_pending_message(
    context: *mut SlagContext,
    out: *mut SlagMessage,
    name: *mut c_char,
    name_len: usize,
    source_line: *mut c_char,
    source_line_len: usize,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let pending = context.pending_error.borrow();
    let Some(position) = pending
        .as_ref()
        .and_then(|pending| pending.position.as_ref())
    else {
        return 0;
    };
    if !out.is_null() {
        *out = SlagMessage {
            line: i32::try_from(position.line).unwrap_or(i32::MAX),
            column: i32::try_from(position.column).unwrap_or(i32::MAX),
        };
    }
    copy_text(position.name.as_deref(), name, name_len);
    copy_text(Some(&position.line_text), source_line, source_line_len);
    1
}

/// Clear the pending error and its position (`v8::TryCatch`'s destructor).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_clear_error(context: *mut SlagContext) {
    if let Some(context) = context.as_ref() {
        *context.pending_error.borrow_mut() = None;
    }
}

/// The position a [`slag_pending_message`] reports (`v8::Message`): the 1-based
/// line and the 0-based column.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SlagMessage {
    pub line: i32,
    pub column: i32,
}

/// Copy `text` into `buffer` as a NUL-terminated UTF-8 string, truncated to
/// `len`. A missing text writes the empty string.
fn copy_text(text: Option<&str>, buffer: *mut c_char, len: usize) {
    if buffer.is_null() || len == 0 {
        return;
    }
    let bytes = text.unwrap_or_default().as_bytes();
    let copy = bytes.len().min(len - 1);
    // SAFETY: the caller promised `buffer` covers `len` bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.cast::<u8>(), copy);
        *buffer.add(copy) = 0;
    }
}

/// Drain the context's job queues (`v8::Isolate::PerformMicrotaskCheckpoint` and
/// the platform's message pump). Returns 1 when a job was queued, 0 when none
/// was, and -1 with a pending error when a job threw. A pump from inside a
/// callback is a no-op: that operation already holds the context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_drain_jobs(context: *mut SlagContext) -> i32 {
    let Some(context) = context.as_mut() else {
        return 0;
    };
    let queued = match context.context.try_borrow() {
        Ok(engine) => engine.has_pending_jobs(),
        Err(_) => return 0,
    };
    let Ok(mut engine) = context.context.try_borrow_mut() else {
        return 0;
    };
    match engine.run_jobs() {
        Ok(()) => i32::from(queued),
        Err(error) => {
            record(context, error);
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn slag_undefined() -> u64 {
    JsValue::undefined().to_bits()
}

#[unsafe(no_mangle)]
pub extern "C" fn slag_null() -> u64 {
    Value::Null.bits()
}

#[unsafe(no_mangle)]
pub extern "C" fn slag_boolean_new(value: i32) -> u64 {
    JsValue::from_bits(Value::Boolean(value != 0).bits()).to_bits()
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

/// Write a value's UTF-8 rendering, running ToString through the engine when the
/// value needs it (a host's `String::Utf8Value`). Returns the required byte
/// length, or -1 when the conversion threw (a symbol, or an object whose
/// `toString` throws).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_value_to_utf8_context(
    context: *mut SlagContext,
    value: u64,
    buffer: *mut c_char,
    len: usize,
) -> i64 {
    let Some(context) = context.as_mut() else {
        return -1;
    };
    let text = match context.context.try_borrow_mut() {
        Ok(mut engine) => engine.to_string(&JsValue::from_bits(value)),
        // A callback already holds the context, so no host code can run here.
        // The primitive rendering is what a name or source reads anyway.
        Err(_) => Ok(render(&JsValue::from_bits(value))),
    };
    let Ok(text) = text else {
        return -1;
    };
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

/// Which of V8's `Value` predicates the value answers, as a bitmask (the bits
/// are `slag_a0.h`'s `SLAG_VALUE_*`, mirrored from `runtime::embed`).
///
/// The brands (date, regexp, map, set, promise, native error, array buffer)
/// live in the agent, so the ordinary host call reads `context`. Inside a host
/// callback the run that called in still holds the context's cell, so the live
/// window answers instead.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_value_type_mask(context: *mut SlagContext, value: u64) -> u32 {
    let value = JsValue::from_bits(value);
    if let Some(context) = context.as_ref()
        && let Ok(engine) = context.context.try_borrow()
    {
        return engine.value_type_mask(&value);
    }
    slag::value_type_mask(&value)
}

/// Strict equality (spec 7.2.16): `===`, so NaN differs from itself and a
/// number and its string do not agree.
#[unsafe(no_mangle)]
pub extern "C" fn slag_value_strict_equals(left: u64, right: u64) -> i32 {
    i32::from(crux::ops::is_strictly_equal(
        &Value::from_bits(left),
        &Value::from_bits(right),
    ))
}

/// SameValue (spec 7.2.11): `Object.is`, where NaN equals itself and `+0` and
/// `-0` differ.
#[unsafe(no_mangle)]
pub extern "C" fn slag_value_same_value(left: u64, right: u64) -> i32 {
    i32::from(crux::ops::same_value(
        &Value::from_bits(left),
        &Value::from_bits(right),
    ))
}

/// ToBoolean (spec 7.1.2), which needs no agent.
#[unsafe(no_mangle)]
pub extern "C" fn slag_value_boolean(value: u64) -> i32 {
    i32::from(crux::convert::to_boolean(&Value::from_bits(value)))
}

/// `typeof` as a string value (spec 13.5.3), which needs no agent.
#[unsafe(no_mangle)]
pub extern "C" fn slag_value_typeof(value: u64) -> u64 {
    JsValue::string(crux::value::type_of(&Value::from_bits(value))).to_bits()
}

/// IsCallable (spec 7.2.3).
#[unsafe(no_mangle)]
pub extern "C" fn slag_value_is_callable(value: u64) -> i32 {
    i32::from(crux::value::is_callable(&Value::from_bits(value)))
}

/// IsConstructor (spec 7.2.4). The `[[Construct]]` slots live in the agent, so
/// the ordinary host call reads `context`; inside a callback the run holds the
/// cell and the live window answers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_value_is_constructor(context: *mut SlagContext, value: u64) -> i32 {
    let value = JsValue::from_bits(value);
    if let Some(context) = context.as_ref()
        && let Ok(engine) = context.context.try_borrow()
    {
        return i32::from(engine.value_is_constructor(&value));
    }
    i32::from(slag::value_is_constructor(&value))
}

/// ToString as a value (`v8::Value::ToString`), which runs host code, so it needs
/// the context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_value_to_string(
    context: *mut SlagContext,
    value: u64,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let converted = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.to_string_value(&JsValue::from_bits(value))
    };
    match converted {
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

/// ToNumber (`v8::Value::ToNumber`), which may run `valueOf`/`toString`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_value_to_number_context(
    context: *mut SlagContext,
    value: u64,
    out: *mut f64,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let converted = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.to_number(&JsValue::from_bits(value))
    };
    match converted {
        Ok(number) => {
            if !out.is_null() {
                *out = number;
            }
            0
        }
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// ToObject (`v8::Value::ToObject`), which boxes a primitive.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_value_to_object(
    context: *mut SlagContext,
    value: u64,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let converted = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.to_object(&JsValue::from_bits(value))
    };
    match converted {
        Ok(object) => {
            if !out.is_null() {
                *out = object.as_value().to_bits();
            }
            0
        }
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// A built-in error object (`v8::Exception::Error` and its typed siblings).
/// `kind` is a `SlagExceptionKind` number; the value carries the engine's own
/// `name`, `message`, and stack, so a script catches what V8 would have made.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_exception_new(
    context: *mut SlagContext,
    kind: i32,
    text_ptr: *const u8,
    len: usize,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let Some(kind) = error_kind(kind) else {
        return 2;
    };
    let Ok(message) = text(text_ptr, len) else {
        return 2;
    };
    let created = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.error_value(kind, message)
    };
    match created {
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

/// Throw `value` (`Isolate::ThrowException`): the pending exception becomes
/// exactly that language value, so `TryCatch::Exception` hands it back.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_throw_exception(context: *mut SlagContext, value: u64) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    record(
        context,
        JsError::new(ErrorKind::Error, String::new()).with_value(Value::from_bits(value)),
    );
    0
}

fn error_kind(kind: i32) -> Option<ErrorKind> {
    Some(match kind {
        0 => ErrorKind::Error,
        1 => ErrorKind::EvalError,
        2 => ErrorKind::RangeError,
        3 => ErrorKind::ReferenceError,
        4 => ErrorKind::SyntaxError,
        5 => ErrorKind::TypeError,
        6 => ErrorKind::UriError,
        _ => return None,
    })
}

/// A symbol with `description` (`v8::Private::New` and `v8::Private::ForApi`).
/// The isolate-facing dedupe is the facade's, not the engine's.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_symbol_new(text_ptr: *const u8, len: usize, out: *mut u64) -> i32 {
    let Ok(description) = text(text_ptr, len) else {
        return 2;
    };
    let symbol = crux::symbol::Symbol::new(Some(JsString::from_utf8(description)));
    if !out.is_null() {
        *out = Value::Symbol(Handle::new(symbol)).bits();
    }
    0
}

/// A string from Latin-1 bytes (`v8::String::NewFromOneByte`): each byte is one
/// code unit, so 0xE9 is U+00E9.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_string_new_one_byte(text_ptr: *const u8, len: usize) -> u64 {
    let utf16: Vec<u16> = bytes(text_ptr, len)
        .iter()
        .map(|&byte| u16::from(byte))
        .collect();
    Value::String(Handle::new(JsString::from_utf16(&utf16))).bits()
}

/// A string from UTF-16 code units (`v8::String::NewExternalTwoByte` over an
/// internal copy).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_string_new_two_byte(units: *const u16, len: usize) -> u64 {
    let units = slice_of::<u16>(units, len);
    Value::String(Handle::new(JsString::from_utf16(units))).bits()
}

/// The string's UTF-16 length, or -1 for a non-string (`v8::String::Length`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_string_length(value: u64) -> i64 {
    match Value::from_bits(value).as_string() {
        Some(text) => text.as_slice().len() as i64,
        None => -1,
    }
}

/// Whether every code unit fits in one byte
/// (`v8::String::ContainsOnlyOneByte`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_string_contains_only_one_byte(value: u64) -> i32 {
    match Value::from_bits(value).as_string() {
        Some(text) => i32::from(
            text.as_slice()
                .iter()
                .all(|&unit| unit <= u16::from(u8::MAX)),
        ),
        None => 0,
    }
}

/// The code units a `v8::String::Write` call addresses: `start` is a code unit
/// index, `length` a code unit count, and a negative length means the rest of
/// the string. A start past the end is an empty window, not an error.
fn code_unit_window(units: &[u16], start: i64, length: i64) -> &[u16] {
    let start = if start < 0 {
        0
    } else {
        (start as usize).min(units.len())
    };
    let rest = &units[start..];
    if length < 0 {
        rest
    } else {
        &rest[..rest.len().min(length as usize)]
    }
}

/// Copy a window of code units into `out` (`v8::String::Write`). Answers the
/// number copied, or -1 for a non-string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_string_code_units(
    value: u64,
    start: i64,
    length: i64,
    out: *mut u16,
) -> i64 {
    let Some(text) = Value::from_bits(value).as_string() else {
        return -1;
    };
    let units = code_unit_window(text.as_slice(), start, length);
    if !out.is_null() {
        std::ptr::copy_nonoverlapping(units.as_ptr(), out, units.len());
    }
    units.len() as i64
}

/// Copy a window of code units as one byte each
/// (`v8::String::WriteOneByte`), the form a host uses when it knows the string
/// is Latin-1.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_string_write_one_byte(
    value: u64,
    start: i64,
    length: i64,
    out: *mut u8,
) -> i64 {
    let Some(text) = Value::from_bits(value).as_string() else {
        return -1;
    };
    let units = code_unit_window(text.as_slice(), start, length);
    if !out.is_null() {
        for (index, unit) in units.iter().enumerate() {
            *out.add(index) = *unit as u8;
        }
    }
    units.len() as i64
}

/// Whether `unit` continues the surrogate pair `previous` began.
fn is_trail_of_pair(previous: Option<u16>, unit: u16) -> bool {
    previous
        .is_some_and(|lead| (0xD800..0xDC00).contains(&lead) && (0xDC00..0xE000).contains(&unit))
}

/// The UTF-8 byte count of one code unit, given the one before it
/// (`unibrow::Utf8::Length`): the trail of a pair adds the one byte the pair
/// had left to count, and a lone surrogate counts as much as any three-byte
/// character.
fn encoded_length(unit: u16, previous: Option<u16>) -> usize {
    if unit <= 0x7F || is_trail_of_pair(previous, unit) {
        1
    } else if unit <= 0x7FF {
        2
    } else {
        3
    }
}

/// The UTF-8 bytes of a code point. A lone surrogate goes through the
/// three-byte path unchanged, which is the encoding V8 writes for one.
fn utf8_bytes(point: u32) -> ([u8; 4], usize) {
    if point <= 0x7F {
        ([point as u8, 0, 0, 0], 1)
    } else if point <= 0x7FF {
        (
            [0xC0 | (point >> 6) as u8, 0x80 | (point & 0x3F) as u8, 0, 0],
            2,
        )
    } else if point <= 0xFFFF {
        (
            [
                0xE0 | (point >> 12) as u8,
                0x80 | ((point >> 6) & 0x3F) as u8,
                0x80 | (point & 0x3F) as u8,
                0,
            ],
            3,
        )
    } else {
        (
            [
                0xF0 | (point >> 18) as u8,
                0x80 | ((point >> 12) & 0x3F) as u8,
                0x80 | ((point >> 6) & 0x3F) as u8,
                0x80 | (point & 0x3F) as u8,
            ],
            4,
        )
    }
}

/// The code point a surrogate pair encodes.
fn combined_point(lead: u16, trail: u16) -> u32 {
    0x1_0000 + ((u32::from(lead) - 0xD800) << 10) + (u32::from(trail) - 0xDC00)
}

/// Encode a window of the string as UTF-8 (`v8::String::WriteUtf8`), stopping
/// before a character that does not fit rather than write part of one.
///
/// Without `replace_invalid` a lone surrogate is written as the three bytes of
/// its own code point, which is the encoding V8 writes for one and is not valid
/// UTF-8. `null_terminate` needs room for the terminator inside `capacity`; the
/// answer counts it, which is what V8's byte count includes. `written` receives
/// the number of code units covered, which is V8's `nchars_ref`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_string_write_utf8(
    value: u64,
    start: i64,
    capacity: i64,
    replace_invalid: i32,
    null_terminate: i32,
    out: *mut u8,
    written: *mut i64,
) -> i64 {
    let Some(text) = Value::from_bits(value).as_string() else {
        return -1;
    };
    let units = code_unit_window(text.as_slice(), start, -1);
    let terminate = null_terminate != 0;
    let content_capacity = if capacity < 0 {
        usize::MAX
    } else {
        (capacity as usize).saturating_sub(usize::from(terminate))
    };
    let mut encoded: Vec<u8> = Vec::new();
    let mut read = 0;
    let mut previous: Option<u16> = None;
    while read < units.len() {
        let unit = units[read];
        if content_capacity - encoded.len() < encoded_length(unit, previous) {
            if is_trail_of_pair(previous, unit) {
                // Half a pair fits and the whole one does not: give back the
                // bytes the lead reserved and leave the trail unread.
                encoded.truncate(encoded.len() - 3);
                read -= 1;
            }
            break;
        }
        if units
            .get(read + 1)
            .is_some_and(|&next| is_trail_of_pair(Some(unit), next))
        {
            encoded.extend_from_slice(&[0, 0, 0]);
        } else if let Some(lead) = previous.filter(|&lead| is_trail_of_pair(Some(lead), unit)) {
            let (bytes, _) = utf8_bytes(combined_point(lead, unit));
            encoded.resize(encoded.len() + 1, 0);
            let at = encoded.len() - 4;
            encoded[at..at + 4].copy_from_slice(&bytes);
        } else {
            let point = if replace_invalid != 0 && (0xD800..0xE000).contains(&unit) {
                0xFFFD
            } else {
                u32::from(unit)
            };
            let (bytes, length) = utf8_bytes(point);
            encoded.extend_from_slice(&bytes[..length]);
        }
        previous = Some(unit);
        read += 1;
    }
    if terminate && capacity != 0 {
        encoded.push(0);
    }
    if !out.is_null() {
        std::ptr::copy_nonoverlapping(encoded.as_ptr(), out, encoded.len());
    }
    if !written.is_null() {
        *written = read as i64;
    }
    encoded.len() as i64
}

/// The byte count of the UTF-8 encoding (`v8::String::Utf8Length`); a lone
/// surrogate counts as the three bytes the encoder writes for it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_string_utf8_length(value: u64) -> i64 {
    let Some(text) = Value::from_bits(value).as_string() else {
        return -1;
    };
    let mut total = 0usize;
    let mut previous = None;
    for &unit in text.as_slice() {
        total += encoded_length(unit, previous);
        previous = Some(unit);
    }
    total as i64
}

/// ToNumber for a conversion: the engine when it is free to borrow, and the
/// running callback's window when the call came from inside a host callback
/// (the engine is borrowed there, so only the window can run it). `None` means
/// neither route is available and the caller records the re-entry refusal.
unsafe fn conversion_number(
    context: &SlagContext,
    token: *const c_void,
    value: u64,
) -> Option<Result<f64, JsError>> {
    let value = JsValue::from_bits(value);
    if let Ok(mut engine) = context.context.try_borrow_mut() {
        return Some(engine.to_number(&value));
    }
    CallbackWindow::admit(token).map(|window| window.to_number(value))
}

unsafe fn convert_to_int32(
    context: *mut SlagContext,
    token: *const c_void,
    value: u64,
    out: *mut i32,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Some(number) = conversion_number(context, token, value) else {
        reentrant(context);
        return 1;
    };
    match number {
        Ok(number) => {
            if !out.is_null() {
                *out = crux::convert::to_int32(number);
            }
            0
        }
        Err(error) => {
            record(context, error);
            1
        }
    }
}

unsafe fn convert_to_uint32(
    context: *mut SlagContext,
    token: *const c_void,
    value: u64,
    out: *mut u32,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Some(number) = conversion_number(context, token, value) else {
        reentrant(context);
        return 1;
    };
    match number {
        Ok(number) => {
            if !out.is_null() {
                *out = crux::convert::to_uint32(number);
            }
            0
        }
        Err(error) => {
            record(context, error);
            1
        }
    }
}

unsafe fn convert_to_integer(
    context: *mut SlagContext,
    token: *const c_void,
    value: u64,
    out: *mut i64,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Some(number) = conversion_number(context, token, value) else {
        reentrant(context);
        return 1;
    };
    match number {
        Ok(number) => {
            if !out.is_null() {
                let integer = crux::convert::to_integer_or_infinity(number);
                *out = integer.clamp(i64::MIN as f64, i64::MAX as f64) as i64;
            }
            0
        }
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// ToInt32 with ToNumber first (`v8::Value::Int32Value`), so a fractional or
/// out-of-range number wraps rather than failing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_value_to_int32(
    context: *mut SlagContext,
    value: u64,
    out: *mut i32,
) -> i32 {
    convert_to_int32(context, std::ptr::null(), value, out)
}

/// [`slag_value_to_int32`] inside a running callback (`v8::Value::Int32Value` in
/// a host function). Returns 2 when `token` names no running callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_callback_value_to_int32(
    context: *mut SlagContext,
    token: *const c_void,
    value: u64,
    out: *mut i32,
) -> i32 {
    if CallbackWindow::admit(token).is_none() {
        return 2;
    }
    convert_to_int32(context, token, value, out)
}

/// ToUint32 with ToNumber first (`v8::Value::Uint32Value`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_value_to_uint32(
    context: *mut SlagContext,
    value: u64,
    out: *mut u32,
) -> i32 {
    convert_to_uint32(context, std::ptr::null(), value, out)
}

/// [`slag_value_to_uint32`] inside a running callback. Returns 2 when `token`
/// names no running callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_callback_value_to_uint32(
    context: *mut SlagContext,
    token: *const c_void,
    value: u64,
    out: *mut u32,
) -> i32 {
    if CallbackWindow::admit(token).is_none() {
        return 2;
    }
    convert_to_uint32(context, token, value, out)
}

/// ToIntegerOrInfinity with ToNumber first (`v8::Value::IntegerValue`). The
/// infinities saturate at the `i64` ends, which is the width V8's `Maybe`
/// answers with.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_value_to_integer(
    context: *mut SlagContext,
    value: u64,
    out: *mut i64,
) -> i32 {
    convert_to_integer(context, std::ptr::null(), value, out)
}

/// [`slag_value_to_integer`] inside a running callback. Returns 2 when `token`
/// names no running callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_callback_value_to_integer(
    context: *mut SlagContext,
    token: *const c_void,
    value: u64,
    out: *mut i64,
) -> i32 {
    if CallbackWindow::admit(token).is_none() {
        return 2;
    }
    convert_to_integer(context, token, value, out)
}

/// The promise operations, on whichever route a call takes: the ordinary engine
/// borrow, or the running callback's window (a host callback holds the engine
/// mutably, so only its window can run one).
trait PromiseOps {
    fn capability(&mut self) -> Result<PromiseHandles, JsError>;
    fn then(
        &mut self,
        promise: JsValue,
        on_fulfilled: JsValue,
        on_rejected: JsValue,
    ) -> Result<JsValue, JsError>;
    fn settle(&mut self, function: JsValue, value: JsValue) -> Result<bool, JsError>;
    fn resolved(&mut self, value: JsValue) -> Result<JsValue, JsError>;
    fn rejected(&mut self, reason: JsValue) -> Result<JsValue, JsError>;
    fn state(&mut self, promise: JsValue) -> Result<PromiseState, JsError>;
    fn result(&mut self, promise: JsValue) -> Result<JsValue, JsError>;
    fn mark_handled(&mut self, promise: JsValue) -> Result<(), JsError>;
}

impl PromiseOps for Context {
    fn capability(&mut self) -> Result<PromiseHandles, JsError> {
        Context::promise_capability(self)
    }

    fn then(
        &mut self,
        promise: JsValue,
        on_fulfilled: JsValue,
        on_rejected: JsValue,
    ) -> Result<JsValue, JsError> {
        Context::promise_then(self, promise, on_fulfilled, on_rejected)
    }

    fn settle(&mut self, function: JsValue, value: JsValue) -> Result<bool, JsError> {
        Context::promise_settle(self, function, value)
    }

    fn resolved(&mut self, value: JsValue) -> Result<JsValue, JsError> {
        Context::promise_resolved(self, value)
    }

    fn rejected(&mut self, reason: JsValue) -> Result<JsValue, JsError> {
        Context::promise_rejected(self, reason)
    }

    fn state(&mut self, promise: JsValue) -> Result<PromiseState, JsError> {
        Context::promise_state(self, promise)
    }

    fn result(&mut self, promise: JsValue) -> Result<JsValue, JsError> {
        Context::promise_result(self, promise)
    }

    fn mark_handled(&mut self, promise: JsValue) -> Result<(), JsError> {
        Context::promise_mark_handled(self, promise)
    }
}

impl PromiseOps for CallbackWindow {
    fn capability(&mut self) -> Result<PromiseHandles, JsError> {
        CallbackWindow::promise_capability(self)
    }

    fn then(
        &mut self,
        promise: JsValue,
        on_fulfilled: JsValue,
        on_rejected: JsValue,
    ) -> Result<JsValue, JsError> {
        CallbackWindow::promise_then(self, promise, on_fulfilled, on_rejected)
    }

    fn settle(&mut self, function: JsValue, value: JsValue) -> Result<bool, JsError> {
        CallbackWindow::promise_settle(self, function, value)
    }

    fn resolved(&mut self, value: JsValue) -> Result<JsValue, JsError> {
        CallbackWindow::promise_resolved(self, value)
    }

    fn rejected(&mut self, reason: JsValue) -> Result<JsValue, JsError> {
        CallbackWindow::promise_rejected(self, reason)
    }

    fn state(&mut self, promise: JsValue) -> Result<PromiseState, JsError> {
        CallbackWindow::promise_state(self, promise)
    }

    fn result(&mut self, promise: JsValue) -> Result<JsValue, JsError> {
        CallbackWindow::promise_result(self, promise)
    }

    fn mark_handled(&mut self, promise: JsValue) -> Result<(), JsError> {
        CallbackWindow::promise_mark_handled(self, promise)
    }
}

/// Run one promise operation on the route `token` names: a running callback's
/// window when there is one, the engine otherwise. `Err` is an ABI status, with
/// the error already recorded.
unsafe fn promise_call<T>(
    context: *mut SlagContext,
    token: *const c_void,
    body: impl FnOnce(&mut dyn PromiseOps) -> Result<T, JsError>,
) -> Result<T, i32> {
    let Some(current) = context.as_ref() else {
        return Err(2);
    };
    if token.is_null() {
        let Ok(mut engine) = current.context.try_borrow_mut() else {
            reentrant(current);
            return Err(1);
        };
        body(&mut *engine).map_err(|error| {
            record(current, error);
            1
        })
    } else {
        let Some(mut window) = CallbackWindow::admit(token) else {
            return Err(2);
        };
        body(&mut window).map_err(|error| {
            record(current, error);
            1
        })
    }
}

/// The identity of a JS function value, which is what a resolver is keyed by.
fn function_id(bits: u64) -> Option<u64> {
    Value::from_bits(bits)
        .as_function()
        .map(|function| function.id())
}

unsafe fn new_resolver(
    context: *mut SlagContext,
    token: *const c_void,
    out_resolver: *mut u64,
    out_promise: *mut u64,
) -> i32 {
    let handles = match promise_call(context, token, |ops| ops.capability()) {
        Ok(handles) => handles,
        Err(status) => return status,
    };
    let promise = handles.promise.to_bits();
    let resolve = handles.resolve.to_bits();
    let reject = handles.reject.to_bits();
    if let Some(id) = function_id(resolve) {
        slag_root_retain(promise);
        slag_root_retain(resolve);
        slag_root_retain(reject);
        (*context)
            .resolvers
            .borrow_mut()
            .insert(id, Resolver { promise, reject });
    }
    if !out_resolver.is_null() {
        *out_resolver = resolve;
    }
    if !out_promise.is_null() {
        *out_promise = promise;
    }
    0
}

unsafe fn resolver_settle(
    context: *mut SlagContext,
    token: *const c_void,
    resolver: u64,
    value: u64,
    reject: i32,
    settled: *mut i32,
) -> i32 {
    let Some(current) = context.as_ref() else {
        return 2;
    };
    let Some(id) = function_id(resolver) else {
        return 2;
    };
    let function = match current.resolvers.borrow().get(&id) {
        Some(entry) if reject != 0 => entry.reject,
        Some(_) => resolver,
        None => return 2,
    };
    if function == 0 {
        // The pair has already fired, so neither function can run again.
        if !settled.is_null() {
            *settled = 0;
        }
        return 0;
    }
    match promise_call(context, token, |ops| {
        ops.settle(JsValue::from_bits(function), JsValue::from_bits(value))
    }) {
        Ok(answer) => {
            if answer {
                let released = (*context)
                    .resolvers
                    .borrow_mut()
                    .get_mut(&id)
                    .map(|entry| std::mem::replace(&mut entry.reject, 0))
                    .unwrap_or(0);
                if released != 0 {
                    slag_root_release(released);
                }
            }
            if !settled.is_null() {
                *settled = i32::from(answer);
            }
            0
        }
        Err(status) => status,
    }
}

unsafe fn promise_state_of(context: *mut SlagContext, token: *const c_void, promise: u64) -> i32 {
    match promise_call(context, token, |ops| ops.state(JsValue::from_bits(promise))) {
        Ok(PromiseState::Pending) => 0,
        Ok(PromiseState::Fulfilled) => 1,
        Ok(PromiseState::Rejected) => 2,
        Err(status) => -status,
    }
}

unsafe fn promise_result_of(
    context: *mut SlagContext,
    token: *const c_void,
    promise: u64,
    out: *mut u64,
) -> i32 {
    match promise_call(context, token, |ops| {
        ops.result(JsValue::from_bits(promise))
    }) {
        Ok(value) => {
            if !out.is_null() {
                *out = value.to_bits();
            }
            0
        }
        Err(status) => status,
    }
}

unsafe fn promise_mark(context: *mut SlagContext, token: *const c_void, promise: u64) -> i32 {
    match promise_call(context, token, |ops| {
        ops.mark_handled(JsValue::from_bits(promise))
    }) {
        Ok(()) => 0,
        Err(status) => status,
    }
}

unsafe fn promise_then_of(
    context: *mut SlagContext,
    token: *const c_void,
    promise: u64,
    on_fulfilled: u64,
    on_rejected: u64,
    out: *mut u64,
) -> i32 {
    let derived = match promise_call(context, token, |ops| {
        ops.then(
            JsValue::from_bits(promise),
            JsValue::from_bits(on_fulfilled),
            JsValue::from_bits(on_rejected),
        )
    }) {
        Ok(derived) => derived,
        Err(status) => return status,
    };
    if !out.is_null() {
        *out = derived.to_bits();
    }
    0
}

/// `Promise.resolve(value)` (`v8::Promise::Resolve`): the value itself when it is
/// already a promise of this realm, and a new fulfilled promise otherwise.
unsafe fn promise_resolved_of(
    context: *mut SlagContext,
    token: *const c_void,
    value: u64,
    out: *mut u64,
) -> i32 {
    match promise_call(context, token, |ops| {
        ops.resolved(JsValue::from_bits(value))
    }) {
        Ok(promise) => {
            if !out.is_null() {
                *out = promise.to_bits();
            }
            0
        }
        Err(status) => status,
    }
}

/// `Promise.reject(reason)` (`v8::Promise::Reject`): a new rejected promise.
unsafe fn promise_rejected_of(
    context: *mut SlagContext,
    token: *const c_void,
    reason: u64,
    out: *mut u64,
) -> i32 {
    match promise_call(context, token, |ops| {
        ops.rejected(JsValue::from_bits(reason))
    }) {
        Ok(promise) => {
            if !out.is_null() {
                *out = promise.to_bits();
            }
            0
        }
        Err(status) => status,
    }
}

/// `v8::Promise::Resolve`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_promise_resolved(
    context: *mut SlagContext,
    value: u64,
    out: *mut u64,
) -> i32 {
    promise_resolved_of(context, std::ptr::null(), value, out)
}

/// [`slag_promise_resolved`] inside a running callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_callback_promise_resolved(
    context: *mut SlagContext,
    token: *const c_void,
    value: u64,
    out: *mut u64,
) -> i32 {
    promise_resolved_of(context, token, value, out)
}

/// `v8::Promise::Reject`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_promise_rejected(
    context: *mut SlagContext,
    reason: u64,
    out: *mut u64,
) -> i32 {
    promise_rejected_of(context, std::ptr::null(), reason, out)
}

/// [`slag_promise_rejected`] inside a running callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_callback_promise_rejected(
    context: *mut SlagContext,
    token: *const c_void,
    reason: u64,
    out: *mut u64,
) -> i32 {
    promise_rejected_of(context, token, reason, out)
}

/// `v8::Promise::Resolver::New`: the pair's resolving function (which is the
/// resolver handle) and the promise it settles.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_promise_new_resolver(
    context: *mut SlagContext,
    out_resolver: *mut u64,
    out_promise: *mut u64,
) -> i32 {
    new_resolver(context, std::ptr::null(), out_resolver, out_promise)
}

/// [`slag_promise_new_resolver`] inside a running callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_callback_promise_new_resolver(
    context: *mut SlagContext,
    token: *const c_void,
    out_resolver: *mut u64,
    out_promise: *mut u64,
) -> i32 {
    new_resolver(context, token, out_resolver, out_promise)
}

/// The promise a resolver settles (`v8::Promise::Resolver::GetPromise`). Returns
/// 2 when the value is not a resolver this facade minted.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_promise_resolver_promise(
    context: *mut SlagContext,
    resolver: u64,
    out: *mut u64,
) -> i32 {
    let Some(current) = context.as_ref() else {
        return 2;
    };
    let Some(id) = function_id(resolver) else {
        return 2;
    };
    let promise = match current.resolvers.borrow().get(&id) {
        Some(entry) => entry.promise,
        None => return 2,
    };
    if !out.is_null() {
        *out = promise;
    }
    0
}

/// `v8::Promise::Resolver::Resolve` (`reject` 0) or `Reject` (`reject` 1),
/// answering through `settled` whether this call settled the promise: a pair
/// that already fired is a no-op, which is what V8's answer reports.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_promise_resolver_settle(
    context: *mut SlagContext,
    resolver: u64,
    value: u64,
    reject: i32,
    settled: *mut i32,
) -> i32 {
    resolver_settle(context, std::ptr::null(), resolver, value, reject, settled)
}

/// [`slag_promise_resolver_settle`] inside a running callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_callback_promise_resolver_settle(
    context: *mut SlagContext,
    token: *const c_void,
    resolver: u64,
    value: u64,
    reject: i32,
    settled: *mut i32,
) -> i32 {
    resolver_settle(context, token, resolver, value, reject, settled)
}

/// A promise's state (`v8::Promise::State`): 0 pending, 1 fulfilled, 2 rejected,
/// -1 when the value is not a promise (with a pending error), -2 for a bad
/// argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_promise_state(context: *mut SlagContext, promise: u64) -> i32 {
    promise_state_of(context, std::ptr::null(), promise)
}

/// [`slag_promise_state`] inside a running callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_callback_promise_state(
    context: *mut SlagContext,
    token: *const c_void,
    promise: u64,
) -> i32 {
    promise_state_of(context, token, promise)
}

/// A promise's result (`v8::Promise::Result`), *undefined* while pending.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_promise_result(
    context: *mut SlagContext,
    promise: u64,
    out: *mut u64,
) -> i32 {
    promise_result_of(context, std::ptr::null(), promise, out)
}

/// [`slag_promise_result`] inside a running callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_callback_promise_result(
    context: *mut SlagContext,
    token: *const c_void,
    promise: u64,
    out: *mut u64,
) -> i32 {
    promise_result_of(context, token, promise, out)
}

/// `v8::Promise::MarkAsHandled`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_promise_mark_handled(context: *mut SlagContext, promise: u64) -> i32 {
    promise_mark(context, std::ptr::null(), promise)
}

/// [`slag_promise_mark_handled`] inside a running callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_callback_promise_mark_handled(
    context: *mut SlagContext,
    token: *const c_void,
    promise: u64,
) -> i32 {
    promise_mark(context, token, promise)
}

/// The derived promise of `promise.then(on_fulfilled, on_rejected)`
/// (`v8::Promise::Then2`). A side the host did not give is *undefined*, which the
/// engine reads as "no handler".
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_promise_then(
    context: *mut SlagContext,
    promise: u64,
    on_fulfilled: u64,
    on_rejected: u64,
    out: *mut u64,
) -> i32 {
    promise_then_of(
        context,
        std::ptr::null(),
        promise,
        on_fulfilled,
        on_rejected,
        out,
    )
}

/// [`slag_promise_then`] inside a running callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_callback_promise_then(
    context: *mut SlagContext,
    token: *const c_void,
    promise: u64,
    on_fulfilled: u64,
    on_rejected: u64,
    out: *mut u64,
) -> i32 {
    promise_then_of(context, token, promise, on_fulfilled, on_rejected, out)
}

/// Run one container operation on the engine, recording its error. The engine is
/// borrowed outright: a `Map`/`Set` call runs the engine's own intrinsic method,
/// which a host callback cannot do without the window route (the facade's
/// container surface therefore refuses a re-entry rather than risking it).
unsafe fn container_call<T>(
    context: *mut SlagContext,
    body: impl FnOnce(&mut Context) -> Result<T, JsError>,
) -> Result<T, i32> {
    let Some(current) = context.as_ref() else {
        return Err(2);
    };
    let Ok(mut engine) = current.context.try_borrow_mut() else {
        reentrant(current);
        return Err(1);
    };
    body(&mut engine).map_err(|error| {
        record(current, error);
        1
    })
}

unsafe fn container_value(
    context: *mut SlagContext,
    out: *mut u64,
    body: impl FnOnce(&mut Context) -> Result<JsValue, JsError>,
) -> i32 {
    match container_call(context, body) {
        Ok(value) => {
            if !out.is_null() {
                *out = value.to_bits();
            }
            0
        }
        Err(status) => status,
    }
}

unsafe fn container_flag(
    context: *mut SlagContext,
    out: *mut i32,
    body: impl FnOnce(&mut Context) -> Result<bool, JsError>,
) -> i32 {
    match container_call(context, body) {
        Ok(flag) => {
            if !out.is_null() {
                *out = i32::from(flag);
            }
            0
        }
        Err(status) => status,
    }
}

/// A new `Map` (`v8::Map::New`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_map_new(context: *mut SlagContext, out: *mut u64) -> i32 {
    container_value(context, out, |engine| engine.new_map())
}

/// `map.set(key, value)` through the engine's own intrinsic (`v8::Map::Set`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_map_set(
    context: *mut SlagContext,
    map: u64,
    key: u64,
    value: u64,
) -> i32 {
    match container_call(context, |engine| {
        engine.map_set(
            JsValue::from_bits(map),
            JsValue::from_bits(key),
            JsValue::from_bits(value),
        )
    }) {
        Ok(()) => 0,
        Err(status) => status,
    }
}

/// `map.get(key)` (`v8::Map::Get`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_map_get(
    context: *mut SlagContext,
    map: u64,
    key: u64,
    out: *mut u64,
) -> i32 {
    container_value(context, out, |engine| {
        engine.map_get(JsValue::from_bits(map), JsValue::from_bits(key))
    })
}

/// `map.has(key)` (`v8::Map::Has`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_map_has(
    context: *mut SlagContext,
    map: u64,
    key: u64,
    out: *mut i32,
) -> i32 {
    container_flag(context, out, |engine| {
        engine.map_has(JsValue::from_bits(map), JsValue::from_bits(key))
    })
}

/// `map.delete(key)` (`v8::Map::Delete`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_map_delete(
    context: *mut SlagContext,
    map: u64,
    key: u64,
    out: *mut i32,
) -> i32 {
    container_flag(context, out, |engine| {
        engine.map_delete(JsValue::from_bits(map), JsValue::from_bits(key))
    })
}

/// `map.clear()` (`v8::Map::Clear`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_map_clear(context: *mut SlagContext, map: u64) -> i32 {
    match container_call(context, |engine| engine.map_clear(JsValue::from_bits(map))) {
        Ok(()) => 0,
        Err(status) => status,
    }
}

/// A `DataView` over `buffer` (`v8::DataView::New`): the engine validates the
/// geometry, so a range past the end leaves the RangeError pending.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_data_view_new(
    context: *mut SlagContext,
    buffer: u64,
    byte_offset: usize,
    byte_length: usize,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let created = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.create_data_view(JsValue::from_bits(buffer), byte_offset, byte_length)
    };
    match created {
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

/// The map's live entry count (`v8::Map::Size`), or a negated status when the
/// value is not a map.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_map_size(context: *mut SlagContext, map: u64) -> i32 {
    match container_call(context, |engine| engine.map_size(JsValue::from_bits(map))) {
        Ok(size) => i32::try_from(size).unwrap_or(i32::MAX),
        Err(status) => -status,
    }
}

/// The map's entries flattened to `[key, value, ...]` (`v8::Map::AsArray`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_map_as_array(
    context: *mut SlagContext,
    map: u64,
    out: *mut u64,
) -> i32 {
    container_value(context, out, |engine| {
        engine.map_as_array(JsValue::from_bits(map))
    })
}

/// A new `Set` (`v8::Set::New`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_set_new(context: *mut SlagContext, out: *mut u64) -> i32 {
    container_value(context, out, |engine| engine.new_set())
}

/// `set.add(value)` through the engine's own intrinsic (`v8::Set::Add`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_set_add(context: *mut SlagContext, set: u64, value: u64) -> i32 {
    match container_call(context, |engine| {
        engine.set_add(JsValue::from_bits(set), JsValue::from_bits(value))
    }) {
        Ok(()) => 0,
        Err(status) => status,
    }
}

/// `set.has(value)` (`v8::Set::Has`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_set_has(
    context: *mut SlagContext,
    set: u64,
    value: u64,
    out: *mut i32,
) -> i32 {
    container_flag(context, out, |engine| {
        engine.set_has(JsValue::from_bits(set), JsValue::from_bits(value))
    })
}

/// `set.delete(value)` (`v8::Set::Delete`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_set_delete(
    context: *mut SlagContext,
    set: u64,
    value: u64,
    out: *mut i32,
) -> i32 {
    container_flag(context, out, |engine| {
        engine.set_delete(JsValue::from_bits(set), JsValue::from_bits(value))
    })
}

/// `set.clear()` (`v8::Set::Clear`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_set_clear(context: *mut SlagContext, set: u64) -> i32 {
    match container_call(context, |engine| engine.set_clear(JsValue::from_bits(set))) {
        Ok(()) => 0,
        Err(status) => status,
    }
}

/// The set's live element count (`v8::Set::Size`), or a negated status when the
/// value is not a set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_set_size(context: *mut SlagContext, set: u64) -> i32 {
    match container_call(context, |engine| engine.set_size(JsValue::from_bits(set))) {
        Ok(size) => i32::try_from(size).unwrap_or(i32::MAX),
        Err(status) => -status,
    }
}

/// The set's elements as an array (`v8::Set::AsArray`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_set_as_array(
    context: *mut SlagContext,
    set: u64,
    out: *mut u64,
) -> i32 {
    container_value(context, out, |engine| {
        engine.set_as_array(JsValue::from_bits(set))
    })
}

/// Whether `prototype` is on `value`'s prototype chain (`v8::Signature`'s
/// receiver check): 1 yes, 0 no, a negative status otherwise.
unsafe fn prototype_chain(
    context: *mut SlagContext,
    token: *const c_void,
    value: u64,
    prototype: u64,
) -> i32 {
    let Some(current) = context.as_ref() else {
        return -2;
    };
    let value = JsValue::from_bits(value);
    let prototype = JsValue::from_bits(prototype);
    let answer = if token.is_null() {
        let Ok(engine) = current.context.try_borrow() else {
            reentrant(current);
            return -1;
        };
        engine.has_prototype_in_chain(value, prototype)
    } else {
        let Some(window) = CallbackWindow::admit(token) else {
            return -2;
        };
        window.has_prototype_in_chain(value, prototype)
    };
    match answer {
        Ok(flag) => i32::from(flag),
        Err(error) => {
            record(current, error);
            -1
        }
    }
}

/// Whether `prototype` is on `value`'s prototype chain: the receiver check a
/// `v8::Signature` declares.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_value_has_prototype_in_chain(
    context: *mut SlagContext,
    value: u64,
    prototype: u64,
) -> i32 {
    prototype_chain(context, std::ptr::null(), value, prototype)
}

/// [`slag_value_has_prototype_in_chain`] inside a running callback, where the
/// engine is borrowed and only the window can walk a chain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_callback_value_has_prototype_in_chain(
    context: *mut SlagContext,
    token: *const c_void,
    value: u64,
    prototype: u64,
) -> i32 {
    prototype_chain(context, token, value, prototype)
}

/// Whether the value is a `WebAssembly.Instance` (`v8::WasmInstanceObject`): the
/// engine's per-instance exports record is the brand.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_wasm_is_instance(context: *mut SlagContext, value: u64) -> i32 {
    let Some(context) = context.as_ref() else {
        return 0;
    };
    let Ok(engine) = context.context.try_borrow() else {
        return 0;
    };
    i32::from(
        engine
            .wasm_instance_exports(JsValue::from_bits(value))
            .is_some(),
    )
}

/// Whether the value is a `WebAssembly.Memory` (`v8::WasmMemoryObject`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_wasm_is_memory(context: *mut SlagContext, value: u64) -> i32 {
    let Some(context) = context.as_ref() else {
        return 0;
    };
    let Ok(engine) = context.context.try_borrow() else {
        return 0;
    };
    i32::from(engine.is_wasm_memory(JsValue::from_bits(value)))
}

/// The `index`-th export of an instance when it is a function
/// (`v8::WasmInstanceObject::GetExportedFunction`). Answers 0 with `found` set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_wasm_instance_export(
    context: *mut SlagContext,
    instance: u64,
    index: u32,
    out: *mut u64,
    found: *mut i32,
) -> i32 {
    wasm_export(context, out, found, |engine| {
        engine.wasm_exported_by_index(JsValue::from_bits(instance), index)
    })
}

/// A named export of an instance when it is a function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_wasm_instance_export_named(
    context: *mut SlagContext,
    instance: u64,
    name: u64,
    out: *mut u64,
    found: *mut i32,
) -> i32 {
    wasm_export(context, out, found, |engine| {
        engine.wasm_exported_by_name(JsValue::from_bits(instance), JsValue::from_bits(name))
    })
}

unsafe fn wasm_export(
    context: *mut SlagContext,
    out: *mut u64,
    found: *mut i32,
    body: impl FnOnce(&mut Context) -> Result<Option<JsValue>, JsError>,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let exported = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        body(&mut engine)
    };
    match exported {
        Ok(exported) => {
            if !found.is_null() {
                *found = i32::from(exported.is_some());
            }
            if let (Some(value), false) = (exported, out.is_null()) {
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

/// The engine's function index for a named export of an instance, or -1
/// (`v8::WasmInstanceObject::GetFunctionIndex`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_wasm_instance_function_index(
    context: *mut SlagContext,
    instance: u64,
    name: u64,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return -1;
    };
    let Ok(mut engine) = context.context.try_borrow_mut() else {
        return -1;
    };
    let Ok(Some(exported)) =
        engine.wasm_exported_by_name(JsValue::from_bits(instance), JsValue::from_bits(name))
    else {
        return -1;
    };
    engine.wasm_function_index(exported)
}

/// The first memory an instance exports (`v8::WasmInstanceObject::GetMemoryObject`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_wasm_instance_memory(
    context: *mut SlagContext,
    instance: u64,
    out: *mut u64,
    found: *mut i32,
) -> i32 {
    wasm_export(context, out, found, |engine| {
        engine.wasm_instance_memory(JsValue::from_bits(instance))
    })
}

/// A memory's current buffer (`v8::WasmMemoryObject::Buffer`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_wasm_memory_buffer(
    context: *mut SlagContext,
    memory: u64,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let created = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.wasm_memory_buffer(JsValue::from_bits(memory))
    };
    match created {
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

/// A symbol's description (`v8::Private::Name`), or `undefined` when it has
/// none. Answers 2 when the value is not a symbol.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_symbol_description(value: u64, out: *mut u64) -> i32 {
    let ValueKind::Symbol(symbol) = Value::from_bits(value).kind() else {
        return 2;
    };
    let description = match symbol.description.as_ref() {
        Some(text) => Value::String(Handle::new(text.clone())),
        None => Value::Undefined,
    };
    if !out.is_null() {
        *out = description.bits();
    }
    0
}

/// A plain object with no prototype, which is what an isolate's default
/// security token is (`v8::Context::GetSecurityToken`): opaque, comparable by
/// identity, and shared by every context that has no token of its own.
#[unsafe(no_mangle)]
pub extern "C" fn slag_security_token_new() -> u64 {
    Value::Object(crux::object::JsObject::ordinary_object_create(None)).bits()
}

/// The realm an object was created in, 0 for anything that is not an object (a
/// function answers its object part's realm).
#[unsafe(no_mangle)]
pub extern "C" fn slag_value_realm(value: u64) -> u32 {
    Value::from_bits(value)
        .as_object()
        .map_or(0, |object| object.realm())
}

/// Whether two values were created in the same realm. A value with no realm is
/// never in the same realm as anything, which is the reading a check wants: it
/// has nothing to compare.
#[unsafe(no_mangle)]
pub extern "C" fn slag_value_same_realm(left: u64, right: u64) -> i32 {
    let realm = slag_value_realm(left);
    i32::from(realm != 0 && realm == slag_value_realm(right))
}

/// The realm a context's objects are created in: the comparing half of an
/// access check, which asks a value's realm against the entered context's.
/// Answers 0 for a null context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_context_realm(context: *mut SlagContext) -> u32 {
    context.as_ref().map_or(0, |context| context.realm)
}

/// Make `context`'s realm the one objects created from here on belong to
/// (`v8::Context::Scope`), answering the realm to restore: the pair is what
/// makes a host-side access, one made from C++ between scripts, compare against
/// the context it was made in rather than whichever realm happened to be last.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_context_enter(context: *mut SlagContext) -> u32 {
    match context.as_ref() {
        Some(context) => crux::object::set_current_realm(context.realm),
        None => crux::object::current_realm(),
    }
}

/// Restore the realm [`slag_context_enter`] answered.
#[unsafe(no_mangle)]
pub extern "C" fn slag_context_leave(realm: u32) {
    crux::object::set_current_realm(realm);
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

/// Create an object whose property access goes to `handler`: the object
/// `v8::ObjectTemplate::NewInstance` makes for a template with a handler. The
/// handler's callbacks are copied; its `data` value is pinned for as long as
/// the object can call it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_object_new_handler(
    context: *mut SlagContext,
    count: usize,
    api_wrapper: i32,
    handler: *const SlagPropertyHandler,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Some(handler) = handler.as_ref() else {
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
        engine.create_host_object(Rc::new(CxxHandler::new(
            *handler,
            context.agent,
            Rc::clone(&context.pending_error),
        )))
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

/// The token of the host callback that is running on this thread, or null when
/// there is none. The facade publishes it on `PropertyCallbackInfo`; the
/// allocation entry points below admit only a running token.
#[unsafe(no_mangle)]
pub extern "C" fn slag_callback_token() -> *const c_void {
    CallbackWindow::token()
}

/// An object that is also callable (`ObjectTemplate::NewInstance` for a template
/// with a call handler): the value is a builtin whose body runs `callback`, and
/// its object part carries `count` internal fields.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_object_new_callable(
    context: *mut SlagContext,
    count: usize,
    api_wrapper: i32,
    callback: SlagHostFn,
    data: *mut c_void,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let host = host_fn(context, callback, data);
    let created = {
        let mut engine = match context.context.try_borrow_mut() {
            Ok(engine) => engine,
            Err(_) => {
                reentrant(context);
                return 1;
            }
        };
        engine.create_callable_instance(host)
    };
    let value = match created {
        Ok(value) => value,
        Err(error) => {
            record(context, error);
            return 1;
        }
    };
    let Some(object) = value.as_object() else {
        return 2;
    };
    object.set_internal_fields(count, api_wrapper != 0);
    if !out.is_null() {
        *out = value.to_bits();
    }
    0
}

/// An ordinary object allocated through a running callback (`v8::Object::New`
/// inside a getter, say). Returns 2 when `token` names no running callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_callback_object_new(
    context: *mut SlagContext,
    token: *const c_void,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Some(window) = CallbackWindow::admit(token) else {
        return 2;
    };
    match window.create_object() {
        Ok(object) => {
            if !out.is_null() {
                *out = object.as_value().to_bits();
            }
            0
        }
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// An array allocated at the top level (`v8::Array::New`). A null `values`
/// makes `len` holes; otherwise the `len` values are the elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_array_new(
    context: *mut SlagContext,
    values: *const u64,
    len: usize,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let engine = match context.context.try_borrow() {
        Ok(engine) => engine,
        Err(_) => {
            reentrant(context);
            return 1;
        }
    };
    let created = if values.is_null() {
        engine.create_array_with_length(u32::try_from(len).unwrap_or(u32::MAX))
    } else {
        let values: Vec<JsValue> = slice_of::<u64>(values, len)
            .iter()
            .map(|bits| JsValue::from_bits(*bits))
            .collect();
        engine.create_array(&values)
    };
    match created {
        Ok(array) => {
            if !out.is_null() {
                *out = array.to_bits();
            }
            0
        }
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// The callback form of [`slag_array_new`] (`v8::Array::New` inside a handler).
/// Returns 2 when `token` names no running callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_callback_array_new(
    context: *mut SlagContext,
    token: *const c_void,
    values: *const u64,
    len: usize,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Some(window) = CallbackWindow::admit(token) else {
        return 2;
    };
    let created = if values.is_null() {
        window.create_array_with_length(u32::try_from(len).unwrap_or(u32::MAX))
    } else {
        let values: Vec<JsValue> = slice_of::<u64>(values, len)
            .iter()
            .map(|bits| JsValue::from_bits(*bits))
            .collect();
        window.create_array(&values)
    };
    match created {
        Ok(array) => {
            if !out.is_null() {
                *out = array.to_bits();
            }
            0
        }
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// [[Set]] a property through a running callback (`v8::Object::Set` inside a
/// handler). Returns 2 when `token` names no running callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_callback_object_set_key(
    context: *mut SlagContext,
    token: *const c_void,
    object: u64,
    key: u64,
    value: u64,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Some(window) = CallbackWindow::admit(token) else {
        return 2;
    };
    match window.set_property(
        JsValue::from_bits(object),
        JsValue::from_bits(key),
        JsValue::from_bits(value),
    ) {
        Ok(()) => 0,
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// [[Get]] a property through a running callback (`v8::Object::Get` inside a
/// handler).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_callback_object_get_key(
    context: *mut SlagContext,
    token: *const c_void,
    object: u64,
    key: u64,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Some(window) = CallbackWindow::admit(token) else {
        return 2;
    };
    match window.get_property(JsValue::from_bits(object), JsValue::from_bits(key)) {
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

/// [[HasProperty]] through a running callback (`v8::Object::Has` inside a
/// handler).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_callback_object_has_key(
    context: *mut SlagContext,
    token: *const c_void,
    object: u64,
    key: u64,
    out: *mut i32,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Some(window) = CallbackWindow::admit(token) else {
        return 2;
    };
    match window.has_property(JsValue::from_bits(object), JsValue::from_bits(key)) {
        Ok(found) => {
            if !out.is_null() {
                *out = i32::from(found);
            }
            0
        }
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// [[Delete]] through a running callback (`v8::Object::Delete` inside a
/// handler).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_callback_object_delete_key(
    context: *mut SlagContext,
    token: *const c_void,
    object: u64,
    key: u64,
    out: *mut i32,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Some(window) = CallbackWindow::admit(token) else {
        return 2;
    };
    match window.delete_property(JsValue::from_bits(object), JsValue::from_bits(key)) {
        Ok(deleted) => {
            if !out.is_null() {
                *out = i32::from(deleted);
            }
            0
        }
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// The own property through a running callback (`v8::Object::GetRealNamedProperty`
/// inside a handler).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_callback_object_get_own_key(
    context: *mut SlagContext,
    token: *const c_void,
    object: u64,
    key: u64,
    out: *mut u64,
    found: *mut i32,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Some(window) = CallbackWindow::admit(token) else {
        return 2;
    };
    match window.get_own_property(JsValue::from_bits(object), JsValue::from_bits(key)) {
        Ok(answer) => {
            if !found.is_null() {
                *found = i32::from(answer.is_some());
            }
            if !out.is_null()
                && let Some(value) = answer
            {
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

/// The kind of the own property through a running callback
/// (`v8::Object::HasRealNamedProperty` and its siblings inside a handler).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_callback_object_own_property_kind(
    context: *mut SlagContext,
    token: *const c_void,
    object: u64,
    key: u64,
    out: *mut i32,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Some(window) = CallbackWindow::admit(token) else {
        return 2;
    };
    match window.own_property_kind(JsValue::from_bits(object), JsValue::from_bits(key)) {
        Ok(kind) => {
            if !out.is_null() {
                *out = match kind {
                    None => 0,
                    Some(OwnProperty::DataNamed) => 1,
                    Some(OwnProperty::DataIndexed) => 2,
                    Some(OwnProperty::CallbackNamed) => 3,
                    Some(OwnProperty::CallbackIndexed) => 4,
                };
            }
            0
        }
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// The object's identity hash (`v8::Object::GetIdentityHash`): the engine's id
/// for the object, mixed so neighbouring objects do not collide and never zero.
/// 0 for a non-object.
#[unsafe(no_mangle)]
pub extern "C" fn slag_object_identity_hash(object: u64) -> i32 {
    match JsValue::from_bits(object).as_object() {
        Some(object) => crux::object::identity_hash(object.id()),
        None => 0,
    }
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

/// The property key a name value names: the string or symbol `SetAccessor`
/// passes (`v8::Name`).
fn property_key_of(value: Value) -> Option<PropertyKey> {
    match value.kind() {
        ValueKind::String(text) => Some(PropertyKey::from_js_string(&text)),
        ValueKind::Symbol(symbol) => Some(PropertyKey::Symbol(symbol)),
        _ => None,
    }
}

/// Turn an accessor callback's report into the host function's result: an error
/// the callback recorded fails the access, and so does a report without one.
fn accessor_result(pending: Option<PendingError>, code: i32, message: &str) -> Result<(), JsError> {
    if let Some(pending) = pending {
        return Err(pending.error);
    }
    if code != 0 {
        return Err(JsError::new(ErrorKind::Error, message.into()));
    }
    Ok(())
}

fn accessor_getter(
    pending: Rc<RefCell<Option<PendingError>>>,
    callback: unsafe extern "C" fn(*mut c_void, u64, u64, *mut u64) -> i32,
    data: *mut c_void,
    key: u64,
) -> HostFn {
    Box::new(move |call: &FunctionCall<'_>| {
        // The getter runs as a host function, so the agent is already the
        // dispatch's; publishing it is what lets the callback allocate.
        let _window = call.enter_callback_window();
        let mut answer = JsValue::undefined().to_bits();
        // SAFETY: the callback is the facade's, and the pointers are this
        // invocation's.
        let code = unsafe { callback(data, call.this().to_bits(), key, &mut answer) };
        accessor_result(
            pending.borrow_mut().take(),
            code,
            "the accessor getter failed",
        )?;
        Ok(JsValue::from_bits(answer))
    })
}

fn accessor_setter(
    pending: Rc<RefCell<Option<PendingError>>>,
    callback: unsafe extern "C" fn(*mut c_void, u64, u64, u64) -> i32,
    data: *mut c_void,
    key: u64,
) -> HostFn {
    Box::new(move |call: &FunctionCall<'_>| {
        let _window = call.enter_callback_window();
        let value = call.arg(0).unwrap_or_else(JsValue::undefined).to_bits();
        // SAFETY: as in `accessor_getter`.
        let code = unsafe { callback(data, call.this().to_bits(), key, value) };
        accessor_result(
            pending.borrow_mut().take(),
            code,
            "the accessor setter failed",
        )?;
        Ok(JsValue::undefined())
    })
}

/// Define an accessor property (`v8::ObjectTemplate::SetAccessor` and
/// `v8::Object::SetAccessor`): `key` names it, and the two callbacks run as host
/// functions, so a read or a write reaches the host with the property's key and
/// a working allocation window. `data` is handed back to each callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_object_set_accessor(
    context: *mut SlagContext,
    object: u64,
    key: u64,
    getter: SlagAccessorGetter,
    setter: SlagAccessorSetter,
    data: *mut c_void,
    attributes: i32,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Some(target) = JsValue::from_bits(object).as_object() else {
        return 2;
    };
    let key_value = Value::from_bits(key);
    let Some(property) = property_key_of(key_value) else {
        return 2;
    };
    // A `SetAccessor` always has a getter; a write-only property is not one V8
    // offers.
    if getter.is_none() {
        return 2;
    }
    let label = match key_value.kind() {
        ValueKind::String(text) => text.to_string_lossy(),
        _ => "symbol".to_string(),
    };
    let pending = Rc::clone(&context.pending_error);
    let get = getter.map(|callback| accessor_getter(Rc::clone(&pending), callback, data, key));
    let attributes = u8::try_from(attributes).unwrap_or(0);
    let set = setter.map(|callback| accessor_setter(pending, callback, data, key));
    let engine = match context.context.try_borrow() {
        Ok(engine) => engine,
        Err(_) => {
            reentrant(context);
            return 1;
        }
    };
    match engine.define_accessor_key(&target, &property, &label, get, set, attributes) {
        Ok(()) => 0,
        Err(error) => {
            record(context, error);
            1
        }
    }
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

/// [[HasProperty]] by a key value (`v8::Object::Has`). 0 on success with `out`
/// written, 1 with a pending error, 2 for a bad argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_object_has_key(
    context: *mut SlagContext,
    object: u64,
    key: u64,
    out: *mut i32,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let result = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.has_property(JsValue::from_bits(object), JsValue::from_bits(key))
    };
    match result {
        Ok(found) => {
            if !out.is_null() {
                *out = i32::from(found);
            }
            0
        }
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// [[Delete]] by a key value (`v8::Object::Delete`). 0 on success with `out`
/// written (true once the property is gone), 1 with a pending error, 2 for a
/// bad argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_object_delete_key(
    context: *mut SlagContext,
    object: u64,
    key: u64,
    out: *mut i32,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let result = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.delete_property(JsValue::from_bits(object), JsValue::from_bits(key))
    };
    match result {
        Ok(deleted) => {
            if !out.is_null() {
                *out = i32::from(deleted);
            }
            0
        }
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// The object's own named property (`v8::Object::GetRealNamedProperty`). 0 on
/// success with `out` and `found` written, 1 with a pending error, 2 for a bad
/// argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_object_get_own_key(
    context: *mut SlagContext,
    object: u64,
    key: u64,
    out: *mut u64,
    found: *mut i32,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let result = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.get_own_property(JsValue::from_bits(object), JsValue::from_bits(key))
    };
    match result {
        Ok(value) => {
            if !out.is_null() && !found.is_null() {
                *found = i32::from(value.is_some());
                *out = value.map_or(0, |value| value.to_bits());
            }
            0
        }
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// Which kind of own property `object` has under `key`, for V8's real-property
/// predicates: 0 absent, 1 data named, 2 data indexed, 3 callback named,
/// 4 callback indexed. 0 on success with `out` written, 1 with a pending error,
/// 2 for a bad argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_object_own_property_kind(
    context: *mut SlagContext,
    object: u64,
    key: u64,
    out: *mut i32,
) -> i32 {
    let Some(context) = context.as_mut() else {
        return 2;
    };
    let result = {
        let Ok(mut engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return 1;
        };
        engine.own_property_kind(JsValue::from_bits(object), JsValue::from_bits(key))
    };
    match result {
        Ok(kind) => {
            if !out.is_null() {
                *out = match kind {
                    None => 0,
                    Some(OwnProperty::DataNamed) => 1,
                    Some(OwnProperty::DataIndexed) => 2,
                    Some(OwnProperty::CallbackNamed) => 3,
                    Some(OwnProperty::CallbackIndexed) => 4,
                };
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
            let name = b"broken.js";
            assert_eq!(
                slag_compile(
                    context,
                    bad.as_ptr(),
                    bad.len(),
                    name.as_ptr(),
                    name.len(),
                    &mut script,
                ),
                1
            );
            assert!(script.is_null());
            assert_eq!(slag_has_error(context), 1);

            let mut message = [0i8; 256];
            let len = slag_pending_error(context, message.as_mut_ptr(), message.len());
            assert!(len > 0);
            // Reading the message does not consume it: the TryCatch reads the
            // exception, the message, and the stack of one error.
            assert_eq!(slag_has_error(context), 1);

            // A compile error records a position, so a message can name it.
            let mut position = SlagMessage::default();
            let mut script_name = [0i8; 64];
            let mut source_line = [0i8; 64];
            assert_eq!(
                slag_pending_message(
                    context,
                    &mut position,
                    script_name.as_mut_ptr(),
                    script_name.len(),
                    source_line.as_mut_ptr(),
                    source_line.len(),
                ),
                1
            );
            assert_eq!(position.line, 1);
            assert!(position.column >= 0);
            assert_eq!(
                std::ffi::CStr::from_ptr(script_name.as_ptr())
                    .to_str()
                    .unwrap(),
                "broken.js"
            );
            assert_eq!(
                std::ffi::CStr::from_ptr(source_line.as_ptr())
                    .to_str()
                    .unwrap(),
                "function ("
            );

            slag_clear_error(context);
            assert_eq!(slag_has_error(context), 0);

            let good = b"1 + 2";
            assert_eq!(
                slag_compile(
                    context,
                    good.as_ptr(),
                    good.len(),
                    std::ptr::null(),
                    0,
                    &mut script,
                ),
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

    #[test]
    fn strings_round_trip_through_the_write_forms() {
        unsafe {
            let latin1 = [b'a', b'b', 0xE9];
            let text = slag_string_new_one_byte(latin1.as_ptr(), latin1.len());
            assert_eq!(slag_string_length(text), 3);
            assert_eq!(slag_string_contains_only_one_byte(text), 1);
            assert_eq!(slag_string_utf8_length(text), 4);

            let mut units = [0u16; 8];
            assert_eq!(slag_string_code_units(text, 0, -1, units.as_mut_ptr()), 3);
            assert_eq!(units[..3], [u16::from(b'a'), u16::from(b'b'), 0xE9]);

            let mut window = [0u16; 8];
            assert_eq!(slag_string_code_units(text, 1, 1, window.as_mut_ptr()), 1);
            assert_eq!(window[0], u16::from(b'b'));

            let mut bytes = [0u8; 8];
            assert_eq!(
                slag_string_write_one_byte(text, 0, -1, bytes.as_mut_ptr()),
                3
            );
            assert_eq!(bytes[..3], [b'a', b'b', 0xE9]);

            let pair = [0xD83Du16, 0xDE00];
            let astral = slag_string_new_two_byte(pair.as_ptr(), pair.len());
            assert_eq!(slag_string_length(astral), 2);
            assert_eq!(slag_string_contains_only_one_byte(astral), 0);
            assert_eq!(slag_string_utf8_length(astral), 4);

            let mut utf8 = [0u8; 8];
            let mut covered = 0i64;
            assert_eq!(
                slag_string_write_utf8(astral, 0, 8, 0, 1, utf8.as_mut_ptr(), &mut covered),
                5,
                "four bytes and the terminator"
            );
            assert_eq!(covered, 2, "a pair covers two code units");
            assert_eq!(utf8[..5], [0xF0, 0x9F, 0x98, 0x80, 0]);

            assert_eq!(
                slag_string_write_utf8(astral, 0, 3, 0, 1, utf8.as_mut_ptr(), &mut covered),
                1,
                "only the terminator fits"
            );
            assert_eq!(covered, 0);

            let lone = [0xD800u16, u16::from(b'A')];
            let surrogate = slag_string_new_two_byte(lone.as_ptr(), lone.len());
            assert_eq!(slag_string_utf8_length(surrogate), 4);
            assert_eq!(
                slag_string_write_utf8(surrogate, 0, 8, 0, 0, utf8.as_mut_ptr(), &mut covered),
                4,
                "the surrogate's own three bytes and the A"
            );
            assert_eq!(utf8[..4], [0xED, 0xA0, 0x80, b'A']);
        }
    }

    #[test]
    fn a_promise_resolver_settles_once_through_the_abi() {
        unsafe {
            let isolate = slag_isolate_new();
            let context = slag_context_new(isolate);
            assert!(!context.is_null());

            let mut resolver = 0;
            let mut promise = 0;
            assert_eq!(
                slag_promise_new_resolver(context, &mut resolver, &mut promise),
                0
            );
            assert_ne!(resolver, 0);
            assert_ne!(promise, 0);
            assert_eq!(slag_promise_state(context, promise), 0);

            let mut read_back = 0;
            assert_eq!(
                slag_promise_resolver_promise(context, resolver, &mut read_back),
                0
            );
            assert_eq!(read_back, promise);

            // A pending promise's result is undefined.
            let mut result = 0;
            assert_eq!(slag_promise_result(context, promise, &mut result), 0);
            assert_eq!(slag_value_is_undefined(result), 1);

            let mut settled = -1;
            assert_eq!(
                slag_promise_resolver_settle(
                    context,
                    resolver,
                    slag_number_new(7.0),
                    0,
                    &mut settled
                ),
                0
            );
            assert_eq!(settled, 1, "the first call settles it");
            assert_eq!(slag_promise_state(context, promise), 1);
            assert_eq!(slag_promise_result(context, promise, &mut result), 0);
            assert_eq!(slag_value_to_number(result), 7.0);

            // The pair shares one flag, so neither side settles it again.
            assert_eq!(
                slag_promise_resolver_settle(
                    context,
                    resolver,
                    slag_number_new(8.0),
                    0,
                    &mut settled
                ),
                0
            );
            assert_eq!(settled, 0);
            assert_eq!(
                slag_promise_resolver_settle(
                    context,
                    resolver,
                    slag_number_new(9.0),
                    1,
                    &mut settled
                ),
                0
            );
            assert_eq!(settled, 0);
            assert_eq!(slag_promise_state(context, promise), 1);
            assert_eq!(slag_promise_result(context, promise, &mut result), 0);
            assert_eq!(slag_value_to_number(result), 7.0);

            slag_context_free(context);
            slag_isolate_free(isolate);
        }
    }

    #[test]
    fn a_then_derives_a_promise_and_mark_handled_keeps_a_rejection_quiet() {
        unsafe {
            let isolate = slag_isolate_new();
            let context = slag_context_new(isolate);
            assert!(!context.is_null());

            let source = b"(function (value) { return value * 2; })";
            let mut handler = 0;
            assert_eq!(
                slag_eval(context, source.as_ptr(), source.len(), &mut handler),
                0
            );

            let mut resolver = 0;
            let mut promise = 0;
            assert_eq!(
                slag_promise_new_resolver(context, &mut resolver, &mut promise),
                0
            );
            let mut derived = 0;
            assert_eq!(
                slag_promise_then(context, promise, handler, slag_undefined(), &mut derived),
                0
            );

            let mut settled = 0;
            assert_eq!(
                slag_promise_resolver_settle(
                    context,
                    resolver,
                    slag_number_new(21.0),
                    0,
                    &mut settled
                ),
                0
            );
            assert_eq!(settled, 1);
            assert_ne!(slag_drain_jobs(context), -1);

            let mut result = 0;
            assert_eq!(slag_promise_state(context, derived), 1);
            assert_eq!(slag_promise_result(context, derived, &mut result), 0);
            assert_eq!(slag_value_to_number(result), 42.0);

            // A rejection keeps its reason, and marking it handled succeeds.
            let mut rejected = 0;
            let mut reason = 0;
            assert_eq!(
                slag_promise_new_resolver(context, &mut rejected, &mut reason),
                0
            );
            let mut settled = 0;
            assert_eq!(
                slag_promise_resolver_settle(
                    context,
                    rejected,
                    slag_string_new(b"no".as_ptr(), 2),
                    1,
                    &mut settled
                ),
                0
            );
            assert_eq!(settled, 1);
            assert_eq!(slag_promise_state(context, reason), 2);
            assert_eq!(slag_promise_mark_handled(context, reason), 0);

            // A value that is not a promise is refused with a pending error.
            assert_eq!(slag_promise_state(context, slag_number_new(1.0)), -1);
            let mut message = [0 as std::ffi::c_char; 64];
            assert!(slag_pending_error(context, message.as_mut_ptr(), message.len()) > 0);
            assert!(
                std::ffi::CStr::from_ptr(message.as_ptr())
                    .to_str()
                    .expect("utf8")
                    .contains("promise")
            );
            slag_clear_error(context);

            slag_context_free(context);
            slag_isolate_free(isolate);
        }
    }

    #[test]
    fn a_map_and_set_round_trip_through_the_abi() {
        unsafe {
            let isolate = slag_isolate_new();
            let context = slag_context_new(isolate);
            assert!(!context.is_null());

            let mut map = 0;
            assert_eq!(slag_map_new(context, &mut map), 0);
            assert_eq!(slag_map_size(context, map), 0);

            let key = slag_string_new(b"k".as_ptr(), 1);
            assert_eq!(slag_map_set(context, map, key, slag_number_new(5.0)), 0);
            assert_eq!(slag_map_size(context, map), 1);

            let mut read = 0;
            assert_eq!(slag_map_get(context, map, key, &mut read), 0);
            assert_eq!(slag_value_to_number(read), 5.0);

            let mut flag = 0;
            assert_eq!(slag_map_has(context, map, key, &mut flag), 0);
            assert_eq!(flag, 1);

            // AsArray is the flat key-value form, so the value is at index 1.
            let mut pairs = 0;
            assert_eq!(slag_map_as_array(context, map, &mut pairs), 0);
            let index = slag_number_new(1.0);
            assert_eq!(slag_object_get_key(context, pairs, index, &mut read), 0);
            assert_eq!(slag_value_to_number(read), 5.0);

            assert_eq!(slag_map_delete(context, map, key, &mut flag), 0);
            assert_eq!(flag, 1);
            assert_eq!(slag_map_size(context, map), 0);

            let mut set = 0;
            assert_eq!(slag_set_new(context, &mut set), 0);
            assert_eq!(slag_set_add(context, set, slag_number_new(3.0)), 0);
            assert_eq!(slag_set_size(context, set), 1);
            assert_eq!(
                slag_set_has(context, set, slag_number_new(3.0), &mut flag),
                0
            );
            assert_eq!(flag, 1);
            assert_eq!(slag_set_clear(context, set), 0);
            assert_eq!(slag_set_size(context, set), 0);

            // A value that is not a collection is refused with a pending error.
            assert_eq!(slag_map_size(context, slag_number_new(0.0)), -1);
            assert_eq!(slag_set_size(context, slag_number_new(0.0)), -1);
            slag_clear_error(context);

            slag_context_free(context);
            slag_isolate_free(isolate);
        }
    }

    #[test]
    fn the_integer_conversions_convert_then_wrap() {
        unsafe {
            let isolate = slag_isolate_new();
            let context = slag_context_new(isolate);
            assert!(!context.is_null());

            let mut out = 0i32;
            let also = slag_number_new(4294967297.0);
            assert_eq!(slag_value_to_int32(context, also, &mut out), 0);
            assert_eq!(out, 1);

            let negative = slag_number_new(-1.9);
            assert_eq!(slag_value_to_int32(context, negative, &mut out), 0);
            assert_eq!(out, -1);

            let mut unsigned = 0u32;
            assert_eq!(slag_value_to_uint32(context, negative, &mut unsigned), 0);
            assert_eq!(unsigned, u32::MAX);

            let mut integer = 0i64;
            let fraction = slag_number_new(2.9);
            assert_eq!(slag_value_to_integer(context, fraction, &mut integer), 0);
            assert_eq!(integer, 2);

            let infinity = slag_number_new(f64::INFINITY);
            assert_eq!(slag_value_to_integer(context, infinity, &mut integer), 0);
            assert_eq!(integer, i64::MAX);

            // The conversions go through ToNumber, so the engine's own string
            // parsing is what a host sees.
            let source = b"'0x10'";
            let mut text = 0;
            assert_eq!(
                slag_eval(context, source.as_ptr(), source.len(), &mut text),
                0
            );
            assert_eq!(slag_value_to_int32(context, text, &mut out), 0);
            assert_eq!(out, 16);

            slag_context_free(context);
            slag_isolate_free(isolate);
        }
    }
}
