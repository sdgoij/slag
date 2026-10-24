#![no_main]

use libfuzzer_sys::fuzz_target;

// The UTF-16 entry point sees code-unit sequences the UTF-8 lexer cannot: a
// lone surrogate or a surrogate pair that decodes to an identifier character
// reaches a different path, and the runtime's own string type is UTF-16.
fuzz_target!(|data: &[u8]| {
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let _ = parser::parse_script_utf16(&units);
});
