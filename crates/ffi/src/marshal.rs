//! Value and string marshaling between crux and the C surfaces.

use crux::string::JsString;

/// A JS string from UTF-8 (lossless: ECMAScript strings are UTF-16 code
/// units, and valid UTF-8 maps onto them exactly).
pub fn string_from_utf8(text: &str) -> JsString {
    JsString::from_utf8(text)
}

/// A JS string from UTF-16 code units (lossless; lone surrogates are kept).
pub fn string_from_utf16(units: &[u16]) -> JsString {
    JsString::from_utf16(units)
}

/// The string's UTF-16 code units (the ECMAScript string storage).
pub fn string_units(string: &JsString) -> &[u16] {
    string.as_slice()
}

/// Lossy UTF-8 rendering for diagnostics (lone surrogates become U+FFFD).
pub fn string_lossy(string: &JsString) -> String {
    string.to_string_lossy()
}

/// The number of UTF-16 code units (spec 6.1.4.1 StringLength).
pub fn string_length(string: &JsString) -> usize {
    string.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_round_trip_is_exact() {
        let string = string_from_utf8("héllo 💡");
        assert_eq!(string_lossy(&string), "héllo 💡");
        assert_eq!(string_length(&string), 8); // 5 + 1 + 2 (astral pair)
    }

    #[test]
    fn utf16_round_trip_keeps_lone_surrogates() {
        let string = string_from_utf16(&[0xD800, 0x0041]);
        assert_eq!(string_units(&string), &[0xD800, 0x0041]);
    }
}
