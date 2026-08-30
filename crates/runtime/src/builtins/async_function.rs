//! The GeneratorFunction, AsyncFunction, and AsyncGeneratorFunction
//! constructors (spec 27.4.2, 27.5.2, 27.6.2): CreateDynamicFunction with the
//! ~generator~, ~async~, and ~async generator~ kinds. They are not global
//! bindings; user code reaches them through
//! `Object.getPrototypeOf(function*(){}).constructor` etc.

use crux::convert::to_string;
use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use crate::agent::Agent;
use crate::context::as_object;
use crate::realm::Realm;

const GENERATOR_FUNCTION: &str = "%GeneratorFunction%";
const GENERATOR_FUNCTION_PROTO: &str = "%GeneratorFunction.prototype%";
const ASYNC_FUNCTION: &str = "%AsyncFunction%";
const ASYNC_FUNCTION_PROTO: &str = "%AsyncFunction.prototype%";
const ASYNC_GENERATOR_FUNCTION: &str = "%AsyncGeneratorFunction%";
const ASYNC_GENERATOR_FUNCTION_PROTO: &str = "%AsyncGeneratorFunction.prototype%";

/// The CreateDynamicFunction kind (spec 20.2.1.1 step 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Generator,
    Async,
    AsyncGenerator,
}

impl Kind {
    fn all() -> [Kind; 3] {
        [Kind::Generator, Kind::Async, Kind::AsyncGenerator]
    }

    fn key(self) -> &'static str {
        match self {
            Kind::Generator => GENERATOR_FUNCTION,
            Kind::Async => ASYNC_FUNCTION,
            Kind::AsyncGenerator => ASYNC_GENERATOR_FUNCTION,
        }
    }

    fn proto_key(self) -> &'static str {
        match self {
            Kind::Generator => GENERATOR_FUNCTION_PROTO,
            Kind::Async => ASYNC_FUNCTION_PROTO,
            Kind::AsyncGenerator => ASYNC_GENERATOR_FUNCTION_PROTO,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Generator => "GeneratorFunction",
            Kind::Async => "AsyncFunction",
            Kind::AsyncGenerator => "AsyncGeneratorFunction",
        }
    }

    /// The @@toStringTag value (spec 27.4.3.3/27.5.3.3/27.6.3.3).
    fn tag(&self) -> &'static str {
        match self {
            Kind::Generator => "GeneratorFunction",
            Kind::Async => "AsyncFunction",
            Kind::AsyncGenerator => "AsyncGeneratorFunction",
        }
    }

    fn is_async(self) -> bool {
        matches!(self, Kind::Async | Kind::AsyncGenerator)
    }

    /// The intrinsic whose prototype the created function's `prototype`
    /// property inherits; `None` for async functions, which have no
    /// `prototype` property.
    fn instance_proto_intrinsic(self) -> Option<&'static str> {
        match self {
            Kind::Generator => Some("%Generator.prototype%"),
            Kind::Async => None,
            Kind::AsyncGenerator => Some("%AsyncGenerator.prototype%"),
        }
    }

    fn source_prefix(self) -> &'static str {
        match self {
            Kind::Generator => "function*",
            Kind::Async => "async function",
            Kind::AsyncGenerator => "async function*",
        }
    }
}

/// Install the three constructor intrinsics and their prototype objects.
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let function_proto = realm
        .intrinsics
        .get("%Function.prototype%")
        .and_then(|value| as_object(&value));
    let function_ctor = realm
        .intrinsics
        .get("%Function%")
        .and_then(|value| as_object(&value));
    for kind in Kind::all() {
        let proto = JsObject::ordinary_object_create(function_proto);
        let proto_value = Value::Object(proto);
        let ctor = Function::create_builtin(
            Some(JsString::from_utf8(kind.name())),
            1,
            Box::new(placeholder(kind.name())),
            Some(Box::new(placeholder(kind.name()))),
            None,
        )?;
        let ctor_value = Value::Function(ctor);
        realm.intrinsics.define(kind.key(), ctor_value);
        realm.intrinsics.define(kind.proto_key(), proto_value);

        // The prototype's own `prototype` property is the generator (or async
        // generator) prototype intrinsic (spec 27.4.3.2/27.6.3.2); async
        // functions' prototype object has none.
        if let Some(intrinsic) = kind.instance_proto_intrinsic() {
            let instance_proto = realm.intrinsics.get(intrinsic).ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, format!("{intrinsic} missing"))
            })?;
            proto.define_property(
                &JsString::from_utf8("prototype"),
                &PropertyDescriptor {
                    value: Some(instance_proto),
                    writable: Some(false),
                    get: None,
                    set: None,
                    enumerable: Some(false),
                    configurable: Some(true),
                },
            )?;
            // spec 27.4.3.1/27.6.3.1: %Generator.prototype%.constructor is
            // the GeneratorFunction prototype object (the "Generator" in
            // `Object.getPrototypeOf(g)`), so `G.prototype.constructor === G`
            // (GeneratorPrototype/constructor.js).
            if let ValueKind::Object(instance_obj) = instance_proto.kind() {
                instance_obj.define_property(
                    &JsString::from_utf8("constructor"),
                    &PropertyDescriptor {
                        value: Some(proto_value),
                        writable: Some(false),
                        get: None,
                        set: None,
                        enumerable: Some(false),
                        configurable: Some(true),
                    },
                )?;
            }
        }

        // `constructor` back-reference (spec 27.4.3.1/27.5.3.1/27.6.3.1).
        proto.define_property(
            &JsString::from_utf8("constructor"),
            &PropertyDescriptor {
                value: Some(ctor_value),
                writable: Some(false),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
        // @@toStringTag (spec 27.4.3.3/27.5.3.3/27.6.3.3).
        proto.define_property_key(
            &PropertyKey::Symbol(crux::symbol::well_known("toStringTag")),
            &PropertyDescriptor {
                value: Some(Value::String(Handle::new(JsString::from_utf8(kind.tag())))),
                writable: Some(false),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;

        // 27.4.2/27.5.2/27.6.2: the constructor's `prototype` property, and
        // its own [[Prototype]] is %Function% (the Function constructor).
        ctor.define_property(
            &JsString::from_utf8("prototype"),
            &PropertyDescriptor {
                value: Some(proto_value),
                writable: Some(false),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(false),
            },
        )?;
        if let Some(function_ctor) = &function_ctor {
            ctor.object.set_prototype_of(Some(*function_ctor))?;
        }
    }
    Ok(())
}

fn placeholder(name: &'static str) -> crux::function::NativeFn {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

/// Route a call to the three constructors by identity.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    _this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    for kind in Kind::all() {
        if realm.intrinsics.get(kind.key()).as_ref() == Some(callee) {
            return Some(create_dynamic_function(
                agent,
                callee,
                &Value::Undefined,
                args,
                kind,
            ));
        }
    }
    None
}

/// Route `new` on the three constructors to CreateDynamicFunction.
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    for kind in Kind::all() {
        if realm.intrinsics.get(kind.key()).as_ref() == Some(callee) {
            return Some(create_dynamic_function(
                agent, callee, new_target, args, kind,
            ));
        }
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

/// CreateDynamicFunction (spec 20.2.1.1): assemble the kind's source form,
/// parse it, and instantiate with the GetPrototypeFromConstructor prototype.
fn create_dynamic_function(
    agent: &mut Agent,
    ctor: &Value,
    new_target: &Value,
    args: &[Value],
    kind: Kind,
) -> Result<Value, JsError> {
    let new_target = if matches!(new_target.kind(), ValueKind::Undefined) {
        *ctor
    } else {
        *new_target
    };
    let (param_args, body_arg) = split_dynamic_args(args);
    let mut param_strings = Vec::new();
    for arg in param_args {
        param_strings.push(to_string(arg)?.to_string_lossy());
    }
    let body_string = match body_arg {
        Some(arg) => to_string(arg)?.to_string_lossy(),
        None => String::new(),
    };
    let param_string = param_strings.join(",");
    let source = format!(
        "{} anonymous({param_string}\n) {{\n{body_string}\n}}",
        kind.source_prefix()
    );
    let function_ast = parser::parse_function_with_async(&source, kind.is_async())?;
    let func_proto = get_prototype_from_constructor(agent, &new_target, kind.proto_key())?;
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
/// is an object, else the realm's intrinsic `intrinsic_name`.
fn get_prototype_from_constructor(
    agent: &mut Agent,
    constructor: &Value,
    intrinsic_name: &str,
) -> Result<Handle<JsObject>, JsError> {
    let proto = crate::context::get_property_key(
        agent,
        constructor,
        &PropertyKey::from_utf8("prototype"),
        *constructor,
    )?;
    match crate::context::as_object(&proto) {
        Some(handle) => Ok(handle),
        None => crate::context::get_function_realm(agent, constructor)?
            .intrinsics
            .get(intrinsic_name)
            .and_then(|value| as_object(&value))
            .ok_or_else(|| {
                JsError::new(
                    ErrorKind::TypeError,
                    format!("{intrinsic_name} is not defined"),
                )
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::evaluate;

    fn run(source: &str) -> Result<Value, JsError> {
        evaluate(source)
    }

    #[test]
    fn generator_function_constructor_creates_working_generators() {
        assert_eq!(
            run(concat!(
                "const GeneratorFunction = Object.getPrototypeOf(function* () {}).constructor;",
                "JSON.stringify([...GeneratorFunction('n', 'for (let i = 0; i < n; i++) yield i * 2')(3)]);"
            ))
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("[0,2,4]")))
        );
    }

    #[test]
    fn generator_function_prototype_shapes() {
        assert_eq!(
            run(concat!(
                "const GeneratorFunction = Object.getPrototypeOf(function* () {}).constructor;",
                "JSON.stringify([GeneratorFunction.prototype[Symbol.toStringTag],",
                "GeneratorFunction.prototype.prototype === Object.getPrototypeOf((function* () {}).prototype),",
                "GeneratorFunction.prototype.constructor === GeneratorFunction]);"
            ))
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8(
                "[\"GeneratorFunction\",true,true]"
            )))
        );
    }

    #[test]
    fn async_function_constructor_returns_a_promise() {
        assert_eq!(
            run(concat!(
                "const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;",
                "const af = AsyncFunction('x', 'return x + 1');",
                "JSON.stringify([typeof af, af(1) instanceof Promise, typeof AsyncFunction.prototype]);"
            ))
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("[\"function\",true,\"object\"]")))
        );
    }

    #[test]
    fn generator_instances_are_not_constructors() {
        assert_eq!(
            run(concat!(
                "const f = function* () {};",
                "let t = false;",
                "try { new f(); } catch (e) { t = e instanceof TypeError; }",
                "t;"
            ))
            .unwrap(),
            Value::Boolean(true)
        );
    }
}
