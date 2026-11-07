//! Compile a module and read its exported namespace.

use quickjs::{Module, Runtime};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut runtime = Runtime::new();
    let context = runtime.create_context()?;

    let module = Module::new(&context, "answer.js", "export const answer = 42;")?;
    module.evaluate(&context)?;
    let namespace = module.namespace(&context)?;

    let answer = namespace.get(&context, "answer")?;
    println!("answer = {}", answer.as_number().unwrap_or_default());
    Ok(())
}
