#![no_main]

use libfuzzer_sys::fuzz_target;

// Decoding and validation are the two stages a host runs over an untrusted
// module before it executes anything, and both are reachable from
// `WebAssembly.Module` with no other capability. A module that decodes must
// also survive validation: the validator trusts decoded lengths and indices.
fuzz_target!(|data: &[u8]| {
    if let Ok(module) = wasm::decode(data) {
        let _ = wasm::validate(&module);
    }
});
