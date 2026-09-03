//! A tiny "Minecraft-like" voxel sandbox written in JavaScript, driven
//! through the `rl` host module's 3D surface (beginMode3D/drawCube).
//!
//! Run: `cargo run --release -p slag --example raylib_voxel --features slag/raylib,slag/jit`
//! or through the CLI: `cargo run --release -p cli --features raylib -- crates/slag/examples/raylib_voxel.js`

#[cfg(feature = "raylib")]
use slag::{Context, HostCallbacks};

fn main() {
    #[cfg(feature = "raylib")]
    run_demo();
    #[cfg(not(feature = "raylib"))]
    eprintln!("raylib_voxel: build with `--features slag/raylib` to open a window");
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
    context.eval(include_str!("raylib_voxel.js")).unwrap();
    println!("demo finished");
}
