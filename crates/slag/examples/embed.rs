//! The Rust embedding API, end to end: create a context, route host
//! console output, evaluate, call, construct, expose globals, and (with the
//! `jit` feature) install the JIT hook.
//!
//! Run: `cargo run -p slag --example embed`
//!      `cargo run -p slag --example embed --features slag/jit`

use slag::{Context, ErrorKind, HostCallbacks, JsError, JsValue};

fn main() {
    // A fresh agent, realm, and host globals (`console`, timers) per context.
    let mut context = Context::new().unwrap();

    // Host console output routes through the callbacks.
    let callbacks = HostCallbacks {
        console_log: Some(Box::new(|text| println!("[js] {text}"))),
        ..HostCallbacks::default()
    };
    context.set_host_callbacks(callbacks);
    context.eval("console.log('host console works')").unwrap();

    // Evaluate in the global scope; the completion value comes back.
    let greet = context
        .eval("function greet(name) { return 'hello, ' + name; } greet")
        .unwrap();
    assert_eq!(greet.type_name(), "function");

    // Call a script-defined function with host-provided arguments.
    let result = context
        .call(&greet, &JsValue::undefined(), &[JsValue::string("slag")])
        .unwrap();
    assert_eq!(result.as_string().as_deref(), Some("hello, slag"));

    // Construct an object from a constructor value.
    let date_ctor = context.eval("Date").unwrap();
    let now = context.construct(&date_ctor, &[]).unwrap();
    assert_eq!(now.type_name(), "object");

    // Expose host values as globals; read script results back as numbers.
    context.set_global("answer", JsValue::number(42.0)).unwrap();
    assert_eq!(context.eval("answer * 2").unwrap().as_number(), Some(84.0));

    // Register a Rust closure as a callable global function.
    context
        .register_fn(
            "host_sum",
            2,
            Box::new(|call| {
                let a = call
                    .arg(0)
                    .and_then(|value| value.as_number())
                    .unwrap_or(0.0);
                let b = call
                    .arg(1)
                    .and_then(|value| value.as_number())
                    .unwrap_or(0.0);
                Ok(JsValue::number(a + b))
            }),
        )
        .unwrap();
    assert_eq!(
        context.eval("host_sum(20, 22)").unwrap().as_number(),
        Some(42.0)
    );

    // A namespace object built from Rust: create_object + create_function.
    let host_env = context.create_object().unwrap();
    let version = context
        .create_function("version", 0, Box::new(|_| Ok(JsValue::string("1.0"))))
        .unwrap();
    host_env.set("version", version).unwrap();
    context.set_global("host_env", host_env.as_value()).unwrap();
    assert_eq!(
        context
            .eval("host_env.version()")
            .unwrap()
            .as_string()
            .as_deref(),
        Some("1.0")
    );

    // A host function that calls back into JS synchronously.
    context
        .register_fn(
            "apply_to",
            2,
            Box::new(|call| {
                let function = call.arg(0).unwrap_or_else(JsValue::undefined);
                let value = call.arg(1).unwrap_or_else(JsValue::undefined);
                call.call(&function, &JsValue::undefined(), &[value])
            }),
        )
        .unwrap();
    assert_eq!(
        context
            .eval("apply_to(x => x * 3, 14)")
            .unwrap()
            .as_number(),
        Some(42.0)
    );

    // A host constructor: `new HostPoint(x, y)` builds an instance inheriting
    // from HostPoint.prototype and passes it as the callback's `this`.
    let host_point = context
        .create_constructor(
            "HostPoint",
            2,
            Box::new(|call| {
                let instance = call.this().as_object().ok_or_else(|| {
                    JsError::new(ErrorKind::TypeError, "HostPoint: requires new".into())
                })?;
                instance.set("x", call.arg(0).unwrap_or_else(JsValue::undefined))?;
                instance.set("y", call.arg(1).unwrap_or_else(JsValue::undefined))?;
                Ok(JsValue::undefined())
            }),
        )
        .unwrap();
    context.set_global("HostPoint", host_point).unwrap();
    assert_eq!(
        context.eval("(new HostPoint(3, 4)).x").unwrap().as_number(),
        Some(3.0)
    );
    assert_eq!(
        context
            .eval("new HostPoint(3, 4) instanceof HostPoint")
            .unwrap()
            .as_boolean(),
        Some(true)
    );

    // An accessor property: `value` is computed by a host getter/setter pair
    // over the object's own `_value` slot (the receiver arrives as `this`).
    let counter = context.create_object().unwrap();
    counter.set("_value", JsValue::number(0.0)).unwrap();
    context
        .define_accessor(
            &counter,
            "value",
            Some(Box::new(|call| {
                let object = call.this().as_object().ok_or_else(|| {
                    JsError::new(ErrorKind::TypeError, "value: no receiver".into())
                })?;
                object.get("_value")
            })),
            Some(Box::new(|call| {
                let object = call.this().as_object().ok_or_else(|| {
                    JsError::new(ErrorKind::TypeError, "value: no receiver".into())
                })?;
                object.set("_value", call.arg(0).unwrap_or_else(JsValue::undefined))?;
                Ok(JsValue::undefined())
            })),
        )
        .unwrap();
    context.set_global("counter", counter.as_value()).unwrap();
    assert_eq!(
        context
            .eval("counter.value = 5; counter.value")
            .unwrap()
            .as_number(),
        Some(5.0)
    );

    // With the `jit` feature: install the hook, then hot loops run compiled.
    #[cfg(feature = "jit")]
    {
        slag::install_jit(&mut context).unwrap();
        let sum = context
            .eval("(function (n) { var s = 0; for (var i = 0; i < n; i++) { s += i; } return s; })(10000)")
            .unwrap();
        assert_eq!(sum.as_number(), Some(49_995_000.0));
    }

    println!("embedding API: ok");
}
