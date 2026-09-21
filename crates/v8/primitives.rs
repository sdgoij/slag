//! Primitive values and their constructors (`v8::Primitive`, `v8::String`,
//! `v8::Number`, `v8::Boolean`).

use std::ops::{BitOr, BitOrAssign};

use crux::string::JsString;
use crux::value::ValueKind;
use runtime::api;

use crate::data::{Boolean, Integer, Number, Primitive, String};
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
    pub fn new<'s, R>(_scope: &R, value: i32) -> Local<'s, Integer> {
        Local::from_engine(api::Local::number(f64::from(value)))
    }

    pub fn new_from_i32<'s, R>(scope: &R, value: i32) -> Local<'s, Integer> {
        Self::new(scope, value)
    }

    pub fn new_from_u32<'s, R>(_scope: &R, value: u32) -> Local<'s, Integer> {
        Local::from_engine(api::Local::number(f64::from(value)))
    }
}

impl<'s> Local<'s, Integer> {
    pub fn value(&self) -> f64 {
        self.engine().as_number().unwrap_or(f64::NAN)
    }
}

impl String {
    /// A new string from UTF-8 (`v8::String::NewFromUtf8`).
    pub fn new<'s>(scope: &PinScope<'s, '_, ()>, value: &str) -> Option<Local<'s, String>> {
        Self::new_from_utf8(scope, value.as_bytes(), NewStringType::Normal)
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
        Some(Local::from_engine(api::Local::from(
            crux::value::Value::String(crux::handle::Handle::new(JsString::from_utf16(units))),
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
}
