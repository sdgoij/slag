//! Drive a windowed raylib sketch from a Slag script: create a `Context`,
//! install the `rl` host module, and let JavaScript own the whole render
//! loop. The script (see `raylib_demo.js`) blocks in raylib's classic
//! immediate-mode loop until the window closes.
//!
//! Run: `cargo run -p slag --example raylib_demo --features slag/raylib`
//!      (add `jit` to run the loop's JS through the Cranelift JIT:
//!       `--features slag/raylib,slag/jit`)

#[cfg(feature = "raylib")]
use slag::{Context, HostCallbacks};

fn main() {
    #[cfg(feature = "raylib")]
    run_demo();
    #[cfg(not(feature = "raylib"))]
    eprintln!("raylib_demo: build with `--features slag/raylib` to open a window");
}

#[cfg(feature = "raylib")]
fn run_demo() {
    let mut context = Context::new().unwrap();
    let callbacks = HostCallbacks {
        console_log: Some(Box::new(|text| println!("[js] {text}"))),
        ..HostCallbacks::default()
    };
    context.set_host_callbacks(callbacks);
    #[cfg(feature = "jit")]
    slag::install_jit(&mut context).unwrap();
    context.install_raylib().unwrap();
    context.eval(include_str!("raylib_demo.js")).unwrap();
    println!("demo finished");
}
