//! Primitive values and their constructors (`v8::Primitive`, `v8::String`,
//! `v8::Number`, `v8::Boolean`).

use std::mem::MaybeUninit;
use std::ops::{BitOr, BitOrAssign, Deref};

use crux::string::JsString;
use crux::value::ValueKind;
use runtime::api;

use crate::data::{Boolean, Int32, Integer, Number, Primitive, String, Symbol, Uint32};
use crate::handle::Local;
use crate::scope::PinScope;

/// `v8::null`.
pub fn null<'s, R>(_scope: &R) -> Local<'s, Primitive> {
    Local::from_engine(api::Local::null())
}

/// `v8::undefined`.
pub fn undefined<'s, R>(_scope: &R) -> Local<'s, Primitive> {
    Local::from_engine(api::Local::undefined())
}

/// How a new string's internal representation is chosen
/// (`v8::NewStringType`). Slag has one representation, so this only has to
/// exist for callers to name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewStringType {
    Normal,
    Internalized,
}

/// Flags for the `String` write methods (`v8::String::WriteFlags`).
///
/// A hand-rolled bit set rather than a `bitflags!` one, so that the bridge
/// carries no dependency the engine's own graph does not already have.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct WriteFlags(u32);

#[allow(non_upper_case_globals)]
impl WriteFlags {
    pub const kNullTerminate: Self = Self(1);
    pub const kReplaceInvalidUtf8: Self = Self(2);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn is_empty(&self) -> bool {
        self.0 == 0
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(&self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for WriteFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for WriteFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl Boolean {
    pub fn new<'s, R>(_scope: &R, value: bool) -> Local<'s, Boolean> {
        Local::from_engine(api::Local::boolean(value))
    }
}

impl<'s> Local<'s, Boolean> {
    pub fn value(&self) -> bool {
        self.engine().as_boolean().unwrap_or(false)
    }
}

impl Number {
    pub fn new<'s, R>(_scope: &R, value: f64) -> Local<'s, Number> {
        Local::from_engine(api::Local::number(value))
    }
}

impl<'s> Local<'s, Number> {
    /// The value as a double (`v8::Number::Value`).
    pub fn value(&self) -> f64 {
        self.engine().as_number().unwrap_or(f64::NAN)
    }
}

impl Integer {
    /// An `i32` as an integer (`v8::Integer::New`).
    pub fn new<'s, R>(_scope: &R, value: i32) -> Local<'s, Integer> {
        Local::from_engine(api::Local::number(f64::from(value)))
    }

    /// A `u32` as an integer (`v8::Integer::NewFromUnsigned`).
    pub fn new_from_unsigned<'s, R>(_scope: &R, value: u32) -> Local<'s, Integer> {
        Local::from_engine(api::Local::number(f64::from(value)))
    }
}

impl<'s> Local<'s, Integer> {
    /// The value (`v8::Integer::Value`).
    pub fn value(&self) -> i64 {
        self.engine().as_number().map_or(0, |n| n as i64)
    }
}

impl<'s> Local<'s, Uint32> {
    /// The value as an unsigned 32-bit integer (`v8::Uint32::Value`).
    ///
    /// The width is the whole of this accessor: the engine has one number kind,
    /// so `Integer::value` (`i64`) and this differ only in the conversion they
    /// promise — which is what a host keys a property descriptor's arithmetic on.
    pub fn value(&self) -> u32 {
        self.engine().as_number().map_or(0, |n| n as u32)
    }
}

impl<'s> Local<'s, Int32> {
    /// The value as a signed 32-bit integer (`v8::Int32::Value`), for the same
    /// reason as [`Uint32::value`](Local::value).
    pub fn value(&self) -> i32 {
        self.engine().as_number().map_or(0, |n| n as i32)
    }
}

impl String {
    /// A new string from UTF-8 (`v8::String::NewFromUtf8`).
    pub fn new<'s>(scope: &PinScope<'s, '_, ()>, value: &str) -> Option<Local<'s, String>> {
        Self::new_from_utf8(scope, value.as_bytes(), NewStringType::Normal)
    }

    /// A static one-byte string resource, checked at compile time
    /// (v8::String::CreateExternalOneByteConst).
    ///
    /// The checks are the crate we stand in for's: ASCII only, and within the
    /// length a `String` can hold.
    pub const fn create_external_onebyte_const(buffer: &'static [u8]) -> OneByteConst {
        assert!(buffer.is_ascii() && buffer.len() <= (1 << 29) - 24);
        OneByteConst {
            data: buffer.as_ptr(),
            length: buffer.len(),
        }
    }

    /// As [`create_external_onebyte_const`](Self::create_external_onebyte_const),
    /// without the checks.
    ///
    /// # Safety
    ///
    /// The buffer must be ASCII and within the length a `String` can hold.
    pub const unsafe fn create_external_onebyte_const_unchecked(
        buffer: &'static [u8],
    ) -> OneByteConst {
        OneByteConst {
            data: buffer.as_ptr(),
            length: buffer.len(),
        }
    }

    /// A string over a static one-byte buffer
    /// (`v8::String::NewExternalOneByteStatic`).
    ///
    /// The buffer is read as Latin-1, one code unit per byte, as it is there.
    pub fn new_external_onebyte_static<'s>(
        _scope: &PinScope<'s, '_, ()>,
        buffer: &'static [u8],
    ) -> Option<Local<'s, String>> {
        let units: Vec<u16> = buffer.iter().copied().map(u16::from).collect();
        Some(Self::from_code_units(&units))
    }

    /// The empty string (`v8::String::Empty`).
    ///
    /// Infallible, as there: the engine always has one, so there is nothing for
    /// a host to handle.
    pub fn empty<'s>(_scope: &PinScope<'s, '_, ()>) -> Local<'s, String> {
        Self::from_code_units(&[])
    }

    /// A string over a one-byte const (`v8::String::NewExternalOneByteConst`).
    ///
    /// The crate's version makes a string that *references* the const's bytes
    /// and caches the handle inside it, which is why its `OneByteConst` carries
    /// a cache slot; here the bytes are read into the engine's own string and
    /// the const stays a compile-time buffer. Same text, one copy, nothing kept
    /// alive on the const's behalf.
    pub fn new_from_onebyte_const<'s>(
        scope: &PinScope<'s, '_, ()>,
        onebyte_const: &'static OneByteConst,
    ) -> Option<Local<'s, String>> {
        Self::new_external_onebyte_static(scope, onebyte_const.as_ref())
    }

    /// A new string from UTF-8 bytes (`v8::String::NewFromUtf8`).
    pub fn new_from_utf8<'s>(
        _scope: &PinScope<'s, '_, ()>,
        bytes: &[u8],
        _ty: NewStringType,
    ) -> Option<Local<'s, String>> {
        Some(Local::from_engine(api::Local::string(
            std::string::String::from_utf8_lossy(bytes).into_owned(),
        )))
    }

    /// A new string from UTF-16 code units (`v8::String::NewFromTwoByte`).
    ///
    /// Built straight from the units, not through a UTF-8 round trip: this is
    /// the constructor a caller reaches for when the units are the point, and
    /// one that substituted U+FFFD for a lone surrogate would put the loss back
    /// in at the front door.
    pub fn new_from_two_byte<'s>(
        _scope: &PinScope<'s, '_, ()>,
        units: &[u16],
        _ty: NewStringType,
    ) -> Option<Local<'s, String>> {
        Some(Self::from_code_units(units))
    }

    /// A new string from one-byte (Latin-1) characters
    /// (`v8::String::NewFromOneByte`): each byte is one code unit, so `0xE9` is
    /// `é` and not the first byte of a UTF-8 sequence.
    pub fn new_from_one_byte<'s>(
        _scope: &PinScope<'s, '_, ()>,
        bytes: &[u8],
        _ty: NewStringType,
    ) -> Option<Local<'s, String>> {
        let units: Vec<u16> = bytes.iter().copied().map(u16::from).collect();
        Some(Self::from_code_units(&units))
    }

    fn from_code_units<'s>(units: &[u16]) -> Local<'s, String> {
        Local::from_engine(api::Local::from(crux::value::Value::String(
            crux::handle::Handle::new(JsString::from_utf16(units)),
        )))
    }
}

impl<'s> Local<'s, String> {
    /// The raw UTF-16 code units behind the string.
    ///
    /// Read from the engine's `JsString`, which stores code units exactly — a
    /// lone surrogate is a code unit like any other. Going through
    /// `api::Local::as_string` would instead flatten the string through UTF-8
    /// and substitute U+FFFD, discarding exactly the information the string
    /// API below is asked to report, and making every encoding decision the
    /// caller's to make on their own.
    fn code_units(&self) -> Vec<u16> {
        match self.engine().value().kind() {
            ValueKind::String(handle) => handle.as_slice().to_vec(),
            _ => Vec::new(),
        }
    }

    /// The string's UTF-16 code units (`v8::String::Write`'s source).
    pub fn to_utf16(&self) -> Vec<u16> {
        self.code_units()
    }

    /// The string's UTF-8 rendering (`v8::String::ToRustString`, lossy form).
    ///
    /// Lossy is what this one promises, so lone surrogates become U+FFFD here
    /// and only here. The write methods that can choose are a different
    /// matter and must not be built on this.
    pub fn to_rust_string_lossy(&self, _scope: &crate::Isolate) -> std::string::String {
        std::string::String::from_utf16_lossy(&self.code_units())
    }

    /// The string's UTF-8 bytes, lossily (`v8::String::WriteUtf8` without
    /// flags).
    pub fn to_utf8(&self, scope: &crate::Isolate) -> Vec<u8> {
        self.to_rust_string_lossy(scope).into_bytes()
    }

    /// The number of UTF-16 code units (`v8::String::Length`).
    ///
    /// Code units, not characters and not bytes: an astral character counts
    /// two, which is what callers of this method expect.
    pub fn length(&self) -> usize {
        self.code_units().len()
    }

    /// Whether every code unit fits in one byte, i.e. the string is ISO-8859-1
    /// (`v8::String::ContainsOnlyOneByte`).
    pub fn contains_only_onebyte(&self) -> bool {
        self.code_units()
            .iter()
            .all(|&unit| unit <= u16::from(u8::MAX))
    }

    /// The byte count of the UTF-8 encoding (`v8::String::Utf8Length`).
    ///
    /// A lone surrogate counts three bytes, which is what the encoding writes
    /// for one whether or not invalid sequences are replaced.
    pub fn utf8_length(&self, _scope: &crate::Isolate) -> usize {
        let mut total = 0;
        let mut previous = None;
        for unit in self.code_units() {
            total += encoded_length(unit, previous);
            previous = Some(unit);
        }
        total
    }

    /// Copies the string's code units into `buffer`, starting at `offset`
    /// (`v8::String::Write`).
    ///
    /// `kNullTerminate` needs a buffer with room for the terminator; one
    /// without the room gets the units and no terminator, where the crate we
    /// stand in for would write past the end of it.
    pub fn write_v2(
        &self,
        _scope: &crate::Isolate,
        offset: u32,
        buffer: &mut [u16],
        flags: WriteFlags,
    ) {
        let units = self.code_units();
        let start = (offset as usize).min(units.len());
        let copied = buffer.len().min(units.len() - start);
        buffer[..copied].copy_from_slice(&units[start..start + copied]);
        if flags.contains(WriteFlags::kNullTerminate) && copied < buffer.len() {
            buffer[copied] = 0;
        }
    }

    /// Copies the string's code units into `buffer` as one byte each
    /// (`v8::String::WriteOneByte`).
    ///
    /// A unit above `0xFF` is narrowed to its low byte rather than escaped or
    /// refused, which is what the crate we stand in for writes; a caller that
    /// cares asks [`contains_only_onebyte`](Self::contains_only_onebyte) first.
    pub fn write_one_byte_v2(
        &self,
        _scope: &crate::Isolate,
        offset: u32,
        buffer: &mut [u8],
        flags: WriteFlags,
    ) {
        let units = self.code_units();
        let start = (offset as usize).min(units.len());
        let copied = buffer.len().min(units.len() - start);
        for (slot, unit) in buffer[..copied].iter_mut().zip(&units[start..]) {
            *slot = *unit as u8;
        }
        if flags.contains(WriteFlags::kNullTerminate) && copied < buffer.len() {
            buffer[copied] = 0;
        }
    }

    /// [`write_one_byte_v2`](Self::write_one_byte_v2) into a buffer that is not
    /// initialized yet, which is how a host fills a `Vec`'s spare capacity
    /// (`v8::String::WriteOneByte` over uninitialized memory).
    pub fn write_one_byte_uninit_v2(
        &self,
        _scope: &crate::Isolate,
        offset: u32,
        buffer: &mut [MaybeUninit<u8>],
        flags: WriteFlags,
    ) {
        let units = self.code_units();
        let start = (offset as usize).min(units.len());
        let copied = buffer.len().min(units.len() - start);
        for (slot, unit) in buffer[..copied].iter_mut().zip(&units[start..]) {
            slot.write(*unit as u8);
        }
        if flags.contains(WriteFlags::kNullTerminate) && copied < buffer.len() {
            buffer[copied].write(0);
        }
    }

    /// The string as UTF-8, borrowed from `buffer` when it fits there and owned
    /// when it does not (`v8::String::ToRustCowLossy`).
    ///
    /// The borrow is of the bytes the write filled, so a caller that passes a
    /// large enough buffer allocates nothing. A string with a lone surrogate is
    /// not valid UTF-8 — this is the *lossy* form — so it is replaced and owned,
    /// which is what the crate we stand in for's answer is a `str` requires too.
    pub fn to_rust_cow_lossy<'a, const N: usize>(
        &self,
        scope: &crate::Isolate,
        buffer: &'a mut [MaybeUninit<u8>; N],
    ) -> std::borrow::Cow<'a, str> {
        let written = self.write_utf8_uninit_v2(scope, buffer, WriteFlags::empty(), None);
        let needed = self.utf8_length(scope);
        // SAFETY: the write filled `written` bytes of the buffer, which is what
        // the slice covers; the borrow is of the caller's own buffer.
        let filled = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), written) };
        match std::str::from_utf8(filled) {
            Ok(text) if written == needed => std::borrow::Cow::Borrowed(text),
            _ => std::borrow::Cow::Owned(self.to_rust_string_lossy(scope)),
        }
    }

    /// The string as UTF-8 (`v8::String::WriteUtf8`), stopping before a
    /// character that does not fit and reporting how many code units that
    /// covered.
    ///
    /// `kReplaceInvalidUtf8` is what makes the output valid UTF-8: without it a
    /// lone surrogate is written as its own three-byte sequence, which is the
    /// only faithful answer and not a valid encoding of anything. The returned
    /// byte count includes the terminator when one was asked for and written.
    /// [`write_utf8_uninit_v2`](Self::write_utf8_uninit_v2) into a buffer that is
    /// not initialized yet
    /// (`v8::String::WriteUtf8` over uninitialized memory).
    pub fn write_utf8_uninit_v2(
        &self,
        _scope: &crate::Isolate,
        buffer: &mut [MaybeUninit<u8>],
        flags: WriteFlags,
        processed_characters_return: Option<&mut usize>,
    ) -> usize {
        let (written, processed) = encode_utf8(
            &self.code_units(),
            buffer,
            flags.contains(WriteFlags::kReplaceInvalidUtf8),
            flags.contains(WriteFlags::kNullTerminate),
        );
        if let Some(processed_return) = processed_characters_return {
            *processed_return = processed;
        }
        written
    }
}

/// Whether `unit` is the trail of a surrogate pair whose lead is `previous`
/// (`unibrow::Utf16::IsSurrogatePair`).
fn is_surrogate_pair(previous: Option<u16>, unit: u16) -> bool {
    previous
        .is_some_and(|lead| (0xD800..0xDC00).contains(&lead) && (0xDC00..0xE000).contains(&unit))
}

fn is_surrogate(unit: u16) -> bool {
    (0xD800..0xE000).contains(&unit)
}

/// The UTF-8 byte count of one code unit, given the one before it
/// (`unibrow::Utf8::Length`): the trail of a pair adds the one byte the pair
/// had left to count, and a lone surrogate counts as much as any three-byte
/// character.
fn encoded_length(unit: u16, previous: Option<u16>) -> usize {
    if unit <= 0x7F || is_surrogate_pair(previous, unit) {
        1
    } else if unit <= 0x7FF {
        2
    } else {
        3
    }
}

/// The UTF-8 bytes of a code point, and how many of them there are. A lone
/// surrogate goes through the three-byte path unchanged, which is the encoding
/// the crate we stand in for writes for one.
fn utf8_bytes(point: u32) -> ([u8; 4], usize) {
    if point <= 0x7F {
        ([point as u8, 0, 0, 0], 1)
    } else if point <= 0x7FF {
        (
            [0xC0 | (point >> 6) as u8, 0x80 | (point & 0x3F) as u8, 0, 0],
            2,
        )
    } else if point <= 0xFFFF {
        (
            [
                0xE0 | (point >> 12) as u8,
                0x80 | ((point >> 6) & 0x3F) as u8,
                0x80 | (point & 0x3F) as u8,
                0,
            ],
            3,
        )
    } else {
        (
            [
                0xF0 | (point >> 18) as u8,
                0x80 | ((point >> 12) & 0x3F) as u8,
                0x80 | ((point >> 6) & 0x3F) as u8,
                0x80 | (point & 0x3F) as u8,
            ],
            4,
        )
    }
}

/// The code point a surrogate pair stands for (`Utf16::CombineSurrogatePair`).
fn combined_point(lead: u16, trail: u16) -> u32 {
    0x1_0000 + ((u32::from(lead) - 0xD800) << 10) + (u32::from(trail) - 0xDC00)
}

/// Writes one code point as UTF-8 at `at`, returning its byte count.
fn write_codepoint(buffer: &mut [MaybeUninit<u8>], at: usize, point: u32) -> usize {
    let (bytes, length) = utf8_bytes(point);
    for (slot, byte) in buffer[at..at + length].iter_mut().zip(bytes) {
        slot.write(byte);
    }
    length
}

/// The UTF-8 encoding of `units` (`unibrow::Utf8::Encode`), which stops before
/// a character that does not fit rather than write part of one. Returns the
/// bytes written — a terminator included when one was asked for — and the code
/// units that covered.
///
/// A surrogate pair takes two steps, as there: the lead reserves the three
/// bytes of an unmatched surrogate that the trail's iteration then rewrites as
/// the pair's four.
fn encode_utf8(
    units: &[u16],
    buffer: &mut [MaybeUninit<u8>],
    replace_invalid: bool,
    null_terminate: bool,
) -> (usize, usize) {
    let content_capacity = buffer.len().saturating_sub(usize::from(null_terminate));
    let mut written = 0;
    let mut read = 0;
    let mut previous = None;
    while read < units.len() {
        let unit = units[read];
        if content_capacity - written < encoded_length(unit, previous) {
            if is_surrogate_pair(previous, unit) {
                // Half a pair fits and the whole one does not: give back the
                // bytes the lead reserved and leave the trail unread.
                written -= 3;
                read -= 1;
            }
            break;
        }
        if units
            .get(read + 1)
            .is_some_and(|&next| is_surrogate_pair(Some(unit), next))
        {
            written += 3;
        } else if let Some(lead) = previous.filter(|&lead| is_surrogate_pair(Some(lead), unit)) {
            write_codepoint(buffer, written - 3, combined_point(lead, unit));
            written += 1;
        } else {
            let point = if replace_invalid && is_surrogate(unit) {
                0xFFFD
            } else {
                u32::from(unit)
            };
            written += write_codepoint(buffer, written, point);
        }
        previous = Some(unit);
        read += 1;
    }
    if null_terminate && written < buffer.len() {
        buffer[written].write(0);
        written += 1;
    }
    (written, read)
}

/// A string resource fixed at compile time (v8::OneByteConst).
///
/// The crate we stand in for gives this a C++ vtable so the engine can read the
/// resource lazily. There is no C++ here, so it carries the bytes and their
/// length, which is all any accessor reads.
#[derive(Clone, Copy, Debug)]
pub struct OneByteConst {
    data: *const u8,
    length: usize,
}

// SAFETY: `data` points into a `&'static` buffer that is never written, so
// sharing the resource between threads shares immutable bytes.
unsafe impl Sync for OneByteConst {}

// SAFETY: as `Sync`.
unsafe impl Send for OneByteConst {}

impl OneByteConst {
    /// The bytes as a string, with no allocation.
    pub const fn as_str(&self) -> &str {
        if self.length == 0 {
            ""
        } else {
            // SAFETY: the constructor checked that the bytes are ASCII.
            unsafe {
                std::str::from_utf8_unchecked(std::slice::from_raw_parts(self.data, self.length))
            }
        }
    }
}

impl AsRef<str> for OneByteConst {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<[u8]> for OneByteConst {
    fn as_ref(&self) -> &[u8] {
        self.as_str().as_bytes()
    }
}

impl Deref for OneByteConst {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

/// Transcode Latin-1 to UTF-8, returning the bytes written
/// (`v8::latin1_to_utf8`).
///
/// This is the one free function a host calls straight out of the crate we
/// stand in for's string module: a host holding Latin-1 — a byte-oriented op's
/// buffer, a `OneByteConst` — asks for its UTF-8 rendering without going
/// through a JS string.
///
/// # Safety
///
/// - `inbuf` must point to at least `input_length` readable bytes.
/// - `outbuf` must point to at least `2 * input_length` writable bytes: every
///   byte above ASCII becomes two.
pub unsafe fn latin1_to_utf8(input_length: usize, inbuf: *const u8, outbuf: *mut u8) -> usize {
    // SAFETY: the caller's contract on both buffers.
    let input = unsafe { std::slice::from_raw_parts(inbuf, input_length) };
    let mut written = 0;
    for &byte in input {
        if byte.is_ascii() {
            // SAFETY: as above — an ASCII byte needs one of the two bytes the
            // caller promised per input byte.
            unsafe { *outbuf.add(written) = byte };
            written += 1;
        } else {
            // SAFETY: as above — a Latin-1 byte above ASCII is two bytes of
            // UTF-8 (110xxxxx 10xxxxxx).
            unsafe {
                *outbuf.add(written) = (byte >> 6) | 0b1100_0000;
                *outbuf.add(written + 1) = (byte & 0b0011_1111) | 0b1000_0000;
            }
            written += 2;
        }
    }
    written
}

/// A handle for a symbol the engine's own tables hold.
fn symbol_handle<'s>(symbol: crux::handle::Handle<crux::symbol::Symbol>) -> Local<'s, Symbol> {
    Local::from_engine(api::Local::from(crux::value::Value::Symbol(symbol)))
}

impl Symbol {
    /// The global-registry symbol for `description`, minting it on first use
    /// (v8::Symbol::For).
    ///
    /// The registry is the engine's own — the same list `Symbol.for` reads — so
    /// a name either side registers is the one symbol both sides get. The
    /// description is the key, exactly as there: the lookup is the string's
    /// contents, so a rope and its flattened form are one entry.
    ///
    /// The crate we stand in for notes that these symbols are never collected.
    /// Here they are kept by the registry, which lives on the agent for as long
    /// as the isolate does.
    pub fn for_key<'s>(
        scope: &PinScope<'s, '_, ()>,
        description: Local<'_, String>,
    ) -> Local<'s, Symbol> {
        // The description is the registry key, so it has to be the string's own
        // code units: the crate's lossy text conversion would fold two names a
        // lone surrogate tells apart into one entry.
        let key: JsString = (*description
            .engine()
            .value()
            .as_string()
            .expect("bridge bug: a String handle the engine does not know"))
        .clone();
        let symbol = crate::realm_of(scope).with_agent(|agent| {
            let mut registry = agent.global_symbol_registry.borrow_mut();
            if let Some((_, symbol)) = registry.iter().find(|(name, _)| *name == key) {
                return symbol.clone();
            }
            let symbol = crux::symbol::Symbol::new(Some(key.clone()));
            registry.push((key.clone(), symbol.clone()));
            symbol
        });
        symbol_handle(crux::handle::Handle::new(symbol))
    }

    /// The canonical `Symbol.iterator` (v8::Symbol::GetIterator).
    ///
    /// The well-known symbols below are the engine's own singletons, not copies
    /// of them: the engine installs `Symbol.name` from the same table, so
    /// `get_iterator(scope)` and script's `Symbol.iterator` are one value, and a
    /// bridge-read symbol used as a property key is the key a script's
    /// iteration finds.
    pub fn get_iterator<'s>(_scope: &PinScope<'s, '_, ()>) -> Local<'s, Symbol> {
        well_known("iterator")
    }

    /// `Symbol.asyncIterator` (v8::Symbol::GetAsyncIterator).
    pub fn get_async_iterator<'s>(_scope: &PinScope<'s, '_, ()>) -> Local<'s, Symbol> {
        well_known("asyncIterator")
    }

    /// `Symbol.hasInstance` (v8::Symbol::GetHasInstance).
    pub fn get_has_instance<'s>(_scope: &PinScope<'s, '_, ()>) -> Local<'s, Symbol> {
        well_known("hasInstance")
    }

    /// `Symbol.isConcatSpreadable` (v8::Symbol::GetIsConcatSpreadable).
    pub fn get_is_concat_spreadable<'s>(_scope: &PinScope<'s, '_, ()>) -> Local<'s, Symbol> {
        well_known("isConcatSpreadable")
    }

    /// `Symbol.match` (v8::Symbol::GetMatch).
    pub fn get_match<'s>(_scope: &PinScope<'s, '_, ()>) -> Local<'s, Symbol> {
        well_known("match")
    }

    /// `Symbol.replace` (v8::Symbol::GetReplace).
    pub fn get_replace<'s>(_scope: &PinScope<'s, '_, ()>) -> Local<'s, Symbol> {
        well_known("replace")
    }

    /// `Symbol.search` (v8::Symbol::GetSearch).
    pub fn get_search<'s>(_scope: &PinScope<'s, '_, ()>) -> Local<'s, Symbol> {
        well_known("search")
    }

    /// `Symbol.split` (v8::Symbol::GetSplit).
    pub fn get_split<'s>(_scope: &PinScope<'s, '_, ()>) -> Local<'s, Symbol> {
        well_known("split")
    }

    /// `Symbol.toPrimitive` (v8::Symbol::GetToPrimitive).
    pub fn get_to_primitive<'s>(_scope: &PinScope<'s, '_, ()>) -> Local<'s, Symbol> {
        well_known("toPrimitive")
    }

    /// `Symbol.toStringTag` (v8::Symbol::GetToStringTag).
    pub fn get_to_string_tag<'s>(_scope: &PinScope<'s, '_, ()>) -> Local<'s, Symbol> {
        well_known("toStringTag")
    }

    /// `Symbol.unscopables` (v8::Symbol::GetUnscopables).
    pub fn get_unscopables<'s>(_scope: &PinScope<'s, '_, ()>) -> Local<'s, Symbol> {
        well_known("unscopables")
    }
}

/// The engine's canonical well-known symbol `name`, which is the value script
/// sees as `Symbol.name`.
fn well_known<'s>(name: &str) -> Local<'s, Symbol> {
    symbol_handle(crux::symbol::well_known(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewStringType;
    use crate::data::Value;
    use crate::test_support::{bind, eval, eval_number, in_context};

    /// The registry is one table: a name script registers is the symbol the
    /// bridge is handed, and the other way round.
    #[test]
    fn the_symbol_registry_is_shared_with_script() {
        in_context!(scope, {
            // Script first: `for_key` has to find what `Symbol.for` made.
            bind(
                scope,
                "fromScript",
                eval(scope, "Symbol.for('bridge.test')"),
            );
            let name = String::new(scope, "bridge.test").expect("string");
            let symbol = Symbol::for_key(scope, name);
            bind(scope, "fromHost", symbol.cast::<Value>());
            assert_eq!(
                eval_number(scope, "fromScript === fromHost ? 1 : 0"),
                1.0,
                "the host is handed the symbol script registered"
            );

            // And a name the host registers is the one script gets afterwards.
            let name = String::new(scope, "bridge.other").expect("string");
            let symbol = Symbol::for_key(scope, name);
            bind(scope, "hostFirst", symbol.cast::<Value>());
            assert_eq!(
                eval_number(scope, "hostFirst === Symbol.for('bridge.other') ? 1 : 0"),
                1.0,
                "a host-registered name is the symbol script resolves"
            );

            // Two lookups of one name are one symbol, which is what makes the
            // registry worth having.
            let name = String::new(scope, "bridge.other").expect("string");
            let symbol = Symbol::for_key(scope, name);
            bind(scope, "again", symbol.cast::<Value>());
            assert_eq!(eval_number(scope, "again === hostFirst ? 1 : 0"), 1.0);
        });
    }

    /// The well-known accessors answer the engine's singletons, which are the
    /// values script sees as `Symbol.name` — and they work as property keys.
    #[test]
    fn the_well_known_symbols_are_the_ones_script_sees() {
        in_context!(scope, {
            let iterator = Symbol::get_iterator(scope);
            bind(scope, "iterKey", iterator.cast::<Value>());
            assert_eq!(
                eval_number(scope, "iterKey === Symbol.iterator ? 1 : 0"),
                1.0
            );
            assert_eq!(
                eval_number(scope, "typeof [][iterKey] === 'function' ? 1 : 0"),
                1.0,
                "the host's key finds what the engine hung there"
            );

            let tag = Symbol::get_to_string_tag(scope);
            bind(scope, "tagKey", tag.cast::<Value>());
            assert_eq!(
                eval_number(scope, "tagKey === Symbol.toStringTag ? 1 : 0"),
                1.0
            );

            let instance = Symbol::get_has_instance(scope);
            bind(scope, "instanceKey", instance.cast::<Value>());
            assert_eq!(
                eval_number(scope, "instanceKey === Symbol.hasInstance ? 1 : 0"),
                1.0
            );
        });
    }

    /// The empty string and a one-byte const are the text script sees — the
    /// const's bytes read one code unit per byte, as they are there.
    #[test]
    fn strings_from_the_static_side_are_the_ones_script_sees() {
        in_context!(scope, {
            let empty = String::empty(scope);
            bind(scope, "empty", empty.cast::<Value>());
            assert_eq!(
                eval_number(scope, "empty === '' && empty.length === 0 ? 1 : 0"),
                1.0
            );

            static NAME: OneByteConst = String::create_external_onebyte_const(b"deno.core");
            let text = String::new_from_onebyte_const(scope, &NAME).expect("string");
            bind(scope, "text", text.cast::<Value>());
            assert_eq!(eval_number(scope, "text === 'deno.core' ? 1 : 0"), 1.0);
        });
    }

    /// The width is the whole of what the typed integer accessors carry: the
    /// engine has one number kind, so a `Uint32` that read through `Integer`
    /// would answer a value above `i32::MAX` the same way a signed read does.
    #[test]
    fn the_typed_integer_accessors_are_the_width_they_name() {
        in_context!(scope, {
            let wide = eval(scope, "4294967295");
            let wide: Local<'_, Uint32> = Local::try_from(wide).expect("uint32");
            assert_eq!(wide.value(), u32::MAX);

            let narrow = eval(scope, "-5");
            let narrow: Local<'_, Int32> = Local::try_from(narrow).expect("int32");
            assert_eq!(narrow.value(), -5);

            let integer = eval(scope, "4294967295");
            let integer: Local<'_, Integer> = Local::try_from(integer).expect("integer");
            assert_eq!(integer.value(), 4_294_967_295_i64);
        });
    }

    /// The string a script evaluates to.
    fn eval_string<'s>(scope: &crate::PinScope<'s, '_>, source: &str) -> Local<'s, String> {
        Local::<String>::try_from(eval(scope, source)).expect("string")
    }

    fn uninit_buffer(len: usize) -> Vec<MaybeUninit<u8>> {
        vec![MaybeUninit::uninit(); len]
    }

    /// The bytes a write reported as written.
    fn written_bytes(buffer: &[MaybeUninit<u8>], written: usize) -> Vec<u8> {
        buffer[..written]
            .iter()
            .map(|byte| *unsafe { byte.assume_init_ref() })
            .collect()
    }

    #[test]
    fn the_lengths_agree_with_the_encoding() {
        in_context!(scope, {
            assert_eq!(eval_string(scope, "'abc'").length(), 3);
            assert_eq!(
                eval_string(scope, "'\\u{1D11E}'").length(),
                2,
                "an astral character is two code units"
            );

            assert_eq!(eval_string(scope, "'abc'").utf8_length(scope), 3);
            assert_eq!(eval_string(scope, "'\\u{E9}'").utf8_length(scope), 2);
            assert_eq!(eval_string(scope, "'\\u{20AC}'").utf8_length(scope), 3);
            assert_eq!(eval_string(scope, "'\\u{1D11E}'").utf8_length(scope), 4);
            assert_eq!(
                eval_string(scope, "'\\u{D800}'").utf8_length(scope),
                3,
                "a lone surrogate is three bytes, which is what is written for it"
            );
        });
    }

    #[test]
    fn a_string_is_one_byte_when_every_unit_is() {
        in_context!(scope, {
            assert!(eval_string(scope, "'abc'").contains_only_onebyte());
            assert!(eval_string(scope, "'\\u{E9}'").contains_only_onebyte());
            assert!(!eval_string(scope, "'\\u{20AC}'").contains_only_onebyte());
        });
    }

    #[test]
    fn write_v2_copies_code_units_from_an_offset() {
        in_context!(scope, {
            let text = eval_string(scope, "'abc'");
            let mut buffer = [0xFFFFu16; 3];
            text.write_v2(scope, 1, &mut buffer, WriteFlags::empty());
            assert_eq!(buffer, [0x62, 0x63, 0xFFFF]);
        });
    }

    #[test]
    fn write_v2_terminates_only_where_a_terminator_fits() {
        in_context!(scope, {
            let text = eval_string(scope, "'abc'");
            let mut roomy = [0xFFFFu16; 4];
            text.write_v2(scope, 0, &mut roomy, WriteFlags::kNullTerminate);
            assert_eq!(roomy, [0x61, 0x62, 0x63, 0]);

            let mut exact = [0xFFFFu16; 3];
            text.write_v2(scope, 0, &mut exact, WriteFlags::kNullTerminate);
            assert_eq!(
                exact,
                [0x61, 0x62, 0x63],
                "a buffer with no room for a terminator gets no terminator"
            );
        });
    }

    #[test]
    fn write_one_byte_v2_narrows_to_the_low_byte() {
        in_context!(scope, {
            let text = eval_string(scope, "'a\\u{20AC}'");
            let mut buffer = [0u8; 4];
            text.write_one_byte_v2(scope, 0, &mut buffer, WriteFlags::empty());
            assert_eq!(buffer, [0x61, 0xAC, 0, 0]);
        });
    }

    #[test]
    fn the_uninitialized_writes_fill_what_they_report() {
        in_context!(scope, {
            let text = eval_string(scope, "'ab'");
            let mut buffer = uninit_buffer(4);
            text.write_one_byte_uninit_v2(scope, 0, &mut buffer, WriteFlags::empty());
            assert_eq!(written_bytes(&buffer, 2), [0x61, 0x62]);

            let mut bytes = uninit_buffer(3);
            let written =
                text.write_utf8_uninit_v2(scope, &mut bytes, WriteFlags::kNullTerminate, None);
            assert_eq!(written, 3, "the count includes the terminator");
            assert_eq!(written_bytes(&bytes, written), [0x61, 0x62, 0]);
        });
    }

    #[test]
    fn utf8_replaces_a_lone_surrogate_only_when_asked() {
        in_context!(scope, {
            let text = eval_string(scope, "'\\u{D800}A'");
            let mut buffer = uninit_buffer(8);
            let mut processed = 0;
            let written = text.write_utf8_uninit_v2(
                scope,
                &mut buffer,
                WriteFlags::kReplaceInvalidUtf8,
                Some(&mut processed),
            );
            assert_eq!((written, processed), (4, 2));
            assert_eq!(
                written_bytes(&buffer, written),
                [0xEF, 0xBF, 0xBD, 0x41],
                "U+FFFD"
            );

            let written = text.write_utf8_uninit_v2(scope, &mut buffer, WriteFlags::empty(), None);
            assert_eq!(written, 4);
            assert_eq!(
                written_bytes(&buffer, written),
                [0xED, 0xA0, 0x80, 0x41],
                "the surrogate's own three bytes, which is not valid UTF-8"
            );
        });
    }

    #[test]
    fn utf8_writes_a_pair_as_one_character() {
        in_context!(scope, {
            let text = eval_string(scope, "'\\u{1D11E}'");
            let mut buffer = uninit_buffer(4);
            let mut processed = 0;
            let written = text.write_utf8_uninit_v2(
                scope,
                &mut buffer,
                WriteFlags::empty(),
                Some(&mut processed),
            );
            assert_eq!((written, processed), (4, 2));
            assert_eq!(written_bytes(&buffer, written), [0xF0, 0x9D, 0x84, 0x9E]);
        });
    }

    #[test]
    fn utf8_stops_before_a_character_that_does_not_fit() {
        in_context!(scope, {
            let text = eval_string(scope, "'\\u{20AC}'");
            let mut buffer = uninit_buffer(2);
            let mut processed = 0;
            let written = text.write_utf8_uninit_v2(
                scope,
                &mut buffer,
                WriteFlags::empty(),
                Some(&mut processed),
            );
            assert_eq!((written, processed), (0, 0));

            let mut buffer = uninit_buffer(3);
            let written = text.write_utf8_uninit_v2(scope, &mut buffer, WriteFlags::empty(), None);
            assert_eq!(written, 3);
            assert_eq!(written_bytes(&buffer, written), [0xE2, 0x82, 0xAC]);
        });
    }

    /// Three bytes is room for the lead of a pair and not for the character,
    /// and half a character is not one — the same answer the crate we stand in
    /// for gives.
    #[test]
    fn utf8_does_not_write_half_a_pair() {
        in_context!(scope, {
            let text = eval_string(scope, "'\\u{1D11E}'");
            let mut buffer = uninit_buffer(3);
            let mut processed = 0;
            let written = text.write_utf8_uninit_v2(
                scope,
                &mut buffer,
                WriteFlags::empty(),
                Some(&mut processed),
            );
            assert_eq!((written, processed), (0, 0));
        });
    }

    #[test]
    fn a_string_can_be_built_from_latin1_bytes() {
        in_context!(scope, {
            let text = String::new_from_one_byte(scope, &[0x61, 0xE9], NewStringType::Normal)
                .expect("string");
            assert_eq!(text.to_utf16(), [0x61, 0xE9]);
            assert_eq!(text.utf8_length(scope), 3);
        });
    }
}

#[cfg(test)]
mod onebyte_const_tests {
    use super::*;
    use crate::test_support::{eval, in_context};

    static GREETING: OneByteConst = String::create_external_onebyte_const(b"hello");

    /// A static resource is readable without a scope, and the string built over
    /// it carries the same bytes.
    #[test]
    fn a_static_string_resource_is_readable_and_becomes_a_string() {
        assert_eq!(GREETING.as_str(), "hello");
        assert_eq!(<OneByteConst as AsRef<[u8]>>::as_ref(&GREETING), b"hello");
        assert_eq!(&*GREETING, "hello");

        in_context!(scope, {
            let text =
                String::new_external_onebyte_static(scope, GREETING.as_ref()).expect("string");
            assert_eq!(text.to_rust_string_lossy(scope), "hello");
            // Latin-1, one code unit per byte, so `0xE9` is one unit.
            let latin1 =
                String::new_external_onebyte_static(scope, b"\xE9".as_slice()).expect("string");
            assert_eq!(latin1.to_utf16(), [0xE9]);
            assert_eq!(
                Local::<crate::data::Number>::try_from(eval(scope, "'hello'.length"))
                    .expect("number")
                    .value(),
                text.to_utf16().len() as f64
            );
        });
    }
}
