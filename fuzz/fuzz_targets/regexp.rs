#![no_main]

use libfuzzer_sys::fuzz_target;

// The first four bytes are flags, filtered to the letters the parser knows; the
// rest is the pattern as UTF-16 code units. Both compile and execute, because
// the matcher is where the backtracking lives: H-2 was a hang, and the step
// budget that fixed it has to hold for every compiled form.
fuzz_target!(|data: &[u8]| {
    let (flag_bytes, pattern_bytes) = data.split_at(data.len().min(4));
    let flags: Vec<u16> = flag_bytes
        .iter()
        .filter(|byte| byte.is_ascii_alphabetic())
        .map(|byte| u16::from(*byte))
        .collect();
    let pattern: Vec<u16> = pattern_bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    if let Ok(re) = regexp::compile_with_flags(&pattern, &flags) {
        // `exec_at` swallows a budget exhaustion, so a pathological pattern
        // ends the attempt rather than the iteration.
        let _ = re.exec_at(&pattern, 0);
    }
});
