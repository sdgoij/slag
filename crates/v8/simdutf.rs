//! Validation and base64 (`v8::simdutf`).
//!
//! The crate we stand in for wraps the simdutf library, which is a *speed*
//! decision: the answers are the ones a byte-by-byte check gives. Slag's
//! version is that check — the same answer, no SIMD — so a host that gates work
//! on it gets the gate it asked for.
//!
//! The base64 half is not a second implementation either. This engine already
//! decodes the spec's `FromBase64` (what `Uint8Array.fromBase64` is), whose
//! alphabet and last-chunk modes are the ones simdutf exposes, so
//! [`base64_to_binary`] answers out of that decoder. A host that needs simdutf's
//! transcoders is told so at compile time by their absence rather than handed a
//! slower path it did not ask for.

use runtime::builtins::typed_array;

/// The base64 alphabet a host selects (`v8::simdutf::Base64Options`).
///
/// The values are the library's own, written out rather than left to
/// declaration order, because a host passes them as integers to code that
/// expects simdutf's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum Base64Options {
    Default = 0,
    Url = 1,
}

/// How a decode treats a final chunk that is not a full four characters
/// (`v8::simdutf::LastChunkHandling`), with the library's own values.
///
/// `Loose` decodes a two- or three-character tail and ignores its unused bits;
/// `Strict` requires a complete padded final chunk whose unused bits are zero;
/// `StopBeforePartial` stops before an incomplete tail and reports the bytes
/// written so far as success. The engine's `FromBase64` draws the same three
/// distinctions — which is why one decoder serves both, and why the mapping in
/// [`base64_to_binary`] is a rename rather than a behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum LastChunkHandling {
    Loose = 0,
    Strict = 1,
    StopBeforePartial = 2,
}

/// The library's failure codes (`v8::simdutf::ErrorCode`, simdutf's
/// `error_code`).
///
/// [`base64_to_binary`] answers three of them — `Success`,
/// `InvalidBase64Character` and `OutputBufferTooSmall` — because the engine's
/// decoder returns one `SyntaxError` for every base64 syntax problem where the
/// library separates a bad character from an input remainder and from non-zero
/// extra bits. The rest are here because they are the library's own list, which
/// is what a host matching on them would find there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ErrorCode {
    Success = 0,
    HeaderBits,
    TooShort,
    TooLong,
    Overlong,
    TooLarge,
    Surrogate,
    InvalidBase64Character,
    Base64InputRemainder,
    Base64ExtraBits,
    OutputBufferTooSmall,
    Other,
}

/// A decode's answer (`v8::simdutf::Result`, simdutf's `result`): the code and
/// the number of bytes written to the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Result {
    pub error: ErrorCode,
    pub count: usize,
}

impl Result {
    /// Whether the input was valid base64 (`simdutf::result::is_ok`).
    pub fn is_ok(&self) -> bool {
        self.error == ErrorCode::Success
    }
}

/// The largest number of bytes `input` can decode to
/// (`v8::simdutf::maximal_binary_length_from_base64`).
///
/// The bound comes from the input's length, so whitespace and padding can only
/// make it looser than it has to be — the direction every call site needs, since
/// what it sizes is a buffer the decode writes into. simdutf's pointer overload
/// trims trailing `=` first and can be tighter.
pub fn maximal_binary_length_from_base64(input: &[u8]) -> usize {
    input.len().div_ceil(4) * 3
}

/// The number of characters `length` bytes encode to over `options`
/// (`v8::simdutf::base64_length_from_binary`).
///
/// Exact, not a bound, because a host sizes a buffer with it and checks the
/// encoder's write count against it. `Default` is padded; `Url` is the unpadded
/// base64url of RFC 4648 §5.
pub fn base64_length_from_binary(length: usize, options: Base64Options) -> usize {
    let whole_groups = length / 3 * 4;
    match options {
        Base64Options::Default => length.div_ceil(3) * 4,
        Base64Options::Url => match length % 3 {
            0 => whole_groups,
            1 => whole_groups + 2,
            _ => whole_groups + 3,
        },
    }
}

/// Encode `input` into `output` (`v8::simdutf::binary_to_base64`), answering the
/// number of characters written.
///
/// The alphabet and padding are the library's, and are the same two forms
/// [`base64_length_from_binary`] sizes a buffer for: `Default` is padded base64,
/// `Url` the unpadded base64url of RFC 4648 §5. A buffer that form's length
/// asked for is exactly filled.
///
/// # Safety
///
/// The signature is the crate we stand in for's, which is `unsafe` because its
/// C++ writes through the output pointer. This implementation writes nothing
/// outside `output` — it encodes into its own buffer and then copies what fits —
/// so the block a call site carries is sound.
pub unsafe fn binary_to_base64(input: &[u8], output: &mut [u8], options: Base64Options) -> usize {
    let alphabet = typed_array::base64_alphabet(matches!(options, Base64Options::Url));
    let omit_padding = matches!(options, Base64Options::Url);
    let mut encoded = Vec::with_capacity(base64_length_from_binary(input.len(), options));
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        encoded.push(alphabet[(b0 >> 2) as usize]);
        encoded.push(alphabet[(((b0 & 0x3) << 4) | (b1 >> 4)) as usize]);
        if chunk.len() > 1 {
            encoded.push(alphabet[(((b1 & 0xF) << 2) | (b2 >> 6)) as usize]);
        } else if !omit_padding {
            encoded.push(b'=');
        }
        if chunk.len() > 2 {
            encoded.push(alphabet[(b2 & 0x3F) as usize]);
        } else if !omit_padding {
            encoded.push(b'=');
        }
    }
    let written = encoded.len().min(output.len());
    output[..written].copy_from_slice(&encoded[..written]);
    written
}

/// Decode `input` into `output` (`v8::simdutf::base64_to_binary`), answering
/// the code and the number of bytes written.
///
/// # Safety
///
/// The signature is the crate we stand in for's, which is `unsafe` because its
/// C++ writes through the output pointer before it can report an error. This
/// implementation writes nothing outside `output` — it decodes into its own
/// buffer and then copies what fits — so the block a call site carries is sound.
pub unsafe fn base64_to_binary(
    input: &[u8],
    output: &mut [u8],
    options: Base64Options,
    last_chunk: LastChunkHandling,
) -> Result {
    // The engine's decoder reads code units, so the bytes are widened one per
    // unit: on ASCII that is the identity, and a byte above `0x7F` lands in the
    // engine's "not in this alphabet" case rather than being mangled into one
    // that is.
    let units: Vec<u16> = input.iter().copied().map(u16::from).collect();
    let alphabet = typed_array::base64_alphabet(matches!(options, Base64Options::Url));
    let decoded = typed_array::decode_base64(
        &units,
        alphabet,
        match last_chunk {
            LastChunkHandling::Loose => typed_array::LastChunkHandling::Loose,
            LastChunkHandling::Strict => typed_array::LastChunkHandling::Strict,
            LastChunkHandling::StopBeforePartial => {
                typed_array::LastChunkHandling::StopBeforePartial
            }
        },
        // No length bound: the caller's slice decides, below, whether the whole
        // result fits — and an invalid character past where a bounded decode
        // would have stopped is still reported, which is what the library's
        // reported-what-was-written contract asks for.
        usize::MAX,
    );
    let written = decoded.bytes.len().min(output.len());
    output[..written].copy_from_slice(&decoded.bytes[..written]);
    if decoded.error.is_some() {
        return Result {
            error: ErrorCode::InvalidBase64Character,
            count: written,
        };
    }
    if decoded.bytes.len() > output.len() {
        return Result {
            error: ErrorCode::OutputBufferTooSmall,
            count: written,
        };
    }
    Result {
        error: ErrorCode::Success,
        count: written,
    }
}

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

/// The library's `base64_options` as a host passes it across the C ABI, which is
/// the enum's own `u64`.
fn options_from_u64(value: u64) -> Base64Options {
    match value {
        1 => Base64Options::Url,
        _ => Base64Options::Default,
    }
}

/// The library's `last_chunk_handling_options` as a host passes it across the C
/// ABI, which is the enum's own `u64`.
fn last_chunk_from_u64(value: u64) -> LastChunkHandling {
    match value {
        1 => LastChunkHandling::Strict,
        2 => LastChunkHandling::StopBeforePartial,
        _ => LastChunkHandling::Loose,
    }
}

/// simdutf's `result` in the C ABI shape a host re-declares (`deno`'s
/// `SimdutfFfiResult`): the code and the bytes written.
#[repr(C)]
pub struct SimdutfFfiResult {
    pub error: i32,
    pub count: usize,
}

/// The C symbol `simdutf__binary_to_base64`, which `deno` links against by name
/// (`deno/ext/web/lib.rs:302`).
///
/// The crate we stand in for gets this from its compiled V8/simdutf; here it is
/// the engine's own encoder behind an `extern "C"` boundary, so a host that
/// re-declares the symbol — as deno does, to pass output memory it has not
/// initialized, which a `&mut [u8]` wrapper cannot soundly expose — resolves it.
///
/// # Safety
///
/// `input` must point to `length` readable bytes and `output` to
/// [`base64_length_from_binary`]`(length, options)` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn simdutf__binary_to_base64(
    input: *const u8,
    length: usize,
    output: *mut u8,
    options: u64,
) -> usize {
    // SAFETY: the caller's contract, above.
    let input = unsafe { std::slice::from_raw_parts(input, length) };
    let options = options_from_u64(options);
    let bound = base64_length_from_binary(length, options);
    // SAFETY: the caller's contract: `bound` writable bytes.
    let output = unsafe { std::slice::from_raw_parts_mut(output, bound) };
    // SAFETY: `output` is the size the encoder's own bound promised.
    unsafe { binary_to_base64(input, output, options) }
}

/// The C symbol `simdutf__base64_to_binary`, which `deno` links against by name
/// (`deno/ext/web/lib.rs:310`).
///
/// # Safety
///
/// `input` must point to `length` readable bytes and `output` to
/// [`maximal_binary_length_from_base64`]`(input)` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn simdutf__base64_to_binary(
    input: *const u8,
    length: usize,
    output: *mut u8,
    options: u64,
    last_chunk_options: u64,
) -> SimdutfFfiResult {
    // SAFETY: the caller's contract, above.
    let input = unsafe { std::slice::from_raw_parts(input, length) };
    let bound = maximal_binary_length_from_base64(input);
    // SAFETY: the caller's contract: `bound` writable bytes.
    let output = unsafe { std::slice::from_raw_parts_mut(output, bound) };
    // SAFETY: `output` is the size the decoder's own bound promised.
    let result = unsafe {
        base64_to_binary(
            input,
            output,
            options_from_u64(options),
            last_chunk_from_u64(last_chunk_options),
        )
    };
    SimdutfFfiResult {
        error: result.error as i32,
        count: result.count,
    }
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

    use super::{Base64Options, ErrorCode, LastChunkHandling, base64_to_binary};

    /// Decode `input` into a buffer sized the way a host sizes one.
    fn decode(
        input: &[u8],
        options: Base64Options,
        last_chunk: LastChunkHandling,
    ) -> (super::Result, Vec<u8>) {
        let mut output = vec![0u8; super::maximal_binary_length_from_base64(input)];
        // SAFETY: the buffer is the size the bound promises a decode fits in.
        let result = unsafe { base64_to_binary(input, &mut output, options, last_chunk) };
        (result, output)
    }

    /// The encoded length is exact rather than a bound: the standard alphabet
    /// pads and the URL alphabet is the unpadded base64url of RFC 4648 §5.
    #[test]
    fn the_encoded_length_follows_the_alphabet() {
        for (length, padded, url) in [
            (0, 0, 0),
            (1, 4, 2),
            (2, 4, 3),
            (3, 4, 4),
            (4, 8, 6),
            (5, 8, 7),
            (6, 8, 8),
        ] {
            assert_eq!(
                super::base64_length_from_binary(length, Base64Options::Default),
                padded,
                "{length} bytes, standard"
            );
            assert_eq!(
                super::base64_length_from_binary(length, Base64Options::Url),
                url,
                "{length} bytes, url"
            );
        }
    }

    /// The maximal length is a bound every decode fits in — which is what the
    /// call sites use it for, a capacity and a memory-safety assert.
    #[test]
    fn the_maximal_length_is_a_bound() {
        assert_eq!(super::maximal_binary_length_from_base64(b""), 0);
        for input in [
            b"Zg==".as_slice(),
            b"Zm8=",
            b"Zm9vYg",
            b"Zm9vYmFy",
            b"Zm9v\nYmFy",
            b"Zm9vYmFy\n",
        ] {
            let bound = super::maximal_binary_length_from_base64(input);
            let (result, _) = decode(input, Base64Options::Default, LastChunkHandling::Loose);
            assert!(result.is_ok(), "{input:?}");
            assert!(
                result.count <= bound,
                "{input:?}: {} > {bound}",
                result.count
            );
            assert_eq!(
                result.count,
                match input {
                    b"Zg==" => 1,
                    b"Zm8=" => 2,
                    b"Zm9vYg" => 4,
                    _ => 6,
                }
            );
        }
    }

    /// A decode is the engine's `FromBase64`, which is what the library's is:
    /// both alphabets, whitespace skipped, padding accepted, and the final
    /// chunk's unused bits ignored under `Loose` but refused under `Strict`.
    #[test]
    fn a_decode_is_the_engines_from_base64() {
        let standard = |input: &[u8], last_chunk| decode(input, Base64Options::Default, last_chunk);

        let (result, output) = standard(b"Zm9vYmFy", LastChunkHandling::Strict);
        assert!(result.is_ok());
        assert_eq!(&output[..result.count], b"foobar");

        // Padding decides how much of the last chunk is real.
        let (result, output) = standard(b"Zg==", LastChunkHandling::Strict);
        assert!(result.is_ok());
        assert_eq!(&output[..result.count], b"f");
        let (result, output) = standard(b"Zm8=", LastChunkHandling::Strict);
        assert!(result.is_ok());
        assert_eq!(&output[..result.count], b"fo");

        // ASCII whitespace is skipped, wherever it falls.
        let (result, output) = standard(b"Zm9v\nYmFy", LastChunkHandling::Strict);
        assert!(result.is_ok());
        assert_eq!(&output[..result.count], b"foobar");

        // Strict requires a complete final chunk; loose decodes a short tail.
        assert!(!standard(b"Zm9vYg", LastChunkHandling::Strict).0.is_ok());
        let (result, output) = standard(b"Zm9vYg", LastChunkHandling::Loose);
        assert!(result.is_ok());
        assert_eq!(&output[..result.count], b"foob");

        // `stop-before-partial` takes the full chunks and leaves the tail.
        let (result, output) = standard(b"Zm9vYg", LastChunkHandling::StopBeforePartial);
        assert!(result.is_ok());
        assert_eq!(&output[..result.count], b"foo");

        // Unused bits: `YR==` has a non-zero low nibble, so strict refuses it
        // and loose reads the byte anyway.
        assert!(!standard(b"YR==", LastChunkHandling::Strict).0.is_ok());
        let (result, output) = standard(b"YR==", LastChunkHandling::Loose);
        assert!(result.is_ok());
        assert_eq!(&output[..result.count], b"a");

        // The URL alphabet, and each alphabet refusing the other's characters.
        let (result, output) = decode(b"-_8", Base64Options::Url, LastChunkHandling::Loose);
        assert!(result.is_ok());
        assert_eq!(&output[..result.count], &[0xFB, 0xFF]);
        assert!(
            !decode(b"-_8", Base64Options::Default, LastChunkHandling::Loose)
                .0
                .is_ok()
        );
        assert!(
            !decode(b"+/8=", Base64Options::Url, LastChunkHandling::Loose)
                .0
                .is_ok()
        );

        // A character outside the alphabet is the library's invalid-character
        // code, whatever the syntax problem was.
        let (result, _) = standard(b"Zm9v*", LastChunkHandling::Loose);
        assert!(!result.is_ok());
        assert_eq!(result.error, ErrorCode::InvalidBase64Character);
        assert!(!standard(b"Zm9vYmFy\n!", LastChunkHandling::Loose).0.is_ok());
    }

    /// A slice too small for the result is the library's own code rather than a
    /// panic, and the bytes that fit are the ones written.
    #[test]
    fn a_small_output_is_reported() {
        let mut output = [0u8; 2];
        // SAFETY: `base64_to_binary` writes only within the slice it is given.
        let result = unsafe {
            base64_to_binary(
                b"Zm9vYmFy",
                &mut output,
                Base64Options::Default,
                LastChunkHandling::Loose,
            )
        };
        assert_eq!(result.error, ErrorCode::OutputBufferTooSmall);
        assert_eq!(result.count, 2);
        assert_eq!(&output, b"fo");
    }

    /// The encoder is the engine's own, read back through JavaScript: for every
    /// length in a small corpus the library's `binary_to_base64` must produce
    /// exactly what `Uint8Array.prototype.toBase64` does with the matching
    /// alphabet and padding. `Default` is padded base64 and `Url` is the unpadded
    /// base64url, which is the mapping `base64_length_from_binary` sizes a buffer
    /// for, so a wrong alphabet or a wrong padding decision is a different string.
    #[test]
    fn the_encoder_is_the_engines_own_to_base64() {
        crate::test_support::in_context!(scope, {
            for length in 0..=8usize {
                let bytes: Vec<u8> = (0..length)
                    .map(|index| (index as u8).wrapping_mul(37).wrapping_add(11))
                    .collect();
                let list = bytes
                    .iter()
                    .map(|byte| byte.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                for (options, alphabet, omit_padding) in [
                    (Base64Options::Default, "base64", false),
                    (Base64Options::Url, "base64url", true),
                ] {
                    let source = format!(
                        "new Uint8Array([{list}]).toBase64({{alphabet: '{alphabet}', omitPadding: {omit_padding}}})"
                    );
                    let expected =
                        crate::test_support::eval(scope, &source).to_rust_string_lossy(scope);
                    let mut output =
                        vec![0u8; super::base64_length_from_binary(bytes.len(), options)];
                    // SAFETY: `output` is the size the encoder's own bound promises.
                    let written = unsafe { super::binary_to_base64(&bytes, &mut output, options) };
                    output.truncate(written);
                    assert_eq!(
                        String::from_utf8(output).expect("base64 is ascii"),
                        expected,
                        "{length} bytes, {alphabet}"
                    );
                }
            }
        });
    }

    /// The symbols deno links against, called through deno's own declaration: the
    /// `#[link_name]`, the argument order and the `#[repr(C)]` result are pinned,
    /// because a mismatch is a link error or a wrong answer rather than a compile
    /// error. `deno/ext/web/lib.rs:301-318` is the declaration this mirrors.
    #[test]
    fn the_simdutf_c_symbols_are_the_ones_deno_declares() {
        #[repr(C)]
        struct FfiResult {
            error: i32,
            count: usize,
        }

        unsafe extern "C" {
            #[link_name = "simdutf__binary_to_base64"]
            fn ffi_binary_to_base64(
                input: *const u8,
                length: usize,
                output: *mut u8,
                options: u64,
            ) -> usize;

            #[link_name = "simdutf__base64_to_binary"]
            fn ffi_base64_to_binary(
                input: *const u8,
                length: usize,
                output: *mut u8,
                options: u64,
                last_chunk_options: u64,
            ) -> FfiResult;
        }

        for (bytes, expected) in [
            (b"".as_slice(), ""),
            (b"f", "Zg=="),
            (b"hi", "aGk="),
            (b"abc", "YWJj"),
            (b"foobar", "Zm9vYmFy"),
        ] {
            let mut encoded =
                vec![0u8; super::base64_length_from_binary(bytes.len(), Base64Options::Default)];
            // SAFETY: `encoded` is the size the encoder's bound promises.
            let written = unsafe {
                ffi_binary_to_base64(
                    bytes.as_ptr(),
                    bytes.len(),
                    encoded.as_mut_ptr(),
                    Base64Options::Default as u64,
                )
            };
            assert_eq!(
                std::str::from_utf8(&encoded[..written]).expect("ascii"),
                expected
            );

            let mut decoded =
                vec![0u8; super::maximal_binary_length_from_base64(&encoded[..written])];
            // SAFETY: `decoded` is the size the decoder's bound promises.
            let result = unsafe {
                ffi_base64_to_binary(
                    encoded.as_ptr(),
                    written,
                    decoded.as_mut_ptr(),
                    Base64Options::Default as u64,
                    LastChunkHandling::Strict as u64,
                )
            };
            assert_eq!(result.error, 0, "success code for {expected:?}");
            assert_eq!(result.count, bytes.len());
            assert_eq!(&decoded[..result.count], bytes);
        }

        // The URL alphabet is unpadded and uses its own symbols, so the option
        // word decides the answer — it is the library's integer, not a bool.
        let bytes = [0xFBu8, 0xFF];
        let mut encoded =
            vec![0u8; super::base64_length_from_binary(bytes.len(), Base64Options::Url)];
        // SAFETY: `encoded` is the size the encoder's bound promises.
        let written = unsafe {
            ffi_binary_to_base64(
                bytes.as_ptr(),
                bytes.len(),
                encoded.as_mut_ptr(),
                Base64Options::Url as u64,
            )
        };
        assert_eq!(
            std::str::from_utf8(&encoded[..written]).expect("ascii"),
            "-_8"
        );

        let mut decoded = vec![0u8; super::maximal_binary_length_from_base64(&encoded[..written])];
        // SAFETY: `decoded` is the size the decoder's bound promises.
        let result = unsafe {
            ffi_base64_to_binary(
                encoded.as_ptr(),
                written,
                decoded.as_mut_ptr(),
                Base64Options::Url as u64,
                LastChunkHandling::Loose as u64,
            )
        };
        assert_eq!(result.error, 0);
        assert_eq!(result.count, 2);
        assert_eq!(&decoded[..result.count], &bytes);
    }
}
