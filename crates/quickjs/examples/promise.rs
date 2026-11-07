//! Resolve a promise and read its settled state.

use quickjs::{Promise, Runtime, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut runtime = Runtime::new();
    let context = runtime.create_context()?;

    let promise = Promise::resolve(&context, Value::number(42.0))?;
    println!("state = {}", promise.state(&context)?);
    println!(
        "result = {}",
        promise.result(&context)?.as_number().unwrap_or_default()
    );
    Ok(())
}
