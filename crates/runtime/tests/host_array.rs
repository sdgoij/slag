//! A host array through script: the Array exotic's own semantics (isArray,
//! length, indexed access, `push`, `join`, enumeration) on an object whose
//! structural internal methods a host answers.

use std::cell::Cell;
use std::rc::Rc;

use crux::error::JsError;
use crux::host_array::HostArrayOps;
use crux::object::JsObject;
use crux::property::PropertyKey;
use crux::string::JsString;
use crux::value::Value;
use runtime::api::{Context, Isolate};

/// Delegates every structural operation and counts the sets, so the array
/// behaves like an array while the test can see script reach the hook.
#[derive(Debug, Default)]
struct Counting {
    sets: Cell<u32>,
}

impl HostArrayOps for Counting {
    fn set(
        &self,
        array: &JsObject,
        key: &PropertyKey,
        value: &Value,
        receiver: &Value,
        throw: bool,
    ) -> Option<Result<bool, JsError>> {
        self.sets.set(self.sets.get() + 1);
        Some(array.host_array_delegate_set(key, *value, *receiver, throw))
    }
}

#[test]
fn a_host_array_is_an_array_to_script() {
    let mut isolate = Isolate::new();
    let context = Context::new(&mut isolate).expect("the context");
    let global = context.global().as_object().expect("a global object");
    let ops = Rc::new(Counting::default());
    let array = JsObject::host_array_create(None, ops.clone());
    global
        .create_data_property_or_throw(&JsString::from_utf8("__host"), Value::Object(array))
        .expect("the global install");
    // An embedding passes %Array.prototype% at creation; the script does it here
    // so the case also covers a prototype the engine was not told about.
    context
        .try_eval("Object.setPrototypeOf(__host, Array.prototype)")
        .expect("the prototype install");

    let cases = [
        ("Array.isArray(__host)", Value::Boolean(true)),
        ("__host instanceof Array", Value::Boolean(true)),
        ("__host.length", Value::Number(0.0)),
        ("__host[0] = 5; __host[0]", Value::Number(5.0)),
        // The length write-back a delegated define performs.
        ("__host.length", Value::Number(1.0)),
        ("__host.push(6)", Value::Number(2.0)),
        ("__host.length", Value::Number(2.0)),
        ("__host.join(',') === '5,6'", Value::Boolean(true)),
        ("Object.keys(__host).length", Value::Number(2.0)),
    ];
    for (source, expected) in cases {
        assert_eq!(
            context.try_eval(source).expect(source).value(),
            &expected,
            "{source}"
        );
    }
    assert!(
        ops.sets.get() >= 1,
        "script's element write reached the host's [[Set]] hook"
    );
}
