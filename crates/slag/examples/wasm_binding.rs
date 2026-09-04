//! Browser/Node binding demo for the embedding API — a dependency-free wasm
//! module (no wasm-bindgen). Build it for the browser:
//!
//! ```sh
//! cargo build -p slag --example wasm_binding --target wasm32-unknown-unknown --release
//! ```
//! then load `browser/slag.js` against the built module
//! (`target/wasm32-unknown-unknown/release/examples/wasm_binding.wasm`) and
//! open `browser/demo.html`. The wasm module imports three `env` functions
//! the embedding JS provides:
//!
//! - `slag_host_now_ms()` — milliseconds since the Unix epoch (`Date.now()`)
//! - `slag_host_now_monotonic_ms()` — monotonic milliseconds
//!   (`performance.now()`) for `setTimeout` deadlines
//! - `slag_host_console(level, ptr, len)` — a console line (0 log, 1 info,
//!   2 warn, 3 error, 4 debug, 5 unhandled rejection)
//!
//! Exports: `slag_alloc`/`slag_dealloc` (caller-writable byte buffers),
//! `slag_eval(ptr, len)`, `slag_drain`, `slag_next_timeout_ms`, `slag_reset`,
//! and the `slag_result_*` trio (the last outcome's UTF-8 text, valid until
//! the next call). On native hosts `main` runs the same eval path as a
//! self-test: `cargo run -p slag --example wasm_binding`.

use std::alloc::{Layout, alloc, dealloc};
use std::ptr;
use std::slice;

use slag::{Context, HostCallbacks};

/// One script's outcome, kept until the next export call replaces it.
struct Outcome {
    error: bool,
    text: Vec<u8>,
}

static mut CONTEXT: *mut Context = ptr::null_mut();
static mut OUTCOME: Option<Outcome> = None;

// Imported from the embedding JS (`env.slag_host_console`).
#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
unsafe extern "C" {
    fn slag_host_console(level: i32, ptr: u32, len: u32);
}

/// A console-line sink that hands the text to the embedding JS.
fn console_hook(level: i32) -> Box<dyn Fn(&str)> {
    Box::new(move |line: &str| {
        #[cfg(target_arch = "wasm32")]
        {
            let bytes = line.as_bytes();
            // SAFETY: `slag_host_console` is a required import; `line` lives
            // for the call and the engine is single-threaded.
            unsafe { slag_host_console(level, bytes.as_ptr() as u32, bytes.len() as u32) };
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            if matches!(level, 0 | 1 | 4) {
                println!("{line}");
            } else {
                eprintln!("{line}");
            }
        }
    })
}

/// The host callbacks routing `console.*` and rejections to the embedder.
fn host_callbacks() -> HostCallbacks {
    HostCallbacks {
        console_log: Some(console_hook(0)),
        console_info: Some(console_hook(1)),
        console_warn: Some(console_hook(2)),
        console_error: Some(console_hook(3)),
        console_debug: Some(console_hook(4)),
        promise_rejection: Some(console_hook(5)),
        ..HostCallbacks::default()
    }
}

/// The process-wide context, created on first use; `slag_reset` replaces it.
///
/// # Safety
/// Callers must not hold two `&mut Context`s at once; all exports are
/// single-threaded (wasm) or used through the crate's own functions.
fn with_context(body: impl FnOnce(&mut Context) -> Result<(), String>) -> Result<(), String> {
    // SAFETY: the context is created once and only touched through this
    // helper, and never concurrently (wasm is single-threaded).
    let context = unsafe {
        if CONTEXT.is_null() {
            let mut context = Context::new().map_err(|error| error.to_string())?;
            context.set_host_callbacks(host_callbacks());
            CONTEXT = Box::into_raw(Box::new(context));
        }
        &mut *CONTEXT
    };
    body(context)
}

/// The existing context, or `None` before the first eval and after a reset.
/// Drains and timer queries have nothing to do then and must not create one.
fn existing_context() -> Option<&'static mut Context> {
    // SAFETY: single-threaded; only this helper and `with_context` hand the
    // context out, never concurrently.
    unsafe {
        if CONTEXT.is_null() {
            None
        } else {
            Some(&mut *CONTEXT)
        }
    }
}

/// Replace the last outcome (dropping the previous buffer).
fn set_outcome(error: bool, text: String) {
    // SAFETY: only called from the exports, single-threaded.
    unsafe {
        OUTCOME = Some(Outcome {
            error,
            text: text.into_bytes(),
        });
    }
}

/// Evaluate `source` and render its completion, draining jobs only after the
/// text is copied out (so a job-driven GC cannot move the value first).
fn evaluate_in(context: &mut Context, source: &str) -> Result<(), String> {
    let value = context
        .eval_script(source)
        .map_err(|error| error.to_string())?;
    let text = context
        .to_string(&value)
        .map_err(|error| error.to_string())?;
    context.run_jobs().map_err(|error| error.to_string())?;
    set_outcome(false, text);
    Ok(())
}

/// The module's allocator, so the host can hand us input without fixed
/// buffers: `slag_alloc(len)` then `slag_dealloc(ptr, len)`.
fn allocate(len: usize) -> u32 {
    if len == 0 {
        return 0;
    }
    let Ok(layout) = Layout::from_size_align(len, 1) else {
        return 0;
    };
    // SAFETY: `layout` has positive size and align 1.
    unsafe { alloc(layout) as u32 }
}

fn deallocate(ptr: u32, len: usize) {
    if ptr == 0 || len == 0 {
        return;
    }
    let Ok(layout) = Layout::from_size_align(len, 1) else {
        return;
    };
    // SAFETY: `ptr` came from `allocate(len)` with the same length.
    unsafe { dealloc(ptr as *mut u8, layout) }
}

/// A caller-writable buffer of `len` bytes (0 when allocation failed).
#[unsafe(no_mangle)]
pub extern "C" fn slag_alloc(len: u32) -> u32 {
    allocate(len as usize)
}

/// Free a buffer from [`slag_alloc`]; `len` must match the allocation.
#[unsafe(no_mangle)]
pub extern "C" fn slag_dealloc(ptr: u32, len: u32) {
    deallocate(ptr, len as usize);
}

/// Evaluate the UTF-8 script at `ptr` (see [`slag_alloc`]); 0 on success,
/// non-zero when the outcome is an error.
///
/// # Safety
/// `ptr` must point at `len` valid bytes in the module's linear memory (a
/// buffer from [`slag_alloc`] filled by the caller).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_eval(ptr: u32, len: u32) -> i32 {
    let result = (|| {
        // SAFETY: the caller guarantees `len` readable bytes at `ptr` when
        // `len` is non-zero; empty scripts read as an empty slice.
        let bytes = if len == 0 {
            &[][..]
        } else {
            unsafe { slice::from_raw_parts(ptr as *const u8, len as usize) }
        };
        let source =
            std::str::from_utf8(bytes).map_err(|_| "script is not valid UTF-8".to_string())?;
        with_context(|context| evaluate_in(context, source))
    })();
    match result {
        Ok(()) => 0,
        Err(message) => {
            set_outcome(true, message);
            1
        }
    }
}

/// Drain the pending job queues (microtasks and due timers). The JS glue
/// calls this automatically when an engine timer comes due; hosts may also
/// call it directly. Returns 0 on success.
#[unsafe(no_mangle)]
pub extern "C" fn slag_drain() -> i32 {
    let Some(context) = existing_context() else {
        return 0;
    };
    match context.run_jobs() {
        Ok(()) => {
            set_outcome(false, String::new());
            0
        }
        Err(error) => {
            set_outcome(true, error.to_string());
            1
        }
    }
}

/// Milliseconds until the earliest pending engine timer is due, or -1 when
/// none is queued (the JS glue schedules host drains from this).
#[unsafe(no_mangle)]
pub extern "C" fn slag_next_timeout_ms() -> f64 {
    existing_context()
        .and_then(|context| context.next_timeout_ms())
        .unwrap_or(-1.0)
}

/// Drop the current context; the next `slag_eval` starts a fresh realm.
#[unsafe(no_mangle)]
pub extern "C" fn slag_reset() -> i32 {
    // SAFETY: single-threaded; the previous context (if any) is dropped and
    // the pointer cleared so the next call recreates it.
    unsafe {
        if !CONTEXT.is_null() {
            drop(Box::from_raw(CONTEXT));
            CONTEXT = ptr::null_mut();
        }
        OUTCOME = None;
    }
    0
}

/// The last outcome's UTF-8 text (0 bytes when empty).
#[unsafe(no_mangle)]
pub extern "C" fn slag_result_ptr() -> u32 {
    // SAFETY: reads a pointer into the current outcome's buffer; the buffer
    // stays alive until the next export call replaces it.
    unsafe {
        match &*ptr::addr_of!(OUTCOME) {
            Some(outcome) => outcome.text.as_ptr() as u32,
            None => 0,
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn slag_result_len() -> u32 {
    // SAFETY: reads the current outcome's byte length (see `slag_result_ptr`).
    unsafe {
        match &*ptr::addr_of!(OUTCOME) {
            Some(outcome) => outcome.text.len() as u32,
            None => 0,
        }
    }
}

/// Whether the last outcome was an error (e.g. a thrown exception).
#[unsafe(no_mangle)]
pub extern "C" fn slag_result_error() -> i32 {
    // SAFETY: reads the current outcome's error flag (see `slag_result_ptr`).
    unsafe {
        match &*ptr::addr_of!(OUTCOME) {
            Some(outcome) => i32::from(outcome.error),
            None => 0,
        }
    }
}

/// The native-host self-test: the same script render path as the wasm
/// exports, against a local context with stdout console output.
fn main() {
    let result = (|| -> Result<(), String> {
        let mut context = Context::new().map_err(|error| error.to_string())?;
        let render = |context: &mut Context, source: &str| -> Result<String, String> {
            let value = context
                .eval_script(source)
                .map_err(|error| error.to_string())?;
            let text = context
                .to_string(&value)
                .map_err(|error| error.to_string())?;
            context.run_jobs().map_err(|error| error.to_string())?;
            Ok(text)
        };
        for (source, expected) in [
            ("1 + 2", "3"),
            ("'ab' + 'cd'", "abcd"),
            ("JSON.stringify({ a: [1, 2] })", "{\"a\":[1,2]}"),
        ] {
            let actual = render(&mut context, source)?;
            if actual != expected {
                return Err(format!("{source}: got {actual:?}, expected {expected:?}"));
            }
        }
        render(&mut context, "globalThis.n = 40;")?;
        let actual = render(&mut context, "globalThis.n + 2")?;
        if actual != "42" {
            return Err(format!("state: got {actual:?}, expected \"42\""));
        }
        let error = match render(&mut context, "null.x") {
            Ok(text) => return Err(format!("expected an error, got {text:?}")),
            Err(error) => error,
        };
        if !error.starts_with("TypeError:") {
            return Err(format!("error kind: {error}"));
        }
        render(
            &mut context,
            "globalThis.result = 0; setTimeout(() => { globalThis.result = 7; }, 0);",
        )?;
        let actual = render(&mut context, "globalThis.result")?;
        if actual != "7" {
            return Err(format!("timer: got {actual:?}, expected \"7\""));
        }
        Ok(())
    })();
    match result {
        Ok(()) => println!("wasm_binding: native self-test ok"),
        Err(message) => {
            eprintln!("wasm_binding: {message}");
            std::process::exit(1);
        }
    }
}
