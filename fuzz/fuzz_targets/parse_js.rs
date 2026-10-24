#![no_main]

use libfuzzer_sys::fuzz_target;

// The three front ends a host can run untrusted text through. A panic here is
// a defect in the same class as H-1: the parser must report an error, not
// abort the process.
fuzz_target!(|data: &[u8]| {
    // JavaScript source is Unicode text; a chunk that is not UTF-8 is not a
    // program, and filtering here keeps the corpus on inputs the lexer
    // actually sees.
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    let _ = parser::parse_script(source);
    let _ = parser::parse_module(source);
    let _ = parser::parse_script_jsx(source);
});
