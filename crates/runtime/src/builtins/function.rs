//! The Function constructor and %Function.prototype% (spec 20.2).

use crux::convert::{to_integer_or_infinity, to_length, to_number};
use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, ValueKind, is_callable};

use crate::agent::Agent;
use crate::context::{as_object, get_property_key};
use crate::realm::Realm;

/// Intrinsic registry keys. The methods and the constructor are
/// agent-dependent (their crux closures cannot reach the agent), so the
/// runtime call/construct dispatchers recognize them by these identities.
const FUNCTION: &str = "%Function%";
const FUNCTION_PROTO: &str = "%Function.prototype%";
const APPLY: &str = "%Function.prototype.apply%";
const CALL: &str = "%Function.prototype.call%";
const BIND: &str = "%Function.prototype.bind%";
const TO_STRING: &str = "%Function.prototype.toString%";
const HAS_INSTANCE: &str = "%Function.prototype.@@hasInstance%";

/// Install the Function intrinsics and the global `Function` binding
/// (spec 20.2.1-20.2.3), during SetDefaultGlobalBindings.
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    // %Function.prototype% (20.2.3): a built-in function object with no
    // [[Construct]] and no `prototype` property. Its [[Prototype]] is
    // %Object.prototype% once the Phase 8 object intrinsics exist; null now.
    let function_proto = Function::create_builtin(
        Some(JsString::from_utf8("")),
        0,
        Box::new(|_, _| Ok(Value::Undefined)),
        None,
        None,
    )?;
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    function_proto.object.set_prototype_of(object_proto)?;
    let function_proto_value = Value::Function(function_proto);

    // %Function% (20.2.1): call and construct both run CreateDynamicFunction.
    let function_ctor = Function::create_builtin(
        Some(JsString::from_utf8("Function")),
        1,
        Box::new(placeholder("Function")),
        Some(Box::new(placeholder("Function"))),
        None,
    )?;
    let function_ctor_value = Value::Function(function_ctor);

    realm
        .intrinsics
        .define(FUNCTION_PROTO, function_proto_value);
    realm.intrinsics.define(FUNCTION, function_ctor_value);

    // 20.2.2 Function.prototype: non-writable and non-configurable.
    function_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(function_proto_value),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    // 20.2.3.1 constructor back-reference.
    function_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(function_ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    // The constructor's own [[Prototype]] is %Function.prototype%.
    let proto_handle = function_proto.object.handle().ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "%Function.prototype% has no object handle".into(),
        )
    })?;
    function_ctor.object.set_prototype_of(Some(proto_handle))?;

    install_methods(realm, &function_proto)?;

    // The global `Function` property (20.2.1).
    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("Function"),
        &PropertyDescriptor {
            value: Some(function_ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// The `%Function.prototype%` methods (20.2.3.2-20.2.3.7). All bodies are
/// placeholders; `runtime::function::call` dispatches by intrinsic identity.
fn install_methods(
    realm: &Handle<Realm>,
    function_proto: &Handle<Function>,
) -> Result<(), JsError> {
    let proto = function_proto.object.handle().ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "%Function.prototype% has no object handle".into(),
        )
    })?;
    let methods = [
        (
            APPLY,
            "apply",
            2,
            PropertyDescriptor {
                value: None,
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        ),
        (
            CALL,
            "call",
            1,
            PropertyDescriptor {
                value: None,
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        ),
        (
            BIND,
            "bind",
            1,
            PropertyDescriptor {
                value: None,
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        ),
        (
            TO_STRING,
            "toString",
            0,
            PropertyDescriptor {
                value: None,
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        ),
    ];
    for (intrinsic, name, length, mut desc) in methods {
        let method = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            Box::new(placeholder(name)),
            None,
            Some(proto),
        )?;
        realm.intrinsics.define(intrinsic, Value::Function(method));
        desc.value = Some(Value::Function(method));
        function_proto.define_property(&JsString::from_utf8(name), &desc)?;
    }

    // 20.2.3.6 Function.prototype[@@hasInstance]: non-writable and
    // non-configurable so `instanceof` stays tamper-proof.
    let has_instance = Function::create_builtin(
        Some(JsString::from_utf8("[Symbol.hasInstance]")),
        1,
        Box::new(placeholder("Function.prototype[@@hasInstance]")),
        None,
        Some(proto),
    )?;
    realm
        .intrinsics
        .define(HAS_INSTANCE, Value::Function(has_instance));
    function_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("hasInstance")),
        &PropertyDescriptor {
            value: Some(Value::Function(has_instance)),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;

    // 20.2.3.1 Function.prototype.caller/arguments: own accessor properties
    // whose get and set are %ThrowTypeError% (spec 10.2.2), so reads/writes
    // on functions without their own restricted properties (strict, bound,
    // async, generator) throw a TypeError. Sloppy ordinary functions shadow
    // them with their own null-valued data properties. The same intrinsic is
    // the get/set of unmapped arguments objects' `callee`, so all six slots
    // are one object (ThrowTypeError/unique-per-realm-*).
    let thrower = crux::function::throw_type_error(Some(proto))?;
    let thrower_value = Value::Function(thrower);
    realm
        .intrinsics
        .define("%ThrowTypeError%", Value::Function(thrower));
    for name in ["caller", "arguments"] {
        function_proto.define_property(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: None,
                writable: None,
                get: Some(thrower_value),
                set: Some(thrower_value),
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
    }
    Ok(())
}

/// A placeholder body for the agent-dispatched Function built-ins; the
/// runtime dispatcher intercepts calls before the closure can run.
fn placeholder(name: &str) -> crux::function::NativeFn {
    let name = name.to_string();
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be dispatched by the runtime"),
        ))
    })
}

/// Route a call to the Function built-ins by intrinsic identity.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(FUNCTION).as_ref() == Some(callee) {
        let (params, body) = split_dynamic_args(args);
        return Some(create_dynamic_function(
            agent,
            callee,
            &Value::Undefined,
            params,
            body,
        ));
    }
    if intrinsics.get(APPLY).as_ref() == Some(callee) {
        return Some(apply(agent, this, args));
    }
    if intrinsics.get(CALL).as_ref() == Some(callee) {
        return Some(call_method(agent, this, args));
    }
    if intrinsics.get(BIND).as_ref() == Some(callee) {
        return Some(bind(agent, this, args));
    }
    if intrinsics.get(TO_STRING).as_ref() == Some(callee) {
        return Some(function_to_string(agent, this));
    }
    if intrinsics.get(HAS_INSTANCE).as_ref() == Some(callee) {
        let value = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(crate::expr::ordinary_has_instance(agent, this, &value));
    }
    None
}

/// Route `new` on the Function constructor to CreateDynamicFunction.
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    if realm.intrinsics.get(FUNCTION).as_ref() == Some(callee) {
        let (params, body) = split_dynamic_args(args);
        return Some(create_dynamic_function(
            agent, callee, new_target, params, body,
        ));
    }
    None
}

/// The last argument is the body, the rest are parameters.
fn split_dynamic_args(args: &[Value]) -> (&[Value], Option<&Value>) {
    match args.split_last() {
        Some((body, params)) => (params, Some(body)),
        None => (&[], None),
    }
}

/// CreateDynamicFunction (spec 20.2.1.1), kind ~normal~: assemble the source
/// `function anonymous(params\n) {\nbody\n}`, parse it, and instantiate an
/// ordinary function with the GetPrototypeFromConstructor prototype.
fn create_dynamic_function(
    agent: &mut Agent,
    ctor: &Value,
    new_target: &Value,
    param_args: &[Value],
    body_arg: Option<&Value>,
) -> Result<Value, JsError> {
    let new_target = if matches!(new_target.kind(), ValueKind::Undefined) {
        *ctor
    } else {
        *new_target
    };
    let mut param_strings = Vec::new();
    for arg in param_args {
        param_strings.push(crate::context::to_string(agent, arg)?.to_string_lossy());
    }
    let body_string = match body_arg {
        Some(arg) => crate::context::to_string(agent, arg)?.to_string_lossy(),
        None => String::new(),
    };
    let param_string = param_strings.join(",");
    let source = format!("function anonymous({param_string}\n) {{\n{body_string}\n}}");
    let function_ast = parser::parse_function(&source)?;
    let func_proto = get_prototype_from_constructor(agent, &new_target)?;
    let environment = agent.current_realm()?.global_env();
    crate::function::instantiate_dynamic_function(
        agent,
        &function_ast,
        environment,
        func_proto,
        Some(crux::string::JsString::from_utf8(&source)),
    )
}

/// GetPrototypeFromConstructor (spec 10.2.4): `constructor.prototype` when it
/// is an object, else the realm's %Function.prototype% (GetFunctionRealm).
fn get_prototype_from_constructor(
    agent: &mut Agent,
    constructor: &Value,
) -> Result<Handle<JsObject>, JsError> {
    let proto = get_property_key(
        agent,
        constructor,
        &PropertyKey::from_utf8("prototype"),
        *constructor,
    )?;
    match as_object(&proto) {
        Some(handle) => Ok(handle),
        None => crate::context::get_function_realm(agent, constructor)?
            .intrinsics
            .get(FUNCTION_PROTO)
            .and_then(|value| as_object(&value))
            .ok_or_else(|| {
                JsError::new(
                    ErrorKind::TypeError,
                    format!("{FUNCTION_PROTO} is not defined"),
                )
            }),
    }
}

/// Function.prototype.apply (spec 20.2.3.2).
fn apply(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let func = *this;
    if !is_callable(&func) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Apply must be called on a function".into(),
        ));
    }
    let this_arg = args.first().cloned().unwrap_or(Value::Undefined);
    let arg_array = args.get(1).cloned().unwrap_or(Value::Undefined);
    if matches!(arg_array.kind(), ValueKind::Undefined | ValueKind::Null) {
        return match try_leaf_call(agent, &func, this_arg, &[]) {
            Some(result) => result,
            None => crate::function::call(agent, &func, this_arg, &[]),
        };
    }
    // GC-2: the collected argument list sits in a local Vec the stack scan
    // cannot see while the callee (user code) allocates — suppress
    // `--gc-stress` for the build and the call so the list cannot be swept
    // out from under the callee's parameter binding.
    let _stress = crate::ir::StressSuppress::new();
    let arg_list = create_list_from_array_like(agent, &arg_array)?;
    match try_leaf_call(agent, &func, this_arg, &arg_list) {
        Some(result) => result,
        None => crate::function::call(agent, &func, this_arg, &arg_list),
    }
}

/// Function.prototype.call (spec 20.2.3.4).
fn call_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let func = *this;
    if !is_callable(&func) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Call must be called on a function".into(),
        ));
    }
    let this_arg = args.first().cloned().unwrap_or(Value::Undefined);
    let rest = args.get(1..).unwrap_or(&[]);
    match try_leaf_call(agent, &func, this_arg, rest) {
        Some(result) => result,
        None => crate::function::call(agent, &func, this_arg, rest),
    }
}

/// The apply/call builtins' leaf fast path: a certified-leaf callee runs
/// through the leaf machinery on a pooled Vm — the register-op (or JIT)
/// body execution with no execution-context push, mirroring how the
/// vector-call steps route a `f(...args)` call through `do_call_fast`.
/// `None` when the callee is not a leaf-inlineable ES function (the
/// caller falls back to the general `crate::function::call`).
fn try_leaf_call(
    agent: &mut Agent,
    func: &Value,
    this_arg: Value,
    arg_list: &[Value],
) -> Option<Result<Value, JsError>> {
    // Mirror `fast_call_core`'s leaf gate: a single realm (the leaf runs
    // with the current realm), an EcmaScript function, and a warm leaf
    // cache entry. `leaf_lookup` may populate the cache, so clone the
    // entry fields before the agent is reborrowed by the run below.
    if agent.realm_count.get() != 1 {
        return None;
    }
    let ValueKind::Function(function) = func.kind() else {
        return None;
    };
    if !matches!(function.kind, crux::function::FunctionKind::EcmaScript) {
        return None;
    }
    let entry = agent.leaf_lookup(function.id())?;
    let entry = crate::ir::LeafEntry {
        ir: entry.ir.clone(),
        strict: entry.strict,
        environment: entry.environment,
        construct_inline: false,
    };
    let Ok(context) = agent.running_context() else {
        return None;
    };
    let env = context.lexical_environment;
    let mut vm = agent.take_vm(env, entry.strict);
    // GC-2: the leaf's arguments and result live on this pooled Vm's
    // stack, which the precise tracer cannot see — register the Vm as an
    // active run for the whole leaf window (the pushes and the body), so
    // a budget collection inside the body traces it exactly like
    // `run_inner`.
    let result = crate::ir::with_leaf_run(&mut vm, std::rc::Rc::as_ptr(&entry.ir), || {
        vm.stack.push(this_arg);
        vm.stack.push(*func);
        vm.stack.extend_from_slice(arg_list);
        vm.do_call_fast(agent, arg_list.len(), false)
    })
    .and_then(|()| {
        vm.stack.pop().ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "the leaf call produced no result".into(),
            )
        })
    });
    agent.return_vm(vm);
    Some(result)
}

/// Function.prototype.bind (spec 20.2.3.3).
fn bind(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let target = *this;
    if !is_callable(&target) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Bind must be called on a function".into(),
        ));
    }
    let this_arg = args.first().cloned().unwrap_or(Value::Undefined);
    let bound_args = args.get(1..).unwrap_or(&[]).to_vec();
    let proto = agent
        .current_realm()?
        .intrinsics
        .get(FUNCTION_PROTO)
        .and_then(|value| as_object(&value));
    let bound = Function::bound_function_create(target, this_arg, bound_args.clone(), proto)?;

    // SetFunctionLength (spec steps 4-7): always an own `length`, computed
    // from the target's when it is a Number.
    let mut length = 0.0;
    let has_length = match target.kind() {
        ValueKind::Function(f) => f.has_own_property(&JsString::from_utf8("length"))?,
        ValueKind::Object(obj) => obj.has_own_property(&JsString::from_utf8("length"))?,
        _ => false,
    };
    if has_length {
        let target_length =
            get_property_key(agent, &target, &PropertyKey::from_utf8("length"), target)?;
        if let ValueKind::Number(number) = target_length.kind() {
            let int = to_integer_or_infinity(number);
            length = if int == f64::INFINITY {
                f64::INFINITY
            } else if int == f64::NEG_INFINITY {
                0.0
            } else {
                (int - bound_args.len() as f64).max(0.0)
            };
        }
    }
    bound.define_property(
        &JsString::from_utf8("length"),
        &PropertyDescriptor {
            value: Some(Value::Number(length)),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // SetFunctionName with the "bound " prefix (spec steps 8-10).
    let target_name = get_property_key(agent, &target, &PropertyKey::from_utf8("name"), target)?;
    let target_name = match target_name.kind() {
        ValueKind::String(text) => text.as_ref().clone(),
        _ => JsString::from_utf8(""),
    };
    crate::function::set_function_name(&bound.self_value(), &target_name, Some("bound"))?;
    Ok(bound.self_value())
}

/// Function.prototype.toString (spec 20.2.3.5): the exact source text of an
/// ECMAScript function, or the native form when no source is tracked
/// (HostHasSourceTextAvailable).
fn function_to_string(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    if !is_callable(this) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Function.prototype.toString requires a callable this".into(),
        ));
    }
    if let ValueKind::Function(function) = this.kind()
        && let Some(data) = agent.ecma_functions.get(&function.id())
        && let Some(source) = &data.source
    {
        return Ok(Value::String(Handle::new(source.clone())));
    }
    let name = get_property_key(agent, this, &PropertyKey::from_utf8("name"), *this)?;
    let name = match name.kind() {
        ValueKind::String(text) => text.to_string_lossy(),
        _ => String::new(),
    };
    Ok(Value::String(Handle::new(JsString::from_utf8(&format!(
        "function {name}() {{ [native code] }}"
    )))))
}

/// CreateListFromArrayLike (spec 7.3.19): `length` then indexed `Get`s.
fn create_list_from_array_like(agent: &mut Agent, value: &Value) -> Result<Vec<Value>, JsError> {
    if !matches!(value.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "CreateListFromArrayLike called on non-object".into(),
        ));
    }
    let length = get_property_key(agent, value, &PropertyKey::from_utf8("length"), *value)?;
    let length = to_length(to_number(&length)?);
    // Fast path: a dense Array's elements are own data properties in the
    // linear property store, so per-index [[Get]] would be O(n²) (the
    // property-escape fixtures call `String.fromCodePoint.apply(null, …)`
    // on 10k-element arrays). Read the store once; any accessor or hole
    // falls back to the spec [[Get]] loop.
    if let ValueKind::Object(obj) = value.kind()
        && matches!(obj.kind, crux::object::ObjectKind::Array(_))
    {
        // Dense: the elements are the buffer slots (index = position) — a
        // direct read with no per-index [[Get]]. A hole (or a length past
        // the buffer end) falls back to the [[Get]] loop below.
        if let crux::object::ObjectKind::Array(slots) = &obj.kind
            && slots.dense.get()
        {
            let elements = slots.elements.borrow();
            let mut values = Vec::with_capacity(length as usize);
            for index in 0..length {
                match elements.get(index as usize).and_then(|e| *e) {
                    Some(item) => values.push(item),
                    None => break,
                }
            }
            if values.len() == length as usize {
                return Ok(values);
            }
        }
        let props = obj.properties.borrow();
        if (length as usize) <= props.len() {
            let mut values: Vec<Option<Value>> = vec![None; length as usize];
            let mut dense = true;
            for (key, prop) in props.iter() {
                let Some(index) = crux::object::array_index_of(key) else {
                    continue;
                };
                if index >= length {
                    continue;
                }
                match &prop.kind {
                    crux::object::PropertyKind::Data { value: item, .. } => {
                        values[index as usize] = Some(*item);
                    }
                    crux::object::PropertyKind::Accessor { .. } => {
                        dense = false;
                        break;
                    }
                }
            }
            if dense && values.iter().all(Option::is_some) {
                return Ok(values.into_iter().map(Option::unwrap).collect());
            }
        }
    }
    // GC-2: the collected elements sit in a local Vec the stack scan cannot
    // see while the next indexed `Get` allocates (a TypedArray element read
    // boxes a fresh value; `to_string` boxes a key) — suppress `--gc-stress`
    // for the loop so the half-built list cannot be swept (the caller roots
    // it once it is passed to the call).
    let _stress = crate::ir::StressSuppress::new();
    let mut list = Vec::new();
    for index in 0..length {
        let item = get_property_key(
            agent,
            value,
            &PropertyKey::from_utf8(&index.to_string()),
            *value,
        )?;
        list.push(item);
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(source: &str) -> Value {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script(source)
            .unwrap_or_else(|e| panic!("{source}: {:?} {e}", e.kind))
    }

    fn errors(source: &str) {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        assert!(
            agent.run_script(source).is_err(),
            "{source} should have thrown"
        );
    }

    #[test]
    fn function_constructor_creates_callable_functions() {
        assert_eq!(
            value("Function('a', 'b', 'return a + b')(2, 3)"),
            Value::Number(5.0)
        );
        assert_eq!(
            value("new Function('return 40 + 2')()"),
            Value::Number(42.0)
        );
        assert_eq!(value("Function()()"), Value::Undefined);
        assert_eq!(
            value("Function('x', 'return x * 2')(21)"),
            Value::Number(42.0)
        );
        // Parameters and body are strings; a single argument is the body.
        assert_eq!(
            value("Function('return this')() === globalThis"),
            Value::Boolean(true)
        );
    }

    #[test]
    fn dynamic_function_bad_syntax_throws() {
        errors("new Function('a b', 'return a')");
        errors("Function('return 1 +')");
        errors("Function('{')");
    }

    #[test]
    fn apply_and_call_dispatch_this_and_arguments() {
        assert_eq!(
            value("(function (a, b) { return a + b; }).call(null, 2, 3)"),
            Value::Number(5.0)
        );
        assert_eq!(
            value("(function (a, b) { return a + b; }).apply(null, [4, 5])"),
            Value::Number(9.0)
        );
        assert_eq!(
            value("(function () { return this.x; }).call({ x: 7 })"),
            Value::Number(7.0)
        );
        // apply with undefined/null argArray forwards no arguments.
        let result = value("(function (a, b) { return a + b; }).apply(null, undefined)");
        assert!(matches!(result.kind(), ValueKind::Number(n) if n.is_nan()));
        // apply with a non-object argArray is a TypeError.
        errors("(function () {}).apply(null, 'x')");
        errors("Function.prototype.call.call(1)");
    }

    #[test]
    fn apply_call_leaf_fast_path_preserves_semantics() {
        // A certified-leaf callee takes the leaf fast path (apply/call
        // route through `do_call_fast` on a pooled Vm): the leaf body's
        // frame binds the argument list exactly like the general call,
        // including the beyond-`FAST_CALL_MAX_ARGS` count (the vector
        // form's Vec fallback).
        assert_eq!(
            value(
                "(function (a, b, c, d, e, g, h, k, l) { return a + b + c + d + e + g + h + k + l; }).apply(null, [1, 2, 3, 4, 5, 6, 7, 8, 9])"
            ),
            Value::Number(45.0)
        );
        assert_eq!(
            value(
                "(function (a, b, c, d, e, g, h, k, l) { return a + b + c + d + e + g + h + k + l; }).call(null, 1, 2, 3, 4, 5, 6, 7, 8, 9)"
            ),
            Value::Number(45.0)
        );
        // Missing arguments stay `undefined` (spec 10.2.11), matching the
        // general call.
        assert_eq!(
            value("(function (a, b) { return a + (b === undefined ? 100 : b); }).apply(null, [1])"),
            Value::Number(101.0)
        );
        // A capturing leaf reads its closure environment through the
        // fast-path body context.
        assert_eq!(
            value(
                "(function () { var x = 5; return (function (a) { return a + x; }).apply(null, [7]); })()"
            ),
            Value::Number(12.0)
        );
        // `this` binds through OrdinaryCallBindThis on the leaf path.
        assert_eq!(
            value("(function () { return this.x; }).call({ x: 7 })"),
            Value::Number(7.0)
        );
        assert_eq!(
            value("(function () { 'use strict'; return this; }).apply(3) === 3"),
            Value::Boolean(true)
        );
        // A throwing leaf propagates the error.
        errors("(function () { throw new RangeError('boom'); }).apply(null, [])");
        errors("(function (a) { return a.missing.deep; }).apply(null, [null])");
        // The vector-form call (`f(...)` / ≥9 plain args) takes the same
        // leaf-inline path on the interpreter.
        assert_eq!(
            value(
                "(function (a, b, c, d, e, g, h, k, l) { return a + l; })(1, 2, 3, 4, 5, 6, 7, 8, 9)"
            ),
            Value::Number(10.0)
        );
        assert_eq!(
            value(
                "var f = function (a, b, c, d, e, g, h, k, l) { return a * l; }; f(...[1, 2, 3, 4, 5, 6, 7, 8, 9])"
            ),
            Value::Number(9.0)
        );
    }

    #[test]
    fn bind_fixes_this_and_prefixes_arguments() {
        assert_eq!(
            value("(function (a, b) { return a + b; }).bind(null, 2)(3)"),
            Value::Number(5.0)
        );
        assert_eq!(
            value("(function () { return this.x; }).bind({ x: 9 })()"),
            Value::Number(9.0)
        );
        // Bound length and name follow the target (spec steps 4-10).
        assert_eq!(
            value("(function (a, b, c) {}).bind(null).length"),
            Value::Number(3.0)
        );
        assert_eq!(
            value("(function (a, b, c) {}).bind(null, 1, 2).length"),
            Value::Number(1.0)
        );
        let bound_name = value("(function myFn() {}).bind(null).name");
        assert!(
            matches!(bound_name.kind(), ValueKind::String(s) if s.to_string_lossy() == "bound myFn")
        );
        errors("Function.prototype.bind.call(1)");
    }

    #[test]
    fn functions_inherit_from_function_prototype() {
        assert_eq!(
            value("(function () {}) instanceof Function"),
            Value::Boolean(true)
        );
        assert_eq!(value("Function instanceof Function"), Value::Boolean(true));
        assert_eq!(
            value("Function.prototype instanceof Function"),
            Value::Boolean(false)
        );
        assert_eq!(
            value("(function () {}).apply instanceof Function"),
            Value::Boolean(true)
        );
        assert_eq!(
            value("new Function('return 1') instanceof Function"),
            Value::Boolean(true)
        );
        assert_eq!(value("({}) instanceof Function"), Value::Boolean(false));
    }

    #[test]
    fn custom_has_instance_overrides_instanceof() {
        // The global `Symbol` constructor is Phase 8; install the well-known
        // @@hasInstance property from Rust to exercise the dispatch.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent.run_script("function C() {}").unwrap();
        let ctor = match agent.run_script("C").unwrap().kind() {
            ValueKind::Function(f) => f,
            other => panic!("C should be a function, got {other:?}"),
        };
        let key = PropertyKey::Symbol(crux::symbol::well_known("hasInstance"));
        let override_with = |result: bool| {
            let method = Function::create_builtin(
                Some(JsString::from_utf8("[Symbol.hasInstance]")),
                1,
                Box::new(move |_, _| Ok(Value::Boolean(result))),
                None,
                None,
            )
            .unwrap();
            ctor.define_property_key(&key, &PropertyDescriptor::data(Value::Function(method)))
                .unwrap();
        };
        override_with(true);
        assert_eq!(
            agent.run_script("({}) instanceof C").unwrap(),
            Value::Boolean(true)
        );
        override_with(false);
        assert_eq!(
            agent.run_script("({}) instanceof C").unwrap(),
            Value::Boolean(false)
        );
        // The inherited default still walks the prototype chain.
        assert_eq!(
            value("function C() {} let c = new C(); c instanceof C"),
            Value::Boolean(true)
        );
    }

    #[test]
    fn function_prototype_is_callable_without_construct() {
        assert_eq!(value("Function.prototype()"), Value::Undefined);
        assert_eq!(value("Function.prototype.length"), Value::Number(0.0));
        let name = value("Function.prototype.name");
        assert!(matches!(name.kind(), ValueKind::String(s) if s.to_string_lossy() == ""));
        // Not a constructor.
        errors("new Function.prototype()");
    }

    #[test]
    fn function_to_string_renders_source_or_native() {
        // User functions render their exact source text (spec 20.2.3.5).
        let text = value("(function named() { return 1; }).toString()");
        assert!(
            matches!(text.kind(), ValueKind::String(s) if s.to_string_lossy() == "function named() { return 1; }")
        );
        // Function expressions with whitespace round-trip exactly.
        let text = value("var f = function (a, b) {\n  return a + b;\n}; f.toString()");
        assert!(
            matches!(text.kind(), ValueKind::String(s) if s.to_string_lossy() == "function (a, b) {\n  return a + b;\n}")
        );
        // Arrow functions have no tracked source (native form).
        let text = value("var g = (x) => x; g.toString()");
        assert!(
            matches!(text.kind(), ValueKind::String(s) if s.to_string_lossy() == "function g() { [native code] }")
        );
        // %Function.prototype% has an empty name.
        let text = value("Function.prototype.toString()");
        assert!(
            matches!(text.kind(), ValueKind::String(s) if s.to_string_lossy() == "function () { [native code] }")
        );
        errors("Function.prototype.toString.call({})");
    }

    #[test]
    fn function_constructor_properties() {
        assert_eq!(value("Function.length"), Value::Number(1.0));
        assert_eq!(
            value("Function.name"),
            Value::String(Handle::new(JsString::from_utf8("Function")))
        );
        assert_eq!(
            value("Function.prototype.constructor === Function"),
            Value::Boolean(true)
        );
        assert_eq!(
            value("typeof Function.prototype"),
            Value::String(Handle::new(JsString::from_utf8("function")))
        );
    }

    #[test]
    fn dynamic_functions_capture_global_scope() {
        assert_eq!(
            value("var g = 10; Function('return g')()"),
            Value::Number(10.0)
        );
        assert_eq!(
            value("let g = 20; Function('return typeof g')()"),
            Value::String(Handle::new(JsString::from_utf8("number")))
        );
    }

    #[test]
    fn restricted_caller_arguments_properties() {
        // Sloppy ordinary functions have own undefined-valued caller/
        // arguments data properties (no caller tracked, so reads are
        // undefined rather than the actual caller).
        assert_eq!(
            value(
                "(function () { var d = Object.getOwnPropertyDescriptor(arguments.callee, 'caller'); return d.value === undefined && d.writable === false && d.configurable === false; })()"
            ),
            Value::Boolean(true)
        );
        // %Function.prototype% carries own caller/arguments accessors whose
        // get and set are the same function, non-enumerable, configurable.
        assert_eq!(
            value(
                "(function () { var d = Object.getOwnPropertyDescriptor(Function.prototype, 'caller'); var a = Object.getOwnPropertyDescriptor(Function.prototype, 'arguments'); return typeof d.get === 'function' && d.get === d.set && d.get === a.get && d.enumerable === false && d.configurable === true; })()"
            ),
            Value::Boolean(true)
        );
        // Strict functions have no own properties; reads and writes throw.
        assert_eq!(
            value(
                "(function () { 'use strict'; function f() {} return f.hasOwnProperty('caller') === false && f.hasOwnProperty('arguments') === false; })()"
            ),
            Value::Boolean(true)
        );
        errors("(function () { 'use strict'; function f() {} f.caller; })()");
        errors("(function () { 'use strict'; function f() {} f.caller = {}; })()");
        errors("(function () { 'use strict'; function f() {} f.arguments; })()");
        // Bound functions have no own properties; reads and writes throw.
        errors("(function () { var b = (function () {}).bind(null); return b.caller; })()");
        errors("(function () { var b = (function () {}).bind(null); b.arguments = 1; })()");
    }

    #[test]
    fn apply_call_box_non_nullish_this_arg() {
        // spec 20.2.3.2/20.2.3.4: a non-nullish thisArg is ToObject'd, so
        // the callee sees a Number wrapper with the property set.
        assert_eq!(
            value("Function('this.touched = true; return this;').apply(1) instanceof Number"),
            Value::Boolean(true)
        );
        assert_eq!(
            value("Function('this.touched = true; return this;').call('s').touched"),
            Value::Boolean(true)
        );
    }

    #[test]
    fn function_values_work_as_prototypes_in_construct() {
        // OrdinaryCreateFromConstructor accepts a function-valued prototype:
        // new objects inherit through it to %Function.prototype%.
        assert_eq!(
            value(
                "var p = Function(); function F() {} F.prototype = p; var o = new F; typeof o.apply"
            ),
            Value::String(Handle::new(JsString::from_utf8("function")))
        );
    }

    #[test]
    fn bound_functions_delegate_instanceof() {
        // OrdinaryHasInstance (spec 7.3.19 step 2): a bound function unwraps
        // to its target.
        assert_eq!(
            value(
                "var BC = function () {}; var bc = new BC(); var bound = BC.bind(); bound[Symbol.hasInstance](bc)"
            ),
            Value::Boolean(true)
        );
        assert_eq!(
            value(
                "var BC = function () {}; var bound = BC.bind(); var other = {}; other instanceof bound"
            ),
            Value::Boolean(false)
        );
    }

    #[test]
    fn dynamic_function_string_coercion_and_private_identifiers() {
        // ToString runs through the agent: new Function({}) parses
        // "[object Object]" and throws a SyntaxError.
        errors("new Function({})");
        // CreateDynamicFunction: private identifiers in the body are a
        // SyntaxError (AllPrivateIdentifiersValid with no enclosing class).
        errors("new Function('o.#f')");
        errors("new Function('return #f in o')");
    }
}
