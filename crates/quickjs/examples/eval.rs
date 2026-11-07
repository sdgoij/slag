//! Evaluate a script and print the result.

use quickjs::Runtime;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut runtime = Runtime::new();
    let context = runtime.create_context()?;

    let value = context.eval("'hello ' + 'world'")?;
    println!("{}", value.as_string().unwrap_or_default());
    Ok(())
}
