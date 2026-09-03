//! The raylib host module (`rl`), compiled behind the `raylib` feature: a
//! windowing / drawing / input surface backed 1:1 by raylib's C API plus its
//! color palette and key/mouse constants.
//!
//! JS colors cross the boundary as plain numbers packed `0xRRGGBBAA` (a
//! natural fit for doubles); `rl.color(r, g, b, a)` and the `rl.*` color
//! constants produce them. raylib keeps its window state process-global and
//! bound to the thread that opened the window, so every call re-checks that
//! it runs on the thread which installed the module and throws a `TypeError`
//! from any other thread (e.g. a worker agent) instead of racing that state.
//!
//! Drawing is immediate-mode and blocking: the script drives the loop
//! itself, exactly like a raylib C example —
//! `while (!rl.windowShouldClose()) { rl.beginDrawing(); ...; rl.endDrawing(); }`.

use std::ffi::CString;
use std::sync::OnceLock;
use std::thread::ThreadId;

use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::object::JsObject as CruxObject;
use crux::string::JsString;
use crux::value::{Value, ValueKind};
use raylib_sys::{Camera3D, Color, Vector3};

use crate::agent::Agent;

/// The thread that installed the module. raylib's window state is
/// process-global and must only be touched from this thread.
static WINDOW_THREAD: OnceLock<ThreadId> = OnceLock::new();

/// raylib's built-in palette (`raylib.h` color defines), as JS constants.
const COLORS: &[(&str, u8, u8, u8, u8)] = &[
    ("LIGHTGRAY", 200, 200, 200, 255),
    ("GRAY", 130, 130, 130, 255),
    ("DARKGRAY", 80, 80, 80, 255),
    ("YELLOW", 253, 249, 0, 255),
    ("GOLD", 255, 203, 0, 255),
    ("ORANGE", 255, 161, 0, 255),
    ("PINK", 255, 109, 194, 255),
    ("RED", 230, 41, 55, 255),
    ("MAROON", 190, 33, 55, 255),
    ("GREEN", 0, 228, 48, 255),
    ("LIME", 0, 158, 47, 255),
    ("DARKGREEN", 0, 117, 44, 255),
    ("SKYBLUE", 102, 191, 255, 255),
    ("BLUE", 0, 121, 241, 255),
    ("DARKBLUE", 0, 82, 172, 255),
    ("PURPLE", 200, 122, 255, 255),
    ("VIOLET", 135, 60, 190, 255),
    ("DARKPURPLE", 112, 31, 126, 255),
    ("BEIGE", 211, 176, 131, 255),
    ("BROWN", 127, 106, 79, 255),
    ("DARKBROWN", 76, 63, 47, 255),
    ("WHITE", 255, 255, 255, 255),
    ("BLACK", 0, 0, 0, 255),
    ("BLANK", 0, 0, 0, 0),
    ("MAGENTA", 255, 0, 255, 255),
    ("RAYWHITE", 245, 245, 245, 255),
];

/// Named non-ASCII key codes (`raylib.h` `KeyboardKey` enum). Letters
/// (`KEY_A`..`KEY_Z`) and digits (`KEY_ZERO`..`KEY_NINE`) are their ASCII
/// codes and are added programmatically.
const KEY_CODES: &[(&str, i32)] = &[
    ("KEY_SPACE", 32),
    ("KEY_ESCAPE", 256),
    ("KEY_ENTER", 257),
    ("KEY_TAB", 258),
    ("KEY_BACKSPACE", 259),
    ("KEY_INSERT", 260),
    ("KEY_DELETE", 261),
    ("KEY_RIGHT", 262),
    ("KEY_LEFT", 263),
    ("KEY_DOWN", 264),
    ("KEY_UP", 265),
    ("KEY_PAGE_UP", 266),
    ("KEY_PAGE_DOWN", 267),
    ("KEY_HOME", 268),
    ("KEY_END", 269),
    ("KEY_LEFT_SHIFT", 340),
    ("KEY_LEFT_CONTROL", 341),
    ("KEY_LEFT_ALT", 342),
    ("KEY_LEFT_SUPER", 343),
    ("KEY_RIGHT_SHIFT", 344),
    ("KEY_RIGHT_CONTROL", 345),
    ("KEY_RIGHT_ALT", 346),
    ("KEY_RIGHT_SUPER", 347),
];

fn thread_error() -> JsError {
    JsError::new(
        ErrorKind::TypeError,
        "rl.*: raylib is bound to the thread that called install_raylib".into(),
    )
}

fn on_window_thread() -> Result<(), JsError> {
    let installed = WINDOW_THREAD.get_or_init(|| std::thread::current().id());
    if *installed == std::thread::current().id() {
        Ok(())
    } else {
        Err(thread_error())
    }
}

/// Install a method that touches raylib's process-global window state; the
/// install-thread check runs before every call.
fn window_method(
    name: &str,
    arity: u64,
    body: fn(&[Value]) -> Result<Value, JsError>,
) -> Result<Handle<Function>, JsError> {
    let name = JsString::from_utf8(name);
    Function::create_builtin(
        Some(name),
        arity,
        Box::new(move |_, args| {
            on_window_thread()?;
            body(args)
        }),
        None,
        None,
    )
}

/// Install a pure (non-window) method.
fn plain_method(
    name: &str,
    arity: u64,
    body: fn(&[Value]) -> Result<Value, JsError>,
) -> Result<Handle<Function>, JsError> {
    Function::create_builtin(
        Some(JsString::from_utf8(name)),
        arity,
        Box::new(move |_, args| body(args)),
        None,
        None,
    )
}

fn define(rl: &CruxObject, name: &str, function: Handle<Function>) -> Result<(), JsError> {
    rl.create_data_property_or_throw(&JsString::from_utf8(name), Value::Function(function))
}

fn expected(name: &str, index: usize, what: &str) -> JsError {
    JsError::new(
        ErrorKind::TypeError,
        format!("rl.{name}: argument {index} must be {what}"),
    )
}

fn num_arg(args: &[Value], index: usize, name: &str) -> Result<f64, JsError> {
    match args.get(index).map(Value::kind) {
        Some(ValueKind::Number(number)) => Ok(number),
        _ => Err(expected(name, index, "a number")),
    }
}

fn int_arg(args: &[Value], index: usize, name: &str) -> Result<i32, JsError> {
    Ok(num_arg(args, index, name)? as i32)
}

fn text_arg(args: &[Value], index: usize, name: &str) -> Result<CString, JsError> {
    match args.get(index).map(Value::kind) {
        Some(ValueKind::String(text)) => CString::new(text.to_string_lossy()).map_err(|_| {
            JsError::new(
                ErrorKind::TypeError,
                format!("rl.{name}: argument {index} contains a NUL byte"),
            )
        }),
        _ => Err(expected(name, index, "a string")),
    }
}

/// Pack a raylib `Color` into the JS `0xRRGGBBAA` number form.
fn to_js_color(color: Color) -> Value {
    let packed =
        (color.r as u32) << 24 | (color.g as u32) << 16 | (color.b as u32) << 8 | color.a as u32;
    Value::Number(packed as f64)
}

fn color_arg(args: &[Value], index: usize, name: &str) -> Result<Color, JsError> {
    let number = num_arg(args, index, name)?;
    if !number.is_finite() || !(0.0..4294967296.0).contains(&number) {
        return Err(expected(name, index, "a packed 0xRRGGBBAA color"));
    }
    let bits = number as u32;
    Ok(Color::new(
        (bits >> 24) as u8,
        (bits >> 16) as u8,
        (bits >> 8) as u8,
        bits as u8,
    ))
}

/// One color channel in 0..=255.
fn channel_arg(args: &[Value], index: usize, name: &str) -> Result<u8, JsError> {
    let number = num_arg(args, index, name)?;
    if !number.is_finite() || !(0.0..=255.0).contains(&number) {
        return Err(expected(name, index, "a channel in 0..=255"));
    }
    Ok(number as u8)
}

// ---- 3D (needs the rmodels C module) ----

fn begin_mode_3d(args: &[Value]) -> Result<Value, JsError> {
    let px = num_arg(args, 0, "beginMode3D")? as f32;
    let py = num_arg(args, 1, "beginMode3D")? as f32;
    let pz = num_arg(args, 2, "beginMode3D")? as f32;
    let tx = num_arg(args, 3, "beginMode3D")? as f32;
    let ty = num_arg(args, 4, "beginMode3D")? as f32;
    let tz = num_arg(args, 5, "beginMode3D")? as f32;
    let fovy = num_arg(args, 6, "beginMode3D")? as f32;
    let camera = Camera3D {
        position: Vector3 {
            x: px,
            y: py,
            z: pz,
        },
        target: Vector3 {
            x: tx,
            y: ty,
            z: tz,
        },
        up: Vector3 {
            x: 0.0,
            y: 1.0,
            z: 0.0,
        },
        fovy,
        projection: 0, // CAMERA_PERSPECTIVE
    };
    // SAFETY: draw state on the installing thread (see `window_method`).
    unsafe { raylib_sys::BeginMode3D(camera) };
    Ok(Value::Undefined)
}

fn end_mode_3d(_args: &[Value]) -> Result<Value, JsError> {
    // SAFETY: as above.
    unsafe { raylib_sys::EndMode3D() };
    Ok(Value::Undefined)
}

fn draw_cube(args: &[Value]) -> Result<Value, JsError> {
    let x = num_arg(args, 0, "drawCube")? as f32;
    let y = num_arg(args, 1, "drawCube")? as f32;
    let z = num_arg(args, 2, "drawCube")? as f32;
    let width = num_arg(args, 3, "drawCube")? as f32;
    let height = num_arg(args, 4, "drawCube")? as f32;
    let length = num_arg(args, 5, "drawCube")? as f32;
    let color = color_arg(args, 6, "drawCube")?;
    // SAFETY: as above.
    unsafe { raylib_sys::DrawCube(Vector3 { x, y, z }, width, height, length, color) };
    Ok(Value::Undefined)
}

fn draw_cube_wires(args: &[Value]) -> Result<Value, JsError> {
    let x = num_arg(args, 0, "drawCubeWires")? as f32;
    let y = num_arg(args, 1, "drawCubeWires")? as f32;
    let z = num_arg(args, 2, "drawCubeWires")? as f32;
    let width = num_arg(args, 3, "drawCubeWires")? as f32;
    let height = num_arg(args, 4, "drawCubeWires")? as f32;
    let length = num_arg(args, 5, "drawCubeWires")? as f32;
    let color = color_arg(args, 6, "drawCubeWires")?;
    // SAFETY: as above.
    unsafe { raylib_sys::DrawCubeWires(Vector3 { x, y, z }, width, height, length, color) };
    Ok(Value::Undefined)
}

fn draw_grid(args: &[Value]) -> Result<Value, JsError> {
    let slices = int_arg(args, 0, "drawGrid")?;
    let spacing = num_arg(args, 1, "drawGrid")? as f32;
    // SAFETY: as above.
    unsafe { raylib_sys::DrawGrid(slices, spacing) };
    Ok(Value::Undefined)
}

// ---- window ----

fn init_window(args: &[Value]) -> Result<Value, JsError> {
    let width = int_arg(args, 0, "initWindow")?;
    let height = int_arg(args, 1, "initWindow")?;
    let title = text_arg(args, 2, "initWindow")?;
    // SAFETY: raylib copies `title` before returning; the window-thread guard
    // in `window_method` keeps this on the thread that opened the window.
    unsafe { raylib_sys::InitWindow(width, height, title.as_ptr()) };
    Ok(Value::Undefined)
}

fn window_should_close(_args: &[Value]) -> Result<Value, JsError> {
    // SAFETY: window-state read on the installing thread (see `window_method`).
    let close = unsafe { raylib_sys::WindowShouldClose() };
    Ok(Value::Boolean(close))
}

fn close_window(_args: &[Value]) -> Result<Value, JsError> {
    // SAFETY: as above; raylib tolerates a redundant CloseWindow().
    unsafe { raylib_sys::CloseWindow() };
    Ok(Value::Undefined)
}

fn is_window_ready(_args: &[Value]) -> Result<Value, JsError> {
    // SAFETY: as above.
    let ready = unsafe { raylib_sys::IsWindowReady() };
    Ok(Value::Boolean(ready))
}

fn set_target_fps(args: &[Value]) -> Result<Value, JsError> {
    let fps = int_arg(args, 0, "setTargetFPS")?;
    // SAFETY: as above.
    unsafe { raylib_sys::SetTargetFPS(fps) };
    Ok(Value::Undefined)
}

fn get_fps(_args: &[Value]) -> Result<Value, JsError> {
    // SAFETY: as above.
    let fps = unsafe { raylib_sys::GetFPS() };
    Ok(Value::Number(fps as f64))
}

fn get_frame_time(_args: &[Value]) -> Result<Value, JsError> {
    // SAFETY: as above.
    let seconds = unsafe { raylib_sys::GetFrameTime() };
    Ok(Value::Number(seconds as f64))
}

fn get_time(_args: &[Value]) -> Result<Value, JsError> {
    // SAFETY: as above.
    let seconds = unsafe { raylib_sys::GetTime() };
    Ok(Value::Number(seconds))
}

fn get_screen_width(_args: &[Value]) -> Result<Value, JsError> {
    // SAFETY: as above.
    let width = unsafe { raylib_sys::GetScreenWidth() };
    Ok(Value::Number(width as f64))
}

fn get_screen_height(_args: &[Value]) -> Result<Value, JsError> {
    // SAFETY: as above.
    let height = unsafe { raylib_sys::GetScreenHeight() };
    Ok(Value::Number(height as f64))
}

fn set_exit_key(args: &[Value]) -> Result<Value, JsError> {
    let key = int_arg(args, 0, "setExitKey")?;
    // SAFETY: as above.
    unsafe { raylib_sys::SetExitKey(key) };
    Ok(Value::Undefined)
}

// ---- drawing ----

fn begin_drawing(_args: &[Value]) -> Result<Value, JsError> {
    // SAFETY: draw state on the installing thread (see `window_method`).
    unsafe { raylib_sys::BeginDrawing() };
    Ok(Value::Undefined)
}

fn end_drawing(_args: &[Value]) -> Result<Value, JsError> {
    // SAFETY: as above.
    unsafe { raylib_sys::EndDrawing() };
    Ok(Value::Undefined)
}

fn clear_background(args: &[Value]) -> Result<Value, JsError> {
    let color = color_arg(args, 0, "clearBackground")?;
    // SAFETY: as above.
    unsafe { raylib_sys::ClearBackground(color) };
    Ok(Value::Undefined)
}

fn draw_fps(args: &[Value]) -> Result<Value, JsError> {
    let x = int_arg(args, 0, "drawFPS")?;
    let y = int_arg(args, 1, "drawFPS")?;
    // SAFETY: as above.
    unsafe { raylib_sys::DrawFPS(x, y) };
    Ok(Value::Undefined)
}

fn draw_text(args: &[Value]) -> Result<Value, JsError> {
    let text = text_arg(args, 0, "drawText")?;
    let x = int_arg(args, 1, "drawText")?;
    let y = int_arg(args, 2, "drawText")?;
    let size = int_arg(args, 3, "drawText")?;
    let color = color_arg(args, 4, "drawText")?;
    // SAFETY: raylib reads the text only for the duration of the call; the
    // default font is loaded lazily and cached process-globally.
    unsafe { raylib_sys::DrawText(text.as_ptr(), x, y, size, color) };
    Ok(Value::Undefined)
}

fn measure_text(args: &[Value]) -> Result<Value, JsError> {
    let text = text_arg(args, 0, "measureText")?;
    let size = int_arg(args, 1, "measureText")?;
    // SAFETY: as above.
    let width = unsafe { raylib_sys::MeasureText(text.as_ptr(), size) };
    Ok(Value::Number(width as f64))
}

fn draw_circle(args: &[Value]) -> Result<Value, JsError> {
    let x = int_arg(args, 0, "drawCircle")?;
    let y = int_arg(args, 1, "drawCircle")?;
    let radius = num_arg(args, 2, "drawCircle")? as f32;
    let color = color_arg(args, 3, "drawCircle")?;
    // SAFETY: as above.
    unsafe { raylib_sys::DrawCircle(x, y, radius, color) };
    Ok(Value::Undefined)
}

fn draw_circle_lines(args: &[Value]) -> Result<Value, JsError> {
    let x = int_arg(args, 0, "drawCircleLines")?;
    let y = int_arg(args, 1, "drawCircleLines")?;
    let radius = num_arg(args, 2, "drawCircleLines")? as f32;
    let color = color_arg(args, 3, "drawCircleLines")?;
    // SAFETY: draw state on the installing thread (see `window_method`).
    unsafe { raylib_sys::DrawCircleLines(x, y, radius, color) };
    Ok(Value::Undefined)
}

fn draw_rectangle(args: &[Value]) -> Result<Value, JsError> {
    let x = int_arg(args, 0, "drawRectangle")?;
    let y = int_arg(args, 1, "drawRectangle")?;
    let width = int_arg(args, 2, "drawRectangle")?;
    let height = int_arg(args, 3, "drawRectangle")?;
    let color = color_arg(args, 4, "drawRectangle")?;
    // SAFETY: as above.
    unsafe { raylib_sys::DrawRectangle(x, y, width, height, color) };
    Ok(Value::Undefined)
}

fn draw_line(args: &[Value]) -> Result<Value, JsError> {
    let x1 = int_arg(args, 0, "drawLine")?;
    let y1 = int_arg(args, 1, "drawLine")?;
    let x2 = int_arg(args, 2, "drawLine")?;
    let y2 = int_arg(args, 3, "drawLine")?;
    let color = color_arg(args, 4, "drawLine")?;
    // SAFETY: as above.
    unsafe { raylib_sys::DrawLine(x1, y1, x2, y2, color) };
    Ok(Value::Undefined)
}

fn draw_pixel(args: &[Value]) -> Result<Value, JsError> {
    let x = int_arg(args, 0, "drawPixel")?;
    let y = int_arg(args, 1, "drawPixel")?;
    let color = color_arg(args, 2, "drawPixel")?;
    // SAFETY: as above.
    unsafe { raylib_sys::DrawPixel(x, y, color) };
    Ok(Value::Undefined)
}

// ---- input ----

fn is_key_down(args: &[Value]) -> Result<Value, JsError> {
    let key = int_arg(args, 0, "isKeyDown")?;
    // SAFETY: input state on the installing thread (see `window_method`).
    let down = unsafe { raylib_sys::IsKeyDown(key) };
    Ok(Value::Boolean(down))
}

fn is_key_pressed(args: &[Value]) -> Result<Value, JsError> {
    let key = int_arg(args, 0, "isKeyPressed")?;
    // SAFETY: as above.
    let pressed = unsafe { raylib_sys::IsKeyPressed(key) };
    Ok(Value::Boolean(pressed))
}

fn is_key_released(args: &[Value]) -> Result<Value, JsError> {
    let key = int_arg(args, 0, "isKeyReleased")?;
    // SAFETY: as above.
    let released = unsafe { raylib_sys::IsKeyReleased(key) };
    Ok(Value::Boolean(released))
}

fn is_key_up(args: &[Value]) -> Result<Value, JsError> {
    let key = int_arg(args, 0, "isKeyUp")?;
    // SAFETY: as above.
    let up = unsafe { raylib_sys::IsKeyUp(key) };
    Ok(Value::Boolean(up))
}

fn get_key_pressed(_args: &[Value]) -> Result<Value, JsError> {
    // SAFETY: as above.
    let key = unsafe { raylib_sys::GetKeyPressed() };
    Ok(Value::Number(key as f64))
}

fn is_mouse_button_down(args: &[Value]) -> Result<Value, JsError> {
    let button = int_arg(args, 0, "isMouseButtonDown")?;
    // SAFETY: as above.
    let down = unsafe { raylib_sys::IsMouseButtonDown(button) };
    Ok(Value::Boolean(down))
}

fn is_mouse_button_pressed(args: &[Value]) -> Result<Value, JsError> {
    let button = int_arg(args, 0, "isMouseButtonPressed")?;
    // SAFETY: as above.
    let pressed = unsafe { raylib_sys::IsMouseButtonPressed(button) };
    Ok(Value::Boolean(pressed))
}

fn is_mouse_button_released(args: &[Value]) -> Result<Value, JsError> {
    let button = int_arg(args, 0, "isMouseButtonReleased")?;
    // SAFETY: as above.
    let released = unsafe { raylib_sys::IsMouseButtonReleased(button) };
    Ok(Value::Boolean(released))
}

fn get_mouse_x(_args: &[Value]) -> Result<Value, JsError> {
    // SAFETY: as above.
    let x = unsafe { raylib_sys::GetMouseX() };
    Ok(Value::Number(x as f64))
}

fn get_mouse_y(_args: &[Value]) -> Result<Value, JsError> {
    // SAFETY: as above.
    let y = unsafe { raylib_sys::GetMouseY() };
    Ok(Value::Number(y as f64))
}

fn get_mouse_wheel_move(_args: &[Value]) -> Result<Value, JsError> {
    // SAFETY: as above.
    let delta = unsafe { raylib_sys::GetMouseWheelMove() };
    Ok(Value::Number(delta as f64))
}

fn get_mouse_delta_x(_args: &[Value]) -> Result<Value, JsError> {
    // SAFETY: as above.
    let delta = unsafe { raylib_sys::GetMouseDelta() };
    Ok(Value::Number(delta.x as f64))
}

fn get_mouse_delta_y(_args: &[Value]) -> Result<Value, JsError> {
    // SAFETY: as above.
    let delta = unsafe { raylib_sys::GetMouseDelta() };
    Ok(Value::Number(delta.y as f64))
}

fn disable_cursor(_args: &[Value]) -> Result<Value, JsError> {
    // SAFETY: as above.
    unsafe { raylib_sys::DisableCursor() };
    Ok(Value::Undefined)
}

fn enable_cursor(_args: &[Value]) -> Result<Value, JsError> {
    // SAFETY: as above.
    unsafe { raylib_sys::EnableCursor() };
    Ok(Value::Undefined)
}

// ---- color helper ----

fn color(args: &[Value]) -> Result<Value, JsError> {
    let r = channel_arg(args, 0, "color")?;
    let g = channel_arg(args, 1, "color")?;
    let b = channel_arg(args, 2, "color")?;
    let a = if args.len() > 3 {
        channel_arg(args, 3, "color")?
    } else {
        255
    };
    Ok(to_js_color(Color::new(r, g, b, a)))
}

/// Install the `rl` namespace on the current realm's global object.
pub(crate) fn install(agent: &mut Agent) -> Result<(), JsError> {
    // Bind the window thread to the *first* installer. A later install on
    // another thread is still allowed (e.g. a second Context that never opens
    // a window) — the per-call guard keeps real raylib calls off the wrong
    // thread.
    WINDOW_THREAD.get_or_init(|| std::thread::current().id());

    let realm = agent.current_realm()?;
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| value.as_object());
    let rl = CruxObject::ordinary_object_create(object_proto);

    // Window and frame control.
    for (name, arity, body) in [
        (
            "initWindow",
            3u64,
            init_window as fn(&[Value]) -> Result<Value, JsError>,
        ),
        ("windowShouldClose", 0, window_should_close),
        ("closeWindow", 0, close_window),
        ("isWindowReady", 0, is_window_ready),
        ("setTargetFPS", 1, set_target_fps),
        ("getFPS", 0, get_fps),
        ("getFrameTime", 0, get_frame_time),
        ("getTime", 0, get_time),
        ("getScreenWidth", 0, get_screen_width),
        ("getScreenHeight", 0, get_screen_height),
        ("setExitKey", 1, set_exit_key),
        ("beginDrawing", 0, begin_drawing),
        ("endDrawing", 0, end_drawing),
        ("clearBackground", 1, clear_background),
        ("drawFPS", 2, draw_fps),
        ("drawText", 5, draw_text),
        ("measureText", 2, measure_text),
        ("drawCircle", 4, draw_circle),
        ("drawCircleLines", 4, draw_circle_lines),
        ("drawRectangle", 5, draw_rectangle),
        ("drawLine", 5, draw_line),
        ("drawPixel", 3, draw_pixel),
        ("beginMode3D", 7, begin_mode_3d),
        ("endMode3D", 0, end_mode_3d),
        ("drawCube", 7, draw_cube),
        ("drawCubeWires", 7, draw_cube_wires),
        ("drawGrid", 2, draw_grid),
        ("isKeyDown", 1, is_key_down),
        ("isKeyPressed", 1, is_key_pressed),
        ("isKeyReleased", 1, is_key_released),
        ("isKeyUp", 1, is_key_up),
        ("getKeyPressed", 0, get_key_pressed),
        ("isMouseButtonDown", 1, is_mouse_button_down),
        ("isMouseButtonPressed", 1, is_mouse_button_pressed),
        ("isMouseButtonReleased", 1, is_mouse_button_released),
        ("getMouseX", 0, get_mouse_x),
        ("getMouseY", 0, get_mouse_y),
        ("getMouseWheelMove", 0, get_mouse_wheel_move),
        ("getMouseDeltaX", 0, get_mouse_delta_x),
        ("getMouseDeltaY", 0, get_mouse_delta_y),
        ("disableCursor", 0, disable_cursor),
        ("enableCursor", 0, enable_cursor),
    ] {
        define(&rl, name, window_method(name, arity, body)?)?;
    }
    define(&rl, "color", plain_method("color", 3, color)?)?;

    // raylib's palette; `a` occupies the low byte so the packed form reads
    // `0xRRGGBBAA` in hex.
    for (name, r, g, b, a) in COLORS {
        rl.create_data_property_or_throw(
            &JsString::from_utf8(name),
            to_js_color(Color::new(*r, *g, *b, *a)),
        )?;
    }

    // Key codes: named scancodes, then ASCII letters and digits.
    for (name, code) in KEY_CODES {
        rl.create_data_property_or_throw(&JsString::from_utf8(name), Value::Number(*code as f64))?;
    }
    for code in 48..=57 {
        let name = format!("KEY_{}", (code as u8 as char).to_ascii_uppercase());
        rl.create_data_property_or_throw(&JsString::from_utf8(&name), Value::Number(code as f64))?;
    }
    for code in 65..=90 {
        let name = format!("KEY_{}", code as u8 as char);
        rl.create_data_property_or_throw(&JsString::from_utf8(&name), Value::Number(code as f64))?;
    }

    // Mouse buttons.
    rl.create_data_property_or_throw(
        &JsString::from_utf8("MOUSE_BUTTON_LEFT"),
        Value::Number(0.0),
    )?;
    rl.create_data_property_or_throw(
        &JsString::from_utf8("MOUSE_BUTTON_RIGHT"),
        Value::Number(1.0),
    )?;
    rl.create_data_property_or_throw(
        &JsString::from_utf8("MOUSE_BUTTON_MIDDLE"),
        Value::Number(2.0),
    )?;

    let global = realm.global_object;
    global.create_data_property_or_throw(&JsString::from_utf8("rl"), Value::Object(rl))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::embed::Context;

    #[test]
    fn installs_the_rl_namespace_with_constants_and_color_helper() {
        let mut context = Context::new().unwrap();
        context.install_raylib().unwrap();

        assert_eq!(
            context.eval("typeof rl").unwrap().as_string().as_deref(),
            Some("object")
        );
        // Colors round-trip through the 0xRRGGBBAA packing.
        assert_eq!(
            context
                .eval("rl.color(255, 0, 0, 255) === 0xFF0000FF")
                .unwrap()
                .as_boolean(),
            Some(true)
        );
        assert_eq!(
            context
                .eval("rl.RED === rl.color(230, 41, 55, 255)")
                .unwrap()
                .as_boolean(),
            Some(true)
        );
        // A omitted alpha defaults to opaque.
        assert_eq!(
            context.eval("rl.color(1, 2, 3)").unwrap().as_number(),
            Some(0x010203FF as f64)
        );
        // Key and mouse constants.
        assert_eq!(
            context.eval("rl.KEY_ESCAPE === 256").unwrap().as_boolean(),
            Some(true)
        );
        assert_eq!(
            context
                .eval("rl.KEY_Q === 81 && rl.MOUSE_BUTTON_LEFT === 0")
                .unwrap()
                .as_boolean(),
            Some(true)
        );
        // Draw calls installed but need a live window, so only check shape.
        assert_eq!(
            context
                .eval("typeof rl.drawCircle === 'function'")
                .unwrap()
                .as_boolean(),
            Some(true)
        );
    }

    #[test]
    fn channel_errors_name_the_offending_argument() {
        let mut context = Context::new().unwrap();
        context.install_raylib().unwrap();

        let error = match context.eval("rl.color(300, 0, 0)") {
            Ok(_) => panic!("rl.color with an out-of-range channel must throw"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("rl.color"), "{error}");
        assert!(error.contains("argument 0"), "{error}");
        assert!(error.contains("0..=255"), "{error}");
    }
}
