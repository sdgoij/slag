//! The `%Proxy%` intrinsic (spec 28.2): the constructor that creates proxy
//! exotic objects and the `revocable` static. The trap machinery and
//! invariants live in `crux::proxy` (wired into the object internal methods
//! since Phase 5); these entry points create the proxy and the revoker.
//! `%Reflect%` (spec 28.1) lives in `builtins::reflect`.

use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeCtor, NativeFn};
use crux::handle::Handle;
use crux::object::{JsObject, ObjectKind};
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::Value;

use crate::agent::Agent;
use crate::context::as_object;
use crate::realm::Realm;

const PROXY: &str = "%Proxy%";
const PROXY_PROTO: &str = "%Proxy.prototype%";
const REVOCABLE: &str = "%Proxy.revocable%";

fn placeholder(name: &'static str) -> NativeFn {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

fn placeholder_ctor(name: &'static str) -> NativeCtor {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

/// Install the Proxy intrinsics and the global `Proxy` binding (spec
/// 28.2.1-28.2.3).
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let proxy_proto = JsObject::ordinary_object_create(object_proto);
    let proxy_proto_value = Value::Object(proxy_proto.clone());

    // %Proxy% (28.2.1): `new Proxy(target, handler)` creates the proxy; the
    // call form throws (spec 28.2.1.1 step 1).
    let proxy_ctor = Function::create_builtin(
        Some(JsString::from_utf8("Proxy")),
        2,
        placeholder("Proxy"),
        Some(placeholder_ctor("Proxy")),
        None,
    )?;
    let proxy_ctor_value = Value::Function(proxy_ctor.clone());

    realm.intrinsics.define(PROXY, proxy_ctor_value.clone());
    realm
        .intrinsics
        .define(PROXY_PROTO, proxy_proto_value.clone());

    // spec 26.2.1: the Proxy constructor has no `prototype` property; the
    // %Proxy.prototype% intrinsic still exists (with its own constructor
    // back-reference and @@toStringTag) but is not linked from the
    // constructor.
    proxy_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(proxy_ctor_value.clone()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    proxy_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor::none(Value::String(Handle::new(JsString::from_utf8("Proxy")))),
    )?;

    // Proxy.revocable (28.2.2).
    let revocable = Function::create_builtin(
        Some(JsString::from_utf8("revocable")),
        2,
        placeholder(REVOCABLE),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(REVOCABLE, Value::Function(revocable.clone()));
    proxy_ctor.define_property(
        &JsString::from_utf8("revocable"),
        &PropertyDescriptor {
            value: Some(Value::Function(revocable)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("Proxy"),
        &PropertyDescriptor {
            value: Some(proxy_ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    _this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(PROXY).as_ref() == Some(callee) {
        return Some(Err(JsError::new(
            ErrorKind::TypeError,
            "Constructor Proxy requires 'new'".into(),
        )));
    }
    if intrinsics.get(REVOCABLE).as_ref() == Some(callee) {
        return Some(proxy_revocable(agent, args));
    }
    None
}

pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    _new_target: &Value,
) -> Option<Result<Value, JsError>> {
    if agent
        .current_realm()
        .ok()
        .and_then(|realm| realm.intrinsics.get(PROXY))
        .as_ref()
        == Some(callee)
    {
        let target = args.first().cloned().unwrap_or(Value::Undefined);
        let handler = args.get(1).cloned().unwrap_or(Value::Undefined);
        return Some(crux::proxy::proxy_create(target, handler).map(Value::Object));
    }
    None
}

/// Proxy.revocable (spec 28.2.2): `{ proxy, revoke }`; `revoke` clears both
/// internal slots so every subsequent operation on the proxy throws.
fn proxy_revocable(agent: &mut Agent, args: &[Value]) -> Result<Value, JsError> {
    let target = args.first().cloned().unwrap_or(Value::Undefined);
    let handler = args.get(1).cloned().unwrap_or(Value::Undefined);
    let proxy = crux::proxy::proxy_create(target, handler)?;
    let slots = match &proxy.kind {
        ObjectKind::Proxy(slots) => slots.clone(),
        _ => unreachable!("proxy_create returns a proxy object"),
    };
    let revoke = Function::create_builtin(
        // spec 28.2.2.1.1: the revocation function is anonymous (name ""),
        // and its [[Prototype]] is %Function.prototype%.
        Some(JsString::from_utf8("")),
        0,
        Box::new(move |_, _| {
            crux::proxy::revoke(&slots);
            Ok(Value::Undefined)
        }),
        None,
        agent
            .current_realm()?
            .intrinsics
            .get("%Function.prototype%")
            .and_then(|value| as_object(&value)),
    )?;
    let object_proto = agent
        .current_realm()?
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let result = JsObject::ordinary_object_create(object_proto);
    result.create_data_property(&JsString::from_utf8("proxy"), Value::Object(proxy))?;
    result.create_data_property(&JsString::from_utf8("revoke"), Value::Function(revoke))?;
    Ok(Value::Object(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::evaluate;

    fn run(source: &str) -> Result<Value, JsError> {
        evaluate(source)
    }

    fn str(value: &str) -> Value {
        Value::String(Handle::new(JsString::from_utf8(value)))
    }

    #[test]
    fn proxy_forwards_without_traps() {
        assert_eq!(
            run("const p = new Proxy({ a: 1 }, {}); JSON.stringify([p.a, 'a' in p, Object.keys(p).length])")
                .unwrap(),
            str("[1,true,1]")
        );
        assert_eq!(
            run("const p = new Proxy({}, {}); p.b = 2; p.b").unwrap(),
            Value::Number(2.0)
        );
        assert_eq!(
            run("const p = new Proxy({}, {}); delete p.b || true").unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn proxy_traps_run_user_handlers() {
        assert_eq!(
            run(
                "const p = new Proxy({ base: 1 }, { get(t, k) { return k === 'extra' ? 42 : t[k]; } }); JSON.stringify([p.base, p.extra])"
            )
            .unwrap(),
            str("[1,42]")
        );
        assert_eq!(
            run(
                "const p = new Proxy({}, { has(t, k) { return k === 'secret' || k in t; } }); JSON.stringify(['secret' in p, 'other' in p])"
            )
            .unwrap(),
            str("[true,false]")
        );
        assert_eq!(
            run(
                "const p = new Proxy({}, { set(t, k, v) { t[k] = v * 2; return true; } }); p.z = 5; p.z"
            )
            .unwrap(),
            Value::Number(10.0)
        );
        assert_eq!(
            run("const p = new Proxy({}, { ownKeys(t) { return ['a', 'b']; } }); Reflect.ownKeys(p).join(',')")
                .unwrap(),
            str("a,b")
        );
    }

    #[test]
    fn proxy_requires_new() {
        assert!(run("Proxy({}, {})").is_err());
    }

    #[test]
    fn proxy_validates_target_and_handler() {
        assert!(run("new Proxy(1, {})").is_err());
        assert!(run("new Proxy({}, null)").is_err());
    }

    #[test]
    fn proxy_invariants_throw() {
        // The get trap must report the value of a non-writable,
        // non-configurable data property.
        assert!(run(
            "const target = {}; Object.defineProperty(target, 'x', { value: 1, writable: false, configurable: false }); const p = new Proxy(target, { get(t, k) { return 2; } }); p.x"
        )
        .is_err());
        // The getPrototypeOf trap must agree with a non-extensible target.
        assert!(run(
            "const target = {}; Object.preventExtensions(target); const p = new Proxy(target, { getPrototypeOf(t) { return {}; } }); Object.getPrototypeOf(p)"
        )
        .is_err());
    }

    #[test]
    fn proxy_over_callable_target_is_callable() {
        assert_eq!(
            run("const p = new Proxy(function (a, b) { return a + b; }, {}); p(2, 3)").unwrap(),
            Value::Number(5.0)
        );
        assert_eq!(
            run(
                "const p = new Proxy(function () {}, { apply(t, thisArg, args) { return args.length; } }); p(1, 2, 3)"
            )
            .unwrap(),
            Value::Number(3.0)
        );
    }

    #[test]
    fn proxy_over_constructible_target_is_constructible() {
        assert_eq!(
            run("const p = new Proxy(class { constructor() { this.x = 7; } }, {}); new p().x")
                .unwrap(),
            Value::Number(7.0)
        );
        assert_eq!(
            run(
                "const p = new Proxy(class {}, { construct(t, args, nt) { return { made: args[0] }; } }); new p('hi').made"
            )
            .unwrap(),
            str("hi")
        );
    }

    #[test]
    fn revocable_proxy_throws_after_revoke() {
        assert_eq!(
            run(
                "const r = Proxy.revocable({ x: 1 }, {}); r.proxy.x; r.revoke(); (() => { try { return r.proxy.x; } catch (e) { return e.constructor.name; } })()"
            )
            .unwrap(),
            str("TypeError")
        );
    }

    #[test]
    fn proxy_prototype_shapes() {
        assert_eq!(run("typeof Proxy.revocable").unwrap(), str("function"));
        // spec 26.2.1: the Proxy constructor has no prototype property.
        assert_eq!(run("Proxy.prototype").unwrap(), Value::Undefined);
        assert_eq!(
            run("Object.getPrototypeOf(Proxy) === Function.prototype").unwrap(),
            Value::Boolean(true)
        );
        // The revocation function's [[Prototype]] is %Function.prototype%.
        assert_eq!(
            run("Object.getPrototypeOf(Proxy.revocable({}, {}).revoke) === Function.prototype")
                .unwrap(),
            Value::Boolean(true)
        );
    }
}
