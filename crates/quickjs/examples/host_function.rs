//! Add a host function and call it from JavaScript.

use quickjs::{Runtime, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut runtime = Runtime::new();
    let context = runtime.create_context()?;

    runtime.add_function(&context, "double", |args| {
        let number = args
            .get(0)
            .and_then(|value| value.as_number())
            .unwrap_or(0.0);
        args.set_return(Value::number(number * 2.0));
    })?;

    let result = context.eval("double(21)")?;
    println!("double(21) = {}", result.as_number().unwrap_or_default());
    Ok(())
}
