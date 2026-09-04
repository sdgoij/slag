//! Drive a windowed declarative-UI demo from a Slag script: install the `rl`
//! host module (raylib + raygui controls) plus the pure-JS `rlx` layer, and
//! let a Slag script describe a control tree in JSX (see `rlx_demo.jsx`),
//! which the parser desugars to `rlx.h(...)` calls for `rlx.present` to
//! render and draw each frame.
//!
//! Run: `cargo run -p slag --example rlx_demo --features slag/raygui`
//!      (add `jit` to run the loop's JS through the Cranelift JIT:
//!       `--features slag/raygui,slag/jit`)

#[cfg(feature = "raygui")]
use slag::{Context, HostCallbacks};

fn main() {
    #[cfg(feature = "raygui")]
    run_demo();
    #[cfg(not(feature = "raygui"))]
    eprintln!("rlx_demo: build with `--features slag/raygui` to open a window");
}

#[cfg(feature = "raygui")]
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
    context.install_rlx().unwrap();
    context.eval_jsx(include_str!("rlx_demo.jsx")).unwrap();
    println!("demo finished");
}
