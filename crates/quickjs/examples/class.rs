//! Define a host class, install it as a global, and construct an instance.

use quickjs::{Runtime, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut runtime = Runtime::new();
    let context = runtime.create_context()?;

    let class = runtime.create_class("Greeter", |_args| {});
    let greet = runtime.create_function(&context, |args| {
        let name = args
            .get(0)
            .and_then(|value| value.as_string())
            .unwrap_or_else(|| "world".to_string());
        args.set_return(Value::string(format!("hello, {name}")));
    })?;
    class.method("greet", &greet);

    let constructor = class.constructor(&context)?;
    context.set_global("Greeter", constructor.as_value())?;

    let result = context.eval("new Greeter().greet('zig')")?;
    println!("{}", result.as_string().unwrap_or_default());
    Ok(())
}
