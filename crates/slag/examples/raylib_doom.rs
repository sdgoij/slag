//! A DOOM-flavored textured raycaster written in JavaScript, drawn through
//! the `rl` host module's texture/audio surface (real DOOM sprite frames and
//! SFX from `examples/DOOM`).
//!
//! Run: `cargo run --release -p slag --example raylib_doom --features slag/raylib,slag/jit`

#[cfg(feature = "raylib")]
#[path = "raylib_doom_assets.inc"]
mod raylib_doom_assets;

#[cfg(feature = "raylib")]
use slag::{Context, HostCallbacks};

fn main() {
    #[cfg(feature = "raylib")]
    run_demo();
    #[cfg(not(feature = "raylib"))]
    eprintln!("raylib_doom: build with `--features slag/raylib` to open a window");
}

#[cfg(feature = "raylib")]
fn run_demo() {
    let mut context = Context::new().unwrap();
    let callbacks = HostCallbacks {
        console_log: Some(Box::new(|text| println!("[js] {text}"))),
        ..HostCallbacks::default()
    };
    context.set_host_callbacks(callbacks);
    for (name, data) in raylib_doom_assets::ASSETS {
        context.register_raylib_asset(name, data);
    }
    #[cfg(feature = "jit")]
    slag::install_jit(&mut context).unwrap();
    context.install_raylib().unwrap();
    context.eval(include_str!("raylib_doom.js")).unwrap();
    println!("demo finished");
}
