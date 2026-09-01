//! The Rust embedding API, end to end: create a context, route host
//! console output, evaluate, call, construct, expose globals, and (with the
//! `jit` feature) install the JIT hook.
//!
//! Run: `cargo run -p slag --example embed`
//!      `cargo run -p slag --example embed --features slag/jit`

use slag::{Context, HostCallbacks, JsValue};

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
