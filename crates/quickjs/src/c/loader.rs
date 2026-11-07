//! The module loaders (`JS_SetModuleLoaderFunc`/`JS_SetModuleLoaderFunc2`) and
//! the dynamic-import hooks (`JS_SetDynamicImportFunc`/`JS_FinishDynamicImport`).
//! A host callback resolves a static import's specifier (the v2 loader also
//! receives the import-attribute object). The `module_normalize` and
//! `module_check_attrs` arguments are accepted for ABI compatibility but not
//! dispatched in v1.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CString, c_char, c_void};
use std::rc::Rc;

use crux::error::{ErrorKind, JsError};
use crux::string::JsString;
#[cfg(feature = "ng")]
use crux::value::Value as CruxValue;
use runtime::api::{self, Local, Module};

use crate::c::abi;
use crate::c::module::JSModuleDef;
use crate::c::runtime::{JSContext, JSRuntime, context_api, current, set_host_hooks};

/// The C `JSModuleLoaderFunc` (quickjs.h): `(ctx, module_name, opaque) -> JSModuleDef *`.
pub type JSModuleLoaderFunc =
    unsafe extern "C" fn(*mut JSContext, *const c_char, *mut c_void) -> *mut JSModuleDef;

/// The C `JSModuleNormalizeFunc` (quickjs.h): `(ctx, base_name, module_name,
/// opaque) -> char *`.
pub type JSModuleNormalizeFunc =
    unsafe extern "C" fn(*mut JSContext, *const c_char, *const c_char, *mut c_void) -> *mut c_char;

/// The C `JSModuleLoaderFunc2` (quickjs.h): the attributes-aware loader.
pub type JSModuleLoaderFunc2 = unsafe extern "C" fn(
    *mut JSContext,
    *const c_char,
    *mut c_void,
    abi::JSValue,
) -> *mut JSModuleDef;

/// The C `JSModuleCheckSupportedImportAttributes` (quickjs.h): accepted, not
/// dispatched.
pub type JSModuleCheckSupportedImportAttributes =
    unsafe extern "C" fn(*mut JSContext, *mut c_void, abi::JSValue) -> i32;

/// The C `JSModuleNormalizeFunc2` (quickjs-ng only): attributes-aware
/// normalizer. Accepted, not dispatched.
#[cfg(feature = "ng")]
pub type JSModuleNormalizeFunc2 = unsafe extern "C" fn(
    *mut JSContext,
    *const c_char,
    *const c_char,
    abi::JSValue,
    *mut c_void,
) -> *mut c_char;

/// The C `JSDynamicImportFunc` (quickjs-ng only): called when a dynamic
/// `import()` needs the host's loader. `token` is handed back to
/// `JS_FinishDynamicImport`.
#[cfg(feature = "ng")]
pub type JSDynamicImportFunc = unsafe extern "C" fn(
    *mut JSContext,
    *const c_char,
    *const c_char,
    abi::JSValue,
    abi::JSValue,
    *mut c_void,
);

struct LoaderState {
    loader: RefCell<Option<JSModuleLoaderFunc>>,
    loader2: RefCell<Option<JSModuleLoaderFunc2>>,
    #[cfg(feature = "ng")]
    normalize2: RefCell<Option<JSModuleNormalizeFunc2>>,
    #[cfg(feature = "ng")]
    dynamic_import: RefCell<Option<JSDynamicImportFunc>>,
    opaque: Cell<*mut c_void>,
    #[cfg(feature = "ng")]
    dynamic_opaque: Cell<*mut c_void>,
}

thread_local! {
    static LOADERS: RefCell<HashMap<usize, Rc<LoaderState>>> = RefCell::new(HashMap::new());
}

/// A facade-reserved `JSValue` tag for a dynamic-import token. No header tag
/// uses this value, and the host treats the token opaquely.
#[cfg(feature = "ng")]
const JS_TAG_DYNAMIC_IMPORT: i64 = -1000;

/// A pending dynamic import: the boxed resolve/reject functions the token
/// resolves to when the host calls `JS_FinishDynamicImport`. Boxed so the
/// functions stay rooted across the host's asynchronous load.
#[cfg(feature = "ng")]
struct PendingImport {
    resolve: abi::JSValue,
    reject: abi::JSValue,
}

#[cfg(feature = "ng")]
thread_local! {
    static PENDING_IMPORTS: RefCell<HashMap<u64, PendingImport>> = RefCell::new(HashMap::new());
    static NEXT_TOKEN: Cell<u64> = const { Cell::new(1) };
}

struct FacadeHooks {
    state: Rc<LoaderState>,
}

impl std::fmt::Debug for FacadeHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FacadeHooks")
    }
}

impl runtime::host::HostHooks for FacadeHooks {
    fn resolve_imported_module(
        &self,
        specifier: &JsString,
        attributes: &[(JsString, JsString)],
    ) -> Option<Result<Module, JsError>> {
        self.state.resolve(specifier, attributes)
    }

    #[cfg(feature = "ng")]
    fn import_module_dynamically(
        &self,
        specifier: &JsString,
        referrer_name: Option<&JsString>,
        _phase: runtime::api::ModuleImportPhase,
        attributes: &[(JsString, JsString)],
    ) -> Option<Result<CruxValue, JsError>> {
        self.state
            .import_dynamic(specifier, referrer_name, attributes)
    }
}

impl LoaderState {
    fn resolve(
        &self,
        specifier: &JsString,
        attributes: &[(JsString, JsString)],
    ) -> Option<Result<Module, JsError>> {
        let ctx = current();
        if ctx.is_null() {
            return Some(Err(JsError::new(
                ErrorKind::TypeError,
                "no current context for the module loader".into(),
            )));
        }
        let name = specifier.to_string_lossy();
        let c_name = match CString::new(name.as_bytes()) {
            Ok(c_name) => c_name,
            Err(_) => {
                return Some(Err(JsError::new(
                    ErrorKind::Error,
                    "module name is not C-compatible".into(),
                )));
            }
        };
        let opaque = self.opaque.get();

        let loader2 = *self.loader2.borrow();
        let loader = *self.loader.borrow();
        let module_def = if let Some(loader2) = loader2 {
            let attributes = attributes_to_jsvalue(attributes);
            // SAFETY: `ctx` names a live context; `c_name` is NUL-terminated.
            unsafe { loader2(ctx, c_name.as_ptr(), opaque, attributes) }
        } else if let Some(loader) = loader {
            // SAFETY: `ctx` names a live context; `c_name` is NUL-terminated.
            unsafe { loader(ctx, c_name.as_ptr(), opaque) }
        } else {
            return None;
        };

        Some(resolve_module_def(ctx, module_def, &name))
    }

    #[cfg(feature = "ng")]
    fn import_dynamic(
        &self,
        specifier: &JsString,
        referrer_name: Option<&JsString>,
        attributes: &[(JsString, JsString)],
    ) -> Option<Result<CruxValue, JsError>> {
        let func = *self.dynamic_import.borrow();
        let func = func?;
        let ctx = current();
        if ctx.is_null() {
            return Some(Err(JsError::new(
                ErrorKind::TypeError,
                "no current context for dynamic import".into(),
            )));
        }
        let Some(api_ctx) = context_api(ctx) else {
            return Some(Err(JsError::new(
                ErrorKind::TypeError,
                "no current context for dynamic import".into(),
            )));
        };

        let global = api_ctx.global();
        let constructor = match api::Object::get(&api_ctx, &global, "Promise") {
            Ok(constructor) => constructor.into_value(),
            Err(error) => return Some(Err(error)),
        };
        let capability = api_ctx
            .with_agent(|agent| runtime::promise::new_promise_capability(agent, &constructor));
        let capability = match capability {
            Ok(capability) => capability,
            Err(error) => return Some(Err(error)),
        };

        // Box resolve/reject so they survive the host's asynchronous load.
        let resolve = abi::value_to_jsvalue(capability.resolve);
        let reject = abi::value_to_jsvalue(capability.reject);
        let token_id = NEXT_TOKEN.with(|next| {
            let id = next.get();
            next.set(id.wrapping_add(1));
            id
        });
        PENDING_IMPORTS.with(|pending| {
            pending
                .borrow_mut()
                .insert(token_id, PendingImport { resolve, reject });
        });

        let basename = referrer_name
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        let filename = specifier.to_string_lossy();
        let c_basename = CString::new(basename.as_bytes()).unwrap_or_default();
        let c_filename = CString::new(filename.as_bytes()).unwrap_or_default();
        let attributes = attributes_to_jsvalue(attributes);
        let token = abi::JSValue {
            u: token_id,
            tag: JS_TAG_DYNAMIC_IMPORT,
        };
        let opaque = self.dynamic_opaque.get();
        // SAFETY: `func` is the host's callback; the C strings are
        // NUL-terminated and live through the call.
        unsafe {
            func(
                ctx,
                c_basename.as_ptr(),
                c_filename.as_ptr(),
                token,
                attributes,
                opaque,
            );
        }
        Some(Ok(capability.promise))
    }
}

fn resolve_module_def(
    ctx: *mut JSContext,
    module_def: *mut JSModuleDef,
    name: &str,
) -> Result<Module, JsError> {
    if module_def.is_null() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            format!("Cannot find module {name}"),
        ));
    }
    crate::c::module::module_from_def(ctx, module_def).ok_or_else(|| {
        JsError::new(
            ErrorKind::Error,
            "module loader returned a module that failed to compile".into(),
        )
    })
}

fn ensure_loader(rt: *mut JSRuntime) -> Rc<LoaderState> {
    if let Some(state) = LOADERS.with(|loaders| loaders.borrow().get(&(rt as usize)).cloned()) {
        return state;
    }
    let state = Rc::new(LoaderState {
        loader: RefCell::new(None),
        loader2: RefCell::new(None),
        #[cfg(feature = "ng")]
        normalize2: RefCell::new(None),
        #[cfg(feature = "ng")]
        dynamic_import: RefCell::new(None),
        opaque: Cell::new(std::ptr::null_mut()),
        #[cfg(feature = "ng")]
        dynamic_opaque: Cell::new(std::ptr::null_mut()),
    });
    LOADERS.with(|loaders| loaders.borrow_mut().insert(rt as usize, state.clone()));
    set_host_hooks(
        rt,
        Box::new(FacadeHooks {
            state: state.clone(),
        }),
    );
    state
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_SetModuleLoaderFunc(
    rt: *mut JSRuntime,
    _module_normalize: Option<JSModuleNormalizeFunc>,
    module_loader: Option<JSModuleLoaderFunc>,
    opaque: *mut c_void,
) {
    let state = ensure_loader(rt);
    *state.loader.borrow_mut() = module_loader;
    state.opaque.set(opaque);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_SetModuleLoaderFunc2(
    rt: *mut JSRuntime,
    _module_normalize: Option<JSModuleNormalizeFunc>,
    module_loader: Option<JSModuleLoaderFunc2>,
    _module_check_attrs: Option<JSModuleCheckSupportedImportAttributes>,
    opaque: *mut c_void,
) {
    let state = ensure_loader(rt);
    *state.loader2.borrow_mut() = module_loader;
    state.opaque.set(opaque);
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_SetModuleNormalizeFunc2(
    rt: *mut JSRuntime,
    module_normalize: Option<JSModuleNormalizeFunc2>,
) {
    let state = ensure_loader(rt);
    *state.normalize2.borrow_mut() = module_normalize;
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_SetDynamicImportFunc(
    rt: *mut JSRuntime,
    func: Option<JSDynamicImportFunc>,
    opaque: *mut c_void,
) {
    let state = ensure_loader(rt);
    *state.dynamic_import.borrow_mut() = func;
    state.dynamic_opaque.set(opaque);
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_FinishDynamicImport(
    ctx: *mut JSContext,
    token: abi::JSValue,
    promise: abi::JSValue,
    exception: abi::JSValue,
) -> i32 {
    if token.tag != JS_TAG_DYNAMIC_IMPORT {
        return -1;
    }
    let token_id = token.u;
    let Some(pending) = PENDING_IMPORTS.with(|pending| pending.borrow_mut().remove(&token_id))
    else {
        return -1;
    };
    let Some(api_ctx) = context_api(ctx) else {
        return -1;
    };

    let resolve = abi::jsvalue_to_value(pending.resolve);
    let reject = abi::jsvalue_to_value(pending.reject);
    abi::free_jsvalue(pending.resolve);
    abi::free_jsvalue(pending.reject);

    let is_exception = promise.tag == abi::JS_TAG_EXCEPTION;
    let result = api_ctx.with_agent(|agent| {
        if is_exception {
            let reject = reject.ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "reject function missing".into())
            })?;
            let exception = abi::jsvalue_to_value(exception).unwrap_or(CruxValue::Undefined);
            runtime::function::call(agent, &reject, CruxValue::Undefined, &[exception])
        } else {
            let resolve = resolve.ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "resolve function missing".into())
            })?;
            let resolved = dynamic_import_result(promise)?;
            runtime::function::call(agent, &resolve, CruxValue::Undefined, &[resolved])
        }
    });
    i32::from(result.is_err())
}

#[cfg(feature = "ng")]
fn dynamic_import_result(promise: abi::JSValue) -> Result<CruxValue, JsError> {
    if promise.tag == abi::JS_TAG_MODULE {
        let (module, context) = crate::c::module::module_from_value(promise).ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "unknown module in dynamic import".into(),
            )
        })?;
        module.namespace(&context).map(|local| local.into_value())
    } else {
        abi::jsvalue_to_value(promise).ok_or_else(|| {
            JsError::new(ErrorKind::TypeError, "invalid dynamic import result".into())
        })
    }
}

/// Build the import-attribute object (`{ key: value, ... }`) the v2 loader's
/// callback takes, from the engine's validated key/value pairs.
fn attributes_to_jsvalue(attributes: &[(JsString, JsString)]) -> abi::JSValue {
    let Some(api_ctx) = context_api(current()) else {
        return abi::undefined();
    };
    let Ok(object) = api::Object::new(&api_ctx) else {
        return abi::undefined();
    };
    for (key, value) in attributes {
        let key = key.to_string_lossy();
        let value = Local::string(value.to_string_lossy());
        if api::Object::set(&api_ctx, &object, &key, &value, true).is_err() {
            return abi::undefined();
        }
    }
    abi::value_to_jsvalue(object.into_value())
}
