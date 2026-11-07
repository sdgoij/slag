//! Exercise the Rust facade under the engine's per-allocation `--gc-stress`
//! mode, which collects at every safe point with the write-barrier audit on.

use quickjs::{Runtime, Value};

#[test]
fn eval_allocates_and_survives_gc_stress() {
    let mut runtime = Runtime::new();
    runtime.set_gc_stress(true);
    let context = runtime.create_context().unwrap();

    for i in 0..100 {
        let value = context.eval(&format!("({i} * 2) + 0.5")).unwrap();
        assert_eq!(value.as_number(), Some((i as f64) * 2.0 + 0.5));
    }
}

#[test]
fn a_host_value_held_across_gc_stress_survives() {
    let mut runtime = Runtime::new();
    runtime.set_gc_stress(true);
    let context = runtime.create_context().unwrap();

    let object = context.eval("({ x: 42 })").unwrap().as_object().unwrap();
    for _ in 0..50 {
        let _ = context.eval("'churn' + 'allocations'").unwrap();
    }
    assert_eq!(object.get(&context, "x").unwrap().as_number(), Some(42.0));
}

#[test]
fn arrays_and_functions_survive_gc_stress() {
    let mut runtime = Runtime::new();
    runtime.set_gc_stress(true);
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

    let array = context
        .eval("[double(1), double(2), double(3)]")
        .unwrap()
        .as_object()
        .unwrap();
    assert_eq!(array.get(&context, "2").unwrap().as_number(), Some(6.0));
}
