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
//!
//! With the additional `raygui` feature, raygui's `Gui*` controls are
//! installed on the same object as `rl.gui*` (see the gated `gui` module).

use std::ffi::CString;
use std::sync::{Mutex, OnceLock};
use std::thread::ThreadId;

use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::object::JsObject as CruxObject;
use crux::string::JsString;
use crux::value::{Value, ValueKind};
use raylib_sys::{
    BoundingBox, Camera3D, Color, Image, Model, ModelAnimation, Rectangle, Sound, Texture2D,
    Transform, Vector2, Vector3,
};

use crate::agent::Agent;

/// The thread that installed the module. raylib's window state is
/// process-global and must only be touched from this thread.
static WINDOW_THREAD: OnceLock<ThreadId> = OnceLock::new();

/// GPU textures created by `makeTexture`/`loadTexture`, indexed by handle.
static TEXTURES: Mutex<Vec<Texture2D>> = Mutex::new(Vec::new());

/// Sounds created by `loadSound`, indexed by handle.
static SOUNDS: Mutex<Vec<SoundSlot>> = Mutex::new(Vec::new());

/// raylib sounds are only ever touched from the single window thread (the
/// `window_method` guard), so the raw-pointer `Sound` can be shared behind a
/// mutex safely.
struct SoundSlot(Sound);
unsafe impl Send for SoundSlot {}
unsafe impl Sync for SoundSlot {}

/// Models created by `loadModel`, indexed by handle. raylib keeps a model's
/// animations in a separate array (loaded by `LoadModelAnimations`), so a slot
/// owns both and frees them together on `unloadModel`.
static MODELS: Mutex<Vec<ModelSlot>> = Mutex::new(Vec::new());

/// raylib models are only ever touched from the single window thread (the
/// `window_method` guard), so the raw-pointer payload can be shared behind a
/// mutex safely.
struct ModelSlot {
    model: Model,
    animations: *mut ModelAnimation,
    animation_count: i32,
    loaded: bool,
}

unsafe impl Send for ModelSlot {}
unsafe impl Sync for ModelSlot {}

/// The camera passed to the most recent `beginMode3D`, replayed for the
/// billboard draws: raylib's billboard API takes a whole `Camera3D`, but the
/// surface exposes only the individual fields, so the binding caches them.
static CAMERA_3D: Mutex<Option<Camera3D>> = Mutex::new(None);

/// Assets embedded into the binary by an embedder (see
/// `Context::register_raylib_asset`), looked up by their logical file name
/// before falling back to the disk.
static EMBEDDED_FILES: Mutex<Vec<(&'static str, &'static [u8])>> = Mutex::new(Vec::new());

/// Register an in-memory asset under `name` so `loadTexture`/`loadSound` can
/// find it without touching the disk.
pub(crate) fn register_embedded_asset(name: &'static str, data: &'static [u8]) {
    EMBEDDED_FILES.lock().unwrap().push((name, data));
}

/// The embedded bytes for `name`, if any.
fn embedded_asset(name: &str) -> Option<(&'static str, &'static [u8])> {
    let registry = EMBEDDED_FILES.lock().unwrap();
    registry
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(n, data)| (*n, *data))
}

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
    // Remember the camera for `drawBillboard`/`drawBillboardRec`.
    *CAMERA_3D.lock().unwrap() = Some(camera);
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

fn draw_sphere(args: &[Value]) -> Result<Value, JsError> {
    let x = num_arg(args, 0, "drawSphere")? as f32;
    let y = num_arg(args, 1, "drawSphere")? as f32;
    let z = num_arg(args, 2, "drawSphere")? as f32;
    let radius = num_arg(args, 3, "drawSphere")? as f32;
    let color = color_arg(args, 4, "drawSphere")?;
    // SAFETY: draw state on the installing thread (see `window_method`).
    unsafe { raylib_sys::DrawSphere(Vector3 { x, y, z }, radius, color) };
    Ok(Value::Undefined)
}

fn draw_sphere_ex(args: &[Value]) -> Result<Value, JsError> {
    let x = num_arg(args, 0, "drawSphereEx")? as f32;
    let y = num_arg(args, 1, "drawSphereEx")? as f32;
    let z = num_arg(args, 2, "drawSphereEx")? as f32;
    let radius = num_arg(args, 3, "drawSphereEx")? as f32;
    let rings = int_arg(args, 4, "drawSphereEx")?;
    let slices = int_arg(args, 5, "drawSphereEx")?;
    let color = color_arg(args, 6, "drawSphereEx")?;
    // SAFETY: as above.
    unsafe { raylib_sys::DrawSphereEx(Vector3 { x, y, z }, radius, rings, slices, color) };
    Ok(Value::Undefined)
}

fn draw_line_3d(args: &[Value]) -> Result<Value, JsError> {
    let x1 = num_arg(args, 0, "drawLine3D")? as f32;
    let y1 = num_arg(args, 1, "drawLine3D")? as f32;
    let z1 = num_arg(args, 2, "drawLine3D")? as f32;
    let x2 = num_arg(args, 3, "drawLine3D")? as f32;
    let y2 = num_arg(args, 4, "drawLine3D")? as f32;
    let z2 = num_arg(args, 5, "drawLine3D")? as f32;
    let color = color_arg(args, 6, "drawLine3D")?;
    // SAFETY: as above.
    unsafe {
        raylib_sys::DrawLine3D(
            Vector3 {
                x: x1,
                y: y1,
                z: z1,
            },
            Vector3 {
                x: x2,
                y: y2,
                z: z2,
            },
            color,
        )
    };
    Ok(Value::Undefined)
}

fn draw_point_3d(args: &[Value]) -> Result<Value, JsError> {
    let x = num_arg(args, 0, "drawPoint3D")? as f32;
    let y = num_arg(args, 1, "drawPoint3D")? as f32;
    let z = num_arg(args, 2, "drawPoint3D")? as f32;
    let color = color_arg(args, 3, "drawPoint3D")?;
    // SAFETY: as above.
    unsafe { raylib_sys::DrawPoint3D(Vector3 { x, y, z }, color) };
    Ok(Value::Undefined)
}

fn draw_triangle_3d(args: &[Value]) -> Result<Value, JsError> {
    let x1 = num_arg(args, 0, "drawTriangle3D")? as f32;
    let y1 = num_arg(args, 1, "drawTriangle3D")? as f32;
    let z1 = num_arg(args, 2, "drawTriangle3D")? as f32;
    let x2 = num_arg(args, 3, "drawTriangle3D")? as f32;
    let y2 = num_arg(args, 4, "drawTriangle3D")? as f32;
    let z2 = num_arg(args, 5, "drawTriangle3D")? as f32;
    let x3 = num_arg(args, 6, "drawTriangle3D")? as f32;
    let y3 = num_arg(args, 7, "drawTriangle3D")? as f32;
    let z3 = num_arg(args, 8, "drawTriangle3D")? as f32;
    let color = color_arg(args, 9, "drawTriangle3D")?;
    // SAFETY: as above. Vertices must be counter-clockwise.
    unsafe {
        raylib_sys::DrawTriangle3D(
            Vector3 {
                x: x1,
                y: y1,
                z: z1,
            },
            Vector3 {
                x: x2,
                y: y2,
                z: z2,
            },
            Vector3 {
                x: x3,
                y: y3,
                z: z3,
            },
            color,
        )
    };
    Ok(Value::Undefined)
}

/// Resolve a texture handle from the [`TEXTURES`] registry.
fn texture_arg(args: &[Value], index: usize, name: &str) -> Result<Texture2D, JsError> {
    let handle = int_arg(args, index, name)?;
    TEXTURES
        .lock()
        .unwrap()
        .get(handle as usize)
        .copied()
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                format!("rl.{name}: unknown texture {handle}"),
            )
        })
}

/// The camera captured by the most recent `beginMode3D`.
fn current_camera(name: &str) -> Result<Camera3D, JsError> {
    CAMERA_3D.lock().unwrap().ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            format!("rl.{name}: call beginMode3D first (billboards use the active camera)"),
        )
    })
}

fn draw_billboard(args: &[Value]) -> Result<Value, JsError> {
    let texture = texture_arg(args, 0, "drawBillboard")?;
    let x = num_arg(args, 1, "drawBillboard")? as f32;
    let y = num_arg(args, 2, "drawBillboard")? as f32;
    let z = num_arg(args, 3, "drawBillboard")? as f32;
    let size = num_arg(args, 4, "drawBillboard")? as f32;
    let tint = color_arg(args, 5, "drawBillboard")?;
    let camera = current_camera("drawBillboard")?;
    // SAFETY: draw state on the installing thread; the camera came from the
    // matching `beginMode3D`.
    unsafe { raylib_sys::DrawBillboard(camera, texture, Vector3 { x, y, z }, size, tint) };
    Ok(Value::Undefined)
}

fn draw_billboard_rec(args: &[Value]) -> Result<Value, JsError> {
    let texture = texture_arg(args, 0, "drawBillboardRec")?;
    let sx = num_arg(args, 1, "drawBillboardRec")? as f32;
    let sy = num_arg(args, 2, "drawBillboardRec")? as f32;
    let sw = num_arg(args, 3, "drawBillboardRec")? as f32;
    let sh = num_arg(args, 4, "drawBillboardRec")? as f32;
    let x = num_arg(args, 5, "drawBillboardRec")? as f32;
    let y = num_arg(args, 6, "drawBillboardRec")? as f32;
    let z = num_arg(args, 7, "drawBillboardRec")? as f32;
    let width = num_arg(args, 8, "drawBillboardRec")? as f32;
    let height = num_arg(args, 9, "drawBillboardRec")? as f32;
    let tint = color_arg(args, 10, "drawBillboardRec")?;
    let camera = current_camera("drawBillboardRec")?;
    // SAFETY: as above.
    unsafe {
        raylib_sys::DrawBillboardRec(
            camera,
            texture,
            Rectangle {
                x: sx,
                y: sy,
                width: sw,
                height: sh,
            },
            Vector3 { x, y, z },
            Vector2 {
                x: width,
                y: height,
            },
            tint,
        )
    };
    Ok(Value::Undefined)
}

// ---- models (needs the rmodels C module) ----

/// A model file raylib can open. An embedded asset is materialised to a temp
/// file for the duration of the load and removed afterwards: raylib's model
/// loaders take a *path*, and both the model and its animations are read
/// eagerly during the two load calls.
struct ModelFile {
    path: CString,
    temp: Option<std::path::PathBuf>,
}

impl ModelFile {
    fn open(name: &str) -> Result<ModelFile, JsError> {
        if let Some((_, data)) = embedded_asset(name) {
            let mut temp = std::env::temp_dir();
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0);
            temp.push(format!("slag-raylib-{stamp}.glb"));
            std::fs::write(&temp, data).map_err(|error| {
                JsError::new(
                    ErrorKind::TypeError,
                    format!("rl.loadModel: cannot materialise embedded asset {name}: {error}"),
                )
            })?;
            let path = CString::new(temp.to_string_lossy().as_bytes()).map_err(|_| {
                JsError::new(
                    ErrorKind::TypeError,
                    "rl.loadModel: temp path contains a NUL byte".into(),
                )
            })?;
            Ok(ModelFile {
                path,
                temp: Some(temp),
            })
        } else {
            let path = CString::new(name).map_err(|_| {
                JsError::new(
                    ErrorKind::TypeError,
                    "rl.loadModel: path contains a NUL byte".into(),
                )
            })?;
            Ok(ModelFile { path, temp: None })
        }
    }
}

impl Drop for ModelFile {
    fn drop(&mut self) {
        if let Some(path) = self.temp.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The model behind `handle`, if it is loaded.
fn model_arg(args: &[Value], index: usize, name: &str) -> Result<Model, JsError> {
    let handle = int_arg(args, index, name)?;
    let registry = MODELS.lock().unwrap();
    match registry.get(handle as usize) {
        Some(slot) if slot.loaded => Ok(slot.model),
        Some(_) => Err(JsError::new(
            ErrorKind::TypeError,
            format!("rl.{name}: model {handle} was unloaded"),
        )),
        None => Err(JsError::new(
            ErrorKind::TypeError,
            format!("rl.{name}: unknown model {handle}"),
        )),
    }
}

/// The animation at `index` behind `handle`. `ModelAnimation` is plain data, so
/// the element is read out by value; the registry keeps ownership.
fn animation_arg(handle: i32, index: i32, name: &str) -> Result<ModelAnimation, JsError> {
    let registry = MODELS.lock().unwrap();
    let slot = registry
        .get(handle as usize)
        .filter(|slot| slot.loaded)
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                format!("rl.{name}: unknown model {handle}"),
            )
        })?;
    if index < 0 || index >= slot.animation_count {
        return Err(JsError::new(
            ErrorKind::TypeError,
            format!(
                "rl.{name}: animation {index} out of range (model has {})",
                slot.animation_count
            ),
        ));
    }
    if slot.animations.is_null() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            format!("rl.{name}: model {handle} has no animations"),
        ));
    }
    // SAFETY: `index` is in range; the element is plain data owned by the
    // registry and only read (copied) here.
    Ok(unsafe { std::ptr::read(slot.animations.add(index as usize)) })
}

fn load_model(args: &[Value]) -> Result<Value, JsError> {
    let name = text_arg(args, 0, "loadModel")?;
    let name_str = name.to_string_lossy();
    let file = ModelFile::open(&name_str)?;
    // SAFETY: window-thread guard; raylib reads the file during this call.
    let model = unsafe { raylib_sys::LoadModel(file.path.as_ptr()) };
    // Gate on the loaded mesh data rather than raylib's own `IsModelValid`:
    // that check additionally demands an uploaded VBO for every non-null mesh
    // attribute, and the bone buffers are only uploaded when
    // `SUPPORT_GPU_SKINNING` is on — so it reports false for *every* skinned
    // model in a CPU-skinning build (which is what `rl` ships).
    if model.meshes.is_null() || model.meshCount <= 0 {
        return Ok(Value::Number(-1.0));
    }
    let mut animation_count: std::ffi::c_int = 0;
    // SAFETY: as above; the returned array is owned by us until `unloadModel`.
    let animations =
        unsafe { raylib_sys::LoadModelAnimations(file.path.as_ptr(), &mut animation_count) };
    drop(file);
    let mut registry = MODELS.lock().unwrap();
    registry.push(ModelSlot {
        model,
        animations,
        animation_count,
        loaded: true,
    });
    Ok(Value::Number((registry.len() - 1) as f64))
}

/// Whether `handle` refers to a model this module loaded and has not unloaded.
///
/// This deliberately does *not* forward raylib's own `IsModelValid`: that check
/// also requires an uploaded VBO for every non-null mesh attribute, and bone
/// buffers are only uploaded under `SUPPORT_GPU_SKINNING`, so raylib reports
/// every skinned model as invalid in the CPU-skinning build `rl` ships. The
/// useful question for a script is whether the handle is a live model.
fn is_model_valid(args: &[Value]) -> Result<Value, JsError> {
    let handle = int_arg(args, 0, "isModelValid")?;
    let registry = MODELS.lock().unwrap();
    let valid = registry
        .get(handle as usize)
        .is_some_and(|slot| slot.loaded);
    Ok(Value::Boolean(valid))
}

fn unload_model(args: &[Value]) -> Result<Value, JsError> {
    let handle = int_arg(args, 0, "unloadModel")?;
    let mut registry = MODELS.lock().unwrap();
    if let Some(slot) = registry.get_mut(handle as usize) {
        if slot.loaded {
            // SAFETY: window-thread guard; both allocations came from raylib
            // and are freed exactly once (the slot is then marked unloaded).
            unsafe {
                if !slot.animations.is_null() && slot.animation_count > 0 {
                    raylib_sys::UnloadModelAnimations(slot.animations, slot.animation_count);
                }
                raylib_sys::UnloadModel(slot.model);
            }
            slot.animations = std::ptr::null_mut();
            slot.animation_count = 0;
            slot.loaded = false;
        }
    }
    Ok(Value::Undefined)
}

fn draw_model(args: &[Value]) -> Result<Value, JsError> {
    let model = model_arg(args, 0, "drawModel")?;
    let x = num_arg(args, 1, "drawModel")? as f32;
    let y = num_arg(args, 2, "drawModel")? as f32;
    let z = num_arg(args, 3, "drawModel")? as f32;
    let scale = num_arg(args, 4, "drawModel")? as f32;
    let tint = color_arg(args, 5, "drawModel")?;
    // SAFETY: window-thread guard; draw state during begin/endDrawing.
    unsafe { raylib_sys::DrawModel(model, Vector3 { x, y, z }, scale, tint) };
    Ok(Value::Undefined)
}

fn draw_model_ex(args: &[Value]) -> Result<Value, JsError> {
    let model = model_arg(args, 0, "drawModelEx")?;
    let x = num_arg(args, 1, "drawModelEx")? as f32;
    let y = num_arg(args, 2, "drawModelEx")? as f32;
    let z = num_arg(args, 3, "drawModelEx")? as f32;
    let axis_x = num_arg(args, 4, "drawModelEx")? as f32;
    let axis_y = num_arg(args, 5, "drawModelEx")? as f32;
    let axis_z = num_arg(args, 6, "drawModelEx")? as f32;
    let angle = num_arg(args, 7, "drawModelEx")? as f32;
    let scale_x = num_arg(args, 8, "drawModelEx")? as f32;
    let scale_y = num_arg(args, 9, "drawModelEx")? as f32;
    let scale_z = num_arg(args, 10, "drawModelEx")? as f32;
    let tint = color_arg(args, 11, "drawModelEx")?;
    // SAFETY: as above. `rotationAngle` is in degrees, as raylib expects.
    unsafe {
        raylib_sys::DrawModelEx(
            model,
            Vector3 { x, y, z },
            Vector3 {
                x: axis_x,
                y: axis_y,
                z: axis_z,
            },
            angle,
            Vector3 {
                x: scale_x,
                y: scale_y,
                z: scale_z,
            },
            tint,
        )
    };
    Ok(Value::Undefined)
}

fn model_bounds(args: &[Value]) -> Result<Value, JsError> {
    let model = model_arg(args, 0, "modelBounds")?;
    // SAFETY: window-thread guard; reads the model's mesh vertex data.
    let bounds: BoundingBox = unsafe { raylib_sys::GetModelBoundingBox(model) };
    let object = CruxObject::ordinary_object_create(None);
    for (name, value) in [
        ("minX", bounds.min.x),
        ("minY", bounds.min.y),
        ("minZ", bounds.min.z),
        ("maxX", bounds.max.x),
        ("maxY", bounds.max.y),
        ("maxZ", bounds.max.z),
    ] {
        object.create_data_property_or_throw(
            &JsString::from_utf8(name),
            Value::Number(value as f64),
        )?;
    }
    Ok(Value::Object(object))
}

fn model_animation_count(args: &[Value]) -> Result<Value, JsError> {
    let handle = int_arg(args, 0, "modelAnimationCount")?;
    let registry = MODELS.lock().unwrap();
    let count = registry
        .get(handle as usize)
        .filter(|slot| slot.loaded)
        .map(|slot| slot.animation_count)
        .unwrap_or(0);
    Ok(Value::Number(count as f64))
}

/// How many bones the model's skeleton has (0 for a static mesh). Animation
/// playback is a no-op on a model without a skeleton.
fn model_bone_count(args: &[Value]) -> Result<Value, JsError> {
    let handle = int_arg(args, 0, "modelBoneCount")?;
    let registry = MODELS.lock().unwrap();
    let count = registry
        .get(handle as usize)
        .filter(|slot| slot.loaded)
        .map(|slot| slot.model.skeleton.boneCount)
        .unwrap_or(0);
    Ok(Value::Number(count as f64))
}

fn model_animation_name(args: &[Value]) -> Result<Value, JsError> {
    let handle = int_arg(args, 0, "modelAnimationName")?;
    let index = int_arg(args, 1, "modelAnimationName")?;
    let animation = animation_arg(handle, index, "modelAnimationName")?;
    let bytes: Vec<u8> = animation
        .name
        .iter()
        .take_while(|code| **code != 0)
        .map(|code| *code as u8)
        .collect();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok(Value::String(Handle::new(JsString::from_utf8(&text))))
}

fn model_animation_frame_count(args: &[Value]) -> Result<Value, JsError> {
    let handle = int_arg(args, 0, "modelAnimationFrameCount")?;
    let index = int_arg(args, 1, "modelAnimationFrameCount")?;
    let animation = animation_arg(handle, index, "modelAnimationFrameCount")?;
    Ok(Value::Number(animation.keyframeCount as f64))
}

/// The clip's duration in seconds. raylib resamples glTF animations at a fixed
/// 60 fps (`GLTF_FRAMERATE`) and stores `keyframeCount = duration * 60 + 1`, so
/// the duration is recoverable from the frame count.
fn model_animation_duration(args: &[Value]) -> Result<Value, JsError> {
    let handle = int_arg(args, 0, "modelAnimationDuration")?;
    let index = int_arg(args, 1, "modelAnimationDuration")?;
    let animation = animation_arg(handle, index, "modelAnimationDuration")?;
    const GLTF_FRAMERATE: f32 = 60.0;
    let duration = ((animation.keyframeCount - 1) as f32 / GLTF_FRAMERATE).max(0.0);
    Ok(Value::Number(duration as f64))
}

fn update_model_animation(args: &[Value]) -> Result<Value, JsError> {
    let handle = int_arg(args, 0, "updateModelAnimation")?;
    let model = model_arg(args, 0, "updateModelAnimation")?;
    let index = int_arg(args, 1, "updateModelAnimation")?;
    let frame = num_arg(args, 2, "updateModelAnimation")? as f32;
    let animation = animation_arg(handle, index, "updateModelAnimation")?;
    // SAFETY: window-thread guard; the model and animation stay alive in the
    // registry, and this only rewrites the model's pose buffers.
    unsafe { raylib_sys::UpdateModelAnimation(model, animation, frame) };
    Ok(Value::Undefined)
}

/// The current pose transform of `boneIndex`. raylib keeps the pose in
/// `model.currentPose` (model space), refreshed by `updateModelAnimation` and
/// initialised to the bind pose at load; the pose array is allocated for
/// `skeleton.boneCount` entries.
fn bone_pose(model: &Model, bone_index: i32, name: &str) -> Result<Transform, JsError> {
    if model.currentPose.is_null() || model.skeleton.boneCount <= 0 {
        return Err(JsError::new(
            ErrorKind::TypeError,
            format!("rl.{name}: model has no skeleton"),
        ));
    }
    if bone_index < 0 || bone_index >= model.skeleton.boneCount {
        return Err(JsError::new(
            ErrorKind::TypeError,
            format!(
                "rl.{name}: bone {bone_index} is out of range 0..{}",
                model.skeleton.boneCount
            ),
        ));
    }
    // SAFETY: window-thread guard; `currentPose` is non-null with
    // `boneCount` entries, and the bounds check above keeps the index inside.
    let current = unsafe { *model.currentPose.add(bone_index as usize) };
    // raylib zero-fills the runtime pose at load and only fills it from
    // `updateModelAnimation`, so a zero quaternion means no animation has been
    // applied yet: fall back to the bind pose rather than hand back a
    // degenerate transform.
    let rotation = current.rotation;
    if rotation.x == 0.0
        && rotation.y == 0.0
        && rotation.z == 0.0
        && rotation.w == 0.0
        && !model.skeleton.bindPose.is_null()
    {
        // SAFETY: `bindPose` is non-null with `boneCount` entries.
        return Ok(unsafe { *model.skeleton.bindPose.add(bone_index as usize) });
    }
    Ok(current)
}

fn float_object(entries: &[(&str, f32)]) -> Result<Value, JsError> {
    let object = CruxObject::ordinary_object_create(None);
    for (name, value) in entries {
        object.create_data_property_or_throw(
            &JsString::from_utf8(name),
            Value::Number(*value as f64),
        )?;
    }
    Ok(Value::Object(object))
}

fn model_bone_position(args: &[Value]) -> Result<Value, JsError> {
    let model = model_arg(args, 0, "modelBonePosition")?;
    let index = int_arg(args, 1, "modelBonePosition")?;
    let pose = bone_pose(&model, index, "modelBonePosition")?;
    float_object(&[
        ("x", pose.translation.x),
        ("y", pose.translation.y),
        ("z", pose.translation.z),
    ])
}

fn model_bone_transform(args: &[Value]) -> Result<Value, JsError> {
    let model = model_arg(args, 0, "modelBoneTransform")?;
    let index = int_arg(args, 1, "modelBoneTransform")?;
    let pose = bone_pose(&model, index, "modelBoneTransform")?;
    float_object(&[
        ("x", pose.translation.x),
        ("y", pose.translation.y),
        ("z", pose.translation.z),
        ("qx", pose.rotation.x),
        ("qy", pose.rotation.y),
        ("qz", pose.rotation.z),
        ("qw", pose.rotation.w),
    ])
}

// ---- textures (needs the rtextures C module) ----

/// Parse `w*h*8` hex digits (RRGGBBAA per pixel) into RGBA bytes.aa
fn rgba_hex_to_bytes(
    hex: &str,
    width: usize,
    height: usize,
    name: &str,
) -> Result<Vec<u8>, JsError> {
    let hex = hex.strip_prefix('#').unwrap_or(hex);
    let expected = width * height * 8;
    if hex.len() != expected {
        return Err(JsError::new(
            ErrorKind::TypeError,
            format!(
                "rl.{name}: expected {expected} hex digits, got {}",
                hex.len()
            ),
        ));
    }
    let mut bytes = Vec::with_capacity(width * height * 4);
    for pair in hex.as_bytes().chunks(2) {
        let text = std::str::from_utf8(pair)
            .map_err(|_| JsError::new(ErrorKind::TypeError, format!("rl.{name}: non-ASCII hex")))?;
        let byte = u8::from_str_radix(text, 16).map_err(|_| {
            JsError::new(
                ErrorKind::TypeError,
                format!("rl.{name}: invalid hex digit"),
            )
        })?;
        bytes.push(byte);
    }
    Ok(bytes)
}

fn make_texture(args: &[Value]) -> Result<Value, JsError> {
    let width = int_arg(args, 0, "makeTexture")?;
    let height = int_arg(args, 1, "makeTexture")?;
    let hex = match args.get(2).map(Value::kind) {
        Some(ValueKind::String(text)) => text.to_string_lossy(),
        _ => return Err(expected("makeTexture", 2, "an 8-hex-per-pixel string")),
    };
    let mut rgba = rgba_hex_to_bytes(&hex, width as usize, height as usize, "makeTexture")?;
    let image = Image {
        data: rgba.as_mut_ptr() as *mut std::ffi::c_void,
        width,
        height,
        mipmaps: 1,
        format: 7, // PIXELFORMAT_UNCOMPRESSED_R8G8B8A8
    };
    // SAFETY: the window-thread guard runs before this body; `image` points
    // at `rgba` (kept alive through the call) and the GPU copies the pixels.
    let texture = unsafe { raylib_sys::LoadTextureFromImage(image) };
    // SAFETY: as above. TEXTURE_FILTER_POINT keeps the chunky look.
    unsafe { raylib_sys::SetTextureFilter(texture, 0) };
    let mut registry = TEXTURES.lock().unwrap();
    registry.push(texture);
    Ok(Value::Number((registry.len() - 1) as f64))
}

fn draw_texture_rect(args: &[Value]) -> Result<Value, JsError> {
    let handle = int_arg(args, 0, "drawTexture")?;
    let sx = num_arg(args, 1, "drawTexture")? as f32;
    let sy = num_arg(args, 2, "drawTexture")? as f32;
    let sw = num_arg(args, 3, "drawTexture")? as f32;
    let sh = num_arg(args, 4, "drawTexture")? as f32;
    let dx = num_arg(args, 5, "drawTexture")? as f32;
    let dy = num_arg(args, 6, "drawTexture")? as f32;
    let dw = num_arg(args, 7, "drawTexture")? as f32;
    let dh = num_arg(args, 8, "drawTexture")? as f32;
    let shade = num_arg(args, 9, "drawTexture")?;
    let texture = TEXTURES
        .lock()
        .unwrap()
        .get(handle as usize)
        .copied()
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                format!("rl.drawTexture: unknown texture {handle}"),
            )
        })?;
    let light = shade.clamp(0.0, 1.0);
    let tint = Color::new(
        (255.0 * light) as u8,
        (255.0 * light) as u8,
        (255.0 * light) as u8,
        255,
    );
    // SAFETY: draw state on the installing thread (see `window_method`);
    // source/dest are full rectangles in texture/screen pixels.
    unsafe {
        raylib_sys::DrawTexturePro(
            texture,
            Rectangle {
                x: sx,
                y: sy,
                width: sw,
                height: sh,
            },
            Rectangle {
                x: dx,
                y: dy,
                width: dw,
                height: dh,
            },
            Vector2 { x: 0.0, y: 0.0 },
            0.0,
            tint,
        );
    }
    Ok(Value::Undefined)
}

fn texture_width(args: &[Value]) -> Result<Value, JsError> {
    let handle = int_arg(args, 0, "textureWidth")?;
    let width = TEXTURES
        .lock()
        .unwrap()
        .get(handle as usize)
        .map(|t| t.width as f64)
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                format!("rl.textureWidth: unknown texture {handle}"),
            )
        })?;
    Ok(Value::Number(width))
}

fn texture_height(args: &[Value]) -> Result<Value, JsError> {
    let handle = int_arg(args, 0, "textureHeight")?;
    let height = TEXTURES
        .lock()
        .unwrap()
        .get(handle as usize)
        .map(|t| t.height as f64)
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                format!("rl.textureHeight: unknown texture {handle}"),
            )
        })?;
    Ok(Value::Number(height))
}

// ---- audio + file-loaded assets (needs the raudio C module) ----

fn init_audio_device(_args: &[Value]) -> Result<Value, JsError> {
    // SAFETY: window-thread guard; init/use/close all on the same thread.
    unsafe { raylib_sys::InitAudioDevice() };
    Ok(Value::Undefined)
}

fn close_audio_device(_args: &[Value]) -> Result<Value, JsError> {
    // SAFETY: as above.
    unsafe { raylib_sys::CloseAudioDevice() };
    Ok(Value::Undefined)
}

fn load_texture_from_file(args: &[Value]) -> Result<Value, JsError> {
    let path = text_arg(args, 0, "loadTexture")?;
    let path_str = path.to_string_lossy();
    let texture = if let Some((_, data)) = embedded_asset(&path_str) {
        let format = CString::new(".png").unwrap();
        // SAFETY: window-thread guard; raylib decodes from our static bytes.
        let image = unsafe {
            raylib_sys::LoadImageFromMemory(format.as_ptr(), data.as_ptr(), data.len() as i32)
        };
        if image.width <= 0 || image.height <= 0 {
            return Ok(Value::Number(-1.0));
        }
        let texture = unsafe { raylib_sys::LoadTextureFromImage(image) };
        unsafe { raylib_sys::UnloadImage(image) };
        texture
    } else {
        // SAFETY: raylib reads the file during this call; window-thread guard.
        unsafe { raylib_sys::LoadTexture(path.as_ptr()) }
    };
    if texture.id == 0 {
        return Ok(Value::Number(-1.0));
    }
    let mut registry = TEXTURES.lock().unwrap();
    registry.push(texture);
    Ok(Value::Number((registry.len() - 1) as f64))
}

fn load_sound_from_file(args: &[Value]) -> Result<Value, JsError> {
    let path = text_arg(args, 0, "loadSound")?;
    let path_str = path.to_string_lossy();
    let sound = if let Some((_, data)) = embedded_asset(&path_str) {
        let format = CString::new(".wav").unwrap();
        // SAFETY: window-thread guard; raylib decodes from our static bytes.
        let wave = unsafe {
            raylib_sys::LoadWaveFromMemory(format.as_ptr(), data.as_ptr(), data.len() as i32)
        };
        if wave.frameCount == 0 {
            return Ok(Value::Number(-1.0));
        }
        let sound = unsafe { raylib_sys::LoadSoundFromWave(wave) };
        unsafe { raylib_sys::UnloadWave(wave) };
        sound
    } else {
        // SAFETY: as above; the returned handle keeps the decoded samples alive.
        unsafe { raylib_sys::LoadSound(path.as_ptr()) }
    };
    if sound.frameCount == 0 {
        return Ok(Value::Number(-1.0));
    }
    let mut registry = SOUNDS.lock().unwrap();
    registry.push(SoundSlot(sound));
    Ok(Value::Number((registry.len() - 1) as f64))
}

fn sound_handle(args: &[Value], name: &str) -> Result<Sound, JsError> {
    let handle = int_arg(args, 0, name)?;
    SOUNDS
        .lock()
        .unwrap()
        .get(handle as usize)
        .map(|slot| slot.0)
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                format!("rl.{name}: unknown sound {handle}"),
            )
        })
}

fn play_sound(args: &[Value]) -> Result<Value, JsError> {
    let sound = sound_handle(args, "playSound")?;
    // SAFETY: as above.
    unsafe { raylib_sys::PlaySound(sound) };
    Ok(Value::Undefined)
}

fn stop_sound(args: &[Value]) -> Result<Value, JsError> {
    let sound = sound_handle(args, "stopSound")?;
    // SAFETY: as above.
    unsafe { raylib_sys::StopSound(sound) };
    Ok(Value::Undefined)
}

fn set_sound_volume(args: &[Value]) -> Result<Value, JsError> {
    let sound = sound_handle(args, "setSoundVolume")?;
    let volume = num_arg(args, 1, "setSoundVolume")?.clamp(0.0, 1.0) as f32;
    // SAFETY: as above.
    unsafe { raylib_sys::SetSoundVolume(sound, volume) };
    Ok(Value::Undefined)
}

// ---- window ----

fn init_window(args: &[Value]) -> Result<Value, JsError> {
    let width = int_arg(args, 0, "initWindow")?;
    let height = int_arg(args, 1, "initWindow")?;
    let title = text_arg(args, 2, "initWindow")?;
    // SAFETY: all calls on the installing thread (see `window_method`); raylib
    // copies `title` before returning.
    //
    // Without FLAG_WINDOW_HIGHDPI raylib assumes the GL drawable matches the
    // requested logical size. On a Wayland build with a scale > 1 display
    // that is false: raylib's GLFW backend leaves GLFW_SCALE_FRAMEBUFFER
    // enabled (the Wayland-only code that disables it does not compile —
    // `_GLFW_WAYLAND` is never defined for raylib's own sources), so the
    // drawable is `scale` times larger than the window while raylib renders
    // into the WIDTH x HEIGHT viewport, leaving the game in a corner of the
    // window. HIGHDPI makes raylib size its render surface to the actual
    // framebuffer; at scale 1 it is a no-op.
    unsafe {
        raylib_sys::SetConfigFlags(raylib_sys::ConfigFlags::FLAG_WINDOW_HIGHDPI as u32);
        raylib_sys::InitWindow(width, height, title.as_ptr())
    };
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

fn draw_rectangle_lines(args: &[Value]) -> Result<Value, JsError> {
    let x = int_arg(args, 0, "drawRectangleLines")?;
    let y = int_arg(args, 1, "drawRectangleLines")?;
    let width = int_arg(args, 2, "drawRectangleLines")?;
    let height = int_arg(args, 3, "drawRectangleLines")?;
    let color = color_arg(args, 4, "drawRectangleLines")?;
    // SAFETY: as above.
    unsafe { raylib_sys::DrawRectangleLines(x, y, width, height, color) };
    Ok(Value::Undefined)
}

fn draw_rectangle_gradient_v(args: &[Value]) -> Result<Value, JsError> {
    let x = int_arg(args, 0, "drawRectangleGradientV")?;
    let y = int_arg(args, 1, "drawRectangleGradientV")?;
    let width = int_arg(args, 2, "drawRectangleGradientV")?;
    let height = int_arg(args, 3, "drawRectangleGradientV")?;
    let top = color_arg(args, 4, "drawRectangleGradientV")?;
    let bottom = color_arg(args, 5, "drawRectangleGradientV")?;
    // SAFETY: as above.
    unsafe { raylib_sys::DrawRectangleGradientV(x, y, width, height, top, bottom) };
    Ok(Value::Undefined)
}

fn draw_text_ex(args: &[Value]) -> Result<Value, JsError> {
    let text = text_arg(args, 0, "drawTextEx")?;
    let x = num_arg(args, 1, "drawTextEx")? as f32;
    let y = num_arg(args, 2, "drawTextEx")? as f32;
    let size = num_arg(args, 3, "drawTextEx")? as f32;
    let spacing = num_arg(args, 4, "drawTextEx")? as f32;
    let tint = color_arg(args, 5, "drawTextEx")?;
    // SAFETY: the default font is loaded lazily and cached process-globally;
    // raylib reads the text only for the duration of the call.
    let font = unsafe { raylib_sys::GetFontDefault() };
    unsafe { raylib_sys::DrawTextEx(font, text.as_ptr(), Vector2 { x, y }, size, spacing, tint) };
    Ok(Value::Undefined)
}

fn measure_text_ex(args: &[Value]) -> Result<Value, JsError> {
    let text = text_arg(args, 0, "measureTextEx")?;
    let size = num_arg(args, 1, "measureTextEx")? as f32;
    let spacing = num_arg(args, 2, "measureTextEx")? as f32;
    // SAFETY: as above.
    let font = unsafe { raylib_sys::GetFontDefault() };
    let measured = unsafe { raylib_sys::MeasureTextEx(font, text.as_ptr(), size, spacing) };
    let object = CruxObject::ordinary_object_create(None);
    for (name, value) in [("x", measured.x), ("y", measured.y)] {
        object.create_data_property_or_throw(
            &JsString::from_utf8(name),
            Value::Number(value as f64),
        )?;
    }
    Ok(Value::Object(object))
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

// ---- raygui controls (needs the runtime `raygui` feature) ----
//
// The `rl.gui*` surface mirrors raygui's immediate-mode controls, drawn
// inside `beginDrawing`/`endDrawing` like the `draw*` helpers above. A
// control that owns state across frames (a checkbox, slider, ...) holds it
// through a C pointer in raygui, so its wrapper takes the *current* value as
// an argument and returns `{ action, value }`: feed `value` back in on the
// next frame and treat `action` as raygui's own return (nonzero means
// clicked / changed / Enter pressed, depending on the control). Text-heavy
// lists (`;`-separated, like combo/dropdown items) are passed as plain
// strings exactly as raygui expects.

#[cfg(feature = "raygui")]
mod gui {
    use super::*;

    /// raygui's `GuiState` enum (`STATE_NORMAL`..`STATE_DISABLED`) as JS
    /// constants for `guiSetState`.
    const GUI_STATES: &[(&str, i32)] = &[
        ("GUI_STATE_NORMAL", 0),
        ("GUI_STATE_FOCUSED", 1),
        ("GUI_STATE_PRESSED", 2),
        ("GUI_STATE_DISABLED", 3),
    ];

    /// A control's `(x, y, width, height)` bounds, the first four arguments
    /// of every control, flattened like the `draw*` rectangle helpers above.
    fn bounds(args: &[Value], name: &str) -> Result<Rectangle, JsError> {
        Ok(Rectangle {
            x: num_arg(args, 0, name)? as f32,
            y: num_arg(args, 1, name)? as f32,
            width: num_arg(args, 2, name)? as f32,
            height: num_arg(args, 3, name)? as f32,
        })
    }

    fn bool_arg(args: &[Value], index: usize, name: &str) -> Result<bool, JsError> {
        match args.get(index).map(Value::kind) {
            Some(ValueKind::Boolean(value)) => Ok(value),
            _ => Err(expected(name, index, "a boolean")),
        }
    }

    /// Pack raygui's `(return value, out-parameter state)` pair into a JS
    /// object. The updated state flows back through `value` because
    /// JavaScript has no pass-by-reference; `action` keeps raygui's own
    /// return (1 when the control was activated) for callers that branch.
    fn pair(action: i32, value: Value) -> Result<Value, JsError> {
        let result = CruxObject::ordinary_object_create(None);
        result.create_data_property_or_throw(
            &JsString::from_utf8("action"),
            Value::Number(action as f64),
        )?;
        result.create_data_property_or_throw(&JsString::from_utf8("value"), value)?;
        Ok(Value::Object(result))
    }

    // SAFETY for every call below: raygui is synchronous immediate-mode C.
    // Each function consumes its arguments (stack locals or a transient
    // buffer) before returning and never retains the pointers, and the
    // `window_method` guard keeps every call on the thread that installed
    // the module, exactly like the other `rl.*` drawing calls.

    // ---- global state / style ----

    fn gui_enable(_args: &[Value]) -> Result<Value, JsError> {
        unsafe { raylib_sys::GuiEnable() };
        Ok(Value::Undefined)
    }

    fn gui_disable(_args: &[Value]) -> Result<Value, JsError> {
        unsafe { raylib_sys::GuiDisable() };
        Ok(Value::Undefined)
    }

    fn gui_lock(_args: &[Value]) -> Result<Value, JsError> {
        unsafe { raylib_sys::GuiLock() };
        Ok(Value::Undefined)
    }

    fn gui_unlock(_args: &[Value]) -> Result<Value, JsError> {
        unsafe { raylib_sys::GuiUnlock() };
        Ok(Value::Undefined)
    }

    fn gui_is_locked(_args: &[Value]) -> Result<Value, JsError> {
        let locked = unsafe { raylib_sys::GuiIsLocked() };
        Ok(Value::Boolean(locked))
    }

    fn gui_set_alpha(args: &[Value]) -> Result<Value, JsError> {
        let alpha = num_arg(args, 0, "guiSetAlpha")?;
        if !(0.0..=1.0).contains(&alpha) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "rl.guiSetAlpha: argument 0 must be an alpha in 0..=1".into(),
            ));
        }
        unsafe { raylib_sys::GuiSetAlpha(alpha as f32) };
        Ok(Value::Undefined)
    }

    fn gui_set_state(args: &[Value]) -> Result<Value, JsError> {
        let state = int_arg(args, 0, "guiSetState")?;
        // raygui indexes its style arrays by state, so reject values outside
        // the enum instead of letting a bad state read out of bounds later.
        if !(0..=3).contains(&state) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "rl.guiSetState: argument 0 must be one of the rl.GUI_STATE_* constants".into(),
            ));
        }
        unsafe { raylib_sys::GuiSetState(state) };
        Ok(Value::Undefined)
    }

    fn gui_get_state(_args: &[Value]) -> Result<Value, JsError> {
        let state = unsafe { raylib_sys::GuiGetState() };
        Ok(Value::Number(state as f64))
    }

    fn gui_load_style_default(_args: &[Value]) -> Result<Value, JsError> {
        unsafe { raylib_sys::GuiLoadStyleDefault() };
        Ok(Value::Undefined)
    }

    // ---- controls without owned state ----

    fn gui_window_box(args: &[Value]) -> Result<Value, JsError> {
        let bounds = bounds(args, "guiWindowBox")?;
        let title = text_arg(args, 4, "guiWindowBox")?;
        let close = unsafe { raylib_sys::GuiWindowBox(bounds, title.as_ptr()) != 0 };
        Ok(Value::Boolean(close))
    }

    fn gui_group_box(args: &[Value]) -> Result<Value, JsError> {
        let bounds = bounds(args, "guiGroupBox")?;
        let text = text_arg(args, 4, "guiGroupBox")?;
        unsafe { raylib_sys::GuiGroupBox(bounds, text.as_ptr()) };
        Ok(Value::Undefined)
    }

    fn gui_line(args: &[Value]) -> Result<Value, JsError> {
        let bounds = bounds(args, "guiLine")?;
        let text = text_arg(args, 4, "guiLine")?;
        unsafe { raylib_sys::GuiLine(bounds, text.as_ptr()) };
        Ok(Value::Undefined)
    }

    fn gui_panel(args: &[Value]) -> Result<Value, JsError> {
        let bounds = bounds(args, "guiPanel")?;
        let text = text_arg(args, 4, "guiPanel")?;
        unsafe { raylib_sys::GuiPanel(bounds, text.as_ptr()) };
        Ok(Value::Undefined)
    }

    fn gui_label(args: &[Value]) -> Result<Value, JsError> {
        let bounds = bounds(args, "guiLabel")?;
        let text = text_arg(args, 4, "guiLabel")?;
        unsafe { raylib_sys::GuiLabel(bounds, text.as_ptr()) };
        Ok(Value::Undefined)
    }

    fn gui_button(args: &[Value]) -> Result<Value, JsError> {
        let bounds = bounds(args, "guiButton")?;
        let text = text_arg(args, 4, "guiButton")?;
        let clicked = unsafe { raylib_sys::GuiButton(bounds, text.as_ptr()) != 0 };
        Ok(Value::Boolean(clicked))
    }

    fn gui_label_button(args: &[Value]) -> Result<Value, JsError> {
        let bounds = bounds(args, "guiLabelButton")?;
        let text = text_arg(args, 4, "guiLabelButton")?;
        let clicked = unsafe { raylib_sys::GuiLabelButton(bounds, text.as_ptr()) != 0 };
        Ok(Value::Boolean(clicked))
    }

    fn gui_status_bar(args: &[Value]) -> Result<Value, JsError> {
        let bounds = bounds(args, "guiStatusBar")?;
        let text = text_arg(args, 4, "guiStatusBar")?;
        unsafe { raylib_sys::GuiStatusBar(bounds, text.as_ptr()) };
        Ok(Value::Undefined)
    }

    fn gui_dummy_rec(args: &[Value]) -> Result<Value, JsError> {
        let bounds = bounds(args, "guiDummyRec")?;
        let text = text_arg(args, 4, "guiDummyRec")?;
        unsafe { raylib_sys::GuiDummyRec(bounds, text.as_ptr()) };
        Ok(Value::Undefined)
    }

    /// `guiMessageBox` returns the 1-based index of the clicked button, 0
    /// when its window is closed, and -1 while it just sits on screen.
    fn gui_message_box(args: &[Value]) -> Result<Value, JsError> {
        let bounds = bounds(args, "guiMessageBox")?;
        let title = text_arg(args, 4, "guiMessageBox")?;
        let message = text_arg(args, 5, "guiMessageBox")?;
        let buttons = text_arg(args, 6, "guiMessageBox")?;
        let clicked = unsafe {
            raylib_sys::GuiMessageBox(bounds, title.as_ptr(), message.as_ptr(), buttons.as_ptr())
        };
        Ok(Value::Number(clicked as f64))
    }

    // ---- controls with owned state: `{ action, value }` ----

    fn gui_toggle(args: &[Value]) -> Result<Value, JsError> {
        let bounds = bounds(args, "guiToggle")?;
        let text = text_arg(args, 4, "guiToggle")?;
        let mut active = bool_arg(args, 5, "guiToggle")?;
        let action = unsafe { raylib_sys::GuiToggle(bounds, text.as_ptr(), &mut active) };
        pair(action, Value::Boolean(active))
    }

    fn gui_check_box(args: &[Value]) -> Result<Value, JsError> {
        let bounds = bounds(args, "guiCheckBox")?;
        let text = text_arg(args, 4, "guiCheckBox")?;
        let mut checked = bool_arg(args, 5, "guiCheckBox")?;
        let action = unsafe { raylib_sys::GuiCheckBox(bounds, text.as_ptr(), &mut checked) };
        pair(action, Value::Boolean(checked))
    }

    fn gui_slider(args: &[Value]) -> Result<Value, JsError> {
        let bounds = bounds(args, "guiSlider")?;
        let text_left = text_arg(args, 4, "guiSlider")?;
        let text_right = text_arg(args, 5, "guiSlider")?;
        let mut value = num_arg(args, 6, "guiSlider")? as f32;
        let min_value = num_arg(args, 7, "guiSlider")? as f32;
        let max_value = num_arg(args, 8, "guiSlider")? as f32;
        let action = unsafe {
            raylib_sys::GuiSlider(
                bounds,
                text_left.as_ptr(),
                text_right.as_ptr(),
                &mut value,
                min_value,
                max_value,
            )
        };
        pair(action, Value::Number(value as f64))
    }

    fn gui_slider_bar(args: &[Value]) -> Result<Value, JsError> {
        let bounds = bounds(args, "guiSliderBar")?;
        let text_left = text_arg(args, 4, "guiSliderBar")?;
        let text_right = text_arg(args, 5, "guiSliderBar")?;
        let mut value = num_arg(args, 6, "guiSliderBar")? as f32;
        let min_value = num_arg(args, 7, "guiSliderBar")? as f32;
        let max_value = num_arg(args, 8, "guiSliderBar")? as f32;
        let action = unsafe {
            raylib_sys::GuiSliderBar(
                bounds,
                text_left.as_ptr(),
                text_right.as_ptr(),
                &mut value,
                min_value,
                max_value,
            )
        };
        pair(action, Value::Number(value as f64))
    }

    fn gui_progress_bar(args: &[Value]) -> Result<Value, JsError> {
        let bounds = bounds(args, "guiProgressBar")?;
        let text_left = text_arg(args, 4, "guiProgressBar")?;
        let text_right = text_arg(args, 5, "guiProgressBar")?;
        let mut value = num_arg(args, 6, "guiProgressBar")? as f32;
        let min_value = num_arg(args, 7, "guiProgressBar")? as f32;
        let max_value = num_arg(args, 8, "guiProgressBar")? as f32;
        let action = unsafe {
            raylib_sys::GuiProgressBar(
                bounds,
                text_left.as_ptr(),
                text_right.as_ptr(),
                &mut value,
                min_value,
                max_value,
            )
        };
        pair(action, Value::Number(value as f64))
    }

    fn gui_combo_box(args: &[Value]) -> Result<Value, JsError> {
        let bounds = bounds(args, "guiComboBox")?;
        let text = text_arg(args, 4, "guiComboBox")?;
        let mut active = int_arg(args, 5, "guiComboBox")?;
        let action = unsafe { raylib_sys::GuiComboBox(bounds, text.as_ptr(), &mut active) };
        pair(action, Value::Number(active as f64))
    }

    fn gui_dropdown_box(args: &[Value]) -> Result<Value, JsError> {
        let bounds = bounds(args, "guiDropdownBox")?;
        let text = text_arg(args, 4, "guiDropdownBox")?;
        let mut active = int_arg(args, 5, "guiDropdownBox")?;
        let edit_mode = bool_arg(args, 6, "guiDropdownBox")?;
        let action =
            unsafe { raylib_sys::GuiDropdownBox(bounds, text.as_ptr(), &mut active, edit_mode) };
        pair(action, Value::Number(active as f64))
    }

    fn gui_text_box(args: &[Value]) -> Result<Value, JsError> {
        let bounds = bounds(args, "guiTextBox")?;
        // raygui edits the buffer in place and needs headroom past the
        // current contents, so copy the JS string out with spare capacity
        // (at least 256 bytes) and read the result back after the call.
        let initial = match args.get(4).map(Value::kind) {
            Some(ValueKind::String(text)) => text.to_string_lossy(),
            _ => return Err(expected("guiTextBox", 4, "a string")),
        };
        let mut buffer = CString::new(initial.as_bytes())
            .map_err(|_| {
                JsError::new(
                    ErrorKind::TypeError,
                    "rl.guiTextBox: argument 4 contains a NUL byte".into(),
                )
            })?
            .into_bytes_with_nul();
        let capacity = buffer.len().max(256);
        buffer.resize(capacity, 0);
        let edit_mode = bool_arg(args, 5, "guiTextBox")?;
        let action = unsafe {
            raylib_sys::GuiTextBox(
                bounds,
                buffer.as_mut_ptr() as *mut std::ffi::c_char,
                capacity as i32,
                edit_mode,
            )
        };
        // SAFETY: raygui keeps the buffer NUL-terminated after every edit.
        let edited =
            unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr() as *const std::ffi::c_char) }
                .to_string_lossy()
                .into_owned();
        pair(
            action,
            Value::String(Handle::new(JsString::from_utf8(&edited))),
        )
    }

    fn gui_color_picker(args: &[Value]) -> Result<Value, JsError> {
        let bounds = bounds(args, "guiColorPicker")?;
        let text = text_arg(args, 4, "guiColorPicker")?;
        let mut color = color_arg(args, 5, "guiColorPicker")?;
        let action = unsafe { raylib_sys::GuiColorPicker(bounds, text.as_ptr(), &mut color) };
        pair(action, to_js_color(color))
    }

    /// Install the `rl.gui*` methods and constants on the `rl` namespace.
    pub(super) fn install(rl: &CruxObject) -> Result<(), JsError> {
        for (name, arity, body) in [
            (
                "guiEnable",
                0u64,
                gui_enable as fn(&[Value]) -> Result<Value, JsError>,
            ),
            ("guiDisable", 0, gui_disable),
            ("guiLock", 0, gui_lock),
            ("guiUnlock", 0, gui_unlock),
            ("guiIsLocked", 0, gui_is_locked),
            ("guiSetAlpha", 1, gui_set_alpha),
            ("guiSetState", 1, gui_set_state),
            ("guiGetState", 0, gui_get_state),
            ("guiLoadStyleDefault", 0, gui_load_style_default),
            ("guiWindowBox", 5, gui_window_box),
            ("guiGroupBox", 5, gui_group_box),
            ("guiLine", 5, gui_line),
            ("guiPanel", 5, gui_panel),
            ("guiLabel", 5, gui_label),
            ("guiButton", 5, gui_button),
            ("guiLabelButton", 5, gui_label_button),
            ("guiStatusBar", 5, gui_status_bar),
            ("guiDummyRec", 5, gui_dummy_rec),
            ("guiMessageBox", 7, gui_message_box),
            ("guiToggle", 6, gui_toggle),
            ("guiCheckBox", 6, gui_check_box),
            ("guiSlider", 9, gui_slider),
            ("guiSliderBar", 9, gui_slider_bar),
            ("guiProgressBar", 9, gui_progress_bar),
            ("guiComboBox", 6, gui_combo_box),
            ("guiDropdownBox", 7, gui_dropdown_box),
            ("guiTextBox", 6, gui_text_box),
            ("guiColorPicker", 6, gui_color_picker),
        ] {
            define(rl, name, window_method(name, arity, body)?)?;
        }
        for (name, state) in GUI_STATES {
            rl.create_data_property_or_throw(
                &JsString::from_utf8(name),
                Value::Number(*state as f64),
            )?;
        }
        Ok(())
    }
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
        ("drawRectangleLines", 5, draw_rectangle_lines),
        ("drawRectangleGradientV", 6, draw_rectangle_gradient_v),
        ("drawTextEx", 6, draw_text_ex),
        ("measureTextEx", 3, measure_text_ex),
        ("beginMode3D", 7, begin_mode_3d),
        ("endMode3D", 0, end_mode_3d),
        ("drawCube", 7, draw_cube),
        ("drawCubeWires", 7, draw_cube_wires),
        ("drawGrid", 2, draw_grid),
        ("drawSphere", 5, draw_sphere),
        ("drawSphereEx", 7, draw_sphere_ex),
        ("drawLine3D", 7, draw_line_3d),
        ("drawPoint3D", 4, draw_point_3d),
        ("drawTriangle3D", 10, draw_triangle_3d),
        ("drawBillboard", 6, draw_billboard),
        ("drawBillboardRec", 11, draw_billboard_rec),
        ("loadModel", 1, load_model),
        ("isModelValid", 1, is_model_valid),
        ("unloadModel", 1, unload_model),
        ("drawModel", 6, draw_model),
        ("drawModelEx", 12, draw_model_ex),
        ("modelBounds", 1, model_bounds),
        ("modelAnimationCount", 1, model_animation_count),
        ("modelBoneCount", 1, model_bone_count),
        ("modelAnimationName", 2, model_animation_name),
        ("modelAnimationFrameCount", 2, model_animation_frame_count),
        ("modelAnimationDuration", 2, model_animation_duration),
        ("updateModelAnimation", 3, update_model_animation),
        ("modelBonePosition", 2, model_bone_position),
        ("modelBoneTransform", 2, model_bone_transform),
        ("makeTexture", 3, make_texture),
        ("drawTexture", 10, draw_texture_rect),
        ("textureWidth", 1, texture_width),
        ("textureHeight", 1, texture_height),
        ("initAudioDevice", 0, init_audio_device),
        ("closeAudioDevice", 0, close_audio_device),
        ("loadTexture", 1, load_texture_from_file),
        ("loadSound", 1, load_sound_from_file),
        ("playSound", 1, play_sound),
        ("stopSound", 1, stop_sound),
        ("setSoundVolume", 2, set_sound_volume),
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

    #[cfg(feature = "raygui")]
    gui::install(&rl)?;

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

    // raylib's process-global window state binds to the thread of the first
    // install, and the libtest harness runs every #[test] on its own thread,
    // so all surface checks share one test: a guarded call from a parallel
    // second installer would throw the thread-bound TypeError instead of
    // running its assertion.
    #[test]
    fn installs_the_rl_surface_with_constants_and_argument_validation() {
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
        // An omitted alpha defaults to opaque.
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
        // The new 3D primitives, billboards and HUD helpers are installed;
        // calling them needs a live window, so only the shape is checked.
        for name in [
            "drawSphere",
            "drawSphereEx",
            "drawLine3D",
            "drawPoint3D",
            "drawTriangle3D",
            "drawBillboard",
            "drawBillboardRec",
            "drawRectangleLines",
            "drawRectangleGradientV",
            "drawTextEx",
            "measureTextEx",
        ] {
            assert_eq!(
                context
                    .eval(&format!("typeof rl.{name}"))
                    .unwrap()
                    .as_string()
                    .as_deref(),
                Some("function"),
                "rl.{name}"
            );
        }
        // Model bindings are installed; loading needs a window, so only the
        // shape and the non-window handle checks run here.
        for name in [
            "loadModel",
            "isModelValid",
            "unloadModel",
            "drawModel",
            "drawModelEx",
            "modelBounds",
            "modelAnimationCount",
            "modelBoneCount",
            "modelAnimationName",
            "modelAnimationFrameCount",
            "modelAnimationDuration",
            "updateModelAnimation",
            "modelBonePosition",
            "modelBoneTransform",
        ] {
            assert_eq!(
                context
                    .eval(&format!("typeof rl.{name}"))
                    .unwrap()
                    .as_string()
                    .as_deref(),
                Some("function"),
                "rl.{name}"
            );
        }
        // A handle that was never handed out is simply invalid, not an error.
        assert_eq!(
            context.eval("rl.isModelValid(-1)").unwrap().as_boolean(),
            Some(false)
        );
        assert_eq!(
            context
                .eval("rl.modelAnimationCount(-1)")
                .unwrap()
                .as_number(),
            Some(0.0)
        );
        // Using an unknown model throws a TypeError naming the call.
        let error = match context.eval("rl.updateModelAnimation(-1, 0, 0)") {
            Ok(_) => panic!("rl.updateModelAnimation with an unknown model must throw"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("rl.updateModelAnimation"), "{error}");

        // rl.color argument validation names the offending argument.
        let error = match context.eval("rl.color(300, 0, 0)") {
            Ok(_) => panic!("rl.color with an out-of-range channel must throw"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("rl.color"), "{error}");
        assert!(error.contains("argument 0"), "{error}");
        assert!(error.contains("0..=255"), "{error}");

        #[cfg(feature = "raygui")]
        {
            // Controls are installed as `rl.gui*` methods; only their shape is
            // checked here — drawing them needs a live window.
            assert_eq!(
                context
                    .eval(
                        "typeof rl.guiButton === 'function' && typeof rl.guiSlider === 'function'"
                    )
                    .unwrap()
                    .as_boolean(),
                Some(true)
            );
            assert_eq!(
                context.eval("rl.GUI_STATE_DISABLED").unwrap().as_number(),
                Some(3.0)
            );
            // States outside the enum are rejected instead of letting raygui
            // index its style arrays out of bounds on a later draw.
            let error = match context.eval("rl.guiSetState(7)") {
                Ok(_) => panic!("rl.guiSetState with an out-of-range state must throw"),
                Err(error) => error.to_string(),
            };
            assert!(error.contains("rl.guiSetState"), "{error}");
            assert!(error.contains("GUI_STATE"), "{error}");
        }
    }
}
