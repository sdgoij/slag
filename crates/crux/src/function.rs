//! Function objects (spec 10.2): callable and constructible values.
//!
//! Phase 5 gives functions the object machinery of ch. 10: an embedded
//! ordinary object holding `length`/`name`/`prototype` and any other own
//! properties, a [[Call]]/[[Construct]] dispatch over three kinds — native
//! built-ins backed by Rust closures, ECMAScript functions (body evaluation
//! joins with the Phase 6/7 evaluator), and bound functions (spec 10.4.1)
//! that delegate to a target with a fixed `this` and leading arguments.
//!
//! `Value::Function` stays separate from `Value::Object` (Phase 4 decision);
//! the embedded `JsObject` provides the object side of a function value.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{ErrorKind, JsError};
use crate::handle::Handle;
use crate::heap::Trace;
use crate::object::JsObject;
use crate::ops::same_value;
use crate::property::{PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::{Value, ValueKind, is_callable, is_constructor};

static NEXT_FUNCTION_ID: AtomicU64 = AtomicU64::new(1);

/// The runtime's executor for ECMAScript function bodies. The bodies live in
/// the runtime's agent, so `call`/`construct` on `FunctionKind::EcmaScript`
/// route through this hook when the runtime has installed it; `agent` is the
/// pointer recorded by [`with_agent`]. `new_target` is `Some` for `construct`.
type EcmaHook = fn(
    agent: *mut (),
    callee: &Value,
    this: Value,
    args: &[Value],
    new_target: Option<&Value>,
) -> Result<Value, JsError>;

thread_local! {
    static CURRENT_AGENT: RefCell<*mut ()> = const { RefCell::new(std::ptr::null_mut()) };
}

/// The ECMAScript executor is a plain function pointer: process-global.
static ECMA_HOOK: std::sync::OnceLock<EcmaHook> = std::sync::OnceLock::new();

/// Install the ECMAScript executor (the runtime calls this once at startup).
pub fn install_ecma_hook(hook: EcmaHook) {
    let _ = ECMA_HOOK.set(hook);
}

/// Run `body` with `agent` recorded as the current agent. Crux code that
/// invokes an ECMAScript function (proxy traps, object coercion) consults the
/// recorded pointer inside this window; `agent` must stay alive for the whole
/// of `body`, which is synchronous. A nested `run_inner` (a certified callee
/// run in the caller's Vm) re-enters with the same pointer, so the swap is
/// skipped when the agent is already current.
pub fn with_agent<T>(agent: *mut (), body: impl FnOnce() -> T) -> T {
    let already = CURRENT_AGENT.with(|slot| *slot.borrow() == agent);
    if already {
        return body();
    }
    let previous = CURRENT_AGENT.with(|slot| slot.replace(agent));
    let result = body();
    CURRENT_AGENT.with(|slot| slot.replace(previous));
    result
}

/// The agent pointer recorded by the innermost active [`with_agent`] window,
/// or null outside any window. Built-in closures that need the runtime's
/// agent (timers, console, host globals) cast this to `&mut Agent` inside
/// their closures; the pointer is only valid while the enclosing
/// `with_agent` call is on the stack.
pub fn current_agent() -> *mut () {
    CURRENT_AGENT.with(|slot| *slot.borrow())
}

/// Run an ECMAScript function through the runtime's executor, or report the
/// Phase-6 stub error when no executor is installed.
fn ecma_call(
    callee: &Value,
    this: Value,
    args: &[Value],
    new_target: Option<&Value>,
) -> Result<Value, JsError> {
    let agent = CURRENT_AGENT.with(|slot| *slot.borrow());
    let hook = ECMA_HOOK.get().copied();
    match hook {
        Some(hook) if !agent.is_null() => hook(agent, callee, this, args, new_target),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "ECMAScript function bodies require the runtime".into(),
        )),
    }
}

/// Route a built-in closure through the runtime's dispatcher when one is
/// active: nested proxy/bound forwarding (spec 10.5.12 step 6.a) can reach
/// an agent-dispatched built-in (whose closure is a placeholder) from inside
/// a `with_agent` window, and only the runtime knows the intrinsic identity.
/// Outside any agent window the closure runs directly.
fn call_with_hook(
    callee: &Value,
    this: Value,
    args: &[Value],
    native: &NativeFn,
) -> Result<Value, JsError> {
    let agent = CURRENT_AGENT.with(|slot| *slot.borrow());
    let hook = ECMA_HOOK.get().copied();
    match hook {
        Some(hook) if !agent.is_null() => hook(agent, callee, this, args, None),
        _ => native(&this, args),
    }
}

/// The construct half of [`call_with_hook`] (spec 10.5.13 step 7.a).
fn construct_with_hook(
    callee: &Value,
    args: &[Value],
    new_target: &Value,
    ctor: &NativeCtor,
) -> Result<Value, JsError> {
    let agent = CURRENT_AGENT.with(|slot| *slot.borrow());
    let hook = ECMA_HOOK.get().copied();
    match hook {
        Some(hook) if !agent.is_null() => {
            hook(agent, callee, Value::Undefined, args, Some(new_target))
        }
        _ => ctor(new_target, args),
    }
}

/// A native function body: `(this, args) -> result`. Host-provided closures
/// implement built-in methods; the Phase 7 evaluator bridges them.
pub type NativeFn = Box<dyn Fn(&Value, &[Value]) -> Result<Value, JsError>>;

/// A native constructor body: `(newTarget, args) -> object`.
pub type NativeCtor = Box<dyn Fn(&Value, &[Value]) -> Result<Value, JsError>>;

/// What happens when the function is called or constructed (spec 10.2.1).
pub enum FunctionKind {
    /// A built-in function with an optional [[Construct]].
    Builtin {
        call: Option<NativeFn>,
        construct: Option<NativeCtor>,
    },
    /// An ordinary ECMAScript function. The [[Environment]],
    /// [[FormalParameters]], [[ECMAScriptCode]], [[ThisMode]] and related
    /// slots join with the Phase 7 evaluator.
    EcmaScript,
    /// A bound function exotic (spec 10.4.1.1-10.4.1.3).
    Bound {
        target: Value,
        bound_this: Value,
        bound_args: Vec<Value>,
    },
}

impl fmt::Debug for FunctionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FunctionKind::Builtin { construct, .. } => {
                write!(f, "Builtin(constructor: {})", construct.is_some())
            }
            FunctionKind::EcmaScript => f.write_str("EcmaScript"),
            FunctionKind::Bound { .. } => f.write_str("Bound"),
        }
    }
}

/// A function object. Equality is identity (like `Symbol` and `JsObject`).
pub struct Function {
    id: u64,
    pub name: Option<JsString>,
    /// The object part: [[Prototype]], [[Extensible]], and own properties.
    /// Held as a handle so the object is also usable as a prototype link and
    /// `JsObject::handle()` recovers it.
    pub object: Handle<JsObject>,
    pub kind: FunctionKind,
    /// Strong back-reference to the owning handle so `this`-receiver
    /// operations (accessor invocation, own-property creation on `set`)
    /// target the real function value instead of a copy. A self-cycle under
    /// the GC model (the Rc model's weak ref existed only to break it).
    /// A lock-free `Cell`: written once by `link_self_handle`, read on every
    /// `self_value` (the hot receiver path), and the handle is `Copy`.
    self_handle: Cell<Option<Handle<Function>>>,
}

impl Trace for Function {
    fn trace(&self, visit: &mut dyn FnMut(crate::heap::GcAny)) {
        // The object part is a forward edge (a function keeps its object
        // alive); `name` may be a rope; `self_handle` is a self-cycle.
        self.object.trace(visit);
        if let Some(name) = &self.name {
            name.trace(visit);
        }
        // A bound function's target, bound `this`, and bound argument list
        // are Values — heap edges (GC-2; a swept target turned `extends`
        // heritage reads into "not a constructor" under `--gc-stress`).
        if let FunctionKind::Bound {
            target,
            bound_this,
            bound_args,
        } = &self.kind
        {
            target.trace(visit);
            bound_this.trace(visit);
            bound_args.trace(visit);
        }
    }
}

impl PartialEq for Function {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for Function {}

impl fmt::Debug for Function {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.name {
            Some(name) => write!(f, "Function({})", name.to_string_lossy()),
            None => f.write_str("Function"),
        }
    }
}

impl Function {
    /// The stable identity of this function (used by the runtime to look up
    /// the ECMAScript body, spec 10.2.1 slots).
    pub fn id(&self) -> u64 {
        self.id
    }

    /// A bare ECMAScript function value: identity and name only, used until
    /// the Phase 7 evaluator fills in the callable body.
    pub fn new(name: Option<JsString>) -> Handle<Function> {
        let function = Handle::new(Self {
            id: NEXT_FUNCTION_ID.fetch_add(1, Ordering::Relaxed),
            name,
            object: JsObject::ordinary_object_create(None),
            kind: FunctionKind::EcmaScript,
            self_handle: Cell::new(None),
        });
        function.self_handle.set(Some(function));
        function.object.function_self.set(Some(function));
        function
    }

    fn link_self_handle(function: &Handle<Function>) {
        function.self_handle.set(Some(*function));
    }

    /// The function as a language value, recovering the original handle.
    pub fn self_value(&self) -> Value {
        self.self_handle
            .get()
            .map(Value::Function)
            .unwrap_or(Value::Undefined)
    }

    /// CreateBuiltinFunction (spec 10.2.3) with `length`/`name` own data
    /// properties (writable false, enumerable false, configurable true).
    pub fn create_builtin(
        name: Option<JsString>,
        length: u64,
        call: NativeFn,
        construct: Option<NativeCtor>,
        prototype: Option<Handle<JsObject>>,
    ) -> Result<Handle<Function>, JsError> {
        let function = Handle::new(Self {
            id: NEXT_FUNCTION_ID.fetch_add(1, Ordering::Relaxed),
            name: name.clone(),
            object: JsObject::ordinary_object_create(prototype),
            kind: FunctionKind::Builtin {
                call: Some(call),
                construct,
            },
            self_handle: Cell::new(None),
        });
        Self::link_self_handle(&function);
        function.object.function_self.set(Some(function));
        function.define_property(
            &JsString::from_utf8("length"),
            &PropertyDescriptor {
                value: Some(Value::Number(length as f64)),
                writable: Some(false),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
        if let Some(name) = name {
            function.define_property(
                &JsString::from_utf8("name"),
                &PropertyDescriptor {
                    value: Some(Value::String(Handle::new(name))),
                    writable: Some(false),
                    get: None,
                    set: None,
                    enumerable: Some(false),
                    configurable: Some(true),
                },
            )?;
        }
        Ok(function)
    }

    /// BoundFunctionCreate (spec 10.4.1.3). The bound function's [[Prototype]]
    /// is `%Function.prototype%` per spec step 5; the caller (the runtime
    /// `bind` builtin) supplies it, `None` leaves the prototype null.
    pub fn bound_function_create(
        target: Value,
        bound_this: Value,
        bound_args: Vec<Value>,
        prototype: Option<Handle<JsObject>>,
    ) -> Result<Handle<Function>, JsError> {
        if !is_callable(&target) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Cannot bind a non-callable value".into(),
            ));
        }
        let function = Handle::new(Self {
            id: NEXT_FUNCTION_ID.fetch_add(1, Ordering::Relaxed),
            name: None,
            object: JsObject::ordinary_object_create(prototype),
            kind: FunctionKind::Bound {
                target,
                bound_this,
                bound_args,
            },
            self_handle: Cell::new(None),
        });
        Self::link_self_handle(&function);
        function.object.function_self.set(Some(function));
        Ok(function)
    }

    /// Whether this function has a [[Construct]] internal method.
    pub fn is_constructor(&self) -> bool {
        match &self.kind {
            FunctionKind::Builtin {
                construct: Some(_), ..
            } => true,
            FunctionKind::Builtin {
                construct: None, ..
            } => false,
            // Non-arrow ECMAScript functions are constructors; Phase 7
            // distinguishes arrows and methods.
            FunctionKind::EcmaScript => true,
            FunctionKind::Bound { target, .. } => is_constructor(target),
        }
    }

    /// The object side of a function value: [[GetPrototypeOf]].
    pub fn get_prototype_of(&self) -> Result<Option<Handle<JsObject>>, JsError> {
        self.object.get_prototype_of()
    }

    /// OrdinaryGet (spec 10.1.8.3) over the function's own object part,
    /// with the function value as receiver.
    pub fn get(&self, key: &JsString) -> Result<Value, JsError> {
        self.get_key(&PropertyKey::from_js_string(key))
    }

    pub fn get_key(&self, key: &PropertyKey) -> Result<Value, JsError> {
        self.object.get_with_receiver_key(key, self.self_value())
    }

    /// OrdinarySet (spec 10.1.9.3) with the function as receiver.
    pub fn set(&self, key: &JsString, value: Value, throw: bool) -> Result<bool, JsError> {
        self.set_key(&PropertyKey::from_js_string(key), value, throw)
    }

    pub fn set_key(&self, key: &PropertyKey, value: Value, throw: bool) -> Result<bool, JsError> {
        self.object
            .set_with_receiver_key(key, value, self.self_value(), throw)
    }

    /// spec 7.3.13 HasProperty over the function's own object part.
    pub fn has_property(&self, key: &JsString) -> Result<bool, JsError> {
        self.object.has_property(key)
    }

    pub fn has_own_property(&self, key: &JsString) -> Result<bool, JsError> {
        self.object.has_own_property(key)
    }

    pub fn get_own_property(
        &self,
        key: &JsString,
    ) -> Result<Option<crate::object::Property>, JsError> {
        self.object.get_own_property(key)
    }

    /// OrdinaryDefineOwnProperty (spec 10.1.6.3) on the function object.
    pub fn define_property(
        &self,
        key: &JsString,
        desc: &PropertyDescriptor,
    ) -> Result<bool, JsError> {
        self.object.define_property(key, desc)
    }

    pub fn define_property_key(
        &self,
        key: &PropertyKey,
        desc: &PropertyDescriptor,
    ) -> Result<bool, JsError> {
        self.object.define_property_key(key, desc)
    }

    /// spec 7.3.8 DeletePropertyOrThrow on the function object.
    pub fn delete(&self, key: &JsString) -> Result<bool, JsError> {
        self.object.delete(key)
    }

    pub fn own_property_keys(&self) -> Result<Vec<PropertyKey>, JsError> {
        self.object.own_property_keys()
    }
}

/// Call (spec 7.3.13): invoke a callable value with a `this` value and an
/// argument list. Proxies over callable targets dispatch through the apply
/// trap (spec 10.5.12).
pub fn call(callee: &Value, this: Value, args: &[Value]) -> Result<Value, JsError> {
    match callee.kind() {
        ValueKind::Function(function) => match &function.kind {
            FunctionKind::Builtin {
                call: Some(native), ..
            } => call_with_hook(callee, this, args, native),
            FunctionKind::Builtin { call: None, .. } => Err(JsError::new(
                ErrorKind::TypeError,
                "value is not a function".into(),
            )),
            FunctionKind::EcmaScript => ecma_call(callee, this, args, None),
            FunctionKind::Bound {
                target,
                bound_this,
                bound_args,
            } => {
                let mut all = bound_args.clone();
                all.extend_from_slice(args);
                call(target, *bound_this, &all)
            }
        },
        ValueKind::Object(obj) => match &obj.kind {
            crate::object::ObjectKind::IsHTMLDDA => Ok(Value::Null),
            crate::object::ObjectKind::Proxy(slots) => crate::proxy::apply(slots, this, args),
            crate::object::ObjectKind::Host(ops) => match ops.call(obj.as_ref(), &this, args) {
                Some(result) => result,
                None => Err(JsError::new(
                    ErrorKind::TypeError,
                    "value is not a function".into(),
                )),
            },
            _ => Err(JsError::new(
                ErrorKind::TypeError,
                "value is not a function".into(),
            )),
        },
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "value is not a function".into(),
        )),
    }
}

/// Construct (spec 7.3.14): invoke a constructor with an argument list and a
/// `newTarget` (defaulting to `callee` itself). Proxies over constructible
/// targets dispatch through the construct trap (spec 10.5.13).
pub fn construct(callee: &Value, args: &[Value], new_target: &Value) -> Result<Value, JsError> {
    match callee.kind() {
        ValueKind::Function(function) => match &function.kind {
            FunctionKind::Builtin {
                construct: Some(ctor),
                ..
            } => construct_with_hook(callee, args, new_target, ctor),
            FunctionKind::Builtin {
                construct: None, ..
            } => Err(not_constructible(callee)),
            FunctionKind::EcmaScript => ecma_call(callee, Value::Undefined, args, Some(new_target)),
            FunctionKind::Bound {
                target, bound_args, ..
            } => {
                let mut all = bound_args.clone();
                all.extend_from_slice(args);
                let target_value = *target;
                let new_target = if same_value(&Value::Function(function), new_target) {
                    &target_value
                } else {
                    new_target
                };
                construct(target, &all, new_target)
            }
        },
        ValueKind::Object(obj) => match &obj.kind {
            crate::object::ObjectKind::Proxy(slots) => {
                crate::proxy::construct(slots, args, new_target)
            }
            crate::object::ObjectKind::Host(ops) => {
                match ops.construct(obj.as_ref(), args, new_target) {
                    Some(result) => result,
                    None => Err(not_constructible(callee)),
                }
            }
            _ => Err(not_constructible(callee)),
        },
        _ => Err(not_constructible(callee)),
    }
}

fn not_constructible(callee: &Value) -> JsError {
    JsError::new(
        ErrorKind::TypeError,
        format!("{} is not a constructor", crate::value::type_of(callee)),
    )
}

/// %ThrowTypeError% (spec 10.2.2): the anonymous built-in that always throws,
/// with non-extensible [[Extensible]] and fully restricted `length`/`name`.
/// Created once per realm by the runtime; the intrinsic table owns it. The
/// `prototype` is %Function.prototype% (spec 8.2.2 steps 6-11).
pub fn throw_type_error(proto: Option<Handle<JsObject>>) -> Result<Handle<Function>, JsError> {
    let function = Function::create_builtin(
        Some(JsString::from_utf8("")),
        0,
        Box::new(|_, _| {
            Err(JsError::new(
                ErrorKind::TypeError,
                "Invalid operation performed on a restricted property".into(),
            ))
        }),
        None,
        proto,
    )?;
    // Restricted attributes: length and name are non-configurable.
    let restricted = PropertyDescriptor {
        value: None,
        writable: Some(false),
        get: None,
        set: None,
        enumerable: Some(false),
        configurable: Some(false),
    };
    for key in [JsString::from_utf8("length"), JsString::from_utf8("name")] {
        function.define_property(&key, &restricted)?;
    }
    // [[Extensible]] is false.
    function.object.prevent_extensions()?;
    Ok(function)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::PropertyKind;

    #[test]
    fn functions_are_identity_equal() {
        let a = Function::new(Some(JsString::from_utf8("f")));
        let b = Function::new(Some(JsString::from_utf8("f")));
        assert_eq!(a, a);
        assert_ne!(a, b);
    }

    #[test]
    fn carries_the_binding_name() {
        let f = Function::new(Some(JsString::from_utf8("fib")));
        assert_eq!(f.name.clone().unwrap().to_string_lossy(), "fib");
        assert!(Function::new(None).name.is_none());
    }

    #[test]
    fn builtin_functions_are_callable_and_have_length_and_name() {
        let f = Function::create_builtin(
            Some(JsString::from_utf8("add")),
            2,
            Box::new(|_, args| {
                let a = args.first().cloned().unwrap_or(Value::Undefined);
                let b = args.get(1).cloned().unwrap_or(Value::Undefined);
                match (a.kind(), b.kind()) {
                    (ValueKind::Number(x), ValueKind::Number(y)) => Ok(Value::Number(x + y)),
                    _ => Ok(Value::Undefined),
                }
            }),
            None,
            None,
        )
        .unwrap();
        assert!(is_callable(&Value::Function(f)));
        assert!(!is_constructor(&Value::Function(f)));
        let value = call(
            &Value::Function(f),
            Value::Undefined,
            &[Value::Number(2.0), Value::Number(3.0)],
        )
        .unwrap();
        assert_eq!(value, Value::Number(5.0));
        let length = f.get(&JsString::from_utf8("length")).unwrap();
        assert_eq!(length, Value::Number(2.0));
        let name = f.get(&JsString::from_utf8("name")).unwrap();
        assert_eq!(name, Value::String(Handle::new(JsString::from_utf8("add"))));
        let length_prop = f
            .get_own_property(&JsString::from_utf8("length"))
            .unwrap()
            .unwrap();
        match length_prop.kind {
            PropertyKind::Data { writable, .. } => assert!(!writable),
            _ => panic!("length must be a data property"),
        }
    }

    #[test]
    fn constructors_are_detected() {
        let ctor = Function::create_builtin(
            Some(JsString::from_utf8("C")),
            0,
            Box::new(|_, _| Ok(Value::Undefined)),
            Some(Box::new(|_, _| {
                Ok(Value::Object(JsObject::ordinary_object_create(None)))
            })),
            None,
        )
        .unwrap();
        let value = Value::Function(ctor);
        assert!(is_constructor(&value));
        let obj = construct(&value, &[], &value).unwrap();
        assert!(obj.is_object());
    }

    #[test]
    fn bound_functions_delegate_call_with_fixed_this_and_args() {
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorded = calls.clone();
        let target = Function::create_builtin(
            Some(JsString::from_utf8("f")),
            2,
            Box::new(move |this, args| {
                recorded.borrow_mut().push((*this, args.to_vec()));
                Ok(Value::Number(args.len() as f64))
            }),
            None,
            None,
        )
        .unwrap();
        let bound = Function::bound_function_create(
            Value::Function(target),
            Value::Number(7.0),
            vec![Value::Boolean(true)],
            None,
        )
        .unwrap();
        let result = call(
            &Value::Function(bound),
            Value::Undefined,
            &[Value::Number(1.0)],
        )
        .unwrap();
        assert_eq!(result, Value::Number(2.0));
        let (this, args) = &calls.borrow()[0];
        assert_eq!(this, &Value::Number(7.0));
        assert_eq!(args, &[Value::Boolean(true), Value::Number(1.0)]);
        // Bound functions are callable but inherit constructor-ness.
        assert!(is_callable(&Value::Function(bound)));
        assert!(!is_constructor(&Value::Function(bound)));
    }

    #[test]
    fn bound_function_requires_a_callable_target() {
        assert!(
            Function::bound_function_create(Value::Undefined, Value::Undefined, vec![], None)
                .is_err()
        );
    }

    #[test]
    fn set_creates_own_properties_on_the_real_function() {
        let f = Function::create_builtin(
            Some(JsString::from_utf8("f")),
            0,
            Box::new(|_, _| Ok(Value::Undefined)),
            None,
            None,
        )
        .unwrap();
        // The receiver for a receiver-less set is the function itself, so the
        // write lands on the real function's own properties.
        assert!(
            f.set(&JsString::from_utf8("x"), Value::Number(1.0), false)
                .unwrap()
        );
        assert_eq!(
            f.get(&JsString::from_utf8("x")).unwrap(),
            Value::Number(1.0)
        );
        assert_eq!(
            f.get_own_property(&JsString::from_utf8("length"))
                .unwrap()
                .unwrap()
                .value(),
            Some(Value::Number(0.0))
        );
    }

    #[test]
    fn throw_type_error_always_throws_and_is_restricted() {
        let thrower = throw_type_error(None).unwrap();
        assert!(call(&Value::Function(thrower), Value::Undefined, &[]).is_err());
        assert!(!thrower.object.is_extensible().unwrap());
        for key in ["length", "name"] {
            let prop = thrower
                .get_own_property(&JsString::from_utf8(key))
                .unwrap()
                .unwrap();
            assert!(!prop.configurable);
            assert_eq!(prop.writable(), Some(false));
        }
    }
}
