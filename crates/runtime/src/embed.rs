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
use crux::object::JsObject as CruxObject;
use crux::property::PropertyDescriptor;
use crux::string::JsString;
use crux::value::Value;

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

    /// The realm's global object.
    pub fn global(&self) -> Result<JsObject, JsError> {
        Ok(JsObject(self.agent.current_realm()?.global_object.clone()))
    }

    /// Define an own data property on the global object.
    pub fn set_global(&mut self, name: &str, value: JsValue) -> Result<(), JsError> {
        let global = self.agent.current_realm()?.global_object.clone();
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

    /// Call a function value with a `this` and arguments, then drain jobs.
    pub fn call(
        &mut self,
        function: &JsValue,
        this: &JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JsError> {
        let values: Vec<Value> = args.iter().map(JsValue::value).cloned().collect();
        let result = crate::function::call(
            &mut self.agent,
            function.value(),
            this.value().clone(),
            &values,
        )?;
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
        let global = self.agent.current_realm()?.global_object.clone();
        global.create_data_property_or_throw(
            &JsString::from_utf8("process"),
            Value::Object(process),
        )?;
        Ok(())
    }

    /// Install an `fs` global with a minimal Node-style subset —
    /// `readFileSync`, `readdirSync`, `statSync` — backed by the host
    /// filesystem. Not a full Node `fs`; enough for host tools written in
    /// Slag (the test262 fixture tally is one). Only compiled with the
    /// `fs` feature.
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

        let global = realm.global_object.clone();
        global.create_data_property_or_throw(&JsString::from_utf8("fs"), Value::Object(fs))?;
        Ok(())
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
        let global = realm.global_object.clone();
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
        let global = realm.global_object.clone();
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
                if let Some(Value::Number(id)) = args.first() {
                    timers_clear.borrow_mut().cancelled.insert(*id as u64, true);
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
                if let Some(Value::Number(id)) = args.first() {
                    timers.borrow_mut().cancelled.insert(*id as u64, true);
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
            .and_then(|value| match value {
                Value::Function(function) => function.object.handle(),
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
    match args.get(index) {
        Some(Value::String(s)) => Ok(s.to_string_lossy()),
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
    if let Value::Object(object) = value {
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

    pub fn is_undefined(&self) -> bool {
        matches!(self.0, Value::Undefined)
    }

    pub fn is_null(&self) -> bool {
        matches!(self.0, Value::Null)
    }

    pub fn is_boolean(&self) -> bool {
        matches!(self.0, Value::Boolean(_))
    }

    pub fn is_number(&self) -> bool {
        matches!(self.0, Value::Number(_))
    }

    pub fn is_string(&self) -> bool {
        matches!(self.0, Value::String(_))
    }

    pub fn is_object(&self) -> bool {
        matches!(self.0, Value::Object(_))
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
        match self.0 {
            Value::Boolean(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_number(&self) -> Option<f64> {
        match self.0 {
            Value::Number(n) => Some(n),
            _ => None,
        }
    }

    /// The string's lossy UTF-8 rendering when the value is a String.
    pub fn as_string(&self) -> Option<String> {
        match &self.0 {
            Value::String(s) => Some(s.to_string_lossy()),
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

    /// The object as a value.
    pub fn as_value(&self) -> JsValue {
        JsValue(Value::Object(self.0.clone()))
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
}
