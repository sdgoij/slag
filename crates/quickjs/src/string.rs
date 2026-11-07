//! The `String`/`Symbol`/`BigInt` facades (rquickjs-shaped): thin handles over
//! the engine's string, symbol, and arbitrary-precision integer values.

use crux::bigint::BigInt as CruxBigInt;
use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::string::JsString;
use crux::symbol::Symbol as CruxSymbol;
use crux::value::{Value as CruxValue, ValueKind};
use runtime::api::Local;

use crate::context::Context;
use crate::exception::Error;
use crate::value::Value;

/// A JavaScript string (rquickjs's `String`).
#[derive(Debug, Clone, Copy)]
pub struct String(Local);

impl String {
    /// Create a string from UTF-8 text (rquickjs's `String::from_str`).
    pub fn from_str(_context: &Context, text: &str) -> Result<Self, Error> {
        Ok(Self(Local::string(text)))
    }

    /// The string's lossy UTF-8 rendering.
    pub fn to_string(&self) -> Result<std::string::String, Error> {
        self.crux().map(|string| string.to_string_lossy())
    }

    /// The number of code units (spec 6.1.4.1 StringLength).
    pub fn len(&self) -> usize {
        self.crux().map_or(0, |string| string.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The value this string is.
    pub fn as_value(&self) -> Value {
        Value::from_local(self.0)
    }

    fn crux(&self) -> Result<Handle<JsString>, Error> {
        match self.0.value().kind() {
            ValueKind::String(handle) => Ok(handle),
            _ => Err(not("a string")),
        }
    }
}

/// A JavaScript symbol (rquickjs's `Symbol`).
#[derive(Debug, Clone, Copy)]
pub struct Symbol(Local);

impl Symbol {
    /// Create a unique symbol, optionally with a description (rquickjs's
    /// `Symbol::new`).
    pub fn new(_context: &Context, description: Option<&str>) -> Result<Self, Error> {
        let symbol = CruxSymbol::new(description.map(JsString::from_utf8));
        Ok(Self(Local::from_value(CruxValue::Symbol(Handle::new(
            symbol,
        )))))
    }

    /// The symbol's description, if it has one.
    pub fn description(&self) -> Option<std::string::String> {
        self.crux()
            .ok()?
            .description
            .as_ref()
            .map(|d| d.to_string_lossy())
    }

    /// spec 6.1.6.3.7 SymbolDescriptiveString.
    pub fn to_string(&self) -> Result<std::string::String, Error> {
        self.crux()
            .map(|symbol| crux::symbol::descriptive_string(&symbol))
    }

    /// The value this symbol is.
    pub fn as_value(&self) -> Value {
        Value::from_local(self.0)
    }

    fn crux(&self) -> Result<Handle<CruxSymbol>, Error> {
        match self.0.value().kind() {
            ValueKind::Symbol(handle) => Ok(handle),
            _ => Err(not("a symbol")),
        }
    }
}

/// A JavaScript BigInt (rquickjs's `BigInt`).
#[derive(Debug, Clone, Copy)]
pub struct BigInt(Local);

impl BigInt {
    pub fn from_i64(_context: &Context, value: i64) -> Result<Self, Error> {
        Ok(Self::from_crux(CruxBigInt::from(value)))
    }

    pub fn from_u64(_context: &Context, value: u64) -> Result<Self, Error> {
        Ok(Self::from_crux(CruxBigInt::from(value)))
    }

    /// NumberToBigInt (spec 7.1.16); fails on a non-integral or non-finite
    /// number (rquickjs's `BigInt::from_f64`).
    pub fn from_f64(_context: &Context, value: f64) -> Result<Self, Error> {
        let bigint = CruxBigInt::from_f64_exact(value).ok_or_else(|| {
            JsError::new(
                ErrorKind::RangeError,
                "number is not an integral BigInt".into(),
            )
        })?;
        Ok(Self::from_crux(bigint))
    }

    /// Parse `text` in the given radix (2-36) (rquickjs's `BigInt::from_str`).
    pub fn from_str(_context: &Context, radix: u32, text: &str) -> Result<Self, Error> {
        if !(2..=36).contains(&radix) {
            return Err(JsError::new(ErrorKind::RangeError, "radix out of range".into()).into());
        }
        let bigint = CruxBigInt::parse_str(text, radix)
            .ok_or_else(|| JsError::new(ErrorKind::SyntaxError, "invalid BigInt literal".into()))?;
        Ok(Self::from_crux(bigint))
    }

    /// The value when it fits in an `i64`; `None` otherwise.
    pub fn to_i64(&self) -> Option<i64> {
        let bigint = self.crux().ok()?;
        let wrapped = bigint.to_i64_wrapping();
        (CruxBigInt::from(wrapped) == *bigint).then_some(wrapped)
    }

    /// The value when it lies in `[0, 2^64)`; `None` otherwise.
    pub fn to_u64(&self) -> Option<u64> {
        self.crux().ok()?.to_u64()
    }

    /// Correctly rounded `f64` conversion.
    pub fn to_f64(&self) -> f64 {
        self.crux().map_or(f64::NAN, |bigint| bigint.to_f64())
    }

    /// The value in `radix` (2-36).
    pub fn to_string(&self, radix: u32) -> Result<std::string::String, Error> {
        if !(2..=36).contains(&radix) {
            return Err(JsError::new(ErrorKind::RangeError, "radix out of range".into()).into());
        }
        let bigint = self.crux()?;
        Ok(crux::bigint::to_string(&bigint, radix))
    }

    /// The value this BigInt is.
    pub fn as_value(&self) -> Value {
        Value::from_local(self.0)
    }

    fn from_crux(bigint: CruxBigInt) -> Self {
        Self(Local::from_value(CruxValue::BigInt(Handle::new(bigint))))
    }

    fn crux(&self) -> Result<Handle<CruxBigInt>, Error> {
        match self.0.value().kind() {
            ValueKind::BigInt(handle) => Ok(handle),
            _ => Err(not("a BigInt")),
        }
    }
}

fn not(kind: &str) -> Error {
    JsError::new(ErrorKind::TypeError, format!("value is not {kind}")).into()
}
