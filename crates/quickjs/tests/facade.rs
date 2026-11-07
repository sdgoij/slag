//! L0 facade tests: eval, globals, objects, and error surfacing.

use quickjs::{
    Array, ArrayBuffer, BigInt, DataView, ElementType, Module, Promise, Runtime,
    String as JsString, Symbol, TypedArray, Value,
};

#[test]
fn eval_numbers_and_strings() {
    let mut runtime = Runtime::new();
    let context = runtime.create_context().unwrap();

    let value = context.eval("1 + 2").unwrap();
    assert!(value.is_number());
    assert_eq!(value.as_number(), Some(3.0));

    let text = context.eval("'hello ' + 'world'").unwrap();
    assert_eq!(text.as_string(), Some("hello world".to_string()));
}

#[test]
fn globals_round_trip() {
    let mut runtime = Runtime::new();
    let context = runtime.create_context().unwrap();

    context.set_global("answer", Value::number(42.0)).unwrap();
    assert_eq!(context.eval("answer").unwrap().as_number(), Some(42.0));
}

#[test]
fn objects_get_and_set() {
    let mut runtime = Runtime::new();
    let context = runtime.create_context().unwrap();

    let object = context.eval("({ x: 1 })").unwrap().as_object().unwrap();
    assert_eq!(object.get(&context, "x").unwrap().as_number(), Some(1.0));

    object.set(&context, "y", Value::string("hi")).unwrap();
    assert_eq!(
        object.get(&context, "y").unwrap().as_string(),
        Some("hi".to_string())
    );
    assert!(object.has(&context, "y").unwrap());
}

#[test]
fn eval_errors_surface_a_message() {
    let mut runtime = Runtime::new();
    let context = runtime.create_context().unwrap();

    let error = context.eval("not valid js @@@").unwrap_err();
    assert!(!error.message().is_empty());
}

#[test]
fn a_thrown_value_is_carried() {
    let mut runtime = Runtime::new();
    let context = runtime.create_context().unwrap();

    let error = context.eval("throw 'boom'").unwrap_err();
    assert_eq!(
        error.thrown().and_then(|value| value.as_string()),
        Some("boom".to_string())
    );
}

#[test]
fn host_functions_are_callable_from_js() {
    let mut runtime = Runtime::new();
    let context = runtime.create_context().unwrap();

    runtime
        .add_function(&context, "double", |args| {
            let number = args
                .get(0)
                .and_then(|value| value.as_number())
                .unwrap_or(0.0);
            args.set_return(Value::number(number * 2.0));
        })
        .unwrap();

    assert_eq!(context.eval("double(21)").unwrap().as_number(), Some(42.0));
}

#[test]
fn functions_call_from_rust() {
    let mut runtime = Runtime::new();
    let context = runtime.create_context().unwrap();

    let function = context
        .eval("(function add(a, b) { return a + b })")
        .unwrap()
        .as_function()
        .unwrap();

    let result = function
        .call(
            &context,
            Value::undefined(),
            &[Value::number(2.0), Value::number(3.0)],
        )
        .unwrap();
    assert_eq!(result.as_number(), Some(5.0));
}

#[test]
fn arrays_round_trip() {
    let mut runtime = Runtime::new();
    let context = runtime.create_context().unwrap();

    let array = Array::new(&context, &[Value::number(1.0), Value::number(2.0)]).unwrap();
    assert_eq!(array.length(&context).unwrap(), 2.0);
    assert_eq!(array.get(&context, 1).unwrap().as_number(), Some(2.0));

    array.set(&context, 1, Value::number(9.0)).unwrap();
    assert_eq!(array.get(&context, 1).unwrap().as_number(), Some(9.0));
}

#[test]
fn strings_round_trip() {
    let mut runtime = Runtime::new();
    let context = runtime.create_context().unwrap();

    let string = JsString::from_str(&context, "hello").unwrap();
    assert_eq!(string.len(), 5);
    assert!(!string.is_empty());
    assert_eq!(string.to_string().unwrap(), "hello");
}

#[test]
fn symbols_are_unique() {
    let mut runtime = Runtime::new();
    let context = runtime.create_context().unwrap();

    let a = Symbol::new(&context, Some("x")).unwrap();
    let b = Symbol::new(&context, Some("x")).unwrap();
    assert_eq!(a.description(), Some("x".to_string()));
    assert_eq!(a.to_string().unwrap(), "Symbol(x)");
    assert_ne!(a.as_value(), b.as_value());
}

#[test]
fn bigints_round_trip() {
    let mut runtime = Runtime::new();
    let context = runtime.create_context().unwrap();

    let negative = BigInt::from_i64(&context, -123).unwrap();
    assert_eq!(negative.to_i64(), Some(-123));
    assert_eq!(negative.to_string(10).unwrap(), "-123");

    let parsed = BigInt::from_str(&context, 16, "ff").unwrap();
    assert_eq!(parsed.to_u64(), Some(255));
    assert_eq!(parsed.to_string(10).unwrap(), "255");
}

#[test]
fn promise_helpers() {
    let mut runtime = Runtime::new();
    let context = runtime.create_context().unwrap();

    let promise = Promise::resolve(&context, Value::number(42.0)).unwrap();
    assert_eq!(promise.state(&context).unwrap(), "fulfilled");
    assert_eq!(promise.result(&context).unwrap().as_number(), Some(42.0));
}

#[test]
fn array_buffer_round_trip() {
    let mut runtime = Runtime::new();
    let context = runtime.create_context().unwrap();

    let buffer = ArrayBuffer::new_copy(&context, &[1, 2, 3, 4]).unwrap();
    assert_eq!(buffer.len(&context), 4);
    assert_eq!(buffer.to_vec(&context), Some(vec![1, 2, 3, 4]));

    buffer.copy_from_slice(&context, &[9, 9]).unwrap();
    assert_eq!(buffer.to_vec(&context), Some(vec![9, 9, 3, 4]));
}

#[test]
fn typed_array_and_data_view() {
    let mut runtime = Runtime::new();
    let context = runtime.create_context().unwrap();

    let buffer = ArrayBuffer::new(&context, 8).unwrap();
    let view = TypedArray::new_buffer(&context, ElementType::Uint8, &buffer, 0, 8).unwrap();
    assert_eq!(view.element_type(), Some(ElementType::Uint8));
    assert_eq!(view.length(), 8);
    assert_eq!(view.byte_length(), 8);

    let data_view = DataView::new(&context, &buffer, 0, 8).unwrap();
    assert!(data_view.as_value().is_object());
}

#[test]
fn modules_evaluate_and_expose_a_namespace() {
    let mut runtime = Runtime::new();
    let context = runtime.create_context().unwrap();

    let module = Module::new(&context, "m.js", "export const answer = 42;").unwrap();
    module.evaluate(&context).unwrap();
    let namespace = module.namespace(&context).unwrap();
    assert_eq!(
        namespace.get(&context, "answer").unwrap().as_number(),
        Some(42.0)
    );
}

#[test]
fn classes_construct_and_carry_methods() {
    let mut runtime = Runtime::new();
    let context = runtime.create_context().unwrap();

    let class = runtime.create_class("Point", |_args| {});
    let get = runtime
        .create_function(&context, |args| {
            args.set_return(Value::number(7.0));
        })
        .unwrap();
    class.method("get", &get);

    let constructor = class.constructor(&context).unwrap();
    context.set_global("Point", constructor.as_value()).unwrap();

    let instance = context.eval("new Point()").unwrap();
    assert!(instance.is_object());
    assert_eq!(
        context.eval("new Point().get()").unwrap().as_number(),
        Some(7.0)
    );
}
