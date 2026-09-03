//! Drive a windowed raygui control panel from a Slag script: create a
//! `Context`, install the `rl` host module (which, under the `raygui`
//! feature, also installs the `rl.gui*` controls), and let JavaScript own
//! the whole render loop. The script (see `raygui_demo.js`) mixes a small
//! bouncing-dots canvas with raygui controls drawn inside the loop.
//!
//! Run: `cargo run -p slag --example raygui_demo --features slag/raygui`
//!      (add `jit` to run the loop's JS through the Cranelift JIT:
//!       `--features slag/raygui,slag/jit`)

#[cfg(feature = "raygui")]
use slag::{Context, HostCallbacks};

fn main() {
    #[cfg(feature = "raygui")]
    run_demo();
    #[cfg(not(feature = "raygui"))]
    eprintln!("raygui_demo: build with `--features slag/raygui` to open a window");
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
    context.eval(include_str!("raygui_demo.js")).unwrap();
    println!("demo finished");
}
