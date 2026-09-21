//! ASCII and UTF validation (`v8::simdutf`).
//!
//! The crate we stand in for wraps the simdutf library, which is a *speed*
//! decision: the answers are the ones a byte-by-byte check gives. Slag's
//! version is that check — the same answer, no SIMD — so a host that gates work
//! on it gets the gate it asked for.
//!
//! Only the predicate a host reaches for is here. A host that needs simdutf's
//! transcoders is told so at compile time by their absence rather than handed a
//! slower path it did not ask for.

/// Whether every byte is ASCII (`v8::simdutf::validate_ascii`).
pub fn validate_ascii(input: &[u8]) -> bool {
    input.iter().all(|byte| byte.is_ascii())
}

/// Whether every code unit is valid UTF-16LE
/// (`v8::simdutf::validate_utf16le`).
///
/// Unpaired surrogates are invalid, which is what the library's answer means
/// too — this is the check a host uses before handing bytes to a decoder.
pub fn validate_utf16le(input: &[u16]) -> bool {
    let mut index = 0;
    while index < input.len() {
        let unit = input[index];
        if (0xD800..0xDC00).contains(&unit) {
            let Some(&next) = input.get(index + 1) else {
                return false;
            };
            if !(0xDC00..0xE000).contains(&next) {
                return false;
            }
            index += 2;
        } else if (0xDC00..0xE000).contains(&unit) {
            return false;
        } else {
            index += 1;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    #[test]
    fn ascii_is_the_high_bit_scan() {
        assert!(super::validate_ascii(b""));
        assert!(super::validate_ascii(b"plain ascii\n"));
        assert!(!super::validate_ascii("café".as_bytes()));
        assert!(!super::validate_ascii(&[0x7F, 0x80]));
    }

    #[test]
    fn utf16_needs_paired_surrogates() {
        assert!(super::validate_utf16le(&[]));
        assert!(super::validate_utf16le(&[0x0061, 0xD83D, 0xDE00]));
        assert!(!super::validate_utf16le(&[0xD83D]));
        assert!(!super::validate_utf16le(&[0xDE00]));
        assert!(!super::validate_utf16le(&[0xD83D, 0x0061]));
    }
}
