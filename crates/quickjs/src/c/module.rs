//! C module support: source-text modules (compiled through `JS_Eval` with
//! `JS_EVAL_TYPE_MODULE`, then `JS_ResolveModule` / `JS_EvalFunction` /
//! `JS_GetModuleNamespace`) and C modules (`JS_NewCModule`, `JS_AddModuleExport`,
//! `JS_SetModuleExport`).
//!
//! A module is either a compiled source-text module (a `JSValue` with
//! `JS_TAG_MODULE`) or a C module the host declared. Both live in a
//! thread-local box registry rooted as a GC root source; a source module's
//! `JSValue` pointer and a C module's `JSModuleDef *` name the same box, so
//! `JS_VALUE_GET_PTR(module_value)` is the `JSModuleDef`, as in QuickJS.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, c_char};

use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::heap::{GcAny, Pin, RootSource, Trace};
use crux::string::JsString;
use crux::value::Value as CruxValue;
use runtime::api::{self, Local};

use crate::c::abi;
use crate::c::runtime::{JSContext, context_api};

/// The C `JSModuleDef` (quickjs.h): opaque; the state lives in the registry.
#[repr(C)]
pub struct JSModuleDef {
    _private: [u8; 0],
}

/// The C `JSModuleInitFunc` (quickjs.h): `(ctx, m) -> int`.
pub type JSModuleInitFunc = unsafe extern "C" fn(*mut JSContext, *mut JSModuleDef) -> i32;

enum ModuleState {
    Compiled(api::Module),
    CModule {
        name: JsString,
        init: Option<JSModuleInitFunc>,
        export_names: Vec<JsString>,
        export_values: Vec<(JsString, CruxValue)>,
    },
}

struct JsModule {
    state: ModuleState,
    context: api::Context,
    pin: Option<Pin>,
    refs: Cell<u32>,
}

thread_local! {
    static MODULES: RefCell<HashMap<usize, Box<JsModule>>> = RefCell::new(HashMap::new());
    static MODULES_REGISTERED: Cell<bool> = const { Cell::new(false) };
}

struct ModuleRoots;

impl RootSource for ModuleRoots {
    fn roots(&self, visit: &mut dyn FnMut(GcAny)) {
        MODULES.with(|modules| {
            for entry in modules.borrow().values() {
                if let ModuleState::CModule {
                    name,
                    export_names,
                    export_values,
                    ..
                } = &entry.state
                {
                    name.trace(visit);
                    for name in export_names {
                        name.trace(visit);
                    }
                    for (name, value) in export_values {
                        name.trace(visit);
                        value.trace(visit);
                    }
                }
            }
        });
    }
}

static MODULE_ROOTS: ModuleRoots = ModuleRoots;

fn register() {
    MODULES_REGISTERED.with(|registered| {
        if registered.replace(true) {
            return;
        }
        crux::heap::register_root_source(&MODULE_ROOTS);
    });
}

fn insert(state: ModuleState, context: api::Context, pin: Option<Pin>) -> *mut JSModuleDef {
    register();
    let boxed = Box::new(JsModule {
        state,
        context,
        pin,
        refs: Cell::new(1),
    });
    let ptr = &*boxed as *const JsModule as *mut JSModuleDef;
    MODULES.with(|modules| modules.borrow_mut().insert(ptr as usize, boxed));
    ptr
}

/// Add a reference to a live module box (for a `JSValue` with `JS_TAG_MODULE`).
pub(crate) fn dup_module_value(ptr: usize) {
    MODULES.with(|modules| {
        if let Some(entry) = modules.borrow().get(&ptr) {
            entry.refs.set(entry.refs.get() + 1);
        }
    });
}

/// Drop a reference; the last one removes and drops the box (and its pin).
pub(crate) fn free_module_value(ptr: usize) {
    MODULES.with(|modules| {
        let mut modules = modules.borrow_mut();
        let Some(entry) = modules.get(&ptr) else {
            return;
        };
        let refs = entry.refs.get();
        if refs > 1 {
            entry.refs.set(refs - 1);
            return;
        }
        modules.remove(&ptr);
    });
}

/// Build the module `JSValue` for a compiled module, rooting it with a pin.
pub(crate) fn module_value(module: api::Module, context: api::Context) -> abi::JSValue {
    let pin = module.pin();
    let ptr = insert(ModuleState::Compiled(module), context, Some(pin));
    abi::JSValue {
        u: ptr as usize as u64,
        tag: abi::JS_TAG_MODULE,
    }
}

/// The compiled module and its context a module `JSValue` names.
pub(crate) fn module_from_value(value: abi::JSValue) -> Option<(api::Module, api::Context)> {
    if value.tag != abi::JS_TAG_MODULE {
        return None;
    }
    MODULES.with(|modules| {
        modules
            .borrow()
            .get(&(value.u as usize))
            .and_then(|entry| match entry.state {
                ModuleState::Compiled(module) => Some((module, entry.context)),
                ModuleState::CModule { .. } => None,
            })
    })
}

/// The compiled module a `JSModuleDef` names, when it is already realized.
fn compiled_module(m: *mut JSModuleDef) -> Option<api::Module> {
    MODULES.with(|modules| {
        modules
            .borrow()
            .get(&(m as usize))
            .and_then(|entry| match entry.state {
                ModuleState::Compiled(module) => Some(module),
                ModuleState::CModule { .. } => None,
            })
    })
}

/// The compiled module a `JSModuleDef` names, realizing a C module on demand.
pub(crate) fn module_from_def(ctx: *mut JSContext, m: *mut JSModuleDef) -> Option<api::Module> {
    realize_c_module(ctx, m).ok()
}

pub(crate) fn set_exception(api_ctx: &api::Context, error: &JsError) -> abi::JSValue {
    let thrown = api_ctx
        .with_agent(|agent| runtime::builtins::error::to_throwable(agent, error))
        .unwrap_or(crux::value::Value::Undefined);
    unsafe { &*api_ctx.isolate() }.set_pending_exception(thrown);
    abi::exception()
}

fn cstr_jsstring(ptr: *const c_char) -> Option<JsString> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the header requires a NUL-terminated string.
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .ok()
        .map(JsString::from_utf8)
}

/// A C module's evaluation: its exports are already set on the environment, so
/// there is nothing to run.
fn c_module_steps(_context: api::Context, _module: api::Module) -> Result<Local, JsError> {
    Ok(Local::undefined())
}

/// Turn a C-module placeholder into a compiled synthetic module: run the init
/// callback (which adds exports and sets values into the box), then build the
/// module with the declared names and apply the recorded values.
fn realize_c_module(ctx: *mut JSContext, m: *mut JSModuleDef) -> Result<api::Module, JsError> {
    if let Some(module) = compiled_module(m) {
        return Ok(module);
    }

    let (context, init) = MODULES.with(|modules| {
        let modules = modules.borrow();
        let entry = modules
            .get(&(m as usize))
            .ok_or_else(|| JsError::new(ErrorKind::Error, "module is not live".into()))?;
        match &entry.state {
            ModuleState::CModule { init, .. } => Ok((entry.context, *init)),
            ModuleState::Compiled(_) => Err(JsError::new(
                ErrorKind::Error,
                "module is already compiled".into(),
            )),
        }
    })?;

    // The init callback declares and sets the module's exports in the box.
    if let Some(init) = init {
        // SAFETY: `init` is the host's callback, called with the live context
        // and this module.
        let result = unsafe { init(ctx, m) };
        if result != 0 {
            return Err(JsError::new(
                ErrorKind::Error,
                "module init callback failed".into(),
            ));
        }
    }

    let (name, names, values) = MODULES.with(|modules| {
        let modules = modules.borrow();
        let entry = modules
            .get(&(m as usize))
            .ok_or_else(|| JsError::new(ErrorKind::Error, "module is not live".into()))?;
        match &entry.state {
            ModuleState::CModule {
                name,
                export_names,
                export_values,
                ..
            } => Ok((name.clone(), export_names.clone(), export_values.clone())),
            ModuleState::Compiled(_) => Err(JsError::new(
                ErrorKind::Error,
                "module is already compiled".into(),
            )),
        }
    })?;

    // Root the name and each export name through the box registry while the
    // synthetic module is created: the built `Vec<Local>` lives on the Rust
    // heap, which the conservative stack scan cannot see, and the constructor
    // allocates.
    let name_box = abi::value_to_jsvalue(CruxValue::String(Handle::new(name.clone())));
    let name_local =
        Local::from_value(abi::jsvalue_to_value(name_box).unwrap_or(CruxValue::Undefined));
    let mut name_boxes = Vec::with_capacity(names.len());
    let mut name_locals = Vec::with_capacity(names.len());
    for name in &names {
        let boxed = abi::value_to_jsvalue(CruxValue::String(Handle::new(name.clone())));
        name_locals.push(Local::from_value(
            abi::jsvalue_to_value(boxed).unwrap_or(CruxValue::Undefined),
        ));
        name_boxes.push(boxed);
    }

    let module =
        api::Module::create_synthetic_module(&context, &name_local, &name_locals, c_module_steps)?;
    let pin = module.pin();
    abi::free_jsvalue(name_box);
    for boxed in name_boxes {
        abi::free_jsvalue(boxed);
    }

    module.instantiate(&context)?;
    for (export_name, value) in values {
        let boxed = abi::value_to_jsvalue(value);
        let name_box = abi::value_to_jsvalue(CruxValue::String(Handle::new(export_name)));
        let name_local =
            Local::from_value(abi::jsvalue_to_value(name_box).unwrap_or(CruxValue::Undefined));
        let value_local =
            Local::from_value(abi::jsvalue_to_value(boxed).unwrap_or(CruxValue::Undefined));
        module.set_synthetic_module_export(&name_local, &value_local)?;
        abi::free_jsvalue(name_box);
        abi::free_jsvalue(boxed);
    }

    MODULES.with(|modules| {
        let mut modules = modules.borrow_mut();
        if let Some(entry) = modules.get_mut(&(m as usize)) {
            entry.state = ModuleState::Compiled(module);
            entry.pin = Some(pin);
        }
    });

    Ok(module)
}

/// Compile `source` as a module and, unless compile-only, link and evaluate it.
pub(crate) fn eval_module(
    api_ctx: &api::Context,
    _ctx: *mut JSContext,
    filename: Option<&str>,
    source: &str,
    eval_flags: i32,
) -> abi::JSValue {
    const JS_EVAL_FLAG_COMPILE_ONLY: i32 = 32;
    let specifier = filename.unwrap_or("module");
    let module = match api::Module::compile_with_name(api_ctx, specifier, filename, source) {
        Ok(module) => module,
        Err(error) => return set_exception(api_ctx, &error),
    };
    // Name the module so a later `import` of the same specifier resolves.
    if let Err(error) = module.register(api_ctx, specifier) {
        return set_exception(api_ctx, &error);
    }
    if (eval_flags & JS_EVAL_FLAG_COMPILE_ONLY) != 0 {
        return module_value(module, *api_ctx);
    }
    if let Err(error) = module.instantiate(api_ctx) {
        return set_exception(api_ctx, &error);
    }
    match module.evaluate(api_ctx) {
        Ok(local) => abi::value_to_jsvalue(local.into_value()),
        Err(error) => set_exception(api_ctx, &error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_ResolveModule(_ctx: *mut JSContext, obj: abi::JSValue) -> i32 {
    ffi::guard(|| resolve_module_impl(obj))
}

fn resolve_module_impl(obj: abi::JSValue) -> i32 {
    let Some((module, context)) = module_from_value(obj) else {
        return -1;
    };
    match module.instantiate(&context) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_EvalFunction(
    _ctx: *mut JSContext,
    fun_obj: abi::JSValue,
) -> abi::JSValue {
    ffi::guard(|| eval_function_impl(fun_obj))
}

fn eval_function_impl(fun_obj: abi::JSValue) -> abi::JSValue {
    let Some((module, context)) = module_from_value(fun_obj) else {
        return abi::exception();
    };
    match module.evaluate(&context) {
        Ok(local) => abi::value_to_jsvalue(local.into_value()),
        Err(error) => set_exception(&context, &error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_GetModuleNamespace(
    ctx: *mut JSContext,
    m: *mut JSModuleDef,
) -> abi::JSValue {
    let module = match realize_c_module(ctx, m) {
        Ok(module) => module,
        Err(_) => return abi::exception(),
    };
    let context = MODULES.with(|modules| {
        modules
            .borrow()
            .get(&(m as usize))
            .map(|entry| entry.context)
    });
    let Some(context) = context else {
        return abi::exception();
    };
    match module.namespace(&context) {
        Ok(local) => abi::value_to_jsvalue(local.into_value()),
        Err(_) => abi::exception(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_GetModuleName(
    _ctx: *mut JSContext,
    m: *mut JSModuleDef,
) -> crate::c::atom::JSAtom {
    let name = MODULES.with(|modules| {
        modules
            .borrow()
            .get(&(m as usize))
            .and_then(|entry| match &entry.state {
                ModuleState::CModule { name, .. } => Some(name.clone()),
                ModuleState::Compiled(module) => module.name().map(|n| JsString::from_utf8(&n)),
            })
    });
    match name {
        Some(name) => crate::c::atom::intern(name),
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewCModule(
    ctx: *mut JSContext,
    name: *const c_char,
    init: Option<JSModuleInitFunc>,
) -> *mut JSModuleDef {
    let Some(api_ctx) = context_api(ctx) else {
        return std::ptr::null_mut();
    };
    let name = cstr_jsstring(name).unwrap_or_else(|| JsString::from_utf8(""));
    insert(
        ModuleState::CModule {
            name,
            init,
            export_names: Vec::new(),
            export_values: Vec::new(),
        },
        api_ctx,
        None,
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_AddModuleExport(
    _ctx: *mut JSContext,
    m: *mut JSModuleDef,
    name: *const c_char,
) -> i32 {
    let Some(name) = cstr_jsstring(name) else {
        return -1;
    };
    MODULES.with(|modules| {
        let mut modules = modules.borrow_mut();
        let Some(entry) = modules.get_mut(&(m as usize)) else {
            return -1;
        };
        match &mut entry.state {
            ModuleState::CModule { export_names, .. } => {
                export_names.push(name);
                0
            }
            ModuleState::Compiled(_) => -1,
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_SetModuleExport(
    _ctx: *mut JSContext,
    m: *mut JSModuleDef,
    export_name: *const c_char,
    val: abi::JSValue,
) -> i32 {
    let Some(name) = cstr_jsstring(export_name) else {
        return -1;
    };
    let Some(value) = abi::jsvalue_to_value(val) else {
        return -1;
    };
    if let Some(module) = compiled_module(m) {
        let value_box = abi::value_to_jsvalue(value);
        let name_box = abi::value_to_jsvalue(CruxValue::String(Handle::new(name)));
        let name_local =
            Local::from_value(abi::jsvalue_to_value(name_box).unwrap_or(CruxValue::Undefined));
        let value_local =
            Local::from_value(abi::jsvalue_to_value(value_box).unwrap_or(CruxValue::Undefined));
        let result = module.set_synthetic_module_export(&name_local, &value_local);
        abi::free_jsvalue(name_box);
        abi::free_jsvalue(value_box);
        return if result.is_ok() { 0 } else { -1 };
    }
    MODULES.with(|modules| {
        let mut modules = modules.borrow_mut();
        let Some(entry) = modules.get_mut(&(m as usize)) else {
            return -1;
        };
        match &mut entry.state {
            ModuleState::CModule { export_values, .. } => {
                export_values.push((name, value));
                0
            }
            ModuleState::Compiled(_) => -1,
        }
    })
}
