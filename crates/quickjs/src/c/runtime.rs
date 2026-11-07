//! Runtimes and contexts: the `JSRuntime`/`JSContext` lifecycle over one
//! `runtime::api::Isolate` (the runtime) and its realms (the contexts).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;

use runtime::api::{self, Isolate};

use crate::c::abi;

/// The C `JSRuntime` (opaque; the state lives in the thread-local registry).
#[repr(C)]
pub struct JSRuntime {
    _private: [u8; 0],
}

/// The C `JSContext` (opaque; the state lives in the thread-local registry).
#[repr(C)]
pub struct JSContext {
    _private: [u8; 0],
}

/// The C `JSGCObjectHeader` (opaque; the facade's collector keeps no such
/// header on the host side, and host refs are roots anyway).
#[repr(C)]
pub struct JSGCObjectHeader {
    _private: [u8; 0],
}

/// The C `JS_MarkFunc` (quickjs.h): marks one GC object through the host's
/// own mark walk. The facade never drives it — see [`JS_MarkValue`].
pub type JS_MarkFunc = unsafe extern "C" fn(*mut JSRuntime, *mut JSGCObjectHeader);

/// The runtime's engine state.
pub struct JsRuntime {
    pub isolate: Box<Isolate>,
    pub opaque: *mut c_void,
}

/// A context (realm) on a runtime.
pub struct JsContext {
    pub rt: *mut JSRuntime,
    pub api: api::Context,
    pub opaque: *mut c_void,
    pub retain: Cell<u32>,
}

thread_local! {
    static RUNTIMES: RefCell<HashMap<usize, Box<JsRuntime>>> = RefCell::new(HashMap::new());
    static CONTEXTS: RefCell<HashMap<usize, Box<JsContext>>> = RefCell::new(HashMap::new());
    /// The context the last ctx-bearing C call used. A few QuickJS APIs (e.g.
    /// `JS_IsError`) take no context and read this instead.
    static CURRENT: Cell<*mut JSContext> = const { Cell::new(std::ptr::null_mut()) };
}

/// The context the last ctx-bearing C call used, if any.
pub fn current() -> *mut JSContext {
    CURRENT.with(|current| current.get())
}

/// The api context a `JSContext` names, if it is live; records it as current.
pub fn context_api(ctx: *mut JSContext) -> Option<api::Context> {
    CURRENT.with(|current| current.set(ctx));
    CONTEXTS.with(|contexts| {
        contexts
            .borrow()
            .get(&(ctx as usize))
            .map(|entry| entry.api)
    })
}

/// Install host hooks on the agent behind `rt`.
pub(crate) fn set_host_hooks(rt: *mut JSRuntime, hooks: Box<dyn runtime::host::HostHooks>) {
    RUNTIMES.with(|runtimes| {
        let mut runtimes = runtimes.borrow_mut();
        if let Some(runtime) = runtimes.get_mut(&(rt as usize)) {
            runtime.isolate.agent().host_hooks = Some(hooks);
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewRuntime() -> *mut JSRuntime {
    let runtime = Box::new(JsRuntime {
        isolate: Isolate::new(),
        opaque: std::ptr::null_mut(),
    });
    let ptr = &*runtime as *const JsRuntime as *mut JSRuntime;
    RUNTIMES.with(|runtimes| runtimes.borrow_mut().insert(ptr as usize, runtime));
    ptr
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_FreeRuntime(rt: *mut JSRuntime) {
    RUNTIMES.with(|runtimes| {
        runtimes.borrow_mut().remove(&(rt as usize));
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_RunGC(rt: *mut JSRuntime) {
    // Deliberately not wrapped in `ffi::guard`: the conservative stack scan runs
    // *during* `collect_garbage`, and a `catch_unwind` frame on the stack at that
    // moment leaves stale words the scan misreads as roots, keeping short-lived
    // objects alive across the collection (observed as a finalizer that never
    // fires). The finalizers it drains are C function pointers and do not panic.
    RUNTIMES.with(|runtimes| {
        let mut runtimes = runtimes.borrow_mut();
        if let Some(runtime) = runtimes.get_mut(&(rt as usize)) {
            runtime.isolate.agent().collect_garbage();
            runtime.isolate.run_finalizers();
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_IsJobPending(rt: *mut JSRuntime) -> abi::JS_BOOL {
    RUNTIMES.with(|runtimes| {
        let mut runtimes = runtimes.borrow_mut();
        abi::js_bool(
            runtimes
                .get_mut(&(rt as usize))
                .is_some_and(|runtime| !runtime.isolate.agent().job_queues_empty()),
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_ExecutePendingJob(
    rt: *mut JSRuntime,
    pctx: *mut *mut JSContext,
) -> i32 {
    ffi::guard(|| execute_pending_job_impl(rt, pctx))
}

fn execute_pending_job_impl(rt: *mut JSRuntime, pctx: *mut *mut JSContext) -> i32 {
    // Divergence (gap G5): the engine drains every queue at once, so this runs
    // all pending jobs rather than one.
    let ok = RUNTIMES.with(|runtimes| {
        let mut runtimes = runtimes.borrow_mut();
        match runtimes.get_mut(&(rt as usize)) {
            Some(runtime) => runtime.isolate.agent().run_jobs().is_ok(),
            None => false,
        }
    });
    if !ok {
        return -1;
    }
    if !pctx.is_null() {
        // SAFETY: the caller passes a writable slot or null.
        unsafe { *pctx = current() };
    }
    0
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_GetPendingJobContext(_rt: *mut JSRuntime) -> *mut JSContext {
    current()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewContext(rt: *mut JSRuntime) -> *mut JSContext {
    let api = RUNTIMES.with(|runtimes| {
        let mut runtimes = runtimes.borrow_mut();
        let runtime = runtimes.get_mut(&(rt as usize))?;
        api::Context::new(&mut runtime.isolate).ok()
    });
    let Some(api) = api else {
        return std::ptr::null_mut();
    };
    let context = Box::new(JsContext {
        rt,
        api,
        opaque: std::ptr::null_mut(),
        retain: Cell::new(1),
    });
    let ptr = &*context as *const JsContext as *mut JSContext;
    CONTEXTS.with(|contexts| contexts.borrow_mut().insert(ptr as usize, context));
    ptr
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_FreeContext(ctx: *mut JSContext) {
    CONTEXTS.with(|contexts| {
        let mut contexts = contexts.borrow_mut();
        let Some(entry) = contexts.get(&(ctx as usize)) else {
            return;
        };
        let count = entry.retain.get();
        if count > 1 {
            entry.retain.set(count - 1);
            return;
        }
        contexts.remove(&(ctx as usize));
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_DupContext(ctx: *mut JSContext) -> *mut JSContext {
    CONTEXTS.with(|contexts| {
        let contexts = contexts.borrow();
        let Some(entry) = contexts.get(&(ctx as usize)) else {
            return std::ptr::null_mut();
        };
        entry.retain.set(entry.retain.get() + 1);
        ctx
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_GetRuntime(ctx: *mut JSContext) -> *mut JSRuntime {
    CONTEXTS.with(|contexts| {
        contexts
            .borrow()
            .get(&(ctx as usize))
            .map(|entry| entry.rt)
            .unwrap_or(std::ptr::null_mut())
    })
}
