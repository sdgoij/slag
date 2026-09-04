//! Embedding API: host-facing [`Context`], value handles, and callbacks.
//!
//! A [`Context`] wraps an [`Agent`] with an initialized realm plus the
//! host-defined globals the spec leaves to the host: `console`, the timer
//! functions (`setTimeout`/`setInterval`/`clearTimeout`/`clearInterval`), a
//! `Math.random` override point, and (for the CLI) `process.argv` and a
//! minimal `fs`. The host
//! configures these through [`HostCallbacks`]; [`JsValue`] and [`JsObject`]
//! are the host-facing handle types for values and objects.
//!
//! ```
//! use runtime::embed::{Context, JsValue};
//!
//! let mut ctx = Context::new().unwrap();
//! let value = ctx.eval("1 + 2").unwrap();
//! assert_eq!(value.as_number(), Some(3.0));
//! ```

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::object::{JsObject as CruxObject, ObjectKind, typed_array_effective_length};
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::typed_array::SharedBuffer;
use crux::value::{Value, ValueKind};

use crate::agent::Agent;
use crate::context::to_string as js_to_string;
use crate::host::HostHooks;
use crate::realm::Realm;

/// A console or rejection callback: receives the rendered text.
pub type OutputFn = Box<dyn Fn(&str)>;

/// A `Math.random` replacement source.
pub type RandomFn = Box<dyn Fn() -> f64>;

/// Host-defined behavior the embedding API exposes. Each callback is
/// optional; `None` falls back to a sensible default (console output to
/// stdout/stderr, the built-in PRNG, no rejection reporting).
#[derive(Default)]
pub struct HostCallbacks {
    /// `console.log` output; defaults to stdout.
    pub console_log: Option<OutputFn>,
    /// `console.info` output; defaults to stdout.
    pub console_info: Option<OutputFn>,
    /// `console.warn` output; defaults to stderr.
    pub console_warn: Option<OutputFn>,
    /// `console.error` output; defaults to stderr.
    pub console_error: Option<OutputFn>,
    /// `console.debug` output; defaults to stdout.
    pub console_debug: Option<OutputFn>,
    /// Replacement source for `Math.random`; `None` keeps the built-in PRNG.
    pub random: Option<RandomFn>,
    /// Called with a description of the rejection when a promise is rejected
    /// without a handler (and again when its first handler is attached).
    pub promise_rejection: Option<OutputFn>,
}

/// Which console method a built-in closure dispatches to.
#[derive(Debug, Clone, Copy)]
enum ConsoleSlot {
    Log,
    Info,
    Warn,
    Error,
    Debug,
}

/// Timer bookkeeping shared between the timer globals and their jobs.
#[derive(Default)]
struct TimerState {
    next_id: u64,
    cancelled: HashMap<u64, bool>,
}

/// The embedding context: an initialized agent plus host globals.
pub struct Context {
    agent: Agent,
    callbacks: Rc<RefCell<HostCallbacks>>,
    timers: Rc<RefCell<TimerState>>,
}

impl Context {
    /// Create a fresh agent, initialize its realm, and install the
    /// host-defined globals (`console`, timers, the `Math.random` override).
    pub fn new() -> Result<Self, JsError> {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm()?;
        let callbacks = Rc::new(RefCell::new(HostCallbacks::default()));
        agent.host_hooks = Some(Box::new(RejectionHooks {
            callbacks: callbacks.clone(),
            inner: None,
        }));
        let mut context = Context {
            agent,
            callbacks,
            timers: Rc::new(RefCell::new(TimerState::default())),
        };
        context.install_console()?;
        context.install_timers()?;
        context.install_random_override()?;
        Ok(context)
    }

    /// Replace the host callbacks. The console closures read the callbacks
    /// at call time, so changes apply to subsequent calls.
    pub fn set_host_callbacks(&mut self, callbacks: HostCallbacks) {
        *self.callbacks.borrow_mut() = callbacks;
    }

    /// The current callbacks.
    pub fn host_callbacks(&self) -> std::cell::Ref<'_, HostCallbacks> {
        self.callbacks.borrow()
    }

    /// Install custom host hooks; promise-rejection tracking still routes to
    /// [`HostCallbacks::promise_rejection`] before delegating to `hooks`.
    pub fn set_host_hooks(&mut self, hooks: Box<dyn HostHooks>) {
        let rejection = RejectionHooks {
            callbacks: self.callbacks.clone(),
            inner: Some(hooks),
        };
        self.agent.host_hooks = Some(Box::new(rejection));
    }

    /// The underlying agent (advanced use; the spec state lives here).
    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    /// The underlying agent, mutably.
    pub fn agent_mut(&mut self) -> &mut Agent {
        &mut self.agent
    }

    /// Toggle `--gc-stress`: collect at every safe point (script boundary /
    /// job-queue drain) instead of only on heap growth (docs/gc-plan.md
    /// GC-1 slice 3; GC-2 hardens the root audit under it).
    pub fn set_gc_stress(&mut self, enabled: bool) {
        self.agent.set_gc_stress(enabled);
    }

    /// The realm's global object.
    pub fn global(&self) -> Result<JsObject, JsError> {
        Ok(JsObject(self.agent.current_realm()?.global_object))
    }

    /// Define an own data property on the global object.
    pub fn set_global(&mut self, name: &str, value: JsValue) -> Result<(), JsError> {
        let global = self.agent.current_realm()?.global_object;
        global.create_data_property_or_throw(&JsString::from_utf8(name), value.into_value())?;
        Ok(())
    }

    /// Evaluate a Script in the global scope (spec 16.1.4-16.1.6) and drain
    /// the job queues, returning the script's completion value.
    pub fn eval(&mut self, source: &str) -> Result<JsValue, JsError> {
        let value = self.agent.run_script(source)?;
        self.agent.run_jobs()?;
        Ok(JsValue(value))
    }

    /// Evaluate a Script WITHOUT draining the job queues. Pending microtasks
    /// and timers stay queued until the caller runs [`Context::run_jobs`]; a
    /// host binding that stringifies the completion before the drain avoids
    /// holding a live value across the jobs' GC points.
    pub fn eval_script(&mut self, source: &str) -> Result<JsValue, JsError> {
        Ok(JsValue(self.agent.run_script(source)?))
    }

    /// Like [`Context::eval`], parsing with the JSX extension enabled: JSX
    /// elements desugar to `rlx.h(...)` calls at parse time.
    pub fn eval_jsx(&mut self, source: &str) -> Result<JsValue, JsError> {
        let value = self.agent.run_script_jsx(source)?;
        self.agent.run_jobs()?;
        Ok(JsValue(value))
    }

    /// Call a function value with a `this` and arguments, then drain jobs.
    pub fn call(
        &mut self,
        function: &JsValue,
        this: &JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JsError> {
        let values: Vec<Value> = args.iter().map(JsValue::value).cloned().collect();
        let result =
            crate::function::call(&mut self.agent, function.value(), *this.value(), &values)?;
        self.agent.run_jobs()?;
        Ok(JsValue(result))
    }

    /// Construct an object from a constructor value, then drain jobs.
    pub fn construct(
        &mut self,
        constructor: &JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JsError> {
        let values: Vec<Value> = args.iter().map(JsValue::value).cloned().collect();
        let result = crate::function::construct(
            &mut self.agent,
            constructor.value(),
            &values,
            constructor.value(),
        )?;
        self.agent.run_jobs()?;
        Ok(JsValue(result))
    }

    /// Drain the job queues (promise, timeout, then generic jobs).
    pub fn run_jobs(&mut self) -> Result<(), JsError> {
        self.agent.run_jobs()
    }

    /// Milliseconds until the earliest pending timeout fires, or `None` when
    /// no timeout jobs are queued. Hosts use it to schedule the next drain.
    pub fn next_timeout_ms(&self) -> Option<f64> {
        self.agent.next_timeout_delay_ns().map(|ns| ns as f64 / 1e6)
    }

    // ── buffer / typed-array access (Phase 0.1: embedding API) ──────────────
    // Host-facing byte access so an embedder can exchange raw bytes with JS:
    // read bytes out of a TypedArray/ArrayBuffer, write bytes back in, and
    // create an ArrayBuffer from bytes. This is what JSBuffer (Node `Buffer`)
    // and `fs`/`fetch` byte paths will be built on.

    /// Copy the bytes of an ArrayBuffer or TypedArray value.
    ///
    /// - TypedArray: the view's bytes (`byteOffset .. byteOffset+byteLength`).
    /// - ArrayBuffer (or SharedArrayBuffer): all its bytes.
    /// - Anything else, or a detached buffer: `None`.
    pub fn as_bytes(&self, value: &JsValue) -> Option<Vec<u8>> {
        match value.0.kind() {
            ValueKind::Object(obj) => match &obj.kind {
                ObjectKind::IntegerIndexed(slots) => {
                    if slots.buffer.is_detached() {
                        return None;
                    }
                    let len = typed_array_effective_length(slots) * slots.element_type.size();
                    slots.buffer.read(slots.byte_offset, len).ok()
                }
                _ => {
                    // ArrayBuffer/SharedArrayBuffer: the storage is agent-side.
                    let state = self.agent.buffer_data.get(&obj.id())?.borrow();
                    if state.detached {
                        return None;
                    }
                    state.shared.read(0, state.byte_length).ok()
                }
            },
            _ => None,
        }
    }

    /// Write `bytes` into an ArrayBuffer or TypedArray value.
    ///
    /// Errors when the value is neither, is detached, or the bytes don't fit
    /// the view/buffer.
    pub fn set_bytes(&self, value: &JsValue, bytes: &[u8]) -> Result<(), JsError> {
        match value.0.kind() {
            ValueKind::Object(obj) => match &obj.kind {
                ObjectKind::IntegerIndexed(slots) => {
                    let len = typed_array_effective_length(slots) * slots.element_type.size();
                    if bytes.len() > len {
                        return Err(JsError::new(
                            ErrorKind::TypeError,
                            format!(
                                "set_bytes: {} bytes do not fit a {}-byte TypedArray",
                                bytes.len(),
                                len
                            ),
                        ));
                    }
                    slots.buffer.write(slots.byte_offset, bytes)
                }
                _ => {
                    let cell = self.agent.buffer_data.get(&obj.id()).ok_or_else(|| {
                        JsError::new(
                            ErrorKind::TypeError,
                            "set_bytes: not an ArrayBuffer or TypedArray".into(),
                        )
                    })?;
                    let state = cell.borrow_mut();
                    if state.detached {
                        return Err(JsError::new(
                            ErrorKind::TypeError,
                            "set_bytes: detached ArrayBuffer".into(),
                        ));
                    }
                    if bytes.len() > state.byte_length {
                        return Err(JsError::new(
                            ErrorKind::TypeError,
                            format!(
                                "set_bytes: {} bytes do not fit a {}-byte ArrayBuffer",
                                bytes.len(),
                                state.byte_length
                            ),
                        ));
                    }
                    state.shared.write(0, bytes)
                }
            },
            _ => Err(JsError::new(
                ErrorKind::TypeError,
                "set_bytes: expected an ArrayBuffer or TypedArray".into(),
            )),
        }
    }

    /// Create an ArrayBuffer from bytes (spec 25.1.2.2 with a supplied
    /// [[ArrayBufferData]]).
    pub fn array_buffer_from_bytes(&mut self, bytes: &[u8]) -> Result<JsValue, JsError> {
        let shared = SharedBuffer::new(bytes.len());
        shared.write(0, bytes)?;
        let value = crate::builtins::array_buffer::array_buffer_from_block(
            self.agent_mut(),
            shared,
            bytes.len(),
        )?;
        Ok(JsValue(value))
    }

    // ── coercions & type checks (Phase 1: embedding API) ────────────────────
    // The coercion half of what `bun_jsc`'s `JSC__JSValue__*` externs do:
    // ToNumber/ToString/ToObject need an agent (objects run valueOf/toString),
    // so they live on `Context`; the pure ToBoolean and BigInt check live on
    // `JsValue`.

    /// ToNumber coercion (spec 7.1.3): objects run @@toPrimitive/valueOf
    /// through the agent.
    pub fn to_number(&mut self, value: &JsValue) -> Result<f64, JsError> {
        let agent = self.agent_mut();
        let prim =
            crate::context::to_primitive(agent, &value.0, crux::convert::ToPrimitiveHint::Number)?;
        crux::convert::to_number(&prim)
    }

    /// ToString coercion (spec 7.1.12): objects run toString/valueOf through
    /// the agent.
    pub fn to_string(&mut self, value: &JsValue) -> Result<String, JsError> {
        let agent = self.agent_mut();
        Ok(crate::context::to_string(agent, &value.0)?.to_string_lossy())
    }

    /// ToObject (spec 7.1.13): box a primitive into its wrapper object.
    pub fn to_object(&mut self, value: &JsValue) -> Result<JsObject, JsError> {
        let agent = self.agent_mut();
        match crate::context::to_object(agent, &value.0)?.kind() {
            ValueKind::Object(obj) => Ok(JsObject(obj)),
            ValueKind::Function(f) => Ok(JsObject(f.object)),
            _ => unreachable!("ToObject always returns an object or function"),
        }
    }

    /// Whether `value` is a Date instance (has a [[DateValue]] slot).
    pub fn is_date(&self, value: &JsValue) -> bool {
        match value.0.kind() {
            ValueKind::Object(obj) => self.agent.date_data.contains_key(&obj.id()),
            _ => false,
        }
    }

    /// OrdinaryHasInstance (spec 7.3.19): `value instanceof constructor`.
    pub fn is_instance_of(
        &mut self,
        value: &JsValue,
        constructor: &JsValue,
    ) -> Result<bool, JsError> {
        let agent = self.agent_mut();
        let result = crate::expr::ordinary_has_instance(agent, &constructor.0, &value.0)?;
        Ok(matches!(result.kind(), ValueKind::Boolean(true)))
    }

    /// JSON.stringify (spec 25.5.2.2): serialize `value` to JSON text.
    /// Returns `None` when JSON.stringify yields `undefined` (undefined,
    /// functions, symbols, or objects that can't stringify to JSON).
    pub fn json_stringify(&mut self, value: &JsValue) -> Result<Option<String>, JsError> {
        let agent = self.agent_mut();
        let stringify = agent
            .current_realm()?
            .intrinsics
            .get("%JSON.stringify%")
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "%JSON.stringify% missing".into()))?;
        let result = crate::function::call(agent, &stringify, Value::Undefined, &[value.0])?;
        if matches!(result.kind(), ValueKind::Undefined) {
            return Ok(None);
        }
        Ok(Some(
            crate::context::to_string(agent, &result)?.to_string_lossy(),
        ))
    }

    /// IsIterable (spec 7.2.14): whether `value` has a callable @@iterator.
    pub fn is_iterable(&mut self, value: &JsValue) -> Result<bool, JsError> {
        let ValueKind::Object(obj) = value.0.kind() else {
            return Ok(false);
        };
        let key = crux::property::PropertyKey::Symbol(crux::symbol::well_known("iterator"));
        let method = obj.get_key(&key)?;
        Ok(!matches!(
            method.kind(),
            ValueKind::Undefined | ValueKind::Null
        ))
    }

    /// Symbol.for (spec 20.4.2.10): look up or create the global-registry
    /// symbol for `key`.
    pub fn symbol_for(&mut self, key: &str) -> Result<JsValue, JsError> {
        let mut registry = self.agent.global_symbol_registry.borrow_mut();
        let key = JsString::from_utf8(key);
        if let Some((_, symbol)) = registry.iter().find(|(k, _)| *k == key) {
            return Ok(JsValue(Value::Symbol(Handle::new(symbol.clone()))));
        }
        let symbol = crux::symbol::Symbol::new(Some(key.clone()));
        registry.push((key, symbol.clone()));
        Ok(JsValue(Value::Symbol(Handle::new(symbol))))
    }

    /// Array.prototype.push: append `value` to `array`, returning the new
    /// length. Errors when `array` is not array-like.
    pub fn array_push(&mut self, array: &JsValue, value: JsValue) -> Result<u64, JsError> {
        let agent = self.agent_mut();
        let result = crate::builtins::array::push(agent, &array.0, std::slice::from_ref(&value.0))?;
        match result.kind() {
            ValueKind::Number(n) => Ok(n as u64),
            _ => Ok(0),
        }
    }

    /// Parse a decimal string into a BigInt value.
    pub fn bigint_from_latin1(&mut self, digits: &str) -> Result<JsValue, JsError> {
        let big = crux::bigint::BigInt::parse_str(digits, 10).ok_or_else(|| {
            JsError::new(
                ErrorKind::SyntaxError,
                format!("bigint_from_latin1: invalid BigInt literal {digits:?}"),
            )
        })?;
        Ok(JsValue(Value::BigInt(Handle::new(big))))
    }

    /// Add two BigInt values (spec 13.9.3).
    pub fn bigint_sum(&mut self, a: &JsValue, b: &JsValue) -> Result<JsValue, JsError> {
        let ValueKind::BigInt(x) = a.0.kind() else {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "bigint_sum: expected a BigInt".into(),
            ));
        };
        let ValueKind::BigInt(y) = b.0.kind() else {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "bigint_sum: expected a BigInt".into(),
            ));
        };
        Ok(JsValue(Value::BigInt(Handle::new(crux::bigint::add(
            &x, &y,
        )))))
    }

    /// Compare two BigInt values: -1 (a < b), 0 (a == b), 1 (a > b).
    pub fn bigint_compare(&mut self, a: &JsValue, b: &JsValue) -> Result<i8, JsError> {
        let ValueKind::BigInt(x) = a.0.kind() else {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "bigint_compare: expected a BigInt".into(),
            ));
        };
        let ValueKind::BigInt(y) = b.0.kind() else {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "bigint_compare: expected a BigInt".into(),
            ));
        };
        Ok(if crux::bigint::less_than(&x, &y) {
            -1
        } else if crux::bigint::equal(&x, &y) {
            0
        } else {
            1
        })
    }

    /// Create a Date instance from a ms-since-epoch time value (the
    /// `dateInstanceFromNumber` extern).
    pub fn date_from_number(&mut self, ms: f64) -> Result<JsValue, JsError> {
        let agent = self.agent_mut();
        let proto = agent
            .current_realm()?
            .intrinsics
            .get("%Date.prototype%")
            .and_then(|value| value.as_object())
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "%Date.prototype% missing".into()))?;
        let object = CruxObject::ordinary_object_create(Some(proto));
        agent.date_data.insert(object.id(), ms);
        Ok(JsValue(Value::Object(object)))
    }

    /// The [[DateValue]] (ms since epoch) of a Date instance, if any.
    pub fn unix_timestamp(&self, value: &JsValue) -> Option<f64> {
        match value.0.kind() {
            ValueKind::Object(obj) => self.agent.date_data.get(&obj.id()).copied(),
            _ => None,
        }
    }

    /// Date.prototype.toISOString of a Date instance (RangeError for invalid
    /// dates).
    pub fn to_iso_string(&mut self, value: &JsValue) -> Result<String, JsError> {
        let ms = self
            .unix_timestamp(value)
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not a Date".into()))?;
        crate::builtins::date::to_iso_string(ms)
    }

    /// Install a `process` global with an `argv` array, Node-style:
    /// `[executable, script, ...args]`.
    pub fn install_process_argv(&mut self, argv: &[String]) -> Result<(), JsError> {
        let values: Vec<Value> = argv
            .iter()
            .map(|s| Value::String(Handle::new(JsString::from_utf8(s))))
            .collect();
        let array = crate::builtins::array::array_from_values(&self.agent, &values)?;
        let process = CruxObject::ordinary_object_create(None);
        process.create_data_property_or_throw(&JsString::from_utf8("argv"), array)?;
        let global = self.agent.current_realm()?.global_object;
        global.create_data_property_or_throw(
            &JsString::from_utf8("process"),
            Value::Object(process),
        )?;
        Ok(())
    }

    /// Install an `fs` global with a minimal Node-style subset —
    /// `readFileSync`, `writeFileSync`, `readdirSync`, `statSync` — backed
    /// by the host filesystem. Not a full Node `fs`; enough for host tools
    /// written in Slag (the regexp table generator and the test262 fixture
    /// tally are two). Only compiled with the `fs` feature.
    #[cfg(feature = "fs")]
    pub fn install_fs(&mut self) -> Result<(), JsError> {
        let realm = self.agent.current_realm()?;
        let object_proto = realm
            .intrinsics
            .get("%Object.prototype%")
            .and_then(|value| value.as_object());
        let fs = CruxObject::ordinary_object_create(object_proto);

        let read_file = Function::create_builtin(
            Some(JsString::from_utf8("readFileSync")),
            1,
            Box::new(|_, args| {
                let path = string_arg(args, 0, "readFileSync")?;
                let bytes = std::fs::read(&path).map_err(|e| {
                    JsError::new(ErrorKind::TypeError, format!("readFileSync: {path}: {e}"))
                })?;
                let text = String::from_utf8_lossy(&bytes);
                Ok(Value::String(Handle::new(JsString::from_utf8(&text))))
            }),
            None,
            None,
        )?;
        fs.create_data_property_or_throw(
            &JsString::from_utf8("readFileSync"),
            Value::Function(read_file),
        )?;

        let write_file = Function::create_builtin(
            Some(JsString::from_utf8("writeFileSync")),
            2,
            Box::new(|_, args| {
                let path = string_arg(args, 0, "writeFileSync")?;
                let text = string_arg(args, 1, "writeFileSync")?;
                std::fs::write(&path, text).map_err(|e| {
                    JsError::new(ErrorKind::TypeError, format!("writeFileSync: {path}: {e}"))
                })?;
                Ok(Value::Undefined)
            }),
            None,
            None,
        )?;
        fs.create_data_property_or_throw(
            &JsString::from_utf8("writeFileSync"),
            Value::Function(write_file),
        )?;

        let read_dir = Function::create_builtin(
            Some(JsString::from_utf8("readdirSync")),
            1,
            Box::new(|_, args| {
                let path = string_arg(args, 0, "readdirSync")?;
                let agent = current_agent_mut()?;
                let mut names = Vec::new();
                for entry in std::fs::read_dir(&path).map_err(|e| {
                    JsError::new(ErrorKind::TypeError, format!("readdirSync: {path}: {e}"))
                })? {
                    let entry = entry.map_err(|e| {
                        JsError::new(ErrorKind::TypeError, format!("readdirSync: {path}: {e}"))
                    })?;
                    names.push(entry.file_name().to_string_lossy().into_owned());
                }
                let values: Vec<Value> = names
                    .iter()
                    .map(|name| Value::String(Handle::new(JsString::from_utf8(name))))
                    .collect();
                crate::builtins::array::array_from_values(agent, &values)
            }),
            None,
            None,
        )?;
        fs.create_data_property_or_throw(
            &JsString::from_utf8("readdirSync"),
            Value::Function(read_dir),
        )?;

        let stat = Function::create_builtin(
            Some(JsString::from_utf8("statSync")),
            1,
            Box::new(|_, args| {
                let path = string_arg(args, 0, "statSync")?;
                let meta = std::fs::metadata(&path).map_err(|e| {
                    JsError::new(ErrorKind::TypeError, format!("statSync: {path}: {e}"))
                })?;
                let object = CruxObject::ordinary_object_create(None);
                object.create_data_property_or_throw(
                    &JsString::from_utf8("size"),
                    Value::Number(meta.len() as f64),
                )?;
                let is_directory = meta.is_dir();
                let is_file = meta.is_file();
                let dir_check = Function::create_builtin(
                    Some(JsString::from_utf8("isDirectory")),
                    0,
                    Box::new(move |_, _| Ok(Value::Boolean(is_directory))),
                    None,
                    None,
                )?;
                object.create_data_property_or_throw(
                    &JsString::from_utf8("isDirectory"),
                    Value::Function(dir_check),
                )?;
                let file_check = Function::create_builtin(
                    Some(JsString::from_utf8("isFile")),
                    0,
                    Box::new(move |_, _| Ok(Value::Boolean(is_file))),
                    None,
                    None,
                )?;
                object.create_data_property_or_throw(
                    &JsString::from_utf8("isFile"),
                    Value::Function(file_check),
                )?;
                Ok(Value::Object(object))
            }),
            None,
            None,
        )?;
        fs.create_data_property_or_throw(&JsString::from_utf8("statSync"), Value::Function(stat))?;

        let global = realm.global_object;
        global.create_data_property_or_throw(&JsString::from_utf8("fs"), Value::Object(fs))?;
        Ok(())
    }

    /// Install a `rlx` global: the declarative layer's pure core. It exposes
    /// `rlx.h` (virtual elements), `rlx.render` (element tree → draw ops
    /// with stable paths), `rlx.present` (the per-frame driver: render,
    /// draw, and dispatch control events), and `rlx.useState`/`rlx.useRef`
    /// (component state retained per tree path). `rlx.draw` is the stock
    /// backend mapping ops onto the `rl.gui*` surface. Pure JS with no
    /// raylib dependency, so it installs on any realm; `rlx.draw` only needs
    /// raylib at draw time.
    pub fn install_rlx(&mut self) -> Result<(), JsError> {
        crate::rlx::install(&mut self.agent)
    }

    /// Install a `rl` global exposing an immediate-mode raylib surface to
    /// scripts: `initWindow`/`beginDrawing`/`endDrawing`, the `draw*`
    /// primitives, input queries, and the color/key/mouse constants. A
    /// script drives the render loop itself —
    /// `while (!rl.windowShouldClose()) { rl.beginDrawing(); ...; }` —
    /// exactly like a raylib C example. Only compiled with the `raylib`
    /// feature (it compiles raylib's C library and needs a display); with
    /// the additional `raygui` feature the same install also exposes
    /// raygui's controls as `rl.gui*`. The
    /// window state is bound to the installing thread, so calls from worker
    /// agents throw instead of racing raylib's global state.
    #[cfg(feature = "raylib")]
    pub fn install_raylib(&mut self) -> Result<(), JsError> {
        crate::raylib::install(&mut self.agent)
    }

    /// Make `bytes` available to `rl.loadTexture`/`rl.loadSound` under
    /// `name` (used to embed assets so a raylib demo needs no files on
    /// disk). Both the name and the bytes must outlive this call (`&'static`,
    /// e.g. from `include_bytes!`).
    #[cfg(feature = "raylib")]
    pub fn register_raylib_asset(&mut self, name: &'static str, data: &'static [u8]) {
        crate::raylib::register_embedded_asset(name, data);
    }

    /// Install the `console` global backed by the host callbacks.
    fn install_console(&mut self) -> Result<(), JsError> {
        let realm = self.agent.current_realm()?;
        let object_proto = realm
            .intrinsics
            .get("%Object.prototype%")
            .and_then(|value| value.as_object());
        let console = CruxObject::ordinary_object_create(object_proto);
        for (name, slot) in [
            ("log", ConsoleSlot::Log),
            ("info", ConsoleSlot::Info),
            ("warn", ConsoleSlot::Warn),
            ("error", ConsoleSlot::Error),
            ("debug", ConsoleSlot::Debug),
        ] {
            let callbacks = self.callbacks.clone();
            let method = Function::create_builtin(
                Some(JsString::from_utf8(name)),
                0,
                Box::new(move |_, args| console_output(&callbacks, slot, args)),
                None,
                None,
            )?;
            console.create_data_property_or_throw(
                &JsString::from_utf8(name),
                Value::Function(method),
            )?;
        }
        let global = realm.global_object;
        global.create_data_property_or_throw(
            &JsString::from_utf8("console"),
            Value::Object(console),
        )?;
        Ok(())
    }

    /// Install `setTimeout`/`setInterval`/`clearTimeout`/`clearInterval` on
    /// the runtime's timeout job queue.
    fn install_timers(&mut self) -> Result<(), JsError> {
        let realm = self.agent.current_realm()?;
        let global = realm.global_object;
        let timers = self.timers.clone();

        let timers_set = timers.clone();
        let set_timeout = Function::create_builtin(
            Some(JsString::from_utf8("setTimeout")),
            1,
            Box::new(move |_, args| {
                let agent = current_agent_mut()?;
                let callback = args.first().cloned().unwrap_or(Value::Undefined);
                if !crux::value::is_callable(&callback) {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "setTimeout: the callback is not callable".into(),
                    ));
                }
                let delay = args.get(1).cloned().unwrap_or(Value::Undefined);
                let ms = crate::context::to_number(agent, &delay)
                    .unwrap_or(0.0)
                    .max(0.0) as u64;
                let id = schedule_timeout(agent, &timers_set, callback, ms, false)?;
                Ok(Value::Number(id as f64))
            }),
            None,
            None,
        )?;
        global.create_data_property_or_throw(
            &JsString::from_utf8("setTimeout"),
            Value::Function(set_timeout),
        )?;

        let timers_interval = timers.clone();
        let set_interval = Function::create_builtin(
            Some(JsString::from_utf8("setInterval")),
            1,
            Box::new(move |_, args| {
                let agent = current_agent_mut()?;
                let callback = args.first().cloned().unwrap_or(Value::Undefined);
                if !crux::value::is_callable(&callback) {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "setInterval: the callback is not callable".into(),
                    ));
                }
                let delay = args.get(1).cloned().unwrap_or(Value::Undefined);
                let ms = crate::context::to_number(agent, &delay)
                    .unwrap_or(0.0)
                    .max(0.0) as u64;
                let id = schedule_timeout(agent, &timers_interval, callback, ms, true)?;
                Ok(Value::Number(id as f64))
            }),
            None,
            None,
        )?;
        global.create_data_property_or_throw(
            &JsString::from_utf8("setInterval"),
            Value::Function(set_interval),
        )?;

        let timers_clear = timers.clone();
        let clear_timeout = Function::create_builtin(
            Some(JsString::from_utf8("clearTimeout")),
            1,
            Box::new(move |_, args| {
                if let Some(id) = args.first().and_then(|v| v.as_number()) {
                    timers_clear.borrow_mut().cancelled.insert(id as u64, true);
                }
                Ok(Value::Undefined)
            }),
            None,
            None,
        )?;
        global.create_data_property_or_throw(
            &JsString::from_utf8("clearTimeout"),
            Value::Function(clear_timeout),
        )?;

        let clear_interval = Function::create_builtin(
            Some(JsString::from_utf8("clearInterval")),
            1,
            Box::new(move |_, args| {
                if let Some(id) = args.first().and_then(|v| v.as_number()) {
                    timers.borrow_mut().cancelled.insert(id as u64, true);
                }
                Ok(Value::Undefined)
            }),
            None,
            None,
        )?;
        global.create_data_property_or_throw(
            &JsString::from_utf8("clearInterval"),
            Value::Function(clear_interval),
        )?;
        Ok(())
    }

    /// Re-define `Math.random` to consult [`HostCallbacks::random`].
    fn install_random_override(&mut self) -> Result<(), JsError> {
        let realm = self.agent.current_realm()?;
        let math = realm.global_object.get(&JsString::from_utf8("Math"))?;
        let Some(math) = math.as_object() else {
            return Ok(());
        };
        let callbacks = self.callbacks.clone();
        // CreateBuiltinFunction (spec 10.2.3): the [[Prototype]] is
        // %Function.prototype% — the override must keep the same shape as the
        // Math.random it replaces.
        let function_proto = realm
            .intrinsics
            .get("%Function.prototype%")
            .and_then(|value| match value.kind() {
                ValueKind::Function(function) => function.object.handle(),
                _ => None,
            });
        let random = Function::create_builtin(
            Some(JsString::from_utf8("random")),
            0,
            Box::new(move |_, _| {
                let value = {
                    let callbacks = callbacks.borrow();
                    match &callbacks.random {
                        Some(custom) => custom(),
                        None => crate::builtins::math::default_random(),
                    }
                };
                Ok(Value::Number(value))
            }),
            None,
            function_proto,
        )?;
        math.define_property_or_throw(
            &JsString::from_utf8("random"),
            &PropertyDescriptor {
                value: Some(Value::Function(random)),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
        Ok(())
    }
}

/// The agent recorded by the innermost `crux::function::with_agent` window,
/// or a clear error when a host-global builtin runs outside one.
fn current_agent_mut() -> Result<&'static mut Agent, JsError> {
    let agent = crux::function::current_agent();
    if agent.is_null() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "host global called outside an agent window".into(),
        ));
    }
    // SAFETY: `with_agent` guarantees the pointer is a live `&mut Agent` for
    // the duration of the enclosing call; builtin closures only run inside
    // those windows.
    Ok(unsafe { &mut *(agent as *mut Agent) })
}

/// `console.*`: stringify each argument with ToString (so objects run their
/// `toString`/`valueOf` through the agent) and dispatch to the callback.
/// The `index`-th argument as a UTF-8 string, or a TypeError naming the
/// host function that expected it.
#[cfg(feature = "fs")]
fn string_arg(args: &[Value], index: usize, name: &str) -> Result<String, JsError> {
    match args.get(index).map(|v| v.kind()) {
        Some(ValueKind::String(s)) => Ok(s.to_string_lossy()),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name}: expected a string path"),
        )),
    }
}

fn console_output(
    callbacks: &Rc<RefCell<HostCallbacks>>,
    slot: ConsoleSlot,
    args: &[Value],
) -> Result<Value, JsError> {
    let mut parts = Vec::with_capacity(args.len());
    let agent = crux::function::current_agent();
    for arg in args {
        let text = if agent.is_null() {
            arg.to_string()
        } else {
            // SAFETY: as in `current_agent_mut`.
            let agent = unsafe { &mut *(agent as *mut Agent) };
            js_to_string(agent, arg)?.to_string_lossy()
        };
        parts.push(text);
    }
    let line = parts.join(" ");
    let callbacks = callbacks.borrow();
    let slot_fn = match slot {
        ConsoleSlot::Log => callbacks.console_log.as_ref(),
        ConsoleSlot::Info => callbacks.console_info.as_ref(),
        ConsoleSlot::Warn => callbacks.console_warn.as_ref(),
        ConsoleSlot::Error => callbacks.console_error.as_ref(),
        ConsoleSlot::Debug => callbacks.console_debug.as_ref(),
    };
    match slot_fn {
        Some(callback) => callback(&line),
        None => match slot {
            ConsoleSlot::Log | ConsoleSlot::Info | ConsoleSlot::Debug => println!("{line}"),
            ConsoleSlot::Warn | ConsoleSlot::Error => eprintln!("{line}"),
        },
    }
    Ok(Value::Undefined)
}

/// Enqueue a timeout job that runs `callback`; `repeat` re-enqueues until
/// the timer is cleared.
fn schedule_timeout(
    agent: &mut Agent,
    timers: &Rc<RefCell<TimerState>>,
    callback: Value,
    ms: u64,
    repeat: bool,
) -> Result<u64, JsError> {
    timers.borrow_mut().next_id += 1;
    let id = timers.borrow().next_id;
    timers.borrow_mut().cancelled.insert(id, false);
    let realm = agent.current_realm()?;
    let job_timers = timers.clone();
    agent.enqueue_timeout_job(
        Some(realm),
        ms,
        timer_job(job_timers, id, callback, ms, repeat),
    );
    Ok(id)
}

/// One timer firing: skip when cleared, run the callback, and re-enqueue
/// intervals.
fn timer_job(
    timers: Rc<RefCell<TimerState>>,
    id: u64,
    callback: Value,
    ms: u64,
    repeat: bool,
) -> impl FnOnce(&mut Agent) -> Result<Value, JsError> {
    move |agent| {
        let cancelled = timers.borrow().cancelled.get(&id).copied().unwrap_or(false);
        if cancelled {
            return Ok(Value::Undefined);
        }
        let result = crate::function::call(agent, &callback, Value::Undefined, &[]);
        if repeat && result.is_ok() {
            let still_active = timers.borrow().cancelled.get(&id).copied().unwrap_or(false);
            if !still_active {
                let realm = agent.current_realm().ok();
                agent.enqueue_timeout_job(realm, ms, timer_job(timers, id, callback, ms, true));
            }
        }
        result
    }
}

/// Host hooks that route promise rejections to the callbacks while
/// delegating everything else to an optional inner implementation.
struct RejectionHooks {
    callbacks: Rc<RefCell<HostCallbacks>>,
    inner: Option<Box<dyn HostHooks>>,
}

impl fmt::Debug for RejectionHooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RejectionHooks").finish_non_exhaustive()
    }
}

impl HostHooks for RejectionHooks {
    fn ensure_can_compile_strings(
        &self,
        callee_realm: &Realm,
        param_strings: &[JsString],
        body_string: &JsString,
        direct: bool,
    ) -> Result<(), JsError> {
        match &self.inner {
            Some(inner) => {
                inner.ensure_can_compile_strings(callee_realm, param_strings, body_string, direct)
            }
            None => Ok(()),
        }
    }

    fn promise_rejection_tracker(
        &self,
        _promise: &Value,
        reason: Option<&Value>,
        _operation: bool,
    ) -> Result<(), JsError> {
        if let Some(callback) = &self.callbacks.borrow().promise_rejection {
            let text = reason.map(describe_value).unwrap_or_default();
            callback(&text);
        }
        match &self.inner {
            Some(inner) => inner.promise_rejection_tracker(_promise, reason, _operation),
            None => Ok(()),
        }
    }

    fn create_worker(
        &self,
        source: &str,
        shared: &[crux::typed_array::SharedBuffer],
    ) -> Result<(), JsError> {
        match &self.inner {
            Some(inner) => inner.create_worker(source, shared),
            None => Err(JsError::new(
                ErrorKind::TypeError,
                "HostCreateWorker is not implemented by this host".into(),
            )),
        }
    }
}

/// A best-effort description of a rejection reason: the `message` property
/// for error objects, otherwise the value's diagnostic rendering.
fn describe_value(value: &Value) -> String {
    if let ValueKind::Object(object) = value.kind() {
        let message = object
            .get_own_property(&JsString::from_utf8("message"))
            .ok()
            .flatten()
            .and_then(|property| property.value());
        if let Some(message) = message {
            return message.to_string();
        }
    }
    value.to_string()
}

/// A host-facing handle over an ECMAScript language value.
#[derive(Debug, Clone, PartialEq)]
pub struct JsValue(Value);

impl JsValue {
    pub fn undefined() -> Self {
        Self(Value::Undefined)
    }

    pub fn null() -> Self {
        Self(Value::Null)
    }

    pub fn boolean(value: bool) -> Self {
        Self(Value::Boolean(value))
    }

    pub fn number(value: f64) -> Self {
        Self(Value::Number(value))
    }

    pub fn string(value: impl Into<String>) -> Self {
        let text: String = value.into();
        Self(Value::String(Handle::new(JsString::from_utf8(&text))))
    }

    /// `typeof` of the value (spec 7.2.6).
    pub fn type_name(&self) -> &'static str {
        crux::value::type_of(&self.0)
    }

    /// ToBoolean coercion (spec 7.1.2) — pure, no agent needed.
    pub fn to_boolean(&self) -> bool {
        crux::convert::to_boolean(&self.0)
    }

    /// Whether the value is a BigInt (spec 7.2.6 `typeof` "bigint").
    pub fn is_big_int(&self) -> bool {
        matches!(self.0.kind(), ValueKind::BigInt(_))
    }

    /// SameValue (spec 7.2.11): `Object.is` semantics — NaN equals NaN,
    /// +0 and -0 differ.
    pub fn is_same_value(&self, other: &JsValue) -> bool {
        crux::ops::same_value(&self.0, &other.0)
    }

    pub fn is_undefined(&self) -> bool {
        self.0.is_undefined()
    }

    pub fn is_null(&self) -> bool {
        self.0.is_null()
    }

    pub fn is_boolean(&self) -> bool {
        self.0.is_boolean()
    }

    pub fn is_number(&self) -> bool {
        self.0.is_number()
    }

    pub fn is_string(&self) -> bool {
        self.0.is_string()
    }

    pub fn is_object(&self) -> bool {
        self.0.is_object()
    }

    /// Whether the value is callable (spec 7.2.3).
    pub fn is_callable(&self) -> bool {
        crux::value::is_callable(&self.0)
    }

    /// Whether the value is constructible (spec 7.2.4).
    pub fn is_constructor(&self) -> bool {
        crux::value::is_constructor(&self.0)
    }

    pub fn as_boolean(&self) -> Option<bool> {
        match self.0.kind() {
            ValueKind::Boolean(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_number(&self) -> Option<f64> {
        match self.0.kind() {
            ValueKind::Number(n) => Some(n),
            _ => None,
        }
    }

    /// The string's lossy UTF-8 rendering when the value is a String.
    pub fn as_string(&self) -> Option<String> {
        match self.0.kind() {
            ValueKind::String(s) => Some(s.to_string_lossy()),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<JsObject> {
        self.0.as_object().map(JsObject)
    }

    /// The underlying crux value.
    pub fn value(&self) -> &Value {
        &self.0
    }

    pub fn into_value(self) -> Value {
        self.0
    }
}

impl From<Value> for JsValue {
    fn from(value: Value) -> Self {
        Self(value)
    }
}

impl From<JsValue> for Value {
    fn from(value: JsValue) -> Self {
        value.0
    }
}

impl fmt::Display for JsValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A host-facing handle over an object value.
#[derive(Debug, Clone)]
pub struct JsObject(Handle<CruxObject>);

impl JsObject {
    /// [[Get]] the named property. Accessor properties run through the agent
    /// when inside an eval/call window.
    pub fn get(&self, key: &str) -> Result<JsValue, JsError> {
        self.0.get(&JsString::from_utf8(key)).map(JsValue)
    }

    /// Create an own data property (spec 7.3.5).
    pub fn set(&self, key: &str, value: JsValue) -> Result<(), JsError> {
        self.0
            .create_data_property_or_throw(&JsString::from_utf8(key), value.0)?;
        Ok(())
    }

    /// Define an own property with explicit descriptor attributes
    /// (spec 7.3.6).
    pub fn define(
        &self,
        key: &str,
        value: JsValue,
        writable: bool,
        enumerable: bool,
        configurable: bool,
    ) -> Result<(), JsError> {
        self.0.define_property_or_throw(
            &JsString::from_utf8(key),
            &PropertyDescriptor {
                value: Some(value.0),
                writable: Some(writable),
                get: None,
                set: None,
                enumerable: Some(enumerable),
                configurable: Some(configurable),
            },
        )?;
        Ok(())
    }

    /// The object's unique identity.
    pub fn id(&self) -> u64 {
        self.0.id()
    }

    /// The object's own enumerable string property keys — `Object.keys`
    /// semantics (spec 20.1.3.4): array indices ascending, then strings in
    /// insertion order (spec 10.1.12.1); symbols and non-enumerable
    /// properties excluded.
    pub fn property_keys(&self) -> Result<Vec<String>, JsError> {
        let mut keys = Vec::new();
        for key in self.0.own_property_keys()? {
            let PropertyKey::String(id) = key else {
                continue; // symbols: Object.keys excludes them
            };
            let name = crux::string::lookup(id);
            if self
                .0
                .get_own_property(&name)?
                .is_some_and(|p| p.enumerable)
            {
                keys.push(name.to_string_lossy());
            }
        }
        Ok(keys)
    }

    /// The object as a value.
    pub fn as_value(&self) -> JsValue {
        JsValue(Value::Object(self.0))
    }
}

impl From<Handle<CruxObject>> for JsObject {
    fn from(object: Handle<CruxObject>) -> Self {
        Self(object)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(source: &str) -> JsValue {
        Context::new().unwrap().eval(source).unwrap()
    }

    #[test]
    fn eval_returns_the_completion_value() {
        assert_eq!(eval("1 + 2").as_number(), Some(3.0));
        assert_eq!(eval("'a' + 'b'").as_string().as_deref(), Some("ab"));
        assert_eq!(eval("undefined").type_name(), "undefined");
    }

    #[test]
    fn console_routes_to_callbacks() {
        let mut context = Context::new().unwrap();
        let seen = Rc::new(RefCell::new(Vec::new()));
        let captured = seen.clone();
        context.set_host_callbacks(HostCallbacks {
            console_log: Some(Box::new(move |line| {
                captured.borrow_mut().push(line.to_string())
            })),
            ..HostCallbacks::default()
        });
        context.eval("console.log('hello', 42)").unwrap();
        assert_eq!(seen.borrow().as_slice(), &["hello 42".to_string()]);
    }

    #[test]
    fn random_can_be_overridden() {
        let mut context = Context::new().unwrap();
        context.set_host_callbacks(HostCallbacks {
            random: Some(Box::new(|| 0.5)),
            ..HostCallbacks::default()
        });
        let value = context.eval("Math.random()").unwrap();
        assert_eq!(value.as_number(), Some(0.5));
    }

    #[test]
    fn set_timeout_runs_after_jobs_drain() {
        let mut context = Context::new().unwrap();
        context
            .eval("globalThis.result = 0; setTimeout(() => { globalThis.result = 7; }, 0);")
            .unwrap();
        context.run_jobs().unwrap();
        let result = context.eval("globalThis.result").unwrap();
        assert_eq!(result.as_number(), Some(7.0));
    }

    #[test]
    fn clear_timeout_cancels_the_job() {
        let mut context = Context::new().unwrap();
        context
            .eval("globalThis.result = 0; const id = setTimeout(() => { globalThis.result = 7; }, 0); clearTimeout(id);")
            .unwrap();
        context.run_jobs().unwrap();
        let result = context.eval("globalThis.result").unwrap();
        assert_eq!(result.as_number(), Some(0.0));
    }

    // ---- Part B, B5.3: map-based store + transitions (VM level) ----

    #[test]
    fn member_reads_stay_consistent_across_shape_mutations() {
        let mut context = Context::new().unwrap();
        // The construct-then-read pattern warms the map cache; every mutation
        // below must leave reads serving the property vector's value.
        context
            .eval("function C(x) { this.x = x; } globalThis.o = new C(1);")
            .unwrap();
        assert_eq!(
            context.eval("globalThis.o.x").unwrap().as_number(),
            Some(1.0)
        );
        // An in-place value update through [[Set]] must mirror into the
        // inline field (the map read serves from there).
        context.eval("globalThis.o.x = 2;").unwrap();
        assert_eq!(
            context.eval("globalThis.o.x").unwrap().as_number(),
            Some(2.0)
        );
        // A defineProperty value update on a mapped key must mirror too.
        context
            .eval("Object.defineProperty(globalThis.o, 'x', { value: 3 });")
            .unwrap();
        assert_eq!(
            context.eval("globalThis.o.x").unwrap().as_number(),
            Some(3.0)
        );
        // Deleting the mapped key drops the object to dictionary mode: the
        // stale inline field must not win over the (now empty) property
        // vector.
        context.eval("delete globalThis.o.x;").unwrap();
        assert_eq!(
            context.eval("globalThis.o.x").unwrap().type_name(),
            "undefined"
        );
    }

    #[test]
    fn shared_shape_reads_per_object_fields() {
        let mut context = Context::new().unwrap();
        context
            .eval("function C(v) { this.x = v; } globalThis.a = new C(1); globalThis.b = new C(2);")
            .unwrap();
        // Both instances share the transitioned map; each reads its own
        // inline field through the same (map_id, name) cache entry.
        assert_eq!(
            context.eval("globalThis.a.x").unwrap().as_number(),
            Some(1.0)
        );
        assert_eq!(
            context.eval("globalThis.b.x").unwrap().as_number(),
            Some(2.0)
        );
        // A mutation on one instance must not leak into the other.
        context.eval("globalThis.a.x = 10;").unwrap();
        assert_eq!(
            context.eval("globalThis.a.x").unwrap().as_number(),
            Some(10.0)
        );
        assert_eq!(
            context.eval("globalThis.b.x").unwrap().as_number(),
            Some(2.0)
        );
    }

    // ---- Part B, B5.4: constructor boilerplate ----

    #[test]
    fn constructor_boilerplate_pre_sizes_the_final_shape() {
        let mut context = Context::new().unwrap();
        context
            .eval("function C(x) { this.x = x; this.y = x * 2; } globalThis.a = new C(1); globalThis.b = new C(2);")
            .unwrap();
        // The second construct starts on the cached final shape; both read
        // through their own pre-sized fields.
        assert_eq!(
            context.eval("globalThis.a.x").unwrap().as_number(),
            Some(1.0)
        );
        assert_eq!(
            context.eval("globalThis.a.y").unwrap().as_number(),
            Some(2.0)
        );
        assert_eq!(
            context.eval("globalThis.b.x").unwrap().as_number(),
            Some(2.0)
        );
        assert_eq!(
            context.eval("globalThis.b.y").unwrap().as_number(),
            Some(4.0)
        );
    }

    #[test]
    fn boilerplate_skipped_field_falls_through_to_the_prototype() {
        let mut context = Context::new().unwrap();
        context
            .eval("function C(x) { this.x = x; if (x > 1) { this.y = 2; } } C.prototype.y = 99; globalThis.o = new C(0);")
            .unwrap();
        // The pre-sized `y` field was never written by this construct: the
        // read must fall through to the prototype, not serve the unset field
        // as an own undefined.
        assert_eq!(
            context.eval("globalThis.o.x").unwrap().as_number(),
            Some(0.0)
        );
        assert_eq!(
            context.eval("globalThis.o.y").unwrap().as_number(),
            Some(99.0)
        );
        // A construct that does write y gets its own value.
        context.eval("globalThis.p = new C(5);").unwrap();
        assert_eq!(
            context.eval("globalThis.p.y").unwrap().as_number(),
            Some(2.0)
        );
    }

    #[test]
    fn boilerplate_rebuilds_after_prototype_redefine() {
        let mut context = Context::new().unwrap();
        context
            .eval("function C(x) { this.x = x; } globalThis.o1 = new C(1); C.prototype = { tag: 'new' }; globalThis.o2 = new C(2);")
            .unwrap();
        // The prototype redefine bumped the function's generation: the cache
        // rebuilt with the new prototype.
        assert_eq!(
            context.eval("globalThis.o1.x").unwrap().as_number(),
            Some(1.0)
        );
        assert_eq!(
            context.eval("globalThis.o2.x").unwrap().as_number(),
            Some(2.0)
        );
        assert_eq!(
            context
                .eval("globalThis.o2.tag")
                .unwrap()
                .as_string()
                .as_deref(),
            Some("new")
        );
        assert_eq!(
            context.eval("globalThis.o1.tag").unwrap().type_name(),
            "undefined"
        );
    }

    #[test]
    #[cfg(feature = "fs")]
    fn fs_reads_files_and_directories() {
        let mut context = Context::new().unwrap();
        context.install_fs().unwrap();
        let dir = std::env::temp_dir().join(format!("slag_fs_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hello.txt");
        std::fs::write(&file, "hello slag").unwrap();
        let js_path = |path: &std::path::Path| path.to_str().unwrap().replace('\\', "\\\\");
        let content = context
            .eval(&format!("fs.readFileSync('{}')", js_path(&file)))
            .unwrap();
        assert_eq!(content.as_string().as_deref(), Some("hello slag"));
        let is_file = context
            .eval(&format!("fs.statSync('{}').isFile()", js_path(&file)))
            .unwrap();
        assert_eq!(is_file.as_boolean(), Some(true));
        let listing = context
            .eval(&format!("fs.readdirSync('{}').join(',')", js_path(&dir)))
            .unwrap();
        assert_eq!(listing.as_string().as_deref(), Some("hello.txt"));
        let out = dir.join("out.txt");
        context
            .eval(&format!(
                "fs.writeFileSync('{}', 'written back')",
                js_path(&out)
            ))
            .unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "written back");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn call_and_construct_from_the_host() {
        let mut context = Context::new().unwrap();
        let add = context.eval("(a, b) => a + b").unwrap();
        let sum = context
            .call(
                &add,
                &JsValue::undefined(),
                &[JsValue::number(2.0), JsValue::number(3.0)],
            )
            .unwrap();
        assert_eq!(sum.as_number(), Some(5.0));
        let array_ctor = context.eval("Array").unwrap();
        let array = context
            .construct(&array_ctor, &[JsValue::number(3.0)])
            .unwrap();
        assert!(array.is_object());
    }

    #[test]
    fn process_argv_is_installed() {
        let mut context = Context::new().unwrap();
        context
            .install_process_argv(&["slag".into(), "file.js".into(), "arg".into()])
            .unwrap();
        let argv = context.eval("process.argv.join(',')").unwrap();
        assert_eq!(argv.as_string().as_deref(), Some("slag,file.js,arg"));
    }

    #[test]
    fn set_global_exposes_values_to_scripts() {
        let mut context = Context::new().unwrap();
        context
            .set_global("hostValue", JsValue::number(41.0))
            .unwrap();
        assert_eq!(
            eval_in(&mut context, "hostValue + 1").as_number(),
            Some(42.0)
        );
    }

    fn eval_in(context: &mut Context, source: &str) -> JsValue {
        context.eval(source).unwrap()
    }

    #[test]
    fn buffer_bytes_round_trip() {
        let mut context = Context::new().unwrap();

        // Host creates an ArrayBuffer from bytes; as_bytes reads it back.
        let ab = context.array_buffer_from_bytes(b"hello").unwrap();
        assert_eq!(context.as_bytes(&ab).as_deref(), Some(&b"hello"[..]));

        // Host writes into it; JS sees the change through a Uint8Array view.
        context.set_bytes(&ab, b"world").unwrap();
        context.set_global("buf", ab).unwrap();
        assert_eq!(
            eval_in(&mut context, "new Uint8Array(buf).join(',')").as_string(),
            Some("119,111,114,108,100".into()) // "world"
        );

        // JS-side view: as_bytes reads the view's bytes, set_bytes writes them
        // back into the shared storage.
        let view = eval_in(&mut context, "new Uint8Array(buf)");
        assert_eq!(context.as_bytes(&view).as_deref(), Some(&b"world"[..]));
        context.set_bytes(&view, b"abcde").unwrap();
        assert_eq!(
            eval_in(&mut context, "new Uint8Array(buf).join(',')").as_string(),
            Some("97,98,99,100,101".into()) // "abcde"
        );

        // Non-buffer values return None / error.
        let num = JsValue::number(1.0);
        assert_eq!(context.as_bytes(&num), None);
        assert!(context.set_bytes(&num, b"x").is_err());
    }

    #[test]
    fn coercions_and_type_checks() {
        let mut context = Context::new().unwrap();

        // ToBoolean (pure, no agent window needed).
        assert!(JsValue::number(1.0).to_boolean());
        assert!(!JsValue::number(0.0).to_boolean());
        assert!(!JsValue::undefined().to_boolean());
        assert!(JsValue::string("x").to_boolean());

        // ToNumber / ToString on primitives and objects (agent-aware).
        let s = JsValue::string("42");
        assert_eq!(context.to_number(&s).unwrap(), 42.0);
        let obj = eval_in(&mut context, "({ valueOf: () => 7 })");
        assert_eq!(context.to_number(&obj).unwrap(), 7.0);
        let obj2 = eval_in(&mut context, "({ toString: () => 'hi' })");
        assert_eq!(context.to_string(&obj2).unwrap(), "hi");

        // ToObject boxes primitives.
        let boxed = context.to_object(&JsValue::string("s")).unwrap();
        assert_eq!(boxed.get("length").unwrap().as_number(), Some(1.0));

        // Type checks.
        assert!(eval_in(&mut context, "10n").is_big_int());
        assert!(!eval_in(&mut context, "10").is_big_int());
        let date = eval_in(&mut context, "new Date(0)");
        assert!(context.is_date(&date));
        let plain = eval_in(&mut context, "({})");
        assert!(!context.is_date(&plain));

        // instanceof.
        let arr = eval_in(&mut context, "[]");
        let array_ctor = eval_in(&mut context, "Array");
        assert!(context.is_instance_of(&arr, &array_ctor).unwrap());
        assert!(
            !context
                .is_instance_of(&JsValue::number(1.0), &array_ctor)
                .unwrap()
        );
    }

    #[test]
    fn json_iterable_symbol_bigint_date_helpers() {
        let mut context = Context::new().unwrap();

        // SameValue (Object.is semantics).
        let nan = JsValue::number(f64::NAN);
        assert!(nan.is_same_value(&nan));
        assert!(!JsValue::number(0.0).is_same_value(&JsValue::number(-0.0)));
        assert!(JsValue::number(1.0).is_same_value(&JsValue::number(1.0)));

        // JSON.stringify (None for undefined/function/symbol).
        let obj = eval_in(&mut context, "({ a: 1, b: [true, null] })");
        assert_eq!(
            context.json_stringify(&obj).unwrap().as_deref(),
            Some("{\"a\":1,\"b\":[true,null]}")
        );
        assert_eq!(context.json_stringify(&JsValue::undefined()).unwrap(), None);

        // IsIterable.
        let iter_arr = eval_in(&mut context, "[1,2,3]");
        assert!(context.is_iterable(&iter_arr).unwrap());
        let iter_set = eval_in(&mut context, "new Set()");
        assert!(context.is_iterable(&iter_set).unwrap());
        let iter_obj = eval_in(&mut context, "({})");
        assert!(!context.is_iterable(&iter_obj).unwrap());
        assert!(!context.is_iterable(&JsValue::number(1.0)).unwrap());

        // Symbol.for — same registry entry on repeat.
        let s1 = context.symbol_for("bun.poc").unwrap();
        let s2 = context.symbol_for("bun.poc").unwrap();
        assert!(s1.is_same_value(&s2));
        context.set_global("s", s1).unwrap();
        assert!(eval_in(&mut context, "Symbol.for('bun.poc') === s").to_boolean());

        // Array.push.
        let arr = eval_in(&mut context, "[]");
        assert_eq!(context.array_push(&arr, JsValue::number(1.0)).unwrap(), 1);
        assert_eq!(context.array_push(&arr, JsValue::number(2.0)).unwrap(), 2);
        context.set_global("arr", arr).unwrap();
        assert_eq!(
            eval_in(&mut context, "arr.join(',')").as_string(),
            Some("1,2".into())
        );

        // BigInt helpers.
        let a = context.bigint_from_latin1("9007199254740993").unwrap();
        let b = context.bigint_from_latin1("1").unwrap();
        let sum = context.bigint_sum(&a, &b).unwrap();
        context.set_global("sum", sum).unwrap();
        assert_eq!(
            eval_in(&mut context, "String(sum)").as_string(),
            Some("9007199254740994".into())
        );
        assert_eq!(context.bigint_compare(&a, &b).unwrap(), 1);
        assert_eq!(context.bigint_compare(&a, &a).unwrap(), 0);

        // Date helpers.
        let date = context.date_from_number(1_700_000_000_000.0).unwrap();
        assert_eq!(context.unix_timestamp(&date), Some(1_700_000_000_000.0));
        assert_eq!(
            context.to_iso_string(&date).unwrap(),
            "2023-11-14T22:13:20.000Z"
        );
        let js_date = eval_in(&mut context, "new Date(0)");
        assert_eq!(context.unix_timestamp(&js_date), Some(0.0));
        assert_eq!(
            context.to_iso_string(&js_date).unwrap(),
            "1970-01-01T00:00:00.000Z"
        );
    }

    #[test]
    fn property_keys_enumeration() {
        let mut context = Context::new().unwrap();

        // Insertion order for string keys.
        let obj = eval_in(&mut context, "({ b: 1, a: 2, c: 3 })")
            .as_object()
            .unwrap();
        assert_eq!(obj.property_keys().unwrap(), vec!["b", "a", "c"]);

        // Array indices ascending, then extra strings.
        let arr = eval_in(&mut context, "['x', 'y']").as_object().unwrap();
        arr.set("extra", JsValue::number(1.0)).unwrap();
        assert_eq!(arr.property_keys().unwrap(), vec!["0", "1", "extra"]);

        // Non-enumerable properties excluded (Object.keys semantics).
        let with_hidden = eval_in(
            &mut context,
            "Object.defineProperty({ a: 1 }, 'hidden', { value: 2, enumerable: false })",
        )
        .as_object()
        .unwrap();
        assert_eq!(with_hidden.property_keys().unwrap(), vec!["a"]);

        // Symbol keys excluded.
        let with_sym = eval_in(&mut context, "({ a: 1, [Symbol.for('x')]: 2 })")
            .as_object()
            .unwrap();
        assert_eq!(with_sym.property_keys().unwrap(), vec!["a"]);
    }

    #[test]
    fn symbol_property_keys_survive_per_allocation_collection() {
        // With the stress collector, a collection runs after every
        // allocation: a symbol-keyed property whose key handle is not traced
        // (map transition, descriptor, property vector, for-in seen set)
        // would be swept out from under the live object. Each snippet below
        // exercises one such storage site.
        let mut context = Context::new().unwrap();
        context.set_gc_stress(true);
        // Map transition keyed by a user symbol, then reads through it.
        assert_eq!(
            context
                .eval("var s = Symbol('k'); var o = {}; for (var i = 0; i < 1000; i++) { o[s] = i; } o[s]")
                .unwrap()
                .as_number(),
            Some(999.0)
        );
        // Well-known symbol keys installed at realm bootstrap and looked up.
        assert_eq!(
            context
                .eval("var a = [1, 2]; var it = a[Symbol.iterator](); it.next().value")
                .unwrap()
                .as_number(),
            Some(1.0)
        );
        // for-in over an object with symbol keys (the seen-set holds key
        // handles transiently).
        assert_eq!(
            context
                .eval("var s = Symbol('x'); var o = { a: 1 }; o[s] = 2; var n = 0; for (var k in o) { n++; } n")
                .unwrap()
                .as_number(),
            Some(1.0)
        );
        // A symbol wrapper and its unwrapping.
        assert_eq!(
            context
                .eval("var s = Symbol('w'); var o = Object(s); o.valueOf() === s")
                .unwrap()
                .as_boolean(),
            Some(true)
        );
        // A leaf-inlined construct with a heap-valued argument (the construct
        // leaf branch reads its args from a stack buffer under
        // per-allocation collection).
        assert_eq!(
            context
                .eval("function C(x) { this.x = x; } var n = 0; for (var i = 0; i < 1000; i++) { var o = new C({ v: i }); n += o.x.v; } n")
                .unwrap()
                .as_number(),
            Some(499500.0)
        );
    }
}
