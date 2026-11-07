//! Build an ArrayBuffer and a Uint8 view over it, then read the bytes back.

use quickjs::{ArrayBuffer, ElementType, Runtime, TypedArray};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut runtime = Runtime::new();
    let context = runtime.create_context()?;

    let buffer = ArrayBuffer::new_copy(&context, &[1, 2, 3, 4])?;
    let view = TypedArray::new_buffer(&context, ElementType::Uint8, &buffer, 0, 4)?;

    println!("length = {}", view.length());
    println!("bytes = {:?}", view.to_vec(&context));
    Ok(())
}
