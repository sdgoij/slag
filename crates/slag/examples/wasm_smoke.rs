//! WASM smoke harness for the embed API (`Context`), usable from Node or a
//! browser: write a UTF-8 script into the `input_buffer`, call `run(len)`,
//! and read the rendered completion/console output from the `output_buffer`.
//!
//! Build:
//! ```sh
//! cargo build -p slag --example wasm_smoke --target wasm32-unknown-unknown --release
//! ```
//! The exports are `input_buffer`/`input_capacity` (caller-writable script
//! buffer), `run` (evaluate; returns the output byte count), and
//! `output_buffer`/`panic_buffer` for reading results; the module imports
//! `env.slag_host_now_ms` (Date.now). On native hosts the same code path
//! runs as a plain example: `cargo run -p slag --example wasm_smoke`.

use std::cell::RefCell;
use std::ptr;
use std::rc::Rc;
use std::slice;
use std::sync::atomic::{AtomicUsize, Ordering};

use slag::{Context, HostCallbacks};

/// JS writes the script here; Slag renders output there. Kept as fixed
/// statics so the wasm exports can hand out stable addresses.
const BUF_LEN: usize = 1 << 16;
const PANIC_CAP: usize = 2048;

static mut INPUT: [u8; BUF_LEN] = [0; BUF_LEN];
static mut OUTPUT: [u8; BUF_LEN] = [0; BUF_LEN];
static mut PANIC_BUF: [u8; PANIC_CAP] = [0; PANIC_CAP];

static PANIC_LEN: AtomicUsize = AtomicUsize::new(0);
static SET_HOOK: std::sync::Once = std::sync::Once::new();

thread_local! {
    static OUT_LEN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Record Rust panic messages (a wasm abort would otherwise hide them).
fn install_panic_hook() {
    SET_HOOK.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let text = info.to_string();
            let n = text.len().min(PANIC_CAP);
            // SAFETY: single-threaded; PANIC_BUF is written only here.
            unsafe {
                ptr::copy_nonoverlapping(
                    text.as_ptr(),
                    ptr::addr_of_mut!(PANIC_BUF).cast::<u8>(),
                    n,
                );
            }
            PANIC_LEN.store(n, Ordering::Relaxed);
        }));
    });
    PANIC_LEN.store(0, Ordering::Relaxed);
}

/// Evaluate `script` in a fresh Context, routing `console.*` into the same
/// output buffer as the completion value.
fn evaluate(script: &str) -> Result<String, String> {
    let mut context = Context::new().map_err(|error| format!("Context::new: {error}"))?;
    let lines = Rc::new(RefCell::new(String::new()));
    let hook = |lines: Rc<RefCell<String>>| {
        Box::new(move |line: &str| {
            lines.borrow_mut().push_str(line);
            lines.borrow_mut().push('\n');
        }) as Box<dyn Fn(&str)>
    };
    let callbacks = HostCallbacks {
        console_log: Some(hook(lines.clone())),
        console_info: Some(hook(lines.clone())),
        console_warn: Some(hook(lines.clone())),
        console_error: Some(hook(lines.clone())),
        console_debug: Some(hook(lines.clone())),
        ..HostCallbacks::default()
    };
    context.set_host_callbacks(callbacks);

    let completed = context.eval(script).map(|value| value.to_string());
    let mut out = lines.borrow().clone();
    // Drop the context first: its console closures keep `lines` alive.
    drop(context);
    match completed {
        Ok(text) => out.push_str(&text),
        Err(error) => out.push_str(&format!("<error> {error}")),
    }
    Ok(out)
}

/// Append `text` to the output buffer; returns the total bytes written.
fn write_out(text: &str) -> usize {
    OUT_LEN.with(|used| {
        let start = used.get();
        // SAFETY: OUTPUT is written only through this helper, on one thread.
        let n = unsafe {
            let room = BUF_LEN.saturating_sub(start);
            let n = text.len().min(room);
            ptr::copy_nonoverlapping(
                text.as_ptr(),
                ptr::addr_of_mut!(OUTPUT).cast::<u8>().add(start),
                n,
            );
            n
        };
        used.set(start + n);
        start + n
    })
}

/// Base of the caller-writable script buffer.
#[unsafe(no_mangle)]
pub extern "C" fn input_buffer() -> *mut u8 {
    ptr::addr_of_mut!(INPUT).cast::<u8>()
}

#[unsafe(no_mangle)]
pub extern "C" fn input_capacity() -> usize {
    BUF_LEN
}

/// Base of the rendered-output buffer; `run` returns how many bytes to read.
#[unsafe(no_mangle)]
pub extern "C" fn output_buffer() -> *mut u8 {
    ptr::addr_of_mut!(OUTPUT).cast::<u8>()
}

#[unsafe(no_mangle)]
pub extern "C" fn output_capacity() -> usize {
    BUF_LEN
}

/// Evaluate the UTF-8 script at `input_buffer` (first `len` bytes) and write
/// console output plus the completion value to `output_buffer`; returns the
/// number of output bytes (0 if there is none).
#[unsafe(no_mangle)]
pub extern "C" fn run(len: usize) -> usize {
    install_panic_hook();
    OUT_LEN.with(|used| used.set(0));
    // SAFETY: the caller wrote the script before calling `run`; `len` is
    // clamped to the buffer size below.
    let bytes =
        unsafe { slice::from_raw_parts(ptr::addr_of!(INPUT).cast::<u8>(), len.min(BUF_LEN)) };
    let script = match std::str::from_utf8(bytes) {
        Ok(script) => script,
        Err(_) => return write_out("<invalid UTF-8 script>"),
    };
    match evaluate(script) {
        Ok(text) => write_out(&text),
        Err(error) => write_out(&format!("<error> {error}")),
    }
}

/// Base of the last Rust panic message (see `panic_len` for its length).
#[unsafe(no_mangle)]
pub extern "C" fn panic_buffer() -> *mut u8 {
    ptr::addr_of_mut!(PANIC_BUF).cast::<u8>()
}

#[unsafe(no_mangle)]
pub extern "C" fn panic_len() -> usize {
    PANIC_LEN.load(Ordering::Relaxed)
}

fn main() {
    // The standalone WebAssembly smoke (Cut 10 exit): compile, instantiate,
    // and call a wasm module from JS, plus Memory/Global access. It runs the
    // same evaluate path a browser host drives, so `cargo run -p slag
    // --example wasm_smoke` verifies the embed end to end on any host.
    let script = "const bytes = new Uint8Array([\
        0,97,115,109,1,0,0,0,\
        1,7,1,96,2,127,127,1,127,\
        3,2,1,0,\
        7,7,1,3,97,100,100,0,0,\
        10,9,1,7,0,32,0,32,1,106,11]);\
        const instance = new WebAssembly.Instance(new WebAssembly.Module(bytes), {});\
        const sum = instance.exports.add(20, 22);\
        const memory = new WebAssembly.Memory({ initial: 1 });\
        new Uint8Array(memory.buffer)[0] = 42;\
        const global = new WebAssembly.Global({ value: 'i32', mutable: true }, 5);\
        console.log('wasm add(20, 22) =', sum);\
        console.log('memory[0] =', new Uint8Array(memory.buffer)[0]);\
        console.log('global.value =', global.value);\
        'wasm smoke ok'";
    let output = match evaluate(script) {
        Ok(output) => output,
        Err(error) => {
            eprintln!("wasm smoke: {error}");
            std::process::exit(1);
        }
    };
    println!("{output}");
    let expected = [
        "wasm add(20, 22) = 42",
        "memory[0] = 42",
        "global.value = 5",
        "wasm smoke ok",
    ];
    if !expected.iter().all(|line| output.contains(line)) {
        eprintln!("wasm smoke: output did not match the expected lines");
        std::process::exit(1);
    }
}
