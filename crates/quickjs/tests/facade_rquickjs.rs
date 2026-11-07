//! A representative slice of upstream `rquickjs` tests, ported to this facade's
//! shape (explicit `&Context`, no `'js` lifetime).

use quickjs::{Runtime, Value};

#[test]
fn eval_produces_each_primitive() {
    let mut runtime = Runtime::new();
    let context = runtime.create_context().unwrap();

    assert!(context.eval("null").unwrap().is_null());
    assert!(context.eval("undefined").unwrap().is_undefined());
    assert!(context.eval("true").unwrap().is_boolean());
    assert!(context.eval("1").unwrap().is_number());
    assert!(context.eval("'x'").unwrap().is_string());
    assert!(context.eval("Symbol()").unwrap().is_symbol());
    assert!(context.eval("1n").unwrap().is_bigint());
    assert!(context.eval("({})").unwrap().is_object());
    assert!(context.eval("(function f(){})").unwrap().is_function());
}

#[test]
fn host_callback_receives_this_and_args() {
    let mut runtime = Runtime::new();
    let context = runtime.create_context().unwrap();

    runtime
        .add_function(&context, "describe", |args| {
            let this = args.this().as_number().unwrap_or(-1.0);
            args.set_return(Value::string(format!("this={this} len={}", args.len())));
        })
        .unwrap();

    let result = context.eval("describe.call(5, 'a', 'b')").unwrap();
    assert_eq!(result.as_string(), Some("this=5 len=2".to_string()));
}

#[test]
fn a_thrown_object_is_carried_as_a_value() {
    let mut runtime = Runtime::new();
    let context = runtime.create_context().unwrap();

    let error = context.eval("throw { code: 42 }").unwrap_err();
    let thrown = error.thrown().unwrap();
    assert!(thrown.is_object());
    assert_eq!(
        thrown
            .as_object()
            .unwrap()
            .get(&context, "code")
            .unwrap()
            .as_number(),
        Some(42.0)
    );
}
